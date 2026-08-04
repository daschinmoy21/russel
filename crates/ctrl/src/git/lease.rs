use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{LazyLock, Mutex},
};

#[derive(Debug, Default)]
pub(super) struct CheckoutState {
    pub(super) leases: usize,
    pub(super) deleting: bool,
}

static ACTIVE_CHECKOUTS: LazyLock<Mutex<HashMap<PathBuf, CheckoutState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn active_checkouts() -> std::sync::MutexGuard<'static, HashMap<PathBuf, CheckoutState>>
{
    ACTIVE_CHECKOUTS.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("ACTIVE_CHECKOUTS lock poisoned — recovering");
        poisoned.into_inner()
    })
}

/// Lease for a checkout that is still being read by a deployment.
#[derive(Debug)]
pub struct CheckoutLease {
    path: Option<PathBuf>,
}

impl CheckoutLease {
    pub(super) fn inactive() -> Self {
        Self { path: None }
    }

    pub(super) fn active(path: PathBuf) -> Self {
        loop {
            let mut active = active_checkouts();
            let deleting = active.get(&path).is_some_and(|state| state.deleting);
            if deleting {
                drop(active);
                std::thread::yield_now();
                continue;
            }
            active.entry(path.clone()).or_default().leases += 1;
            return Self { path: Some(path) };
        }
    }
}

impl Clone for CheckoutLease {
    fn clone(&self) -> Self {
        match &self.path {
            Some(path) => Self::active(path.clone()),
            None => Self::inactive(),
        }
    }
}

impl Drop for CheckoutLease {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let mut active = active_checkouts();
        if let Some(state) = active.get_mut(&path) {
            debug_assert!(state.leases > 0);
            state.leases = state.leases.saturating_sub(1);
            if state.leases == 0 && !state.deleting {
                active.remove(&path);
            }
        }
    }
}
