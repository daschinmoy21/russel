//! Service id and binary name rules shared by ctrl, agent, and CLI.
//!
//! A service id becomes a directory under the data root, a Podman name, a
//! TAP/subnet lease key, and a `/vm/{id}` URL segment, so every crate must
//! agree on exactly one charset.

use crate::reserved::is_reserved_service_dir;

/// Longest accepted service id, in bytes.
pub const MAX_SERVICE_ID_LEN: usize = 128;

/// Validate a service id: 1..=128 ASCII `[A-Za-z0-9_-]`, not a reserved
/// data-root directory (`secrets`, `traefik`, `_pool`, `*.bak`, …).
pub fn validate_service_id(service_id: &str) -> anyhow::Result<()> {
    if service_id.is_empty() {
        anyhow::bail!("service_id cannot be empty");
    }
    if service_id.len() > MAX_SERVICE_ID_LEN {
        anyhow::bail!("service_id too long (max {MAX_SERVICE_ID_LEN} characters)");
    }
    if service_id.contains('/') || service_id.contains('\\') {
        anyhow::bail!("service_id cannot contain path separators");
    }
    if !service_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "service_id can only contain ASCII alphanumeric characters, dashes, and underscores"
        );
    }
    // Reject host state trees (secrets/traefik/_pool/*.bak) so deploy/destroy
    // cannot wipe /var/lib/russel/{secrets,traefik,_pool} or backup dirs.
    if is_reserved_service_dir(service_id) {
        anyhow::bail!("service_id is reserved: {service_id}");
    }
    Ok(())
}

/// Validate a binary name for shell safety: only `[A-Za-z0-9._+-]`.
///
/// Also rejects `.`, `..`, all-dots names, and names without at least one
/// alphanumeric character (they would resolve to `.` / `..` when joined as
/// `rootfs/bin/<name>`).
pub fn validate_bin_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("bin_name must not be empty");
    }
    if name.len() > 256 {
        anyhow::bail!("bin_name too long (max 256 characters)");
    }
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
        anyhow::bail!("bin_name '{name}' contains invalid characters (only A-Za-z0-9._+- allowed)");
    }
    if !has_alnum {
        anyhow::bail!("bin_name must contain at least one alphanumeric character");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn service_id_accepts_normal_ids() {
        for id in ["api", "basic-http-tester", "svc_01", "pooltpl", "my-svc_1"] {
            validate_service_id(id).unwrap();
        }
        validate_service_id(&"a".repeat(MAX_SERVICE_ID_LEN)).unwrap();
    }

    #[test]
    fn service_id_rejects_reserved_ids() {
        for id in ["secrets", "traefik", "_pool"] {
            let msg = validate_service_id(id).unwrap_err().to_string();
            assert!(
                msg.contains("reserved"),
                "expected reserved error for {id}, got: {msg}"
            );
        }
        // .bak ids fail the charset (dot) before the reserved check.
        assert!(validate_service_id("foo.bak").is_err());
    }

    #[test]
    fn service_id_rejects_empty_long_and_path_chars() {
        for id in ["", "../etc", "a/b", "a\\b", "svc.with.dot", "foo::x0"] {
            assert!(validate_service_id(id).is_err(), "{id:?} must be rejected");
        }
        assert!(validate_service_id(&"a".repeat(MAX_SERVICE_ID_LEN + 1)).is_err());
    }

    #[test]
    fn service_id_rejects_unicode() {
        let err = validate_service_id("café").unwrap_err();
        assert!(
            err.to_string().contains("ASCII"),
            "Unicode letters must fail the ASCII charset check, got: {err}"
        );
    }

    #[test]
    fn bin_name_accepts_shell_safe_names() {
        for name in ["app", "my-app", "app_v2", "gcc-12.2", "g++", "a.out"] {
            validate_bin_name(name).unwrap();
        }
    }

    #[test]
    fn bin_name_rejects_dots_and_metacharacters() {
        for name in ["", ".", "..", "...", "-_+", "a b", "a;b", "$(x)", "a/b"] {
            assert!(
                validate_bin_name(name).is_err(),
                "{name:?} must be rejected"
            );
        }
        assert!(validate_bin_name(&"a".repeat(257)).is_err());
    }
}
