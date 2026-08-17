use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(test)]
use std::cell::Cell;

use crate::{
    contracts::{AccountKey, LoginIntent},
    data_store::write_json_atomic,
    error::AppError,
};

use super::authentication::AuthenticationBaseline;
use super::snapshot::{
    ManifestFile, capture_file_set, read_file_set_manifest, reject_reparse_point,
    validate_file_set, write_file_set_manifest,
};

const RECORD_VERSION: u8 = 1;
const RECORD_FILE_NAME: &str = "record.json";
const BEFORE_DIRECTORY_NAME: &str = "before";
const BEFORE_MANIFEST_NAME: &str = "manifest.json";
const PREPARING_PREFIX: &str = ".preparing-";

type ReplaceFileSet<'a> = dyn Fn(&Path, &[ManifestFile]) -> Result<(), AppError> + 'a;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RecoveryState {
    Prepared,
    AwaitingUser,
    Committed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum RecoveryKind {
    Switch {
        target_account: AccountKey,
        previous_account: Option<AccountKey>,
    },
    Login {
        intent: LoginIntent,
        previous_account: Option<AccountKey>,
        created_at: u64,
        authentication_baseline: AuthenticationBaseline,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecoveryRecord {
    pub version: u8,
    pub id: String,
    pub state: RecoveryState,
    pub previous_client_was_running: bool,
    #[serde(flatten)]
    pub kind: RecoveryKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryCommitOutcome {
    Committed,
    CleanupPending(String),
}

pub(crate) struct RecoveryStore {
    root_directory: PathBuf,
    record_path: PathBuf,
    before_directory: PathBuf,
    #[cfg(test)]
    record_write_failure_after: Cell<Option<usize>>,
}

impl RecoveryStore {
    pub fn new(data_directory: &Path) -> Result<Self, AppError> {
        let root_directory = data_directory.join("recovery");
        fs::create_dir_all(&root_directory)
            .map_err(|error| recovery_error(format!("无法创建恢复目录：{error}")))?;
        reject_reparse_point(&root_directory)?;

        let store = Self {
            record_path: root_directory.join(RECORD_FILE_NAME),
            before_directory: root_directory.join(BEFORE_DIRECTORY_NAME),
            root_directory,
            #[cfg(test)]
            record_write_failure_after: Cell::new(None),
        };
        if !store.record_path.exists() {
            store.cleanup_orphaned_preparation()?;
        }
        Ok(store)
    }

    pub fn record(&self) -> Result<Option<RecoveryRecord>, AppError> {
        if !self.record_path.exists() {
            return Ok(None);
        }
        self.load_record().map(Some)
    }

    pub fn prepare(
        &self,
        active_directory: &Path,
        kind: RecoveryKind,
        previous_client_was_running: bool,
    ) -> Result<RecoveryRecord, AppError> {
        self.ensure_empty()?;
        reject_reparse_point(active_directory)?;

        let id = Uuid::new_v4().to_string();
        let staging_directory = self.root_directory.join(format!("{PREPARING_PREFIX}{id}"));
        let staging_before = staging_directory.join(BEFORE_DIRECTORY_NAME);
        fs::create_dir(&staging_directory)
            .map_err(|error| recovery_error(format!("无法创建恢复暂存目录：{error}")))?;

        let preparation = (|| {
            let before = capture_file_set(active_directory, &staging_before)?;
            write_file_set_manifest(&staging_before.join(BEFORE_MANIFEST_NAME), &before)?;
            validate_file_set(&staging_before.join("files"), &before.files)?;
            fs::rename(&staging_before, &self.before_directory)
                .map_err(|error| recovery_error(format!("无法发布恢复副本：{error}")))?;
            fs::remove_dir(&staging_directory)
                .map_err(|error| recovery_error(format!("无法清理恢复暂存目录：{error}")))?;

            let record = RecoveryRecord {
                version: RECORD_VERSION,
                id,
                state: RecoveryState::Prepared,
                previous_client_was_running,
                kind,
            };
            self.save_record(&record)?;
            Ok(record)
        })();

        if preparation.is_err() {
            if staging_directory.exists() {
                let _ = remove_directory_checked(&staging_directory);
            }
            if !self.record_path.exists() && self.before_directory.exists() {
                let _ = remove_directory_checked(&self.before_directory);
            }
        }
        preparation
    }

    pub fn transition(
        &self,
        id: &str,
        next_state: RecoveryState,
    ) -> Result<RecoveryRecord, AppError> {
        let mut record = self.require(id)?;
        let valid = matches!(
            (&record.kind, record.state, next_state),
            (
                RecoveryKind::Switch { .. },
                RecoveryState::Prepared,
                RecoveryState::Committed
            ) | (
                RecoveryKind::Login { .. },
                RecoveryState::Prepared,
                RecoveryState::AwaitingUser
            ) | (
                RecoveryKind::Login { .. },
                RecoveryState::AwaitingUser,
                RecoveryState::Committed
            )
        );
        if !valid {
            return Err(recovery_error("恢复记录状态转换无效"));
        }
        record.state = next_state;
        self.save_record(&record)?;
        Ok(record)
    }

    pub fn commit(&self, id: &str) -> Result<RecoveryCommitOutcome, AppError> {
        self.transition(id, RecoveryState::Committed)?;
        match self.cleanup() {
            Ok(()) => Ok(RecoveryCommitOutcome::Committed),
            Err(error) => Ok(RecoveryCommitOutcome::CleanupPending(error.to_string())),
        }
    }

    pub fn discard_prepared(&self, id: &str) -> Result<(), AppError> {
        let record = self.require(id)?;
        if record.state != RecoveryState::Prepared {
            return Err(recovery_error("只能放弃尚未写入活动配置的恢复记录"));
        }
        self.cleanup()
    }

    pub fn rollback(
        &self,
        id: &str,
        before_active_write: &dyn Fn() -> Result<(), AppError>,
        replace_active_file_set: &ReplaceFileSet<'_>,
    ) -> Result<(), AppError> {
        let record = self.require(id)?;
        if record.state == RecoveryState::Committed {
            return Err(recovery_error("已提交的操作不能回滚"));
        }
        let before = self.load_and_validate_before()?;
        before_active_write()?;
        replace_active_file_set(&self.before_directory.join("files"), &before)?;
        self.cleanup()
    }

    pub fn cleanup_committed(&self, id: &str) -> Result<(), AppError> {
        let record = self.require(id)?;
        if record.state != RecoveryState::Committed {
            return Err(recovery_error("恢复记录尚未提交"));
        }
        self.cleanup()
    }

    fn require(&self, id: &str) -> Result<RecoveryRecord, AppError> {
        Uuid::parse_str(id).map_err(|_| recovery_error("恢复记录标识无效"))?;
        let record = self.load_record()?;
        if record.id != id {
            return Err(recovery_error("恢复记录标识不匹配"));
        }
        Ok(record)
    }

    fn load_record(&self) -> Result<RecoveryRecord, AppError> {
        let bytes = fs::read(&self.record_path)
            .map_err(|error| recovery_error(format!("无法读取恢复记录：{error}")))?;
        let record: RecoveryRecord = serde_json::from_slice(&bytes)
            .map_err(|error| recovery_error(format!("恢复记录格式无效：{error}")))?;
        if record.version != RECORD_VERSION {
            return Err(recovery_error(format!(
                "不支持的恢复记录版本：{}",
                record.version
            )));
        }
        Uuid::parse_str(&record.id).map_err(|_| recovery_error("恢复记录标识无效"))?;
        validate_record_state(&record)?;
        Ok(record)
    }

    fn save_record(&self, record: &RecoveryRecord) -> Result<(), AppError> {
        #[cfg(test)]
        if let Some(remaining) = self.record_write_failure_after.get() {
            if remaining == 0 {
                self.record_write_failure_after.set(None);
                return Err(recovery_error("测试注入：无法持久化恢复记录"));
            }
            self.record_write_failure_after.set(Some(remaining - 1));
        }
        validate_record_state(record)?;
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| recovery_error(format!("无法生成恢复记录：{error}")))?;
        write_json_atomic(&self.record_path, &bytes)
            .map_err(|error| recovery_error(format!("无法持久化恢复记录：{error}")))
    }

    fn load_and_validate_before(&self) -> Result<Vec<ManifestFile>, AppError> {
        reject_reparse_point(&self.before_directory)?;
        let manifest = read_file_set_manifest(&self.before_directory.join(BEFORE_MANIFEST_NAME))?;
        validate_file_set(&self.before_directory.join("files"), &manifest.files)?;
        Ok(manifest.files)
    }

    fn ensure_empty(&self) -> Result<(), AppError> {
        if self.record_path.exists() {
            return Err(recovery_error(
                "已有账号操作正在等待完成或恢复，请先处理后再试",
            ));
        }
        let mut entries = fs::read_dir(&self.root_directory)
            .map_err(|error| recovery_error(format!("无法读取恢复目录：{error}")))?;
        if entries
            .next()
            .transpose()
            .map_err(|error| recovery_error(format!("无法读取恢复目录项：{error}")))?
            .is_some()
        {
            return Err(recovery_error(
                "恢复目录包含未识别的数据，请关闭战网并重新启动本工具",
            ));
        }
        Ok(())
    }

    fn cleanup(&self) -> Result<(), AppError> {
        if self.record_path.exists() {
            fs::remove_file(&self.record_path)
                .map_err(|error| recovery_error(format!("无法清理恢复记录：{error}")))?;
        }
        if self.before_directory.exists() {
            remove_directory_checked(&self.before_directory)?;
        }
        Ok(())
    }

    fn cleanup_orphaned_preparation(&self) -> Result<(), AppError> {
        if self.before_directory.exists() {
            remove_directory_checked(&self.before_directory)?;
        }
        for entry in fs::read_dir(&self.root_directory)
            .map_err(|error| recovery_error(format!("无法读取恢复目录：{error}")))?
        {
            let entry =
                entry.map_err(|error| recovery_error(format!("无法读取恢复目录项：{error}")))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_prefix(PREPARING_PREFIX) else {
                return Err(recovery_error(format!("恢复目录包含未识别的项目：{name}")));
            };
            Uuid::parse_str(id)
                .map_err(|_| recovery_error(format!("恢复暂存目录名称无效：{name}")))?;
            remove_directory_checked(&entry.path())?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn fail_record_write_after(&self, successful_writes: usize) {
        self.record_write_failure_after.set(Some(successful_writes));
    }
}

fn validate_record_state(record: &RecoveryRecord) -> Result<(), AppError> {
    let valid = matches!(
        (&record.kind, record.state),
        (RecoveryKind::Switch { .. }, RecoveryState::Prepared)
            | (RecoveryKind::Switch { .. }, RecoveryState::Committed)
            | (RecoveryKind::Login { .. }, RecoveryState::Prepared)
            | (RecoveryKind::Login { .. }, RecoveryState::AwaitingUser)
            | (RecoveryKind::Login { .. }, RecoveryState::Committed)
    );
    if valid {
        Ok(())
    } else {
        Err(recovery_error("恢复记录包含不适用于当前操作的状态"))
    }
}

fn remove_directory_checked(path: &Path) -> Result<(), AppError> {
    reject_reparse_point(path)?;
    fs::remove_dir_all(path).map_err(|error| recovery_error(format!("无法清理恢复目录：{error}")))
}

fn recovery_error(message: impl Into<String>) -> AppError {
    AppError::Message(format!("恢复数据不可用：{}", message.into()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{RecoveryKind, RecoveryState, RecoveryStore};
    use crate::contracts::AccountKey;

    fn key(id: &str) -> AccountKey {
        AccountKey {
            environment: "cn".into(),
            account_id: id.into(),
        }
    }

    #[test]
    fn preparation_publishes_one_record_and_one_before_image() {
        let temporary = tempdir().unwrap();
        let data = temporary.path().join("data");
        let active = temporary.path().join("active");
        fs::create_dir_all(&active).unwrap();
        fs::write(active.join("Battle.net.config"), b"before").unwrap();
        let store = RecoveryStore::new(&data).unwrap();

        let record = store
            .prepare(
                &active,
                RecoveryKind::Switch {
                    target_account: key("target"),
                    previous_account: Some(key("before")),
                },
                true,
            )
            .unwrap();

        assert_eq!(record.state, RecoveryState::Prepared);
        let names = fs::read_dir(data.join("recovery"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            ["before".to_owned(), "record.json".to_owned()].into()
        );
        assert!(data.join("recovery/before/manifest.json").is_file());
        assert!(
            data.join("recovery/before/files/Battle.net.config")
                .is_file()
        );
    }

    #[test]
    fn startup_removes_orphaned_preparation_without_a_record() {
        let temporary = tempdir().unwrap();
        let data = temporary.path().join("data");
        fs::create_dir_all(data.join("recovery/before/files")).unwrap();
        fs::write(
            data.join("recovery/before/files/Battle.net.config"),
            b"before",
        )
        .unwrap();

        RecoveryStore::new(&data).unwrap();

        assert_eq!(fs::read_dir(data.join("recovery")).unwrap().count(), 0);
    }

    #[test]
    fn invalid_state_for_operation_kind_is_rejected() {
        let temporary = tempdir().unwrap();
        let data = temporary.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let store = RecoveryStore::new(&data).unwrap();
        let invalid = serde_json::json!({
            "version": 1,
            "id": uuid::Uuid::new_v4().to_string(),
            "state": "awaitingUser",
            "previousClientWasRunning": false,
            "kind": "switch",
            "targetAccount": { "environment": "cn", "accountId": "1" },
            "previousAccount": null
        });
        fs::write(
            data.join("recovery/record.json"),
            serde_json::to_vec(&invalid).unwrap(),
        )
        .unwrap();

        assert!(store.record().is_err());
    }

    #[test]
    fn record_uses_only_minimal_recovery_state() {
        let temporary = tempdir().unwrap();
        let data = temporary.path().join("data");
        let active = temporary.path().join("active");
        fs::create_dir_all(&active).unwrap();
        fs::write(active.join("Battle.net.config"), b"before").unwrap();
        let store = RecoveryStore::new(&data).unwrap();
        let record = store
            .prepare(
                &active,
                RecoveryKind::Switch {
                    target_account: key("target"),
                    previous_account: None,
                },
                false,
            )
            .unwrap();

        let json = fs::read_to_string(data.join("recovery/record.json")).unwrap();
        assert!(json.contains("\"state\": \"prepared\""));
        assert!(!json.contains("launching"));
        assert!(!json.contains("verifying"));
        assert!(!json.contains("rollingBack"));

        store
            .transition(&record.id, RecoveryState::Committed)
            .unwrap();
        let committed = fs::read_to_string(data.join("recovery/record.json")).unwrap();
        assert!(committed.contains("\"state\": \"committed\""));
    }
}
