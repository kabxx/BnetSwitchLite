use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    commands::now_epoch_ms,
    contracts::{AccountKey, LoginIntent, LoginSessionSnapshot},
    error::AppError,
};

use super::{
    backup_exclusion, secure_fs,
    secure_snapshot::{SecurePayload, SnapshotCodec},
};

const RECORD_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RecoveryState {
    Prepared,
    AwaitingUser,
    Committed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RecoveryKind {
    Switch,
    Login,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecoveryRecord {
    pub version: u8,
    pub id: String,
    pub kind: RecoveryKind,
    pub state: RecoveryState,
    pub before_snapshot_id: String,
    pub candidate_snapshot_id: Option<String>,
    pub previous_client_was_running: bool,
    pub created_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum RecoveryContext {
    Switch {
        target_account: AccountKey,
        previous_account: Option<AccountKey>,
    },
    Login {
        intent: LoginIntent,
        previous_account: Option<AccountKey>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecoveryEnvelope {
    pub before: SecurePayload,
    pub context: RecoveryContext,
}

#[derive(Clone)]
pub(crate) struct RecoveryStore {
    root: PathBuf,
    record_path: PathBuf,
    before_directory: PathBuf,
    candidate_directory: PathBuf,
    codec: SnapshotCodec,
}

impl RecoveryStore {
    pub fn new(data_directory: &Path, codec: SnapshotCodec) -> Result<Self, AppError> {
        let root = data_directory.join("recovery");
        secure_fs::ensure_private_directory(&root)?;
        backup_exclusion::set_and_verify(&root).map_err(AppError::Transaction)?;
        let store = Self {
            record_path: root.join("record.json"),
            before_directory: root.join("before"),
            candidate_directory: root.join("candidate"),
            root,
            codec,
        };
        if !secure_fs::private_file_exists(&store.record_path)? {
            store.cleanup_orphans()?;
        }
        Ok(store)
    }

    pub fn record(&self) -> Result<Option<RecoveryRecord>, AppError> {
        if !secure_fs::private_file_exists(&self.record_path)? {
            return Ok(None);
        }
        let record: RecoveryRecord =
            serde_json::from_slice(&secure_fs::read_private_file(&self.record_path)?)
                .map_err(|error| AppError::Transaction(format!("恢复记录格式无效：{error}")))?;
        validate_record(&record)?;
        Ok(Some(record))
    }

    pub fn context(&self, id: &str) -> Result<RecoveryEnvelope, AppError> {
        let record = self.require(id)?;
        let (snapshot_id, envelope): (_, RecoveryEnvelope) =
            self.codec.read(&self.before_directory)?;
        if snapshot_id != record.before_snapshot_id {
            return Err(AppError::Transaction(
                "恢复记录与 before-image 不匹配".into(),
            ));
        }
        let kind_matches = matches!(
            (record.kind, &envelope.context),
            (RecoveryKind::Switch, RecoveryContext::Switch { .. })
                | (RecoveryKind::Login, RecoveryContext::Login { .. })
        );
        if !kind_matches {
            return Err(AppError::Transaction("恢复记录与恢复负载类型不一致".into()));
        }
        Ok(envelope)
    }

    pub fn prepare(
        &self,
        kind: RecoveryKind,
        previous_client_was_running: bool,
        envelope: &RecoveryEnvelope,
    ) -> Result<RecoveryRecord, AppError> {
        if self.record()?.is_some() {
            return Err(AppError::Transaction(
                "已有账号操作正在等待完成或恢复".into(),
            ));
        }
        self.ensure_empty()?;
        let id = Uuid::new_v4().to_string();
        let staging = self.root.join(format!(".preparing-{id}"));
        secure_fs::create_private_directory(&staging)?;

        let result = (|| {
            let before_snapshot_id = self.codec.write(&staging, envelope)?;
            let (verified_id, verified): (_, RecoveryEnvelope) = self.codec.read(&staging)?;
            if verified_id != before_snapshot_id {
                return Err(AppError::Transaction("恢复负载标识校验失败".into()));
            }
            if std::mem::discriminant(&verified.context)
                != std::mem::discriminant(&envelope.context)
            {
                return Err(AppError::Transaction("恢复负载发布前校验失败".into()));
            }
            secure_fs::rename_private_directory(&staging, &self.before_directory)?;
            let record = RecoveryRecord {
                version: RECORD_VERSION,
                id,
                kind,
                state: RecoveryState::Prepared,
                before_snapshot_id,
                candidate_snapshot_id: None,
                previous_client_was_running,
                created_at: now_epoch_ms(),
            };
            self.save_record(&record)?;
            Ok(record)
        })();
        if result.is_err() {
            let _ = secure_fs::remove_private_directory(&staging);
            if !secure_fs::private_file_exists(&self.record_path)? {
                let _ = secure_fs::remove_private_directory(&self.before_directory);
            }
        }
        result
    }

    pub fn transition(&self, id: &str, state: RecoveryState) -> Result<RecoveryRecord, AppError> {
        let mut record = self.require(id)?;
        let valid = matches!(
            (record.state, state),
            (RecoveryState::Prepared, RecoveryState::AwaitingUser)
                | (RecoveryState::AwaitingUser, RecoveryState::Committed)
                | (RecoveryState::Prepared, RecoveryState::Committed)
        );
        if !valid {
            return Err(AppError::Transaction("恢复状态转换无效".into()));
        }
        record.state = state;
        self.save_record(&record)?;
        Ok(record)
    }

    pub fn commit(&self, id: &str) -> Result<Option<String>, AppError> {
        self.transition(id, RecoveryState::Committed)?;
        match self.cleanup() {
            Ok(()) => Ok(None),
            Err(error) => Ok(Some(error.to_string())),
        }
    }

    pub fn stage_login_candidate(
        &self,
        id: &str,
        payload: &SecurePayload,
    ) -> Result<RecoveryRecord, AppError> {
        let mut record = self.require(id)?;
        if record.kind != RecoveryKind::Login || record.state != RecoveryState::AwaitingUser {
            return Err(AppError::Login("当前登录会话不能暂存账号快照".into()));
        }
        if payload.account.is_none() || !payload.preferences.has_authentication_keys()? {
            return Err(AppError::Login("登录结果不包含可保存的认证状态".into()));
        }
        if record.candidate_snapshot_id.is_some() {
            let existing = self.login_candidate(id)?;
            if existing.account != payload.account {
                return Err(AppError::Login(
                    "已暂存的登录账号与本次选择不一致，请取消后重试".into(),
                ));
            }
            return Ok(record);
        }

        secure_fs::remove_private_directory(&self.candidate_directory)?;
        let staging = self.root.join(format!(".candidate-{}", record.id));
        secure_fs::remove_private_directory(&staging)?;
        secure_fs::create_private_directory(&staging)?;
        let result = (|| {
            let candidate_id = self.codec.write(&staging, payload)?;
            let (verified_id, verified): (_, SecurePayload) = self.codec.read(&staging)?;
            if verified_id != candidate_id || verified.account != payload.account {
                return Err(AppError::Transaction("候选账号快照发布前校验失败".into()));
            }
            secure_fs::rename_private_directory(&staging, &self.candidate_directory)?;
            record.candidate_snapshot_id = Some(candidate_id);
            self.save_record(&record)?;
            Ok(record.clone())
        })();
        if result.is_err() {
            let _ = secure_fs::remove_private_directory(&staging);
        }
        result
    }

    pub fn login_candidate(&self, id: &str) -> Result<SecurePayload, AppError> {
        let record = self.require(id)?;
        if record.kind != RecoveryKind::Login {
            return Err(AppError::Login("恢复记录不是登录会话".into()));
        }
        let expected = record
            .candidate_snapshot_id
            .as_deref()
            .ok_or_else(|| AppError::Login("登录候选快照不存在".into()))?;
        let (actual, payload): (_, SecurePayload) = self.codec.read(&self.candidate_directory)?;
        if actual != expected || payload.account.is_none() {
            return Err(AppError::Transaction("登录候选快照与恢复记录不匹配".into()));
        }
        Ok(payload)
    }

    pub fn mark_login_committed(&self, id: &str) -> Result<RecoveryRecord, AppError> {
        let record = self.require(id)?;
        if record.kind != RecoveryKind::Login || record.candidate_snapshot_id.is_none() {
            return Err(AppError::Login("登录结果尚未完整暂存".into()));
        }
        self.login_candidate(id)?;
        self.transition(id, RecoveryState::Committed)
    }

    pub fn rollback(
        &self,
        id: &str,
        apply_before: &dyn Fn(&SecurePayload) -> Result<(), AppError>,
    ) -> Result<(), AppError> {
        let record = self.require(id)?;
        if record.state == RecoveryState::Committed {
            return Err(AppError::Transaction("已提交的恢复记录不能回滚".into()));
        }
        let envelope = self.context(id)?;
        apply_before(&envelope.before)?;
        self.cleanup()
    }

    pub fn cleanup_committed(&self, id: &str) -> Result<(), AppError> {
        let record = self.require(id)?;
        if record.state != RecoveryState::Committed {
            return Err(AppError::Transaction("恢复记录尚未提交".into()));
        }
        self.cleanup()
    }

    pub fn login_snapshot(&self) -> Result<Option<LoginSessionSnapshot>, AppError> {
        let Some(record) = self.record()? else {
            return Ok(None);
        };
        if record.kind != RecoveryKind::Login || record.state == RecoveryState::Committed {
            return Ok(None);
        }
        let envelope = self.context(&record.id)?;
        let RecoveryContext::Login { intent, .. } = envelope.context else {
            unreachable!();
        };
        Ok(Some(LoginSessionSnapshot {
            id: record.id,
            intent,
            created_at: record.created_at,
        }))
    }

    fn require(&self, id: &str) -> Result<RecoveryRecord, AppError> {
        Uuid::parse_str(id).map_err(|_| AppError::Transaction("恢复记录标识无效".into()))?;
        let record = self
            .record()?
            .ok_or_else(|| AppError::Transaction("恢复记录不存在".into()))?;
        if record.id != id {
            return Err(AppError::Transaction("恢复记录标识不匹配".into()));
        }
        Ok(record)
    }

    fn save_record(&self, record: &RecoveryRecord) -> Result<(), AppError> {
        validate_record(record)?;
        secure_fs::write_private_file_replace(
            &self.record_path,
            &serde_json::to_vec_pretty(record)
                .map_err(|error| AppError::Transaction(format!("无法生成恢复记录：{error}")))?,
        )
    }

    fn cleanup(&self) -> Result<(), AppError> {
        if secure_fs::private_file_exists(&self.record_path)? {
            secure_fs::remove_private_file(&self.record_path)?;
        }
        secure_fs::remove_private_directory(&self.candidate_directory)?;
        secure_fs::remove_private_directory(&self.before_directory)?;
        self.cleanup_staging_directories()
    }

    fn cleanup_orphans(&self) -> Result<(), AppError> {
        secure_fs::remove_private_directory(&self.before_directory)?;
        secure_fs::remove_private_directory(&self.candidate_directory)?;
        self.cleanup_staging_directories()
    }

    fn cleanup_staging_directories(&self) -> Result<(), AppError> {
        for entry in secure_fs::read_private_directory(&self.root)? {
            let entry = entry
                .map_err(|error| AppError::Transaction(format!("无法读取恢复目录项：{error}")))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name
                .strip_prefix(".preparing-")
                .or_else(|| name.strip_prefix(".candidate-"));
            let Some(id) = id else {
                return Err(AppError::Transaction(format!(
                    "恢复目录包含未识别项目：{name}"
                )));
            };
            Uuid::parse_str(id)
                .map_err(|_| AppError::Transaction("恢复暂存目录名称无效".into()))?;
            secure_fs::remove_private_directory(&entry.path())?;
        }
        Ok(())
    }

    fn ensure_empty(&self) -> Result<(), AppError> {
        if secure_fs::read_private_directory(&self.root)?
            .next()
            .transpose()
            .map_err(|error| AppError::Transaction(format!("无法读取恢复目录项：{error}")))?
            .is_some()
        {
            return Err(AppError::Transaction(
                "恢复目录包含未完成或未识别的数据".into(),
            ));
        }
        Ok(())
    }
}

fn validate_record(record: &RecoveryRecord) -> Result<(), AppError> {
    if record.version != RECORD_VERSION
        || Uuid::parse_str(&record.id).is_err()
        || Uuid::parse_str(&record.before_snapshot_id).is_err()
        || record
            .candidate_snapshot_id
            .as_deref()
            .is_some_and(|id| Uuid::parse_str(id).is_err())
    {
        return Err(AppError::Transaction("恢复记录版本或标识无效".into()));
    }
    if record.kind == RecoveryKind::Switch && record.candidate_snapshot_id.is_some() {
        return Err(AppError::Transaction("切换记录包含意外候选快照".into()));
    }
    if record.candidate_snapshot_id.is_some()
        && (record.kind != RecoveryKind::Login || record.state == RecoveryState::Prepared)
    {
        return Err(AppError::Transaction("恢复记录包含无效候选快照状态".into()));
    }
    if record.kind == RecoveryKind::Login
        && record.state == RecoveryState::Committed
        && record.candidate_snapshot_id.is_none()
    {
        return Err(AppError::Transaction("已提交登录记录缺少候选快照".into()));
    }
    if record.state == RecoveryState::AwaitingUser
        && !matches!(record.kind, RecoveryKind::Switch | RecoveryKind::Login)
    {
        return Err(AppError::Transaction("恢复记录状态无效".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{RecoveryContext, RecoveryEnvelope, RecoveryKind, RecoveryState, RecoveryStore};
    use crate::{
        contracts::{AccountKey, LoginIntent, LoginRegion},
        platform::macos::{
            preferences::PreferenceSnapshot,
            secure_snapshot::{ConfigSnapshot, SecurePayload, SnapshotCodec},
        },
    };

    fn account(id: &str) -> AccountKey {
        AccountKey {
            environment: "test.actual.battle.net".into(),
            account_id: id.into(),
        }
    }

    fn payload(account: Option<AccountKey>, authenticated: bool) -> SecurePayload {
        let preferences = if authenticated {
            plist_dictionary(&[("UnifiedAuth/test", "fixture")])
        } else {
            plist_dictionary(&[])
        };
        SecurePayload {
            version: 1,
            account,
            created_at: 1,
            config: ConfigSnapshot {
                present: true,
                bytes: b"fixture".to_vec(),
                sha256: format!("{:x}", Sha256::digest(b"fixture")),
                unix_mode: 0o600,
            },
            preferences: PreferenceSnapshot {
                any_host: preferences,
                current_host: plist_dictionary(&[]),
            },
        }
    }

    fn plist_dictionary(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut dictionary = plist::Dictionary::new();
        for (key, value) in entries {
            dictionary.insert((*key).into(), plist::Value::String((*value).into()));
        }
        let mut bytes = Vec::new();
        plist::to_writer_binary(&mut bytes, &plist::Value::Dictionary(dictionary)).unwrap();
        bytes
    }

    fn store(root: &Path) -> RecoveryStore {
        let canonical_root = root.canonicalize().unwrap();
        RecoveryStore::new(&canonical_root, SnapshotCodec).unwrap()
    }

    fn login_envelope() -> RecoveryEnvelope {
        RecoveryEnvelope {
            before: payload(None, false),
            context: RecoveryContext::Login {
                intent: LoginIntent::Reauthenticate {
                    account_key: AccountKey {
                        environment: "eu.actual.battle.net".into(),
                        account_id: "2".into(),
                    },
                },
                previous_account: None,
            },
        }
    }

    #[test]
    fn reauthentication_region_survives_the_encrypted_recovery_round_trip() {
        let temporary = TempDir::new().unwrap();
        let store = store(temporary.path());
        let record = store
            .prepare(RecoveryKind::Login, false, &login_envelope())
            .unwrap();
        let context = store.context(&record.id).unwrap();

        let RecoveryContext::Login { intent, .. } = context.context else {
            panic!("expected login recovery context");
        };
        assert_eq!(intent.requested_region(), Some(LoginRegion::Europe));
    }

    #[test]
    fn before_image_id_must_match_the_durable_record() {
        let temporary = TempDir::new().unwrap();
        let store = store(temporary.path());
        let mut record = store
            .prepare(RecoveryKind::Login, false, &login_envelope())
            .unwrap();
        record.before_snapshot_id = Uuid::new_v4().to_string();
        store.save_record(&record).unwrap();

        assert!(store.context(&record.id).is_err());
    }

    #[test]
    fn staged_candidate_is_bound_to_record_and_removed_on_rollback() {
        let temporary = TempDir::new().unwrap();
        let store = store(temporary.path());
        let record = store
            .prepare(RecoveryKind::Login, false, &login_envelope())
            .unwrap();
        store
            .transition(&record.id, RecoveryState::AwaitingUser)
            .unwrap();
        let selected = account("2");
        store
            .stage_login_candidate(&record.id, &payload(Some(selected.clone()), true))
            .unwrap();
        assert_eq!(
            store.login_candidate(&record.id).unwrap().account,
            Some(selected)
        );

        store.rollback(&record.id, &|_| Ok(())).unwrap();
        assert!(store.record().unwrap().is_none());
        assert!(!store.before_directory.exists());
        assert!(!store.candidate_directory.exists());
    }

    #[test]
    fn unauthenticated_candidate_is_rejected_without_committing() {
        let temporary = TempDir::new().unwrap();
        let store = store(temporary.path());
        let record = store
            .prepare(RecoveryKind::Login, false, &login_envelope())
            .unwrap();
        store
            .transition(&record.id, RecoveryState::AwaitingUser)
            .unwrap();

        assert!(
            store
                .stage_login_candidate(&record.id, &payload(Some(account("2")), false))
                .is_err()
        );
        let pending = store.record().unwrap().unwrap();
        assert_eq!(pending.state, RecoveryState::AwaitingUser);
        assert!(pending.candidate_snapshot_id.is_none());
        assert!(store.before_directory.exists());
    }

    #[test]
    fn stale_candidate_staging_does_not_block_retry() {
        let temporary = TempDir::new().unwrap();
        let store = store(temporary.path());
        let record = store
            .prepare(RecoveryKind::Login, false, &login_envelope())
            .unwrap();
        store
            .transition(&record.id, RecoveryState::AwaitingUser)
            .unwrap();
        let stale = store.root.join(format!(".candidate-{}", record.id));
        crate::platform::macos::secure_fs::create_private_directory(&stale).unwrap();

        store
            .stage_login_candidate(&record.id, &payload(Some(account("3")), true))
            .unwrap();
        assert!(!stale.exists());
    }

    #[test]
    fn awaiting_user_transition_stays_pending_without_a_deadline() {
        let temporary = TempDir::new().unwrap();
        let store = store(temporary.path());
        let record = store
            .prepare(RecoveryKind::Login, false, &login_envelope())
            .unwrap();

        let awaiting = store
            .transition(&record.id, RecoveryState::AwaitingUser)
            .unwrap();
        assert_eq!(awaiting.state, RecoveryState::AwaitingUser);
        assert_eq!(store.record().unwrap().unwrap().id, record.id);
        assert!(store.before_directory.exists());
    }

    #[test]
    fn rollback_failure_preserves_the_journal_and_before_image() {
        let temporary = TempDir::new().unwrap();
        let store = store(temporary.path());
        let record = store
            .prepare(RecoveryKind::Login, false, &login_envelope())
            .unwrap();
        store
            .transition(&record.id, RecoveryState::AwaitingUser)
            .unwrap();

        assert!(
            store
                .rollback(&record.id, &|_| {
                    Err(crate::error::AppError::Transaction(
                        "injected failure".into(),
                    ))
                })
                .is_err()
        );
        assert_eq!(store.record().unwrap().unwrap().id, record.id);
        assert!(store.before_directory.exists());
    }
}
