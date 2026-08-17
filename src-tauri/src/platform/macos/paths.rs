use std::{
    fs,
    path::{Path, PathBuf},
    str::FromStr,
};

use core_foundation::url::CFURL;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{
    NSSearchPathDirectory, NSSearchPathDomainMask, NSSearchPathForDirectoriesInDomains, NSString,
    NSURL,
};
use plist::Value;
use security_framework::os::macos::code_signing::{Flags, SecRequirement, SecStaticCode};

use crate::error::AppError;

const BUNDLE_IDENTIFIER: &str = "net.battle.app";
const DEFAULT_BUNDLE: &str = "/Applications/Battle.net.app";
const TEAM_ID_ENV: &str = "BNETSWITCHLITE_BLIZZARD_TEAM_ID";
const COMPILED_TEAM_ID: Option<&str> = option_env!("BNETSWITCHLITE_BLIZZARD_TEAM_ID");

#[derive(Clone, Debug)]
pub(crate) struct BattleNetPaths {
    pub cached_data_db: PathBuf,
    pub config: PathBuf,
}

impl BattleNetPaths {
    pub fn discover() -> Result<Self, AppError> {
        let directories = NSSearchPathForDirectoriesInDomains(
            NSSearchPathDirectory::ApplicationSupportDirectory,
            NSSearchPathDomainMask::UserDomainMask,
            true,
        );
        let support_directory = directories
            .firstObject()
            .map(|path| PathBuf::from(path.to_string()).join("Battle.net"))
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                AppError::Message("无法确定当前用户的 Application Support 目录".into())
            })?;
        Ok(Self {
            cached_data_db: support_directory.join("CachedData.db"),
            config: support_directory.join("Battle.net.config"),
        })
    }

    pub fn detect_client_bundle(&self) -> Option<PathBuf> {
        let bundle_id = NSString::from_str(BUNDLE_IDENTIFIER);
        if let Some(url) =
            NSWorkspace::sharedWorkspace().URLForApplicationWithBundleIdentifier(&bundle_id)
        {
            if let Some(path) = url.path() {
                let candidate = PathBuf::from(path.to_string());
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
        Path::new(DEFAULT_BUNDLE)
            .exists()
            .then(|| PathBuf::from(DEFAULT_BUNDLE))
    }
}

pub(crate) fn validate_client_bundle(path: &Path) -> Result<PathBuf, AppError> {
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| AppError::InvalidClientExecutable)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(AppError::InvalidClientExecutable);
    }
    if path.extension().and_then(|value| value.to_str()) != Some("app") {
        return Err(AppError::InvalidClientExecutable);
    }

    let canonical = path
        .canonicalize()
        .map_err(|_| AppError::InvalidClientExecutable)?;
    let info_path = canonical.join("Contents/Info.plist");
    let info = Value::from_file(&info_path).map_err(|_| AppError::InvalidClientExecutable)?;
    let bundle_id = info
        .as_dictionary()
        .and_then(|value| value.get("CFBundleIdentifier"))
        .and_then(Value::as_string);
    let executable_name = info
        .as_dictionary()
        .and_then(|value| value.get("CFBundleExecutable"))
        .and_then(Value::as_string);
    if bundle_id != Some(BUNDLE_IDENTIFIER) || executable_name != Some("Battle.net") {
        return Err(AppError::InvalidClientExecutable);
    }
    let executable = canonical.join("Contents/MacOS/Battle.net");
    if !fs::metadata(&executable).is_ok_and(|metadata| metadata.is_file()) {
        return Err(AppError::InvalidClientExecutable);
    }

    validate_signature(&canonical, Some(BUNDLE_IDENTIFIER))?;
    Ok(canonical)
}

pub(crate) fn validate_blizzard_bundle(path: &Path) -> Result<PathBuf, AppError> {
    reject_symlink_components(path)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| AppError::InvalidClientExecutable)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || path.extension().and_then(|value| value.to_str()) != Some("app")
    {
        return Err(AppError::InvalidClientExecutable);
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| AppError::InvalidClientExecutable)?;
    let info = Value::from_file(canonical.join("Contents/Info.plist"))
        .map_err(|_| AppError::InvalidClientExecutable)?;
    let dictionary = info
        .as_dictionary()
        .ok_or(AppError::InvalidClientExecutable)?;
    let executable = dictionary
        .get("CFBundleExecutable")
        .and_then(Value::as_string)
        .filter(|value| !value.is_empty())
        .ok_or(AppError::InvalidClientExecutable)?;
    let executable_path = canonical.join("Contents/MacOS").join(executable);
    if !fs::metadata(executable_path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(AppError::InvalidClientExecutable);
    }
    validate_signature(&canonical, None)?;
    Ok(canonical)
}

fn validate_signature(path: &Path, identifier: Option<&str>) -> Result<(), AppError> {
    let team_id = COMPILED_TEAM_ID
        .filter(|value| is_team_id(value))
        .ok_or_else(|| {
            AppError::ClientLaunch(format!(
                "未配置 Blizzard Team ID；请在构建时设置 {TEAM_ID_ENV}=<codesign -dvvv 输出的 TeamIdentifier> 后重新构建"
            ))
        })?;
    let requirement_text = match identifier {
        Some(identifier) => format!(
            "anchor apple generic and identifier \"{identifier}\" and certificate leaf[subject.OU] = \"{team_id}\""
        ),
        None => format!("anchor apple generic and certificate leaf[subject.OU] = \"{team_id}\""),
    };
    let url = CFURL::from_path(path, path.is_dir()).ok_or(AppError::InvalidClientExecutable)?;
    let code = SecStaticCode::from_path(&url, Flags::NONE)
        .map_err(|_| AppError::InvalidClientExecutable)?;
    let requirement = SecRequirement::from_str(&requirement_text)
        .map_err(|_| AppError::InvalidClientExecutable)?;
    code.check_validity(
        Flags::CHECK_ALL_ARCHITECTURES
            | Flags::CHECK_NESTED_CODE
            | Flags::STRICT_VALIDATE
            | Flags::RESTRICT_SYMLINKS
            | Flags::NO_NETWORK_ACCESS,
        &requirement,
    )
    .map_err(|_| AppError::InvalidClientExecutable)
}

fn is_team_id(value: &str) -> bool {
    value.len() == 10 && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn reject_symlink_components(path: &Path) -> Result<(), AppError> {
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::Prefix(_)
        )
    }) {
        return Err(AppError::InvalidClientExecutable);
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| AppError::InvalidClientExecutable)?
            .join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| AppError::InvalidClientExecutable)?;
        if metadata.file_type().is_symlink() {
            return Err(AppError::InvalidClientExecutable);
        }
    }
    Ok(())
}

pub(crate) fn bundle_url(path: &Path) -> objc2::rc::Retained<NSURL> {
    NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), true)
}
