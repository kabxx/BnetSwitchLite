use std::{
    collections::HashSet,
    mem::size_of,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HWND, LPARAM, WPARAM},
    System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    },
    UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, SEND_MESSAGE_TIMEOUT_FLAGS, SendMessageTimeoutW,
    },
};
use windows::core::{BOOL, Error as WindowsError, HRESULT};

use crate::{contracts::LoginRegion, error::AppError};

use super::paths::validate_client_executable;

const CLIENT_PROCESS_NAMES: [&str; 3] = [
    "Battle.net.exe",
    "Battle.net Launcher.exe",
    "Battle.net Helper.exe",
];
const WINDOW_PROCESS_NAMES: [&str; 2] = ["Battle.net.exe", "Battle.net Launcher.exe"];
const WM_QUERYENDSESSION: u32 = 0x0011;
const WM_ENDSESSION: u32 = 0x0016;
const SMTO_ABORTIFHUNG: SEND_MESSAGE_TIMEOUT_FLAGS = SEND_MESSAGE_TIMEOUT_FLAGS(0x0002);
const REQUIRED_EMPTY_SAMPLES: u8 = 2;

pub fn is_client_running() -> Result<bool, AppError> {
    Ok(!client_processes()?.is_empty())
}

pub fn is_main_client_running() -> Result<bool, AppError> {
    Ok(client_processes()?.iter().any(|process| {
        WINDOW_PROCESS_NAMES
            .iter()
            .any(|name| process.name.eq_ignore_ascii_case(name))
    }))
}

pub fn ensure_client_stopped(timeout: Duration) -> Result<(), AppError> {
    let deadline = Instant::now() + timeout;
    let mut empty_samples = 0_u8;
    while Instant::now() < deadline {
        if client_processes()?.is_empty() {
            empty_samples += 1;
            if empty_samples >= REQUIRED_EMPTY_SAMPLES {
                return Ok(());
            }
        } else {
            return Err(AppError::Message(
                "检测到 Battle.net 已重新启动，已在修改配置前取消操作".into(),
            ));
        }
        thread::sleep(Duration::from_millis(150));
    }
    Err(AppError::Message(
        "无法确认 Battle.net 已完全退出，已取消操作".into(),
    ))
}

#[cfg(test)]
pub fn client_process_ids() -> Result<Vec<u32>, AppError> {
    Ok(client_processes()?
        .into_iter()
        .map(|process| process.process_id)
        .collect())
}

pub fn graceful_stop(timeout: Duration) -> Result<(), AppError> {
    let deadline = Instant::now() + timeout;
    let processes = client_processes()?;
    if processes.is_empty() {
        return Ok(());
    }

    let window_process_ids = processes
        .iter()
        .filter(|process| {
            WINDOW_PROCESS_NAMES
                .iter()
                .any(|name| process.name.eq_ignore_ascii_case(name))
        })
        .map(|process| process.process_id)
        .collect::<HashSet<_>>();
    if !window_process_ids.is_empty() {
        send_close_messages(&window_process_ids, deadline)?;
    }

    let mut empty_samples = 0_u8;
    while Instant::now() < deadline {
        if client_processes()?.is_empty() {
            empty_samples += 1;
            if empty_samples >= REQUIRED_EMPTY_SAMPLES {
                return Ok(());
            }
        } else {
            empty_samples = 0;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            thread::sleep(remaining.min(Duration::from_millis(250)));
        }
    }
    Err(AppError::ClientStopTimeout)
}

pub fn launch(executable: &Path) -> Result<(), AppError> {
    launch_with_region(executable, None)
}

pub fn launch_for_login(executable: &Path, region: Option<LoginRegion>) -> Result<(), AppError> {
    launch_with_region(executable, region)
}

fn launch_with_region(executable: &Path, region: Option<LoginRegion>) -> Result<(), AppError> {
    let executable = validate_client_executable(executable)?;
    let working_directory = executable
        .parent()
        .ok_or_else(|| AppError::ClientLaunch("无法确定 Battle.net.exe 的所在目录".into()))?;
    let mut command = Command::new(&executable);
    command.current_dir(working_directory);
    if let Some(region) = region {
        command.arg(login_region_argument(region));
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| AppError::ClientLaunch(error.to_string()))
}

fn login_region_argument(region: LoginRegion) -> String {
    format!("--setregion={}", region.launch_code())
}

pub fn wait_for_client_started(timeout: Duration) -> Result<(), AppError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_client_running()? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(AppError::ClientLaunch(
        "启动命令已发出，但未检测到 Battle.net 进程".into(),
    ))
}

#[derive(Debug)]
struct ClientProcess {
    process_id: u32,
    name: String,
}

fn client_processes() -> Result<Vec<ClientProcess>, AppError> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|error| AppError::Message(format!("无法读取 Windows 进程列表：{error}")))?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let result = (|| {
        let mut processes = Vec::new();
        match unsafe { Process32FirstW(snapshot, &mut entry) } {
            Ok(()) => {}
            Err(error) if is_process_list_end(&error) => return Ok(processes),
            Err(error) => return Err(process_enumeration_error(error)),
        }

        loop {
            let name_end = entry
                .szExeFile
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..name_end]);
            if CLIENT_PROCESS_NAMES
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
            {
                processes.push(ClientProcess {
                    process_id: entry.th32ProcessID,
                    name,
                });
            }

            match unsafe { Process32NextW(snapshot, &mut entry) } {
                Ok(()) => {}
                Err(error) if is_process_list_end(&error) => return Ok(processes),
                Err(error) => return Err(process_enumeration_error(error)),
            }
        }
    })();
    let close_result = unsafe { CloseHandle(snapshot) }
        .map_err(|error| AppError::Message(format!("无法释放 Windows 进程快照：{error}")));
    match (result, close_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(processes), Ok(())) => Ok(processes),
    }
}

fn is_process_list_end(error: &WindowsError) -> bool {
    error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0)
}

fn process_enumeration_error(error: WindowsError) -> AppError {
    AppError::Message(format!("读取 Windows 进程列表时发生错误：{error}"))
}

fn send_close_messages(process_ids: &HashSet<u32>, deadline: Instant) -> Result<(), AppError> {
    for window in collect_windows(process_ids)? {
        for (message, wparam) in [(WM_QUERYENDSESSION, WPARAM(0)), (WM_ENDSESSION, WPARAM(1))] {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            let message_timeout = remaining.as_millis().clamp(1, 5_000) as u32;
            let mut ignored = 0usize;
            unsafe {
                let _ = SendMessageTimeoutW(
                    window,
                    message,
                    wparam,
                    LPARAM(0),
                    SMTO_ABORTIFHUNG,
                    message_timeout,
                    Some(&mut ignored),
                );
            }
        }
    }
    Ok(())
}

fn collect_windows(process_ids: &HashSet<u32>) -> Result<Vec<HWND>, AppError> {
    let mut context = WindowCollector {
        process_ids,
        windows: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(collect_process_windows),
            LPARAM((&mut context as *mut WindowCollector<'_>) as isize),
        )
    }
    .map_err(|error| AppError::Message(format!("无法枚举 Battle.net 窗口：{error}")))?;
    Ok(context.windows)
}

struct WindowCollector<'a> {
    process_ids: &'a HashSet<u32>,
    windows: Vec<HWND>,
}

unsafe extern "system" fn collect_process_windows(window: HWND, parameter: LPARAM) -> BOOL {
    let context = unsafe { &mut *(parameter.0 as *mut WindowCollector<'_>) };
    let mut process_id = 0u32;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut process_id));
    }
    if context.process_ids.contains(&process_id) {
        context.windows.push(window);
    }
    BOOL(1)
}

#[cfg(test)]
mod tests {
    use crate::contracts::LoginRegion;
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_NO_MORE_FILES};
    use windows::core::{Error as WindowsError, HRESULT};

    #[test]
    fn can_enumerate_client_processes() {
        super::client_process_ids().unwrap();
    }

    #[test]
    fn only_no_more_files_finishes_process_enumeration() {
        let finished = WindowsError::from_hresult(HRESULT::from_win32(ERROR_NO_MORE_FILES.0));
        let denied = WindowsError::from_hresult(HRESULT::from_win32(ERROR_ACCESS_DENIED.0));
        assert!(super::is_process_list_end(&finished));
        assert!(!super::is_process_list_end(&denied));
    }

    #[test]
    fn login_region_is_passed_as_one_argument() {
        assert_eq!(
            super::login_region_argument(LoginRegion::China),
            "--setregion=CN"
        );
        assert_eq!(
            super::login_region_argument(LoginRegion::Asia),
            "--setregion=KR"
        );
    }
}
