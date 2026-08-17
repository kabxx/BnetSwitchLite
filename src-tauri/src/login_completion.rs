use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU8, Ordering},
};

use crate::contracts::LoginCancellationStatus;
use crate::error::AppError;

const STARTING: u8 = 0;
const ACTIVE: u8 = 1;
const CANCELLATION_REQUESTED: u8 = 2;
const ROLLING_BACK: u8 = 3;
const COMMITTING: u8 = 4;
const FINISHED: u8 = 5;

#[derive(Clone)]
pub(crate) struct LoginCompletionToken {
    session_id: Arc<str>,
    state: Arc<AtomicU8>,
}

impl LoginCompletionToken {
    pub(crate) fn activate(&self) -> Result<(), AppError> {
        match self
            .state
            .compare_exchange(STARTING, ACTIVE, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(()),
            Err(CANCELLATION_REQUESTED) => Ok(()),
            Err(_) => Err(AppError::StateUnavailable),
        }
    }

    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(crate) fn cancellation_requested(&self) -> bool {
        self.state.load(Ordering::Acquire) == CANCELLATION_REQUESTED
    }

    pub(crate) fn begin_rollback(&self) -> bool {
        self.state
            .compare_exchange(
                CANCELLATION_REQUESTED,
                ROLLING_BACK,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn resolve_precommit_error(&self) -> bool {
        loop {
            let state = self.state.load(Ordering::Acquire);
            match state {
                STARTING | ACTIVE => {
                    if self
                        .state
                        .compare_exchange(state, FINISHED, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return false;
                    }
                }
                CANCELLATION_REQUESTED => return self.begin_rollback(),
                ROLLING_BACK | COMMITTING | FINISHED => return false,
                _ => return false,
            }
        }
    }

    pub(crate) fn begin_failure_rollback(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, ROLLING_BACK, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn begin_commit(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn was_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == ROLLING_BACK
    }

    fn finish(&self) {
        self.state.store(FINISHED, Ordering::Release);
    }
}

struct ActiveCompletion {
    session_id: Arc<str>,
    state: Arc<AtomicU8>,
}

#[derive(Default)]
pub(crate) struct LoginCompletionRegistry {
    active: Mutex<Option<ActiveCompletion>>,
}

impl LoginCompletionRegistry {
    pub(crate) fn begin(&self, session_id: String) -> Result<LoginCompletionToken, AppError> {
        let mut active = self.active.lock().map_err(|_| AppError::StateUnavailable)?;
        if active.is_some() {
            return Err(AppError::OperationBusy);
        }

        let session_id: Arc<str> = session_id.into();
        let state = Arc::new(AtomicU8::new(STARTING));
        *active = Some(ActiveCompletion {
            session_id: Arc::clone(&session_id),
            state: Arc::clone(&state),
        });
        Ok(LoginCompletionToken { session_id, state })
    }

    pub(crate) fn request_cancellation(
        &self,
        session_id: &str,
    ) -> Result<LoginCancellationStatus, AppError> {
        let active = self.active.lock().map_err(|_| AppError::StateUnavailable)?;
        let Some(active) = active.as_ref() else {
            return Ok(LoginCancellationStatus::Starting);
        };
        if &*active.session_id != session_id {
            return Ok(LoginCancellationStatus::TooLate);
        }

        loop {
            match active.state.load(Ordering::Acquire) {
                current @ (STARTING | ACTIVE) => {
                    if active
                        .state
                        .compare_exchange(
                            current,
                            CANCELLATION_REQUESTED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Ok(LoginCancellationStatus::Accepted);
                    }
                }
                CANCELLATION_REQUESTED | ROLLING_BACK => {
                    return Ok(LoginCancellationStatus::Accepted);
                }
                COMMITTING | FINISHED => return Ok(LoginCancellationStatus::TooLate),
                _ => return Ok(LoginCancellationStatus::TooLate),
            }
        }
    }

    pub(crate) fn finish(&self, token: &LoginCompletionToken) -> Result<(), AppError> {
        token.finish();
        let mut active = self.active.lock().map_err(|_| AppError::StateUnavailable)?;
        if active.as_ref().is_some_and(|active| {
            active.session_id == token.session_id && Arc::ptr_eq(&active.state, &token.state)
        }) {
            *active = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::LoginCompletionRegistry;

    #[test]
    fn starting_cancellation_is_accepted_and_prevents_commit() {
        let registry = LoginCompletionRegistry::default();
        let token = registry.begin("login-1".into()).unwrap();

        assert!(matches!(
            registry.request_cancellation("login-1").unwrap(),
            super::LoginCancellationStatus::Accepted
        ));
        token.activate().unwrap();
        assert!(token.cancellation_requested());
        assert!(!token.begin_commit());
        assert!(token.begin_rollback());
        assert!(token.was_cancelled());
        registry.finish(&token).unwrap();
    }

    #[test]
    fn commit_point_rejects_late_cancellation() {
        let registry = LoginCompletionRegistry::default();
        let token = registry.begin("login-1".into()).unwrap();

        token.activate().unwrap();
        assert!(token.begin_commit());
        assert!(matches!(
            registry.request_cancellation("login-1").unwrap(),
            super::LoginCancellationStatus::TooLate
        ));
        registry.finish(&token).unwrap();
    }

    #[test]
    fn cancellation_is_scoped_to_the_active_session() {
        let registry = LoginCompletionRegistry::default();
        let token = registry.begin("login-1".into()).unwrap();

        token.activate().unwrap();
        assert!(matches!(
            registry.request_cancellation("login-2").unwrap(),
            super::LoginCancellationStatus::TooLate
        ));
        assert!(!token.cancellation_requested());
        registry.finish(&token).unwrap();
    }

    #[test]
    fn cancellation_waits_for_completion_registration() {
        let registry = LoginCompletionRegistry::default();

        assert!(matches!(
            registry.request_cancellation("login-1").unwrap(),
            super::LoginCancellationStatus::Starting
        ));
    }

    #[test]
    fn cancellation_wins_a_precommit_error_race() {
        let registry = LoginCompletionRegistry::default();
        let token = registry.begin("login-1".into()).unwrap();
        token.activate().unwrap();
        assert!(matches!(
            registry.request_cancellation("login-1").unwrap(),
            super::LoginCancellationStatus::Accepted
        ));

        assert!(token.resolve_precommit_error());
        assert!(token.was_cancelled());
        registry.finish(&token).unwrap();
    }

    #[test]
    fn precommit_error_rejects_a_late_cancellation() {
        let registry = LoginCompletionRegistry::default();
        let token = registry.begin("login-1".into()).unwrap();
        token.activate().unwrap();

        assert!(!token.resolve_precommit_error());
        assert!(matches!(
            registry.request_cancellation("login-1").unwrap(),
            super::LoginCancellationStatus::TooLate
        ));
        registry.finish(&token).unwrap();
    }

    #[test]
    fn operation_failure_claims_the_precommit_rollback_once() {
        let registry = LoginCompletionRegistry::default();
        let token = registry.begin("login-1".into()).unwrap();
        token.activate().unwrap();

        assert!(token.begin_failure_rollback());
        assert!(!token.begin_failure_rollback());
        assert!(matches!(
            registry.request_cancellation("login-1").unwrap(),
            super::LoginCancellationStatus::Accepted
        ));
        registry.finish(&token).unwrap();
    }
}
