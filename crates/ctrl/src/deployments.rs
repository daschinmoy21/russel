//! Deployment history journal (`/var/lib/russel/<id>/deployments.json`).
//!
//! ## Schema (`schema_version` = 1)
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "next_version": 3,
//!   "entries": [ /* newest-first DeploymentJournalEntry */ ]
//! }
//! ```
//!
//! On each successful deploy:
//! - previous `active` → `previous` (at most one)
//! - older `previous` → `superseded`
//! - new entry inserted at head as `active`
//! - history capped at [`MAX_HISTORY`] entries
//!
//! Explicit operator rollback marks the current `active` as `rolled_back`, then
//! relaunches a prior entry's recorded [`GenerationArtifact`] (#558), which
//! appends a new active. Rows recorded before artifacts existed, and an
//! explicit `rebuild`, redeploy from the entry's `desired_state` instead.

use std::path::{Path, PathBuf};

use russel_core::api::{DeployRequest, DeploymentRecord, DeploymentsResponse};
use russel_core::config::RuntimeKind;
use serde::{Deserialize, Serialize};

use crate::git::redact_repo_url;
use crate::metadata::{deployed_at_now, write_metadata};

pub const SCHEMA_VERSION: u32 = 1;
/// Max retained journal rows per service (newest kept).
pub const MAX_HISTORY: usize = 20;

/// Typed error for rollback target selection so the HTTP layer can match on
/// semantics instead of string-matching error messages.
#[derive(Debug)]
pub enum RollbackSelectError {
    NotFound { msg: String },
    Conflict { msg: String },
    Internal { msg: String },
}

impl std::fmt::Display for RollbackSelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { msg } | Self::Conflict { msg } | Self::Internal { msg } => {
                write!(f, "{msg}")
            }
        }
    }
}

const STATUS_ACTIVE: &str = "active";
const STATUS_PREVIOUS: &str = "previous";
const STATUS_SUPERSEDED: &str = "superseded";
const STATUS_ROLLED_BACK: &str = "rolled_back";

/// File name of the deployments journal inside a service dir.
pub(crate) const JOURNAL_FILE: &str = "deployments.json";

/// On-disk path for a service's deployments journal.
pub fn deployments_path(service_id: &str) -> PathBuf {
    crate::paths::service_dir(service_id).join(JOURNAL_FILE)
}

/// Journal path under an arbitrary base dir (tests).
#[cfg(test)]
pub fn deployments_path_in(base: &Path, service_id: &str) -> PathBuf {
    base.join(service_id).join(JOURNAL_FILE)
}

/// Snapshot of desired deploy inputs stored alongside each history row so
/// explicit rollback can redeploy without dual-live retain-N artifacts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DesiredStateSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_host: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub podman_args: Vec<String>,
    /// Commit this generation was built from (#448).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// The deployed tree had changes `rev` does not contain, so rebuilding
    /// `rev` does not reproduce it exactly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dirty: bool,
}

/// Custom `Deserialize` so both on-disk shapes of `desired_state` decode:
/// - the journal's flat `host_port` / `guest_port` (written by `Serialize`), and
/// - metadata's nested `port: { host, guest }` (written by the deploy pipeline).
///
/// The nested form is folded into the flat fields; a nested value is only used
/// when the flat field is absent so explicit flat values always win.
impl<'de> Deserialize<'de> for DesiredStateSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            repo_url: Option<String>,
            #[serde(default)]
            config_path: Option<String>,
            #[serde(default)]
            runtime: Option<RuntimeKind>,
            #[serde(default)]
            host_port: Option<u16>,
            #[serde(default)]
            guest_port: Option<u16>,
            #[serde(default)]
            ingress_host: Option<String>,
            #[serde(default)]
            port: Option<RawPort>,
            #[serde(default)]
            env: std::collections::HashMap<String, String>,
            #[serde(default)]
            podman_args: Vec<String>,
            #[serde(default)]
            rev: Option<String>,
            #[serde(default)]
            dirty: bool,
        }

        #[derive(Deserialize)]
        struct RawPort {
            #[serde(default)]
            host: Option<u16>,
            #[serde(default)]
            guest: Option<u16>,
        }

        let raw = Raw::deserialize(deserializer)?;
        let mut snap = Self {
            repo_url: raw.repo_url,
            config_path: raw.config_path,
            runtime: raw.runtime,
            host_port: raw.host_port,
            guest_port: raw.guest_port,
            ingress_host: raw.ingress_host,
            env: raw.env,
            podman_args: raw.podman_args,
            rev: raw.rev,
            dirty: raw.dirty,
        };
        if let Some(port) = raw.port {
            if snap.host_port.is_none() {
                snap.host_port = port.host;
            }
            if snap.guest_port.is_none() {
                snap.guest_port = port.guest;
            }
        }
        Ok(snap)
    }
}

impl DesiredStateSnapshot {
    /// Build from the deploy pipeline's `desired_state` JSON object plus port.
    pub fn from_desired_json(
        desired: Option<&serde_json::Value>,
        host_port: Option<u16>,
        guest_port: Option<u16>,
    ) -> Self {
        let mut snap = desired
            .and_then(|d| serde_json::from_value::<Self>(d.clone()).ok())
            .unwrap_or_default();
        // A pinned port in desired_state is the operator's requested ingress
        // pin. Only fill missing values from the live workload, whose backend
        // is allocated when nothing is pinned. Port 0 is never a valid pin
        // (reserve rejects it), so treat it as absent on both sides.
        if snap.host_port.filter(|&h| h != 0).is_none() {
            snap.host_port = host_port.filter(|&h| h != 0);
        }
        if snap.guest_port.filter(|&g| g != 0).is_none() {
            snap.guest_port = guest_port.filter(|&g| g != 0);
        }
        snap
    }

    /// Deserialize the `desired_state` blob of a `metadata.json` document.
    ///
    /// Best-effort: unrecognized/malformed content yields `Default` so callers
    /// fall back to legacy top-level metadata fields.
    pub fn from_metadata_desired_state(meta: &serde_json::Value) -> Self {
        meta.get("desired_state")
            .and_then(|v| serde_json::from_value::<Self>(v.clone()).ok())
            .unwrap_or_default()
    }

    /// `desired_state` of a `metadata.json` document, with the legacy
    /// top-level `repo_url` / `config_path` / ports / `runtime` filling gaps.
    ///
    /// The one reader for "what did the last deploy run with", shared by
    /// update and health restart. A legacy port that would fill a gap but is
    /// out of `u16` range is an error: dropping it would silently turn a
    /// recorded host:guest mapping into a freshly allocated host port.
    pub fn from_metadata_with_legacy(meta: &serde_json::Value) -> anyhow::Result<Self> {
        let str_field = |key: &str| meta.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let port_field = |key: &str| -> anyhow::Result<Option<u16>> {
            meta.get(key)
                .and_then(|v| v.as_u64())
                .map(u16::try_from)
                .transpose()
                .map_err(|e| anyhow::anyhow!("invalid {key} in metadata: {e}"))
        };
        let mut snap = Self::from_metadata_desired_state(meta);
        snap.repo_url = snap.repo_url.or_else(|| str_field("repo_url"));
        snap.config_path = snap.config_path.or_else(|| str_field("config_path"));
        if snap.host_port.is_none() {
            snap.host_port = port_field("host_port")?;
        }
        if snap.guest_port.is_none() {
            snap.guest_port = port_field("guest_port")?;
        }
        snap.runtime = snap
            .runtime
            .or_else(|| str_field("runtime").and_then(|s| s.parse().ok()));
        Ok(snap)
    }

    pub fn is_rollback_ready(&self) -> bool {
        self.repo_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty())
    }

    /// Rebuild the deploy request for update, rollback, and health restart:
    /// the recorded source only, never values resolved from the previous
    /// Russelfile or allocation (#445), pinned to the recorded commit (#448)
    /// and forced, since these redeploy on purpose. A dirty generation is not
    /// pinned: its commit may lack the Russelfile or the changes it ran with,
    /// so it rebuilds what the source holds now, as before #448. The id is a
    /// check against the file's `service.name`. `None` without a non-blank
    /// `repo_url`.
    pub fn redeploy_request(self, service_id: &str) -> Option<DeployRequest> {
        let repo_url = self.repo_url.filter(|url| !url.trim().is_empty())?;
        Some(DeployRequest {
            repo_url,
            config_path: self.config_path.unwrap_or_else(|| "Russelfile.toml".into()),
            vm_id: Some(service_id.to_string()),
            rev: self.rev.filter(|_| !self.dirty),
            force: true,
        })
    }
}

/// How `secret://` refs in a generation's `[service.env]` get their values.
/// The Russelfile keeps the refs only, so every launch of a recorded
/// generation reads the secret store as it is then.
pub const SECRETS_RESOLVED_AT_LAUNCH: &str = "resolved_at_launch";

/// Largest `flake.lock` kept with a generation. A bigger lock is left out of
/// the record: the generation still relaunches, only the lock text for an
/// exact rebuild is missing.
pub const MAX_RECORDED_LOCK_BYTES: usize = 64 * 1024;

/// What one successful deploy ran, kept so rollback can launch the same
/// thing again without the source repo or the network (#558).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationArtifact {
    /// Build output: `/nix/store/<hash>-<name>`.
    pub store_path: String,
    /// The build wrapped `service.package` (no committed flake).
    #[serde(default)]
    pub using_package: bool,
    /// The Russelfile exactly as it was deployed. Rollback parses this, so
    /// later edits to the file in the repo leave an old generation as it was.
    pub russelfile: String,
    /// Source identity: the commit, and whether the tree had changes beyond it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dirty: bool,
    /// Lock identity: the build dir's `flake.lock` after the build, when it
    /// had one and it fit in [`MAX_RECORDED_LOCK_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flake_lock: Option<String>,
    /// Secret-resolution policy, see [`SECRETS_RESOLVED_AT_LAUNCH`].
    pub secrets: String,
}

/// See [`JournalEntry::rollback_job`].
#[derive(Debug)]
pub enum RollbackJob {
    /// Launch the recorded build output again; nothing is fetched or built.
    Relaunch {
        artifact: GenerationArtifact,
        repo_url: Option<String>,
        config_path: Option<String>,
    },
    /// Build the recorded commit from source.
    Rebuild(DeployRequest),
}

/// Full on-disk journal entry (API record + desired_state blob).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_id: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeKind>,
    pub deployed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub rollback_ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_state: Option<DesiredStateSnapshot>,
    /// Recorded build output and inputs; `None` on rows from before #558.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<GenerationArtifact>,
}

impl JournalEntry {
    /// The entry's `desired_state`, with the entry's top-level source fields
    /// filling gaps (rows written before `desired_state` existed).
    pub fn recorded_source(&self) -> DesiredStateSnapshot {
        let mut snap = self.desired_state.clone().unwrap_or_default();
        snap.repo_url = snap.repo_url.or_else(|| self.repo_url.clone());
        snap.config_path = snap.config_path.or_else(|| self.config_path.clone());
        snap.runtime = snap.runtime.or(self.runtime);
        snap.host_port = snap.host_port.or(self.host_port);
        snap.guest_port = snap.guest_port.or(self.guest_port);
        snap
    }

    /// What an explicit rollback to this entry runs (#558): its recorded
    /// build output, or with `rebuild` (or on a row from before artifacts
    /// were recorded) a build of its recorded source. `None` when the entry
    /// has neither.
    pub fn rollback_job(&self, service_id: &str, rebuild: bool) -> Option<RollbackJob> {
        let source = self.recorded_source();
        match &self.artifact {
            Some(artifact) if !rebuild => Some(RollbackJob::Relaunch {
                artifact: artifact.clone(),
                repo_url: source.repo_url,
                config_path: source.config_path,
            }),
            _ => source
                .redeploy_request(service_id)
                .map(RollbackJob::Rebuild),
        }
    }

    pub fn to_record(&self) -> DeploymentRecord {
        DeploymentRecord {
            version: self.version,
            generation_id: self.generation_id.clone(),
            status: self.status.clone(),
            runtime: self.runtime,
            deployed_at: self.deployed_at.clone(),
            store_path: self.store_path.clone(),
            repo_url: self.repo_url.as_deref().map(redact_repo_url),
            config_path: self.config_path.clone(),
            host_port: self.host_port,
            guest_port: self.guest_port,
            message: self.message.clone(),
            rollback_ready: self.rollback_ready,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalFile {
    schema_version: u32,
    next_version: u32,
    entries: Vec<JournalEntry>,
}

impl Default for JournalFile {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            next_version: 1,
            entries: Vec::new(),
        }
    }
}

/// Inputs for recording a successful deploy.
#[derive(Debug, Clone)]
pub struct AppendSuccess {
    pub generation_id: Option<String>,
    pub runtime: Option<RuntimeKind>,
    pub store_path: Option<String>,
    pub repo_url: Option<String>,
    pub config_path: Option<String>,
    pub host_port: Option<u16>,
    pub guest_port: Option<u16>,
    pub message: Option<String>,
    pub desired_state: Option<DesiredStateSnapshot>,
    pub artifact: Option<GenerationArtifact>,
}

fn load_journal(path: &Path) -> anyhow::Result<JournalFile> {
    if !path.exists() {
        return Ok(JournalFile::default());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read deployments journal {}: {e}", path.display()))?;
    let mut journal: JournalFile = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("parse deployments journal {}: {e}", path.display()))?;
    if journal.schema_version == 0 {
        journal.schema_version = SCHEMA_VERSION;
    }
    if journal.next_version == 0 {
        journal.next_version = journal
            .entries
            .iter()
            .map(|e| e.version)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
    }
    Ok(journal)
}

fn save_journal(path: &Path, journal: &JournalFile) -> anyhow::Result<()> {
    // Reuse atomic-ish write helper (pretty JSON, create parents).
    let value = serde_json::to_value(journal)
        .map_err(|e| anyhow::anyhow!("serialize deployments journal: {e}"))?;
    write_metadata(path, &value)
}

/// Demote current active entries.
///
/// When `as_rolled_back` is true (explicit operator rollback completed), active
/// becomes `rolled_back`. Otherwise active becomes `previous`, and any prior
/// `previous` becomes `superseded` (at most one previous retained as such).
fn demote_active_entries(entries: &mut [JournalEntry], as_rolled_back: bool) {
    let demote_to = if as_rolled_back {
        STATUS_ROLLED_BACK
    } else {
        STATUS_PREVIOUS
    };
    for entry in entries.iter_mut() {
        if entry.status == STATUS_ACTIVE {
            entry.status = demote_to.to_string();
            recompute_rollback_ready(entry);
        }
    }
    // At most one `previous`; older previous → superseded.
    let mut saw_previous = false;
    for entry in entries.iter_mut() {
        if entry.status == STATUS_PREVIOUS {
            if saw_previous {
                entry.status = STATUS_SUPERSEDED.to_string();
            } else {
                saw_previous = true;
            }
        }
    }
}

fn cap_history(entries: &mut Vec<JournalEntry>) {
    if entries.len() <= MAX_HISTORY {
        return;
    }
    entries.truncate(MAX_HISTORY);
}

fn recompute_rollback_ready(entry: &mut JournalEntry) {
    let ready = entry.artifact.is_some()
        || entry
            .desired_state
            .as_ref()
            .is_some_and(DesiredStateSnapshot::is_rollback_ready)
        || entry
            .repo_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty());
    entry.rollback_ready = ready && entry.status != STATUS_ACTIVE;
}

/// Store paths of the journal's `active` and `previous` generations, and of
/// the newest `rolled_back` one, so rolling forward again after a rollback
/// finds its build (#558): the retention window that [`crate::gcroots`]
/// keeps rooted.
pub fn retained_store_paths(service_id: &str) -> anyhow::Result<Vec<String>> {
    retained_store_paths_at(&deployments_path(service_id))
}

pub fn retained_store_paths_at(path: &Path) -> anyhow::Result<Vec<String>> {
    let entries = load_journal(path)?.entries;
    let newest_rolled_back = entries
        .iter()
        .find(|e| e.status == STATUS_ROLLED_BACK)
        .map(|e| e.version);
    Ok(entries
        .into_iter()
        .filter(|e| {
            e.status == STATUS_ACTIVE
                || e.status == STATUS_PREVIOUS
                || Some(e.version) == newest_rolled_back
        })
        .filter_map(|e| e.store_path)
        .collect())
}

/// Append a successful deployment to the journal under the default base path.
pub fn append_success(service_id: &str, info: AppendSuccess) -> anyhow::Result<u32> {
    append_success_at(&deployments_path(service_id), info)
}

/// Append a successful deployment to a specific journal path.
pub fn append_success_at(path: &Path, info: AppendSuccess) -> anyhow::Result<u32> {
    let mut journal = load_journal(path)?;
    let as_rolled_back = take_pending_rollback(path);
    demote_active_entries(&mut journal.entries, as_rolled_back);

    let version = journal.next_version;
    journal.next_version = journal.next_version.saturating_add(1);

    let mut desired = info.desired_state;
    if let Some(ds) = desired.as_mut() {
        ds.repo_url = ds.repo_url.as_deref().map(redact_repo_url);
    }
    let repo_url = info
        .repo_url
        .or_else(|| desired.as_ref().and_then(|d| d.repo_url.clone()))
        .map(|u| redact_repo_url(&u));
    let config_path = info
        .config_path
        .or_else(|| desired.as_ref().and_then(|d| d.config_path.clone()));
    let host_port = info
        .host_port
        .or_else(|| desired.as_ref().and_then(|d| d.host_port));
    let guest_port = info
        .guest_port
        .or_else(|| desired.as_ref().and_then(|d| d.guest_port));
    let runtime = info
        .runtime
        .or_else(|| desired.as_ref().and_then(|d| d.runtime));

    let message = if as_rolled_back {
        Some(
            info.message
                .unwrap_or_else(|| "rollback redeploy complete".into()),
        )
    } else {
        info.message
    };

    let mut entry = JournalEntry {
        version,
        generation_id: info.generation_id,
        status: STATUS_ACTIVE.to_string(),
        runtime,
        deployed_at: deployed_at_now(),
        store_path: info.store_path,
        repo_url,
        config_path,
        host_port,
        guest_port,
        message,
        rollback_ready: false,
        desired_state: desired,
        artifact: info.artifact,
    };
    recompute_rollback_ready(&mut entry);

    journal.entries.insert(0, entry);
    cap_history(&mut journal.entries);
    save_journal(path, &journal)?;
    Ok(version)
}

/// Path for a pending explicit-rollback marker (consumed on next successful append).
fn rollback_pending_path_for_journal(journal_path: &Path) -> PathBuf {
    journal_path.with_file_name("rollback.pending")
}

/// Record that the next successful deploy for this service is an explicit
/// operator rollback. On `append_success`, the current `active` entry is
/// demoted to `rolled_back` instead of `previous`.
pub fn note_pending_rollback(service_id: &str, from_version: u32) -> anyhow::Result<()> {
    note_pending_rollback_at(&deployments_path(service_id), from_version)
}

pub fn note_pending_rollback_at(journal_path: &Path, from_version: u32) -> anyhow::Result<()> {
    let path = rollback_pending_path_for_journal(journal_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            anyhow::anyhow!(
                "create parent for rollback.pending {}: {e}",
                parent.display()
            )
        })?;
    }
    std::fs::write(&path, format!("{from_version}\n"))
        .map_err(|e| anyhow::anyhow!("write rollback.pending {}: {e}", path.display()))?;
    Ok(())
}

fn take_pending_rollback(journal_path: &Path) -> bool {
    let path = rollback_pending_path_for_journal(journal_path);
    if !path.exists() {
        return false;
    }
    let _ = std::fs::remove_file(&path);
    true
}

/// Drop a pending rollback marker without applying it (failed redeploy).
pub fn clear_pending_rollback(service_id: &str) {
    let path = rollback_pending_path_for_journal(&deployments_path(service_id));
    let _ = std::fs::remove_file(path);
}

/// List deployments for a service (newest first). Missing journal → empty list.
pub fn list(service_id: &str) -> anyhow::Result<DeploymentsResponse> {
    list_at(&deployments_path(service_id), service_id)
}

pub fn list_at(path: &Path, service_id: &str) -> anyhow::Result<DeploymentsResponse> {
    let journal = load_journal(path)?;
    let active_version = journal
        .entries
        .iter()
        .find(|e| e.status == STATUS_ACTIVE)
        .map(|e| e.version);
    Ok(DeploymentsResponse {
        service_id: service_id.to_string(),
        active_version,
        deployments: journal
            .entries
            .iter()
            .map(JournalEntry::to_record)
            .collect(),
    })
}

/// Pick the rollback target: explicit version, or latest `previous` with
/// `rollback_ready`.
pub fn select_rollback_target(
    service_id: &str,
    version: Option<u32>,
) -> Result<JournalEntry, RollbackSelectError> {
    select_rollback_target_at(&deployments_path(service_id), version)
}

pub fn select_rollback_target_at(
    path: &Path,
    version: Option<u32>,
) -> Result<JournalEntry, RollbackSelectError> {
    let journal =
        load_journal(path).map_err(|e| RollbackSelectError::Internal { msg: e.to_string() })?;
    if journal.entries.is_empty() {
        return Err(RollbackSelectError::NotFound {
            msg: "no deployment history for this service".into(),
        });
    }

    match version {
        Some(v) => {
            let entry = journal
                .entries
                .into_iter()
                .find(|e| e.version == v)
                .ok_or_else(|| RollbackSelectError::NotFound {
                    msg: format!("deployment version {v} not found in history"),
                })?;
            if entry.status == STATUS_ACTIVE {
                return Err(RollbackSelectError::Conflict {
                    msg: format!("version {v} is already active"),
                });
            }
            if !entry_is_rollbackable(&entry) {
                return Err(RollbackSelectError::Conflict {
                    msg: format!(
                        "version {v} is not rollback-ready: no recorded repo_url/desired_state \
                         (previous generation not retained; redeploy required — full retain-N=2 cutover TBD)"
                    ),
                });
            }
            Ok(entry)
        }
        None => {
            // Prefer status=previous + rollback_ready; then any non-active rollbackable row.
            if let Some(entry) = journal
                .entries
                .iter()
                .find(|e| e.status == STATUS_PREVIOUS && e.rollback_ready)
            {
                return Ok(entry.clone());
            }
            journal
                .entries
                .into_iter()
                .find(|e| e.status != STATUS_ACTIVE && entry_is_rollbackable(e))
                .ok_or_else(|| RollbackSelectError::Conflict {
                    msg: "no previous rollback-ready deployment; redeploy required — \
                          full retain-N=2 cutover TBD"
                        .into(),
                })
        }
    }
}

fn entry_is_rollbackable(entry: &JournalEntry) -> bool {
    entry.rollback_ready
        || entry.artifact.is_some()
        || entry
            .desired_state
            .as_ref()
            .is_some_and(DesiredStateSnapshot::is_rollback_ready)
        || entry
            .repo_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_desired(repo: &str) -> DesiredStateSnapshot {
        DesiredStateSnapshot {
            repo_url: Some(repo.into()),
            config_path: Some("Russelfile.toml".into()),
            runtime: Some(RuntimeKind::Container),
            host_port: Some(8080),
            guest_port: Some(3000),
            ingress_host: Some("app.example.com".into()),
            env: Default::default(),
            podman_args: vec![],
            ..Default::default()
        }
    }

    fn sample_artifact(generation: &str) -> GenerationArtifact {
        GenerationArtifact {
            store_path: format!("/nix/store/{generation}"),
            using_package: false,
            russelfile: "[service]\nname = \"api\"\n".into(),
            rev: Some("1111111111111111111111111111111111111111".into()),
            dirty: false,
            flake_lock: None,
            secrets: SECRETS_RESOLVED_AT_LAUNCH.into(),
        }
    }

    #[test]
    fn rollback_relaunches_the_recorded_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        append(&path, "https://example.com/r.git", "aaaa1111");
        append(&path, "https://example.com/r.git", "bbbb2222");

        let target = select_rollback_target_at(&path, None).unwrap();
        assert_eq!(target.version, 1);
        match target.rollback_job("api", false).unwrap() {
            RollbackJob::Relaunch {
                artifact,
                repo_url,
                config_path,
            } => {
                assert_eq!(artifact, sample_artifact("aaaa1111"));
                assert_eq!(repo_url.as_deref(), Some("https://example.com/r.git"));
                assert_eq!(config_path.as_deref(), Some("Russelfile.toml"));
            }
            other => panic!("expected a relaunch, got {other:?}"),
        }
        // An explicit rebuild builds the recorded source instead.
        match target.rollback_job("api", true).unwrap() {
            RollbackJob::Rebuild(request) => {
                assert_eq!(request.repo_url, "https://example.com/r.git");
                assert!(request.force);
            }
            other => panic!("expected a rebuild, got {other:?}"),
        }
    }

    #[test]
    fn a_relaunch_records_the_same_artifact_again() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        append(&path, "https://example.com/r.git", "aaaa1111");
        append(&path, "https://example.com/r.git", "bbbb2222");
        let target = select_rollback_target_at(&path, None).unwrap();
        let Some(RollbackJob::Relaunch { artifact, .. }) = target.rollback_job("api", false) else {
            panic!("expected a relaunch");
        };
        note_pending_rollback_at(&path, target.version).unwrap();
        // What `record` appends after a relaunch: the recorded artifact.
        let v3 = append_success_at(
            &path,
            AppendSuccess {
                generation_id: Some("cccc3333".into()),
                runtime: Some(RuntimeKind::Container),
                store_path: Some(artifact.store_path.clone()),
                repo_url: Some("https://example.com/r.git".into()),
                config_path: Some("Russelfile.toml".into()),
                host_port: Some(8080),
                guest_port: Some(3000),
                message: Some("relaunched the recorded build".into()),
                desired_state: Some(sample_desired("https://example.com/r.git")),
                artifact: Some(artifact.clone()),
            },
        )
        .unwrap();
        assert_eq!(v3, 3);

        let listed = list_at(&path, "api").unwrap();
        let statuses: Vec<_> = listed
            .deployments
            .iter()
            .map(|d| (d.version, d.status.as_str()))
            .collect();
        assert_eq!(
            statuses,
            [(3, "active"), (2, "rolled_back"), (1, "previous")]
        );
        // Rolling forward again relaunches version 2's own build.
        let back = select_rollback_target_at(&path, Some(2)).unwrap();
        let Some(RollbackJob::Relaunch { artifact: v2, .. }) = back.rollback_job("api", false)
        else {
            panic!("expected a relaunch");
        };
        assert_eq!(v2.store_path, "/nix/store/bbbb2222");
        // The build rolled away from stays rooted next to the active one.
        let retained = retained_store_paths_at(&path).unwrap();
        assert!(retained.contains(&"/nix/store/aaaa1111".to_string()));
        assert!(retained.contains(&"/nix/store/bbbb2222".to_string()));
    }

    #[test]
    fn rows_from_before_artifacts_rebuild_from_source() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"schema_version":1,"next_version":3,"entries":[
              {"version":2,"status":"active","deployed_at":"2026-09-30T10:00:00Z",
               "store_path":"/nix/store/bbbb","repo_url":"https://example.com/r.git",
               "config_path":"Russelfile.toml","rollback_ready":false},
              {"version":1,"status":"previous","deployed_at":"2026-09-29T10:00:00Z",
               "store_path":"/nix/store/aaaa","repo_url":"https://example.com/r.git",
               "config_path":"Russelfile.toml","rollback_ready":true,
               "desired_state":{"rev":"1111111111111111111111111111111111111111"}}]}"#,
        )
        .unwrap();
        let target = select_rollback_target_at(&path, None).unwrap();
        assert!(target.artifact.is_none());
        match target.rollback_job("api", false).unwrap() {
            RollbackJob::Rebuild(request) => assert_eq!(
                request.rev.as_deref(),
                Some("1111111111111111111111111111111111111111")
            ),
            other => panic!("expected a rebuild, got {other:?}"),
        }
    }

    fn append(path: &Path, repo: &str, generation: &str) -> u32 {
        append_success_at(
            path,
            AppendSuccess {
                generation_id: Some(generation.into()),
                runtime: Some(RuntimeKind::Container),
                store_path: Some(format!("/nix/store/{generation}")),
                repo_url: Some(repo.into()),
                config_path: Some("Russelfile.toml".into()),
                host_port: Some(8080),
                guest_port: Some(3000),
                message: Some("deploy complete".into()),
                desired_state: Some(sample_desired(repo)),
                artifact: Some(sample_artifact(generation)),
            },
        )
        .unwrap()
    }

    #[test]
    fn append_promotes_previous_and_supersedes_older() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");

        let v1 = append(&path, "https://example.com/r1.git", "aaaa1111");
        let v2 = append(&path, "https://example.com/r2.git", "bbbb2222");
        let v3 = append(&path, "https://example.com/r3.git", "cccc3333");
        assert_eq!((v1, v2, v3), (1, 2, 3));

        let list = list_at(&path, "api").unwrap();
        assert_eq!(list.active_version, Some(3));
        assert_eq!(list.deployments.len(), 3);
        assert_eq!(list.deployments[0].status, "active");
        assert_eq!(list.deployments[0].version, 3);
        assert_eq!(list.deployments[1].status, "previous");
        assert_eq!(list.deployments[1].version, 2);
        assert!(list.deployments[1].rollback_ready);
        assert_eq!(list.deployments[2].status, "superseded");
        assert_eq!(list.deployments[2].version, 1);
    }

    #[test]
    fn retained_store_paths_are_active_and_previous_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        assert!(retained_store_paths_at(&path).unwrap().is_empty());

        append(&path, "https://example.com/r1.git", "aaaa1111");
        append(&path, "https://example.com/r2.git", "bbbb2222");
        append(&path, "https://example.com/r3.git", "cccc3333");

        // v1 is superseded: it leaves the retention window and loses its root.
        assert_eq!(
            retained_store_paths_at(&path).unwrap(),
            ["/nix/store/cccc3333", "/nix/store/bbbb2222"]
        );
    }

    #[test]
    fn history_capped_at_max() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");

        for i in 0..(MAX_HISTORY + 5) {
            append(
                &path,
                &format!("https://example.com/r{i}.git"),
                &format!("{i:08x}"),
            );
        }

        let list = list_at(&path, "api").unwrap();
        assert_eq!(list.deployments.len(), MAX_HISTORY);
        assert_eq!(list.active_version, Some((MAX_HISTORY + 5) as u32));
        // Newest kept
        assert_eq!(list.deployments[0].version, (MAX_HISTORY + 5) as u32);
        // Oldest of the retained set
        let oldest = list.deployments.last().unwrap().version;
        assert_eq!(oldest, 6); // 25 total → keep 6..=25
    }

    #[test]
    fn select_rollback_defaults_to_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        append(&path, "https://example.com/r1.git", "aaaa1111");
        append(&path, "https://example.com/r2.git", "bbbb2222");

        let target = select_rollback_target_at(&path, None).unwrap();
        assert_eq!(target.version, 1);
        assert_eq!(target.status, "previous");
        assert!(target.rollback_ready);
    }

    #[test]
    fn select_rollback_by_version_and_reject_active() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        append(&path, "https://example.com/r1.git", "aaaa1111");
        append(&path, "https://example.com/r2.git", "bbbb2222");

        let target = select_rollback_target_at(&path, Some(1)).unwrap();
        assert_eq!(target.version, 1);

        let err = select_rollback_target_at(&path, Some(2)).unwrap_err();
        assert!(err.to_string().contains("already active"));

        let err = select_rollback_target_at(&path, Some(99)).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn pending_rollback_demotes_active_to_rolled_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        append(&path, "https://example.com/r1.git", "aaaa1111");
        append(&path, "https://example.com/r2.git", "bbbb2222");

        note_pending_rollback_at(&path, 1).unwrap();
        // Rollback redeploy creates new active from v1 desired state
        let v3 = append(&path, "https://example.com/r1.git", "dddd4444");
        assert_eq!(v3, 3);
        let list = list_at(&path, "api").unwrap();
        assert_eq!(list.active_version, Some(3));
        assert_eq!(list.deployments[0].status, "active");
        assert!(
            list.deployments
                .iter()
                .any(|d| d.version == 2 && d.status == "rolled_back"),
            "prior active should be rolled_back after pending-rollback append"
        );
        // pending marker consumed
        assert!(!rollback_pending_path_for_journal(&path).exists());
    }

    #[test]
    fn empty_journal_lists_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "missing");
        let list = list_at(&path, "missing").unwrap();
        assert!(list.deployments.is_empty());
        assert!(list.active_version.is_none());
    }

    #[test]
    fn desired_state_from_json() {
        let json = serde_json::json!({
            "repo_url": "https://example.com/app.git",
            "config_path": "Russelfile.toml",
            "runtime": "microvm",
            "env": {"LOG_LEVEL": "debug"},
            "port": {"host": 9000, "guest": 3000},
            "ingress_host": "ABC.example.com"
        });
        let snap = DesiredStateSnapshot::from_desired_json(Some(&json), None, None);
        assert_eq!(
            snap.repo_url.as_deref(),
            Some("https://example.com/app.git")
        );
        assert_eq!(snap.runtime, Some(RuntimeKind::Microvm));
        assert_eq!(snap.host_port, Some(9000));
        assert_eq!(snap.ingress_host.as_deref(), Some("ABC.example.com"));
        assert_eq!(snap.env.get("LOG_LEVEL").map(String::as_str), Some("debug"));
        assert!(snap.is_rollback_ready());
    }

    #[test]
    fn desired_state_pin_wins_over_live_backend() {
        let json = serde_json::json!({
            "port": {"host": 4000, "guest": 3000},
            "ingress_host": "abc.com"
        });
        let snap = DesiredStateSnapshot::from_desired_json(Some(&json), Some(3107), Some(3000));
        assert_eq!(snap.host_port, Some(4000));
        assert_eq!(snap.guest_port, Some(3000));
        assert_eq!(snap.ingress_host.as_deref(), Some("abc.com"));
    }

    /// A pinned deploy records the pin in desired_state and in the row;
    /// rolling back to it (after a later deploy) replays the pin.
    #[test]
    fn rollback_target_keeps_the_ingress_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        let pinned = serde_json::json!({
            "repo_url": "https://example.com/app.git",
            "config_path": "Russelfile.toml",
            "runtime": "container",
            "port": {"host": 8081, "guest": 3000}
        });
        for generation in ["aaaa1111", "bbbb2222"] {
            append_success_at(
                &path,
                AppendSuccess {
                    generation_id: Some(generation.into()),
                    runtime: Some(RuntimeKind::Container),
                    store_path: Some(format!("/nix/store/{generation}")),
                    repo_url: Some("https://example.com/app.git".into()),
                    config_path: Some("Russelfile.toml".into()),
                    host_port: Some(8081),
                    guest_port: Some(3000),
                    message: Some("deploy complete".into()),
                    desired_state: Some(DesiredStateSnapshot::from_desired_json(
                        Some(&pinned),
                        Some(8081),
                        Some(3000),
                    )),
                    artifact: None,
                },
            )
            .unwrap();
        }
        let target = select_rollback_target_at(&path, None).unwrap();
        assert_eq!(target.generation_id.as_deref(), Some("aaaa1111"));
        assert_eq!(target.host_port, Some(8081));
        let source = target.recorded_source();
        assert_eq!(
            (source.host_port, source.guest_port),
            (Some(8081), Some(3000))
        );
        assert!(source.redeploy_request("api").is_some());
    }

    /// Health restart reads metadata. A service updated before pinned
    /// updates went cold was left on an allocated port; the pin in
    /// desired_state still wins, so the restart goes back to it.
    #[test]
    fn health_restart_source_keeps_the_pin_over_a_drifted_port() {
        let meta = serde_json::json!({
            "repo_url": "https://example.com/app.git",
            "config_path": "Russelfile.toml",
            "runtime": "container",
            "host_port": 3107,
            "guest_port": 3000,
            "desired_state": {
                "repo_url": "https://example.com/app.git",
                "runtime": "container",
                "port": {"host": 8081, "guest": 3000}
            }
        });
        let snap = DesiredStateSnapshot::from_metadata_with_legacy(&meta).unwrap();
        assert_eq!((snap.host_port, snap.guest_port), (Some(8081), Some(3000)));
    }

    #[test]
    fn desired_state_zero_pin_falls_back_to_live_backend() {
        let json = serde_json::json!({
            "port": {"host": 0, "guest": 0},
        });
        let snap = DesiredStateSnapshot::from_desired_json(Some(&json), Some(3107), Some(3000));
        assert_eq!(snap.host_port, Some(3107));
        assert_eq!(snap.guest_port, Some(3000));
    }

    #[test]
    fn from_metadata_desired_state_parses_nested_port() {
        let meta = serde_json::json!({
            "service_id": "api",
            "desired_state": {
                "repo_url": "https://example.com/app.git",
                "config_path": "Russelfile.toml",
                "runtime": "container",
                "env": {"LOG_LEVEL": "info"},
                "podman_args": ["--network", "bridge"],
                "port": {"host": 9000, "guest": 3000},
                "ingress_host": "abc.com"
            }
        });
        let snap = DesiredStateSnapshot::from_metadata_desired_state(&meta);
        assert_eq!(
            snap.repo_url.as_deref(),
            Some("https://example.com/app.git")
        );
        assert_eq!(snap.runtime, Some(RuntimeKind::Container));
        assert_eq!(snap.host_port, Some(9000));
        assert_eq!(snap.guest_port, Some(3000));
        assert_eq!(snap.ingress_host.as_deref(), Some("abc.com"));
        assert_eq!(snap.env.get("LOG_LEVEL").map(String::as_str), Some("info"));
        assert_eq!(snap.podman_args, vec!["--network", "bridge"]);
    }

    #[test]
    fn metadata_with_legacy_prefers_desired_state() {
        let meta = serde_json::json!({
            "repo_url": "https://example.com/legacy.git",
            "config_path": "legacy/Russelfile.toml",
            "runtime": "microvm",
            "host_port": 7000,
            "guest_port": 7001,
            "desired_state": {
                "repo_url": "https://example.com/app.git",
                "runtime": "container",
                "port": {"host": 9000, "guest": 4000}
            }
        });
        let snap = DesiredStateSnapshot::from_metadata_with_legacy(&meta).unwrap();
        assert_eq!(
            snap.repo_url.as_deref(),
            Some("https://example.com/app.git")
        );
        // Gap in desired_state: legacy value fills it.
        assert_eq!(snap.config_path.as_deref(), Some("legacy/Russelfile.toml"));
        assert_eq!(snap.runtime, Some(RuntimeKind::Container));
        assert_eq!((snap.host_port, snap.guest_port), (Some(9000), Some(4000)));
    }

    #[test]
    fn metadata_with_legacy_reads_top_level_only_metadata() {
        let meta = serde_json::json!({
            "repo_url": "https://example.com/app.git",
            "runtime": "container",
            "host_port": 8080,
            "guest_port": 4000
        });
        let snap = DesiredStateSnapshot::from_metadata_with_legacy(&meta).unwrap();
        assert_eq!(
            snap.repo_url.as_deref(),
            Some("https://example.com/app.git")
        );
        assert_eq!(snap.runtime, Some(RuntimeKind::Container));
        assert_eq!((snap.host_port, snap.guest_port), (Some(8080), Some(4000)));
    }

    #[test]
    fn metadata_with_legacy_rejects_out_of_range_port() {
        // Dropping 70000 would redeploy without the recorded mapping and move
        // the service to a newly allocated host port.
        let meta = serde_json::json!({
            "repo_url": "https://example.com/app.git",
            "host_port": 8080,
            "guest_port": 70000
        });
        let err = DesiredStateSnapshot::from_metadata_with_legacy(&meta).unwrap_err();
        assert!(err.to_string().contains("guest_port"), "{err}");

        // A bad legacy value that desired_state already covers is never read.
        let meta = serde_json::json!({
            "repo_url": "https://example.com/app.git",
            "host_port": 8080,
            "guest_port": 70000,
            "desired_state": {"port": {"host": 8080, "guest": 4000}}
        });
        let snap = DesiredStateSnapshot::from_metadata_with_legacy(&meta).unwrap();
        assert_eq!(snap.guest_port, Some(4000));
    }

    /// #445: update/rollback re-read the Russelfile; recorded env, ports,
    /// host, runtime, and podman args from the last deploy are not replayed.
    #[test]
    fn redeploy_request_carries_only_the_recorded_source() {
        let mut env = std::collections::HashMap::new();
        env.insert("LOG_LEVEL".to_string(), "debug".to_string());
        let snap = DesiredStateSnapshot {
            repo_url: Some("https://example.com/app.git".into()),
            config_path: Some("svc/Russelfile.toml".into()),
            runtime: Some(RuntimeKind::Container),
            host_port: Some(8080),
            guest_port: Some(3000),
            ingress_host: Some("abc.com".into()),
            env,
            podman_args: vec!["--network".into(), "bridge".into()],
            rev: Some("0123456789abcdef0123456789abcdef01234567".into()),
            dirty: false,
        };
        let req = snap.redeploy_request("api").unwrap();
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "repo_url": "https://example.com/app.git",
                "config_path": "svc/Russelfile.toml",
                "vm_id": "api",
                "rev": "0123456789abcdef0123456789abcdef01234567",
                "force": true,
            })
        );
    }

    /// A dirty generation's commit may not hold its Russelfile (untracked) or
    /// its changes, so update and health restart must not pin to it.
    #[test]
    fn redeploy_request_does_not_pin_a_dirty_generation() {
        let snap = DesiredStateSnapshot {
            repo_url: Some("/srv/app".into()),
            rev: Some("0123456789abcdef0123456789abcdef01234567".into()),
            dirty: true,
            ..Default::default()
        };
        let req = snap.redeploy_request("api").unwrap();
        assert_eq!(req.rev, None);
        assert!(req.force);
    }

    #[test]
    fn redeploy_request_requires_repo_url() {
        assert!(
            DesiredStateSnapshot::default()
                .redeploy_request("api")
                .is_none()
        );
        let blank = DesiredStateSnapshot {
            repo_url: Some("  ".into()),
            ..Default::default()
        };
        assert!(blank.redeploy_request("api").is_none());
    }

    #[test]
    fn journal_recorded_source_falls_back_to_top_level() {
        let entry = JournalEntry {
            version: 1,
            generation_id: None,
            status: "previous".into(),
            runtime: Some(RuntimeKind::Container),
            deployed_at: "2026-07-28T09:00:00Z".into(),
            store_path: None,
            repo_url: Some("https://example.com/app.git".into()),
            config_path: Some("svc/Russelfile.toml".into()),
            host_port: Some(8080),
            guest_port: None,
            message: None,
            rollback_ready: true,
            desired_state: None,
            artifact: None,
        };
        let req = entry.recorded_source().redeploy_request("api").unwrap();
        assert_eq!(req.repo_url, "https://example.com/app.git");
        assert_eq!(req.config_path, "svc/Russelfile.toml");
        assert_eq!(req.vm_id.as_deref(), Some("api"));
    }

    #[test]
    fn from_metadata_desired_state_malformed_is_default() {
        let meta = serde_json::json!({ "desired_state": "not-an-object" });
        let snap = DesiredStateSnapshot::from_metadata_desired_state(&meta);
        assert!(snap.repo_url.is_none());
        assert!(snap.host_port.is_none());
        assert!(snap.env.is_empty());
        assert!(snap.podman_args.is_empty());
    }

    #[test]
    fn desired_state_deserializes_flat_ports() {
        // Journal on-disk form serializes flat host_port/guest_port; ensure the
        // custom Deserialize still reads them (regression for load_journal).
        let json = serde_json::json!({
            "repo_url": "https://example.com/app.git",
            "host_port": 8080,
            "guest_port": 3000,
            "ingress_host": "abc.com"
        });
        let snap: DesiredStateSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(snap.host_port, Some(8080));
        assert_eq!(snap.guest_port, Some(3000));
        assert_eq!(snap.ingress_host.as_deref(), Some("abc.com"));
    }

    #[test]
    fn append_success_strips_http_userinfo() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        append(&path, "https://user:token@example.com/app.git", "aaaa1111");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("token"), "{raw}");
        assert!(!raw.contains("user:"), "{raw}");
        assert!(raw.contains("https://example.com/app.git"), "{raw}");

        let list = list_at(&path, "api").unwrap();
        assert_eq!(
            list.deployments[0].repo_url.as_deref(),
            Some("https://example.com/app.git")
        );
    }

    #[test]
    fn list_redacts_legacy_journal_userinfo() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
              "schema_version": 1,
              "next_version": 2,
              "entries": [{
                "version": 1,
                "status": "active",
                "deployed_at": "2026-01-01T00:00:00Z",
                "repo_url": "https://user:token@example.com/app.git",
                "rollback_ready": false
              }]
            }"#,
        )
        .unwrap();
        let list = list_at(&path, "api").unwrap();
        assert_eq!(
            list.deployments[0].repo_url.as_deref(),
            Some("https://example.com/app.git")
        );
    }

    #[test]
    fn select_rollback_no_previous_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = deployments_path_in(tmp.path(), "api");
        append(&path, "https://example.com/r1.git", "aaaa1111");
        // Only active, no previous
        let err = select_rollback_target_at(&path, None).unwrap_err();
        assert!(err.to_string().contains("no previous rollback-ready"));
    }
}
