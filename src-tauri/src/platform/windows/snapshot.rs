use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{contracts::AccountKey, data_store::write_json_atomic, error::AppError};

const SNAPSHOT_VERSION: u8 = 1;
const PROFILE_VERSION: u8 = 1;
const REPARSE_POINT_ATTRIBUTE: u32 = 0x400;
const INTERNAL_FILE_PREFIX: &str = ".bnetswitchlite-";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManifestFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileSetManifest {
    pub version: u8,
    pub created_at: u64,
    pub files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotManifest {
    version: u8,
    account: AccountKey,
    generation: String,
    created_at: u64,
    files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountProfile {
    version: u8,
    verified_login: bool,
    account: AccountKey,
    active_generation: String,
    last_saved_at: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SnapshotSummary {
    pub last_saved_at: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ValidatedSnapshot {
    pub account: AccountKey,
    pub files: Vec<ManifestFile>,
    pub files_directory: PathBuf,
}

pub(crate) struct SnapshotStore {
    accounts_directory: PathBuf,
}

impl SnapshotStore {
    pub fn new(data_directory: &Path) -> Result<Self, AppError> {
        let accounts_directory = data_directory.join("accounts");
        fs::create_dir_all(&accounts_directory)
            .map_err(|error| AppError::Snapshot(format!("无法创建快照目录：{error}")))?;
        reject_reparse_point(&accounts_directory)?;
        Ok(Self { accounts_directory })
    }

    pub fn save(
        &self,
        account: &AccountKey,
        source_directory: &Path,
    ) -> Result<SnapshotSummary, AppError> {
        validate_account_key(account)?;
        reject_reparse_point(source_directory)?;

        let account_directory = self.account_directory(account);
        let generations_directory = account_directory.join("generations");
        fs::create_dir_all(&generations_directory)
            .map_err(|error| AppError::Snapshot(format!("无法创建账号快照目录：{error}")))?;
        reject_reparse_point(&account_directory)?;
        reject_reparse_point(&generations_directory)?;

        let generation = Uuid::new_v4().to_string();
        let staging_directory = generations_directory.join(format!(".staging-{generation}"));
        let final_directory = generations_directory.join(&generation);
        fs::create_dir(&staging_directory)
            .map_err(|error| AppError::Snapshot(format!("无法创建临时代际：{error}")))?;

        let result = (|| {
            let file_set = capture_file_set(source_directory, &staging_directory)?;
            require_battle_net_config(&file_set.files)?;

            let manifest = SnapshotManifest {
                version: SNAPSHOT_VERSION,
                account: account.clone(),
                generation: generation.clone(),
                created_at: file_set.created_at,
                files: file_set.files,
            };
            write_pretty_json(&staging_directory.join("manifest.json"), &manifest)?;
            validate_file_set(&staging_directory.join("files"), &manifest.files)?;

            fs::rename(&staging_directory, &final_directory)
                .map_err(|error| AppError::Snapshot(format!("无法发布快照代际：{error}")))?;

            let profile = AccountProfile {
                version: PROFILE_VERSION,
                verified_login: true,
                account: account.clone(),
                active_generation: generation.clone(),
                last_saved_at: manifest.created_at,
            };
            write_pretty_json(&account_directory.join("profile.json"), &profile)?;
            cleanup_old_generations(&generations_directory, &generation);

            Ok(SnapshotSummary {
                last_saved_at: manifest.created_at,
            })
        })();

        if result.is_err() {
            let _ = fs::remove_dir_all(&staging_directory);
        }
        result
    }

    pub fn summary(&self, account: &AccountKey) -> Result<Option<SnapshotSummary>, AppError> {
        let profile_path = self.account_directory(account).join("profile.json");
        if !profile_path.exists() {
            return Ok(None);
        }
        let profile: AccountProfile = read_json(&profile_path, "账号快照索引")?;
        validate_profile(account, &profile)?;
        Ok(Some(SnapshotSummary {
            last_saved_at: profile.last_saved_at,
        }))
    }

    pub fn validate(&self, account: &AccountKey) -> Result<ValidatedSnapshot, AppError> {
        validate_account_key(account)?;
        let account_directory = self.account_directory(account);
        reject_reparse_point(&account_directory)?;
        let profile: AccountProfile =
            read_json(&account_directory.join("profile.json"), "账号快照索引")?;
        validate_profile(account, &profile)?;
        validate_generation_id(&profile.active_generation)?;

        let generation_directory = account_directory
            .join("generations")
            .join(&profile.active_generation);
        reject_reparse_point(&generation_directory)?;
        let manifest: SnapshotManifest =
            read_json(&generation_directory.join("manifest.json"), "账号快照清单")?;
        if manifest.version != SNAPSHOT_VERSION
            || manifest.account != *account
            || manifest.generation != profile.active_generation
        {
            return Err(AppError::Snapshot("账号快照索引与清单不一致".into()));
        }
        require_battle_net_config(&manifest.files)?;
        let files_directory = generation_directory.join("files");
        validate_file_set(&files_directory, &manifest.files)?;

        Ok(ValidatedSnapshot {
            account: manifest.account,
            files: manifest.files,
            files_directory,
        })
    }

    pub fn remove(&self, account: &AccountKey) -> Result<(), AppError> {
        validate_account_key(account)?;
        reject_reparse_point(&self.accounts_directory)?;
        let account_directory = self.account_directory(account);
        match fs::symlink_metadata(&account_directory) {
            Ok(_) => reject_reparse_tree(&account_directory)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(AppError::Snapshot(format!(
                    "无法读取待删除账号快照：{error}"
                )));
            }
        }

        let quarantine = self
            .accounts_directory
            .join(format!(".deleting-{}", Uuid::new_v4()));
        fs::rename(&account_directory, &quarantine)
            .map_err(|error| AppError::Snapshot(format!("无法隔离待删除账号快照：{error}")))?;
        if let Err(validation_error) = reject_reparse_tree(&quarantine) {
            return match fs::rename(&quarantine, &account_directory) {
                Ok(()) => Err(validation_error),
                Err(restore_error) => Err(AppError::Snapshot(format!(
                    "{}；待删除数据恢复失败：{restore_error}",
                    validation_error.nested_message()
                ))),
            };
        }
        if let Err(cleanup_error) = fs::remove_dir_all(&quarantine) {
            return match fs::rename(&quarantine, &account_directory) {
                Ok(()) => Err(AppError::Snapshot(format!(
                    "无法删除账号快照，已恢复原数据：{cleanup_error}"
                ))),
                Err(restore_error) => Err(AppError::Snapshot(format!(
                    "无法删除账号快照：{cleanup_error}；待删除数据恢复失败：{restore_error}"
                ))),
            };
        }
        Ok(())
    }

    fn account_directory(&self, account: &AccountKey) -> PathBuf {
        self.accounts_directory
            .join(account_directory_name(account))
    }
}

fn reject_reparse_tree(path: &Path) -> Result<(), AppError> {
    reject_reparse_point(path)?;
    for entry in fs::read_dir(path)
        .map_err(|error| AppError::Snapshot(format!("无法读取账号快照目录：{error}")))?
    {
        let entry =
            entry.map_err(|error| AppError::Snapshot(format!("无法读取账号快照项：{error}")))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| AppError::Snapshot(format!("无法读取账号快照项属性：{error}")))?;
        if has_reparse_attribute(&metadata) {
            return Err(AppError::Snapshot(
                "账号快照包含不受支持的链接或重解析点".into(),
            ));
        }
        if metadata.is_dir() {
            reject_reparse_tree(&entry.path())?;
        }
    }
    Ok(())
}

pub(crate) fn capture_file_set(
    source_directory: &Path,
    output_directory: &Path,
) -> Result<FileSetManifest, AppError> {
    reject_reparse_point(source_directory)?;
    fs::create_dir_all(output_directory)
        .map_err(|error| AppError::Snapshot(format!("无法创建事务目录：{error}")))?;
    let files_directory = output_directory.join("files");
    fs::create_dir(&files_directory)
        .map_err(|error| AppError::Snapshot(format!("无法创建文件集目录：{error}")))?;

    let source_files = enumerate_regular_files(source_directory)?;
    let mut files = Vec::with_capacity(source_files.len());
    for (name, source_path) in source_files {
        let destination = files_directory.join(&name);
        let (size, sha256) = copy_and_hash(&source_path, &destination)?;
        files.push(ManifestFile { name, size, sha256 });
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(FileSetManifest {
        version: SNAPSHOT_VERSION,
        created_at: now_epoch_ms(),
        files,
    })
}

pub(crate) fn validate_file_set(
    directory: &Path,
    expected: &[ManifestFile],
) -> Result<(), AppError> {
    reject_reparse_point(directory)?;
    let expected_by_name = expected
        .iter()
        .map(|file| (file.name.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    if expected_by_name.len() != expected.len() {
        return Err(AppError::Snapshot("快照清单包含重复文件名".into()));
    }

    let actual = enumerate_regular_files(directory)?;
    let actual_names = actual.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected_names = expected_by_name.keys().copied().collect::<BTreeSet<_>>();
    if actual_names != expected_names {
        return Err(AppError::Snapshot("快照文件集合与清单不一致".into()));
    }

    for (name, path) in actual {
        validate_file_name(&name)?;
        let expected = expected_by_name[&name.as_str()];
        let (size, hash) = hash_file(&path)?;
        if size != expected.size || hash != expected.sha256 {
            return Err(AppError::Snapshot(format!("快照文件校验失败：{name}")));
        }
    }
    Ok(())
}

pub(crate) fn enumerate_regular_files(
    directory: &Path,
) -> Result<BTreeMap<String, PathBuf>, AppError> {
    let mut files = BTreeMap::new();
    let entries = fs::read_dir(directory).map_err(|error| {
        AppError::Snapshot(format!("无法读取目录 {}：{error}", directory.display()))
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|error| AppError::Snapshot(format!("无法读取目录项：{error}")))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| AppError::Snapshot(format!("无法读取文件属性：{error}")))?;
        if has_reparse_attribute(&metadata) {
            return Err(AppError::Snapshot(format!(
                "拒绝处理重解析点：{}",
                entry.path().display()
            )));
        }
        if !metadata.is_file() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| AppError::Snapshot("目录中包含无法表示的文件名".into()))?;
        validate_file_name(&name)?;
        files.insert(name, entry.path());
    }
    Ok(files)
}

pub(crate) fn copy_and_hash(source: &Path, destination: &Path) -> Result<(u64, String), AppError> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| AppError::Snapshot(format!("无法读取源文件属性：{error}")))?;
    if !metadata.is_file() || has_reparse_attribute(&metadata) {
        return Err(AppError::Snapshot(format!(
            "拒绝复制非常规文件：{}",
            source.display()
        )));
    }
    let mut reader = BufReader::new(
        File::open(source)
            .map_err(|error| AppError::Snapshot(format!("无法打开源文件：{error}")))?,
    );
    let destination_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| AppError::Snapshot(format!("无法创建目标文件：{error}")))?;
    let mut writer = BufWriter::new(destination_file);
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| AppError::Snapshot(format!("无法读取源文件：{error}")))?;
        if count == 0 {
            break;
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|error| AppError::Snapshot(format!("无法写入目标文件：{error}")))?;
        hasher.update(&buffer[..count]);
        total += count as u64;
    }
    writer
        .flush()
        .and_then(|_| writer.get_ref().sync_all())
        .map_err(|error| AppError::Snapshot(format!("无法持久化目标文件：{error}")))?;
    Ok((total, format!("{:x}", hasher.finalize())))
}

pub(crate) fn hash_file(path: &Path) -> Result<(u64, String), AppError> {
    let mut reader = BufReader::new(
        File::open(path).map_err(|error| AppError::Snapshot(format!("无法打开文件：{error}")))?,
    );
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| AppError::Snapshot(format!("无法读取文件：{error}")))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        total += count as u64;
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

pub(crate) fn write_file_set_manifest(
    path: &Path,
    manifest: &FileSetManifest,
) -> Result<(), AppError> {
    write_pretty_json(path, manifest)
}

pub(crate) fn read_file_set_manifest(path: &Path) -> Result<FileSetManifest, AppError> {
    let manifest: FileSetManifest = read_json(path, "事务文件清单")?;
    if manifest.version != SNAPSHOT_VERSION {
        return Err(AppError::Snapshot(format!(
            "不支持的文件清单版本：{}",
            manifest.version
        )));
    }
    Ok(manifest)
}

pub(crate) fn reject_reparse_point(path: &Path) -> Result<(), AppError> {
    for component in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        let metadata = fs::symlink_metadata(component).map_err(|error| {
            AppError::Snapshot(format!("无法读取路径属性 {}：{error}", component.display()))
        })?;
        if has_reparse_attribute(&metadata) {
            return Err(AppError::Snapshot(format!(
                "拒绝处理包含重解析点的路径：{}",
                component.display()
            )));
        }
    }
    Ok(())
}

fn validate_profile(account: &AccountKey, profile: &AccountProfile) -> Result<(), AppError> {
    if profile.version != PROFILE_VERSION {
        return Err(AppError::Snapshot(format!(
            "不支持的账号快照版本：{}",
            profile.version
        )));
    }
    if profile.account != *account {
        return Err(AppError::Snapshot("账号快照索引与请求账号不一致".into()));
    }
    if !profile.verified_login {
        return Err(AppError::Snapshot("账号快照未经过登录验证".into()));
    }
    validate_generation_id(&profile.active_generation)
}

fn validate_generation_id(generation: &str) -> Result<(), AppError> {
    Uuid::parse_str(generation)
        .map(|_| ())
        .map_err(|_| AppError::Snapshot("快照代际标识无效".into()))
}

fn validate_account_key(account: &AccountKey) -> Result<(), AppError> {
    if account.environment.trim().is_empty() || account.account_id.trim().is_empty() {
        return Err(AppError::Snapshot("账号标识为空".into()));
    }
    Ok(())
}

fn validate_file_name(name: &str) -> Result<(), AppError> {
    let path = Path::new(name);
    if name.is_empty()
        || name.starts_with(INTERNAL_FILE_PREFIX)
        || path.components().count() != 1
        || name == "."
        || name == ".."
        || name.ends_with([' ', '.'])
        || name.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
    {
        return Err(AppError::Snapshot(format!("快照文件名无效：{name}")));
    }
    Ok(())
}

fn require_battle_net_config(files: &[ManifestFile]) -> Result<(), AppError> {
    if files
        .iter()
        .any(|file| file.name.eq_ignore_ascii_case("Battle.net.config"))
    {
        Ok(())
    } else {
        Err(AppError::Snapshot(
            "未找到 Battle.net.config，拒绝保存不完整快照".into(),
        ))
    }
}

fn account_directory_name(account: &AccountKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(account.environment.as_bytes());
    hasher.update([0]);
    hasher.update(account.account_id.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn write_pretty_json<T: Serialize>(path: &Path, value: &T) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| AppError::Snapshot(format!("无法生成 JSON：{error}")))?;
    write_json_atomic(path, &bytes)
        .map_err(|error| AppError::Snapshot(format!("无法原子写入 {}：{error}", path.display())))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, label: &str) -> Result<T, AppError> {
    let bytes =
        fs::read(path).map_err(|error| AppError::Snapshot(format!("无法读取{label}：{error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Snapshot(format!("{label}格式无效：{error}")))
}

fn cleanup_old_generations(generations_directory: &Path, active: &str) {
    let Ok(entries) = fs::read_dir(generations_directory) else {
        return;
    };
    let mut generations = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name == active || name.starts_with(".staging-") || !entry.path().is_dir() {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    generations.sort_by(|left, right| right.0.cmp(&left.0));
    for (_, path) in generations.into_iter().skip(1) {
        let _ = fs::remove_dir_all(path);
    }
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(windows)]
fn has_reparse_attribute(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & REPARSE_POINT_ATTRIBUTE != 0
}

#[cfg(not(windows))]
fn has_reparse_attribute(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use std::{fs, fs::OpenOptions};

    #[cfg(windows)]
    use std::os::windows::fs::OpenOptionsExt;

    use tempfile::tempdir;

    use super::SnapshotStore;
    use crate::contracts::AccountKey;

    fn key() -> AccountKey {
        AccountKey {
            environment: "cn".into(),
            account_id: "42".into(),
        }
    }

    fn other_key() -> AccountKey {
        AccountKey {
            environment: "us".into(),
            account_id: "84".into(),
        }
    }

    #[test]
    fn publishes_new_generation_without_destroying_previous_snapshot() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("roaming");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("Battle.net.config"), b"first").unwrap();
        let store = SnapshotStore::new(&temporary.path().join("data")).unwrap();
        let first = store.save(&key(), &source).unwrap();

        fs::write(source.join("Battle.net.config"), b"second").unwrap();
        fs::write(source.join("extra.db"), b"extra").unwrap();
        let second = store.save(&key(), &source).unwrap();

        assert!(second.last_saved_at >= first.last_saved_at);
        let snapshot = store.validate(&key()).unwrap();
        assert_eq!(snapshot.files.len(), 2);
        assert_eq!(
            fs::read(snapshot.files_directory.join("Battle.net.config")).unwrap(),
            b"second"
        );
    }

    #[test]
    fn refuses_snapshot_without_battle_net_config() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("roaming");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("other.json"), b"{}").unwrap();
        let store = SnapshotStore::new(&temporary.path().join("data")).unwrap();

        assert!(store.save(&key(), &source).is_err());
        assert!(store.summary(&key()).unwrap().is_none());
    }

    #[test]
    fn snapshot_without_current_verified_login_field_is_rejected() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("roaming");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("Battle.net.config"), b"first").unwrap();
        let store = SnapshotStore::new(&temporary.path().join("data")).unwrap();
        store.save(&key(), &source).unwrap();
        let profile_path = store.account_directory(&key()).join("profile.json");
        let mut profile: serde_json::Value =
            serde_json::from_slice(&fs::read(&profile_path).unwrap()).unwrap();
        profile.as_object_mut().unwrap().remove("verifiedLogin");
        super::write_pretty_json(&profile_path, &profile).unwrap();

        assert!(store.summary(&key()).is_err());
        assert!(store.validate(&key()).is_err());
    }

    #[test]
    fn rejects_windows_path_and_stream_file_names() {
        for name in ["..\\escape", "file:stream", "trailing.", "bad?.json"] {
            assert!(super::validate_file_name(name).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn removing_one_account_deletes_all_of_its_generations_only() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("roaming");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("Battle.net.config"), b"first").unwrap();
        let store = SnapshotStore::new(&temporary.path().join("data")).unwrap();
        store.save(&key(), &source).unwrap();
        fs::write(source.join("Battle.net.config"), b"second").unwrap();
        store.save(&key(), &source).unwrap();
        store.save(&other_key(), &source).unwrap();

        store.remove(&key()).unwrap();

        assert!(store.summary(&key()).unwrap().is_none());
        assert!(store.validate(&other_key()).is_ok());
        assert_eq!(
            fs::read_dir(temporary.path().join("data/accounts"))
                .unwrap()
                .count(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn failed_removal_keeps_the_original_snapshot_retryable() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("roaming");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("Battle.net.config"), b"first").unwrap();
        let store = SnapshotStore::new(&temporary.path().join("data")).unwrap();
        store.save(&key(), &source).unwrap();
        let snapshot = store.validate(&key()).unwrap();
        let _held = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(snapshot.files_directory.join("Battle.net.config"))
            .unwrap();

        assert!(store.remove(&key()).is_err());
        assert!(store.validate(&key()).is_ok());
    }
}
