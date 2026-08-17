use std::{
    collections::HashSet,
    ffi::c_void,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use objc2_app_kit::{NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration};
use objc2_foundation::{NSArray, NSString};

use crate::{contracts::LoginRegion, error::AppError};

use super::paths::{bundle_url, validate_blizzard_bundle, validate_client_bundle};

const BUNDLE_IDENTIFIER: &str = "net.battle.app";
const HELPER_BUNDLE_IDENTIFIER: &str = "net.battle.app.helper";
// libproc.h PROC_UID_ONLY: list processes for one effective user ID.
const PROC_UID_ONLY: u32 = 4;
const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
const PROC_NAME_MAXSIZE: usize = 1024;
const REQUIRED_EMPTY_SAMPLES: u8 = 2;
const PID_BUFFER_HEADROOM: usize = 256;
const MAX_PID_ENUMERATION_ATTEMPTS: usize = 4;

#[link(name = "proc")]
unsafe extern "C" {
    fn proc_listpids(
        process_type: u32,
        type_info: u32,
        buffer: *mut c_void,
        buffer_size: i32,
    ) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut c_void, buffer_size: u32) -> i32;
    fn proc_name(pid: i32, buffer: *mut c_void, buffer_size: u32) -> i32;
}

pub(crate) fn is_client_running(client: &ValidatedClient) -> Result<bool, AppError> {
    Ok(!client_runtime_processes(client)?.is_empty())
}

pub(crate) fn is_main_client_running(client: &ValidatedClient) -> Result<bool, AppError> {
    Ok(related_processes(client)?
        .iter()
        .any(|process| is_main_client_process(client, process)))
}

pub(crate) fn ensure_client_stopped(
    client: &ValidatedClient,
    timeout: Duration,
) -> Result<(), AppError> {
    let deadline = Instant::now() + timeout;
    let mut empty_samples = 0_u8;
    while Instant::now() < deadline {
        let processes = client_runtime_processes(client)?;
        if processes.is_empty() {
            empty_samples += 1;
            if empty_samples >= REQUIRED_EMPTY_SAMPLES {
                return Ok(());
            }
        } else if processes
            .iter()
            .any(|process| is_main_client_process(client, process))
        {
            return Err(AppError::Message(
                "检测到 Battle.net 已重新启动，已在修改配置前取消操作".into(),
            ));
        } else {
            // A verified helper can outlive the main app briefly during a
            // normal shutdown; it is not a restart signal.
            empty_samples = 0;
        }
        thread::sleep(Duration::from_millis(150));
    }
    Err(AppError::Message(
        "无法确认 Battle.net 已完全退出，已取消操作".into(),
    ))
}

pub(crate) fn graceful_stop(client: &ValidatedClient, timeout: Duration) -> Result<(), AppError> {
    let deadline = Instant::now() + timeout;
    let processes = client_runtime_processes(client)?;
    if processes.is_empty() {
        return Ok(());
    }

    for process in &processes {
        let current_processes = client_runtime_processes(client)?;
        if !current_processes
            .iter()
            .any(|current| current.pid == process.pid && current.path == process.path)
        {
            continue;
        }
        if let Some(application) =
            NSRunningApplication::runningApplicationWithProcessIdentifier(process.pid)
        {
            let _ = application.terminate();
        }
    }

    let mut empty_samples = 0_u8;
    while Instant::now() < deadline {
        if client_runtime_processes(client)?.is_empty() {
            empty_samples += 1;
            if empty_samples >= REQUIRED_EMPTY_SAMPLES {
                return Ok(());
            }
        } else {
            empty_samples = 0;
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err(AppError::ClientStopTimeout)
}

pub(crate) fn launch(client: &ValidatedClient) -> Result<(), AppError> {
    launch_with_region(client, None)
}

pub(crate) fn launch_for_login(
    client: &ValidatedClient,
    region: Option<LoginRegion>,
    timeout: Duration,
) -> Result<(), AppError> {
    launch_with_region(client, region)?;
    wait_for_client_started(client, timeout)
}

fn launch_with_region(
    client: &ValidatedClient,
    region: Option<LoginRegion>,
) -> Result<(), AppError> {
    if is_main_client_running(client)? {
        return Ok(());
    }
    if let Some(region) = region {
        let configuration = NSWorkspaceOpenConfiguration::configuration();
        let argument = NSString::from_str(&login_region_argument(region));
        let arguments = NSArray::from_retained_slice(&[argument]);
        configuration.setArguments(&arguments);
        NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
            &bundle_url(&client.bundle),
            &configuration,
            None,
        );
        return Ok(());
    }
    if NSWorkspace::sharedWorkspace().openURL(&bundle_url(&client.bundle)) {
        Ok(())
    } else {
        Err(AppError::ClientLaunch(
            "Launch Services 拒绝打开 Battle.net.app".into(),
        ))
    }
}

fn login_region_argument(region: LoginRegion) -> String {
    format!("--setregion={}", region.launch_code())
}

pub(crate) fn wait_for_client_started(
    client: &ValidatedClient,
    timeout: Duration,
) -> Result<(), AppError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if related_processes(client)?
            .iter()
            .any(|process| process.path == client.main_executable)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(AppError::ClientLaunch(
        "启动命令已发出，但未检测到 Battle.net 进程".into(),
    ))
}

#[derive(Clone, Debug)]
struct RelatedProcess {
    pid: i32,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct ValidatedClient {
    bundle: PathBuf,
    main_executable: PathBuf,
    agent_root: PathBuf,
    agent_bundles: Vec<PathBuf>,
}

impl ValidatedClient {
    pub(crate) fn new(bundle: &Path) -> Result<Self, AppError> {
        let bundle = validate_client_bundle(bundle)?;
        let main_executable = bundle
            .join("Contents/MacOS/Battle.net")
            .canonicalize()
            .map_err(|_| AppError::InvalidClientExecutable)?;
        if !main_executable.starts_with(&bundle) {
            return Err(AppError::InvalidClientExecutable);
        }
        let agent_root = Path::new("/Users/Shared/Battle.net/Agent");
        let agent_root = if agent_root.exists() {
            agent_root
                .canonicalize()
                .map_err(|_| AppError::InvalidClientExecutable)?
        } else {
            agent_root.to_path_buf()
        };
        let agent_bundles = validate_agent_bundles(&agent_root)?;
        Ok(Self {
            bundle,
            main_executable,
            agent_root,
            agent_bundles,
        })
    }

    pub(crate) fn bundle(&self) -> &Path {
        &self.bundle
    }
}

fn validate_agent_bundles(agent_root: &Path) -> Result<Vec<PathBuf>, AppError> {
    if !agent_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut bundles = Vec::new();
    for entry in std::fs::read_dir(agent_root)
        .map_err(|error| AppError::Message(format!("无法读取 Battle.net Agent 目录：{error}")))?
    {
        let entry = entry.map_err(|error| {
            AppError::Message(format!("无法读取 Battle.net Agent 目录项：{error}"))
        })?;
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
            AppError::Message(format!("无法读取 Battle.net Agent 目录项属性：{error}"))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let candidate = entry.path().join("Agent.app");
        if candidate.exists() {
            // Battle.net updates can leave incomplete or obsolete version
            // directories behind. Only a running process under an unverified
            // bundle is a safety boundary; stale bundles are ignored here.
            if let Ok(bundle) = validate_blizzard_bundle(&candidate) {
                bundles.push(bundle);
            }
        }
    }
    Ok(bundles)
}

fn related_processes(client: &ValidatedClient) -> Result<Vec<RelatedProcess>, AppError> {
    let bundle_application_pids = verified_bundle_application_pids(client)?;
    let pids = enumerate_relevant_user_pids()?;

    let mut result = Vec::new();
    for pid in pids.into_iter().filter(|pid| *pid > 0) {
        let name = process_name(pid);
        let candidate_by_identity = bundle_application_pids.contains(&pid)
            || name.as_deref().is_some_and(is_candidate_process_name);
        let Some(path) = process_path(pid) else {
            if process_is_gone(pid) {
                continue;
            }
            if candidate_by_identity {
                return Err(AppError::Message(format!(
                    "检测到无法确认路径的 Battle.net 候选进程 {pid}，已停止本次操作"
                )));
            }
            continue;
        };
        if !candidate_by_identity
            && !path.starts_with(&client.bundle)
            && !path.starts_with(&client.agent_root)
        {
            continue;
        }
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(_) if process_is_gone(pid) => continue,
            Err(error) => {
                return Err(AppError::Message(format!(
                    "无法规范化 macOS 进程 {pid} 的路径：{error}，已停止本次操作"
                )));
            }
        };
        let is_bundle_process = canonical.starts_with(&client.bundle);
        let is_agent_path = canonical.starts_with(&client.agent_root);
        let agent_bundle = canonical
            .starts_with(&client.agent_root)
            .then(|| {
                canonical
                    .ancestors()
                    .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "Agent.app"))
                    .map(Path::to_path_buf)
            })
            .flatten();
        let is_agent = agent_bundle.is_some();
        let is_verified_helper = is_helper_process_name(name.as_deref().unwrap_or_default())
            && validated_helper_bundle(&canonical).is_some();
        let candidate = is_bundle_process || is_agent_path || is_verified_helper;
        if !candidate {
            if candidate_by_identity {
                return Err(AppError::Message(format!(
                    "检测到来源无法确认的 Battle.net 候选进程 {pid}，已停止本次操作"
                )));
            }
            continue;
        }
        if is_agent_path && !is_agent {
            return Err(AppError::Message(format!(
                "检测到无法归属到已验证 Agent.app 的 Battle.net 进程 {pid}，已停止本次操作"
            )));
        }
        if is_bundle_process || is_agent || is_verified_helper {
            if let Some(agent_bundle) = agent_bundle {
                if !client
                    .agent_bundles
                    .iter()
                    .any(|bundle| bundle == &agent_bundle)
                {
                    return Err(AppError::Message(format!(
                        "检测到未通过启动前签名校验的 Battle.net Agent：{}",
                        agent_bundle.display()
                    )));
                }
            }
            result.push(RelatedProcess {
                pid,
                path: canonical,
            });
        }
    }
    Ok(result)
}

fn client_runtime_processes(client: &ValidatedClient) -> Result<Vec<RelatedProcess>, AppError> {
    // The shared Agent is a persistent updater and is intentionally excluded
    // from the client stop condition. Helpers have already passed validation
    // in related_processes, so this filter only classifies their paths.
    Ok(related_processes(client)?
        .into_iter()
        .filter(|process| {
            process.path == client.main_executable
                || process.path.starts_with(&client.bundle)
                || process
                    .path
                    .file_name()
                    .is_some_and(|name| name == "Battle.net Helper")
        })
        .collect())
}

fn is_main_client_process(client: &ValidatedClient, process: &RelatedProcess) -> bool {
    process.path == client.main_executable
}

fn verified_bundle_application_pids(client: &ValidatedClient) -> Result<HashSet<i32>, AppError> {
    let mut pids = HashSet::new();
    let applications = NSWorkspace::sharedWorkspace().runningApplications();
    for application in applications.iter() {
        let Some(identifier) = application.bundleIdentifier() else {
            continue;
        };
        let identifier = identifier.to_string();
        let is_main = identifier == BUNDLE_IDENTIFIER;
        let is_known_agent = client
            .agent_bundles
            .iter()
            .any(|bundle| bundle_identifier(bundle).as_deref() == Some(identifier.as_str()));
        if !is_main && !is_known_agent {
            continue;
        }
        let Some(url) = application.bundleURL() else {
            return Err(AppError::Message(
                "检测到无法确认来源的 Battle.net 候选进程，已停止本次操作".into(),
            ));
        };
        let Some(path) = url.path() else {
            return Err(AppError::Message(
                "检测到无法确认路径的 Battle.net 候选进程，已停止本次操作".into(),
            ));
        };
        let application_bundle = Path::new(&path.to_string())
            .canonicalize()
            .map_err(|error| {
                AppError::Message(format!(
                    "无法规范化正在运行的 Battle.net 路径：{error}，已停止本次操作"
                ))
            })?;
        let expected = if is_main {
            application_bundle == client.bundle
        } else {
            client.agent_bundles.contains(&application_bundle)
        };
        if !expected {
            return Err(AppError::Message(format!(
                "检测到另一位置正在运行的 Battle.net 组件：{}，请先退出后重试",
                application_bundle.display()
            )));
        }
        pids.insert(application.processIdentifier());
    }
    Ok(pids)
}

fn bundle_identifier(bundle: &Path) -> Option<String> {
    let info = plist::Value::from_file(bundle.join("Contents/Info.plist")).ok()?;
    info.as_dictionary()
        .and_then(|dictionary| dictionary.get("CFBundleIdentifier"))
        .and_then(plist::Value::as_string)
        .filter(|identifier| !identifier.is_empty())
        .map(str::to_owned)
}

fn process_path(pid: i32) -> Option<PathBuf> {
    let mut buffer = vec![0_u8; PROC_PIDPATHINFO_MAXSIZE];
    let length = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast(),
            PROC_PIDPATHINFO_MAXSIZE as u32,
        )
    };
    if length <= 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(length as usize);
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..end])))
}

fn process_is_gone(pid: i32) -> bool {
    process_name(pid).is_none() && process_path(pid).is_none()
}

fn process_name(pid: i32) -> Option<Vec<u8>> {
    let mut buffer = vec![0_u8; PROC_NAME_MAXSIZE];
    let length = unsafe { proc_name(pid, buffer.as_mut_ptr().cast(), PROC_NAME_MAXSIZE as u32) };
    if length <= 0 {
        return None;
    }
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(length as usize);
    buffer.truncate(end);
    Some(buffer)
}

fn is_candidate_process_name(name: &[u8]) -> bool {
    name == b"Battle.net" || name == b"Agent" || is_helper_process_name(name)
}

fn is_helper_process_name(name: &[u8]) -> bool {
    name.starts_with(b"Battle.net Helper")
}

fn validated_helper_bundle(executable: &Path) -> Option<PathBuf> {
    let canonical_executable = executable.canonicalize().ok()?;
    if canonical_executable.file_name()?.to_str()? != "Battle.net Helper" {
        return None;
    }

    let bundle = canonical_executable.parent()?.parent()?.parent()?;
    if bundle.file_name()?.to_str()? != "Battle.net Helper.app"
        || bundle.parent()?.file_name()?.to_str()? != "Frameworks"
        || bundle.parent()?.parent()?.file_name()?.to_str()? != "battle.net-core.framework"
    {
        return None;
    }

    let version_directory = bundle.parent()?.parent()?.parent()?;
    if !version_directory
        .file_name()?
        .to_str()?
        .starts_with("Battle.net.")
    {
        return None;
    }
    let versions_directory = version_directory.parent()?;
    if versions_directory.file_name()?.to_str()? != "Versions" {
        return None;
    }

    let expected_versions = PathBuf::from(std::env::var_os("HOME")?)
        .join("Library/Application Support/Battle.net/Versions")
        .canonicalize()
        .ok()?;
    if !versions_directory.starts_with(&expected_versions) {
        return None;
    }
    if bundle_identifier(bundle).as_deref() != Some(HELPER_BUNDLE_IDENTIFIER) {
        return None;
    }

    validate_blizzard_bundle(bundle).ok()
}

fn enumerate_relevant_user_pids() -> Result<Vec<i32>, AppError> {
    let current_uid = unsafe { libc::geteuid() };
    let mut pids = enumerate_user_pids(current_uid)?;
    if current_uid != 0 {
        pids.extend(enumerate_user_pids(0)?);
        pids.sort_unstable();
        pids.dedup();
    }
    Ok(pids)
}

fn enumerate_user_pids(uid: u32) -> Result<Vec<i32>, AppError> {
    let required_bytes = unsafe { proc_listpids(PROC_UID_ONLY, uid, std::ptr::null_mut(), 0) };
    if required_bytes <= 0 {
        return Err(AppError::Message("无法读取 macOS 进程列表".into()));
    }
    let pid_size = std::mem::size_of::<i32>();
    let mut capacity = required_bytes as usize / pid_size + PID_BUFFER_HEADROOM;
    for _ in 0..MAX_PID_ENUMERATION_ATTEMPTS {
        let mut pids = vec![0_i32; capacity];
        let buffer_bytes = capacity
            .checked_mul(pid_size)
            .filter(|bytes| *bytes <= i32::MAX as usize)
            .ok_or_else(|| AppError::Message("macOS 进程列表过大，无法安全读取".into()))?;
        let bytes = unsafe {
            proc_listpids(
                PROC_UID_ONLY,
                uid,
                pids.as_mut_ptr().cast(),
                buffer_bytes as i32,
            )
        };
        if bytes <= 0 {
            return Err(AppError::Message("无法读取 macOS 进程列表".into()));
        }
        let used_bytes = bytes as usize;
        if used_bytes < buffer_bytes {
            pids.truncate(used_bytes / pid_size);
            return Ok(pids);
        }
        capacity = capacity
            .checked_mul(2)
            .ok_or_else(|| AppError::Message("macOS 进程列表过大，无法安全读取".into()))?;
    }
    Err(AppError::Message(
        "macOS 进程列表持续变化，无法确认 Battle.net 是否已退出".into(),
    ))
}

#[cfg(test)]
mod tests {
    use crate::contracts::LoginRegion;

    use super::is_candidate_process_name;

    #[test]
    fn only_battle_net_process_names_are_fail_closed_candidates() {
        for name in [
            b"Battle.net".as_slice(),
            b"Agent".as_slice(),
            b"Battle.net Helper".as_slice(),
            b"Battle.net Helper (Renderer)".as_slice(),
        ] {
            assert!(is_candidate_process_name(name));
        }
        for name in [
            b"Finder".as_slice(),
            b"Some Helper".as_slice(),
            b"UpdateAgent".as_slice(),
        ] {
            assert!(!is_candidate_process_name(name));
        }
    }

    #[test]
    fn login_region_is_passed_as_one_argument() {
        assert_eq!(
            super::login_region_argument(LoginRegion::Europe),
            "--setregion=EU"
        );
        assert_eq!(
            super::login_region_argument(LoginRegion::Americas),
            "--setregion=US"
        );
    }
}
