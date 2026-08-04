use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use super::lease::active_checkouts;

/// Claim an unleased checkout and delete it while holding the active-checkout
/// mutex. A new lease therefore cannot appear between the eligibility check
/// and the deletion.
fn remove_unleased_checkout(path: &Path) -> std::io::Result<bool> {
    let mut active = active_checkouts();
    let state = active.entry(path.to_path_buf()).or_default();
    if state.leases != 0 || state.deleting {
        return Ok(false);
    }
    state.deleting = true;
    let result = fs::remove_dir_all(path);
    active.remove(path);
    result.map(|()| true)
}

/// Remove checkout directories older than `max_age`.
///
/// Filesystem errors on individual entries are logged and skipped so one
/// bad directory does not abort GC of the rest.
pub fn gc_old_checkouts(checkout_root: &Path, max_age: Duration) -> anyhow::Result<usize> {
    if !checkout_root.exists() {
        return Ok(0);
    }
    let now = SystemTime::now();
    let mut removed = 0usize;
    let entries = match fs::read_dir(checkout_root) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read checkout root for GC");
            return Ok(0);
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "skip unreadable checkout entry during GC");
                continue;
            }
        };
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "skip checkout with unreadable metadata during GC"
                );
                continue;
            }
        };
        if !meta.is_dir() {
            continue;
        }
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "skip checkout with unknown mtime during GC"
                );
                continue;
            }
        };
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        if age > max_age {
            match remove_unleased_checkout(&entry.path()) {
                Ok(true) => {
                    tracing::info!(path = %entry.path().display(), "GC removed old git checkout");
                    removed += 1;
                }
                Ok(false) => {
                    tracing::debug!(path = %entry.path().display(), "skip active checkout during GC");
                }
                Err(e) => {
                    tracing::warn!(
                        path = %entry.path().display(),
                        error = %e,
                        "failed to GC git checkout"
                    );
                }
            }
        }
    }
    Ok(removed)
}
