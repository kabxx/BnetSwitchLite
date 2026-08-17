use std::{
    env, fs,
    mem::size_of,
    path::{Path, PathBuf},
};

use serde_json::Value;

#[cfg(windows)]
use std::os::windows::{ffi::OsStrExt, fs::MetadataExt};

#[cfg(windows)]
use windows::{
    Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW},
    core::PCWSTR,
};

use crate::error::AppError;

const CLIENT_EXECUTABLE_NAME: &str = "Battle.net.exe";

#[derive(Clone, Debug)]
pub struct BattleNetPaths {
    pub cached_data_db: PathBuf,
    pub roaming_dir: PathBuf,
    pub roaming_config: PathBuf,
}

impl BattleNetPaths {
    pub fn discover() -> Result<Self, AppError> {
        let local = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| AppError::Message("无法确定 Windows LocalAppData 目录".into()))?;
        let roaming = env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| AppError::Message("无法确定 Windows AppData 目录".into()))?;
        let local_root = local.join("Battle.net");
        let roaming_dir = roaming.join("Battle.net");

        Ok(Self {
            cached_data_db: local_root.join("CachedData.db"),
            roaming_config: roaming_dir.join("Battle.net.config"),
            roaming_dir,
        })
    }

    pub fn detect_client_executable(&self) -> Option<PathBuf> {
        let mut candidates = Vec::new();
        collect_config_candidates(&self.roaming_config, &mut candidates);

        for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(root) = env::var_os(variable) {
                candidates.push(PathBuf::from(root).join("Battle.net"));
            }
        }
        candidates.extend([
            PathBuf::from(r"C:\Battle.net"),
            PathBuf::from(r"D:\Battle.net"),
        ]);

        candidates.into_iter().find_map(|candidate| {
            if candidate.is_file() {
                return validate_client_executable(&candidate).ok();
            }
            validate_client_executable(&candidate.join(CLIENT_EXECUTABLE_NAME)).ok()
        })
    }
}

pub fn validate_client_executable(path: &Path) -> Result<PathBuf, AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| AppError::InvalidClientExecutable)?;
    if !metadata.is_file() || is_reparse_point(&metadata) {
        return Err(AppError::InvalidClientExecutable);
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(AppError::InvalidClientExecutable)?;
    if !name.eq_ignore_ascii_case(CLIENT_EXECUTABLE_NAME) {
        return Err(AppError::InvalidClientExecutable);
    }
    if !has_battle_net_version_identity(path) {
        return Err(AppError::InvalidClientExecutable);
    }
    for component in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        let metadata =
            fs::symlink_metadata(component).map_err(|_| AppError::InvalidClientExecutable)?;
        if is_reparse_point(&metadata) {
            return Err(AppError::InvalidClientExecutable);
        }
    }
    path.canonicalize()
        .map_err(|_| AppError::InvalidClientExecutable)
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & 0x0400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn has_battle_net_version_identity(path: &Path) -> bool {
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), None) };
    if size == 0 {
        return false;
    }
    let mut version_data = vec![0_u8; size as usize];
    if unsafe {
        GetFileVersionInfoW(
            PCWSTR(path.as_ptr()),
            None,
            size,
            version_data.as_mut_ptr().cast(),
        )
    }
    .is_err()
    {
        return false;
    }

    let translation_key = "\\VarFileInfo\\Translation"
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut translations = std::ptr::null_mut();
    let mut translation_bytes = 0_u32;
    if !unsafe {
        VerQueryValueW(
            version_data.as_ptr().cast(),
            PCWSTR(translation_key.as_ptr()),
            &mut translations,
            &mut translation_bytes,
        )
    }
    .as_bool()
        || translation_bytes < 4
    {
        return false;
    }

    let translations = unsafe {
        std::slice::from_raw_parts(
            translations.cast::<u16>(),
            translation_bytes as usize / size_of::<u16>(),
        )
    };
    translations.chunks_exact(2).any(|translation| {
        let company = version_string(&version_data, translation[0], translation[1], "CompanyName")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let product = version_string(&version_data, translation[0], translation[1], "ProductName")
            .unwrap_or_default()
            .to_ascii_lowercase();
        company.contains("blizzard") && product.contains("battle.net")
    })
}

#[cfg(windows)]
fn version_string(data: &[u8], language: u16, code_page: u16, field: &str) -> Option<String> {
    let key = format!("\\StringFileInfo\\{language:04x}{code_page:04x}\\{field}")
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut value = std::ptr::null_mut();
    let mut characters = 0_u32;
    if !unsafe {
        VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(key.as_ptr()),
            &mut value,
            &mut characters,
        )
    }
    .as_bool()
        || characters == 0
    {
        return None;
    }
    let value = unsafe {
        std::slice::from_raw_parts(value.cast::<u16>(), characters.saturating_sub(1) as usize)
    };
    Some(String::from_utf16_lossy(value).trim().to_owned())
}

#[cfg(not(windows))]
fn has_battle_net_version_identity(_path: &Path) -> bool {
    false
}

fn collect_config_candidates(config_path: &Path, candidates: &mut Vec<PathBuf>) {
    let Ok(bytes) = fs::read(config_path) else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return;
    };
    collect_path_values(&value, candidates);
}

fn collect_path_values(value: &Value, candidates: &mut Vec<PathBuf>) {
    match value {
        Value::Object(properties) => {
            for (name, child) in properties {
                if name == "Path" {
                    if let Some(path) = child.as_str().filter(|path| !path.trim().is_empty()) {
                        candidates.push(PathBuf::from(path));
                    }
                }
                collect_path_values(child, candidates);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_path_values(item, candidates);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_the_standard_client_is_a_launch_target() {
        assert!("Battle.net.exe".eq_ignore_ascii_case(super::CLIENT_EXECUTABLE_NAME));
        assert!(!"Battle.net Launcher.exe".eq_ignore_ascii_case(super::CLIENT_EXECUTABLE_NAME));
    }
}
