#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(windows)]
pub(crate) mod windows;

use crate::{
    contracts::{AccountKey, AppSnapshot, LoginIntent, OperationEvent},
    error::AppError,
    login_completion::LoginCompletionToken,
};

pub(crate) trait PlatformService: Sized {
    fn new() -> Result<Self, AppError>;

    fn prepare_for_use(&mut self, report: &impl Fn(OperationEvent)) -> Result<(), AppError>;

    fn snapshot(&self) -> Result<AppSnapshot, AppError>;

    fn detect_accounts(&mut self) -> Result<AppSnapshot, AppError>;

    fn switch_account(
        &mut self,
        target: &AccountKey,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError>;

    fn begin_login(
        &mut self,
        intent: LoginIntent,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError>;

    fn complete_login(
        &mut self,
        session_id: &str,
        completion: &LoginCompletionToken,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError>;

    fn cancel_login(
        &mut self,
        session_id: &str,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError>;

    fn remove_account(&mut self, key: &AccountKey) -> Result<AppSnapshot, AppError>;

    fn set_client_path(&mut self, path: &str) -> Result<AppSnapshot, AppError>;

    fn open_client(&mut self) -> Result<AppSnapshot, AppError>;
}

#[cfg(target_os = "macos")]
pub(crate) use macos::MacosPlatformService as NativePlatformService;
#[cfg(windows)]
pub(crate) use windows::WindowsPlatformService as NativePlatformService;
