//! Service-state side of microVM restart-on-exit (#467). The relaunch loop
//! lives in [`crate::restart`]; these transitions keep it from racing a
//! deploy, stop, or destroy of the same service.

use std::time::{Duration, Instant};

use russel_core::api::{ServiceStatus, VmState};

use super::app::{AppState, ServiceState};
use super::helpers::push_capped;

/// A run this long resets the crash-loop backoff.
pub(crate) const RESTART_STABLE_AFTER: Duration = Duration::from_secs(60);

const RESTART_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Delay before relaunch number `streak + 1` of a crash loop: 1s, 2s, 4s, … 30s.
pub(crate) fn restart_backoff(streak: u32) -> Duration {
    Duration::from_secs(1u64 << streak.min(5)).min(RESTART_BACKOFF_CAP)
}

/// Down, and no deploy in flight. A deploy sets `prebuild_status` until it
/// finishes, and an old generation that dies mid-deploy still reads `failed`.
fn restartable(s: &ServiceState) -> bool {
    matches!(s.status, ServiceStatus::Failed | ServiceStatus::Stopped)
        && s.prebuild_status.is_none()
}

impl AppState {
    /// Record a crash of a service that is down (`failed`, or `stopped` as
    /// found at ctrl start) and return the generation a relaunch must claim
    /// plus the backoff before it. `None` when the service is in any other
    /// state: a deploy, stop, or destroy already owns it.
    pub(crate) fn note_crash_for_restart(&self, service_id: &str) -> Option<(u64, Duration)> {
        let mut inner = self.lock_inner();
        let s = inner.services.get_mut(service_id)?;
        if !restartable(s) {
            return None;
        }
        if s.started_at.elapsed() >= RESTART_STABLE_AFTER {
            s.restart_streak = 0;
        }
        let delay = restart_backoff(s.restart_streak);
        s.restart_streak = s.restart_streak.saturating_add(1);
        Some((s.process_generation, delay))
    }

    /// Claim a down service for a relaunch. Fails when anything bumped its
    /// generation since [`Self::note_crash_for_restart`] (a deploy, stop, or
    /// destroy). While claimed the service reads `building`, so those
    /// operations get a conflict instead of racing the relaunch.
    pub(crate) fn claim_restart(&self, service_id: &str, generation: u64) -> bool {
        let mut inner = self.lock_inner();
        let Some(s) = inner.services.get_mut(service_id) else {
            return false;
        };
        if s.process_generation != generation || !restartable(s) {
            return false;
        }
        s.status = ServiceStatus::Building;
        s.vm_state = VmState::Pending;
        true
    }

    /// A claimed relaunch failed: back to `failed`, with the error in the logs.
    pub(crate) fn fail_restart(&self, service_id: &str, error: &str) {
        let mut inner = self.lock_inner();
        let Some(s) = inner.services.get_mut(service_id) else {
            return;
        };
        if s.status != ServiceStatus::Building {
            return;
        }
        s.status = ServiceStatus::Failed;
        s.vm_state = VmState::Failed;
        s.vm_pid = None;
        s.process_generation = s.process_generation.wrapping_add(1);
        // A failed relaunch never ran, so it must not count as a stable run.
        s.started_at = Instant::now();
        push_capped(&mut s.logs, &format!("RESTART FAILED: {error}\n"));
    }

    /// Count a successful relaunch.
    pub(crate) fn count_restart(&self, service_id: &str) {
        let mut inner = self.lock_inner();
        if let Some(s) = inner.services.get_mut(service_id) {
            s.restarts = s.restarts.saturating_add(1);
            push_capped(&mut s.logs, "RESTARTED (restart = unless-stopped)\n");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn failed_service(state: &AppState, id: &str) {
        let mut inner = state.lock_inner();
        let s = inner.services.entry(id.to_string()).or_default();
        s.status = ServiceStatus::Failed;
        s.vm_state = VmState::Failed;
    }

    #[test]
    fn backoff_doubles_to_a_cap() {
        let secs: Vec<u64> = (0..8).map(|n| restart_backoff(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn crash_loop_grows_the_backoff() {
        let state = AppState::default();
        failed_service(&state, "svc");
        let (_, first) = state.note_crash_for_restart("svc").unwrap();
        let (_, second) = state.note_crash_for_restart("svc").unwrap();
        assert_eq!((first.as_secs(), second.as_secs()), (1, 2));
    }

    #[test]
    fn claim_fails_after_a_stop_or_deploy_bumps_the_generation() {
        let state = AppState::default();
        failed_service(&state, "svc");
        let (generation, _) = state.note_crash_for_restart("svc").unwrap();
        state
            .lock_inner()
            .services
            .get_mut("svc")
            .unwrap()
            .process_generation += 1;
        assert!(!state.claim_restart("svc", generation));
    }

    #[test]
    fn no_restart_while_a_deploy_is_in_flight() {
        let state = AppState::default();
        failed_service(&state, "svc");
        state.mark_building("svc").unwrap();
        // The old generation dies mid-build and the exit handler marks it failed.
        failed_service(&state, "svc");
        assert!(state.note_crash_for_restart("svc").is_none());
    }

    #[test]
    fn claim_blocks_other_operations_and_failure_releases_it() {
        let state = AppState::default();
        failed_service(&state, "svc");
        let (generation, _) = state.note_crash_for_restart("svc").unwrap();
        assert!(state.claim_restart("svc", generation));
        assert!(state.mark_building("svc").is_err(), "deploy must wait");

        state.fail_restart("svc", "boom");
        let status = state.status("svc").unwrap();
        assert_eq!(status.status, "failed");
        assert!(!state.claim_restart("svc", generation), "stale claim");
        assert!(state.note_crash_for_restart("svc").is_some());
    }
}
