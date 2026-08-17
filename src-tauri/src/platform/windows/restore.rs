use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufReader, Read, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use atomicwrites::{AtomicFile, OverwriteBehavior};
use sha2::{Digest, Sha256};
use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::PCWSTR};

use crate::{contracts::AccountKey, error::AppError};

use super::{
    recovery::{RecoveryCommitOutcome, RecoveryKind, RecoveryRecord, RecoveryState, RecoveryStore},
    snapshot::{
        ManifestFile, ValidatedSnapshot, enumerate_regular_files, reject_reparse_point,
        validate_file_set,
    },
};

const MINIMUM_FREE_SPACE_RESERVE: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct RestoreReceipt {
    pub operation_id: String,
    pub previous_client_was_running: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryOutcome {
    None,
    RolledBack,
    CleanedCompleted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CommitOutcome {
    Committed,
    CleanupPending(String),
}

pub(crate) struct RestoreEngine {
    active_directory: PathBuf,
    recovery: RecoveryStore,
}

impl RestoreEngine {
    pub fn new(data_directory: &Path, active_directory: &Path) -> Result<Self, AppError> {
        fs::create_dir_all(data_directory)
            .map_err(|error| AppError::Transaction(format!("无法创建便携数据目录：{error}")))?;
        reject_reparse_point(data_directory)?;
        if active_directory.exists() {
            reject_reparse_point(active_directory)?;
        }
        Ok(Self {
            active_directory: active_directory.to_path_buf(),
            recovery: RecoveryStore::new(data_directory)?,
        })
    }

    pub fn has_pending_operation(&self) -> bool {
        match self.recovery.record() {
            Ok(Some(record)) => matches!(record.kind, RecoveryKind::Switch { .. }),
            Ok(None) => false,
            Err(_) => true,
        }
    }

    pub fn pending_client_restart_required(&self) -> Result<bool, AppError> {
        Ok(self
            .switch_record()?
            .map(|record| record.previous_client_was_running)
            .unwrap_or(false))
    }

    pub fn pending_recovery_requires_active_write(&self) -> Result<bool, AppError> {
        Ok(matches!(
            self.switch_record()?,
            Some(record) if record.state == RecoveryState::Prepared
        ))
    }

    pub fn apply_snapshot(
        &self,
        target: &ValidatedSnapshot,
        previous_account: Option<AccountKey>,
        previous_client_was_running: bool,
        before_active_write: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<RestoreReceipt, AppError> {
        if self.recovery.record()?.is_some() {
            return Err(AppError::Transaction(
                "检测到未完成的账号操作，请先恢复后再试".into(),
            ));
        }
        if !self.active_directory.is_dir() {
            return Err(AppError::Transaction(
                "未找到 Battle.net 配置目录，请先登录一次战网客户端".into(),
            ));
        }
        validate_file_set(&target.files_directory, &target.files)?;
        reject_reparse_point(&self.active_directory)?;
        ensure_available_space(&self.active_directory, &target.files)?;

        let record = self.recovery.prepare(
            &self.active_directory,
            RecoveryKind::Switch {
                target_account: target.account.clone(),
                previous_account,
            },
            previous_client_was_running,
        )?;

        if let Err(error) = before_active_write() {
            let _ = self.recovery.discard_prepared(&record.id);
            return Err(error);
        }

        if let Err(error) = self.replace_active_file_set(&target.files_directory, &target.files) {
            let error = error.nested_message();
            let rollback = self.rollback_record(&record.id, before_active_write);
            return match rollback {
                Ok(()) => Err(AppError::Transaction(format!(
                    "应用目标账号失败，已恢复原配置：{error}"
                ))),
                Err(rollback_error) => Err(AppError::Transaction(format!(
                    "应用目标账号失败且自动恢复未完成：{error}；{}",
                    rollback_error.nested_message()
                ))),
            };
        }

        Ok(RestoreReceipt {
            operation_id: record.id,
            previous_client_was_running,
        })
    }

    pub fn commit(&self, receipt: &RestoreReceipt) -> Result<CommitOutcome, AppError> {
        let record = self.require_switch(&receipt.operation_id)?;
        if record.state != RecoveryState::Prepared {
            return Err(AppError::Transaction("当前事务不能提交".into()));
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
        receipt: &RestoreReceipt,
        before_active_write: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<(), AppError> {
        self.rollback_record(&receipt.operation_id, before_active_write)
    }

    pub fn recover_pending(
        &self,
        before_active_write: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<RecoveryOutcome, AppError> {
        let Some(record) = self.switch_record()? else {
            return Ok(RecoveryOutcome::None);
        };
        match record.state {
            RecoveryState::Prepared => {
                self.rollback_record(&record.id, before_active_write)?;
                Ok(RecoveryOutcome::RolledBack)
            }
            RecoveryState::Committed => {
                self.recovery.cleanup_committed(&record.id)?;
                Ok(RecoveryOutcome::CleanedCompleted)
            }
            RecoveryState::AwaitingUser => Err(AppError::Transaction(
                "恢复记录属于未完成登录流程，请先完成或取消登录".into(),
            )),
        }
    }

    pub(crate) fn replace_active_file_set(
        &self,
        source_directory: &Path,
        expected: &[ManifestFile],
    ) -> Result<(), AppError> {
        reject_reparse_point(&self.active_directory)?;
        validate_file_set(source_directory, expected)?;
        let current = enumerate_regular_files(&self.active_directory)?;
        let target_names = expected
            .iter()
            .map(|file| file.name.as_str())
            .collect::<BTreeSet<_>>();

        for file in expected {
            atomic_replace_verified(
                &source_directory.join(&file.name),
                &self.active_directory.join(&file.name),
                file,
            )?;
        }
        for (name, path) in current {
            if !target_names.contains(name.as_str()) {
                fs::remove_file(&path).map_err(|error| {
                    AppError::Transaction(format!("无法移除旧的战网配置文件 {name}：{error}"))
                })?;
            }
        }
        validate_file_set(&self.active_directory, expected)
    }

    fn rollback_record(
        &self,
        id: &str,
        before_active_write: &dyn Fn() -> Result<(), AppError>,
    ) -> Result<(), AppError> {
        let record = self.require_switch(id)?;
        self.recovery
            .rollback(&record.id, before_active_write, &|source, expected| {
                self.replace_active_file_set(source, expected)
            })
    }

    fn switch_record(&self) -> Result<Option<RecoveryRecord>, AppError> {
        match self.recovery.record()? {
            Some(record) if matches!(record.kind, RecoveryKind::Switch { .. }) => Ok(Some(record)),
            Some(_) => Ok(None),
            None => Ok(None),
        }
    }

    fn require_switch(&self, id: &str) -> Result<RecoveryRecord, AppError> {
        let record = self
            .recovery
            .record()?
            .ok_or_else(|| AppError::Transaction("切换恢复记录不存在".into()))?;
        if record.id != id || !matches!(record.kind, RecoveryKind::Switch { .. }) {
            return Err(AppError::Transaction("切换事务标识不匹配".into()));
        }
        Ok(record)
    }

    #[cfg(test)]
    fn fail_recovery_write_after(&self, successful_writes: usize) {
        self.recovery.fail_record_write_after(successful_writes);
    }
}

fn ensure_available_space(directory: &Path, files: &[ManifestFile]) -> Result<(), AppError> {
    let path = directory
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut available = 0_u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(path.as_ptr()), Some(&mut available), None, None) }
        .map_err(|error| AppError::Transaction(format!("无法读取战网配置盘可用空间：{error}")))?;
    let required = files
        .iter()
        .try_fold(MINIMUM_FREE_SPACE_RESERVE, |total, file| {
            total.checked_add(file.size)
        })
        .ok_or_else(|| AppError::Transaction("目标账号快照大小无效".into()))?;
    if available < required {
        return Err(AppError::Transaction(format!(
            "战网配置盘可用空间不足，至少需要 {} MiB",
            required.div_ceil(1024 * 1024)
        )));
    }
    Ok(())
}

fn atomic_replace_verified(
    source: &Path,
    destination: &Path,
    expected: &ManifestFile,
) -> Result<(), AppError> {
    let result = AtomicFile::new(destination, OverwriteBehavior::AllowOverwrite).write(|file| {
        let mut reader = BufReader::new(File::open(source)?);
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            file.write_all(&buffer[..count])?;
            hasher.update(&buffer[..count]);
            total += count as u64;
        }
        file.sync_all()?;
        let hash = format!("{:x}", hasher.finalize());
        if total != expected.size || hash != expected.sha256 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "source changed while applying snapshot",
            ));
        }
        Ok(())
    });
    result
        .map_err(Into::<std::io::Error>::into)
        .map_err(|error| {
            AppError::Transaction(format!("无法原子替换配置文件 {}：{error}", expected.name))
        })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::super::{
        recovery::RecoveryState,
        snapshot::{SnapshotStore, ValidatedSnapshot},
    };
    use super::{CommitOutcome, RecoveryOutcome, RestoreEngine};
    use crate::contracts::AccountKey;

    fn allow_write() -> Result<(), crate::error::AppError> {
        Ok(())
    }

    fn reject_write() -> Result<(), crate::error::AppError> {
        Err(crate::error::AppError::Message("测试拒绝活动写入".into()))
    }

    fn key(id: &str) -> AccountKey {
        AccountKey {
            environment: "cn".into(),
            account_id: id.into(),
        }
    }

    fn write_files(directory: &std::path::Path, files: &[(&str, &[u8])]) {
        fs::create_dir_all(directory).unwrap();
        for (name, contents) in files {
            fs::write(directory.join(name), contents).unwrap();
        }
    }

    fn setup() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        ValidatedSnapshot,
        RestoreEngine,
    ) {
        let temporary = tempdir().unwrap();
        let data = temporary.path().join("data");
        let target_source = temporary.path().join("target");
        let active = temporary.path().join("active");
        write_files(
            &target_source,
            &[("Battle.net.config", b"target"), ("target-only", b"new")],
        );
        write_files(
            &active,
            &[("Battle.net.config", b"before"), ("old-only", b"old")],
        );
        let snapshots = SnapshotStore::new(&data).unwrap();
        snapshots.save(&key("target"), &target_source).unwrap();
        let target = snapshots.validate(&key("target")).unwrap();
        let engine = RestoreEngine::new(&data, &active).unwrap();
        (temporary, active, target, engine)
    }

    #[test]
    fn restore_and_rollback_reproduce_exact_file_sets() {
        let (_temporary, active, target, engine) = setup();
        let receipt = engine
            .apply_snapshot(&target, Some(key("before")), true, &allow_write)
            .unwrap();

        assert_eq!(
            fs::read(active.join("Battle.net.config")).unwrap(),
            b"target"
        );
        assert!(active.join("target-only").exists());
        assert!(!active.join("old-only").exists());

        engine.rollback(&receipt, &allow_write).unwrap();
        assert_eq!(
            fs::read(active.join("Battle.net.config")).unwrap(),
            b"before"
        );
        assert!(active.join("old-only").exists());
        assert!(!active.join("target-only").exists());
        assert!(!engine.has_pending_operation());
    }

    #[test]
    fn write_guard_failure_leaves_active_files_untouched() {
        let (_temporary, active, target, engine) = setup();

        assert!(
            engine
                .apply_snapshot(&target, None, false, &reject_write)
                .is_err()
        );
        assert_eq!(
            fs::read(active.join("Battle.net.config")).unwrap(),
            b"before"
        );
        assert!(active.join("old-only").exists());
        assert!(!active.join("target-only").exists());
        assert!(!engine.has_pending_operation());
    }

    #[test]
    fn startup_recovery_rolls_back_prepared_switch() {
        let (_temporary, active, target, engine) = setup();
        engine
            .apply_snapshot(&target, Some(key("before")), true, &allow_write)
            .unwrap();

        assert_eq!(
            engine.recover_pending(&allow_write).unwrap(),
            RecoveryOutcome::RolledBack
        );
        assert_eq!(
            fs::read(active.join("Battle.net.config")).unwrap(),
            b"before"
        );
        assert!(active.join("old-only").exists());
        assert!(!engine.has_pending_operation());
    }

    #[test]
    fn commit_keeps_target_and_removes_recovery_data() {
        let (temporary, active, target, engine) = setup();
        let receipt = engine
            .apply_snapshot(&target, None, false, &allow_write)
            .unwrap();

        assert_eq!(engine.commit(&receipt).unwrap(), CommitOutcome::Committed);
        assert_eq!(
            fs::read(active.join("Battle.net.config")).unwrap(),
            b"target"
        );
        assert_eq!(
            fs::read_dir(temporary.path().join("data/recovery"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn committed_record_is_cleaned_without_rolling_back() {
        let (_temporary, active, target, engine) = setup();
        let receipt = engine
            .apply_snapshot(&target, None, false, &allow_write)
            .unwrap();
        engine
            .recovery
            .transition(&receipt.operation_id, RecoveryState::Committed)
            .unwrap();

        assert!(!engine.pending_recovery_requires_active_write().unwrap());
        assert_eq!(
            engine.recover_pending(&reject_write).unwrap(),
            RecoveryOutcome::CleanedCompleted
        );
        assert_eq!(
            fs::read(active.join("Battle.net.config")).unwrap(),
            b"target"
        );
    }

    #[test]
    fn failed_commit_record_write_is_not_reported_as_success() {
        let (_temporary, active, target, engine) = setup();
        let receipt = engine
            .apply_snapshot(&target, None, false, &allow_write)
            .unwrap();
        engine.fail_recovery_write_after(0);

        assert!(engine.commit(&receipt).is_err());
        engine.rollback(&receipt, &allow_write).unwrap();
        assert_eq!(
            fs::read(active.join("Battle.net.config")).unwrap(),
            b"before"
        );
        assert!(!engine.has_pending_operation());
    }

    #[test]
    fn second_switch_cannot_replace_the_active_recovery_record() {
        let (_temporary, _active, target, engine) = setup();
        engine
            .apply_snapshot(&target, None, false, &allow_write)
            .unwrap();

        assert!(
            engine
                .apply_snapshot(&target, None, false, &allow_write)
                .is_err()
        );
        assert_eq!(
            engine.recovery.record().unwrap().unwrap().state,
            RecoveryState::Prepared
        );
    }
}
