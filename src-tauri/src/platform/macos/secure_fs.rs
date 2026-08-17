use std::{
    ffi::CString,
    fs::{self, File, OpenOptions, ReadDir},
    io::{ErrorKind, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

use uuid::Uuid;

use crate::error::AppError;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

pub(crate) struct UserFile {
    pub bytes: Vec<u8>,
    pub mode: u32,
}

pub(crate) fn ensure_private_directory(path: &Path) -> Result<(), AppError> {
    reject_symlink_chain(path, true)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_owned_directory_metadata(path, &metadata)?;
            if permission_mode(&metadata) != PRIVATE_DIRECTORY_MODE {
                fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                    .map_err(|error| snapshot_error("无法收紧敏感目录权限", error))?;
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let parent = parent(path)?;
            let parent_directory = open_directory(parent, None)?;
            mkdir_at(&parent_directory, file_name(path)?, PRIVATE_DIRECTORY_MODE)?;
            let directory = open_directory(path, None)?;
            directory
                .set_permissions(fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .map_err(|error| snapshot_error("无法设置敏感目录权限", error))?;
        }
        Err(error) => return Err(snapshot_error("无法读取敏感目录属性", error)),
    }
    open_directory(path, Some(PRIVATE_DIRECTORY_MODE))?;
    Ok(())
}

pub(crate) fn create_private_directory(path: &Path) -> Result<(), AppError> {
    reject_symlink_chain(path, true)?;
    let parent = parent(path)?;
    let parent_directory = open_directory(parent, Some(PRIVATE_DIRECTORY_MODE))?;
    mkdir_at(&parent_directory, file_name(path)?, PRIVATE_DIRECTORY_MODE)?;
    let directory = open_directory(path, None)?;
    directory
        .set_permissions(fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
        .map_err(|error| snapshot_error("无法设置敏感目录权限", error))?;
    open_directory(path, Some(PRIVATE_DIRECTORY_MODE))?;
    Ok(())
}

pub(crate) fn validate_private_directory(path: &Path) -> Result<(), AppError> {
    reject_symlink_chain(path, false)?;
    open_directory(path, Some(PRIVATE_DIRECTORY_MODE))?;
    Ok(())
}

pub(crate) fn read_private_directory(path: &Path) -> Result<ReadDir, AppError> {
    validate_private_directory(path)?;
    fs::read_dir(path).map_err(|error| snapshot_error("无法读取敏感目录", error))
}

pub(crate) fn private_directory_exists(path: &Path) -> Result<bool, AppError> {
    reject_symlink_chain(path, true)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_owned_directory_metadata(path, &metadata)?;
            open_directory(path, Some(PRIVATE_DIRECTORY_MODE))?;
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(snapshot_error("无法读取敏感目录属性", error)),
    }
}

pub(crate) fn sync_directory(path: &Path, mode: Option<u32>) -> Result<(), AppError> {
    open_directory(path, mode)?
        .sync_all()
        .map_err(|error| snapshot_error("无法持久化目录", error))
}

pub(crate) fn private_file_exists(path: &Path) -> Result<bool, AppError> {
    Ok(open_owned_regular_file_optional(path, Some(PRIVATE_FILE_MODE))?.is_some())
}

pub(crate) fn read_private_file(path: &Path) -> Result<Vec<u8>, AppError> {
    let mut file = open_private_file(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| snapshot_error("无法读取敏感文件", error))?;
    Ok(bytes)
}

pub(crate) fn read_user_file_optional(path: &Path) -> Result<Option<UserFile>, AppError> {
    match open_owned_regular_file_optional(path, None)? {
        Some(mut file) => {
            let metadata = file
                .metadata()
                .map_err(|error| snapshot_error("无法读取用户文件属性", error))?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)
                .map_err(|error| snapshot_error("无法读取用户文件", error))?;
            Ok(Some(UserFile {
                bytes,
                mode: permission_mode(&metadata),
            }))
        }
        None => Ok(None),
    }
}

pub(crate) fn write_private_file_new(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let parent_directory = open_directory(parent(path)?, Some(PRIVATE_DIRECTORY_MODE))?;
    let mut file = open_file_at_new(
        &parent_directory,
        file_name(path)?,
        PRIVATE_FILE_MODE,
        "无法创建敏感文件",
    )?;
    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))
        .map_err(|error| snapshot_error("无法设置敏感文件权限", error))?;
    validate_file_handle(path, &file, Some(PRIVATE_FILE_MODE))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| snapshot_error("无法持久化敏感文件", error))
}

pub(crate) fn write_private_file_replace(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let parent = parent(path)?;
    let parent_directory = open_directory(parent, Some(PRIVATE_DIRECTORY_MODE))?;
    if private_file_exists(path)? {
        open_private_file(path)?;
    }
    let temporary = parent.join(format!(".bnetswitchlite-{}.tmp", Uuid::new_v4()));
    write_private_file_new(&temporary, bytes)?;
    if let Err(error) = rename_at(
        &parent_directory,
        file_name(&temporary)?,
        &parent_directory,
        file_name(path)?,
    ) {
        let _ = remove_private_file(&temporary);
        return Err(snapshot_error("无法原子发布敏感文件", error));
    }
    open_private_file(path)?;
    sync_directory(parent, Some(PRIVATE_DIRECTORY_MODE))
}

pub(crate) fn write_user_file_replace(
    path: &Path,
    bytes: &[u8],
    mode: u32,
) -> Result<(), AppError> {
    reject_symlink_chain(path, true)?;
    let parent = parent(path)?;
    let parent_directory = open_directory(parent, None)?;
    let _ = open_owned_regular_file_optional(path, None)?;
    let temporary = parent.join(format!(".bnetswitchlite-{}.tmp", Uuid::new_v4()));
    let mode = mode & 0o777;
    let mut file = open_file_at_new(
        &parent_directory,
        file_name(&temporary)?,
        mode,
        "无法创建配置暂存文件",
    )?;
    file.set_permissions(fs::Permissions::from_mode(mode))
        .map_err(|error| snapshot_error("无法设置配置暂存文件权限", error))?;
    validate_file_handle(&temporary, &file, Some(mode))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = unlink_at(&parent_directory, file_name(&temporary)?, 0);
        return Err(snapshot_error("无法持久化配置暂存文件", error));
    }
    if let Err(error) = rename_at(
        &parent_directory,
        file_name(&temporary)?,
        &parent_directory,
        file_name(path)?,
    ) {
        let _ = unlink_at(&parent_directory, file_name(&temporary)?, 0);
        return Err(snapshot_error("无法原子发布配置文件", error));
    }
    open_owned_regular_file(path, Some(mode))?;
    sync_directory(parent, None)
}

pub(crate) fn remove_private_file(path: &Path) -> Result<(), AppError> {
    if !private_file_exists(path)? {
        return Ok(());
    }
    let parent_directory = open_directory(parent(path)?, Some(PRIVATE_DIRECTORY_MODE))?;
    unlink_at(&parent_directory, file_name(path)?, 0)
        .map_err(|error| snapshot_error("无法删除敏感文件", error))
}

pub(crate) fn remove_user_file(path: &Path) -> Result<(), AppError> {
    if open_owned_regular_file_optional(path, None)?.is_some() {
        let parent_directory = open_directory(parent(path)?, None)?;
        unlink_at(&parent_directory, file_name(path)?, 0)
            .map_err(|error| snapshot_error("无法删除用户文件", error))
    } else {
        Ok(())
    }
}

pub(crate) fn rename_private_directory(source: &Path, destination: &Path) -> Result<(), AppError> {
    validate_private_directory(source)?;
    let source_parent = parent(source)?;
    let destination_parent = parent(destination)?;
    if source_parent != destination_parent {
        return Err(AppError::Snapshot(
            "敏感目录只能在同一受保护目录内发布".into(),
        ));
    }
    let parent_directory = open_directory(source_parent, Some(PRIVATE_DIRECTORY_MODE))?;
    reject_symlink_chain(destination, true)?;
    match fs::symlink_metadata(destination) {
        Ok(_) => return Err(AppError::Snapshot("敏感目录发布目标已存在".into())),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(snapshot_error("无法读取敏感目录发布目标", error)),
    }
    rename_at(
        &parent_directory,
        file_name(source)?,
        &parent_directory,
        file_name(destination)?,
    )
    .map_err(|error| snapshot_error("无法原子发布敏感目录", error))?;
    validate_private_directory(destination)?;
    sync_directory(destination_parent, Some(PRIVATE_DIRECTORY_MODE))
}

pub(crate) fn remove_private_directory(path: &Path) -> Result<(), AppError> {
    reject_symlink_chain(path, true)?;
    match fs::symlink_metadata(path) {
        Ok(_) => validate_private_tree(path)?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(snapshot_error("无法读取敏感目录属性", error)),
    }
    let parent_path = parent(path)?;
    let parent_directory = open_directory(parent_path, Some(PRIVATE_DIRECTORY_MODE))?;
    let quarantine = format!(".deleting-{}", Uuid::new_v4());
    let quarantine_path = parent_path.join(&quarantine);
    rename_at(
        &parent_directory,
        file_name(path)?,
        &parent_directory,
        std::ffi::OsStr::new(&quarantine),
    )
    .map_err(|error| snapshot_error("无法隔离待清理敏感目录", error))?;
    validate_private_tree(&quarantine_path)?;
    if let Err(cleanup_error) = fs::remove_dir_all(&quarantine_path) {
        let restore = rename_at(
            &parent_directory,
            std::ffi::OsStr::new(&quarantine),
            &parent_directory,
            file_name(path)?,
        );
        return match restore {
            Ok(()) => Err(snapshot_error(
                "无法清理敏感目录，已恢复原路径",
                cleanup_error,
            )),
            Err(restore_error) => Err(AppError::Snapshot(format!(
                "无法清理敏感目录：{cleanup_error}；无法恢复待清理目录：{restore_error}"
            ))),
        };
    }
    parent_directory
        .sync_all()
        .map_err(|error| snapshot_error("无法持久化敏感目录清理", error))
}

pub(crate) fn reject_symlink_chain(path: &Path, allow_missing_leaf: bool) -> Result<(), AppError> {
    let mut components: Vec<PathBuf> = path
        .ancestors()
        .filter(|component| !component.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    components.reverse();
    for (index, component) in components.iter().enumerate() {
        match fs::symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::Snapshot(format!(
                    "安全路径不能经过符号链接：{}",
                    component.display()
                )));
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == ErrorKind::NotFound
                    && allow_missing_leaf
                    && index + 1 == components.len() => {}
            Err(error) => return Err(snapshot_error("无法读取安全路径属性", error)),
        }
    }
    Ok(())
}

fn open_private_file(path: &Path) -> Result<File, AppError> {
    reject_symlink_chain(path, false)?;
    open_owned_regular_file(path, Some(PRIVATE_FILE_MODE))
}

fn open_owned_regular_file(path: &Path, mode: Option<u32>) -> Result<File, AppError> {
    open_owned_regular_file_optional(path, mode)?.ok_or_else(|| {
        snapshot_error(
            "无法安全打开文件",
            std::io::Error::from(ErrorKind::NotFound),
        )
    })
}

fn open_owned_regular_file_optional(
    path: &Path,
    mode: Option<u32>,
) -> Result<Option<File>, AppError> {
    let parent_directory = open_directory(parent(path)?, None)?;
    let name = c_name(file_name(path)?)?;
    let descriptor = unsafe {
        libc::openat(
            parent_directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(snapshot_error("无法安全打开文件", error));
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    validate_file_handle(path, &file, mode)?;
    Ok(Some(file))
}

fn validate_file_handle(path: &Path, file: &File, mode: Option<u32>) -> Result<(), AppError> {
    let metadata = file
        .metadata()
        .map_err(|error| snapshot_error("无法读取文件句柄属性", error))?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(AppError::Snapshot(format!(
            "安全文件必须是无硬链接的普通文件：{}",
            path.display()
        )));
    }
    validate_owner(path, &metadata)?;
    if mode.is_some_and(|expected| permission_mode(&metadata) != expected) {
        return Err(AppError::Snapshot(format!(
            "安全文件权限必须为 {:04o}：{}",
            mode.unwrap_or_default(),
            path.display()
        )));
    }
    Ok(())
}

fn open_directory(path: &Path, mode: Option<u32>) -> Result<File, AppError> {
    if !path.is_absolute() {
        return Err(AppError::Snapshot(format!(
            "安全目录必须是绝对路径：{}",
            path.display()
        )));
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .map_err(|error| snapshot_error("无法打开文件系统根目录", error))?;
    for component in path.components() {
        let Component::Normal(component) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(AppError::Snapshot(format!(
                "安全目录包含无效路径组件：{}",
                path.display()
            )));
        };
        let name = c_name(component)?;
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(snapshot_error(
                "无法安全打开目录",
                std::io::Error::last_os_error(),
            ));
        }
        directory = unsafe { File::from_raw_fd(descriptor) };
    }
    let metadata = directory
        .metadata()
        .map_err(|error| snapshot_error("无法读取目录句柄属性", error))?;
    validate_owned_directory_metadata(path, &metadata)?;
    if mode.is_some_and(|expected| permission_mode(&metadata) != expected) {
        return Err(AppError::Snapshot(format!(
            "敏感目录权限必须为 {:04o}：{}",
            mode.unwrap_or_default(),
            path.display()
        )));
    }
    Ok(directory)
}

fn validate_owned_directory_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), AppError> {
    if !metadata.file_type().is_dir() {
        return Err(AppError::Snapshot(format!(
            "安全路径不是目录：{}",
            path.display()
        )));
    }
    validate_owner(path, metadata)
}

fn validate_owner(path: &Path, metadata: &fs::Metadata) -> Result<(), AppError> {
    let current_uid = unsafe { libc::geteuid() };
    if metadata.uid() != current_uid {
        return Err(AppError::Snapshot(format!(
            "安全路径不属于当前用户：{}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_private_tree(path: &Path) -> Result<(), AppError> {
    validate_private_directory(path)?;
    for entry in read_private_directory(path)? {
        let entry = entry.map_err(|error| snapshot_error("无法读取敏感目录项", error))?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path)
            .map_err(|error| snapshot_error("无法读取敏感目录项属性", error))?;
        if metadata.file_type().is_symlink() {
            return Err(AppError::Snapshot(format!(
                "敏感目录包含符号链接：{}",
                entry_path.display()
            )));
        }
        if metadata.file_type().is_dir() {
            validate_private_tree(&entry_path)?;
        } else if metadata.file_type().is_file() {
            open_private_file(&entry_path)?;
        } else {
            return Err(AppError::Snapshot(format!(
                "敏感目录包含不支持的文件类型：{}",
                entry_path.display()
            )));
        }
    }
    Ok(())
}

fn parent(path: &Path) -> Result<&Path, AppError> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| AppError::Snapshot("安全路径没有父目录".into()))
}

fn file_name(path: &Path) -> Result<&std::ffi::OsStr, AppError> {
    path.file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| AppError::Snapshot("安全路径没有文件名".into()))
}

fn c_name(name: &std::ffi::OsStr) -> Result<CString, AppError> {
    CString::new(name.as_bytes()).map_err(|_| AppError::Snapshot("安全路径包含空字节".into()))
}

fn mkdir_at(parent: &File, name: &std::ffi::OsStr, mode: u32) -> Result<(), AppError> {
    let name = c_name(name)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(snapshot_error(
            "无法创建敏感目录",
            std::io::Error::last_os_error(),
        ))
    }
}

fn open_file_at_new(
    parent: &File,
    name: &std::ffi::OsStr,
    mode: u32,
    context: &str,
) -> Result<File, AppError> {
    let name = c_name(name)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if descriptor < 0 {
        Err(snapshot_error(context, std::io::Error::last_os_error()))
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

fn rename_at(
    source_parent: &File,
    source: &std::ffi::OsStr,
    destination_parent: &File,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    let source = c_name(source).map_err(app_error_as_io)?;
    let destination = c_name(destination).map_err(app_error_as_io)?;
    if unsafe {
        libc::renameat(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn unlink_at(parent: &File, name: &std::ffi::OsStr, flags: i32) -> std::io::Result<()> {
    let name = c_name(name).map_err(app_error_as_io)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn app_error_as_io(error: AppError) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

fn permission_mode(metadata: &fs::Metadata) -> u32 {
    metadata.mode() & 0o777
}

fn snapshot_error(context: &str, error: std::io::Error) -> AppError {
    AppError::Snapshot(format!("{context}：{error}"))
}
