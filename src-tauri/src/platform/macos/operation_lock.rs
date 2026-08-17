use std::{
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    thread,
    time::{Duration, Instant},
};

use crate::error::AppError;

pub(crate) struct OperationGuard {
    file: File,
}

impl OperationGuard {
    pub fn acquire(data_directory: &Path, timeout: Duration) -> Result<Self, AppError> {
        let lock_path = data_directory.join(".operation.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)
            .map_err(|error| {
                AppError::DataStorage(format!("无法打开操作锁 {}：{error}", lock_path.display()))
            })?;
        validate_lock_identity(&lock_path, &file)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| AppError::DataStorage(format!("无法收紧操作锁权限：{error}")))?;
        validate_lock_handle(&lock_path, &file)?;
        let started = Instant::now();
        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result == 0 {
                return Ok(Self { file });
            }
            let error = std::io::Error::last_os_error();
            let busy = matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK));
            if !busy {
                return Err(AppError::DataStorage(format!("无法获取操作锁：{error}")));
            }
            if started.elapsed() >= timeout {
                return Err(AppError::OperationBusy);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn validate_lock_identity(path: &Path, file: &File) -> Result<(), AppError> {
    let metadata = file.metadata().map_err(|error| {
        AppError::DataStorage(format!("无法读取操作锁属性 {}：{error}", path.display()))
    })?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err(AppError::DataStorage(format!(
            "操作锁必须是当前用户独占拥有的普通文件：{}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_lock_handle(path: &Path, file: &File) -> Result<(), AppError> {
    let metadata = file.metadata().map_err(|error| {
        AppError::DataStorage(format!("无法读取操作锁属性 {}：{error}", path.display()))
    })?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(AppError::DataStorage(format!(
            "操作锁必须是当前用户独占拥有且权限为 0600 的普通文件：{}",
            path.display()
        )));
    }
    Ok(())
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}
