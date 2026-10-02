//! Nix GC roots for deployed and retained generations (#411).
//!
//! Every build runs with `--no-link`, so nothing but a live process kept a
//! deployed closure alive. `nix-collect-garbage` (NixOS `nix.gc.automatic`)
//! could then delete the store path that a restart, reboot recovery, or
//! rollback re-execs.
//!
//! Each service gets `<data_root>/_pool/gcroots/<service_id>/`, holding one
//! indirect root per store path it still needs: the paths in the current
//! `metadata.json` (app `store_path`, microVM `kernel_path` / `initramfs`,
//! container `rootfs_path`) and the journal's active and previous generations. [`sync`]
//! makes the directory match that set, so a generation that leaves the
//! retention window loses its root on the next sync. The directory lives
//! under `_pool` rather than the service dir because a cold redeploy renames
//! the service dir to `.bak`, and Nix tracks indirect roots by link path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::deployments;

/// Metadata keys whose values may be Nix store paths a service re-execs.
const ROOTED_METADATA_KEYS: &[&str] = &[
    "store_path",
    "app_path",
    "kernel_path",
    "initramfs",
    "rootfs_path",
];

/// `<data_root>/_pool/gcroots/<service_id>`.
pub fn service_roots_dir(service_id: &str) -> PathBuf {
    crate::paths::data_root()
        .join("_pool")
        .join("gcroots")
        .join(service_id)
}

/// The top-level store path (`/nix/store/<hash>-<name>`) under `path`, or
/// `None` when `path` is not inside the Nix store.
pub fn store_root_of(path: &str) -> Option<PathBuf> {
    let rest = path.strip_prefix("/nix/store/")?;
    let name = rest.split('/').next().filter(|n| !n.is_empty())?;
    Some(Path::new("/nix/store").join(name))
}

/// Store paths a service needs kept: everything rooted in its current
/// metadata plus the given journal store paths.
pub fn desired_roots<'a>(
    metadata: Option<&serde_json::Value>,
    journal_store_paths: impl IntoIterator<Item = &'a str>,
) -> BTreeSet<PathBuf> {
    let mut roots = BTreeSet::new();
    if let Some(meta) = metadata {
        for key in ROOTED_METADATA_KEYS {
            if let Some(root) = meta
                .get(*key)
                .and_then(|v| v.as_str())
                .and_then(store_root_of)
            {
                roots.insert(root);
            }
        }
    }
    roots.extend(journal_store_paths.into_iter().filter_map(store_root_of));
    roots
}

/// Make `dir` hold exactly one root per path in `desired`: remove links for
/// paths no longer wanted, then `add` the missing ones. A link is named after
/// the store path's basename, so repeated syncs are idempotent.
pub fn sync_dir(
    dir: &Path,
    desired: &BTreeSet<PathBuf>,
    add: impl Fn(&Path, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("create gcroots dir {}: {e}", dir.display()))?;
    let wanted: BTreeSet<_> = desired.iter().filter_map(|p| p.file_name()).collect();
    for entry in std::fs::read_dir(dir)?.flatten() {
        if !wanted.contains(entry.file_name().as_os_str()) {
            std::fs::remove_file(entry.path()).map_err(|e| {
                anyhow::anyhow!("remove stale gcroot {}: {e}", entry.path().display())
            })?;
        }
    }
    let mut errors = Vec::new();
    for store_path in desired {
        let Some(name) = store_path.file_name() else {
            continue;
        };
        let link = dir.join(name);
        if std::fs::read_link(&link).is_ok_and(|target| target == *store_path) {
            continue;
        }
        // A path already collected cannot be rooted; keep going so the
        // others still are.
        if let Err(e) = add(&link, store_path) {
            errors.push(format!("{}: {e}", store_path.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("gcroots: {}", errors.join("; "))
    }
}

/// Register `link` as an indirect GC root for `store_path`.
fn nix_add_root(link: &Path, store_path: &Path) -> anyhow::Result<()> {
    let output = std::process::Command::new("nix-store")
        .arg("--add-root")
        .arg(link)
        .arg("--realise")
        .arg(store_path)
        .output()
        .map_err(|e| anyhow::anyhow!("run nix-store --add-root: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "nix-store --add-root failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Root what `service_id` needs now and drop roots it no longer needs.
/// Blocking (runs `nix-store`); call from `spawn_blocking` in async code.
pub fn sync(service_id: &str) -> anyhow::Result<()> {
    let metadata =
        std::fs::read_to_string(crate::paths::service_dir(service_id).join("metadata.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let journal = deployments::retained_store_paths(service_id)?;
    let desired = desired_roots(metadata.as_ref(), journal.iter().map(String::as_str));
    sync_dir(&service_roots_dir(service_id), &desired, nix_add_root)
}

/// [`sync`] on the blocking pool.
pub async fn sync_blocking(service_id: &str) -> anyhow::Result<()> {
    let id = service_id.to_string();
    tokio::task::spawn_blocking(move || sync(&id))
        .await
        .map_err(|e| anyhow::anyhow!("gcroots sync task failed: {e}"))?
}

/// [`sync`] on the blocking pool; failures are logged, never fatal: a missing
/// root must not fail a deploy that already succeeded.
pub async fn sync_logged(service_id: &str) {
    if let Err(e) = sync_blocking(service_id).await {
        tracing::warn!(service_id, error = %e, "gcroots sync failed");
    }
}

/// Root a store path a deploy is about to launch, before anything else
/// roots it (#558). The next [`sync`] keeps it if the deploy recorded it and
/// drops it otherwise. Fails when the path is not in the store any more.
pub async fn root_in_flight(service_id: &str, store_path: &Path) -> anyhow::Result<()> {
    let dir = service_roots_dir(service_id);
    let path = store_path.to_path_buf();
    tokio::task::spawn_blocking(move || root_in_dir(&dir, &path, nix_add_root))
        .await
        .map_err(|e| anyhow::anyhow!("gcroot task failed: {e}"))?
}

/// Add one root for `store_path` under `dir`, keeping the roots already there.
fn root_in_dir(
    dir: &Path,
    store_path: &Path,
    add: impl Fn(&Path, &Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let root = store_root_of(&store_path.to_string_lossy())
        .ok_or_else(|| anyhow::anyhow!("{} is not a Nix store path", store_path.display()))?;
    let name = root
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("{} has no store name", root.display()))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("create gcroots dir {}: {e}", dir.display()))?;
    let link = dir.join(name);
    if std::fs::read_link(&link).is_ok_and(|target| target == root) {
        return Ok(());
    }
    add(&link, &root)
}

/// Drop every root for a destroyed service.
pub fn remove(service_id: &str) -> anyhow::Result<()> {
    let dir = service_roots_dir(service_id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!("remove gcroots {}: {e}", dir.display())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn store_root_of_takes_the_top_level_store_dir() {
        assert_eq!(
            store_root_of("/nix/store/abc-app/bin/app"),
            Some(PathBuf::from("/nix/store/abc-app"))
        );
        assert_eq!(
            store_root_of("/nix/store/abc-app"),
            Some(PathBuf::from("/nix/store/abc-app"))
        );
        assert_eq!(store_root_of("/var/lib/russel/_pool/kernel/bzImage"), None);
        assert_eq!(store_root_of("/nix/store/"), None);
    }

    #[test]
    fn desired_roots_cover_metadata_and_journal() {
        let meta = serde_json::json!({
            "store_path": "/nix/store/aaa-app",
            "app_path": "/nix/store/aaa-app/bin/app",
            "kernel_path": "/nix/store/kkk-kernel/bzImage",
            "rootfs_path": "/nix/store/rrr-rootfs",
            "host_port": 8080,
        });
        let pool_kernel =
            serde_json::json!({ "kernel_path": "/var/lib/russel/_pool/kernel/bzImage" });
        let roots = desired_roots(Some(&meta), ["/nix/store/ppp-app", "/nix/store/aaa-app"]);
        let names: Vec<_> = roots.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(
            names,
            [
                "/nix/store/aaa-app",
                "/nix/store/kkk-kernel",
                "/nix/store/ppp-app",
                "/nix/store/rrr-rootfs",
            ]
        );
        assert!(desired_roots(Some(&pool_kernel), []).is_empty());
        assert!(desired_roots(None, []).is_empty());
    }

    #[test]
    fn sync_dir_adds_missing_and_drops_stale_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gcroots/api");
        let added = RefCell::new(Vec::new());
        // Stand-in for nix-store --add-root: a plain symlink to the target.
        let add = |link: &Path, target: &Path| {
            added.borrow_mut().push(target.to_path_buf());
            std::os::unix::fs::symlink(target, link)?;
            Ok(())
        };

        let gen1: BTreeSet<_> = [PathBuf::from("/nix/store/g1-app")].into();
        sync_dir(&dir, &gen1, add).unwrap();
        assert!(dir.join("g1-app").symlink_metadata().is_ok());

        // Deploy gen2: gen1 is now `previous` and stays rooted.
        let gen2: BTreeSet<_> = [
            PathBuf::from("/nix/store/g1-app"),
            PathBuf::from("/nix/store/g2-app"),
        ]
        .into();
        sync_dir(&dir, &gen2, add).unwrap();
        assert_eq!(
            *added.borrow(),
            [
                PathBuf::from("/nix/store/g1-app"),
                PathBuf::from("/nix/store/g2-app")
            ],
            "an existing root is not re-added"
        );

        // Deploy gen3: gen1 leaves the retention window and loses its root.
        let gen3: BTreeSet<_> = [
            PathBuf::from("/nix/store/g2-app"),
            PathBuf::from("/nix/store/g3-app"),
        ]
        .into();
        sync_dir(&dir, &gen3, add).unwrap();
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["g2-app", "g3-app"]);
    }

    #[test]
    fn sync_dir_roots_the_rest_when_one_path_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gcroots/api");
        let add = |link: &Path, target: &Path| {
            if target.ends_with("gone-app") {
                anyhow::bail!("path is not valid");
            }
            std::os::unix::fs::symlink(target, link)?;
            Ok(())
        };
        let desired: BTreeSet<_> = [
            PathBuf::from("/nix/store/gone-app"),
            PathBuf::from("/nix/store/live-app"),
        ]
        .into();
        let err = sync_dir(&dir, &desired, add).unwrap_err().to_string();
        assert!(err.contains("gone-app"), "{err}");
        assert!(dir.join("live-app").symlink_metadata().is_ok());
    }

    #[test]
    fn remove_drops_the_service_roots_dir() {
        let dir = service_roots_dir("gcroots-remove-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/nix/store/x-app", dir.join("x-app")).unwrap();
        remove("gcroots-remove-test").unwrap();
        assert!(!dir.exists());
        remove("gcroots-remove-test").unwrap();
    }

    #[test]
    fn root_in_dir_adds_one_root_and_keeps_the_others() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gcroots/api");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/nix/store/old-app", dir.join("old-app")).unwrap();
        let added = RefCell::new(Vec::new());
        let add = |link: &Path, target: &Path| {
            added.borrow_mut().push(target.to_path_buf());
            std::os::unix::fs::symlink(target, link)?;
            Ok(())
        };

        root_in_dir(&dir, Path::new("/nix/store/new-app/bin/app"), add).unwrap();
        root_in_dir(&dir, Path::new("/nix/store/new-app"), add).unwrap();
        assert_eq!(*added.borrow(), [PathBuf::from("/nix/store/new-app")]);
        assert!(dir.join("old-app").symlink_metadata().is_ok());
        assert!(dir.join("new-app").symlink_metadata().is_ok());

        let gone = |_: &Path, _: &Path| anyhow::bail!("path is not valid");
        let err = root_in_dir(&dir, Path::new("/nix/store/gone-app"), gone).unwrap_err();
        assert!(err.to_string().contains("not valid"));
        assert!(root_in_dir(&dir, Path::new("/tmp/app"), add).is_err());
    }
}
