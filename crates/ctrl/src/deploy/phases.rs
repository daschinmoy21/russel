//! The phases of one deploy (#415), in order: plan → build → prepare_slot →
//! boot → swap_ingress → hold → cutover → record. Each phase is one function that
//! takes what the earlier phases produced; `deploy_inner` only sequences them.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use russel_core::{
    api::{DeployRequest, DeployTiming, PortMapping},
    config::{
        RuntimeKind, Russelfile, resolve_ingress_host, resolve_primary_publish, validate_env_map,
    },
    volumes::{
        ExtraPortSpec, ResolvedVolume, extra_port_key, resolve_volumes, volume_roots_from_env,
    },
};

use crate::{
    build::BuildOutput,
    container::{
        CONTAINER_READY_TIMEOUT, LOG_TAIL_LINES, ReadyOutcome, attach_managed_volumes,
        container_log_path, destroy_preserving_volumes, detach_managed_volumes, log_tail,
        not_ready_error, restore_backed_up_service_dir, validate_podman_args_for_runtime,
        watch_container,
    },
    deployments::{
        self, AppendSuccess, DesiredStateSnapshot, GenerationArtifact, MAX_RECORDED_LOCK_BYTES,
        SECRETS_RESOLVED_AT_LAUNCH,
    },
    git::{CheckoutLease, SourceRev, redact_repo_url},
    ingress::{Backend, HostRule},
    metadata::load_metadata_from_disk,
    microvm::{KernelInfo, ready},
    network::PortAllocator,
};

use super::pipeline::{
    DeployInnerResult, DeployOutput, DeployPipeline, DeployWorkload, DesiredExtras, HistoryMove,
    ResolvedSource, SourceOrigin, build_desired_state, carry_history, kept_history_dir,
    new_generation_id, preserve_history, promote_generation, record_source_in_metadata,
    reject_live_listen_collision, validate_live_ingress_port,
};
use super::rollback::{
    ServiceDirs, attempt_container_rollback, attempt_microvm_rollback, cleanup_failed_deploy,
    destroy_prior_runtime, kill_and_wait_children, resolve_prior_runtime,
};
use super::{Events, WATCH_WINDOW};

/// The Russelfile turned into the deploy's inputs. Every later phase reads it.
pub(super) struct Plan {
    origin: PlanOrigin,
    pub(super) config: Russelfile,
    /// The Russelfile's text, recorded with the generation (#558).
    russelfile: String,
    /// Commit the source builds; `None` outside git (#448).
    rev: Option<SourceRev>,
    runtime: RuntimeKind,
    /// `[service.env]` with `secret://` refs resolved: what the workload gets.
    pub(super) env: HashMap<String, String>,
    ingress_host: Option<String>,
    host_rules: Vec<HostRule>,
    /// The primary publish `[ingress].port` pins, if any.
    pin_mapping: Option<PortMapping>,
    pub(super) volumes: Vec<ResolvedVolume>,
    /// Replayed by rollback, health restart, and update (F-04/08/09).
    pub(super) desired_state: Option<serde_json::Value>,
}

/// Where the plan's build output comes from.
enum PlanOrigin {
    Build {
        /// The Russelfile's folder joined with `service.source`: where Nix
        /// builds (#525). The repo root for a root Russelfile with
        /// `source = "."`.
        build_path: PathBuf,
        /// Keeps the checkout alive until the deploy ends.
        _checkout: CheckoutLease,
    },
    /// A recorded generation's output, launched as is (#558).
    Artifact(GenerationArtifact),
}

impl Plan {
    /// The checkout dir that was built; `None` when relaunching a recorded
    /// generation.
    fn build_path(&self) -> Option<&Path> {
        match &self.origin {
            PlanOrigin::Build { build_path, .. } => Some(build_path),
            PlanOrigin::Artifact(_) => None,
        }
    }
}

pub(super) struct Built {
    pub(super) output: BuildOutput,
    /// What gets recorded with the generation for rollback (#558).
    artifact: GenerationArtifact,
    /// Resolved for the microVM runtime only.
    pub(super) kernel: Option<KernelInfo>,
    build_ms: u128,
}

/// Where the new generation boots, and what to put back if it fails.
pub(super) struct Slot {
    prior_runtime: Option<RuntimeKind>,
    /// Boot beside the live generation and swap ingress to it (zero
    /// downtime). Otherwise the live generation is stopped first (cold).
    dual_live: bool,
    pub(super) generation_id: String,
    /// `{service_id}_g{generation_id}` when dual-live, else the service id.
    pub(super) runtime_key: String,
    dirs: ServiceDirs,
    /// Cold path moved the live service dir to `.bak`; rollback reads it.
    has_backup: bool,
}

/// A started generation that answered, with its phase timings.
pub(super) struct Booted {
    pub(super) workload: DeployWorkload,
    pub(super) create_ms: u128,
    pub(super) start_ms: u128,
    pub(super) network_ms: u128,
    pub(super) ready_ms: u128,
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
        // A route that cannot be registered fails here, while the live
        // generation is still running. `register` checks again after boot.
        self.ingress
            .check_route(service_id, &plan.host_rules)
            .await?;
        // The live generation's route, to point back at if this deploy does
        // not take it over (`hold`, `recover_failed_ingress`).
        let previous_route = self.live_route(service_id, &plan);
        let slot = self.prepare_slot(service_id, &plan, &tx).await?;

        // Armed once boot reserves ports, so dropping it releases them.
        let mut reservation = None;
        let mut booted = match self.boot(&plan, &built, &slot, &mut reservation, &tx).await {
            Ok(booted) => booted,
            Err(error) => {
                return self
                    .recover_failed_boot(service_id, &plan, &slot, error, reservation)
                    .await;
            }
        };

        if let Err(error) = self
            .swap_ingress(service_id, &plan, &slot, &booted.workload)
            .await
        {
            return self
                .recover_failed_ingress(
                    service_id,
                    &slot,
                    &booted.workload,
                    error,
                    previous_route,
                    reservation,
                )
                .await;
        }
        self.hold(service_id, &slot, &mut booted.workload, previous_route, &tx)
            .await?;
        self.cutover(service_id, &plan, &slot, &booted.workload, &tx)
            .await;
        self.record(
            service_id,
            &request,
            &plan,
            &built,
            &slot,
            &booted.workload,
            &tx,
        )
        .await;

        reservation
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("port reservation dropped before deploy completed"))?
            .disarm();

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
            rev: plan.rev.map(|r| r.rev),
        })))
    }

    async fn build(&self, service_id: &str, plan: &Plan, tx: &Events) -> anyhow::Result<Built> {
        let t = Instant::now();
        let microvm = plan.runtime == RuntimeKind::Microvm;
        let (output, artifact) = match &plan.origin {
            PlanOrigin::Build { build_path, .. } => {
                let description = if microvm {
                    "Building package + ensuring kernel/busybox"
                } else {
                    "Building package"
                };
                super::progress(tx, "build", description).await;
                let output = self
                    .builder
                    .build(build_path, plan.config.service.package.as_deref())
                    .await?;
                let artifact = GenerationArtifact {
                    store_path: output.store_path.display().to_string(),
                    using_package: output.using_package,
                    russelfile: plan.russelfile.clone(),
                    rev: plan.rev.as_ref().map(|r| r.rev.clone()),
                    dirty: plan.rev.as_ref().is_some_and(|r| r.dirty),
                    flake_lock: read_flake_lock(build_path),
                    secrets: SECRETS_RESOLVED_AT_LAUNCH.into(),
                };
                (output, artifact)
            }
            PlanOrigin::Artifact(artifact) => {
                super::progress(
                    tx,
                    "build",
                    format!("Using the recorded build {}", artifact.store_path),
                )
                .await;
                let output = BuildOutput {
                    store_path: PathBuf::from(&artifact.store_path),
                    using_package: artifact.using_package,
                };
                (output, artifact.clone())
            }
        };
        // Nothing roots the output until `record`, so a garbage collection
        // during boot could delete it. For a recorded generation this is also
        // the check that its output still exists.
        if let Err(e) = crate::gcroots::root_in_flight(service_id, &output.store_path).await {
            if matches!(plan.origin, PlanOrigin::Artifact(_)) {
                anyhow::bail!(
                    "the recorded build {} cannot be used ({e:#}); \
                     roll back with --rebuild to build this version from source",
                    artifact.store_path
                );
            }
            tracing::warn!(service_id, error = %e, "cannot root the new build");
            super::progress(
                tx,
                "build",
                format!("warning: cannot add a Nix GC root for the new build: {e:#}"),
            )
            .await;
        }
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
            artifact,
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
        let dual_live = is_dual_live(
            prior_runtime,
            plan.pin_mapping.as_ref(),
            &plan.config.ports,
            &plan.volumes,
        );

        // Generation identity: when replacing a live service, boot the candidate
        // under `{service_id}_g{gen}` so the active generation keeps its TAP/port
        // until ingress.swap and cutover complete (zero-downtime path).
        let mut generation_id = new_generation_id();
        // A promote link (#548) may already hold this name, and the
        // generation it names can still be running under it.
        while std::fs::symlink_metadata(crate::paths::service_dir(&format!(
            "{service_id}_g{generation_id}"
        )))
        .is_ok()
        {
            generation_id = new_generation_id();
        }
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
            super::progress(
                tx,
                "candidate",
                format!("Booting generation {generation_id} alongside active service"),
            )
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
            && let Err(e) = destroy_prior_runtime(prior_kind, service_id, &self.runner).await
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
        plan: &Plan,
        built: &Built,
        slot: &Slot,
        reservation: &mut Option<PortReservation>,
        tx: &Events,
    ) -> anyhow::Result<Booted> {
        let key = &slot.runtime_key;
        // Determine port first, then arm the reservation (F-28: avoid
        // releasing the old service's port on early allocation failure).
        let ports = self.ports.clone();
        let owned_key = key.clone();
        let pin = plan.pin_mapping.clone();
        let guest = plan.config.service.port;
        let port =
            tokio::task::spawn_blocking(move || reserve_primary(&ports, &owned_key, pin, guest))
                .await??;
        // Arm before extras so a failed extra reserve releases primary + prior extras.
        *reservation = Some(PortReservation::new(key));
        for (i, extra) in plan.config.ports.iter().enumerate() {
            reject_live_listen_collision(extra.host, "[[ports]] host")?;
            PortAllocator::reserve(&extra_port_key(key, i), extra.host)?;
        }

        // A cold replace already stopped the live generation, so a crash right
        // after answering must fail in boot, where the .bak restore still
        // covers it. Dual-live watches in `hold` instead, with traffic already
        // switched; a first deploy has nothing to protect and does not wait.
        let settle = if slot.prior_runtime.is_some() && !slot.dual_live {
            WATCH_WINDOW
        } else {
            Duration::ZERO
        };
        match plan.runtime {
            RuntimeKind::Microvm => {
                self.deploy_microvm(plan, built, slot, &port, settle, tx)
                    .await
            }
            RuntimeKind::Container => {
                self.deploy_container(plan, built, slot, &port, settle, tx)
                    .await
            }
        }
    }

    /// The new generation did not come up. Destroy what it left, and on the
    /// cold path put the previous generation back from its `.bak` dirs.
    async fn recover_failed_boot(
        &self,
        service_id: &str,
        plan: &Plan,
        slot: &Slot,
        error: anyhow::Error,
        reservation: Option<PortReservation>,
    ) -> anyhow::Result<DeployInnerResult> {
        tracing::error!(
            service_id,
            runtime_key = %slot.runtime_key,
            error = %error,
            "Deployment failed — cleaning up candidate (prior runtime was {:?})",
            slot.prior_runtime
        );
        // Only destroy the candidate; dual-live leaves the active generation alone.
        cleanup_failed_deploy(plan.runtime, &slot.runtime_key, &self.runner).await;
        self.restore_previous(service_id, slot, error, reservation)
            .await
    }

    /// The ingress refused the new generation after it booted. Tear the
    /// candidate down. On the cold path the previous generation is already
    /// stopped, so put it back as after a failed boot and point its route at
    /// it again. Dual-live never stopped it, so there is nothing to restore.
    async fn recover_failed_ingress(
        &self,
        service_id: &str,
        slot: &Slot,
        workload: &DeployWorkload,
        error: anyhow::Error,
        previous_route: Option<(u16, Vec<HostRule>)>,
        reservation: Option<PortReservation>,
    ) -> anyhow::Result<DeployInnerResult> {
        tracing::error!(
            service_id,
            runtime_key = %slot.runtime_key,
            error = %error,
            "ingress failed after the new generation booted; tearing it down"
        );
        self.destroy_candidate(slot, workload, "ingress failure")
            .await;
        let result = self
            .restore_previous(service_id, slot, error, reservation)
            .await?;
        let DeployInnerResult::RolledBack { runtime, error } = result else {
            return Ok(result);
        };
        let error = match self.restore_route(service_id, previous_route).await {
            Ok(()) => error,
            Err(route_error) => format!(
                "{error}; the previous version is running again, but its ingress route \
                 could not be restored: {route_error:#}"
            ),
        };
        Ok(DeployInnerResult::RolledBack { runtime, error })
    }

    /// Register the previous generation's route again. Nothing to do when it
    /// is unknown.
    async fn restore_route(
        &self,
        service_id: &str,
        previous_route: Option<(u16, Vec<HostRule>)>,
    ) -> anyhow::Result<()> {
        let Some((port, host_rules)) = previous_route else {
            return Ok(());
        };
        self.ingress
            .register(service_id, &Backend::from_publish(port), &host_rules)
            .await
    }

    /// The candidate is gone. Dual-live reports the failure. The cold path
    /// puts the previous generation back from its `.bak` dirs.
    async fn restore_previous(
        &self,
        service_id: &str,
        slot: &Slot,
        error: anyhow::Error,
        mut reservation: Option<PortReservation>,
    ) -> anyhow::Result<DeployInnerResult> {
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
                attempt_microvm_rollback(service_id, dirs, &self.runner, &self.state).await,
            ),
            Some(RuntimeKind::Container) => (
                RuntimeKind::Container,
                attempt_container_rollback(service_id, dirs, &self.containers, &self.state).await,
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

    /// Point the stable route at the new generation. The caller tears the
    /// candidate down in full (#116) when this fails.
    async fn swap_ingress(
        &self,
        service_id: &str,
        plan: &Plan,
        slot: &Slot,
        workload: &DeployWorkload,
    ) -> anyhow::Result<()> {
        let backend = Backend::from_publish(workload.port().host);
        if slot.dual_live {
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
        }
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
        super::progress(
            tx,
            "switch",
            format!(
                "New version is live; keeping the previous one running for {}s in case it crashes",
                WATCH_WINDOW.as_secs()
            ),
        )
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
    /// candidate to the stable service id.
    async fn cutover(
        &self,
        service_id: &str,
        plan: &Plan,
        slot: &Slot,
        workload: &DeployWorkload,
        tx: &Events,
    ) {
        if !slot.dual_live {
            if slot.has_backup {
                // `vacate` moved the history into the `.bak` dir with the rest
                // of the service dir. Bring it back before the backup goes.
                match carry_history(
                    Path::new(&slot.dirs.russel_bak),
                    Path::new(&slot.dirs.russel),
                )
                .await
                {
                    Ok(()) => slot.dirs.remove_backups().await,
                    Err(e) => {
                        // Removing the backup now would delete the history.
                        // Keep it aside instead, out of the way of the next
                        // cold replace.
                        let kept = kept_history_dir(service_id, &slot.generation_id);
                        let outcome = match tokio::fs::rename(&slot.dirs.russel_bak, &kept).await {
                            Ok(()) => {
                                format!("kept the previous service dir at {}", kept.display())
                            }
                            Err(_) => {
                                format!("left the previous service dir at {}", slot.dirs.russel_bak)
                            }
                        };
                        tracing::error!(service_id, error = %e, "{outcome}: deployment history could not be moved");
                        super::progress(
                            tx,
                            "cutover",
                            format!(
                                "warning: deployment history could not be moved ({e}); {outcome}"
                            ),
                        )
                        .await;
                    }
                }
            }
            self.attach_build_path(service_id, plan);
            return;
        }

        // Drain old generation only after successful swap.
        super::progress(
            tx,
            "cutover",
            "Draining previous generation after ingress swap",
        )
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
        let live_dir = crate::paths::service_dir(service_id);
        let kept = kept_history_dir(service_id, &slot.generation_id);
        match preserve_history(&live_dir, &crate::paths::service_dir(key), &kept).await {
            Ok(HistoryMove::Carried) => {}
            Ok(HistoryMove::Kept(e)) => {
                tracing::error!(service_id, error = %e, kept = %kept.display(), "deployment history kept aside");
                super::progress(
                    tx,
                    "cutover",
                    format!(
                        "warning: deployment history could not be moved ({e}); kept it at {}",
                        kept.display()
                    ),
                )
                .await;
            }
            Err(e) => {
                // Nothing else holds the history, and the destroy below
                // would delete it. Leave the previous service dir alone and
                // keep the candidate under its runtime key, as a failed
                // promote does. Its processes are already stopped.
                tracing::error!(
                    service_id,
                    runtime_key = %key,
                    error = %e,
                    "CRITICAL: deployment history could not be preserved; previous service dir left in place"
                );
                super::progress(
                    tx,
                    "cutover",
                    format!(
                        "warning: deployment history could not be preserved ({e}); \
                         the previous service dir stays in place and the new version runs as {key}"
                    ),
                )
                .await;
                self.attach_build_path(key, plan);
                return;
            }
        }

        if let Some(prior_kind) = slot.prior_runtime
            && let Err(e) = destroy_prior_runtime(prior_kind, service_id, &self.runner).await
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
            self.attach_build_path(key, plan);
            return;
        }

        // Re-key port + in-memory state to the stable service id. Dual-live
        // has no pinned ports (`is_dual_live`), so only the primary moves.
        PortAllocator::release_service(key);
        if let Err(e) = PortAllocator::claim_existing(service_id, workload.port().host) {
            tracing::warn!(service_id, error = %e, "failed to claim port under service_id after promote");
        }
        self.state.rekey_service(key, service_id);
        self.attach_build_path(service_id, plan);
    }

    /// Note the built checkout in the service log. A relaunch built nothing.
    fn attach_build_path(&self, key: &str, plan: &Plan) {
        if let Some(path) = plan.build_path() {
            self.state.attach_flake_path(key, path.to_path_buf());
        }
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
        tx: &Events,
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
                message: Some(match plan.origin {
                    PlanOrigin::Build { .. } => "deploy complete".into(),
                    PlanOrigin::Artifact(_) => "relaunched the recorded build".into(),
                }),
                desired_state: Some(desired_snap),
                artifact: Some(built.artifact.clone()),
            },
        ) {
            tracing::warn!(service_id, error = %e, "failed to append deployment history");
        }
        // Root this generation and the previous one; drop older roots (#411).
        // A failure here leaves rollback targets collectable, so say so.
        if let Err(e) = crate::gcroots::sync_blocking(service_id).await {
            tracing::warn!(service_id, error = %e, "gcroots sync failed");
            super::progress(
                tx,
                "record",
                format!("warning: Nix GC roots for this service are incomplete: {e:#}"),
            )
            .await;
        }
    }
}

/// Turn the Russelfile into the deploy's inputs. Touches no state.
fn plan(service_id: &str, request: &DeployRequest, source: ResolvedSource) -> anyhow::Result<Plan> {
    let ResolvedSource {
        origin,
        config,
        russelfile,
        rev,
    } = source;
    let runtime = config.service.runtime;
    let podman_args = &config.service.podman_args;
    validate_podman_args_for_runtime(runtime, podman_args)?;
    let origin = match origin {
        SourceOrigin::Checkout {
            repo_path,
            checkout,
        } => PlanOrigin::Build {
            build_path: super::config::resolve_build_dir(
                &repo_path,
                &request.config_path,
                &config.service.source,
            )?,
            _checkout: checkout,
        },
        SourceOrigin::Artifact(artifact) => PlanOrigin::Artifact(artifact),
    };
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
        origin,
        config,
        russelfile,
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

/// The build dir's `flake.lock` after a build, for the generation record.
/// Nix writes one for a path flake that had none, so a generated flake has
/// one too. `None` when missing, unreadable, or over
/// [`MAX_RECORDED_LOCK_BYTES`].
fn read_flake_lock(build_path: &Path) -> Option<String> {
    use std::io::Read;
    // Read at most one byte past the cap: a huge lock is never loaded whole.
    let mut lock = String::new();
    std::fs::File::open(build_path.join("flake.lock"))
        .ok()?
        .take(MAX_RECORDED_LOCK_BYTES as u64 + 1)
        .read_to_string(&mut lock)
        .ok()?;
    (lock.len() <= MAX_RECORDED_LOCK_BYTES).then_some(lock)
}

/// Whether a replace boots the new generation beside the live one
/// (dual-live) or stops the live one first (cold).
///
/// Dual-live gives the new generation its own host port while the old one
/// still holds its own, so a pinned host port (`[ingress].port` or a
/// `[[ports]]` row) cannot move over until the old one is gone. Pinned
/// services are replaced cold so the pin holds on every runtime: a container
/// or passt microVM publishes its host port at create and cannot rebind it
/// afterwards. The cold path still refuses a crashing version: the new one
/// is watched for [`WATCH_WINDOW`] in `boot`, and a crash restores the
/// previous generation from its `.bak` dirs.
///
/// A writable volume (`rw = true`, managed or bind) is replaced cold too. The
/// candidate resolves the same host path as the live generation, so dual-live
/// would run two writers on it at once. Read-only volumes are safe to share
/// and stay dual-live.
fn is_dual_live(
    prior_runtime: Option<RuntimeKind>,
    pin: Option<&PortMapping>,
    extra_ports: &[ExtraPortSpec],
    volumes: &[ResolvedVolume],
) -> bool {
    prior_runtime.is_some()
        && pin.is_none()
        && extra_ports.is_empty()
        && !volumes.iter().any(|v| v.rw)
}

/// The new generation's primary publish under `key`: the pinned host port
/// when `[ingress].port` is set, else a free one from the allocator.
fn reserve_primary(
    ports: &PortAllocator,
    key: &str,
    pin: Option<PortMapping>,
    guest: u16,
) -> anyhow::Result<PortMapping> {
    match pin {
        Some(pinned) => {
            PortAllocator::reserve(key, pinned.host)?;
            Ok(pinned)
        }
        None => Ok(PortMapping {
            host: ports.next(key)?,
            guest,
        }),
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::network::{port_test_lock, reserve_test_port};
    use std::sync::Arc;

    fn pin(host: u16) -> PortMapping {
        PortMapping { host, guest: 3000 }
    }

    const RECORDED_RUSSELFILE: &str = r#"
[service]
name = "api"
source = "."
port = 3000
memory = "256mb"
bin = "basic-http"
type = "container"

[service.env]
GREETING = "from the recorded file"
"#;

    fn recorded_artifact() -> GenerationArtifact {
        GenerationArtifact {
            store_path: "/nix/store/aaaa-basic-http".into(),
            using_package: false,
            russelfile: RECORDED_RUSSELFILE.into(),
            rev: Some("1111111111111111111111111111111111111111".into()),
            dirty: false,
            flake_lock: Some("{\"version\": 7}".into()),
            secrets: SECRETS_RESOLVED_AT_LAUNCH.into(),
        }
    }

    #[test]
    fn a_recorded_generation_plans_without_a_checkout() {
        // The repo URL points nowhere: planning a relaunch must not touch it.
        let request = DeployRequest {
            repo_url: "/nonexistent/russel-558-repo".into(),
            config_path: "Russelfile.toml".into(),
            vm_id: Some("api".into()),
            rev: None,
            force: true,
        };
        let artifact = recorded_artifact();
        let source = ResolvedSource {
            config: Russelfile::load_from_str(&artifact.russelfile).unwrap(),
            russelfile: artifact.russelfile.clone(),
            rev: None,
            origin: SourceOrigin::Artifact(artifact.clone()),
        };
        let plan = plan("api", &request, source).unwrap();
        assert!(plan.build_path().is_none());
        assert!(matches!(&plan.origin, PlanOrigin::Artifact(a) if *a == artifact));
        assert_eq!(plan.russelfile, RECORDED_RUSSELFILE);
        assert_eq!(plan.env["GREETING"], "from the recorded file");
        assert_eq!(plan.runtime, RuntimeKind::Container);
    }

    #[test]
    fn flake_lock_is_recorded_up_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_flake_lock(dir.path()), None);
        std::fs::write(dir.path().join("flake.lock"), "{\"version\": 7}").unwrap();
        assert_eq!(
            read_flake_lock(dir.path()).as_deref(),
            Some("{\"version\": 7}")
        );
        let big = "x".repeat(MAX_RECORDED_LOCK_BYTES + 1);
        std::fs::write(dir.path().join("flake.lock"), big).unwrap();
        assert_eq!(read_flake_lock(dir.path()), None);
    }

    #[test]
    fn pinned_update_is_replaced_cold_on_every_runtime() {
        for runtime in [RuntimeKind::Container, RuntimeKind::Microvm] {
            assert!(!is_dual_live(Some(runtime), Some(&pin(8081)), &[], &[]));
        }
    }

    #[test]
    fn unpinned_update_is_dual_live() {
        assert!(is_dual_live(Some(RuntimeKind::Container), None, &[], &[]));
        assert!(is_dual_live(Some(RuntimeKind::Microvm), None, &[], &[]));
    }

    #[test]
    fn first_deploy_and_extra_ports_are_cold() {
        assert!(!is_dual_live(None, None, &[], &[]));
        let extra = [ExtraPortSpec {
            host: 5432,
            guest: 5432,
        }];
        assert!(!is_dual_live(
            Some(RuntimeKind::Container),
            None,
            &extra,
            &[]
        ));
    }

    fn volumes_of(toml: &str) -> Vec<ResolvedVolume> {
        let config = Russelfile::load_from_str(&format!(
            "[service]\nname = \"vol\"\nsource = \".\"\nport = 3000\nmemory = \"256mb\"\n{toml}"
        ))
        .unwrap();
        resolve_volumes("vol", &config.volumes, &[]).unwrap()
    }

    /// #555: two generations on one writable volume are two writers.
    #[test]
    fn writable_volume_is_replaced_cold() {
        let volumes =
            volumes_of("[[volumes]]\nname = \"data\"\nguest = \"/data\"\nrw = true\nkeep = true\n");
        assert!(volumes[0].rw);
        for runtime in [RuntimeKind::Container, RuntimeKind::Microvm] {
            assert!(!is_dual_live(Some(runtime), None, &[], &volumes));
        }
        // One writable row among read-only ones is enough.
        let mixed = volumes_of(
            "[[volumes]]\nname = \"ro\"\nguest = \"/ro\"\n[[volumes]]\nname = \"rw\"\nguest = \"/rw\"\nrw = true\n",
        );
        assert!(!is_dual_live(
            Some(RuntimeKind::Container),
            None,
            &[],
            &mixed
        ));
    }

    #[test]
    fn read_only_or_no_volume_stays_dual_live() {
        let read_only = volumes_of("[[volumes]]\nname = \"cfg\"\nguest = \"/cfg\"\n");
        assert_eq!(read_only.len(), 1);
        assert!(!read_only[0].rw);
        assert!(is_dual_live(
            Some(RuntimeKind::Container),
            None,
            &[],
            &read_only
        ));
        assert!(is_dual_live(Some(RuntimeKind::Container), None, &[], &[]));
        // A first deploy has nothing to run beside, whatever the volumes.
        let writable = volumes_of("[[volumes]]\nname = \"d\"\nguest = \"/d\"\nrw = true\n");
        assert!(!is_dual_live(None, None, &[], &writable));
    }

    /// The port side of two `russel update`s of a container with
    /// `[ingress].port`: the live generation holds the pin, the cold path
    /// releases it (`vacate` → `destroy_prior_runtime`), and the new
    /// generation reserves it again under the service id.
    #[test]
    fn pinned_container_update_keeps_the_pinned_port() {
        let _lock = port_test_lock();
        let id = "pin-keep";
        let pinned = pin(reserve_test_port(id));
        let ports = PortAllocator;

        // Why dual-live cannot keep a pin: a candidate beside the live
        // generation cannot take the port the live one holds.
        let candidate = "pin-keep_gcafe";
        assert!(reserve_primary(&ports, candidate, Some(pinned.clone()), 3000).is_err());
        PortAllocator::release_service(candidate);

        for _update in 0..2 {
            assert!(!is_dual_live(
                Some(RuntimeKind::Container),
                Some(&pinned),
                &[],
                &[]
            ));
            PortAllocator::release_service(id);
            let port = reserve_primary(&ports, id, Some(pinned.clone()), 3000).unwrap();
            assert_eq!((port.host, port.guest), (pinned.host, pinned.guest));
            assert_eq!(PortAllocator::allocated_port(id), Some(pinned.host));
        }
        PortAllocator::release_service(id);
    }

    #[test]
    fn unpinned_candidate_gets_an_allocated_port() {
        let _lock = port_test_lock();
        let key = "pin-none_gbeef";
        let port = reserve_primary(&PortAllocator, key, None, 3000).unwrap();
        assert_eq!(port.guest, 3000);
        assert_eq!(PortAllocator::allocated_port(key), Some(port.host));
        PortAllocator::release_service(key);
    }

    fn history_entry() -> AppendSuccess {
        AppendSuccess {
            generation_id: Some("abcd1234".into()),
            runtime: Some(RuntimeKind::Container),
            store_path: None,
            repo_url: Some("https://example.com/repo.git".into()),
            config_path: Some("Russelfile.toml".into()),
            host_port: Some(8081),
            guest_port: Some(3000),
            message: None,
            desired_state: None,
            artifact: None,
        }
    }

    /// A service with one recorded deployment and a pending rollback marker,
    /// its dir already moved to `.bak` the way `vacate` leaves it.
    fn vacated_service(id: &str) -> Slot {
        let dirs = ServiceDirs::of(id);
        let journal = deployments::deployments_path(id);
        assert_eq!(
            deployments::append_success_at(&journal, history_entry()).unwrap(),
            1
        );
        std::fs::write(journal.with_file_name("rollback.pending"), "1").unwrap();
        std::fs::rename(&dirs.russel, &dirs.russel_bak).unwrap();
        Slot {
            prior_runtime: Some(RuntimeKind::Container),
            dual_live: false,
            generation_id: "dcba4321".into(),
            runtime_key: id.into(),
            dirs,
            has_backup: true,
        }
    }

    fn cold_plan(id: &str, dir: &str) -> Plan {
        let russelfile = format!(
            "[service]\nname = \"{id}\"\nsource = \".\"\nport = 3000\nmemory = \"256mb\"\n"
        );
        let config = Russelfile::load_from_str(&russelfile).unwrap();
        Plan {
            origin: PlanOrigin::Build {
                build_path: PathBuf::from(dir),
                _checkout: crate::git::GitClient.hold_checkout(Path::new(dir)),
            },
            config,
            russelfile,
            rev: None,
            runtime: RuntimeKind::Container,
            env: HashMap::new(),
            ingress_host: None,
            host_rules: vec![],
            pin_mapping: Some(pin(8081)),
            volumes: vec![],
            desired_state: None,
        }
    }

    /// Review of #564: when the history can go neither to the candidate nor
    /// to the kept dir, a dual-live cutover must not destroy or replace the
    /// previous service dir that still holds it.
    #[tokio::test]
    async fn dual_live_cutover_leaves_the_history_when_it_cannot_be_preserved() {
        let id = "dual-history-stuck";
        let live = deployments::deployments_path(id);
        deployments::append_success_at(&live, history_entry()).unwrap();
        std::fs::write(live.with_file_name("rollback.pending"), "1").unwrap();
        let generation = "feed4321";
        let key = format!("{id}_g{generation}");
        // Both destinations refuse: a directory where the candidate's
        // journal goes, and a file where the kept dir goes.
        std::fs::create_dir_all(
            crate::paths::service_dir(&key)
                .join(deployments::JOURNAL_FILE)
                .join("blocker"),
        )
        .unwrap();
        let kept = kept_history_dir(id, generation);
        std::fs::write(&kept, "not a dir").unwrap();

        let slot = Slot {
            prior_runtime: Some(RuntimeKind::Container),
            dual_live: true,
            generation_id: generation.into(),
            runtime_key: key.clone(),
            dirs: ServiceDirs::of(id),
            has_backup: false,
        };
        let plan = cold_plan(id, &crate::paths::service_dir(&key).display().to_string());
        let workload = DeployWorkload::Container {
            container_id: "fake".into(),
            container_name: "fake".into(),
            rootfs_path: PathBuf::new(),
            port: PortMapping {
                host: 0,
                guest: 3000,
            },
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        DeployPipeline::new(crate::state::AppState::default())
            .cutover(id, &plan, &slot, &workload, &tx)
            .await;

        // The previous service dir and its history are untouched, and the
        // candidate was not promoted over them.
        assert!(live.is_file());
        assert!(live.with_file_name("rollback.pending").exists());
        assert!(crate::paths::service_dir(&key).is_dir());
        let mut warned = false;
        while let Ok(event) = rx.try_recv() {
            if let russel_core::api::DeployEvent::Progress { description, .. } = event {
                warned |= description.contains("could not be preserved");
            }
        }
        assert!(warned, "cutover must report the unpreserved history");
        std::fs::remove_file(&kept).unwrap();
        std::fs::remove_dir_all(crate::paths::service_dir(&key)).unwrap();
        std::fs::remove_dir_all(crate::paths::service_dir(id)).unwrap();
    }

    #[tokio::test]
    async fn history_falls_back_to_the_kept_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to, kept) = (
            tmp.path().join("api"),
            tmp.path().join("api_gfeed"),
            tmp.path().join("api.history-feed.bak"),
        );
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join(deployments::JOURNAL_FILE), "{}").unwrap();
        std::fs::create_dir_all(to.join(deployments::JOURNAL_FILE).join("blocker")).unwrap();
        let moved = preserve_history(&from, &to, &kept).await.unwrap();
        assert!(matches!(moved, HistoryMove::Kept(_)));
        assert!(kept.join(deployments::JOURNAL_FILE).is_file());
        assert!(!from.join(deployments::JOURNAL_FILE).exists());
    }

    /// The journal moves, then the marker cannot: the journal goes back, so
    /// the history is never split between the two dirs.
    #[tokio::test]
    async fn history_move_is_all_or_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (from, to) = (tmp.path().join("api"), tmp.path().join("api_gfeed"));
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join(deployments::JOURNAL_FILE), "{}").unwrap();
        std::fs::write(from.join("rollback.pending"), "").unwrap();
        std::fs::create_dir_all(to.join("rollback.pending").join("blocker")).unwrap();
        assert!(carry_history(&from, &to).await.is_err());
        assert!(from.join(deployments::JOURNAL_FILE).is_file());
        assert!(from.join("rollback.pending").is_file());
        assert!(!to.join(deployments::JOURNAL_FILE).exists());
    }

    /// A failed history move must not delete the backup that still holds
    /// the history: cutover keeps it aside and says so.
    #[tokio::test]
    async fn cold_cutover_keeps_the_backup_when_history_cannot_move() {
        let id = "cold-history-stuck";
        let slot = vacated_service(id);
        // A directory where the journal should land: rename and copy fail.
        let journal = deployments::deployments_path(id);
        std::fs::create_dir_all(journal.join("blocker")).unwrap();
        let plan = cold_plan(id, &slot.dirs.russel);
        let workload = DeployWorkload::Container {
            container_id: "fake".into(),
            container_name: "fake".into(),
            rootfs_path: PathBuf::new(),
            port: pin(8081),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        DeployPipeline::new(crate::state::AppState::default())
            .cutover(id, &plan, &slot, &workload, &tx)
            .await;

        let kept = kept_history_dir(id, &slot.generation_id);
        assert!(!Path::new(&slot.dirs.russel_bak).exists());
        assert!(kept.join(deployments::JOURNAL_FILE).is_file());
        assert!(kept.join("rollback.pending").exists());
        let mut warned = false;
        while let Ok(event) = rx.try_recv() {
            if let russel_core::api::DeployEvent::Progress { description, .. } = event {
                warned |= description.contains("history could not be moved");
            }
        }
        assert!(warned, "cutover must report the failed move");
        assert!(russel_core::reserved::is_reserved_service_dir(
            kept.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::remove_dir_all(&kept).unwrap();
    }

    /// #554: the real cold `cutover` drops the `.bak` dir, which held the
    /// journal. The next `record` must continue the numbering.
    #[tokio::test]
    async fn cold_cutover_keeps_deployment_history() {
        let id = "cold-history";
        let slot = vacated_service(id);
        std::fs::create_dir_all(&slot.dirs.russel).unwrap();
        let plan = cold_plan(id, &slot.dirs.russel);
        let workload = DeployWorkload::Container {
            container_id: "fake".into(),
            container_name: "fake".into(),
            rootfs_path: PathBuf::new(),
            port: pin(8081),
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        DeployPipeline::new(crate::state::AppState::default())
            .cutover(id, &plan, &slot, &workload, &tx)
            .await;

        assert!(!Path::new(&slot.dirs.russel_bak).exists());
        let journal = deployments::deployments_path(id);
        assert!(journal.with_file_name("rollback.pending").exists());
        assert_eq!(
            deployments::append_success_at(&journal, history_entry()).unwrap(),
            2
        );
    }

    /// A failed cold boot puts the `.bak` dir back: the history is there once,
    /// untouched.
    #[tokio::test]
    async fn restoring_the_backup_brings_the_history_back() {
        let id = "cold-restore";
        let slot = vacated_service(id);
        std::fs::create_dir_all(&slot.dirs.russel).unwrap();
        slot.dirs.restore().await;

        let journal = deployments::deployments_path(id);
        assert!(journal.with_file_name("rollback.pending").exists());
        assert_eq!(
            deployments::append_success_at(&journal, history_entry()).unwrap(),
            2
        );
    }

    /// An ingress that records `register` calls and can be told to refuse them.
    #[derive(Default)]
    struct FakeIngress {
        registered: std::sync::Mutex<Vec<(String, u16, Vec<HostRule>)>>,
        refuse: bool,
    }

    #[async_trait::async_trait]
    impl crate::ingress::Ingress for FakeIngress {
        async fn register(
            &self,
            service_id: &str,
            backend: &Backend,
            host_rules: &[HostRule],
        ) -> anyhow::Result<()> {
            if self.refuse {
                anyhow::bail!(
                    "ingress host \"taken.example.com\" is already routed by service \"other\""
                );
            }
            self.registered.lock().unwrap().push((
                service_id.to_string(),
                backend.port,
                host_rules.to_vec(),
            ));
            Ok(())
        }

        async fn deregister(&self, _service_id: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn swap(
            &self,
            service_id: &str,
            backend: &Backend,
            host_rules: &[HostRule],
        ) -> anyhow::Result<()> {
            self.register(service_id, backend, host_rules).await
        }

        fn primary_host(&self, _service_id: &str) -> Option<String> {
            None
        }
    }

    fn pipeline_with(ingress: FakeIngress) -> (DeployPipeline, Arc<FakeIngress>) {
        let ingress = Arc::new(ingress);
        let mut pipeline = DeployPipeline::new(crate::state::AppState::default());
        pipeline.ingress = ingress.clone();
        (pipeline, ingress)
    }

    /// #557: after a cold boot the old generation is stopped and its dirs are
    /// in `.bak`. An ingress failure goes through the same restore as a failed
    /// boot, so the `.bak` dirs (and the history in them) come back.
    #[tokio::test]
    async fn cold_ingress_failure_restores_the_backup() {
        let id = "cold-ingress-fail";
        let mut slot = vacated_service(id);
        // Stopped before the deploy: nothing to relaunch, the dirs come back.
        slot.prior_runtime = None;
        std::fs::create_dir_all(&slot.dirs.russel).unwrap();
        let (pipeline, _) = pipeline_with(FakeIngress::default());

        let Err(error) = pipeline
            .restore_previous(id, &slot, anyhow::anyhow!("host taken"), None)
            .await
        else {
            panic!("a restore that relaunches nothing must report the error");
        };
        assert_eq!(error.to_string(), "host taken");

        assert!(!Path::new(&slot.dirs.russel_bak).exists());
        let journal = deployments::deployments_path(id);
        assert!(journal.exists());
        assert_eq!(
            deployments::append_success_at(&journal, history_entry()).unwrap(),
            2
        );
    }

    /// Dual-live never stopped the live generation: the failure is reported
    /// and nothing on disk moves.
    #[tokio::test]
    async fn dual_live_ingress_failure_leaves_the_live_dirs_alone() {
        let id = "dual-ingress-fail";
        let mut slot = vacated_service(id);
        slot.dual_live = true;
        slot.has_backup = false;
        let (pipeline, _) = pipeline_with(FakeIngress::default());

        let Err(error) = pipeline
            .restore_previous(id, &slot, anyhow::anyhow!("host taken"), None)
            .await
        else {
            panic!("a restore that relaunches nothing must report the error");
        };
        assert!(format!("{error:#}").contains("active generation left untouched"));
        assert!(Path::new(&slot.dirs.russel_bak).exists());
    }

    #[tokio::test]
    async fn restoring_the_route_registers_the_previous_backend() {
        let (pipeline, ingress) = pipeline_with(FakeIngress::default());
        let rules = vec![HostRule {
            host: "api.example.com".into(),
        }];
        pipeline
            .restore_route("api", Some((8081, rules.clone())))
            .await
            .unwrap();
        pipeline.restore_route("api", None).await.unwrap();
        assert_eq!(
            *ingress.registered.lock().unwrap(),
            vec![("api".to_string(), 8081, rules)]
        );

        let (pipeline, _) = pipeline_with(FakeIngress {
            refuse: true,
            ..FakeIngress::default()
        });
        assert!(
            pipeline
                .restore_route("api", Some((8081, vec![])))
                .await
                .is_err()
        );
    }
}
