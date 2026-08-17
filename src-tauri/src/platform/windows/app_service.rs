use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    account_reader::{read_account_catalog, read_account_catalog_for_probe},
    commands::now_epoch_ms,
    contracts::{
        AccountCatalog, AccountKey, AppSnapshot, ClientSnapshot, ClientStatus, LoginIntent,
        OperationEvent, OperationKind, SnapshotStatus,
    },
    data_store::{DataStore, HiddenAccountKey, SettingsDocument},
    error::AppError,
    login_completion::LoginCompletionToken,
    service_common::{account_snapshots, emit, login_evidence_ready, wait_for_stable},
};

use super::{
    authentication::AuthenticationBaseline,
    login_session::{LoginRecoveryOutcome, LoginSessionManager},
    operation_lock::OperationGuard,
    paths::{BattleNetPaths, validate_client_executable},
    process::{
        ensure_client_stopped, graceful_stop, is_client_running, is_main_client_running, launch,
        launch_for_login, wait_for_client_started,
    },
    restore::{CommitOutcome, RecoveryOutcome, RestoreEngine, RestoreReceipt},
    snapshot::SnapshotStore,
};

const LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const CLIENT_STOP_TIMEOUT: Duration = Duration::from_secs(25);
const CLIENT_START_TIMEOUT: Duration = Duration::from_secs(10);
const ACCOUNT_VERIFY_TIMEOUT: Duration = Duration::from_secs(45);
const LOGIN_POLL_INTERVAL: Duration = Duration::from_millis(250);
const LOGIN_PROBE_ERROR_TIMEOUT: Duration = Duration::from_secs(10);

enum LoginEvidence {
    Pending,
    Ready(AccountKey),
    WrongAccount,
}

pub(crate) struct WindowsPlatformService {
    store: DataStore,
    paths: BattleNetPaths,
    hidden_accounts: HashSet<HiddenAccountKey>,
    settings: SettingsDocument,
    snapshots: SnapshotStore,
    restore: RestoreEngine,
    login_sessions: LoginSessionManager,
    startup_notice: Option<String>,
}

impl WindowsPlatformService {
    pub fn new() -> Result<Self, AppError> {
        let store = DataStore::open().map_err(AppError::DataStorage)?;
        let hidden_accounts = store
            .load_hidden_accounts()
            .map_err(AppError::DataStorage)?;
        let settings = store.load_settings().map_err(AppError::DataStorage)?;
        let paths = BattleNetPaths::discover()?;
        let snapshots = SnapshotStore::new(store.data_directory())?;
        let restore = RestoreEngine::new(store.data_directory(), &paths.roaming_dir)?;
        let login_sessions = LoginSessionManager::new(store.data_directory(), &paths.roaming_dir)?;
        let startup_notice = restore
            .has_pending_operation()
            .then(|| "检测到上次未完成的切换，正在安全恢复原配置。".to_owned());

        Ok(Self {
            store,
            paths,
            hidden_accounts,
            settings,
            snapshots,
            restore,
            login_sessions,
            startup_notice,
        })
    }

    pub fn prepare_for_use(&mut self, report: &impl Fn(OperationEvent)) -> Result<(), AppError> {
        let restore_pending = self.restore.has_pending_operation();
        let login_recovery_pending = self.login_sessions.needs_automatic_recovery()?;
        if !restore_pending && !login_recovery_pending {
            return Ok(());
        }
        let guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        if restore_pending {
            self.recover_pending(&guard, report)?;
        }
        if login_recovery_pending {
            let requires_active_write = self
                .login_sessions
                .automatic_recovery_requires_active_write()?;
            if requires_active_write && is_client_running()? {
                graceful_stop(CLIENT_STOP_TIMEOUT)?;
            }
            if requires_active_write {
                ensure_client_stopped(CLIENT_STOP_TIMEOUT)?;
            }
            let outcome = self.login_sessions.recover_if_needed(&self.restore, &|| {
                ensure_client_stopped(CLIENT_STOP_TIMEOUT)
            })?;
            if !matches!(outcome, LoginRecoveryOutcome::None) {
                self.startup_notice = Some("已恢复上次未完成登录前的战网配置。".into());
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Result<AppSnapshot, AppError> {
        self.snapshot_from_catalog(read_account_catalog(&self.paths.cached_data_db))
    }

    fn snapshot_from_catalog(
        &self,
        catalog_result: Result<AccountCatalog, AppError>,
    ) -> Result<AppSnapshot, AppError> {
        let (catalog, read_notice) = match catalog_result {
            Ok(catalog) => (catalog, None),
            Err(AppError::AccountDatabaseMissing) => {
                (Default::default(), Some("登录战网后点击刷新账号。".into()))
            }
            Err(error) => (Default::default(), Some(error.to_string())),
        };
        let client_path = self.resolved_client_path();
        let running = is_main_client_running()?;
        let accounts = account_snapshots(catalog.accounts, &self.hidden_accounts, |key| match self
            .snapshots
            .summary(key)
        {
            Ok(Some(summary)) => match self.snapshots.validate(key) {
                Ok(_) => (SnapshotStatus::Ready, Some(summary.last_saved_at), None),
                Err(error) => (
                    SnapshotStatus::Expired,
                    Some(summary.last_saved_at),
                    Some(error.to_string()),
                ),
            },
            Ok(None) => (SnapshotStatus::Missing, None, None),
            Err(error) => (SnapshotStatus::Expired, None, Some(error.to_string())),
        });

        Ok(AppSnapshot {
            app_name: "战网切号器".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            mode: "desktop".into(),
            platform: "windows".into(),
            data_directory: self.store.data_directory().display().to_string(),
            client: ClientSnapshot {
                status: if running {
                    ClientStatus::Running
                } else {
                    ClientStatus::Stopped
                },
                executable_path: client_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                detected_automatically: self.settings.client_executable_path.is_none()
                    && client_path.is_some(),
            },
            accounts,
            login_session: self.login_sessions.snapshot()?,
            notice: self.startup_notice.clone().or(read_notice),
            updated_at: now_epoch_ms(),
        })
    }

    pub fn detect_accounts(&mut self) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        self.require_client_path()?;
        let catalog = match read_account_catalog(&self.paths.cached_data_db) {
            Ok(catalog) => catalog,
            Err(AppError::AccountDatabaseMissing) => {
                return self.snapshot_from_catalog(Err(AppError::AccountDatabaseMissing));
            }
            Err(error) => return Err(error),
        };
        self.hidden_accounts = self
            .store
            .restore_detected_accounts(
                catalog
                    .accounts
                    .iter()
                    .map(|account| HiddenAccountKey::from(&account.key)),
            )
            .map_err(AppError::DataStorage)?;
        self.snapshot_from_catalog(Ok(catalog))
    }

    pub fn switch_account(
        &mut self,
        target_key: &AccountKey,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        let guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        self.recover_pending(&guard, report)?;
        self.ensure_no_login_session()?;
        emit(
            report,
            OperationKind::Switch,
            "validating",
            "正在切换账号",
            "正在校验目标账号快照",
            5,
        );
        let target = self.snapshots.validate(target_key)?;
        let client_path = self.require_client_path()?;
        let was_running = is_main_client_running()?;

        if was_running {
            emit(
                report,
                OperationKind::Switch,
                "stoppingClient",
                "正在切换账号",
                "正在安全关闭战网客户端",
                18,
            );
            graceful_stop(CLIENT_STOP_TIMEOUT)?;
        }
        let current = match (|| {
            wait_for_stable_directory(&self.paths.roaming_dir)?;
            Ok::<_, AppError>(read_account_catalog(&self.paths.cached_data_db)?.current_account_key)
        })() {
            Ok(current) => current,
            Err(error) => {
                return Err(restart_after_preparation_error(
                    was_running,
                    &client_path,
                    error,
                ));
            }
        };

        emit(
            report,
            OperationKind::Switch,
            "restoring",
            "正在切换账号",
            "正在事务化恢复目标账号",
            58,
        );
        if let Err(error) = ensure_client_stopped(CLIENT_STOP_TIMEOUT) {
            return Err(restart_after_preparation_error(
                was_running,
                &client_path,
                error,
            ));
        }
        let receipt =
            match self
                .restore
                .apply_snapshot(&target, current.clone(), was_running, &|| {
                    ensure_client_stopped(CLIENT_STOP_TIMEOUT)
                }) {
                Ok(receipt) => receipt,
                Err(error) => {
                    if was_running {
                        let restart = launch(&client_path);
                        return Err(match restart {
                            Ok(()) => error,
                            Err(restart_error) => combine_errors(error, restart_error),
                        });
                    }
                    return Err(error);
                }
            };

        emit(
            report,
            OperationKind::Switch,
            "launchingClient",
            "正在切换账号",
            "正在启动战网客户端",
            76,
        );
        if let Err(error) = launch(&client_path) {
            return self.rollback_switch(&receipt, &client_path, error, report);
        }

        emit(
            report,
            OperationKind::Switch,
            "verifying",
            "正在切换账号",
            "正在等待战网确认目标账号",
            88,
        );
        if let Err(error) = wait_for_account(&self.paths.cached_data_db, target_key) {
            return self.rollback_switch(&receipt, &client_path, error, report);
        }

        match self.restore.commit(&receipt) {
            Ok(CommitOutcome::Committed) => {}
            Ok(CommitOutcome::CleanupPending(error)) => {
                self.startup_notice = Some(format!(
                    "账号已切换成功，临时事务文件将在下次启动时继续清理：{error}"
                ));
            }
            Err(error) => {
                return self.rollback_switch(&receipt, &client_path, error, report);
            }
        }
        emit(
            report,
            OperationKind::Switch,
            "completed",
            "账号切换完成",
            "战网已确认目标账号",
            100,
        );
        self.snapshot()
    }

    pub fn begin_login(
        &mut self,
        intent: LoginIntent,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        let login_region = intent.requested_region();
        let guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        self.recover_pending(&guard, report)?;
        self.ensure_no_login_session()?;
        let client_path = self.require_client_path()?;
        let was_running = is_main_client_running()?;

        emit(
            report,
            OperationKind::Login,
            "preparing",
            "正在准备登录",
            "正在保存当前战网配置",
            8,
        );
        if was_running {
            emit(
                report,
                OperationKind::Login,
                "stoppingClient",
                "正在准备登录",
                "正在安全关闭战网客户端",
                16,
            );
            graceful_stop(CLIENT_STOP_TIMEOUT)?;
        }
        if let Err(error) = wait_for_stable_directory(&self.paths.roaming_dir) {
            return Err(restart_after_preparation_error(
                was_running,
                &client_path,
                error,
            ));
        }
        let previous_account = match read_account_catalog(&self.paths.cached_data_db) {
            Ok(catalog) => catalog.current_account_key,
            Err(AppError::AccountDatabaseMissing) => None,
            Err(error) => {
                if was_running {
                    let _ = launch(&client_path);
                }
                return Err(error);
            }
        };

        ensure_client_stopped(CLIENT_STOP_TIMEOUT)
            .map_err(|error| restart_after_preparation_error(was_running, &client_path, error))?;
        let authentication_baseline = AuthenticationBaseline::capture()
            .map_err(|error| restart_after_preparation_error(was_running, &client_path, error))?;

        emit(
            report,
            OperationKind::Login,
            "clearingPointer",
            "正在准备登录",
            "正在启动战网登录页",
            24,
        );
        let session = match self.login_sessions.begin(
            intent,
            was_running,
            previous_account,
            authentication_baseline,
            &self.restore,
            &|| ensure_client_stopped(CLIENT_STOP_TIMEOUT),
        ) {
            Ok(session) => session,
            Err(error) => {
                if was_running {
                    return match launch(&client_path) {
                        Ok(()) => Err(error),
                        Err(restart_error) => Err(combine_errors(error, restart_error)),
                    };
                }
                return Err(error);
            }
        };

        if let Err(launch_error) = launch_for_login(&client_path, login_region) {
            let rollback = self
                .login_sessions
                .rollback(&session.id, &self.restore, &|| {
                    ensure_client_stopped(CLIENT_STOP_TIMEOUT)
                });
            return match rollback {
                Ok(()) => Err(launch_error),
                Err(rollback_error) => Err(AppError::Login(format!(
                    "{}；恢复登录前配置失败：{}",
                    launch_error.nested_message(),
                    rollback_error.nested_message()
                ))),
            };
        }
        emit(
            report,
            OperationKind::Login,
            "awaitingUser",
            "请在战网中完成登录",
            "正在等待登录完成并自动保存",
            32,
        );
        self.snapshot()
    }

    pub fn complete_login(
        &mut self,
        session_id: &str,
        completion_control: &LoginCompletionToken,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        completion_control.activate()?;
        let result = self.complete_login_after_lock(session_id, completion_control, report);
        match result {
            Err(error) if completion_control.begin_failure_rollback() => {
                match self.rollback_failed_login_after_lock(session_id, report) {
                    Ok(_) => Err(error),
                    Err(rollback_error) => Err(AppError::Login(format!(
                        "{}；失败后恢复登录前配置未完成：{}",
                        error.nested_message(),
                        rollback_error.nested_message()
                    ))),
                }
            }
            Err(error) if completion_control.resolve_precommit_error() => {
                match self.cancel_login_after_lock(session_id, report) {
                    Ok(snapshot) => Ok(snapshot),
                    Err(rollback_error) => Err(AppError::Login(format!(
                        "{}；取消时恢复失败：{}",
                        error.nested_message(),
                        rollback_error.nested_message()
                    ))),
                }
            }
            result => result,
        }
    }

    fn complete_login_after_lock(
        &mut self,
        session_id: &str,
        completion_control: &LoginCompletionToken,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        let (intent, _previous_account) = self.login_sessions.require_awaiting(session_id)?;
        let client_path = self.require_client_path()?;
        let authentication_baseline = self.login_sessions.authentication_baseline(session_id)?;

        if completion_control.begin_rollback() {
            return self.cancel_login_after_lock(session_id, report);
        }

        emit(
            report,
            OperationKind::Login,
            "awaitingLogin",
            "正在等待登录",
            "正在等待战网登录",
            32,
        );

        let mut probe_error_started_at = None;
        let current = loop {
            if completion_control.begin_rollback() {
                return self.cancel_login_after_lock(session_id, report);
            }
            match self.probe_completed_login(session_id, &intent, &authentication_baseline) {
                Ok(LoginEvidence::Ready(current)) => break current,
                Ok(LoginEvidence::Pending | LoginEvidence::WrongAccount) => {
                    probe_error_started_at = None;
                    if !is_main_client_running()? {
                        return Err(AppError::Login("Battle.net 已退出，登录未完成".into()));
                    }
                    thread::sleep(LOGIN_POLL_INTERVAL);
                }
                Err(AppError::AccountDatabaseIncompatible(error)) => {
                    return Err(AppError::AccountDatabaseIncompatible(error));
                }
                Err(error) => {
                    let started_at = probe_error_started_at.get_or_insert_with(Instant::now);
                    if started_at.elapsed() >= LOGIN_PROBE_ERROR_TIMEOUT {
                        return Err(error);
                    }
                    thread::sleep(LOGIN_POLL_INTERVAL);
                }
            }
        };

        emit(
            report,
            OperationKind::Login,
            "stoppingClient",
            "正在保存登录结果",
            "正在关闭战网客户端",
            48,
        );
        if is_client_running()? {
            graceful_stop(CLIENT_STOP_TIMEOUT)?;
        }

        if completion_control.begin_rollback() {
            return self.cancel_login_after_lock(session_id, report);
        }

        let completion = (|| {
            wait_for_stable_directory(&self.paths.roaming_dir)?;
            self.login_sessions
                .require_completed_login_marker(session_id)?;
            let current_authentication = AuthenticationBaseline::capture()?;
            if !authentication_baseline.has_fresh_value_since(&current_authentication) {
                return Err(AppError::Login("尚未检测到本次登录产生的新认证状态".into()));
            }
            let catalog = read_account_catalog(&self.paths.cached_data_db)?;
            let verified = resolve_login_account(&intent, catalog.current_account_key)?;
            if verified != current {
                return Err(AppError::Login(
                    "登录结果与目标账号不一致，请确认登录的是所选账号。".into(),
                ));
            }

            if completion_control.cancellation_requested() {
                return Ok(false);
            }

            emit(
                report,
                OperationKind::Login,
                "capturing",
                "正在保存登录结果",
                "正在保存登录状态",
                68,
            );
            if !completion_control.begin_commit() {
                return Ok(false);
            }
            self.snapshots.save(&verified, &self.paths.roaming_dir)?;

            let hidden = HiddenAccountKey::from(&verified);
            self.hidden_accounts = self
                .store
                .load_hidden_accounts()
                .map_err(AppError::DataStorage)?;
            if self.hidden_accounts.contains(&hidden) {
                let mut next = self.hidden_accounts.clone();
                next.remove(&hidden);
                self.store
                    .save_hidden_accounts(&next)
                    .map_err(AppError::DataStorage)?;
                self.hidden_accounts = next;
            }
            match self.login_sessions.complete(session_id)? {
                CommitOutcome::Committed => {}
                CommitOutcome::CleanupPending(error) => {
                    self.startup_notice = Some(format!(
                        "账号已保存，临时恢复文件将在下次启动时继续清理：{error}"
                    ));
                }
            }
            Ok(true)
        })();

        match completion {
            Ok(true) => {}
            Ok(false) if completion_control.begin_rollback() => {
                return self.cancel_login_after_lock(session_id, report);
            }
            Ok(false) => return Err(AppError::StateUnavailable),
            Err(error) => return Err(error),
        }

        emit(
            report,
            OperationKind::Login,
            "launchingClient",
            "账号已保存",
            "正在启动战网客户端",
            88,
        );
        launch(&client_path)?;
        wait_for_client_started(CLIENT_START_TIMEOUT)?;
        emit(
            report,
            OperationKind::Login,
            "completed",
            "账号登录状态已保存",
            "登录状态已保存",
            100,
        );
        self.snapshot()
    }

    pub fn cancel_login(
        &mut self,
        session_id: &str,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        self.cancel_login_after_lock(session_id, report)
    }

    fn cancel_login_after_lock(
        &mut self,
        session_id: &str,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        let restart_required = self
            .login_sessions
            .previous_client_was_running(session_id)?;
        let client_path = self.require_client_path()?;
        emit(
            report,
            OperationKind::Login,
            "stoppingClient",
            "正在取消登录",
            "正在关闭战网客户端",
            72,
        );
        if is_client_running()? {
            graceful_stop(CLIENT_STOP_TIMEOUT)?;
        }
        ensure_client_stopped(CLIENT_STOP_TIMEOUT)?;
        emit(
            report,
            OperationKind::Login,
            "rollingBack",
            "正在取消登录",
            "正在恢复登录前状态",
            84,
        );
        self.login_sessions
            .rollback(session_id, &self.restore, &|| {
                ensure_client_stopped(CLIENT_STOP_TIMEOUT)
            })?;
        if restart_required {
            emit(
                report,
                OperationKind::Login,
                "launchingClient",
                "正在取消登录",
                "正在启动战网客户端",
                94,
            );
            launch(&client_path)?;
            wait_for_client_started(CLIENT_START_TIMEOUT)?;
        }
        emit(
            report,
            OperationKind::Login,
            "completed",
            "已取消登录",
            "登录前状态已恢复",
            100,
        );
        self.snapshot()
    }

    fn rollback_failed_login_after_lock(
        &mut self,
        session_id: &str,
        report: &impl Fn(OperationEvent),
    ) -> Result<(), AppError> {
        emit(
            report,
            OperationKind::Login,
            "rollingBack",
            "登录未完成",
            "正在恢复登录前的战网配置",
            70,
        );
        if is_client_running()? {
            graceful_stop(CLIENT_STOP_TIMEOUT)?;
        }
        ensure_client_stopped(CLIENT_STOP_TIMEOUT)?;
        self.login_sessions
            .rollback(session_id, &self.restore, &|| {
                ensure_client_stopped(CLIENT_STOP_TIMEOUT)
            })?;
        emit(
            report,
            OperationKind::Login,
            "completed",
            "登录未完成",
            "已恢复登录前的战网配置",
            100,
        );
        Ok(())
    }

    pub fn set_client_path(&mut self, path: &str) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        let path = validate_client_executable(&PathBuf::from(path))?;
        self.settings = self.store.load_settings().map_err(AppError::DataStorage)?;
        self.settings.client_executable_path = Some(path.display().to_string());
        self.store
            .save_settings(&self.settings)
            .map_err(AppError::DataStorage)?;
        self.snapshot()
    }

    pub fn open_client(&mut self) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        if is_main_client_running()? {
            return self.snapshot();
        }

        let path = self.require_client_path()?;
        launch(&path)?;
        wait_for_client_started(CLIENT_START_TIMEOUT)?;
        self.snapshot()
    }

    pub fn remove_account(&mut self, key: &AccountKey) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        self.ensure_no_login_session()?;
        self.snapshots.remove(key)?;
        self.hidden_accounts = self
            .store
            .load_hidden_accounts()
            .map_err(AppError::DataStorage)?;
        let hidden = HiddenAccountKey::from(key);
        let mut next = self.hidden_accounts.clone();
        next.insert(hidden);
        self.store
            .save_hidden_accounts(&next)
            .map_err(AppError::DataStorage)?;
        self.hidden_accounts = next;
        self.snapshot()
    }

    fn probe_completed_login(
        &self,
        session_id: &str,
        intent: &LoginIntent,
        authentication_baseline: &AuthenticationBaseline,
    ) -> Result<LoginEvidence, AppError> {
        if !self
            .login_sessions
            .completed_login_marker_present(session_id)?
        {
            return Ok(LoginEvidence::Pending);
        }
        let current_authentication = AuthenticationBaseline::capture()?;
        let fresh_authentication =
            authentication_baseline.has_fresh_value_since(&current_authentication);
        let current =
            read_account_catalog_for_probe(&self.paths.cached_data_db)?.current_account_key;
        Ok(classify_login_evidence(
            true,
            fresh_authentication,
            current,
            intent,
        ))
    }

    fn rollback_switch(
        &mut self,
        receipt: &RestoreReceipt,
        client_path: &Path,
        original_error: AppError,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        let original_error = original_error.nested_message();
        emit(
            report,
            OperationKind::Switch,
            "rollingBack",
            "切换未完成",
            "正在恢复切换前的账号配置",
            94,
        );
        if is_client_running()? {
            graceful_stop(CLIENT_STOP_TIMEOUT).map_err(|stop_error| {
                AppError::Transaction(format!(
                    "{original_error}；战网客户端仍在运行，暂未覆盖文件：{}。请关闭战网并重新打开本工具以继续恢复",
                    stop_error.nested_message()
                ))
            })?;
        }
        ensure_client_stopped(CLIENT_STOP_TIMEOUT).map_err(|stop_error| {
            AppError::Transaction(format!(
                "{original_error}；无法确认战网客户端保持退出状态，暂未覆盖文件：{}。请关闭战网并重新打开本工具以继续恢复",
                stop_error.nested_message()
            ))
        })?;
        self.restore
            .rollback(receipt, &|| ensure_client_stopped(CLIENT_STOP_TIMEOUT))
            .map_err(|rollback_error| {
                AppError::Transaction(format!(
                    "{original_error}；自动恢复未完成：{}。请勿启动战网，重新打开本工具继续恢复",
                    rollback_error.nested_message()
                ))
            })?;
        if receipt.previous_client_was_running {
            launch(client_path).map_err(|restart_error| {
                AppError::Transaction(format!(
                    "{original_error}；原配置已恢复，但无法重新启动战网：{}",
                    restart_error.nested_message()
                ))
            })?;
        }
        Err(AppError::Transaction(format!(
            "{original_error}；已恢复切换前的账号配置"
        )))
    }

    fn recover_pending(
        &mut self,
        _guard: &OperationGuard,
        report: &impl Fn(OperationEvent),
    ) -> Result<(), AppError> {
        if !self.restore.has_pending_operation() {
            return Ok(());
        }
        let requires_active_write = self.restore.pending_recovery_requires_active_write()?;
        emit(
            report,
            OperationKind::Recovery,
            "stoppingClient",
            "正在恢复上次操作",
            if requires_active_write {
                "正在确认战网客户端已退出"
            } else {
                "正在清理已完成的事务文件"
            },
            20,
        );
        let restart_required =
            requires_active_write && self.restore.pending_client_restart_required()?;
        if requires_active_write && is_client_running()? {
            graceful_stop(CLIENT_STOP_TIMEOUT)?;
        }
        if requires_active_write {
            ensure_client_stopped(CLIENT_STOP_TIMEOUT)?;
        }
        emit(
            report,
            OperationKind::Recovery,
            "rollingBack",
            "正在恢复上次操作",
            if requires_active_write {
                "正在还原切换前的账号配置"
            } else {
                "正在完成事务清理"
            },
            64,
        );
        let outcome = self
            .restore
            .recover_pending(&|| ensure_client_stopped(CLIENT_STOP_TIMEOUT))?;
        if restart_required {
            if let Some(path) = self.resolved_client_path() {
                launch(&path)?;
            } else {
                self.startup_notice = Some(
                    "已恢复上次操作前的配置，但未找到 Battle.net.exe，请手动启动客户端。".into(),
                );
            }
        }
        if !matches!(outcome, RecoveryOutcome::None) {
            self.startup_notice = Some(
                match outcome {
                    RecoveryOutcome::RolledBack => "已恢复上次未完成切换前的账号配置。",
                    RecoveryOutcome::CleanedCompleted => "已完成上次操作的安全清理。",
                    RecoveryOutcome::None => unreachable!(),
                }
                .into(),
            );
        }
        emit(
            report,
            OperationKind::Recovery,
            "completed",
            "恢复完成",
            "账号配置已回到一致状态",
            100,
        );
        Ok(())
    }

    fn resolved_client_path(&self) -> Option<PathBuf> {
        self.settings
            .client_executable_path
            .as_ref()
            .and_then(|path| validate_client_executable(&PathBuf::from(path)).ok())
            .or_else(|| self.paths.detect_client_executable())
    }

    fn require_client_path(&self) -> Result<PathBuf, AppError> {
        self.resolved_client_path()
            .ok_or(AppError::ClientExecutableMissing)
    }

    fn ensure_no_login_session(&self) -> Result<(), AppError> {
        if self.login_sessions.snapshot()?.is_some() {
            return Err(AppError::Login("请先完成或取消当前登录流程".into()));
        }
        Ok(())
    }
}

impl crate::platform::PlatformService for WindowsPlatformService {
    fn new() -> Result<Self, AppError> {
        WindowsPlatformService::new()
    }

    fn prepare_for_use(&mut self, report: &impl Fn(OperationEvent)) -> Result<(), AppError> {
        WindowsPlatformService::prepare_for_use(self, report)
    }

    fn snapshot(&self) -> Result<AppSnapshot, AppError> {
        WindowsPlatformService::snapshot(self)
    }

    fn detect_accounts(&mut self) -> Result<AppSnapshot, AppError> {
        WindowsPlatformService::detect_accounts(self)
    }

    fn switch_account(
        &mut self,
        target: &AccountKey,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        WindowsPlatformService::switch_account(self, target, report)
    }

    fn begin_login(
        &mut self,
        intent: LoginIntent,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        WindowsPlatformService::begin_login(self, intent, report)
    }

    fn complete_login(
        &mut self,
        session_id: &str,
        completion: &LoginCompletionToken,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        WindowsPlatformService::complete_login(self, session_id, completion, report)
    }

    fn cancel_login(
        &mut self,
        session_id: &str,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        WindowsPlatformService::cancel_login(self, session_id, report)
    }

    fn remove_account(&mut self, key: &AccountKey) -> Result<AppSnapshot, AppError> {
        WindowsPlatformService::remove_account(self, key)
    }

    fn set_client_path(&mut self, path: &str) -> Result<AppSnapshot, AppError> {
        WindowsPlatformService::set_client_path(self, path)
    }

    fn open_client(&mut self) -> Result<AppSnapshot, AppError> {
        WindowsPlatformService::open_client(self)
    }
}

fn wait_for_account(database_path: &Path, expected: &AccountKey) -> Result<(), AppError> {
    let started = Instant::now();
    let mut last_error = None;
    let mut matching_samples = 0_u8;
    while started.elapsed() < ACCOUNT_VERIFY_TIMEOUT {
        if !is_main_client_running()? {
            return Err(AppError::Transaction(
                "战网客户端在确认账号前已退出，切换未完成".into(),
            ));
        }
        match read_account_catalog_for_probe(database_path) {
            Ok(catalog) if catalog.current_account_key.as_ref() == Some(expected) => {
                matching_samples += 1;
                if matching_samples >= 2 {
                    return Ok(());
                }
                last_error = None;
            }
            Ok(_) => {
                matching_samples = 0;
                last_error = None;
            }
            Err(error) => {
                matching_samples = 0;
                last_error = Some(error.to_string());
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    let detail = last_error
        .map(|error| format!("最后一次读取失败：{error}"))
        .unwrap_or_else(|| "战网没有确认目标账号，登录状态可能已经失效".into());
    Err(AppError::Transaction(format!(
        "等待目标账号登录超时；{detail}"
    )))
}

fn wait_for_stable_directory(directory: &Path) -> Result<(), AppError> {
    if !directory.is_dir() {
        return Err(AppError::Snapshot(
            "未找到 Battle.net 配置目录，请先登录一次战网客户端".into(),
        ));
    }
    wait_for_stable(
        directory_fingerprint(directory)?,
        Duration::from_secs(6),
        Duration::from_millis(250),
        || directory_fingerprint(directory),
        AppError::Snapshot("战网配置仍在变化，请稍后重试".into()),
    )
}

fn directory_fingerprint(directory: &Path) -> Result<Vec<(String, u64, u128)>, AppError> {
    let mut fingerprint = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|error| AppError::Snapshot(format!("无法读取战网配置目录：{error}")))?
    {
        let entry = entry
            .map_err(|error| AppError::Snapshot(format!("无法读取战网配置目录项：{error}")))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| AppError::Snapshot(format!("无法读取战网配置文件属性：{error}")))?;
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let modified = metadata
            .modified()
            .unwrap_or(SystemTime::UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        fingerprint.push((name, metadata.len(), modified));
    }
    fingerprint.sort();
    Ok(fingerprint)
}

fn combine_errors(primary: AppError, secondary: AppError) -> AppError {
    AppError::Message(format!(
        "{}；{}",
        primary.nested_message(),
        secondary.nested_message()
    ))
}

fn restart_after_preparation_error(
    was_running: bool,
    client_path: &Path,
    error: AppError,
) -> AppError {
    if !was_running {
        return error;
    }
    match launch(client_path) {
        Ok(()) => error,
        Err(restart_error) => combine_errors(error, restart_error),
    }
}

fn validate_login_identity(intent: &LoginIntent, current: &AccountKey) -> Result<(), AppError> {
    if let Some(region) = intent.requested_region()
        && !region.matches_environment(&current.environment)
    {
        return Err(AppError::Login(
            "登录结果与目标账号不一致，请确认登录的是所选账号。".into(),
        ));
    }
    let LoginIntent::Reauthenticate { account_key } = intent;
    if login_evidence_ready(true, Some(current), account_key) {
        Ok(())
    } else {
        Err(AppError::Login(
            "登录结果与目标账号不一致，请确认登录的是所选账号。".into(),
        ))
    }
}

fn classify_login_evidence(
    completed_marker: bool,
    fresh_authentication: bool,
    current: Option<AccountKey>,
    intent: &LoginIntent,
) -> LoginEvidence {
    if !completed_marker || !fresh_authentication {
        return LoginEvidence::Pending;
    }
    let Some(current) = current else {
        return LoginEvidence::Pending;
    };
    if validate_login_identity(intent, &current).is_ok() {
        LoginEvidence::Ready(current)
    } else {
        LoginEvidence::WrongAccount
    }
}

fn resolve_login_account(
    intent: &LoginIntent,
    current_account: Option<AccountKey>,
) -> Result<AccountKey, AppError> {
    let current = current_account
        .ok_or_else(|| AppError::Login("尚未检测到已登录账号，请返回战网完成登录".into()))?;
    validate_login_identity(intent, &current)?;
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::{
        LoginEvidence, classify_login_evidence, resolve_login_account, validate_login_identity,
    };
    use crate::contracts::{AccountKey, LoginIntent};

    fn key(environment: &str, account_id: &str) -> AccountKey {
        AccountKey {
            environment: environment.into(),
            account_id: account_id.into(),
        }
    }

    #[test]
    fn reauthentication_must_match_the_requested_environment_and_account() {
        let requested = key("kr.actual.battle.net", "1");
        let intent = LoginIntent::Reauthenticate {
            account_key: requested.clone(),
        };
        assert!(validate_login_identity(&intent, &requested).is_ok());
        assert!(validate_login_identity(&intent, &key("kr.actual.battle.net", "2")).is_err());
        assert!(validate_login_identity(&intent, &key("us.actual.battle.net", "1")).is_err());
    }

    #[test]
    fn unfinished_login_without_an_active_account_is_rejected() {
        assert!(
            resolve_login_account(
                &LoginIntent::Reauthenticate {
                    account_key: key("cn.actual.battlenet.com.cn", "1"),
                },
                None
            )
            .is_err()
        );
    }

    #[test]
    fn completed_reauthentication_resolves_the_requested_account() {
        let current = key("cn.actual.battlenet.com.cn", "2");
        assert_eq!(
            resolve_login_account(
                &LoginIntent::Reauthenticate {
                    account_key: current.clone(),
                },
                Some(current.clone())
            )
            .unwrap(),
            current
        );
    }

    #[test]
    fn login_evidence_requires_fresh_authentication_marker_and_exact_target() {
        let target = key("cn.actual.battlenet.com.cn", "2");
        let intent = LoginIntent::Reauthenticate {
            account_key: target.clone(),
        };
        assert!(matches!(
            classify_login_evidence(true, false, Some(target.clone()), &intent),
            LoginEvidence::Pending
        ));
        assert!(matches!(
            classify_login_evidence(false, true, Some(target.clone()), &intent),
            LoginEvidence::Pending
        ));
        assert!(matches!(
            classify_login_evidence(true, true, Some(target.clone()), &intent),
            LoginEvidence::Ready(account) if account == target
        ));
        assert!(matches!(
            classify_login_evidence(
                true,
                true,
                Some(key("cn.actual.battlenet.com.cn", "other")),
                &intent
            ),
            LoginEvidence::WrongAccount
        ));
    }
}
