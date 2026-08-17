use std::{
    collections::HashSet,
    fs,
    path::Path,
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
    operation_lock::OperationGuard,
    paths::BattleNetPaths,
    preferences,
    process::{
        ValidatedClient, ensure_client_stopped, graceful_stop, is_client_running,
        is_main_client_running, launch, launch_for_login, wait_for_client_started,
    },
    recovery::{RecoveryContext, RecoveryEnvelope, RecoveryKind, RecoveryState, RecoveryStore},
    secure_snapshot::{SecurePayload, SnapshotCodec, SnapshotStore, apply_config, capture_current},
};

const LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const CLIENT_STOP_TIMEOUT: Duration = Duration::from_secs(25);
const CLIENT_START_TIMEOUT: Duration = Duration::from_secs(15);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const LOGIN_POLL_INTERVAL: Duration = Duration::from_millis(250);
const LOGIN_PROBE_ERROR_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct MacosPlatformService {
    store: DataStore,
    paths: BattleNetPaths,
    hidden_accounts: HashSet<HiddenAccountKey>,
    settings: SettingsDocument,
    snapshots: SnapshotStore,
    recovery: RecoveryStore,
    startup_notice: Option<String>,
}

impl MacosPlatformService {
    pub fn new() -> Result<Self, AppError> {
        let store = DataStore::open().map_err(AppError::DataStorage)?;
        let hidden_accounts = store
            .load_hidden_accounts()
            .map_err(AppError::DataStorage)?;
        let settings = store.load_settings().map_err(AppError::DataStorage)?;
        let paths = BattleNetPaths::discover()?;
        let codec = SnapshotCodec;
        let snapshots = SnapshotStore::new(store.data_directory())?;
        let recovery = RecoveryStore::new(store.data_directory(), codec)?;
        let startup_notice = recovery
            .record()?
            .filter(|record| {
                record.state == RecoveryState::Prepared
                    || (record.state == RecoveryState::AwaitingUser
                        && record.kind == RecoveryKind::Login)
            })
            .map(|_| "检测到上次未完成的账号操作，正在安全恢复原状态。".to_owned());
        Ok(Self {
            store,
            paths,
            hidden_accounts,
            settings,
            snapshots,
            recovery,
            startup_notice,
        })
    }

    pub fn prepare_for_use(&mut self, report: &impl Fn(OperationEvent)) -> Result<(), AppError> {
        let Some(record) = self.recovery.record()? else {
            return Ok(());
        };
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        match record.state {
            RecoveryState::Committed => {
                if record.kind == RecoveryKind::Login {
                    self.finalize_committed_login(&record.id)?;
                } else {
                    self.recovery.cleanup_committed(&record.id)?;
                }
                self.startup_notice = Some("已完成上次操作的安全清理。".into());
                return Ok(());
            }
            RecoveryState::Prepared | RecoveryState::AwaitingUser => {}
        }
        let client = match self.require_client() {
            Ok(client) => client,
            Err(error) => {
                self.startup_notice = Some(format!(
                    "上次操作仍需恢复，请重新选择有效的 Battle.net.app 后继续：{error}"
                ));
                return Ok(());
            }
        };
        self.recover_incomplete(&record, &client, report)
    }

    fn recover_incomplete(
        &mut self,
        record: &super::recovery::RecoveryRecord,
        client: &ValidatedClient,
        report: &impl Fn(OperationEvent),
    ) -> Result<(), AppError> {
        emit(
            report,
            OperationKind::Recovery,
            "stoppingClient",
            "正在恢复上次操作",
            "正在确认战网客户端已退出",
            22,
        );
        if is_client_running(client)? {
            graceful_stop(client, CLIENT_STOP_TIMEOUT)?;
        }
        ensure_client_stopped(client, CLIENT_STOP_TIMEOUT)?;
        emit(
            report,
            OperationKind::Recovery,
            "rollingBack",
            "正在恢复上次操作",
            "正在还原配置和认证状态",
            68,
        );
        self.recovery
            .rollback(&record.id, &|before| self.apply_payload(client, before))?;
        if record.previous_client_was_running && record.kind != RecoveryKind::Login {
            launch(client)?;
        }
        self.startup_notice = Some("已恢复上次未完成操作前的战网状态。".into());
        emit(
            report,
            OperationKind::Recovery,
            "completed",
            "恢复完成",
            "战网状态已恢复一致",
            100,
        );
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
        let (client, client_notice) = match self.resolved_client() {
            Ok(client) => (client, None),
            Err(error) => (None, Some(error.to_string())),
        };
        let running = client
            .as_ref()
            .map(is_main_client_running)
            .transpose()?
            .unwrap_or(false);
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
            platform: "macos".into(),
            data_directory: self.store.data_directory().display().to_string(),
            client: ClientSnapshot {
                status: if running {
                    ClientStatus::Running
                } else {
                    ClientStatus::Stopped
                },
                executable_path: client
                    .as_ref()
                    .map(|client| client.bundle().display().to_string())
                    .unwrap_or_default(),
                detected_automatically: self.settings.client_executable_path.is_none()
                    && client.is_some(),
            },
            accounts,
            login_session: self.recovery.login_snapshot()?,
            notice: self
                .startup_notice
                .clone()
                .or(read_notice)
                .or(client_notice),
            updated_at: now_epoch_ms(),
        })
    }

    pub fn detect_accounts(&mut self) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        self.require_client()?;
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
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        self.ensure_idle()?;
        let target = self.snapshots.validate(target_key)?;
        let client = self.require_client()?;
        let was_running = is_main_client_running(&client)?;
        emit(
            report,
            OperationKind::Switch,
            "validating",
            "正在切换账号",
            "正在校验账号快照",
            8,
        );
        if is_client_running(&client)? {
            graceful_stop(&client, CLIENT_STOP_TIMEOUT)?;
        }
        ensure_client_stopped(&client, CLIENT_STOP_TIMEOUT)?;
        wait_for_stable_config(&self.paths.config)?;
        // CachedData.db only lists remembered accounts. It cannot prove which
        // account owns the current authentication state.
        let before = capture_current(&self.paths.config, None)?;
        let record = self.recovery.prepare(
            RecoveryKind::Switch,
            was_running,
            &RecoveryEnvelope {
                before,
                context: RecoveryContext::Switch {
                    target_account: target_key.clone(),
                    previous_account: None,
                },
            },
        )?;
        emit(
            report,
            OperationKind::Switch,
            "restoring",
            "正在切换账号",
            "正在恢复配置和认证状态",
            56,
        );
        if let Err(error) = self.apply_payload(&client, &target) {
            return self.rollback_after_failure(&record.id, &client, error);
        }
        emit(
            report,
            OperationKind::Switch,
            "launchingClient",
            "正在切换账号",
            "正在启动战网客户端",
            76,
        );
        if let Err(error) =
            launch(&client).and_then(|_| wait_for_client_started(&client, CLIENT_START_TIMEOUT))
        {
            return self.rollback_after_failure(&record.id, &client, error);
        }
        emit(
            report,
            OperationKind::Switch,
            "verifying",
            "正在切换账号",
            "正在检查客户端和目标资料",
            88,
        );
        if let Err(error) = self.wait_for_target_health(&client, target_key) {
            return self.rollback_after_failure(&record.id, &client, error);
        }
        if let Some(cleanup_error) = self.recovery.commit(&record.id)? {
            self.startup_notice = Some(format!(
                "账号已切换，临时恢复文件将在下次启动时继续清理：{cleanup_error}"
            ));
        }
        emit(
            report,
            OperationKind::Switch,
            "completed",
            "账号切换完成",
            "已自动确认当前活跃账号",
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
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        self.ensure_idle()?;
        let client = self.require_client()?;
        let was_running = is_main_client_running(&client)?;
        emit(
            report,
            OperationKind::Login,
            "preparing",
            "正在准备登录",
            "正在保存当前战网配置",
            8,
        );
        if is_client_running(&client)? {
            emit(
                report,
                OperationKind::Login,
                "stoppingClient",
                "正在准备登录",
                "正在安全关闭战网客户端",
                16,
            );
            graceful_stop(&client, CLIENT_STOP_TIMEOUT)?;
        }
        ensure_client_stopped(&client, CLIENT_STOP_TIMEOUT)?;
        let before = capture_current(&self.paths.config, None)?;
        let record = self.recovery.prepare(
            RecoveryKind::Login,
            was_running,
            &RecoveryEnvelope {
                before,
                context: RecoveryContext::Login {
                    intent,
                    previous_account: None,
                },
            },
        )?;
        emit(
            report,
            OperationKind::Login,
            "clearingPointer",
            "正在准备登录",
            "正在清除当前自动登录状态",
            24,
        );
        if let Err(error) = preferences::clear_authentication_only() {
            return self.rollback_after_failure(&record.id, &client, error);
        }
        self.recovery
            .transition(&record.id, RecoveryState::AwaitingUser)?;
        if let Err(error) = launch_for_login(&client, login_region, CLIENT_START_TIMEOUT) {
            return self.rollback_after_failure(&record.id, &client, error);
        }
        emit(
            report,
            OperationKind::Login,
            "awaitingUser",
            "请在战网客户端中完成登录",
            "登录成功后将自动保存",
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
                let client = self.require_client()?;
                self.rollback_after_failure(session_id, &client, error)
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
        let record = self.require_login_record(session_id)?;
        if record.state != RecoveryState::AwaitingUser {
            return Err(AppError::Login("当前登录会话不能提交".into()));
        }
        let context = self.recovery.context(session_id)?;
        let RecoveryContext::Login {
            intent,
            previous_account: _,
        } = context.context
        else {
            unreachable!();
        };
        let client = self.require_client()?;
        let LoginIntent::Reauthenticate {
            account_key: target,
        } = &intent;
        if completion_control.begin_rollback() {
            return self.cancel_login_after_lock(session_id, report);
        }

        emit(
            report,
            OperationKind::Login,
            "awaitingLogin",
            "正在等待战网登录",
            "正在等待战网登录",
            32,
        );

        if !self.wait_for_login_evidence(&client, target, completion_control)? {
            return self.cancel_login_after_lock(session_id, report);
        }

        emit(
            report,
            OperationKind::Login,
            "stoppingClient",
            "正在保存登录结果",
            "正在关闭战网客户端",
            48,
        );
        if is_client_running(&client)? {
            graceful_stop(&client, CLIENT_STOP_TIMEOUT)?;
        }
        ensure_client_stopped(&client, CLIENT_STOP_TIMEOUT)?;
        if completion_control.begin_rollback() {
            return self.cancel_login_after_lock(session_id, report);
        }

        let candidate = (|| {
            wait_for_stable_config(&self.paths.config)?;
            let catalog = read_account_catalog(&self.paths.cached_data_db)?;
            let authenticated = preferences::capture()?.has_authentication_keys()?;
            if !login_evidence_ready(authenticated, catalog.current_account_key.as_ref(), target) {
                return Err(AppError::Login(
                    "登录结果与目标账号不一致，请确认登录的是所选账号。".into(),
                ));
            }
            if completion_control.begin_rollback() {
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
            let payload = capture_current(&self.paths.config, Some(target.clone()))?;
            self.recovery.stage_login_candidate(session_id, &payload)?;
            Ok(true)
        })();
        match candidate {
            Ok(true) => {}
            Ok(false) => return self.cancel_login_after_lock(session_id, report),
            Err(error) => return Err(error),
        }
        if !completion_control.begin_commit() {
            if completion_control.begin_rollback() {
                return self.cancel_login_after_lock(session_id, report);
            }
            return Err(AppError::StateUnavailable);
        }
        self.recovery.mark_login_committed(session_id)?;
        if let Some(cleanup_error) = self.finalize_committed_login(session_id)? {
            self.startup_notice = Some(format!(
                "账号已保存，临时恢复文件将在下次启动时继续清理：{cleanup_error}"
            ));
        }
        emit(
            report,
            OperationKind::Login,
            "launchingClient",
            "账号已保存",
            "正在启动战网客户端",
            88,
        );
        launch(&client)?;
        wait_for_client_started(&client, CLIENT_START_TIMEOUT)?;
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

    fn wait_for_login_evidence(
        &self,
        client: &ValidatedClient,
        target: &AccountKey,
        completion_control: &LoginCompletionToken,
    ) -> Result<bool, AppError> {
        let mut probe_error_started_at = None;
        loop {
            if completion_control.begin_rollback() {
                return Ok(false);
            }

            match self.login_evidence_ready(target) {
                Ok(true) => return Ok(true),
                Ok(false) => {
                    probe_error_started_at = None;
                    if !is_main_client_running(client)? {
                        return Err(AppError::Login("Battle.net 已退出，登录未完成".into()));
                    }
                }
                Err(error @ AppError::AccountDatabaseIncompatible(_)) => return Err(error),
                Err(error) => {
                    let started_at = probe_error_started_at.get_or_insert_with(Instant::now);
                    if started_at.elapsed() >= LOGIN_PROBE_ERROR_TIMEOUT {
                        return Err(error);
                    }
                }
            }
            thread::sleep(LOGIN_POLL_INTERVAL);
        }
    }

    fn login_evidence_ready(&self, target: &AccountKey) -> Result<bool, AppError> {
        let authenticated = preferences::capture()?.has_authentication_keys()?;
        if !authenticated {
            return Ok(false);
        }
        let catalog = read_account_catalog_for_probe(&self.paths.cached_data_db)?;
        Ok(login_evidence_ready(
            true,
            catalog.current_account_key.as_ref(),
            target,
        ))
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
        let record = self.require_login_record(session_id)?;
        let client = self.require_client()?;
        emit(
            report,
            OperationKind::Login,
            "stoppingClient",
            "正在取消登录",
            "正在关闭战网客户端",
            72,
        );
        if is_client_running(&client)? {
            graceful_stop(&client, CLIENT_STOP_TIMEOUT)?;
        }
        ensure_client_stopped(&client, CLIENT_STOP_TIMEOUT)?;
        emit(
            report,
            OperationKind::Login,
            "rollingBack",
            "正在取消登录",
            "正在恢复登录前状态",
            84,
        );
        self.recovery
            .rollback(session_id, &|before| self.apply_payload(&client, before))?;
        if record.previous_client_was_running {
            emit(
                report,
                OperationKind::Login,
                "launchingClient",
                "正在取消登录",
                "正在启动战网客户端",
                94,
            );
            launch(&client)?;
            wait_for_client_started(&client, CLIENT_START_TIMEOUT)?;
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

    pub fn set_client_path(&mut self, path: &str) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        let client = ValidatedClient::new(Path::new(path))?;
        self.settings = self.store.load_settings().map_err(AppError::DataStorage)?;
        self.settings.client_executable_path = Some(client.bundle().display().to_string());
        self.store
            .save_settings(&self.settings)
            .map_err(AppError::DataStorage)?;
        if let Some(record) = self.recovery.record()? {
            if record.state == RecoveryState::Prepared {
                self.recover_incomplete(&record, &client, &|_| {})?;
            }
        }
        self.snapshot()
    }

    pub fn open_client(&mut self) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        if self
            .recovery
            .record()?
            .is_some_and(|record| record.state == RecoveryState::Prepared)
        {
            return Err(AppError::Transaction(
                "请先选择有效的战网客户端并完成上次操作恢复".into(),
            ));
        }
        let client = self.require_client()?;
        if is_main_client_running(&client)? {
            return self.snapshot();
        }
        launch(&client)?;
        wait_for_client_started(&client, CLIENT_START_TIMEOUT)?;
        self.snapshot()
    }

    pub fn remove_account(&mut self, key: &AccountKey) -> Result<AppSnapshot, AppError> {
        let _guard = OperationGuard::acquire(self.store.data_directory(), LOCK_TIMEOUT)?;
        self.ensure_idle()?;
        self.snapshots.remove(key)?;
        self.hidden_accounts = self
            .store
            .load_hidden_accounts()
            .map_err(AppError::DataStorage)?;
        let mut next = self.hidden_accounts.clone();
        next.insert(HiddenAccountKey::from(key));
        self.store
            .save_hidden_accounts(&next)
            .map_err(AppError::DataStorage)?;
        self.hidden_accounts = next;
        self.snapshot()
    }

    fn unhide(&mut self, key: &AccountKey) -> Result<(), AppError> {
        self.hidden_accounts = self
            .store
            .load_hidden_accounts()
            .map_err(AppError::DataStorage)?;
        let mut next = self.hidden_accounts.clone();
        next.remove(&HiddenAccountKey::from(key));
        self.store
            .save_hidden_accounts(&next)
            .map_err(AppError::DataStorage)?;
        self.hidden_accounts = next;
        Ok(())
    }

    fn finalize_committed_login(&mut self, session_id: &str) -> Result<Option<String>, AppError> {
        let record = self.require_login_record(session_id)?;
        if record.state != RecoveryState::Committed {
            return Err(AppError::Login("登录结果尚未提交".into()));
        }
        let payload = self.recovery.login_candidate(session_id)?;
        let account = payload
            .account
            .clone()
            .ok_or_else(|| AppError::Login("登录候选快照缺少账号身份".into()))?;
        self.snapshots.save(&account, &payload)?;
        self.unhide(&account)?;
        match self.recovery.cleanup_committed(session_id) {
            Ok(()) => Ok(None),
            Err(error) => Ok(Some(error.to_string())),
        }
    }

    fn apply_payload(
        &self,
        client: &ValidatedClient,
        payload: &SecurePayload,
    ) -> Result<(), AppError> {
        ensure_client_stopped(client, CLIENT_STOP_TIMEOUT)?;
        apply_config(&self.paths.config, &payload.config)?;
        preferences::replace(&payload.preferences)
    }

    fn rollback_after_failure(
        &self,
        operation_id: &str,
        client: &ValidatedClient,
        original: AppError,
    ) -> Result<AppSnapshot, AppError> {
        let original = original.nested_message();
        if is_client_running(client)? {
            graceful_stop(client, CLIENT_STOP_TIMEOUT).map_err(|stop| {
                AppError::Transaction(format!(
                    "{original}；客户端仍在运行，恢复材料已保留：{}",
                    stop.nested_message()
                ))
            })?;
        }
        ensure_client_stopped(client, CLIENT_STOP_TIMEOUT)?;
        self.recovery
            .rollback(operation_id, &|before| self.apply_payload(client, before))
            .map_err(|rollback| {
                AppError::Transaction(format!(
                    "{original}；自动恢复未完成：{}。请勿启动战网，重新打开本工具继续恢复",
                    rollback.nested_message()
                ))
            })?;
        Err(AppError::Transaction(format!(
            "{original}；已恢复操作前的战网状态"
        )))
    }

    fn wait_for_target_health(
        &self,
        client: &ValidatedClient,
        target: &AccountKey,
    ) -> Result<(), AppError> {
        let deadline = Instant::now() + HEALTH_CHECK_TIMEOUT;
        let mut matches = 0_u8;
        while Instant::now() < deadline {
            if !is_main_client_running(client)? {
                return Err(AppError::Transaction(
                    "Battle.net 在账号确认前已退出".into(),
                ));
            }
            if self.check_target_health_probe(target).is_ok() {
                matches += 1;
                if matches >= 2 {
                    return Ok(());
                }
            } else {
                matches = 0;
            }
            thread::sleep(Duration::from_millis(500));
        }
        Err(AppError::Transaction(
            "客户端未通过目标账号自动确认，已停止等待".into(),
        ))
    }

    fn check_target_health_probe(&self, target: &AccountKey) -> Result<(), AppError> {
        let catalog = read_account_catalog_for_probe(&self.paths.cached_data_db)?;
        self.check_target_health_from_catalog(target, catalog)
    }

    fn check_target_health_from_catalog(
        &self,
        target: &AccountKey,
        catalog: AccountCatalog,
    ) -> Result<(), AppError> {
        if catalog.current_account_key.as_ref() != Some(target) {
            return Err(AppError::Transaction(
                "当前活跃账号尚未切换到目标账号".into(),
            ));
        }
        Ok(())
    }

    fn resolved_client(&self) -> Result<Option<ValidatedClient>, AppError> {
        if let Some(path) = self.settings.client_executable_path.as_deref() {
            match ValidatedClient::new(Path::new(path)) {
                Ok(client) => return Ok(Some(client)),
                Err(saved_error) => {
                    if let Some(detected) = self.paths.detect_client_bundle() {
                        return ValidatedClient::new(&detected).map(Some);
                    }
                    return Err(saved_error);
                }
            }
        }
        self.paths
            .detect_client_bundle()
            .map(|path| ValidatedClient::new(&path))
            .transpose()
    }

    fn require_client(&self) -> Result<ValidatedClient, AppError> {
        self.resolved_client()?
            .ok_or(AppError::ClientExecutableMissing)
    }

    fn ensure_idle(&mut self) -> Result<(), AppError> {
        if self.recovery.record()?.is_some() {
            return Err(AppError::Message("请先完成或取消当前账号操作".into()));
        }
        Ok(())
    }

    fn require_login_record(
        &self,
        session_id: &str,
    ) -> Result<super::recovery::RecoveryRecord, AppError> {
        let record = self
            .recovery
            .record()?
            .ok_or_else(|| AppError::Login("登录恢复记录不存在".into()))?;
        if record.id != session_id || record.kind != RecoveryKind::Login {
            return Err(AppError::Login("登录会话已失效".into()));
        }
        Ok(record)
    }
}

impl crate::platform::PlatformService for MacosPlatformService {
    fn new() -> Result<Self, AppError> {
        MacosPlatformService::new()
    }

    fn prepare_for_use(&mut self, report: &impl Fn(OperationEvent)) -> Result<(), AppError> {
        MacosPlatformService::prepare_for_use(self, report)
    }

    fn snapshot(&self) -> Result<AppSnapshot, AppError> {
        MacosPlatformService::snapshot(self)
    }

    fn detect_accounts(&mut self) -> Result<AppSnapshot, AppError> {
        MacosPlatformService::detect_accounts(self)
    }

    fn switch_account(
        &mut self,
        target: &AccountKey,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        MacosPlatformService::switch_account(self, target, report)
    }

    fn begin_login(
        &mut self,
        intent: LoginIntent,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        MacosPlatformService::begin_login(self, intent, report)
    }

    fn complete_login(
        &mut self,
        session_id: &str,
        completion: &LoginCompletionToken,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        MacosPlatformService::complete_login(self, session_id, completion, report)
    }

    fn cancel_login(
        &mut self,
        session_id: &str,
        report: &impl Fn(OperationEvent),
    ) -> Result<AppSnapshot, AppError> {
        MacosPlatformService::cancel_login(self, session_id, report)
    }

    fn remove_account(&mut self, key: &AccountKey) -> Result<AppSnapshot, AppError> {
        MacosPlatformService::remove_account(self, key)
    }

    fn set_client_path(&mut self, path: &str) -> Result<AppSnapshot, AppError> {
        MacosPlatformService::set_client_path(self, path)
    }

    fn open_client(&mut self) -> Result<AppSnapshot, AppError> {
        MacosPlatformService::open_client(self)
    }
}

fn wait_for_stable_config(path: &Path) -> Result<(), AppError> {
    if !path.is_file() {
        return Err(AppError::Snapshot(
            "未找到 Battle.net.config，请先登录一次战网客户端".into(),
        ));
    }
    wait_for_stable(
        config_fingerprint(path)?,
        Duration::from_secs(4),
        Duration::from_millis(250),
        || config_fingerprint(path),
        AppError::Snapshot("Battle.net.config 仍在变化，请稍后重试".into()),
    )
}

fn config_fingerprint(path: &Path) -> Result<(u64, u128), AppError> {
    let metadata = fs::metadata(path)
        .map_err(|error| AppError::Snapshot(format!("无法读取战网配置属性：{error}")))?;
    let modified = metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok((metadata.len(), modified))
}
