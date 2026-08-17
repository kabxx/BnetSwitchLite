use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{commands::now_epoch_ms, contracts::AccountKey, error::AppError};

use super::{backup_exclusion, preferences::PreferenceSnapshot, secure_fs};

const FORMAT_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SecurePayload {
    pub version: u8,
    pub account: Option<AccountKey>,
    pub created_at: u64,
    pub config: ConfigSnapshot,
    pub preferences: PreferenceSnapshot,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigSnapshot {
    pub present: bool,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub unix_mode: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotManifest {
    version: u8,
    id: String,
    payload_length: u64,
    payload_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountProfile {
    version: u8,
    verified_login: bool,
    active_generation: String,
    last_saved_at: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotSummary {
    pub last_saved_at: u64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct SnapshotCodec;

pub(crate) struct SnapshotStore {
    accounts_directory: PathBuf,
}

impl SnapshotCodec {
    pub fn write<T: Serialize>(&self, directory: &Path, value: &T) -> Result<String, AppError> {
        secure_fs::ensure_private_directory(directory)?;
        backup_exclusion::set_and_verify(directory).map_err(AppError::Snapshot)?;

        let id = Uuid::new_v4().to_string();
        let payload = serde_json::to_vec_pretty(value)
            .map_err(|error| AppError::Snapshot(format!("无法生成快照负载：{error}")))?;
        let manifest = SnapshotManifest {
            version: FORMAT_VERSION,
            id: id.clone(),
            payload_length: payload.len() as u64,
            payload_sha256: format!("{:x}", Sha256::digest(&payload)),
        };
        secure_write_new(&directory.join("payload.json"), &payload)?;
        secure_write_new(
            &directory.join("manifest.json"),
            &serde_json::to_vec_pretty(&manifest)
                .map_err(|error| AppError::Snapshot(format!("无法生成快照清单：{error}")))?,
        )?;
        secure_fs::sync_directory(directory, Some(0o700))?;
        Ok(id)
    }

    pub fn read<T: DeserializeOwned>(&self, directory: &Path) -> Result<(String, T), AppError> {
        secure_fs::validate_private_directory(directory)?;
        let manifest_bytes = secure_fs::read_private_file(&directory.join("manifest.json"))?;
        let manifest: SnapshotManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| AppError::Snapshot(format!("快照清单格式无效：{error}")))?;
        if manifest.version != FORMAT_VERSION || Uuid::parse_str(&manifest.id).is_err() {
            return Err(AppError::Snapshot("不支持的快照格式".into()));
        }
        let payload = secure_fs::read_private_file(&directory.join("payload.json"))?;
        if payload.len() as u64 != manifest.payload_length
            || format!("{:x}", Sha256::digest(&payload)) != manifest.payload_sha256
        {
            return Err(AppError::Snapshot("快照完整性校验失败".into()));
        }
        let value = serde_json::from_slice(&payload)
            .map_err(|error| AppError::Snapshot(format!("快照负载格式无效：{error}")))?;
        Ok((manifest.id, value))
    }

    fn account_directory_name(account: &AccountKey) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"BnetSwitchLite account path v1\0");
        hasher.update(account.stable_id());
        format!("{:x}", hasher.finalize())
    }
}

impl SnapshotStore {
    pub fn new(data_directory: &Path) -> Result<Self, AppError> {
        let accounts_directory = data_directory.join("accounts");
        secure_fs::ensure_private_directory(&accounts_directory)?;
        backup_exclusion::set_and_verify(&accounts_directory).map_err(AppError::Snapshot)?;
        Ok(Self { accounts_directory })
    }

    pub fn save(
        &self,
        account: &AccountKey,
        payload: &SecurePayload,
    ) -> Result<SnapshotSummary, AppError> {
        if payload.account.as_ref() != Some(account) || payload.version != FORMAT_VERSION {
            return Err(AppError::Snapshot("账号身份与认证快照不一致".into()));
        }
        if !payload.preferences.has_authentication_keys()? {
            return Err(AppError::Snapshot(
                "未检测到可保存的 macOS 战网认证状态".into(),
            ));
        }
        let account_directory = self
            .accounts_directory
            .join(SnapshotCodec::account_directory_name(account));
        let generations = account_directory.join("generations");
        secure_fs::ensure_private_directory(&account_directory)?;
        secure_fs::ensure_private_directory(&generations)?;
        backup_exclusion::set_and_verify(&account_directory).map_err(AppError::Snapshot)?;

        let generation = Uuid::new_v4().to_string();
        let staging = generations.join(format!(".staging-{generation}"));
        let final_directory = generations.join(&generation);
        secure_fs::create_private_directory(&staging)?;
        let result = (|| {
            let codec = SnapshotCodec;
            codec.write(&staging, payload)?;
            let (_, verified): (_, SecurePayload) = codec.read(&staging)?;
            if verified.account.as_ref() != Some(account) {
                return Err(AppError::Snapshot("账号快照发布前校验失败".into()));
            }
            secure_fs::rename_private_directory(&staging, &final_directory)?;
            let profile = AccountProfile {
                version: FORMAT_VERSION,
                verified_login: true,
                active_generation: generation.clone(),
                last_saved_at: payload.created_at,
            };
            secure_write_replace(
                &account_directory.join("profile.json"),
                &serde_json::to_vec_pretty(&profile)
                    .map_err(|error| AppError::Snapshot(format!("无法生成账号索引：{error}")))?,
            )?;
            for entry in secure_fs::read_private_directory(&generations)? {
                let entry = entry
                    .map_err(|error| AppError::Snapshot(format!("无法读取账号代际项：{error}")))?;
                if entry.file_name().to_string_lossy() != generation {
                    let _ = remove_private_directory(&entry.path());
                }
            }
            Ok(SnapshotSummary {
                last_saved_at: payload.created_at,
            })
        })();
        if result.is_err() {
            let _ = remove_private_directory(&staging);
        }
        result
    }

    pub fn summary(&self, account: &AccountKey) -> Result<Option<SnapshotSummary>, AppError> {
        let account_directory = self
            .accounts_directory
            .join(SnapshotCodec::account_directory_name(account));
        if !secure_fs::private_directory_exists(&account_directory)? {
            return Ok(None);
        }
        let profile_path = account_directory.join("profile.json");
        if !secure_fs::private_file_exists(&profile_path)? {
            return Ok(None);
        }
        let profile = read_profile(&profile_path)?;
        Ok(Some(SnapshotSummary {
            last_saved_at: profile.last_saved_at,
        }))
    }

    pub fn validate(&self, account: &AccountKey) -> Result<SecurePayload, AppError> {
        let account_directory = self
            .accounts_directory
            .join(SnapshotCodec::account_directory_name(account));
        secure_fs::validate_private_directory(&account_directory)?;
        let profile = read_profile(&account_directory.join("profile.json"))?;
        let generation = Uuid::parse_str(&profile.active_generation)
            .map_err(|_| AppError::Snapshot("账号快照代际标识无效".into()))?;
        let codec = SnapshotCodec;
        let (_, payload): (_, SecurePayload) = codec.read(
            &account_directory
                .join("generations")
                .join(generation.to_string()),
        )?;
        if payload.version != FORMAT_VERSION || payload.account.as_ref() != Some(account) {
            return Err(AppError::Snapshot("账号快照内容与索引不一致".into()));
        }
        if !payload.preferences.has_authentication_keys()? {
            return Err(AppError::Snapshot("账号认证快照不包含登录状态".into()));
        }
        validate_config_snapshot(&payload.config)?;
        Ok(payload)
    }

    pub fn remove(&self, account: &AccountKey) -> Result<(), AppError> {
        let account_directory = self
            .accounts_directory
            .join(SnapshotCodec::account_directory_name(account));
        remove_private_directory(&account_directory)
    }
}

pub(crate) fn capture_current(
    config_path: &Path,
    account: Option<AccountKey>,
) -> Result<SecurePayload, AppError> {
    let config = if let Some(file) = secure_fs::read_user_file_optional(config_path)? {
        ConfigSnapshot {
            present: true,
            sha256: format!("{:x}", Sha256::digest(&file.bytes)),
            bytes: file.bytes,
            unix_mode: file.mode,
        }
    } else {
        ConfigSnapshot {
            present: false,
            bytes: Vec::new(),
            sha256: format!("{:x}", Sha256::digest([])),
            unix_mode: 0o600,
        }
    };
    Ok(SecurePayload {
        version: FORMAT_VERSION,
        account,
        created_at: now_epoch_ms(),
        config,
        preferences: super::preferences::capture()?,
    })
}

pub(crate) fn apply_config(config_path: &Path, snapshot: &ConfigSnapshot) -> Result<(), AppError> {
    validate_config_snapshot(snapshot)?;
    if snapshot.present {
        secure_fs::write_user_file_replace(
            config_path,
            &snapshot.bytes,
            snapshot.unix_mode & 0o777,
        )?;
        let actual = secure_fs::read_user_file_optional(config_path)?
            .ok_or_else(|| AppError::Transaction("Battle.net.config 写入后不存在".into()))?
            .bytes;
        if format!("{:x}", Sha256::digest(&actual)) != snapshot.sha256 {
            return Err(AppError::Transaction(
                "Battle.net.config 写入后校验失败".into(),
            ));
        }
    } else if secure_fs::read_user_file_optional(config_path)?.is_some() {
        secure_fs::remove_user_file(config_path)?;
    }
    Ok(())
}

fn validate_config_snapshot(snapshot: &ConfigSnapshot) -> Result<(), AppError> {
    if snapshot.present
        && (snapshot.bytes.is_empty()
            || format!("{:x}", Sha256::digest(&snapshot.bytes)) != snapshot.sha256)
    {
        return Err(AppError::Snapshot("Battle.net.config 快照校验失败".into()));
    }
    if !snapshot.present && !snapshot.bytes.is_empty() {
        return Err(AppError::Snapshot("空配置快照包含意外数据".into()));
    }
    Ok(())
}

fn read_profile(path: &Path) -> Result<AccountProfile, AppError> {
    let profile: AccountProfile = serde_json::from_slice(&secure_fs::read_private_file(path)?)
        .map_err(|error| AppError::Snapshot(format!("账号快照索引无效：{error}")))?;
    if profile.version != FORMAT_VERSION {
        return Err(AppError::Snapshot("不支持的账号快照索引版本".into()));
    }
    if !profile.verified_login {
        return Err(AppError::Snapshot("账号快照未经过登录验证".into()));
    }
    Ok(profile)
}

fn secure_write_new(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    secure_fs::write_private_file_new(path, bytes)
}

pub(crate) fn secure_write_replace(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    secure_fs::write_private_file_replace(path, bytes)
}

pub(crate) fn remove_private_directory(path: &Path) -> Result<(), AppError> {
    secure_fs::remove_private_directory(path)
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::{ConfigSnapshot, SecurePayload, SnapshotStore};
    use crate::{contracts::AccountKey, platform::macos::preferences::PreferenceSnapshot};

    fn account(id: &str) -> AccountKey {
        AccountKey {
            environment: "cn.actual.battlenet.com.cn".into(),
            account_id: id.into(),
        }
    }

    fn payload(account: AccountKey) -> SecurePayload {
        let mut preferences = plist::Dictionary::new();
        preferences.insert(
            "UnifiedAuth/test".into(),
            plist::Value::String("fixture".into()),
        );
        let mut preference_bytes = Vec::new();
        plist::to_writer_binary(
            &mut preference_bytes,
            &plist::Value::Dictionary(preferences),
        )
        .unwrap();
        let mut empty_bytes = Vec::new();
        plist::to_writer_binary(
            &mut empty_bytes,
            &plist::Value::Dictionary(plist::Dictionary::new()),
        )
        .unwrap();
        SecurePayload {
            version: 1,
            account: Some(account),
            created_at: 1,
            config: ConfigSnapshot {
                present: false,
                bytes: Vec::new(),
                sha256: format!("{:x}", Sha256::digest([])),
                unix_mode: 0o600,
            },
            preferences: PreferenceSnapshot {
                any_host: preference_bytes,
                current_host: empty_bytes,
            },
        }
    }

    #[test]
    fn removing_one_account_keeps_other_snapshots() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let store = SnapshotStore::new(&root).unwrap();
        let first = account("first");
        let second = account("second");
        store.save(&first, &payload(first.clone())).unwrap();
        store.save(&second, &payload(second.clone())).unwrap();

        store.remove(&first).unwrap();

        assert!(store.summary(&first).unwrap().is_none());
        assert!(store.validate(&second).is_ok());
        store.remove(&first).unwrap();
    }

    #[test]
    fn snapshot_without_current_verified_login_field_is_rejected() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let store = SnapshotStore::new(&root).unwrap();
        let target = account("target");
        store.save(&target, &payload(target.clone())).unwrap();
        let profile_path = store
            .accounts_directory
            .join(super::SnapshotCodec::account_directory_name(&target))
            .join("profile.json");
        let mut profile: serde_json::Value = serde_json::from_slice(
            &crate::platform::macos::secure_fs::read_private_file(&profile_path).unwrap(),
        )
        .unwrap();
        profile.as_object_mut().unwrap().remove("verifiedLogin");
        super::secure_write_replace(&profile_path, &serde_json::to_vec_pretty(&profile).unwrap())
            .unwrap();

        assert!(store.summary(&target).is_err());
        assert!(store.validate(&target).is_err());
    }
}
