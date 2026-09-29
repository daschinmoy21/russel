//! Host state roots. `RUSSEL_DATA_DIR` overrides the default `/var/lib/russel`.
//!
//! Test harnesses call [`pin_temp_roots`] so `cargo test` never touches the
//! real host roots. Once pinned, `RUSSEL_DATA_DIR` is ignored for the rest of
//! the process, so parallel tests do not need to mutate process env.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DEFAULT_DATA_ROOT: &str = "/var/lib/russel";

/// Legacy microVM marker root, used when neither `RUSSEL_MICROVMS_DIR` nor
/// `RUSSEL_DATA_DIR` is set.
pub const DEFAULT_MICROVMS_ROOT: &str = "/var/lib/microvms";

/// Marker root under a relocated data root (`RUSSEL_DATA_DIR`). Reserved, so
/// discovery never lists it as a service.
pub const MICROVMS_SUBDIR: &str = "_microvms";

/// Name prefix of the per-process temp bases made by [`pin_temp_roots`].
const TEMP_ROOT_PREFIX: &str = "russel-test-";

/// Held (flock) for the life of the process that owns a temp base. Liveness
/// comes from the lock, not the PID in the name: PIDs do not cross PID
/// namespaces, but a lock on a shared temp dir does.
const TEMP_ROOT_LOCK: &str = ".lock";

struct PinnedRoots {
    base: PathBuf,
    data: PathBuf,
    microvms: PathBuf,
    _lock: std::fs::File,
}

static PINNED: OnceLock<PinnedRoots> = OnceLock::new();

/// State directory for services, the catalog, and managed volumes.
///
/// A relative or empty `RUSSEL_DATA_DIR` is ignored so a bad env value cannot
/// park service data in the process working directory.
pub fn data_root() -> PathBuf {
    if let Some(pinned) = PINNED.get() {
        return pinned.data.clone();
    }
    data_root_from(std::env::var("RUSSEL_DATA_DIR").ok().as_deref())
}

pub fn data_root_from(value: Option<&str>) -> PathBuf {
    if let Some(raw) = value {
        let raw = raw.trim();
        let path = Path::new(raw);
        if !raw.is_empty() && path.is_absolute() {
            return path.to_path_buf();
        }
    }
    PathBuf::from(DEFAULT_DATA_ROOT)
}

pub fn service_dir(service_id: &str) -> PathBuf {
    data_root().join(service_id)
}

/// Parent of the per-service microVM marker dirs: `RUSSEL_MICROVMS_DIR`, else
/// `<RUSSEL_DATA_DIR>/_microvms`, else `/var/lib/microvms` (#413).
pub fn microvms_root() -> PathBuf {
    if let Some(pinned) = PINNED.get() {
        return pinned.microvms.clone();
    }
    microvms_root_from(
        std::env::var("RUSSEL_MICROVMS_DIR").ok().as_deref(),
        std::env::var("RUSSEL_DATA_DIR").ok().as_deref(),
    )
}

pub fn microvms_root_from(microvms_dir: Option<&str>, data_dir: Option<&str>) -> PathBuf {
    let absolute = |raw: Option<&str>| {
        raw.map(str::trim)
            .filter(|s| !s.is_empty() && Path::new(s).is_absolute())
            .map(PathBuf::from)
    };
    if let Some(dir) = absolute(microvms_dir) {
        return dir;
    }
    if let Some(data) = absolute(data_dir) {
        return data.join(MICROVMS_SUBDIR);
    }
    PathBuf::from(DEFAULT_MICROVMS_ROOT)
}

/// Test support: point [`data_root`] and [`microvms_root`] at a fresh
/// per-process temp dir for the rest of the process. Idempotent. Returns the
/// temp base (`<tmp>/russel-test-<pid>-…`), which holds `data/` and
/// `microvms/`.
///
/// Also removes temp bases left by earlier test processes that have exited,
/// since a `static` never runs its destructor.
///
/// Panics if the temp dir cannot be created: carrying on against the real
/// host roots is the failure this exists to prevent.
#[doc(hidden)]
pub fn pin_temp_roots() -> &'static Path {
    &PINNED
        .get_or_init(|| {
            let tmp = std::env::temp_dir();
            remove_stale_temp_roots(&tmp);
            let base = make_temp_base(&tmp);
            let lock = lock_temp_base(&base);
            let data = base.join("data");
            let microvms = base.join("microvms");
            for dir in [&data, &microvms] {
                std::fs::create_dir(dir)
                    .unwrap_or_else(|e| panic!("create test root {}: {e}", dir.display()));
            }
            PinnedRoots {
                base,
                data,
                microvms,
                _lock: lock,
            }
        })
        .base
}

fn make_temp_base(tmp: &Path) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;

    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut last_err = None;
    for attempt in 0..16u32 {
        let base = tmp.join(format!("{TEMP_ROOT_PREFIX}{pid}-{nanos}-{attempt}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&base) {
            Ok(()) => return base,
            Err(e) => last_err = Some(e),
        }
    }
    panic!("create test root under {}: {last_err:?}", tmp.display());
}

fn lock_temp_base(base: &Path) -> std::fs::File {
    let path = base.join(TEMP_ROOT_LOCK);
    let file = std::fs::File::create(&path)
        .unwrap_or_else(|e| panic!("create test root lock {}: {e}", path.display()));
    file.try_lock()
        .unwrap_or_else(|e| panic!("lock test root {}: {e}", path.display()));
    file
}

/// Best effort: delete `russel-test-*` dirs whose owner no longer holds the
/// lock. A dir without a lock file is skipped: its owner may be between
/// creating the dir and the lock.
fn remove_stale_temp_roots(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .to_str()
            .is_some_and(|n| n.starts_with(TEMP_ROOT_PREFIX))
        {
            continue;
        }
        let Ok(lock) = std::fs::File::open(entry.path().join(TEMP_ROOT_LOCK)) else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue;
        }
        // Another user's dirs fail with EPERM under the sticky /tmp; ignore.
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_relative_env_uses_default() {
        assert_eq!(data_root_from(None), PathBuf::from("/var/lib/russel"));
        assert_eq!(data_root_from(Some("")), PathBuf::from("/var/lib/russel"));
        assert_eq!(data_root_from(Some("  ")), PathBuf::from("/var/lib/russel"));
        assert_eq!(
            data_root_from(Some("relative/russel")),
            PathBuf::from("/var/lib/russel")
        );
    }

    #[test]
    fn microvms_root_follows_env_then_data_root() {
        assert_eq!(
            microvms_root_from(None, None),
            PathBuf::from("/var/lib/microvms")
        );
        assert_eq!(
            microvms_root_from(None, Some("/srv/russel")),
            PathBuf::from("/srv/russel/_microvms")
        );
        assert_eq!(
            microvms_root_from(Some("/srv/vms"), Some("/srv/russel")),
            PathBuf::from("/srv/vms")
        );
        assert_eq!(
            microvms_root_from(Some("rel"), Some("also-rel")),
            PathBuf::from("/var/lib/microvms")
        );
    }

    #[test]
    fn absolute_env_is_the_root() {
        assert_eq!(
            data_root_from(Some(" /srv/russel ")),
            PathBuf::from("/srv/russel")
        );
        assert_eq!(
            data_root_from(Some("/srv/russel")).join("api"),
            PathBuf::from("/srv/russel/api")
        );
    }

    #[test]
    fn pinned_temp_roots_replace_host_roots() {
        let base = pin_temp_roots();
        assert_eq!(pin_temp_roots(), base, "pinning is idempotent");
        assert!(base.starts_with(std::env::temp_dir()));
        assert_eq!(data_root(), base.join("data"));
        assert_eq!(microvms_root(), base.join("microvms"));
        assert_eq!(service_dir("api"), base.join("data/api"));
        assert!(data_root().is_dir() && microvms_root().is_dir());
    }

    #[test]
    fn stale_temp_roots_of_dead_processes_are_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dead = tmp.path().join(format!("{TEMP_ROOT_PREFIX}1-1-0"));
        let live = tmp.path().join(format!("{TEMP_ROOT_PREFIX}2-1-0"));
        let unlocked = tmp.path().join(format!("{TEMP_ROOT_PREFIX}3-1-0"));
        let other = tmp.path().join("russel-other-1");
        for dir in [&dead, &live, &unlocked, &other] {
            std::fs::create_dir_all(dir.join("data")).unwrap();
        }
        // A dead owner leaves the lock file behind but no longer holds it.
        drop(lock_temp_base(&dead));
        // The PID in the name is irrelevant: only the held lock keeps a dir.
        let _held = lock_temp_base(&live);
        remove_stale_temp_roots(tmp.path());
        assert!(!dead.exists());
        assert!(live.exists());
        assert!(unlocked.exists(), "no lock file yet: owner may be starting");
        assert!(other.exists());
    }
}
