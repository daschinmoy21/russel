//! Shell-safe env helpers for deploy (bin names, quoting, container env list).

use std::collections::HashMap;

/// Validate a binary/service name for shell safety: only `[A-Za-z0-9._+-]`.
/// Additionally rejects `.`, `..`, all-dots names, and names without at least
/// one alphanumeric character (paths that would resolve to `.` / `..` when
/// joined as `rootfs/bin/<name>`).
/// Rejects empty strings, whitespace, shell metacharacters.
pub fn validate_bin_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("bin_name must not be empty");
    }
    if name.len() > 256 {
        anyhow::bail!("bin_name too long (max 256 characters)");
    }
    // Reject names that are exactly "." or ".." or all dots.
    if name == "." || name == ".." {
        anyhow::bail!("bin_name must not be '.' or '..'");
    }
    if name.bytes().all(|c| c == b'.') {
        anyhow::bail!("bin_name must not consist entirely of dots");
    }
    let mut has_alnum = false;
    let valid = name.bytes().all(|c| {
        if c.is_ascii_alphanumeric() {
            has_alnum = true;
            true
        } else {
            c == b'.' || c == b'_' || c == b'+' || c == b'-'
        }
    });
    if !valid {
        anyhow::bail!(
            "bin_name '{}' contains invalid characters (only A-Za-z0-9._+- allowed)",
            name
        );
    }
    if !has_alnum {
        anyhow::bail!("bin_name must contain at least one alphanumeric character");
    }
    Ok(())
}

/// Shell-safe single-quoted value for deploy.env: escapes embedded `'` as `'\''`.
pub fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

/// Build the env list for a container start spec: PORT first (managed),
/// then user env vars (already validated; PORT filtered out to prevent override).
pub fn build_container_env(
    guest_port: u16,
    user_env: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![("PORT".to_string(), guest_port.to_string())];
    for (key, value) in user_env {
        if key == "PORT" {
            continue; // managed by Russel, user cannot override
        }
        env.push((key.clone(), value.clone()));
    }
    env
}
