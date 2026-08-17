mod app_service;
pub(crate) mod backup_exclusion;
mod operation_lock;
mod paths;
mod preferences;
mod process;
mod recovery;
pub(crate) mod secure_fs;
mod secure_snapshot;

pub(crate) use app_service::MacosPlatformService;
