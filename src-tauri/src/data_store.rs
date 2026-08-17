use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use atomicwrites::{AtomicFile, OverwriteBehavior};
use serde::{Deserialize, Serialize};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

use crate::contracts::AccountKey;

const HIDDEN_ACCOUNTS_VERSION: u8 = 1;
const SETTINGS_VERSION: u8 = 1;
#[cfg(windows)]
const WINDOWS_DATA_DIRECTORY: &str = "BnetSwitchLiteData";

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HiddenAccountKey {
    pub environment: String,
    pub account_id: String,
}

impl From<&AccountKey> for HiddenAccountKey {
    fn from(value: &AccountKey) -> Self {
        Self {
            environment: value.environment.clone(),
            account_id: value.account_id.clone(),
        }
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsDocument {
    pub version: u8,
    pub client_executable_path: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct HiddenAccountsDocument {
    version: u8,
    accounts: Vec<HiddenAccountKey>,
}

pub struct DataStore {
    data_directory: PathBuf,
    hidden_accounts_path: PathBuf,
    settings_path: PathBuf,
}

impl DataStore {
    #[cfg(windows)]
    pub fn open() -> Result<Self, String> {
        let executable =
            std::env::current_exe().map_err(|error| format!("无法确定程序所在目录：{error}"))?;
        let application_directory = executable
            .parent()
            .ok_or_else(|| "无法确定程序所在目录".to_string())?;
        Self::open_for_windows(application_directory)
    }

    #[cfg(target_os = "macos")]
    pub fn open() -> Result<Self, String> {
        let data_directory = macos_application_support()?.join("BnetSwitchLite");
        crate::platform::macos::secure_fs::ensure_private_directory(&data_directory)
            .map_err(|error| format!("无法安全打开应用数据目录：{error}"))?;
        crate::platform::macos::backup_exclusion::set_and_verify(&data_directory)?;
        verify_writable(
            &data_directory,
            "请检查当前用户的 Application Support 目录权限",
        )?;

        Ok(Self {
            hidden_accounts_path: data_directory.join("hidden-accounts.json"),
            settings_path: data_directory.join("settings.json"),
            data_directory,
        })
    }

    #[cfg(windows)]
    fn open_for_windows(application_directory: &Path) -> Result<Self, String> {
        let data_directory = application_directory.join(WINDOWS_DATA_DIRECTORY);

        fs::create_dir_all(&data_directory).map_err(|error| {
            format!("无法创建应用数据目录 {}：{error}", data_directory.display())
        })?;
        reject_reparse_data_directory(&data_directory)?;
        verify_writable(
            &data_directory,
            "请将 BnetSwitchLite 完整目录移动到可写位置后重试",
        )?;

        Ok(Self {
            hidden_accounts_path: data_directory.join("hidden-accounts.json"),
            settings_path: data_directory.join("settings.json"),
            data_directory,
        })
    }

    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }

    pub fn load_hidden_accounts(&self) -> Result<HashSet<HiddenAccountKey>, String> {
        if !self.hidden_accounts_path.exists() {
            return Ok(HashSet::new());
        }

        let bytes = fs::read(&self.hidden_accounts_path).map_err(|error| {
            format!(
                "无法读取隐藏账号记录 {}：{error}",
                self.hidden_accounts_path.display()
            )
        })?;
        let document: HiddenAccountsDocument = serde_json::from_slice(&bytes)
            .map_err(|error| format!("隐藏账号记录格式无效：{error}"))?;

        if document.version != HIDDEN_ACCOUNTS_VERSION {
            return Err(format!("不支持的隐藏账号记录版本：{}", document.version));
        }

        Ok(document.accounts.into_iter().collect())
    }

    pub fn save_hidden_accounts(
        &self,
        hidden_accounts: &HashSet<HiddenAccountKey>,
    ) -> Result<(), String> {
        let mut accounts = hidden_accounts.iter().cloned().collect::<Vec<_>>();
        accounts.sort();
        let document = HiddenAccountsDocument {
            version: HIDDEN_ACCOUNTS_VERSION,
            accounts,
        };
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| format!("无法生成隐藏账号记录：{error}"))?;

        write_json_atomic(&self.hidden_accounts_path, &bytes)
            .map_err(|error| format!("无法保存隐藏账号记录：{error}"))
    }

    pub fn restore_detected_accounts(
        &self,
        detected_accounts: impl IntoIterator<Item = HiddenAccountKey>,
    ) -> Result<HashSet<HiddenAccountKey>, String> {
        let detected = detected_accounts.into_iter().collect::<HashSet<_>>();
        let mut hidden = self.load_hidden_accounts()?;
        let previous_count = hidden.len();
        hidden.retain(|account| !detected.contains(account));
        if hidden.len() != previous_count {
            self.save_hidden_accounts(&hidden)?;
        }
        Ok(hidden)
    }

    pub fn load_settings(&self) -> Result<SettingsDocument, String> {
        if !self.settings_path.exists() {
            return Ok(SettingsDocument {
                version: SETTINGS_VERSION,
                client_executable_path: None,
            });
        }
        let bytes =
            fs::read(&self.settings_path).map_err(|error| format!("无法读取设置：{error}"))?;
        let settings: SettingsDocument =
            serde_json::from_slice(&bytes).map_err(|error| format!("设置文件格式无效：{error}"))?;
        if settings.version != SETTINGS_VERSION {
            return Err(format!("不支持的设置文件版本：{}", settings.version));
        }
        Ok(settings)
    }

    pub fn save_settings(&self, settings: &SettingsDocument) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(settings)
            .map_err(|error| format!("无法生成设置：{error}"))?;
        write_json_atomic(&self.settings_path, &bytes)
            .map_err(|error| format!("无法保存设置：{error}"))
    }
}

#[cfg(windows)]
fn reject_reparse_data_directory(data_directory: &Path) -> Result<(), String> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    for component in data_directory
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
    {
        let metadata = fs::symlink_metadata(component)
            .map_err(|error| format!("无法读取便携目录属性 {}：{error}", component.display()))?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "程序目录或应用数据目录不能经过符号链接或联接点：{}",
                component.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn write_json_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    AtomicFile::new(path, OverwriteBehavior::AllowOverwrite)
        .write(|file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .map_err(Into::into)
}

fn verify_writable(data_directory: &Path, guidance: &str) -> Result<(), String> {
    let probe_path = data_directory.join(format!(
        ".write-test-{}-{}",
        std::process::id(),
        crate::commands::now_epoch_ms()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut probe = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&probe_path)?;
        probe.write_all(b"BnetSwitchLite")?;
        probe.sync_all()?;
        drop(probe);
        fs::remove_file(&probe_path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&probe_path);
    }

    result.map_err(|error| {
        format!(
            "应用数据目录不可写。{guidance}：{}（{error}）",
            data_directory.display()
        )
    })
}

#[cfg(target_os = "macos")]
fn macos_application_support() -> Result<PathBuf, String> {
    use objc2_foundation::{
        NSSearchPathDirectory, NSSearchPathDomainMask, NSSearchPathForDirectoriesInDomains,
    };

    let directories = NSSearchPathForDirectoriesInDomains(
        NSSearchPathDirectory::ApplicationSupportDirectory,
        NSSearchPathDomainMask::UserDomainMask,
        true,
    );
    let directory = directories
        .firstObject()
        .ok_or_else(|| "无法确定当前用户的 Application Support 目录".to_owned())?;
    let path = PathBuf::from(directory.to_string());
    if !path.is_absolute() {
        return Err("系统返回了无效的 Application Support 路径".into());
    }
    Ok(path)
}

#[cfg(all(test, windows))]
mod tests {
    use super::{DataStore, HiddenAccountKey, SettingsDocument};
    use std::collections::HashSet;

    #[test]
    fn settings_round_trip_in_executable_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DataStore::open_for_windows(temporary.path()).unwrap();
        assert_eq!(
            store.data_directory(),
            temporary.path().join("BnetSwitchLiteData")
        );
        store
            .save_settings(&SettingsDocument {
                version: 1,
                client_executable_path: Some(r"C:\Battle.net\Battle.net.exe".into()),
            })
            .unwrap();

        let settings = store.load_settings().unwrap();
        assert_eq!(
            settings.client_executable_path.as_deref(),
            Some(r"C:\Battle.net\Battle.net.exe")
        );
    }

    #[test]
    fn data_directory_is_always_next_to_executable() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DataStore::open_for_windows(temporary.path()).unwrap();
        assert_eq!(
            store.data_directory(),
            temporary.path().join("BnetSwitchLiteData")
        );
    }

    #[test]
    fn detecting_accounts_restores_only_accounts_still_in_battle_net() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DataStore::open_for_windows(temporary.path()).unwrap();
        let present = HiddenAccountKey {
            environment: "us.actual.battle.net".into(),
            account_id: "1:2".into(),
        };
        let absent = HiddenAccountKey {
            environment: "cn.actual.battlenet.com.cn".into(),
            account_id: "3:4".into(),
        };
        store
            .save_hidden_accounts(&HashSet::from([present.clone(), absent.clone()]))
            .unwrap();

        let remaining = store.restore_detected_accounts([present]).unwrap();

        assert_eq!(remaining, HashSet::from([absent]));
        assert_eq!(store.load_hidden_accounts().unwrap(), remaining);
    }
}
