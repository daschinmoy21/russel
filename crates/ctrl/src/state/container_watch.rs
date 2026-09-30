//! Exit watcher for deployed containers.
//!
//! ctrl owns a microVM's cloud-hypervisor child and sees it exit at once. A
//! container's process belongs to Podman, so ctrl polls `podman inspect`.
//! A container that exits, or that Podman's restart policy relaunched
//! (`RestartCount` went up; Podman restarts at once, so the container
//! rarely reads `exited`), reads `failed` within about a second while the
//! deploy is fresh and within [`WatchTiming::slow`] after that. Once Podman
//! has restarted it and it stays up for [`WatchTiming::stable_after`], it
//! reads `deployed` again with the restarts counted, the way a microVM
//! relaunched by ctrl does.

use std::future::Future;
use std::time::{Duration, Instant};

use russel_core::api::{ServiceStatus, VmState};

use super::app::AppState;
use super::helpers::push_capped;
use crate::container::Observed;

/// Poll cadence and patience of the exit watcher.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WatchTiming {
    /// Poll interval while a deploy or recovery is fresh, when an app that
    /// listens and then crashes (#493) is most likely to die.
    pub fast: Duration,
    /// How long after deploy or recovery to keep polling at `fast`.
    pub fast_for: Duration,
    /// Poll interval after that. One inspect costs ~40 ms of CPU.
    pub slow: Duration,
    /// A restarted container must stay up this long to read `deployed` again.
    pub stable_after: Duration,
    /// A failed container that stays down this long is not coming back
    /// (no restart policy, or stopped outside Russel): stop watching.
    pub give_up_after: Duration,
}

pub(crate) const WATCH_TIMING: WatchTiming = WatchTiming {
    fast: Duration::from_secs(1),
    fast_for: Duration::from_secs(60),
    slow: Duration::from_secs(5),
    stable_after: Duration::from_secs(10),
    give_up_after: Duration::from_secs(30),
};

/// What the watcher's generation of a service reads now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchPhase {
    Deployed,
    Failed,
    /// A redeploy is building; the old container stays in place. The deploy
    /// either bumps the generation or leaves the service as it was.
    Building,
    /// Stopped, destroyed, redeployed, or gone: this watcher is done.
    Lost,
}

impl AppState {
    /// Watch a deployed container until its generation ends.
    ///
    /// `baseline` is the container's `RestartCount` when it was deployed:
    /// `Some(0)` for a container ctrl just started. `None` (adopted or
    /// restored containers) takes the first observation.
    pub(super) fn spawn_container_supervisor(
        &self,
        service_id: String,
        container_id: String,
        generation: u64,
        baseline: Option<u32>,
    ) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            state
                .watch_container(
                    &service_id,
                    &container_id,
                    generation,
                    baseline,
                    || crate::container::observe(&container_id),
                    WATCH_TIMING,
                )
                .await;
        });
    }

    async fn watch_container<O, OF>(
        &self,
        service_id: &str,
        container_id: &str,
        generation: u64,
        mut baseline: Option<u32>,
        mut observe: O,
        timing: WatchTiming,
    ) where
        O: FnMut() -> OF,
        OF: Future<Output = Observed>,
    {
        let mut seen = baseline;
        let mut fast_until = Instant::now() + timing.fast_for;
        // Set when Podman relaunched the container after it failed; the
        // service may read `deployed` again once it stays up.
        let mut restarted_at: Option<Instant> = None;
        let mut down_since: Option<Instant> = None;
        let mut gone_polls = 0u32;
        loop {
            let poll = if Instant::now() < fast_until {
                timing.fast
            } else {
                timing.slow
            };
            tokio::time::sleep(poll).await;

            let phase = self.container_watch_phase(service_id, generation);
            match phase {
                WatchPhase::Lost => return,
                WatchPhase::Building => continue,
                WatchPhase::Deployed | WatchPhase::Failed => {}
            }
            let s = match observe().await {
                Observed::Unknown => continue,
                Observed::Gone => {
                    // Two in a row, so a transient Podman answer cannot fail it.
                    gone_polls += 1;
                    if gone_polls < 2 {
                        continue;
                    }
                    if phase == WatchPhase::Deployed {
                        tracing::warn!(service_id, container_id, "container no longer exists");
                        self.mark_failed_if_generation(
                            service_id,
                            generation,
                            format!("container {container_id} no longer exists"),
                        );
                    }
                    return;
                }
                Observed::State(s) => s,
            };
            gone_polls = 0;
            let base = *baseline.get_or_insert(s.restarts);
            let restarted = seen.is_some_and(|n| s.restarts > n);
            seen = Some(s.restarts);
            if restarted {
                self.set_container_restarts(
                    service_id,
                    generation,
                    s.restarts.saturating_sub(base),
                );
            }

            match phase {
                WatchPhase::Deployed => {
                    if !restarted && !s.has_exited() {
                        continue;
                    }
                    let reason = if s.has_exited() {
                        format!(
                            "container {container_id} exited ({}, exit code {})",
                            s.status, s.exit_code
                        )
                    } else {
                        format!(
                            "container {container_id} exited and Podman restarted it \
                             ({} restart(s) since deploy)",
                            s.restarts.saturating_sub(base)
                        )
                    };
                    tracing::warn!(service_id, generation, reason = %reason, "container died");
                    if self.mark_failed_if_generation(service_id, generation, reason) {
                        restarted_at = restarted.then(Instant::now);
                        down_since = None;
                    }
                }
                WatchPhase::Failed => {
                    if s.has_exited() {
                        let since = *down_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= timing.give_up_after {
                            tracing::info!(
                                service_id,
                                container_id,
                                "failed container stayed down; no longer watching it"
                            );
                            return;
                        }
                        continue;
                    }
                    down_since = None;
                    if restarted {
                        // A crash loop keeps pushing this back.
                        restarted_at = Some(Instant::now());
                        continue;
                    }
                    let Some(at) = restarted_at else {
                        // Failed for another reason (health probes) and not
                        // restarted since: nothing new to report.
                        continue;
                    };
                    if s.status == "running"
                        && at.elapsed() >= timing.stable_after
                        && self.recover_container_if_generation(service_id, generation)
                    {
                        tracing::info!(service_id, container_id, "restarted container is up");
                        restarted_at = None;
                        fast_until = Instant::now() + timing.fast_for;
                    }
                }
                WatchPhase::Building | WatchPhase::Lost => {}
            }
        }
    }

    fn container_watch_phase(&self, service_id: &str, generation: u64) -> WatchPhase {
        let inner = self.lock_inner();
        let Some(s) = inner.services.get(service_id) else {
            return WatchPhase::Lost;
        };
        if s.process_generation != generation {
            return WatchPhase::Lost;
        }
        match s.status {
            ServiceStatus::Deployed if s.vm_state == VmState::Running => WatchPhase::Deployed,
            ServiceStatus::Failed => WatchPhase::Failed,
            ServiceStatus::Building => WatchPhase::Building,
            _ => WatchPhase::Lost,
        }
    }

    fn set_container_restarts(&self, service_id: &str, generation: u64, restarts: u32) {
        let mut inner = self.lock_inner();
        if let Some(s) = inner.services.get_mut(service_id)
            && s.process_generation == generation
        {
            s.restarts = restarts;
        }
    }

    /// A container that failed and that Podman restarted stayed up: back to
    /// `deployed`. Only for the watcher's own generation, and only while the
    /// service still reads `failed`.
    fn recover_container_if_generation(&self, service_id: &str, generation: u64) -> bool {
        {
            let mut inner = self.lock_inner();
            let Some(s) = inner.services.get_mut(service_id) else {
                return false;
            };
            if s.process_generation != generation
                || s.status != ServiceStatus::Failed
                || s.container_id.is_none()
            {
                return false;
            }
            s.status = ServiceStatus::Deployed;
            s.vm_state = VmState::Running;
            s.started_at = Instant::now();
            push_capped(
                &mut s.logs,
                "RESTARTED by Podman (restart = unless-stopped); container is up\n",
            );
        }
        self.persist_catalog();
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::container::ContainerState;

    const TIMING: WatchTiming = WatchTiming {
        fast: Duration::from_millis(10),
        fast_for: Duration::from_secs(60),
        slow: Duration::from_millis(10),
        stable_after: Duration::from_millis(100),
        give_up_after: Duration::from_millis(100),
    };

    fn deployed(state: &AppState, id: &str) -> u64 {
        let mut inner = state.lock_inner();
        let s = inner.services.entry(id.to_string()).or_default();
        s.status = ServiceStatus::Deployed;
        s.vm_state = VmState::Running;
        s.container_id = Some("c1".into());
        s.process_generation += 1;
        s.process_generation
    }

    fn st(line: &str) -> Observed {
        Observed::State(ContainerState::parse(line).unwrap())
    }

    /// Scripted `podman inspect`: each poll takes the next answer, the last repeats.
    fn script(answers: Vec<Observed>) -> impl FnMut() -> std::future::Ready<Observed> {
        let answers = Arc::new(Mutex::new(answers));
        move || {
            let mut a = answers.lock().unwrap();
            let next = if a.len() > 1 {
                a.remove(0)
            } else {
                a[0].clone()
            };
            std::future::ready(next)
        }
    }

    fn status(state: &AppState, id: &str) -> (String, Option<u32>) {
        let s = state.status(id).unwrap();
        (s.status, s.restarts)
    }

    async fn wait_for(state: &AppState, id: &str, want: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.status(id).unwrap().status != want {
            assert!(Instant::now() < deadline, "service never read {want}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn exit_after_deploy_fails_the_service() {
        let state = AppState::default();
        let g = deployed(&state, "svc");
        let answers = script(vec![st("running 0 0"), st("exited 1 0")]);
        tokio::time::timeout(
            Duration::from_secs(5),
            state.watch_container("svc", "c1", g, Some(0), answers, TIMING),
        )
        .await
        .expect("watcher gives up on a container that stays down");
        assert_eq!(status(&state, "svc"), ("failed".into(), None));
        assert!(
            state.lock_inner().services["svc"]
                .logs
                .contains("exit code 1")
        );
    }

    #[tokio::test]
    async fn podman_restart_fails_then_recovers_with_restarts_counted() {
        let state = AppState::default();
        let g = deployed(&state, "svc");
        // #493: the app crashed right after answering; Podman relaunched it.
        let answers = script(vec![st("running 0 0"), st("running 0 1")]);
        let watcher = {
            let state = state.clone();
            tokio::spawn(async move {
                state
                    .watch_container("svc", "c1", g, Some(0), answers, TIMING)
                    .await
            })
        };
        wait_for(&state, "svc", "failed").await;
        assert_eq!(status(&state, "svc").1, Some(1));
        wait_for(&state, "svc", "deployed").await;
        assert_eq!(status(&state, "svc"), ("deployed".into(), Some(1)));
        watcher.abort();
    }

    #[tokio::test]
    async fn crash_loop_stays_failed() {
        let state = AppState::default();
        let g = deployed(&state, "svc");
        let n = Arc::new(Mutex::new(0u32));
        let looping = {
            let n = n.clone();
            move || {
                let mut n = n.lock().unwrap();
                *n += 1;
                std::future::ready(st(&format!("running 0 {}", *n)))
            }
        };
        let watcher = {
            let state = state.clone();
            tokio::spawn(async move {
                state
                    .watch_container("svc", "c1", g, Some(0), looping, TIMING)
                    .await
            })
        };
        wait_for(&state, "svc", "failed").await;
        tokio::time::sleep(TIMING.stable_after * 3).await;
        let (status, restarts) = status(&state, "svc");
        assert_eq!(status, "failed");
        assert!(restarts.unwrap() > 3, "{restarts:?}");
        watcher.abort();
    }

    #[tokio::test]
    async fn stale_generation_never_marks_the_newer_deploy() {
        let state = AppState::default();
        let old = deployed(&state, "svc");
        // A redeploy took over before the old container's exit was seen.
        let _new = deployed(&state, "svc");
        tokio::time::timeout(
            Duration::from_secs(1),
            state.watch_container(
                "svc",
                "c1",
                old,
                Some(0),
                script(vec![st("exited 1 0")]),
                TIMING,
            ),
        )
        .await
        .expect("stale watcher exits");
        assert_eq!(status(&state, "svc").0, "deployed");
    }

    /// #548: promote moves the candidate's state to the stable id. The old
    /// generation's watcher (whose container is now gone) must not fail the
    /// promoted service, even when both generations had the same number.
    #[tokio::test]
    async fn rekey_ends_both_old_watchers() {
        let state = AppState::default();
        let old = deployed(&state, "svc");
        let candidate = deployed(&state, "svc_gdeadbeef");
        assert_eq!(old, candidate);
        let watch = |id: &'static str, g: u64, answers: Vec<Observed>| {
            let state = state.clone();
            tokio::spawn(async move {
                state
                    .watch_container(id, "c1", g, Some(0), script(answers), TIMING)
                    .await
            })
        };
        let old_watcher = watch("svc", old, vec![Observed::Gone]);
        let candidate_watcher = watch("svc_gdeadbeef", candidate, vec![st("running 0 0")]);

        // Cutover: drain the old generation, then promote.
        state.take_processes("svc");
        state.rekey_service("svc_gdeadbeef", "svc");
        let new = state.lock_inner().services["svc"].process_generation;
        assert!(new > old + 1, "{new}");

        for w in [old_watcher, candidate_watcher] {
            tokio::time::timeout(Duration::from_secs(1), w)
                .await
                .expect("old watcher exits after rekey")
                .unwrap();
        }
        assert_eq!(status(&state, "svc").0, "deployed");
        assert!(state.status("svc_gdeadbeef").is_none());
    }

    #[tokio::test]
    async fn stop_ends_the_watcher() {
        let state = AppState::default();
        let g = deployed(&state, "svc");
        let watcher = {
            let state = state.clone();
            tokio::spawn(async move {
                state
                    .watch_container(
                        "svc",
                        "c1",
                        g,
                        Some(0),
                        script(vec![st("running 0 0")]),
                        TIMING,
                    )
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        let claim = state.begin_lifecycle_operation("svc", ServiceStatus::Stopping, VmState::None);
        assert!(matches!(
            claim,
            crate::state::LifecycleClaim::Claimed { .. }
        ));
        tokio::time::timeout(Duration::from_secs(1), watcher)
            .await
            .expect("watcher exits after stop")
            .unwrap();
    }

    #[tokio::test]
    async fn adopted_container_takes_its_restart_count_as_baseline() {
        let state = AppState::default();
        let g = deployed(&state, "svc");
        // Restarted 4 times before ctrl came up; that is not a new crash.
        let watcher = {
            let state = state.clone();
            tokio::spawn(async move {
                state
                    .watch_container(
                        "svc",
                        "c1",
                        g,
                        None,
                        script(vec![st("running 0 4")]),
                        TIMING,
                    )
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(status(&state, "svc"), ("deployed".into(), None));
        watcher.abort();
    }

    #[tokio::test]
    async fn unknown_does_not_fail_but_a_removed_container_does() {
        let state = AppState::default();
        let g = deployed(&state, "svc");
        let answers = script(vec![
            Observed::Unknown,
            Observed::Gone,
            st("running 0 0"),
            Observed::Gone,
            Observed::Gone,
        ]);
        tokio::time::timeout(
            Duration::from_secs(1),
            state.watch_container("svc", "c1", g, Some(0), answers, TIMING),
        )
        .await
        .expect("watcher ends once the container is gone");
        assert_eq!(status(&state, "svc").0, "failed");
    }

    #[tokio::test]
    async fn health_failure_is_not_undone_without_a_restart() {
        let state = AppState::default();
        let g = deployed(&state, "svc");
        state.mark_failed("svc", "health check failed".into());
        let watcher = {
            let state = state.clone();
            tokio::spawn(async move {
                state
                    .watch_container(
                        "svc",
                        "c1",
                        g,
                        Some(0),
                        script(vec![st("running 0 0")]),
                        TIMING,
                    )
                    .await
            })
        };
        tokio::time::sleep(TIMING.stable_after * 3).await;
        assert_eq!(status(&state, "svc").0, "failed");
        watcher.abort();
    }
}
