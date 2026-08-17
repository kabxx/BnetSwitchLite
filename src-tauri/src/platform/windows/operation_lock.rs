use std::{
    os::windows::ffi::OsStrExt,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION, HANDLE, WAIT_ABANDONED,
            WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Security::{GetLengthSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser},
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
            FILE_SHARE_MODE, OPEN_ALWAYS,
        },
        System::Threading::{
            CreateMutexW, GetCurrentProcess, OpenProcessToken, ReleaseMutex, WaitForSingleObject,
        },
    },
    core::{Error as WindowsError, HRESULT, PCWSTR},
};

use crate::error::AppError;

const MUTEX_PREFIX: &str = "Global\\BnetSwitchLite.Operation";

pub struct OperationGuard {
    mutex: HANDLE,
    file: HANDLE,
}

impl OperationGuard {
    pub fn acquire(data_directory: &Path, timeout: Duration) -> Result<Self, AppError> {
        let mutex_name = mutex_name()?;
        let mutex = unsafe { CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr())) }
            .map_err(|error| AppError::Transaction(format!("无法创建操作互斥锁：{error}")))?;

        let wait_millis = timeout.as_millis().min(u32::MAX as u128) as u32;
        let wait_result = unsafe { WaitForSingleObject(mutex, wait_millis) };
        if wait_result == WAIT_TIMEOUT {
            unsafe {
                let _ = CloseHandle(mutex);
            }
            return Err(AppError::OperationBusy);
        } else if wait_result != WAIT_OBJECT_0 && wait_result != WAIT_ABANDONED {
            unsafe {
                let _ = CloseHandle(mutex);
            }
            return Err(AppError::Transaction(format!(
                "无法获取操作互斥锁（Windows 状态 {}）",
                wait_result.0
            )));
        }

        let lock_path = data_directory.join(".operation.lock");
        match acquire_file_handle(&lock_path, timeout) {
            Ok(file) => Ok(Self { mutex, file }),
            Err(error) => {
                unsafe {
                    let _ = ReleaseMutex(mutex);
                    let _ = CloseHandle(mutex);
                }
                Err(error)
            }
        }
    }
}

fn mutex_name() -> Result<Vec<u16>, AppError> {
    let sid_hash = current_user_sid_hash()?;
    Ok(wide(std::ffi::OsStr::new(&format!(
        "{MUTEX_PREFIX}.{sid_hash}"
    ))))
}

fn current_user_sid_hash() -> Result<String, AppError> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|error| AppError::Transaction(format!("无法读取当前 Windows 用户：{error}")))?;

    let mut required = 0_u32;
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut required);
    }
    if required == 0 {
        unsafe {
            let _ = CloseHandle(token);
        }
        return Err(AppError::Transaction(
            "无法读取当前 Windows 用户标识".into(),
        ));
    }

    let mut buffer = vec![0_u8; required as usize];
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            required,
            &mut required,
        )
    };
    unsafe {
        let _ = CloseHandle(token);
    }
    result.map_err(|error| {
        AppError::Transaction(format!("无法读取当前 Windows 用户标识：{error}"))
    })?;

    let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let sid_length = unsafe { GetLengthSid(token_user.User.Sid) } as usize;
    if sid_length == 0 {
        return Err(AppError::Transaction("当前 Windows 用户标识无效".into()));
    }
    let sid_bytes =
        unsafe { std::slice::from_raw_parts(token_user.User.Sid.0.cast::<u8>(), sid_length) };
    Ok(format!("{:x}", Sha256::digest(sid_bytes)))
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.file);
            let _ = ReleaseMutex(self.mutex);
            let _ = CloseHandle(self.mutex);
        }
    }
}

fn acquire_file_handle(path: &Path, timeout: Duration) -> Result<HANDLE, AppError> {
    let path_wide = wide(path.as_os_str());
    let started = Instant::now();

    loop {
        let result = unsafe {
            CreateFileW(
                PCWSTR(path_wide.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        match result {
            Ok(handle) => return Ok(handle),
            Err(error) if is_lock_conflict(&error) && started.elapsed() < timeout => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) if is_lock_conflict(&error) => return Err(AppError::OperationBusy),
            Err(error) => {
                return Err(AppError::DataStorage(format!(
                    "无法打开操作锁 {}：{error}",
                    path.display()
                )));
            }
        }
    }
}

fn is_lock_conflict(error: &WindowsError) -> bool {
    error.code() == HRESULT::from_win32(ERROR_SHARING_VIOLATION.0)
        || error.code() == HRESULT::from_win32(ERROR_LOCK_VIOLATION.0)
}

fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{OperationGuard, is_lock_conflict};
    use crate::error::AppError;
    use std::time::Duration;
    use windows::{
        Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION},
        core::{Error as WindowsError, HRESULT},
    };

    #[test]
    fn file_lock_prevents_a_second_operation() {
        let temporary = tempfile::tempdir().unwrap();
        let first = OperationGuard::acquire(temporary.path(), Duration::from_millis(100)).unwrap();
        let second = OperationGuard::acquire(temporary.path(), Duration::from_millis(100));
        assert!(matches!(second, Err(AppError::OperationBusy)));

        let other_directory = temporary.path().join("other-data-copy");
        std::fs::create_dir(&other_directory).unwrap();
        let cross_copy = std::thread::spawn(move || {
            matches!(
                OperationGuard::acquire(&other_directory, Duration::from_millis(100)),
                Err(AppError::OperationBusy)
            )
        })
        .join()
        .unwrap();
        assert!(cross_copy);

        drop(first);
        OperationGuard::acquire(temporary.path(), Duration::from_millis(100)).unwrap();
    }

    #[test]
    fn only_lock_conflicts_are_reported_as_busy() {
        let sharing = WindowsError::from_hresult(HRESULT::from_win32(ERROR_SHARING_VIOLATION.0));
        let denied = WindowsError::from_hresult(HRESULT::from_win32(ERROR_ACCESS_DENIED.0));
        assert!(is_lock_conflict(&sharing));
        assert!(!is_lock_conflict(&denied));
    }
}
