use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::{
    commands::now_epoch_ms,
    contracts::{AccountKey, LoginIntent, LoginSessionSnapshot},
    data_store::write_json_atomic,
    error::AppError,
};

use super::{
    authentication::AuthenticationBaseline,
    recovery::{RecoveryCommitOutcome, RecoveryKind, RecoveryRecord, RecoveryState, RecoveryStore},
    restore::{CommitOutcome, RestoreEngine},
    snapshot::reject_reparse_point,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoginRecoveryOutcome {
    None,
    RolledBack,
    CleanedCompleted,
}

pub(crate) struct LoginSessionManager {
    active_directory: PathBuf,
    recovery: RecoveryStore,
}

impl LoginSessionManager {
    pub fn new(data_directory: &Path, active_directory: &Path) -> Result<Self, AppError> {
        Ok(Self {
            active_directory: active_directory.to_path_buf(),
            recovery: RecoveryStore::new(data_directory)?,
        })
    }

    pub fn snapshot(&self) -> Result<Option<LoginSessionSnapshot>, AppError> {
        let Some(record) = self.recovery.record()? else {
            return Ok(None);
        };
        let RecoveryKind::Login {
            intent, created_at, ..
        } = &record.kind
        else {
            return Ok(None);
        };
        if record.state == RecoveryState::Committed {
            return Ok(None);
        }
        Ok(Some(LoginSessionSnapshot {
            id: record.id,
            intent: intent.clone(),
            created_at: *created_at,
        }))
    }

    pub fn begin(
        &self,
        intent: LoginIntent,
        previous_client_was_running: bool,
        previous_account: Option<AccountKey>,
        authentication_baseline: AuthenticationBaseline,
        restore: &RestoreEngine,
        before_active_write: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<LoginSessionSnapshot, AppError> {
        if self.recovery.record()?.is_some() {
            return Err(AppError::Login(
                "已有账号操作正在进行，请先完成或取消该流程".into(),
            ));
        }
        if !self.active_directory.is_dir() {
            return Err(AppError::Login(
                "未找到 Battle.net 配置目录，请先启动并登录一次战网客户端".into(),
            ));
        }
        reject_reparse_point(&self.active_directory)?;

        let created_at = now_epoch_ms();
        let record = self.recovery.prepare(
            &self.active_directory,
            RecoveryKind::Login {
                intent: intent.clone(),
                previous_account,
                created_at,
                authentication_baseline,
            },
            previous_client_was_running,
        )?;

        if let Err(error) = before_active_write() {
            let _ = self.recovery.discard_prepared(&record.id);
            return Err(error);
        }
        if let Err(error) =
            clear_saved_account_names(&self.active_directory.join("Battle.net.config"))
        {
            let rollback = self.rollback_internal(&record.id, restore, before_active_write);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(AppError::Login(format!(
                    "{}；恢复登录前配置失败：{}",
                    error.nested_message(),
                    rollback_error.nested_message()
                ))),
            };
        }
        if let Err(error) = self
            .recovery
            .transition(&record.id, RecoveryState::AwaitingUser)
        {
            let rollback = self.rollback_internal(&record.id, restore, before_active_write);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(AppError::Login(format!(
                    "{}；恢复登录前配置失败：{}",
                    error.nested_message(),
                    rollback_error.nested_message()
                ))),
            };
        }

        Ok(LoginSessionSnapshot {
            id: record.id,
            intent,
            created_at,
        })
    }

    pub fn previous_client_was_running(&self, session_id: &str) -> Result<bool, AppError> {
        Ok(self.require_login(session_id)?.previous_client_was_running)
    }

    pub fn require_awaiting(
        &self,
        session_id: &str,
    ) -> Result<(LoginIntent, Option<AccountKey>), AppError> {
        let record = self.require_login(session_id)?;
        if record.state != RecoveryState::AwaitingUser {
            return Err(AppError::Login("当前登录会话不能完成".into()));
        }
        match record.kind {
            RecoveryKind::Login {
                intent,
                previous_account,
                ..
            } => Ok((intent, previous_account)),
            RecoveryKind::Switch { .. } => unreachable!(),
        }
    }

    pub fn require_completed_login_marker(&self, session_id: &str) -> Result<(), AppError> {
        self.require_awaiting(session_id)?;
        if saved_account_names_present(&self.active_directory.join("Battle.net.config"))? {
            Ok(())
        } else {
            Err(AppError::Login(
                "尚未检测到登录完成，请返回战网完成登录".into(),
            ))
        }
    }

    pub fn completed_login_marker_present(&self, session_id: &str) -> Result<bool, AppError> {
        self.require_awaiting(session_id)?;
        saved_account_names_present(&self.active_directory.join("Battle.net.config"))
    }

    pub fn authentication_baseline(
        &self,
        session_id: &str,
    ) -> Result<AuthenticationBaseline, AppError> {
        let record = self.require_login(session_id)?;
        let RecoveryKind::Login {
            authentication_baseline,
            ..
        } = record.kind
        else {
            unreachable!();
        };
        Ok(authentication_baseline)
    }

    pub fn needs_automatic_recovery(&self) -> Result<bool, AppError> {
        Ok(self.login_record()?.is_some())
    }

    pub fn automatic_recovery_requires_active_write(&self) -> Result<bool, AppError> {
        Ok(matches!(
            self.login_record()?,
            Some(record)
                if matches!(
                    record.state,
                    RecoveryState::Prepared | RecoveryState::AwaitingUser
                )
        ))
    }

    pub fn complete(&self, session_id: &str) -> Result<CommitOutcome, AppError> {
        let record = self.require_login(session_id)?;
        if record.state != RecoveryState::AwaitingUser {
            return Err(AppError::Login("当前登录会话不能提交".into()));
        }
        match self.recovery.commit(&record.id)? {
            RecoveryCommitOutcome::Committed => Ok(CommitOutcome::Committed),
            RecoveryCommitOutcome::CleanupPending(error) => {
                Ok(CommitOutcome::CleanupPending(error))
            }
        }
    }

    pub fn rollback(
        &self,
        session_id: &str,
        restore: &RestoreEngine,
        before_active_write: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<(), AppError> {
        let record = self.require_login(session_id)?;
        self.rollback_internal(&record.id, restore, before_active_write)
    }

    pub fn recover_if_needed(
        &self,
        restore: &RestoreEngine,
        before_active_write: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<LoginRecoveryOutcome, AppError> {
        let Some(record) = self.login_record()? else {
            return Ok(LoginRecoveryOutcome::None);
        };
        match record.state {
            RecoveryState::Prepared | RecoveryState::AwaitingUser => {
                self.rollback_internal(&record.id, restore, before_active_write)?;
                Ok(LoginRecoveryOutcome::RolledBack)
            }
            RecoveryState::Committed => {
                self.recovery.cleanup_committed(&record.id)?;
                Ok(LoginRecoveryOutcome::CleanedCompleted)
            }
        }
    }

    fn rollback_internal(
        &self,
        id: &str,
        restore: &RestoreEngine,
        before_active_write: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<(), AppError> {
        let record = self.require_login(id)?;
        self.recovery
            .rollback(&record.id, before_active_write, &|source, expected| {
                restore.replace_active_file_set(source, expected)
            })
    }

    fn login_record(&self) -> Result<Option<RecoveryRecord>, AppError> {
        match self.recovery.record()? {
            Some(record) if matches!(record.kind, RecoveryKind::Login { .. }) => Ok(Some(record)),
            Some(_) => Ok(None),
            None => Ok(None),
        }
    }

    fn require_login_record(&self) -> Result<RecoveryRecord, AppError> {
        self.login_record()?
            .ok_or_else(|| AppError::Login("登录恢复记录不存在".into()))
    }

    fn require_login(&self, session_id: &str) -> Result<RecoveryRecord, AppError> {
        let record = self.require_login_record()?;
        if record.id != session_id {
            return Err(AppError::Login("登录会话已失效，请刷新账号后重试".into()));
        }
        Ok(record)
    }
}

fn clear_saved_account_names(config_path: &Path) -> Result<(), AppError> {
    reject_reparse_point(config_path)?;
    let bytes = fs::read(config_path)
        .map_err(|error| AppError::Login(format!("无法读取 Battle.net.config：{error}")))?;
    let mut value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Login(format!("Battle.net.config 不是有效 JSON：{error}")))?;
    let mut matches = 0_u8;
    clear_saved_account_names_in_value(&mut value, &mut matches)?;
    if matches != 1 {
        return Err(AppError::Login(format!(
            "Battle.net.config 中应恰好包含一个 SavedAccountNames 字段，实际找到 {matches} 个"
        )));
    }
    let mut updated = serde_json::to_vec_pretty(&value)
        .map_err(|error| AppError::Login(format!("无法生成 Battle.net.config：{error}")))?;
    updated.push(b'\n');
    write_json_atomic(config_path, &updated)
        .map_err(|error| AppError::Login(format!("无法更新 Battle.net.config：{error}")))
}

fn saved_account_names_present(config_path: &Path) -> Result<bool, AppError> {
    reject_reparse_point(config_path)?;
    let bytes = fs::read(config_path)
        .map_err(|error| AppError::Login(format!("无法读取 Battle.net.config：{error}")))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Login(format!("Battle.net.config 不是有效 JSON：{error}")))?;
    let mut matches = 0_u8;
    let mut present = false;
    inspect_saved_account_names(&value, &mut matches, &mut present)?;
    if matches != 1 {
        return Err(AppError::Login(format!(
            "Battle.net.config 中应恰好包含一个 SavedAccountNames 字段，实际找到 {matches} 个"
        )));
    }
    Ok(present)
}

fn inspect_saved_account_names(
    value: &Value,
    matches: &mut u8,
    present: &mut bool,
) -> Result<(), AppError> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "SavedAccountNames" {
                    *matches = matches.saturating_add(1);
                    match child {
                        Value::String(value) => *present |= !value.trim().is_empty(),
                        Value::Array(values) => {
                            for value in values {
                                let Value::String(value) = value else {
                                    return Err(AppError::Login(
                                        "Battle.net.config 的 SavedAccountNames 字段类型未知，已停止读取"
                                            .into(),
                                    ));
                                };
                                *present |= !value.trim().is_empty();
                            }
                        }
                        _ => {
                            return Err(AppError::Login(
                                "Battle.net.config 的 SavedAccountNames 字段类型未知，已停止读取"
                                    .into(),
                            ));
                        }
                    }
                } else {
                    inspect_saved_account_names(child, matches, present)?;
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                inspect_saved_account_names(child, matches, present)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn clear_saved_account_names_in_value(value: &mut Value, matches: &mut u8) -> Result<(), AppError> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "SavedAccountNames" {
                    *matches = matches.saturating_add(1);
                    match child {
                        Value::String(value) => value.clear(),
                        Value::Array(values) => values.clear(),
                        _ => {
                            return Err(AppError::Login(
                                "Battle.net.config 的 SavedAccountNames 字段类型未知，已停止修改"
                                    .into(),
                            ));
                        }
                    }
                } else {
                    clear_saved_account_names_in_value(child, matches)?;
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                clear_saved_account_names_in_value(child, matches)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::super::{
        authentication::AuthenticationBaseline,
        recovery::{RecoveryKind, RecoveryState},
        restore::{CommitOutcome, RestoreEngine},
        snapshot::SnapshotStore,
    };
    use super::{LoginRecoveryOutcome, LoginSessionManager};
    use crate::contracts::{AccountKey, LoginIntent};

    fn allow_write() -> Result<(), crate::error::AppError> {
        Ok(())
    }

    fn reject_write() -> Result<(), crate::error::AppError> {
        Err(crate::error::AppError::Message("测试拒绝活动写入".into()))
    }

    fn previous_account() -> AccountKey {
        AccountKey {
            environment: "cn".into(),
            account_id: "before".into(),
        }
    }

    fn login_intent() -> LoginIntent {
        LoginIntent::Reauthenticate {
            account_key: previous_account(),
        }
    }

    fn authentication_baseline() -> AuthenticationBaseline {
        AuthenticationBaseline::default()
    }

    fn setup() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        LoginSessionManager,
        RestoreEngine,
    ) {
        let temporary = tempdir().unwrap();
        let data = temporary.path().join("data");
        let active = temporary.path().join("active");
        fs::create_dir_all(&active).unwrap();
        fs::write(
            active.join("Battle.net.config"),
            br#"{"Client":{"SavedAccountNames":"before"}}"#,
        )
        .unwrap();
        fs::write(active.join("other.config"), b"before-other").unwrap();
        let manager = LoginSessionManager::new(&data, &active).unwrap();
        let restore = RestoreEngine::new(&data, &active).unwrap();
        (temporary, active, manager, restore)
    }

    #[test]
    fn begin_clears_pointer_and_persists_awaiting_user() {
        let (temporary, active, manager, restore) = setup();
        let session = manager
            .begin(
                login_intent(),
                true,
                Some(previous_account()),
                authentication_baseline(),
                &restore,
                &allow_write,
            )
            .unwrap();

        assert!(
            fs::read_to_string(active.join("Battle.net.config"))
                .unwrap()
                .contains("\"SavedAccountNames\": \"\"")
        );
        assert_eq!(
            manager.recovery.record().unwrap().unwrap().state,
            RecoveryState::AwaitingUser
        );
        assert!(temporary.path().join("data/recovery/record.json").is_file());
        assert!(
            temporary
                .path()
                .join("data/recovery/before/files/Battle.net.config")
                .is_file()
        );
        assert_eq!(manager.snapshot().unwrap().unwrap().id, session.id);
        assert!(manager.require_completed_login_marker(&session.id).is_err());
        fs::write(
            active.join("Battle.net.config"),
            br#"{"Client":{"SavedAccountNames":["completed-login"]}}"#,
        )
        .unwrap();
        manager.require_completed_login_marker(&session.id).unwrap();
    }

    #[test]
    fn cancel_restores_exact_files() {
        let (_temporary, active, manager, restore) = setup();
        let session = manager
            .begin(
                login_intent(),
                true,
                None,
                authentication_baseline(),
                &restore,
                &allow_write,
            )
            .unwrap();
        fs::write(active.join("new-file"), b"login-change").unwrap();
        fs::write(active.join("other.config"), b"changed").unwrap();

        manager
            .rollback(&session.id, &restore, &allow_write)
            .unwrap();

        assert!(!active.join("new-file").exists());
        assert_eq!(
            fs::read(active.join("other.config")).unwrap(),
            b"before-other"
        );
        assert!(
            fs::read_to_string(active.join("Battle.net.config"))
                .unwrap()
                .contains("before")
        );
        assert!(manager.snapshot().unwrap().is_none());
    }

    #[test]
    fn awaiting_user_is_rolled_back_after_manager_recreation() {
        let (temporary, active, manager, restore) = setup();
        let intent = login_intent();
        let _session = manager
            .begin(
                intent.clone(),
                false,
                None,
                authentication_baseline(),
                &restore,
                &allow_write,
            )
            .unwrap();
        drop(manager);

        let reopened = LoginSessionManager::new(
            &temporary.path().join("data"),
            &temporary.path().join("active"),
        )
        .unwrap();

        assert!(reopened.needs_automatic_recovery().unwrap());
        assert!(reopened.automatic_recovery_requires_active_write().unwrap());
        assert_eq!(
            reopened.recover_if_needed(&restore, &allow_write).unwrap(),
            LoginRecoveryOutcome::RolledBack
        );
        assert!(reopened.snapshot().unwrap().is_none());
        assert_eq!(
            fs::read(active.join("other.config")).unwrap(),
            b"before-other"
        );
    }

    #[test]
    fn prepared_login_is_rolled_back_on_startup() {
        let (_temporary, active, manager, restore) = setup();
        let record = manager
            .recovery
            .prepare(
                &active,
                RecoveryKind::Login {
                    intent: login_intent(),
                    previous_account: Some(previous_account()),
                    created_at: crate::commands::now_epoch_ms(),
                    authentication_baseline: authentication_baseline(),
                },
                true,
            )
            .unwrap();
        fs::write(active.join("other.config"), b"partially-modified").unwrap();

        assert!(manager.needs_automatic_recovery().unwrap());
        assert_eq!(
            manager.recover_if_needed(&restore, &allow_write).unwrap(),
            LoginRecoveryOutcome::RolledBack
        );
        assert_eq!(
            fs::read(active.join("other.config")).unwrap(),
            b"before-other"
        );
        assert!(manager.snapshot().unwrap().is_none());
        drop(record);
    }

    #[test]
    fn committed_login_is_cleaned_without_rewriting_active_files() {
        let (_temporary, active, manager, restore) = setup();
        let session = manager
            .begin(
                login_intent(),
                false,
                None,
                authentication_baseline(),
                &restore,
                &allow_write,
            )
            .unwrap();
        fs::write(active.join("other.config"), b"logged-in").unwrap();
        manager
            .recovery
            .transition(&session.id, RecoveryState::Committed)
            .unwrap();

        assert!(!manager.automatic_recovery_requires_active_write().unwrap());
        assert_eq!(
            manager.recover_if_needed(&restore, &reject_write).unwrap(),
            LoginRecoveryOutcome::CleanedCompleted
        );
        assert_eq!(fs::read(active.join("other.config")).unwrap(), b"logged-in");
    }

    #[test]
    fn completed_session_keeps_logged_in_files() {
        let (temporary, active, manager, restore) = setup();
        let session = manager
            .begin(
                login_intent(),
                false,
                None,
                authentication_baseline(),
                &restore,
                &allow_write,
            )
            .unwrap();
        fs::write(active.join("other.config"), b"logged-in").unwrap();

        assert_eq!(
            manager.complete(&session.id).unwrap(),
            CommitOutcome::Committed
        );
        assert_eq!(fs::read(active.join("other.config")).unwrap(), b"logged-in");
        assert!(manager.snapshot().unwrap().is_none());
        assert_eq!(
            fs::read_dir(temporary.path().join("data/recovery"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn failed_awaiting_user_write_rolls_back_immediately() {
        let (_temporary, active, manager, restore) = setup();
        manager.recovery.fail_record_write_after(1);

        assert!(
            manager
                .begin(
                    login_intent(),
                    false,
                    None,
                    authentication_baseline(),
                    &restore,
                    &allow_write,
                )
                .is_err()
        );
        assert!(
            fs::read_to_string(active.join("Battle.net.config"))
                .unwrap()
                .contains("before")
        );
        assert!(manager.snapshot().unwrap().is_none());
    }

    #[test]
    fn failed_commit_write_leaves_awaiting_session_retryable() {
        let (_temporary, _active, manager, restore) = setup();
        let session = manager
            .begin(
                login_intent(),
                false,
                None,
                authentication_baseline(),
                &restore,
                &allow_write,
            )
            .unwrap();
        manager.recovery.fail_record_write_after(0);

        assert!(manager.complete(&session.id).is_err());
        assert_eq!(manager.snapshot().unwrap().unwrap().id, session.id);
        assert_eq!(
            manager.complete(&session.id).unwrap(),
            CommitOutcome::Committed
        );
    }

    #[test]
    fn duplicate_saved_account_fields_are_rejected_without_changes() {
        let (_temporary, active, manager, restore) = setup();
        let original = br#"{"A":{"SavedAccountNames":"one"},"B":{"SavedAccountNames":"two"}}"#;
        fs::write(active.join("Battle.net.config"), original).unwrap();

        assert!(
            manager
                .begin(
                    login_intent(),
                    false,
                    None,
                    authentication_baseline(),
                    &restore,
                    &allow_write,
                )
                .is_err()
        );
        assert_eq!(
            fs::read(active.join("Battle.net.config")).unwrap(),
            original
        );
        assert!(manager.snapshot().unwrap().is_none());
    }

    #[test]
    fn switch_and_login_share_one_recovery_slot() {
        let (temporary, active, manager, restore) = setup();
        manager
            .begin(
                login_intent(),
                false,
                None,
                authentication_baseline(),
                &restore,
                &allow_write,
            )
            .unwrap();

        let target_source = temporary.path().join("target");
        fs::create_dir_all(&target_source).unwrap();
        fs::write(target_source.join("Battle.net.config"), b"target").unwrap();
        let snapshots = SnapshotStore::new(&temporary.path().join("data")).unwrap();
        let target_key = AccountKey {
            environment: "cn".into(),
            account_id: "target".into(),
        };
        snapshots.save(&target_key, &target_source).unwrap();
        let target = snapshots.validate(&target_key).unwrap();

        assert!(!restore.has_pending_operation());
        assert!(manager.snapshot().unwrap().is_some());
        assert!(
            restore
                .apply_snapshot(&target, None, false, &allow_write)
                .is_err()
        );
        assert!(
            fs::read_to_string(active.join("Battle.net.config"))
                .unwrap()
                .contains("\"SavedAccountNames\": \"\"")
        );
        assert!(manager.snapshot().unwrap().is_some());
    }
}
