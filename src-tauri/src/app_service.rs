use crate::{
    contracts::{AccountKey, AppSnapshot, LoginIntent, OperationEvent},
    error::AppError,
    login_completion::LoginCompletionToken,
    platform::{NativePlatformService, PlatformService},
};

pub(crate) struct AppService {
    platform: NativePlatformService,
}

impl AppService {
    pub fn new() -> Result<Self, AppError> {
        Ok(Self {
            platform: <NativePlatformService as PlatformService>::new()?,
        })
    }

    pub fn prepare_for_use(&mut self, report: &impl Fn(OperationEvent)) -> Result<(), AppError> {
        PlatformService::prepare_for_use(&mut self.platform, report)
    }

    pub fn snapshot(&self) -> Result<AppSnapshot, AppError> {
        PlatformService::snapshot(&self.platform)
    }

    pub fn detect_accounts(&mut self) -> Result<AppSnapshot, AppError> {
        PlatformService::detect_accounts(&mut self.platform)
    }

    pub fn switch_account(
        &mut self,
        target: &AccountKey,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        PlatformService::switch_account(&mut self.platform, target, report)
    }

    pub fn begin_login(
        &mut self,
        intent: LoginIntent,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        PlatformService::begin_login(&mut self.platform, intent, report)
    }

    pub fn complete_login(
        &mut self,
        session_id: &str,
        completion: &LoginCompletionToken,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        PlatformService::complete_login(&mut self.platform, session_id, completion, report)
    }

    pub fn cancel_login(
        &mut self,
        session_id: &str,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        PlatformService::cancel_login(&mut self.platform, session_id, report)
    }

    pub fn remove_account(&mut self, key: &AccountKey) -> Result<AppSnapshot, AppError> {
        PlatformService::remove_account(&mut self.platform, key)
    }

    pub fn set_client_path(&mut self, path: &str) -> Result<AppSnapshot, AppError> {
        PlatformService::set_client_path(&mut self.platform, path)
    }

    pub fn open_client(&mut self) -> Result<AppSnapshot, AppError> {
        PlatformService::open_client(&mut self.platform)
    }
}
