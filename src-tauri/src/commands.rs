use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use tauri::{State, ipc::Channel};

use crate::{
    app_service::AppService,
    contracts::{AccountKey, AppSnapshot, LoginCompletionResult, LoginIntent, OperationEvent},
    error::AppError,
    login_completion::LoginCompletionRegistry,
};

pub struct AppState {
    service: Arc<Mutex<Option<AppService>>>,
    login_completions: Arc<LoginCompletionRegistry>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            service: Arc::new(Mutex::new(None)),
            login_completions: Arc::new(LoginCompletionRegistry::default()),
        }
    }

    fn service(&self) -> Arc<Mutex<Option<AppService>>> {
        Arc::clone(&self.service)
    }

    fn login_completions(&self) -> Arc<LoginCompletionRegistry> {
        Arc::clone(&self.login_completions)
    }
}

pub fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[tauri::command]
pub async fn get_app_snapshot(
    on_event: Channel<OperationEvent>,
    state: State<'_, AppState>,
) -> Result<AppSnapshot, String> {
    let service = state.service();
    run_blocking(service, move |service| {
        service.prepare_for_use(&|event| {
            let _ = on_event.send(event);
        })?;
        service.snapshot()
    })
    .await
}

#[tauri::command]
pub async fn refresh_accounts(state: State<'_, AppState>) -> Result<AppSnapshot, String> {
    let service = state.service();
    run_blocking(service, |service| service.detect_accounts()).await
}

#[tauri::command]
pub async fn switch_account(
    account_key: AccountKey,
    on_event: Channel<OperationEvent>,
    state: State<'_, AppState>,
) -> Result<AppSnapshot, String> {
    let service = state.service();
    run_blocking(service, move |service| {
        service.switch_account(&account_key, &|event| {
            let _ = on_event.send(event);
        })
    })
    .await
}

#[tauri::command]
pub async fn begin_login(
    intent: LoginIntent,
    on_event: Channel<OperationEvent>,
    state: State<'_, AppState>,
) -> Result<AppSnapshot, String> {
    let service = state.service();
    run_blocking(service, move |service| {
        service.begin_login(intent, &|event| {
            let _ = on_event.send(event);
        })
    })
    .await
}

#[tauri::command]
pub async fn complete_login(
    session_id: String,
    on_event: Channel<OperationEvent>,
    state: State<'_, AppState>,
) -> Result<LoginCompletionResult, String> {
    let service = state.service();
    let completions = state.login_completions();
    let token = completions
        .begin(session_id.clone())
        .map_err(String::from)?;
    let operation_token = token.clone();
    let result = run_blocking(service, move |service| {
        service.complete_login(&session_id, &operation_token, &|event| {
            let _ = on_event.send(event);
        })
    })
    .await
    .map(|snapshot| LoginCompletionResult {
        snapshot,
        cancelled: token.was_cancelled(),
    });
    completions.finish(&token).map_err(String::from)?;
    result
}

#[tauri::command]
pub fn request_login_cancellation(
    session_id: String,
    state: State<'_, AppState>,
) -> Result<crate::contracts::LoginCancellationStatus, String> {
    state
        .login_completions
        .request_cancellation(&session_id)
        .map_err(String::from)
}

#[tauri::command]
pub async fn cancel_login(
    session_id: String,
    on_event: Channel<OperationEvent>,
    state: State<'_, AppState>,
) -> Result<AppSnapshot, String> {
    let service = state.service();
    run_blocking(service, move |service| {
        service.cancel_login(&session_id, &|event| {
            let _ = on_event.send(event);
        })
    })
    .await
}

#[tauri::command]
pub async fn remove_account(
    account_key: AccountKey,
    state: State<'_, AppState>,
) -> Result<AppSnapshot, String> {
    let service = state.service();
    run_blocking(service, move |service| service.remove_account(&account_key)).await
}

#[tauri::command]
pub async fn set_client_path(
    executable_path: String,
    state: State<'_, AppState>,
) -> Result<AppSnapshot, String> {
    let service = state.service();
    run_blocking(service, move |service| {
        service.set_client_path(&executable_path)
    })
    .await
}

#[tauri::command]
pub async fn open_client(state: State<'_, AppState>) -> Result<AppSnapshot, String> {
    let service = state.service();
    run_blocking(service, AppService::open_client).await
}

async fn run_blocking(
    service: Arc<Mutex<Option<AppService>>>,
    operation: impl FnOnce(&mut AppService) -> Result<AppSnapshot, AppError> + Send + 'static,
) -> Result<AppSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut slot = service.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => AppError::OperationBusy,
            std::sync::TryLockError::Poisoned(_) => AppError::StateUnavailable,
        })?;
        if slot.is_none() {
            *slot = Some(AppService::new()?);
        }
        operation(slot.as_mut().ok_or(AppError::StateUnavailable)?)
    })
    .await
    .map_err(|error| format!("后台操作异常结束：{error}"))?
    .map_err(String::from)
}
