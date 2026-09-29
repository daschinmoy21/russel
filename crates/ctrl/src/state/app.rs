//! Core application state types and deploy-tracking primitives.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use russel_core::api::{ServiceStatus, VmState};
use russel_core::config::RuntimeKind;
use tokio::process::Child;
use tokio::sync::Notify;

/// Outcome of attempting to claim a service for a lifecycle operation.
#[derive(Debug)]
pub enum LifecycleClaim {
    /// Service was claimed. Process handles remain in `AppState` until the
    /// success path takes them for reaping; `prior_*` restore status on abort.
    ///
    /// `claim_generation` is the process generation after this claim; abort
    /// must supply the same value so a superseded claim cannot restore.
    Claimed {
        prior_status: ServiceStatus,
        prior_vm_state: VmState,
        claim_generation: u64,
    },
    /// No such service in state.
    NotFound,
    /// Service exists but is already in a conflicting lifecycle op.
    Busy,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub(crate) inner: Arc<Mutex<StateInner>>,
    pub(crate) deploy_count: Arc<AtomicUsize>,
    pub(crate) deploy_notify: Arc<Notify>,
}

/// Guard that decrements the in-flight deploy counter on drop.
///
/// Move into a spawned deploy task so the counter is decremented when the
/// task completes (success, error, or panic). If the spawn itself fails,
/// the guard drops in the caller, still decrementing correctly.
#[derive(Debug)]
pub struct DeployGuard {
    state: AppState,
}

impl Drop for DeployGuard {
    fn drop(&mut self) {
        let prev = self.state.deploy_count.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            self.state.deploy_notify.notify_waiters();
        }
    }
}

#[derive(Debug)]
pub(crate) struct StateInner {
    pub(crate) services: HashMap<String, ServiceState>,
}

#[derive(Debug)]
pub(crate) struct ServiceState {
    pub(crate) status: ServiceStatus,
    pub(crate) vm_state: VmState,
    pub(crate) logs: String,
    pub(crate) started_at: Instant,
    pub(crate) flake_path: Option<std::path::PathBuf>,
    pub(crate) vm_pid: Option<u32>,
    pub(crate) vm_process: Option<Child>,
    pub(crate) container_id: Option<String>,
    pub(crate) runtime: Option<RuntimeKind>,
    pub(crate) host_port: Option<u16>,
    pub(crate) guest_port: Option<u16>,
    /// Auxiliary child processes (socat forwarders, etc.) that must stay alive.
    pub(crate) aux_processes: Vec<Child>,
    /// Prior state captured when mark_building is called, for restoring the
    /// previous deployment if the build fails before take_processes.
    pub(crate) prebuild_status: Option<ServiceStatus>,
    pub(crate) prebuild_vm_state: Option<VmState>,
    /// Bumped when process ownership changes so the matching supervisor exits.
    pub(crate) process_generation: u64,
    /// Relaunches under `restart = "unless-stopped"` since the last deploy.
    pub(crate) restarts: u32,
    /// Consecutive relaunches that did not stay up; drives crash-loop backoff.
    pub(crate) restart_streak: u32,
}

impl Default for ServiceState {
    fn default() -> Self {
        Self {
            status: ServiceStatus::Idle,
            vm_state: VmState::None,
            logs: String::new(),
            started_at: Instant::now(),
            flake_path: None,
            vm_pid: None,
            vm_process: None,
            container_id: None,
            runtime: None,
            host_port: None,
            guest_port: None,
            aux_processes: Vec::new(),
            prebuild_status: None,
            prebuild_vm_state: None,
            process_generation: 0,
            restarts: 0,
            restart_streak: 0,
        }
    }
}

/// Result of one supervisor poll tick.
#[derive(Debug)]
pub(super) enum SupervisePoll {
    /// Children still running under this generation.
    Running,
    /// Ownership moved or service gone — supervisor should exit quietly.
    Stopped,
    /// A supervised child exited or could not be polled.
    Exited(String),
}

/// Which liveness supervisor fits the workload a service currently owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Supervision {
    /// Owned `Child` handles from a cold boot: poll `try_wait`.
    Process,
    /// Podman container: poll `podman inspect` by id.
    Container(String),
    /// Adopted microVM with a known PID and no `Child`: poll `/proc`.
    Pid,
}

impl Supervision {
    /// `None` when the service owns nothing a supervisor could watch.
    pub(crate) fn for_service(s: &ServiceState) -> Option<Self> {
        if s.vm_process.is_some() {
            return Some(Self::Process);
        }
        if s.runtime == Some(RuntimeKind::Container) {
            return s.container_id.clone().map(Self::Container);
        }
        s.vm_pid.map(|_| Self::Pid)
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StateInner {
                services: HashMap::new(),
            })),
            deploy_count: Arc::new(AtomicUsize::new(0)),
            deploy_notify: Arc::new(Notify::new()),
        }
    }
}

impl AppState {
    pub(crate) fn lock_inner(&self) -> std::sync::MutexGuard<'_, StateInner> {
        self.inner.lock().unwrap_or_else(|e| {
            tracing::warn!("state lock is poisoned — recovering prior state");
            e.into_inner()
        })
    }

    pub fn list_services(&self) -> Vec<String> {
        let inner = self.lock_inner();
        let mut ids: Vec<String> = inner.services.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Increment the in-flight deploy counter and return a guard.
    ///
    /// The guard must be moved into the spawned deploy task so the counter
    /// is decremented when the task completes. Call this BEFORE spawning to
    /// ensure the counter is accurate even if the spawn is slow.
    pub fn begin_deploy(&self) -> DeployGuard {
        self.deploy_count.fetch_add(1, Ordering::SeqCst);
        DeployGuard {
            state: self.clone(),
        }
    }

    /// Wait until all in-flight deploy tasks have completed.
    ///
    /// Called during shutdown after `axum::serve` returns, to ensure no
    /// deploy task is still creating VMs when we detach processes.
    pub async fn wait_for_deploys(&self) {
        loop {
            // Register the Notified future BEFORE checking the count so a
            // notification that fires between the count check and the await
            // is not lost (F-10 lost-notification race).
            let notified = self.deploy_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.deploy_count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.as_mut().await;
        }
    }
}
