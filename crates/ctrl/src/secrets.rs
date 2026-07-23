//! Host-side secrets store for Russel.
//!
//! Values live under `/var/lib/russel/secrets/<name>` with mode `0600`.
//! This is intentionally a simple single-node store — not a KMS. Values are
//! never written into Russelfile; deploy resolves `secret://name` env refs.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const DEFAULT_SECRETS_DIR: &str = "/var/lib/russel/secrets";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Validate a secret name: alphanumeric, `_`, `-`, length 1..=64.
pub fn validate_secret_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("secret name must be 1..=64 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("secret name may only contain [A-Za-z0-9_-]");
    }
    if name.starts_with('-') || name.starts_with('.') {
        anyhow::bail!("secret name must not start with '-' or '.'");
    }
    Ok(())
}

fn secrets_dir() -> PathBuf {
    std::env::var("RUSSEL_SECRETS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_SECRETS_DIR))
}

fn secret_path(name: &str) -> anyhow::Result<PathBuf> {
    validate_secret_name(name)?;
    Ok(secrets_dir().join(name))
}

/// Persist a secret value (mode 0600). Overwrites if present via atomic rename.
pub fn set_secret(name: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() {
        anyhow::bail!("secret value must not be empty");
    }
    if value.len() > 64 * 1024 {
        anyhow::bail!("secret value too large (max 64 KiB)");
    }
    let path = secret_path(name)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("secret path has no parent"))?;
    std::fs::create_dir_all(parent).map_err(|e| anyhow::anyhow!("create secrets dir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| anyhow::anyhow!("chmod secrets dir: {e}"))?;
    }

    secure_write(&path, value.as_bytes(), "secret")?;
    Ok(())
}

/// Write a sensitive file atomically with restrictive permissions.
///
/// The parent directory must already exist. The temporary file is created in
/// that directory so the final rename remains atomic on the same filesystem.
pub(crate) fn secure_write(path: &Path, content: &[u8], kind: &str) -> anyhow::Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ u128::from(TEMP_COUNTER.fetch_add(1, Ordering::Relaxed));
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{kind} path has no parent"))?;
    let tmp = parent.join(format!(
        ".{}.tmp.{nonce:x}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
    ));
    let write_result = (|| -> anyhow::Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut f = options
            .open(&tmp)
            .map_err(|e| anyhow::anyhow!("create {kind} tmp: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| anyhow::anyhow!("chmod {kind} tmp: {e}"))?;
        }
        f.write_all(content)
            .map_err(|e| anyhow::anyhow!("write {kind} tmp: {e}"))?;
        f.sync_all()
            .map_err(|e| anyhow::anyhow!("fsync {kind} tmp: {e}"))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("rename {kind} into place: {e}")
    })?;
    Ok(())
}

/// Read a secret value.
pub fn get_secret(name: &str) -> anyhow::Result<String> {
    let path = secret_path(name)?;
    std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("secret {name:?} not found: {e}"))
}

/// Delete a secret if present.
pub fn delete_secret(name: &str) -> anyhow::Result<bool> {
    let path = secret_path(name)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::anyhow!("delete secret: {e}")),
    }
}

/// List secret names (not values).
pub fn list_secrets() -> anyhow::Result<Vec<String>> {
    let dir = secrets_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| anyhow::anyhow!("read secrets dir: {e}"))? {
        let entry = entry.map_err(|e| anyhow::anyhow!("read secrets entry: {e}"))?;
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false)
            && let Some(name) = entry.file_name().to_str()
        {
            // Skip temp files.
            if name.starts_with('.') {
                continue;
            }
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

const SECRET_REF_PREFIX: &str = "secret://";

/// Resolve `secret://name` values in an env map. Non-ref values pass through.
pub fn resolve_env_secrets(
    env: &std::collections::HashMap<String, String>,
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut out = std::collections::HashMap::new();
    for (k, v) in env {
        if let Some(name) = v.strip_prefix(SECRET_REF_PREFIX) {
            let value = get_secret(name)?;
            out.insert(k.clone(), value);
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(out)
}

/// Test helper: operate under an arbitrary secrets root.
/// Serialized via a process-wide mutex; restores env even on panic.
#[cfg(test)]
pub fn with_secrets_dir<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    struct EnvRestore(Option<std::ffi::OsString>);
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("RUSSEL_SECRETS_DIR", v),
                    None => std::env::remove_var("RUSSEL_SECRETS_DIR"),
                }
            }
        }
    }

    let prev = std::env::var_os("RUSSEL_SECRETS_DIR");
    let _restore = EnvRestore(prev);
    unsafe {
        std::env::set_var("RUSSEL_SECRETS_DIR", dir);
    }
    f()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn set_get_list_delete() {
        let tmp = TempDir::new().unwrap();
        with_secrets_dir(tmp.path(), || {
            set_secret("api_key", "sk-test").unwrap();
            assert_eq!(get_secret("api_key").unwrap(), "sk-test");
            assert_eq!(list_secrets().unwrap(), vec!["api_key".to_string()]);
            assert!(delete_secret("api_key").unwrap());
            assert!(!delete_secret("api_key").unwrap());
            assert!(get_secret("api_key").is_err());
        });
    }

    #[test]
    fn rejects_bad_names() {
        assert!(validate_secret_name("").is_err());
        assert!(validate_secret_name("../etc").is_err());
        assert!(validate_secret_name("has space").is_err());
        assert!(validate_secret_name("good_name-1").is_ok());
    }

    #[test]
    fn resolve_secret_refs() {
        let tmp = TempDir::new().unwrap();
        with_secrets_dir(tmp.path(), || {
            set_secret("db_pass", "s3cret").unwrap();
            let mut env = std::collections::HashMap::new();
            env.insert("PASSWORD".into(), "secret://db_pass".into());
            env.insert("PLAIN".into(), "hello".into());
            let resolved = resolve_env_secrets(&env).unwrap();
            assert_eq!(resolved.get("PASSWORD").unwrap(), "s3cret");
            assert_eq!(resolved.get("PLAIN").unwrap(), "hello");
        });
    }
}
