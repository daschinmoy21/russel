//! The phases of one deploy (#415), in order: plan → build → prepare_slot →
//! boot → swap_ingress → hold → cutover → record. Each phase is one function that
//! takes what the earlier phases produced; `deploy_inner` only sequences them.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use russel_core::{
    api::{DeployEvent, DeployRequest, DeployTiming, PortMapping},
    config::{
        RuntimeKind, Russelfile, resolve_ingress_host, resolve_primary_publish, validate_env_map,
    },
    volumes::{ResolvedVolume, extra_port_key, resolve_volumes, volume_roots_from_env},
};

use crate::{
    build::BuildOutput,
    container::{
        CONTAINER_READY_TIMEOUT, LOG_TAIL_LINES, ReadyOutcome, attach_managed_volumes,
        container_log_path, destroy_preserving_volumes, detach_managed_volumes, log_tail,
        not_ready_error, restore_backed_up_service_dir, validate_podman_args_for_runtime,
        watch_container,
    },
    deployments::{self, AppendSuccess, DesiredStateSnapshot},
    git::{CheckoutLease, SourceRev, redact_repo_url},
    ingress::{Backend, HostRule},
    metadata::load_metadata_from_disk,
    microvm::{KernelInfo, ready},
    network::{MicrovmNetMode, PortAllocator, TapForwarder},
};

use super::WATCH_WINDOW;
use super::pipeline::{
    DeployInnerResult, DeployOutput, DeployPipeline, DeployWorkload, DesiredExtras, ResolvedSource,
    build_desired_state, carry_history, new_generation_id, promote_generation,
    record_source_in_metadata, reject_live_listen_collision, validate_live_ingress_port,
};
use super::rollback::{
    attempt_container_rollback, attempt_microvm_rollback, cleanup_failed_deploy,
    destroy_prior_runtime, kill_and_wait_children, resolve_prior_runtime, restore_backup_dirs,
};

type Events = tokio::sync::mpsc::Sender<DeployEvent>;

/// The Russelfile turned into the deploy's inputs. Every later phase reads it.
struct Plan {
    /// The Russelfile's folder joined with `service.source`: where Nix builds
    /// (#525). The repo root for a root Russelfile with `source = "."`.
    build_path: PathBuf,
    /// Keeps the checkout alive until the deploy ends.
    _checkout: CheckoutLease,
    config: Russelfile,
    /// Commit the source builds; `None` outside git (#448).
    rev: Option<SourceRev>,
    runtime: RuntimeKind,
    /// `[service.env]` with `secret://` refs resolved: what the workload gets.
    env: HashMap<String, String>,
    ingress_host: Option<String>,
    host_rules: Vec<HostRule>,
    /// The primary publish `[ingress].port` pins, if any.
    pin_mapping: Option<PortMapping>,
    volumes: Vec<ResolvedVolume>,
    /// Replayed by rollback, health restart, and update (F-04/08/09).
    desired_state: Option<serde_json::Value>,
}

struct Built {
    output: BuildOutput,
    /// Resolved for the microVM runtime only.
    kernel: Option<KernelInfo>,
    build_ms: u128,
}

/// Where the new generation boots, and what to put back if it fails.
struct Slot {
    prior_runtime: Option<RuntimeKind>,
    /// Boot beside the live generation and swap ingress to it (zero
    /// downtime). Otherwise the live generation is stopped first (cold).
    dual_live: bool,
    generation_id: String,
    /// `{service_id}_g{generation_id}` when dual-live, else the service id.
    runtime_key: String,
    dirs: ServiceDirs,
    /// Cold path moved the live service dir to `.bak`; rollback reads it.
    has_backup: bool,
}

struct ServiceDirs {
    russel: String,
    microvms: String,
    russel_bak: String,
    microvms_bak: String,
    has_microvms: bool,
}

impl ServiceDirs {
    fn of(service_id: &str) -> Self {
        let russel = crate::paths::service_dir(service_id).display().to_string();
        let microvms = crate::paths::microvm_dir(service_id).display().to_string();
        Self {
            russel_bak: format!("{russel}.bak"),
            microvms_bak: format!("{microvms}.bak"),
            has_microvms: Path::new(&microvms).exists(),
            russel,
            microvms,
        }
    }

    async fn restore(&self) {
        restore_backup_dirs(
            &self.russel,
            &self.microvms,
            &self.russel_bak,
            &self.microvms_bak,
            self.has_microvms,
        )
        .await;
    }

    /// Best-effort cleanup of the pre-deploy backup dirs. Use remove_dir_all
    /// rather than shelling out to `rm -rf` (F-46); only a keep-id rootfs
    /// falls back to `podman unshare rm` (#464). A missing dir (already
    /// cleaned) is not an error.
    async fn remove_backups(&self) {
        if let Err(e) = crate::container::remove_tree(Path::new(&self.russel_bak)).await
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.russel_bak,
                error = %e,
                "failed to remove russel backup dir after deploy"
            );
        }
        if self.has_microvms
            && let Err(e) = std::fs::remove_dir_all(&self.microvms_bak)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.microvms_bak,
                error = %e,
                "failed to remove microvms backup dir after deploy"
            );
        }
    }
}

struct Booted {
    workload: DeployWorkload,
    create_ms: u128,
    start_ms: u128,
    network_ms: u128,
    ready_ms: u128,
}

impl DeployPipeline {
    pub(super) async fn deploy_inner(
        &self,
        service_id: &str,
        request: DeployRequest,
        source: ResolvedSource,
        started: Instant,
        tx: Events,
    ) -> anyhow::Result<DeployInnerResult> {
        let plan = plan(service_id, &request, source)?;
        // The resolve phase began with the fetch in `resolve_source`.
        let resolve_ms = started.elapsed().as_millis();
        tracing::info!(
            service_id,
            service_name = %plan.config.service.name,
            runtime = %plan.runtime,
            guest = %plan.config.service.guest,
            resolve_ms,
            "repo resolved"
        );

        let built = self.build(service_id, &plan, &tx).await?;
        let slot = self.prepare_slot(service_id, &plan, &tx).await?;

        // Armed once boot reserves ports, so dropping it releases them.
        let mut reservation = None;
        let mut booted = match self
            .boot(service_id, &plan, &built, &slot, &mut reservation, &tx)
            .await
        {
            Ok(booted) => booted,
            Err(error) => {
                return self
                    .recover_failed_boot(service_id, &plan, &slot, error, reservation)
                    .await;
            }
        };

        // The live generation's route, to point back at from `hold`.
        let previous_route = if slot.dual_live {
            self.live_route(service_id, &plan)
        } else {
            None
        };
        self.swap_ingress(service_id, &plan, &slot, &booted.workload)
            .await?;
        self.hold(service_id, &slot, &mut booted.workload, previous_route, &tx)
            .await?;
        let fixed_port = self
            .cutover(service_id, &plan, &slot, &booted.workload, &tx)
            .await;
        self.record(service_id, &request, &plan, &built, &slot, &booted.workload)
            .await;

        reservation
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("port reservation dropped before deploy completed"))?
            .disarm();

        let (fixed_port_socat, fixed_host_port) = fixed_port.unzip();
        Ok(DeployInnerResult::Success(Box::new(DeployOutput {
            store_path: built.output.store_path,
            port: booted.workload.port().clone(),
            runtime: plan.runtime,
            timing: DeployTiming {
                resolve_ms,
                build_ms: built.build_ms,
                create_ms: booted.create_ms,
                start_ms: booted.start_ms,
                network_ms: booted.network_ms,
                ready_ms: booted.ready_ms,
            },
            route_host: plan.ingress_host,
            workload: booted.workload,
            fixed_port_socat,
            fixed_host_port,
            rev: plan.rev.map(|r| r.rev),
        })))
    }

    async fn build(&self, service_id: &str, plan: &Plan, tx: &Events) -> anyhow::Result<Built> {
        let t = Instant::now();
        let microvm = plan.runtime == RuntimeKind::Microvm;
        let description = if microvm {
            "Building package + ensuring kernel/busybox"
        } else {
            "Building package"
        };
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "build".into(),
                description: description.into(),
            })
            .await;
        let output = self
            .builder
            .build(&plan.build_path, plan.config.service.package.as_deref())
            .await?;
        let kernel = if microvm {
            let kernel = self.runner.ensure_kernel().await?;
            self.runner.ensure_busybox().await?;
            Some(kernel)
        } else {
            None
        };
        let build_ms = t.elapsed().as_millis();
        match &kernel {
            Some(ki) => tracing::info!(
                service_id,
                store = %output.store_path.display(),
                kernel = %ki.path.display(),
                build_ms,
                "build complete (kernel + busybox cached)"
            ),
            None => tracing::info!(
                service_id,
                store = %output.store_path.display(),
                build_ms,
                "build complete"
            ),
        }
        Ok(Built {
            output,
            kernel,
            build_ms,
        })
    }

    /// Pick dual-live or cold and make room for the new generation.
    async fn prepare_slot(
        &self,
        service_id: &str,
        plan: &Plan,
        tx: &Events,
    ) -> anyhow::Result<Slot> {
        let prior_runtime = resolve_prior_runtime(service_id).await?;
        // Extra pinned ports cannot move while the old generation still holds
        // them, so skip dual-live and replace in place.
        let dual_live = prior_runtime.is_some() && plan.config.ports.is_empty();

        // Generation identity: when replacing a live service, boot the candidate
        // under `{service_id}_g{gen}` so the active generation keeps its TAP/port
        // until ingress.swap and cutover complete (zero-downtime path).
        let generation_id = new_generation_id();
        let runtime_key = if dual_live {
            let key = format!("{service_id}_g{generation_id}");
            russel_core::ids::validate_service_id(&key)?;
            key
        } else {
            service_id.to_string()
        };

        let dirs = ServiceDirs::of(service_id);
        let has_russel_dir = Path::new(&dirs.russel).exists();

        if dual_live {
            tracing::info!(
                service_id,
                generation_id = %generation_id,
                runtime_key = %runtime_key,
                "dual-live redeploy: keeping active generation until candidate is ready"
            );
            let _ = tx
                .send(DeployEvent::Progress {
                    phase: "candidate".into(),
                    description: format!(
                        "Booting generation {generation_id} alongside active service"
                    ),
                })
                .await;
        } else {
            self.vacate(service_id, prior_runtime, &dirs, has_russel_dir)
                .await?;
        }

        let slot = Slot {
            prior_runtime,
            dual_live,
            generation_id,
            runtime_key,
            dirs,
            has_backup: has_russel_dir && !dual_live,
        };

        // Cold redeploy stashed volumes before the .bak rename. Put them back
        // on the stable id before the new container bind-mounts that path.
        // Dual-live has no stash; this is a no-op and the live tree stays put.
        if let Err(e) = attach_managed_volumes(Path::new(&slot.dirs.russel)).await {
            if slot.has_backup {
                slot.dirs.restore().await;
            }
            return Err(e);
        }
        Ok(slot)
    }

    /// Cold path: stop the live generation so the new one can boot in its
    /// place. Dual-live keeps the active generation untouched until cutover.
    ///
    /// Order (F-23): rename dirs to .bak FIRST so any failure after this
    /// point is rollback-covered. Then disarm the supervisor + kill children.
    /// 1. Rename dirs to .bak (creates rollback safety net)
    /// 2. take_processes — disarm supervisor
    /// 3. kill+wait old children so ports are freed
    /// 4. destroy_prior_runtime — cleans TAP/ports (rollback will re-create)
    async fn vacate(
        &self,
        service_id: &str,
        prior_runtime: Option<RuntimeKind>,
        dirs: &ServiceDirs,
        has_russel_dir: bool,
    ) -> anyhow::Result<()> {
        if has_russel_dir {
            let live = Path::new(&dirs.russel);
            // Park volumes outside the directory we are about to rename.
            // Success deletes the .bak; the data must not be inside it.
            if let Err(e) = detach_managed_volumes(live).await {
                anyhow::bail!("failed to stash managed volumes: {e}");
            }
            if let Err(e) = tokio::fs::rename(&dirs.russel, &dirs.russel_bak).await {
                let _ = attach_managed_volumes(live).await;
                anyhow::bail!("failed to backup russel directory: {}", e);
            }
            if dirs.has_microvms
                && let Err(e) = tokio::fs::rename(&dirs.microvms, &dirs.microvms_bak).await
            {
                let _ = restore_backed_up_service_dir(live, Path::new(&dirs.russel_bak)).await;
                anyhow::bail!("failed to backup microvms directory: {}", e);
            }
        }

        let (old_vm_proc, old_aux_procs) = self
            .state
            .take_processes(service_id)
            .unwrap_or((None, Vec::new()));
        kill_and_wait_children(old_vm_proc, old_aux_procs).await;

        if let Some(prior_kind) = prior_runtime
            && let Err(e) =
                destroy_prior_runtime(prior_kind, service_id, &self.runner, &self.containers).await
        {
            if has_russel_dir {
                dirs.restore().await;
            }
            anyhow::bail!("failed to teardown prior {}: {}", prior_kind, e);
        }
        Ok(())
    }

    /// Reserve ports and start the new generation under `slot.runtime_key`.
    /// `reservation` is armed once the primary port is held.
    async fn boot(
        &self,
        service_id: &str,
        plan: &Plan,
        built: &Built,
        slot: &Slot,
        reservation: &mut Option<PortReservation>,
        tx: &Events,
    ) -> anyhow::Result<Booted> {
        let key = &slot.runtime_key;
        // Determine port first, then arm the reservation (F-28: avoid
        // releasing the old service's port on early allocation failure).
        let port = match plan.pin_mapping.clone() {
            Some(pinned) if !slot.dual_live => {
                PortAllocator::reserve(key, pinned.host)?;
                pinned
            }
            pinned => {
                // Dual-live always allocates a fresh backend port for the
                // candidate so the active generation keeps its listener. Fixed
                // -p is Traefik-facing after cutover; backend port may differ
                // across generations.
                if pinned.is_some() {
                    tracing::info!(
                        service_id,
                        "dual-live redeploy: ignoring fixed -p for candidate backend; \
                         Traefik host stays stable via Ingress::swap"
                    );
                }
                let ports = self.ports.clone();
                let owned_key = key.clone();
                let host = tokio::task::spawn_blocking(move || ports.next(&owned_key)).await??;
                PortMapping {
                    host,
                    guest: plan.config.service.port,
                }
            }
        };
        // Arm before extras so a failed extra reserve releases primary + prior extras.
        *reservation = Some(PortReservation::new(key));
        for (i, extra) in plan.config.ports.iter().enumerate() {
            reject_live_listen_collision(extra.host, "[[ports]] host")?;
            PortAllocator::reserve(&extra_port_key(key, i), extra.host)?;
        }

        let generation_id = Some(slot.generation_id.as_str());
        // A cold replace already stopped the live generation, so a crash right
        // after answering must fail in boot, where the .bak restore still
        // covers it. Dual-live watches in `hold` instead, with traffic already
        // switched; a first deploy has nothing to protect and does not wait.
        let settle = if slot.prior_runtime.is_some() && !slot.dual_live {
            WATCH_WINDOW
        } else {
            Duration::ZERO
        };
        let (workload, create_ms, start_ms, network_ms, ready_ms) = match plan.runtime {
            RuntimeKind::Microvm => {
                self.deploy_microvm(
                    key,
                    &plan.config,
                    &built.output.store_path,
                    &port,
                    built
                        .kernel
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("kernel not resolved"))?,
                    &plan.env,
                    tx,
                    generation_id,
                    plan.desired_state.as_ref(),
                    &plan.volumes,
                    built.output.using_package,
                    settle,
                )
                .await?
            }
            RuntimeKind::Container => {
                self.deploy_container(
                    key,
                    &plan.config,
                    &built.output.store_path,
                    &port,
                    &plan.config.service.podman_args,
                    &plan.env,
                    tx,
                    generation_id,
                    plan.desired_state.as_ref(),
                    &plan.volumes,
                    built.output.using_package,
                    settle,
                )
                .await?
            }
        };
        Ok(Booted {
            workload,
            create_ms,
            start_ms,
            network_ms,
            ready_ms,
        })
    }

    /// The new generation did not come up. Destroy what it left, and on the
    /// cold path put the previous generation back from its `.bak` dirs.
    async fn recover_failed_boot(
        &self,
        service_id: &str,
        plan: &Plan,
        slot: &Slot,
        error: anyhow::Error,
        mut reservation: Option<PortReservation>,
    ) -> anyhow::Result<DeployInnerResult> {
        tracing::error!(
            service_id,
            runtime_key = %slot.runtime_key,
            error = %error,
            "Deployment failed — cleaning up candidate (prior runtime was {:?})",
            slot.prior_runtime
        );
        // Only destroy the candidate; dual-live leaves the active generation alone.
        cleanup_failed_deploy(
            plan.runtime,
            &slot.runtime_key,
            &self.runner,
            &self.containers,
        )
        .await;
        if slot.dual_live {
            // Active generation never stopped — report hard failure without rollback.
            return Err(
                error.context("candidate generation failed; active generation left untouched")
            );
        }
        if !slot.has_backup {
            return Err(error);
        }

        let dirs = &slot.dirs;
        let (prior, rollback) = match slot.prior_runtime {
            Some(RuntimeKind::Microvm) => (
                RuntimeKind::Microvm,
                attempt_microvm_rollback(
                    service_id,
                    &dirs.russel,
                    &dirs.microvms,
                    &dirs.russel_bak,
                    &dirs.microvms_bak,
                    dirs.has_microvms,
                    &self.runner,
                    &self.state,
                )
                .await,
            ),
            Some(RuntimeKind::Container) => (
                RuntimeKind::Container,
                attempt_container_rollback(
                    service_id,
                    &dirs.russel,
                    &dirs.russel_bak,
                    &self.containers,
                    &self.state,
                )
                .await,
            ),
            None => {
                dirs.restore().await;
                return Err(error);
            }
        };
        match rollback {
            Ok(()) => {
                tracing::info!(service_id, runtime = %prior, "rollback to the previous generation succeeded");
                crate::gcroots::sync_logged(service_id).await;
                // The restored generation holds these ports again.
                if let Some(reservation) = reservation.as_mut() {
                    reservation.disarm();
                }
                Ok(DeployInnerResult::RolledBack {
                    runtime: prior,
                    error: error.to_string(),
                })
            }
            Err(rollback_err) => {
                tracing::error!(
                    service_id,
                    runtime = %prior,
                    error = %rollback_err,
                    "CRITICAL: rollback failed; the previous generation could not be restored"
                );
                Err(error)
            }
        }
    }

    /// Point the stable route at the new generation. On failure the candidate
    /// is torn down in full (#116); a dual-live active generation stays.
    async fn swap_ingress(
        &self,
        service_id: &str,
        plan: &Plan,
        slot: &Slot,
        workload: &DeployWorkload,
    ) -> anyhow::Result<()> {
        let backend = Backend::from_publish(workload.port().host);
        let result = if slot.dual_live {
            // Zero-downtime cutover: rewrite Traefik backend for the stable service id.
            tracing::info!(
                service_id,
                backend = %backend.url(),
                generation_id = %slot.generation_id,
                "Ingress::swap — pointing stable route at candidate generation"
            );
            self.ingress
                .swap(service_id, &backend, &plan.host_rules)
                .await
        } else {
            self.ingress
                .register(service_id, &backend, &plan.host_rules)
                .await
        };
        let Err(error) = result else {
            return Ok(());
        };
        self.destroy_candidate(slot, workload, "ingress failure")
            .await;
        Err(error)
    }

    /// Tear down the new generation in full: its network, its VM or
    /// container, and its ports. The live generation is not touched.
    async fn destroy_candidate(&self, slot: &Slot, workload: &DeployWorkload, why: &str) {
        let key = &slot.runtime_key;
        workload.teardown_network().await;
        match workload {
            DeployWorkload::Microvm { .. } => {
                if let Err(e) = self.runner.destroy(key).await {
                    tracing::warn!(runtime_key = %key, error = %e, why, "failed to destroy candidate microVM");
                }
            }
            DeployWorkload::Container { .. } => {
                if let Err(e) = destroy_preserving_volumes(key).await {
                    tracing::warn!(runtime_key = %key, error = %e, why, "failed to destroy candidate container");
                }
            }
        }
        PortAllocator::release_service(key);
    }

    /// The live generation's route: the host port it publishes and the Host
    /// rule it is served under, which this deploy may change. `None` when the
    /// port is unknown.
    fn live_route(&self, service_id: &str, plan: &Plan) -> Option<(u16, Vec<HostRule>)> {
        let meta = load_metadata_from_disk(service_id);
        let port = self
            .state
            .status(service_id)
            .and_then(|s| s.host_port)
            .or_else(|| meta.as_ref().and_then(|m| m.host_port))
            .filter(|p| *p > 0)?;
        let host_rules = match meta {
            Some(m) => m
                .ingress_host
                .map(|host| vec![HostRule { host }])
                .unwrap_or_default(),
            None => plan.host_rules.clone(),
        };
        Some((port, host_rules))
    }

    /// Dual-live only. Traffic is on the new generation now, but the previous
    /// one is still running. Keep it for [`WATCH_WINDOW`]: if the new one dies
    /// meanwhile (#493), point traffic back, tear the new one down, and fail
    /// the deploy. The previous generation was never stopped, so nothing has
    /// to be restored.
    async fn hold(
        &self,
        service_id: &str,
        slot: &Slot,
        workload: &mut DeployWorkload,
        previous_route: Option<(u16, Vec<HostRule>)>,
        tx: &Events,
    ) -> anyhow::Result<()> {
        if !slot.dual_live {
            return Ok(());
        }
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "switch".into(),
                description: format!(
                    "New version is live; keeping the previous one running for {}s in case it crashes",
                    WATCH_WINDOW.as_secs()
                ),
            })
            .await;
        let Some(error) = watch_candidate(&slot.runtime_key, workload, WATCH_WINDOW).await else {
            return Ok(());
        };
        tracing::warn!(
            service_id,
            runtime_key = %slot.runtime_key,
            error = %error,
            "new generation died inside the watch window; switching traffic back"
        );

        let switched_back = match previous_route {
            Some((port, host_rules)) => match self
                .ingress
                .swap(service_id, &Backend::from_publish(port), &host_rules)
                .await
            {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(service_id, error = %e, "could not point the route back at the previous generation");
                    false
                }
            },
            None => false,
        };
        self.destroy_candidate(slot, workload, "crashed inside the watch window")
            .await;
        let outcome = if switched_back {
            "traffic is back on the previous version, which kept running"
        } else {
            "the previous version kept running, but its ingress route could not be restored; \
             run `russel apply --force` with a working build"
        };
        anyhow::bail!(
            "new version crashed within {}s of answering; {outcome}: {error}",
            WATCH_WINDOW.as_secs()
        )
    }

    /// Retire what the new generation replaced. Cold: drop the `.bak` dirs.
    /// Dual-live: drain and destroy the old generation, then promote the
    /// candidate to the stable service id. Returns the socat and host port
    /// when a pinned host port was won back.
    async fn cutover(
        &self,
        service_id: &str,
        plan: &Plan,
        slot: &Slot,
        workload: &DeployWorkload,
        tx: &Events,
    ) -> Option<(tokio::process::Child, u16)> {
        if !slot.dual_live {
            if slot.has_backup {
                slot.dirs.remove_backups().await;
            }
            self.state
                .attach_flake_path(service_id, plan.build_path.clone());
            return None;
        }

        // Drain old generation only after successful swap.
        let _ = tx
            .send(DeployEvent::Progress {
                phase: "cutover".into(),
                description: "Draining previous generation after ingress swap".into(),
            })
            .await;

        let (old_vm_proc, old_aux_procs) = self
            .state
            .take_processes(service_id)
            .unwrap_or((None, Vec::new()));
        kill_and_wait_children(old_vm_proc, old_aux_procs).await;

        let key = &slot.runtime_key;
        // The history journal lives in the stable service dir, which the
        // old generation's destroy removes and promotion replaces. Move it
        // into the candidate's dir so it survives: rollback reads it.
        carry_history(service_id, key).await;

        if let Some(prior_kind) = slot.prior_runtime
            && let Err(e) =
                destroy_prior_runtime(prior_kind, service_id, &self.runner, &self.containers).await
        {
            tracing::error!(
                service_id,
                error = %e,
                "CRITICAL: candidate is live and swapped but old generation destroy failed"
            );
            // Continue promote — traffic is already on the candidate.
        }

        // Promote candidate dirs to the stable service_id path.
        if let Err(e) = promote_generation(key, service_id).await {
            tracing::error!(
                service_id,
                runtime_key = %key,
                error = %e,
                "CRITICAL: failed to promote generation dirs; candidate still running under runtime key"
            );
            // Keep state under runtime_key so stop/destroy can find it.
            self.state.attach_flake_path(key, plan.build_path.clone());
            return None;
        }

        // Re-key port + in-memory state to the stable service id.
        PortAllocator::release_service(key);
        if let Err(e) = PortAllocator::claim_existing(service_id, workload.port().host) {
            tracing::warn!(service_id, error = %e, "failed to claim port under service_id after promote");
        }
        for (i, extra) in plan.config.ports.iter().enumerate() {
            if let Err(e) =
                PortAllocator::claim_existing(&extra_port_key(service_id, i), extra.host)
            {
                tracing::warn!(
                    service_id,
                    index = i,
                    host = extra.host,
                    error = %e,
                    "failed to claim extra port under service_id after promote"
                );
            }
        }
        self.state.rekey_service(key, service_id);
        self.state
            .attach_flake_path(service_id, plan.build_path.clone());

        reclaim_fixed_port(service_id, plan, workload).await
    }

    /// Record the source, the deployment history row, and GC roots.
    async fn record(
        &self,
        service_id: &str,
        request: &DeployRequest,
        plan: &Plan,
        built: &Built,
        slot: &Slot,
        workload: &DeployWorkload,
    ) {
        let repo_url = redact_repo_url(&request.repo_url);
        // Record source so `russel update` / health restart can redeploy.
        if let Err(e) = record_source_in_metadata(service_id, &repo_url, &request.config_path) {
            tracing::warn!(service_id, error = %e, "failed to record source in metadata");
        }

        // Append deployment history journal (operators / dashboard rollback surface).
        let port = workload.port();
        let desired_snap = DesiredStateSnapshot::from_desired_json(
            plan.desired_state.as_ref(),
            Some(port.host),
            Some(port.guest),
        );
        if let Err(e) = deployments::append_success(
            service_id,
            AppendSuccess {
                generation_id: Some(slot.generation_id.clone()),
                runtime: Some(plan.runtime),
                store_path: Some(built.output.store_path.display().to_string()),
                repo_url: Some(repo_url),
                config_path: Some(request.config_path.clone()),
                host_port: Some(port.host),
                guest_port: Some(port.guest),
                message: Some("deploy complete".into()),
                desired_state: Some(desired_snap),
            },
        ) {
            tracing::warn!(service_id, error = %e, "failed to append deployment history");
        }
        // Root this generation and the previous one; drop older roots (#411).
        crate::gcroots::sync_logged(service_id).await;
    }
}

/// Turn the Russelfile into the deploy's inputs. Touches no state.
fn plan(service_id: &str, request: &DeployRequest, source: ResolvedSource) -> anyhow::Result<Plan> {
    let ResolvedSource {
        repo_path,
        _checkout,
        config,
        rev,
    } = source;
    let runtime = config.service.runtime;
    let podman_args = &config.service.podman_args;
    validate_podman_args_for_runtime(runtime, podman_args)?;
    let build_path =
        super::config::resolve_build_dir(&repo_path, &request.config_path, &config.service.source)?;
    if runtime == RuntimeKind::Microvm {
        crate::microvm::preflight::check_host_privileges()?;
    }

    // The Russelfile is the whole desired state: env comes from
    // [service.env] alone. Keep the pre-resolution map for desired_state
    // so rollback and health restart re-resolve secret:// refs.
    let env_pre_resolve = &config.service.env;
    validate_env_map(env_pre_resolve)?;
    let run_as = super::RunAs::from_user(config.service.user.as_deref());
    let mut env = crate::secrets::resolve_env_secrets(env_pre_resolve)?;
    run_as.apply_env_defaults(&mut env);
    validate_env_map(&env)?;

    let ingress_host =
        resolve_ingress_host(config.ingress.as_ref().and_then(|i| i.host.as_deref()))?;
    // service.port is the guest side of the primary publish in every
    // case; [ingress].port only pins the host side (#385).
    let pin_mapping = resolve_primary_publish(&config);
    validate_live_ingress_port(pin_mapping.as_ref().map(|p| p.host))?;

    let host_rules = ingress_host
        .as_ref()
        .map(|host| vec![HostRule { host: host.clone() }])
        .unwrap_or_default();

    // Same rows and meaning on both runtimes (#386).
    let volumes = resolve_volumes(service_id, &config.volumes, &volume_roots_from_env())?;

    let desired_state = Some(build_desired_state(
        &request.repo_url,
        &request.config_path,
        runtime,
        config.service.guest,
        env_pre_resolve,
        podman_args,
        pin_mapping.as_ref(),
        ingress_host.as_deref(),
        DesiredExtras {
            volumes: &volumes,
            extra_ports: &config.ports,
            package: config.service.package.as_deref(),
            args: &config.service.args,
            run_as: Some(run_as),
            restart: config.service.restart.as_deref(),
            rev: rev.as_ref(),
        },
    ));

    Ok(Plan {
        build_path,
        _checkout,
        config,
        rev,
        runtime,
        env,
        ingress_host,
        host_rules,
        pin_mapping,
        volumes,
        desired_state,
    })
}

/// H4: after the old generation is destroyed, reclaim the operator's fixed
/// `-p` for TAP microVMs only (spawn extra socat — never
/// TapForwarder::setup, which deletes TAP). Containers keep the candidate
/// publish port (rebind would require podman recreate), and so do passt
/// microVMs, whose guest IP the host cannot dial; Traefik is SoT.
async fn reclaim_fixed_port(
    service_id: &str,
    plan: &Plan,
    workload: &DeployWorkload,
) -> Option<(tokio::process::Child, u16)> {
    // Keep 0 out of recovery even if an invalid mapping slipped through.
    let fixed = plan
        .pin_mapping
        .as_ref()
        .map(|p| p.host)
        .filter(|&h| h != 0)?;
    let DeployWorkload::Microvm {
        alloc,
        port,
        net_mode: MicrovmNetMode::Tap,
        ..
    } = workload
    else {
        return None;
    };
    let ephemeral = port.host;
    if fixed == ephemeral {
        return None;
    }
    // service_id currently holds ephemeral via claim_existing in cutover.
    if let Err(e) = PortAllocator::reserve(service_id, fixed) {
        tracing::warn!(
            service_id,
            fixed,
            error = %e,
            "dual-live fixed port not free — using candidate ephemeral"
        );
        return None;
    }
    tracing::info!(
        service_id,
        fixed,
        ephemeral,
        "reclaimed fixed host port after dual-live cutover"
    );
    match TapForwarder::spawn_socat(service_id, fixed, &alloc.vm_ip, port.guest).await {
        Ok(socat) => Some((socat, fixed)),
        Err(e) => {
            tracing::warn!(
                service_id,
                error = %e,
                "dual-live fixed-port socat spawn failed; \
                 traffic remains on ephemeral {ephemeral}"
            );
            // Restore ephemeral registration for status/list.
            let _ = PortAllocator::claim_existing(service_id, ephemeral);
            None
        }
    }
}

pub(crate) struct PortReservation {
    service_id: String,
    armed: bool,
}

impl PortReservation {
    fn new(service_id: &str) -> Self {
        Self {
            service_id: service_id.to_string(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PortReservation {
    fn drop(&mut self) {
        if self.armed {
            PortAllocator::release_service(&self.service_id);
        }
    }
}

/// Watch a new generation for `window`. `Some(error)` when it died meanwhile:
/// its VM exited (the agent powers the guest off when the app exits) or its
/// container exited or restarted. The error quotes the console or log tail.
async fn watch_candidate(
    runtime_key: &str,
    workload: &mut DeployWorkload,
    window: Duration,
) -> Option<String> {
    match workload {
        DeployWorkload::Microvm { vm_child, .. } => {
            let status = ready::watch(vm_child, window).await?;
            let console = crate::paths::service_dir(runtime_key).join("console.log");
            Some(ready::exited_error(
                status,
                &ready::console_tail(&console, ready::CONSOLE_TAIL_LINES),
            ))
        }
        DeployWorkload::Container {
            container_name,
            port,
            ..
        } => {
            let state = watch_container(container_name, window).await?;
            let tail = log_tail(&container_log_path(runtime_key), LOG_TAIL_LINES);
            Some(not_ready_error(
                &ReadyOutcome::Died(state),
                port.host,
                port.guest,
                CONTAINER_READY_TIMEOUT,
                &tail,
            ))
        }
    }
}
