//! Russelfile `[[volumes]]` and `[[ports]]` plus destroy policy for managed data.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Max `[[volumes]]` rows in one file.
pub const MAX_VOLUMES: usize = 16;
/// Max extra `[[ports]]` rows in one file.
pub const MAX_EXTRA_PORTS: usize = 8;

/// How destroy treats managed (`name =`) volume directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeDestroyPolicy {
    /// Honor each volume's `keep` field (default).
    FollowFile,
    /// Keep every managed volume directory.
    KeepAll,
    /// Delete every managed volume directory.
    DeleteAll,
}

impl VolumeDestroyPolicy {
    /// Map the destroy query/CLI override. `None` follows the file.
    pub fn from_keep_override(keep_volumes: Option<bool>) -> Self {
        match keep_volumes {
            Some(true) => Self::KeepAll,
            Some(false) => Self::DeleteAll,
            None => Self::FollowFile,
        }
    }

    /// Whether a managed volume with file-level `keep` should survive destroy.
    pub fn keep_managed(self, volume_keep: bool) -> bool {
        match self {
            Self::KeepAll => true,
            Self::DeleteAll => false,
            Self::FollowFile => volume_keep,
        }
    }
}

/// One bind from the host into the container. Rootfs stays `--read-only`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeSpec {
    /// Managed directory under `/var/lib/russel/<id>/volumes/<name>`.
    /// Mutually exclusive with [`Self::host`].
    #[serde(default)]
    pub name: Option<String>,
    /// Absolute host path. Requires `RUSSEL_VOLUME_ROOTS`. Never deleted on destroy.
    #[serde(default)]
    pub host: Option<String>,
    /// Absolute container path.
    pub guest: String,
    /// Bind read-write. Default false (read-only bind).
    #[serde(default)]
    pub rw: bool,
    /// Keep the managed directory when the service is destroyed.
    /// Only valid with `name`. Absolute `host` binds are never deleted.
    #[serde(default)]
    pub keep: bool,
}

/// Extra published port besides `service.port` / `[ingress].port`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraPortSpec {
    pub host: u16,
    pub guest: u16,
}

/// Host path ready to pass to `podman --mount`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedVolume {
    pub guest: String,
    pub host_path: PathBuf,
    pub rw: bool,
    pub keep: bool,
    pub managed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Directory for a managed volume: `/var/lib/russel/<id>/volumes/<name>`.
pub fn managed_volume_dir(service_id: &str, name: &str) -> anyhow::Result<PathBuf> {
    crate::ids::validate_service_id(service_id)?;
    validate_volume_name(name)?;
    Ok(crate::paths::data_root()
        .join(service_id)
        .join("volumes")
        .join(name))
}

/// `RUSSEL_VOLUME_ROOTS` as colon-separated absolute prefixes. Empty if unset.
pub fn volume_roots_from_env() -> Vec<PathBuf> {
    std::env::var("RUSSEL_VOLUME_ROOTS")
        .ok()
        .map(|s| {
            s.split(':')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

const DENIED_HOST_PREFIXES: &[&str] = &[
    "/etc",
    "/root",
    "/proc",
    "/sys",
    "/dev",
    "/nix/store",
    "/boot",
    "/run",
    // Control-plane state. A wide RUSSEL_VOLUME_ROOTS must not bind secrets,
    // other services, or the warm pool into a container.
    "/var/lib/russel",
    "/var/lib/microvms",
];

/// Podman `--mount` is a comma-separated option string. A comma in a path is
/// another option (`bind-propagation`, `Z`, …), and the text before the comma
/// is the path Podman actually opens. That can be a different inode than the
/// path the allowlist canonicalized.
pub fn reject_mount_csv_metacharacters(path: &str, what: &str) -> anyhow::Result<()> {
    if path.contains(',') || path.contains('\n') || path.contains('\r') {
        anyhow::bail!(
            "{what} must not contain commas or newlines (podman --mount splits on commas)"
        );
    }
    Ok(())
}

fn path_hits_denied_prefix(path: &str) -> Option<String> {
    for prefix in DENIED_HOST_PREFIXES {
        if path == *prefix || path.starts_with(&format!("{prefix}/")) {
            return Some((*prefix).to_string());
        }
    }
    for key in [
        "RUSSEL_DATA_DIR",
        "RUSSEL_SECRETS_DIR",
        "RUSSEL_MICROVMS_DIR",
    ] {
        let Ok(raw) = std::env::var(key) else {
            continue;
        };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        // A symlink data/secrets dir must deny the canonical target. If
        // canonicalize fails, still compare the trimmed raw path.
        let prefix = canonicalize_with_ancestors(Path::new(raw))
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| raw.to_string());
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() {
            continue;
        }
        if path == prefix || path.starts_with(&format!("{prefix}/")) {
            return Some(prefix.to_string());
        }
    }
    None
}

/// Structural checks at Russelfile load. Absolute-host allowlist is deploy-time.
pub fn validate_volumes(volumes: &[VolumeSpec]) -> anyhow::Result<()> {
    if volumes.is_empty() {
        return Ok(());
    }
    if volumes.len() > MAX_VOLUMES {
        anyhow::bail!(
            "too many [[volumes]] rows: {} (max {MAX_VOLUMES})",
            volumes.len()
        );
    }
    let mut names = std::collections::HashSet::new();
    let mut guests = std::collections::HashSet::new();
    for vol in volumes {
        validate_one_volume(vol)?;
        if let Some(name) = vol.name.as_deref()
            && !names.insert(name)
        {
            anyhow::bail!("duplicate volume name {name:?}");
        }
        if !guests.insert(vol.guest.as_str()) {
            anyhow::bail!("duplicate volume guest path {}", vol.guest);
        }
    }
    Ok(())
}

fn validate_one_volume(vol: &VolumeSpec) -> anyhow::Result<()> {
    validate_guest_path(&vol.guest)?;
    match (vol.name.as_deref(), vol.host.as_deref()) {
        (Some(name), None) => {
            validate_volume_name(name)?;
        }
        (None, Some(host)) => {
            if vol.keep {
                anyhow::bail!(
                    "volume keep is only valid with name = (managed volumes); \
                     absolute host binds are never deleted on destroy"
                );
            }
            validate_host_path_syntax(host)?;
        }
        (None, None) => {
            anyhow::bail!("[[volumes]] row needs name = or host =");
        }
        (Some(_), Some(_)) => {
            anyhow::bail!("[[volumes]] row must set name = or host =, not both");
        }
    }
    Ok(())
}

fn validate_volume_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("volume name must be 1..=64 characters");
    }
    if name == "." || name == ".." {
        anyhow::bail!("volume name must not be \".\" or \"..\"");
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("volume name must not be empty");
    };
    if !first.is_ascii_alphanumeric() {
        anyhow::bail!("volume name must start with an ASCII letter or digit");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        anyhow::bail!(
            "volume name {name:?} contains invalid characters (only A-Za-z0-9._- allowed)"
        );
    }
    Ok(())
}

fn validate_guest_path(guest: &str) -> anyhow::Result<()> {
    if guest.is_empty() {
        anyhow::bail!("volume guest path must not be empty");
    }
    if !guest.starts_with('/') {
        anyhow::bail!("volume guest path must be absolute (got {guest})");
    }
    // "/" (or only slashes) would bind over the container rootfs.
    if guest.trim_end_matches('/').is_empty() {
        anyhow::bail!("volume guest path must not be \"/\"");
    }
    if guest.contains('\0') {
        anyhow::bail!("volume guest path contains NUL");
    }
    reject_mount_csv_metacharacters(guest, "volume guest path")?;
    if Path::new(guest)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!("volume guest path must not contain '..'");
    }
    Ok(())
}

fn validate_host_path_syntax(host: &str) -> anyhow::Result<()> {
    if !host.starts_with('/') {
        anyhow::bail!("volume host path must be absolute (got {host})");
    }
    if host.contains('\0') {
        anyhow::bail!("volume host path contains NUL");
    }
    reject_mount_csv_metacharacters(host, "volume host path")?;
    if Path::new(host)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!("volume host path must not contain '..'");
    }
    if let Some(prefix) = path_hits_denied_prefix(host) {
        anyhow::bail!("volume host path {host} is denied ({prefix})");
    }
    Ok(())
}

pub fn validate_extra_ports(
    ports: &[ExtraPortSpec],
    service_port: u16,
    ingress_port: Option<u16>,
) -> anyhow::Result<()> {
    if ports.is_empty() {
        return Ok(());
    }
    if ports.len() > MAX_EXTRA_PORTS {
        anyhow::bail!(
            "too many [[ports]] rows: {} (max {MAX_EXTRA_PORTS})",
            ports.len()
        );
    }
    let mut hosts = std::collections::HashSet::new();
    let mut guests = std::collections::HashSet::new();
    guests.insert(service_port);
    if let Some(p) = ingress_port {
        hosts.insert(p);
    }
    for p in ports {
        crate::config::validate_publish_host_port("[[ports]] host", p.host)?;
        if p.guest == 0 {
            anyhow::bail!("[[ports]] guest must not be 0");
        }
        if p.guest == service_port {
            anyhow::bail!(
                "[[ports]] guest {} duplicates service.port; omit it from [[ports]]",
                p.guest
            );
        }
        if !hosts.insert(p.host) {
            anyhow::bail!("duplicate [[ports]] host {}", p.host);
        }
        if !guests.insert(p.guest) {
            anyhow::bail!("duplicate [[ports]] guest {}", p.guest);
        }
    }
    Ok(())
}

/// Allocator key for extra publish port `index` of `service_id`.
///
/// Uses `::` as a delimiter. Validated service IDs only allow
/// `[A-Za-z0-9_-]`, so this cannot collide with another service's primary key
/// (unlike the old `{id}__x{n}` form).
pub fn extra_port_key(service_id: &str, index: usize) -> String {
    format!("{service_id}::x{index}")
}

pub fn validate_package_attr(package: &str) -> anyhow::Result<()> {
    if package.is_empty() || package.len() > 128 {
        anyhow::bail!("service.package must be 1..=128 characters");
    }
    if package.starts_with('.') || package.ends_with('.') || package.contains("..") {
        anyhow::bail!("service.package {package:?} is not a nixpkgs attr path");
    }
    for part in package.split('.') {
        if part.is_empty() {
            anyhow::bail!("service.package {package:?} is not a nixpkgs attr path");
        }
        let mut chars = part.chars();
        let Some(first) = chars.next() else {
            anyhow::bail!("service.package {package:?} is not a nixpkgs attr path");
        };
        if !first.is_ascii_alphanumeric() && first != '_' {
            anyhow::bail!("service.package {package:?} is not a nixpkgs attr path");
        }
        if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            anyhow::bail!("service.package {package:?} is not a nixpkgs attr path");
        }
    }
    Ok(())
}

/// Render `pkgs.<attr>` with hyphenated components quoted.
pub fn nixpkgs_attr_expr(package: &str) -> anyhow::Result<String> {
    validate_package_attr(package)?;
    let mut out = String::from("pkgs");
    for part in package.split('.') {
        if is_nix_ident(part) {
            out.push('.');
            out.push_str(part);
        } else {
            out.push_str(".\"");
            out.push_str(part);
            out.push('"');
        }
    }
    Ok(out)
}

fn is_nix_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `service.args` is process argv after the entrypoint, on both runtimes: the
/// container gets it after `--rootfs … <entrypoint>`, the microVM guest agent
/// reads it one entry per line from `/config/argv` (#463). Entries can hold
/// no newline, so that file needs no escaping.
pub fn validate_service_args(args: &[String]) -> anyhow::Result<()> {
    validate_token_list("service.args", args, true, "")
}

/// `service.podman_args` are extra `podman run` tokens, one per entry. Only
/// the shape is checked here; the control plane rejects Russel-owned and
/// isolation-weakening flags fail-closed at deploy.
pub fn validate_podman_args(args: &[String], runtime_is_container: bool) -> anyhow::Result<()> {
    validate_token_list(
        "service.podman_args",
        args,
        runtime_is_container,
        "they are `podman run` flags",
    )
}

fn validate_token_list(
    field: &str,
    args: &[String],
    runtime_is_container: bool,
    container_only_reason: &str,
) -> anyhow::Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    if !runtime_is_container {
        anyhow::bail!("{field} requires service.type = \"container\" ({container_only_reason})");
    }
    if args.len() > 32 {
        anyhow::bail!("{field}: too many entries (max 32)");
    }
    for arg in args {
        if arg.contains('\0') || arg.contains('\n') || arg.contains('\r') {
            anyhow::bail!("{field} entry must not contain NUL or newlines");
        }
        if arg.len() > 256 {
            anyhow::bail!("{field} entry too long (max 256 bytes)");
        }
    }
    Ok(())
}

/// `service.user` (#466): omitted runs the app as an unprivileged user on
/// both runtimes; `"root"` is the only value.
pub fn validate_user(user: Option<&str>) -> anyhow::Result<()> {
    match user {
        None | Some("root") => Ok(()),
        Some(other) => anyhow::bail!(
            "service.user must be \"root\" or omitted (got {other:?}); omitted runs the app \
             as an unprivileged user"
        ),
    }
}

/// `service.userns` was replaced by `service.user` (#466).
pub fn reject_userns(userns: Option<&str>) -> anyhow::Result<()> {
    if userns.is_some() {
        anyhow::bail!(
            "service.userns was removed: apps now run as an unprivileged user by default, \
             which is what userns = \"keep-id\" did. Delete the line, or set \
             service.user = \"root\" to run the app as root"
        );
    }
    Ok(())
}

pub fn validate_restart(restart: Option<&str>) -> anyhow::Result<()> {
    let Some(restart) = restart else {
        return Ok(());
    };
    if restart != "unless-stopped" {
        anyhow::bail!("service.restart must be \"unless-stopped\" (got {restart:?})");
    }
    Ok(())
}

/// Bind an absolute host path. `roots` is `RUSSEL_VOLUME_ROOTS`.
pub fn resolve_absolute_host(host: &str, roots: &[PathBuf]) -> anyhow::Result<PathBuf> {
    validate_host_path_syntax(host)?;
    if roots.is_empty() {
        anyhow::bail!(
            "absolute volume host {host} requires RUSSEL_VOLUME_ROOTS \
             (colon-separated absolute prefixes)"
        );
    }
    let resolved = canonicalize_with_ancestors(Path::new(host))
        .map_err(|e| anyhow::anyhow!("volume host path {host} could not be canonicalized: {e}"))?;
    let resolved_str = resolved
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 volume host path"))?;
    reject_mount_csv_metacharacters(resolved_str, "volume host path")?;
    if let Some(prefix) = path_hits_denied_prefix(resolved_str) {
        anyhow::bail!("volume host path {resolved_str} is denied ({prefix})");
    }
    let under_root = roots.iter().any(|root| {
        let root = canonicalize_with_ancestors(root).unwrap_or_else(|_| root.clone());
        resolved == root || resolved.starts_with(&root)
    });
    if !under_root {
        anyhow::bail!("volume host path {resolved_str} is not under RUSSEL_VOLUME_ROOTS");
    }
    Ok(resolved)
}

/// Canonicalize existing ancestors, then join the missing suffix.
/// Resolves symlinked parents even when the final path does not exist yet.
fn canonicalize_with_ancestors(path: &Path) -> anyhow::Result<PathBuf> {
    if path.exists() {
        return Ok(path.canonicalize()?);
    }
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        let name = cur
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("no existing ancestor for {}", path.display()))?
            .to_os_string();
        suffix.push(name);
        if !cur.pop() {
            anyhow::bail!("no existing ancestor for {}", path.display());
        }
        if cur.exists() {
            let mut resolved = cur.canonicalize()?;
            for part in suffix.iter().rev() {
                resolved.push(part);
            }
            return Ok(resolved);
        }
    }
}

/// Resolve file volumes to host paths. Creates nothing; caller mkdir managed dirs.
pub fn resolve_volumes(
    service_id: &str,
    volumes: &[VolumeSpec],
    roots: &[PathBuf],
) -> anyhow::Result<Vec<ResolvedVolume>> {
    crate::ids::validate_service_id(service_id)?;
    let mut out = Vec::with_capacity(volumes.len());
    for vol in volumes {
        if let Some(name) = vol.name.as_deref() {
            out.push(ResolvedVolume {
                guest: vol.guest.clone(),
                host_path: managed_volume_dir(service_id, name)?,
                rw: vol.rw,
                keep: vol.keep,
                managed: true,
                name: Some(name.to_string()),
            });
        } else if let Some(host) = vol.host.as_deref() {
            out.push(ResolvedVolume {
                guest: vol.guest.clone(),
                host_path: resolve_absolute_host(host, roots)?,
                rw: vol.rw,
                keep: false,
                managed: false,
                name: None,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn vol(toml: &str) -> VolumeSpec {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn managed_volume_parses() {
        let v = vol(r#"
name = "data"
guest = "/data"
rw = true
keep = true
"#);
        assert_eq!(v.name.as_deref(), Some("data"));
        assert!(v.host.is_none());
        assert!(v.rw);
        assert!(v.keep);
        validate_volumes(&[v]).unwrap();
    }

    #[test]
    fn host_volume_rejects_keep() {
        let v = vol(r#"
host = "/home/me/music"
guest = "/music"
keep = true
"#);
        let err = validate_volumes(&[v]).unwrap_err();
        assert!(err.to_string().contains("keep is only valid with name"));
    }

    #[test]
    fn volumes_load_on_either_runtime() {
        let v = vol("name = \"data\"\nguest = \"/data\"\n");
        validate_volumes(&[v]).unwrap();
    }

    #[test]
    fn guest_must_be_absolute() {
        let v = vol("name = \"data\"\nguest = \"data\"\n");
        let err = validate_volumes(&[v]).unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn guest_rejects_root() {
        let v = vol("name = \"data\"\nguest = \"/\"\n");
        let err = validate_volumes(&[v]).unwrap_err();
        assert!(err.to_string().contains("must not be \"/\""));
        let v = vol("name = \"data\"\nguest = \"///\"\n");
        let err = validate_volumes(&[v]).unwrap_err();
        assert!(err.to_string().contains("must not be \"/\""));
    }

    #[test]
    fn guest_rejects_parent_dir() {
        let v = vol("name = \"data\"\nguest = \"/data/../etc\"\n");
        let err = validate_volumes(&[v]).unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn host_denied_etc() {
        let v = vol("host = \"/etc/shadow\"\nguest = \"/secret\"\n");
        let err = validate_volumes(&[v]).unwrap_err();
        assert!(err.to_string().contains("denied"));
    }

    #[test]
    fn host_denied_russel_state() {
        for host in [
            "/var/lib/russel/secrets/token",
            "/var/lib/russel/other/volumes/data",
            "/var/lib/microvms/api",
        ] {
            let v = vol(&format!("host = {host:?}\nguest = \"/data\"\n"));
            let err = validate_volumes(&[v]).unwrap_err();
            assert!(
                err.to_string().contains("denied"),
                "{host} should be denied, got {err}"
            );
        }
    }

    #[test]
    fn guest_and_host_reject_comma() {
        let guest = vol("name = \"data\"\nguest = \"/data,bind-propagation=rshared\"\n");
        let err = validate_volumes(&[guest]).unwrap_err();
        assert!(err.to_string().contains("commas"), "{err}");

        let host = vol("host = \"/srv/music,Z\"\nguest = \"/music\"\n");
        let err = validate_volumes(&[host]).unwrap_err();
        assert!(err.to_string().contains("commas"), "{err}");
    }

    #[test]
    fn guest_and_host_reject_newline_and_cr() {
        for toml in [
            "name = \"data\"\nguest = \"/data\\nfoo\"\n",
            "name = \"data\"\nguest = \"/data\\rfoo\"\n",
            "host = \"/srv/music\\nZ\"\nguest = \"/music\"\n",
            "host = \"/srv/music\\rZ\"\nguest = \"/music\"\n",
        ] {
            let err = validate_volumes(&[vol(toml)]).unwrap_err();
            assert!(
                err.to_string().contains("newline") || err.to_string().contains("commas"),
                "{err}"
            );
        }
    }

    #[test]
    fn host_denied_via_symlinked_secrets_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real-secrets");
        std::fs::create_dir_all(&real).unwrap();
        let link = tmp.path().join("link-secrets");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let target = real.join("token");
        std::fs::create_dir_all(&target).unwrap();

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
        let _restore = EnvRestore(std::env::var_os("RUSSEL_SECRETS_DIR"));
        unsafe {
            std::env::set_var("RUSSEL_SECRETS_DIR", &link);
        }

        let err = resolve_absolute_host(target.to_str().unwrap(), std::slice::from_ref(&real))
            .unwrap_err();
        assert!(
            err.to_string().contains("denied"),
            "canonical secrets path must be denied when env is a symlink: {err}"
        );
    }

    #[test]
    fn name_or_host_required() {
        let v = vol("guest = \"/data\"\n");
        let err = validate_volumes(&[v]).unwrap_err();
        assert!(err.to_string().contains("name = or host ="));
    }

    #[test]
    fn extra_ports_reject_service_port() {
        let err = validate_extra_ports(
            &[ExtraPortSpec {
                host: 50300,
                guest: 3000,
            }],
            3000,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicates service.port"));
    }

    #[test]
    fn extra_ports_reject_privileged() {
        let err = validate_extra_ports(
            &[ExtraPortSpec {
                host: 80,
                guest: 8080,
            }],
            3000,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("privileged"));
    }

    #[test]
    fn package_attr_and_nix_expr() {
        validate_package_attr("navidrome").unwrap();
        validate_package_attr("nodePackages.foo-bar").unwrap();
        assert_eq!(nixpkgs_attr_expr("navidrome").unwrap(), "pkgs.navidrome");
        assert_eq!(
            nixpkgs_attr_expr("nodePackages.foo-bar").unwrap(),
            "pkgs.nodePackages.\"foo-bar\""
        );
        assert!(validate_package_attr("../evil").is_err());
        assert!(validate_package_attr("").is_err());
        assert!(nixpkgs_attr_expr("../evil").is_err());
        assert!(nixpkgs_attr_expr("").is_err());
    }

    #[test]
    fn extra_port_key_uses_colon_delimiter() {
        // `:` is rejected by validate_service_id, so foo::x0 cannot be a sibling
        // service id (unlike the old foo__x0 form).
        assert_eq!(extra_port_key("foo", 0), "foo::x0");
        assert_eq!(extra_port_key("foo", 1), "foo::x1");
        assert!(!extra_port_key("foo", 0).starts_with("foo__x"));
    }

    #[test]
    fn destroy_policy_keep_override() {
        assert_eq!(
            VolumeDestroyPolicy::from_keep_override(None),
            VolumeDestroyPolicy::FollowFile
        );
        assert_eq!(
            VolumeDestroyPolicy::from_keep_override(Some(true)),
            VolumeDestroyPolicy::KeepAll
        );
        assert!(VolumeDestroyPolicy::FollowFile.keep_managed(true));
        assert!(!VolumeDestroyPolicy::FollowFile.keep_managed(false));
        assert!(VolumeDestroyPolicy::KeepAll.keep_managed(false));
        assert!(!VolumeDestroyPolicy::DeleteAll.keep_managed(true));
    }

    #[test]
    fn absolute_host_requires_roots() {
        let err = resolve_absolute_host("/home/me/music", &[]).unwrap_err();
        assert!(err.to_string().contains("RUSSEL_VOLUME_ROOTS"));
    }

    #[test]
    fn absolute_host_must_be_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ok");
        std::fs::create_dir_all(&root).unwrap();
        let music = root.join("music");
        std::fs::create_dir_all(&music).unwrap();
        let got =
            resolve_absolute_host(music.to_str().unwrap(), std::slice::from_ref(&root)).unwrap();
        assert_eq!(got, music.canonicalize().unwrap());

        let other = tmp.path().join("nope");
        std::fs::create_dir_all(&other).unwrap();
        let err = resolve_absolute_host(other.to_str().unwrap(), &[root]).unwrap_err();
        assert!(err.to_string().contains("not under RUSSEL_VOLUME_ROOTS"));
    }

    #[test]
    fn absolute_host_rejects_canonical_path_with_comma() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ok");
        std::fs::create_dir_all(&root).unwrap();
        let comma_name = root.join("data,bind-propagation=rprivate");
        std::fs::create_dir_all(&comma_name).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&comma_name, &link).unwrap();

        let err =
            resolve_absolute_host(link.to_str().unwrap(), std::slice::from_ref(&root)).unwrap_err();
        assert!(
            err.to_string().contains("commas"),
            "canonical path with a comma must not reach podman: {err}"
        );
    }

    #[test]
    fn absolute_host_resolves_symlinked_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ok");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = root.join("link-out");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let missing = link.join("newdir");
        let err = resolve_absolute_host(missing.to_str().unwrap(), std::slice::from_ref(&root))
            .unwrap_err();
        assert!(
            err.to_string().contains("not under RUSSEL_VOLUME_ROOTS")
                || err.to_string().contains("denied"),
            "symlinked ancestor must not bypass roots: {err}"
        );

        let allowed_missing = root.join("future");
        let got = resolve_absolute_host(
            allowed_missing.to_str().unwrap(),
            std::slice::from_ref(&root),
        )
        .unwrap();
        assert_eq!(got, root.canonicalize().unwrap().join("future"));
    }

    #[test]
    fn managed_volume_dir_rejects_traversal() {
        assert!(managed_volume_dir("", "data").is_err());
        assert!(managed_volume_dir("a/b", "data").is_err());
        assert!(managed_volume_dir("..", "data").is_err());
        assert!(managed_volume_dir("svc", "../etc").is_err());
        assert!(
            managed_volume_dir("svc", "data")
                .unwrap()
                .ends_with("svc/volumes/data")
        );
        assert!(resolve_volumes("a/b", &[], &[]).is_err());
    }

    #[test]
    fn user_root_or_omitted_userns_removed_restart_either_runtime() {
        validate_user(None).unwrap();
        validate_user(Some("root")).unwrap();
        assert!(validate_user(Some("1000")).is_err());
        reject_userns(None).unwrap();
        let err = reject_userns(Some("keep-id")).unwrap_err().to_string();
        assert!(err.contains("service.user"), "{err}");
        validate_restart(Some("unless-stopped")).unwrap();
        assert!(validate_restart(Some("always")).is_err());
    }
}
