//! Secure Russelfile loading under a repository root (openat / O_NOFOLLOW).

use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use russel_core::config::Russelfile;

/// Maximum accepted size for a Russelfile (prevents huge-file DoS via config_path).
pub(crate) const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
/// Validate relative path components of user `config_path` (no absolute / `..`).
pub(crate) fn validate_relative_config_path(config_path: &str) -> anyhow::Result<&Path> {
    if config_path.is_empty() {
        anyhow::bail!("config_path must not be empty");
    }

    let cfg = Path::new(config_path);
    if cfg.is_absolute() {
        anyhow::bail!("config_path must be relative to the repository root (got absolute path)");
    }

    for component in cfg.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                anyhow::bail!("config_path must not contain '..' path components");
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("config_path must be relative to the repository root");
            }
        }
    }
    Ok(cfg)
}

/// Open and parse a Russelfile strictly under `repo_path` without a
/// validate-then-reopen TOCTOU window on the path string.
///
/// Contract (security-sensitive):
/// - rejects empty, absolute, and `..` components
/// - opens the repository directory, then each parent component via `openat`
///   with `O_DIRECTORY | O_NOFOLLOW` (intermediate symlinks rejected; no
///   path-based open after `canonicalize` that could race with renames)
/// - opens the leaf with `openat(..., O_NOFOLLOW)` relative to the final
///   directory descriptor (final symlink rejected)
/// - `fstat`s the open fd for regular-file + size cap
/// - reads at most `MAX_CONFIG_BYTES` from the same open handle
///
/// Callers must load configuration only through this helper (or an equivalent
/// descriptor-relative open) so deploy never reopens a validated path string.
#[cfg(test)]
pub(crate) fn load_russelfile_under_repo(
    repo_path: &Path,
    config_path: &str,
) -> anyhow::Result<Russelfile> {
    load_russelfile_text_under_repo(repo_path, config_path).map(|(config, _)| config)
}

/// [`load_russelfile_under_repo`], plus the file's text: a generation
/// records the exact Russelfile it ran (#558).
pub(crate) fn load_russelfile_text_under_repo(
    repo_path: &Path,
    config_path: &str,
) -> anyhow::Result<(Russelfile, String)> {
    let cfg = validate_relative_config_path(config_path)?;

    let file_name = cfg
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("config_path '{}' has no file name", config_path))?;

    #[cfg(unix)]
    let mut file = {
        let repo_dir = open_directory_fd(repo_path).map_err(|e| {
            anyhow::anyhow!(
                "cannot resolve repository path {}: {e}",
                repo_path.display()
            )
        })?;

        // Walk parent components with stable directory descriptors so a rename
        // of an intermediate directory cannot swap in a symlink after a path
        // canonicalize (path-based open would follow the new link).
        let mut dir_fd = repo_dir;
        if let Some(parent) = cfg.parent() {
            for component in parent.components() {
                match component {
                    Component::CurDir => {}
                    Component::Normal(name) => {
                        dir_fd =
                            openat_directory_nofollow(dir_fd.as_raw_fd(), name).map_err(|e| {
                                anyhow::anyhow!(
                                    "config_path '{}' not found under repository: {e}",
                                    config_path
                                )
                            })?;
                    }
                    Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                        // Already rejected by validate_relative_config_path.
                        anyhow::bail!("config_path must be relative to the repository root");
                    }
                }
            }
        }

        openat_file_nofollow(dir_fd.as_raw_fd(), file_name).map_err(|e| {
            // Leaf symlink → ELOOP / "Too many levels of symbolic links"
            anyhow::anyhow!("cannot open config_path '{}': {e}", config_path)
        })?
    };

    #[cfg(not(unix))]
    let mut file = {
        let repo_canon = repo_path.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "cannot resolve repository path {}: {e}",
                repo_path.display()
            )
        })?;
        let joined = repo_canon.join(cfg);
        let parent = joined.parent().ok_or_else(|| {
            anyhow::anyhow!("config_path '{}' has no parent directory", config_path)
        })?;
        let parent_canon = parent.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "config_path '{}' not found under repository: {e}",
                config_path
            )
        })?;
        if !parent_canon.starts_with(&repo_canon) {
            anyhow::bail!("config_path escapes repository root");
        }
        OpenOptions::new()
            .read(true)
            .open(parent_canon.join(file_name))
            .map_err(|e| anyhow::anyhow!("cannot open config_path '{}': {e}", config_path))?
    };

    let meta = file
        .metadata()
        .map_err(|e| anyhow::anyhow!("cannot stat opened config_path: {e}"))?;
    if !meta.is_file() {
        anyhow::bail!("config_path must be a regular file");
    }
    if meta.len() > MAX_CONFIG_BYTES {
        anyhow::bail!(
            "config_path exceeds maximum size of {} bytes",
            MAX_CONFIG_BYTES
        );
    }

    // Bound the read itself (not only the pre-check): a concurrent writer could
    // grow the file after fstat; `take` ensures we never buffer more than cap+1.
    let mut contents = String::new();
    file.by_ref()
        .take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_string(&mut contents)
        .map_err(|e| anyhow::anyhow!("failed to read config_path '{}': {e}", config_path))?;
    if (contents.len() as u64) > MAX_CONFIG_BYTES {
        anyhow::bail!(
            "config_path exceeds maximum size of {} bytes",
            MAX_CONFIG_BYTES
        );
    }

    let config = Russelfile::load_from_str(&contents)?;
    Ok((config, contents))
}

/// Directory the build runs in: the Russelfile's folder joined with
/// `service.source` (#525). A Russelfile at the repo root with `source = "."`
/// builds the repo root, as before; `--config apps/api/Russelfile.toml` builds
/// `apps/api`, so one repo can hold several services.
///
/// Both inputs are already free of `..` and absolute parts. Each component is
/// then opened with `O_NOFOLLOW`, so a symlinked directory in the checkout
/// can't point the build outside it.
pub(crate) fn resolve_build_dir(
    repo_path: &Path,
    config_path: &str,
    source: &str,
) -> anyhow::Result<PathBuf> {
    let cfg = validate_relative_config_path(config_path)?;
    russel_core::config::validate_source_path(source)?;
    let rel: PathBuf = cfg
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(source)
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    let not_a_dir = |e: std::io::Error| {
        anyhow::anyhow!(
            "service.source '{source}' in {config_path} is not a directory in the repository: {e}"
        )
    };

    #[cfg(unix)]
    {
        let mut dir_fd = open_directory_fd(repo_path).map_err(|e| {
            anyhow::anyhow!(
                "cannot resolve repository path {}: {e}",
                repo_path.display()
            )
        })?;
        for component in rel.components() {
            let Component::Normal(name) = component else {
                anyhow::bail!("service.source must stay inside the repository");
            };
            dir_fd = openat_directory_nofollow(dir_fd.as_raw_fd(), name).map_err(not_a_dir)?;
        }
    }

    #[cfg(not(unix))]
    {
        let repo_canon = repo_path.canonicalize().map_err(not_a_dir)?;
        let dir_canon = repo_canon.join(&rel).canonicalize().map_err(not_a_dir)?;
        if !dir_canon.starts_with(&repo_canon) || !dir_canon.is_dir() {
            anyhow::bail!("service.source must be a directory inside the repository");
        }
    }

    Ok(repo_path.join(rel))
}

/// Open `path` as a directory (symlinks on the final component may be followed;
/// containment is enforced by subsequent `openat` + `O_NOFOLLOW` steps).
#[cfg(unix)]
pub(crate) fn open_directory_fd(path: &Path) -> std::io::Result<OwnedFd> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    opts.custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC);
    let file = opts.open(path)?;
    Ok(OwnedFd::from(file))
}

/// `openat(parent, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)`.
#[cfg(unix)]
pub(crate) fn openat_directory_nofollow(
    parent_fd: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
) -> std::io::Result<OwnedFd> {
    let c_name = std::ffi::CString::new(name.as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(parent_fd, c_name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `openat(parent, name, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)` — regular file open.
#[cfg(unix)]
pub(crate) fn openat_file_nofollow(
    parent_fd: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
) -> std::io::Result<File> {
    let c_name = std::ffi::CString::new(name.as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(parent_fd, c_name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
