use std::collections::HashMap;
use std::{fmt, fs, path::Path, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::volumes::{
    ExtraPortSpec, VolumeSpec, reject_userns, validate_extra_ports, validate_package_attr,
    validate_podman_args, validate_restart, validate_service_args, validate_user, validate_volumes,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Russelfile {
    pub service: ServiceConfig,
    #[serde(default)]
    pub ingress: Option<IngressConfig>,
    /// Bind mounts. Container runtime only. Rootfs stays read-only.
    #[serde(default)]
    pub volumes: Vec<VolumeSpec>,
    /// Extra published ports besides `service.port` / `[ingress].port`.
    /// Container runtime only.
    #[serde(default)]
    pub ports: Vec<ExtraPortSpec>,
}

impl Russelfile {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = fs::read_to_string(path)?;
        Self::load_from_str(&contents)
    }

    /// Parse from a string — shared by `load` and tests.
    pub fn load_from_str(contents: &str) -> anyhow::Result<Self> {
        let mut config: Self = toml::from_str(contents)?;
        // One ASCII rule for `service.name` and the service id; the wider
        // `[A-Za-z0-9._+-]` rule belongs to `bin` only.
        crate::ids::validate_service_id(&config.service.name)
            .map_err(|e| anyhow::anyhow!("service.name {:?}: {e}", config.service.name))?;
        if let Some(bin) = config.service.bin.as_deref() {
            crate::ids::validate_bin_name(bin).map_err(|e| anyhow::anyhow!("service.bin: {e}"))?;
        }
        validate_env_map(&config.service.env).map_err(|e| anyhow::anyhow!("[service.env]: {e}"))?;
        if config.service.port == 0 {
            anyhow::bail!("service.port must not be 0");
        }
        if let Some(ref mut ingress) = config.ingress {
            if let Some(host) = ingress.host.as_mut() {
                let normalized = host.trim().to_ascii_lowercase();
                if normalized.is_empty() {
                    anyhow::bail!("ingress.host must not be empty");
                }
                if !is_valid_dns_name(&normalized) {
                    anyhow::bail!("ingress.host {host:?} is not a valid DNS name");
                }
                *host = normalized;
            }
            if let Some(port) = ingress.port {
                validate_publish_host_port("ingress.port", port)?;
            }
        }
        if config.service.cpus < 1 || config.service.cpus > 32 {
            anyhow::bail!("service.cpus must be 1..=32 (got {})", config.service.cpus);
        }
        if config.service.guest == GuestKind::Linux {
            anyhow::bail!(
                "guest = \"linux\" is not implemented yet (omit guest or set guest = \"busybox\"; \
                 linux is a host-built NixOS userspace)"
            );
        }
        validate_source_path(&config.service.source)?;
        let is_container = config.service.runtime == RuntimeKind::Container;
        if let Some(ref package) = config.service.package {
            validate_package_attr(package)?;
        }
        validate_service_args(&config.service.args)?;
        validate_podman_args(&config.service.podman_args, is_container)?;
        validate_user(config.service.user.as_deref())?;
        reject_userns(config.service.userns.as_deref())?;
        validate_restart(config.service.restart.as_deref())?;
        validate_volumes(&config.volumes)?;
        validate_extra_ports(
            &config.ports,
            config.service.port,
            config.ingress.as_ref().and_then(|i| i.port),
        )?;
        Ok(config)
    }
}

/// Validate that `name` is a sane DNS-style name.
///
/// Rules: labels contain only `[A-Za-z0-9-]`, each label is 1..=63 chars,
/// no leading/trailing hyphen in a label, and total length is at most 253.
/// Single-label names are allowed.
pub fn is_valid_dns_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return false;
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return false;
        }
    }
    true
}

/// Validate `service.source` path: must be relative, no `..`, not absolute, not empty.
/// When source is `"."` it is always valid. Callers must further verify the directory
/// exists under the repo path at deploy time.
pub fn validate_source_path(source: &str) -> anyhow::Result<()> {
    if source.is_empty() {
        anyhow::bail!("service.source must not be empty");
    }
    if source == "." {
        return Ok(());
    }
    let p = Path::new(source);
    if p.is_absolute() {
        anyhow::bail!("service.source must be a relative path (got: {source})");
    }
    for component in p.components() {
        match component {
            std::path::Component::ParentDir => {
                anyhow::bail!("service.source must not contain '..' (got: {source})");
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                anyhow::bail!("service.source must be relative (got: {source})");
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    Microvm,
    /// Default when `service.type` is omitted: microVMs are experimental in
    /// v0.1 and need a privileged ctrl (#473).
    #[default]
    Container,
}

impl fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Microvm => write!(f, "microvm"),
            Self::Container => write!(f, "container"),
        }
    }
}

impl FromStr for RuntimeKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "microvm" => Ok(Self::Microvm),
            "container" => Ok(Self::Container),
            other => {
                anyhow::bail!("unknown runtime {other:?}, expected \"microvm\" or \"container\"")
            }
        }
    }
}

/// Guest userspace. Orthogonal to [`RuntimeKind`] (isolation).
///
/// `busybox` is today's service guest: pid 1 mounts the store and execs one
/// ELF (microVM) or a hardened rootfs with no distro (container).
/// `linux` is a host-built NixOS userspace. Serde parses the flag;
/// [`Russelfile::load_from_str`] rejects it until the boot path lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GuestKind {
    #[default]
    Busybox,
    Linux,
}

impl fmt::Display for GuestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busybox => write!(f, "busybox"),
            Self::Linux => write!(f, "linux"),
        }
    }
}

impl FromStr for GuestKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "busybox" => Ok(Self::Busybox),
            "linux" => Ok(Self::Linux),
            other => anyhow::bail!("unknown guest {other:?}, expected \"busybox\" or \"linux\""),
        }
    }
}

/// Normalize and DNS-validate Russelfile `[ingress].host`, so a bad `Host()`
/// value fails before Traefik.
pub fn resolve_ingress_host(file: Option<&str>) -> anyhow::Result<Option<String>> {
    file.map(normalize_ingress_host).transpose()
}

fn normalize_ingress_host(host: &str) -> anyhow::Result<String> {
    let normalized = host.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        anyhow::bail!("ingress.host must not be empty");
    }
    if !is_valid_dns_name(&normalized) {
        anyhow::bail!("ingress.host {host:?} is not a valid DNS name");
    }
    Ok(normalized)
}

/// Reject a host-side ingress port that file pins already reject at load:
/// 0, privileged (< 1024), and the default ctrl/agent binds as a hint.
/// The live `RUSSEL_CTRL_ADDR` / `RUSSEL_AGENT_ADDR` collision check stays
/// deploy-time in the pipeline, which sees the live process env.
/// The one host-side publish rule, shared by `[ingress].port` and
/// `[[ports]]` hosts. `field` names the source in the error.
pub fn validate_publish_host_port(field: &str, port: u16) -> anyhow::Result<()> {
    if port == 0 {
        anyhow::bail!("{field} must not be 0");
    }
    if port < 1024 {
        anyhow::bail!("{field} {port} is privileged (< 1024); Traefik owns 80/443");
    }
    if port == 7878 || port == 7946 {
        anyhow::bail!(
            "{field} {port} collides with the default ctrl (7878) or agent (7946) listen port"
        );
    }
    Ok(())
}

/// The primary publish for a deploy: `service.port` is always the guest side
/// (and `PORT`); `[ingress].port` only pins the host side. `[[ports]]` rows
/// are additional listeners; load already rejects one reusing that host.
pub fn resolve_primary_publish(config: &Russelfile) -> Option<crate::api::PortMapping> {
    let host = config.ingress.as_ref().and_then(|i| i.port)?;
    Some(crate::api::PortMapping {
        host,
        guest: config.service.port,
    })
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressConfig {
    /// Exact Traefik Host() value, e.g. "abc.com" or "api.abc.com".
    pub host: Option<String>,
    /// Host-side backend port (Traefik + publish). Not the guest listen port.
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    /// Same rule as a service id ([`crate::ids::validate_service_id`]):
    /// 1..=128 ASCII `[A-Za-z0-9_-]`, not a reserved data-root dir.
    pub name: String,
    pub source: String,
    pub port: u16,
    pub memory: Memory,
    /// Runtime kind (`container`, the default, or `microvm`). TOML field is `type`.
    #[serde(default, rename = "type")]
    pub runtime: RuntimeKind,
    /// Guest userspace (`busybox` or `linux`). Default busybox.
    #[serde(default)]
    pub guest: GuestKind,
    /// Name of the binary produced by the build.
    /// Defaults to `name` if not specified. Wider charset than `name`:
    /// [`crate::ids::validate_bin_name`] (`[A-Za-z0-9._+-]`, max 256).
    pub bin: Option<String>,
    /// User-defined environment variables injected at deploy time.
    /// Checked at load with [`validate_env_map`].
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// When true, include bash + curl debug tools and a `/usr/bin/env` wrapper
    /// in the container rootfs. Required for entrypoints that use
    /// `#!/usr/bin/env bash` shebangs. Defaults to false for production
    /// hardening (no debug tools, no env wrapper — entrypoints must be
    /// statically-linked ELF binaries or use an absolute `/nix/store/…`
    /// interpreter path).
    #[serde(default)]
    pub debug: bool,
    /// CPUs (1..=32): microVM vCPUs, or the container `--cpus` limit.
    /// Defaults to 1 when omitted.
    #[serde(default = "default_cpus")]
    pub cpus: u8,
    /// nixpkgs attribute to wrap when no committed `flake.nix` exists
    /// (e.g. `"navidrome"`). A committed flake always wins.
    #[serde(default)]
    pub package: Option<String>,
    /// Process argv after the entrypoint binary (`$out/bin/<bin> <args…>`),
    /// on both runtimes. Not the same thing as the CLI's `--podman-arg`, which
    /// adds flags to `podman run` and never reaches the process.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra `podman run` tokens, one per entry (container only). Not
    /// process argv: that is `args`. Validated fail-closed by the control plane.
    #[serde(default)]
    pub podman_args: Vec<String>,
    /// Who the app runs as, on both runtimes (#466). Omitted: an unprivileged
    /// user (the control plane's own uid, or `nobody` under a root ctrl).
    /// `"root"` is the only value.
    #[serde(default)]
    pub user: Option<String>,
    /// Removed (#466): loading a Russelfile that sets it fails with a pointer
    /// to `user`. Kept only to give that message.
    #[serde(default)]
    pub userns: Option<String>,
    /// Only `"unless-stopped"` is accepted. Containers get Podman's restart
    /// policy; microVMs are relaunched by ctrl.
    #[serde(default)]
    pub restart: Option<String>,
}

pub fn default_cpus() -> u8 {
    1
}

impl ServiceConfig {
    /// Entrypoint under `$out/bin/`. Prefers explicit `bin`, else `name`.
    ///
    /// Ignores `package` so a committed flake cannot select the wrong binary
    /// via the package attr. When the build wraps `service.package`, call
    /// [`Self::bin_name_for_build`] with `using_package = true`.
    pub fn bin_name(&self) -> &str {
        self.bin_name_for_build(false)
    }

    /// Like [`Self::bin_name`]. When `using_package` is true and `bin` is
    /// unset, use the last component of `service.package`.
    pub fn bin_name_for_build(&self, using_package: bool) -> &str {
        if let Some(bin) = self.bin.as_deref() {
            return bin;
        }
        if using_package && let Some(package) = self.package.as_deref() {
            return package.rsplit('.').next().unwrap_or(package);
        }
        &self.name
    }
}

/// Validate a single env key name.
/// Allowed: `^[A-Za-z_][A-Za-z0-9_]*$`. Rejected: empty, leading digit, bad chars.
pub fn validate_env_key(key: &str) -> anyhow::Result<()> {
    if key.is_empty() {
        anyhow::bail!("env key must not be empty");
    }
    let first = key.as_bytes()[0];
    if !first.is_ascii_alphabetic() && first != b'_' {
        anyhow::bail!("env key '{}' must start with a letter or underscore", key);
    }
    if !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        anyhow::bail!(
            "env key '{}' contains invalid characters (only A-Za-z0-9_ allowed)",
            key
        );
    }
    Ok(())
}

/// Reserved env keys that users may not set.
/// Denylist of env vars that would alter guest init behavior or weaken isolation.
const RESERVED_ENV_KEYS: &[&str] = &[
    "PORT",
    "VM_IP",
    "HOST_IP",
    "APP",
    "IFS",
    "PATH",
    "LD_PRELOAD",
    // glibc runs an audit module from LD_AUDIT the same way LD_PRELOAD injects a library.
    "LD_AUDIT",
    "LD_LIBRARY_PATH",
    "BASH_ENV",
    "ENV",
    "SHELL",
];

/// Validate a full env map: keys, reserved keys, value length, value content, key count.
pub fn validate_env_map(env: &std::collections::HashMap<String, String>) -> anyhow::Result<()> {
    if env.len() > 64 {
        anyhow::bail!("too many env keys: {} (max 64)", env.len());
    }
    for (key, value) in env {
        validate_env_key(key)?;
        if RESERVED_ENV_KEYS.contains(&key.as_str()) {
            anyhow::bail!("env key '{}' is reserved and cannot be set by user", key);
        }
        if value.contains('\0') {
            anyhow::bail!("env key '{}' value contains NUL byte", key);
        }
        if value.contains('\n') || value.contains('\r') {
            anyhow::bail!(
                "env key '{}' value contains newline or carriage return",
                key
            );
        }
        if value.len() > 4096 {
            anyhow::bail!(
                "env key '{}' value too long: {} bytes (max 4096)",
                key,
                value.len()
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum Memory {
    Mebibytes(u16),
}

impl Memory {
    pub fn as_mebibytes(&self) -> u16 {
        match self {
            Self::Mebibytes(value) => *value,
        }
    }
}

impl<'de> Deserialize<'de> for Memory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let normalized = value.trim().to_ascii_lowercase();
        let mebibytes: u16 = normalized
            .strip_suffix("mb")
            .or_else(|| normalized.strip_suffix("mib"))
            .ok_or_else(|| serde::de::Error::custom("memory must end in mb or mib"))?
            .parse()
            .map_err(|_| serde::de::Error::custom("memory must be a number followed by mb"))?;

        if mebibytes < 16 {
            return Err(serde::de::Error::custom("memory must be at least 16mb"));
        }

        Ok(Self::Mebibytes(mebibytes))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn minimal_russelfile(ingress: Option<&str>) -> String {
        let ingress = ingress
            .map(|fields| format!("\n[ingress]\n{fields}"))
            .unwrap_or_default();
        format!(
            "\n[service]\nname = \"app\"\nsource = \".\"\nport = 3000\nmemory = \"256mb\"{ingress}\n"
        )
    }

    #[test]
    fn parse_minimal_russelfile() {
        let toml = r#"
[service]
name = "my-app"
source = "."
port = 3000
memory = "256mb"
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(config.service.name, "my-app");
        assert_eq!(config.service.port, 3000);
        assert_eq!(config.service.bin_name(), "my-app");
        assert_eq!(config.service.memory.as_mebibytes(), 256);
    }
    #[test]
    fn custom_bin_name_takes_priority() {
        let toml = r#"
[service]
name = "my-app"
source = "."
port = 3000
memory = "256mb"
bin = "custom-binary"
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(config.service.bin_name(), "custom-binary");
    }

    #[test]
    fn memory_parses_mb_and_mib() {
        let toml_mb = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "512mb"
"#;
        let config: Russelfile = toml::from_str(toml_mb).unwrap();
        assert_eq!(config.service.memory.as_mebibytes(), 512);

        let toml_mib = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "1024mib"
"#;
        let config: Russelfile = toml::from_str(toml_mib).unwrap();
        assert_eq!(config.service.memory.as_mebibytes(), 1024);
    }

    #[test]
    fn memory_rejects_invalid_suffix() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "512gb"
"#;
        let err = toml::from_str::<Russelfile>(toml).unwrap_err();
        assert!(err.to_string().contains("mb"));
    }

    #[test]
    fn database_table_is_not_part_of_the_schema() {
        // Postgres / Redis are ordinary `service.package` apps with
        // `[[volumes]]`. There is no `[database]` section, placeholder or not.
        for table in [
            "[database.postgres]\nenabled = false",
            "[database.redis]\nenabled = false",
            "[database.mysql]\nenabled = true",
        ] {
            let toml = format!("{}\n{table}\n", minimal_russelfile(None));
            let err = Russelfile::load_from_str(&toml).unwrap_err();
            assert!(
                err.to_string().contains("unknown field `database`"),
                "expected [database] to be rejected: {err}"
            );
        }
    }

    #[test]
    fn deny_unknown_fields() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
typo_field = "oops"
"#;
        let err = toml::from_str::<Russelfile>(toml).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn runtime_defaults_to_container_when_omitted() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(config.service.runtime, RuntimeKind::Container);
    }

    #[test]
    fn runtime_parses_container() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
type = "container"
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(config.service.runtime, RuntimeKind::Container);
    }

    #[test]
    fn runtime_rejects_unknown_value() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
type = "kubernetes"
"#;
        let err = toml::from_str::<Russelfile>(toml).unwrap_err();
        assert!(err.to_string().contains("kubernetes"));
    }

    #[test]
    fn guest_defaults_to_busybox_when_omitted() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
"#;
        let config = Russelfile::load_from_str(toml).unwrap();
        assert_eq!(config.service.guest, GuestKind::Busybox);
    }

    #[test]
    fn guest_parses_busybox() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
guest = "busybox"
"#;
        let config = Russelfile::load_from_str(toml).unwrap();
        assert_eq!(config.service.guest, GuestKind::Busybox);
    }

    #[test]
    fn guest_linux_rejected_at_load() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
guest = "linux"
"#;
        let parsed: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(parsed.service.guest, GuestKind::Linux);
        let err = Russelfile::load_from_str(toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("linux"), "expected linux in error: {msg}");
        assert!(
            msg.contains("not implemented"),
            "expected not implemented in error: {msg}"
        );
    }

    #[test]
    fn guest_rejects_unknown_value() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
guest = "ubuntu"
"#;
        let err = toml::from_str::<Russelfile>(toml).unwrap_err();
        assert!(err.to_string().contains("ubuntu"));
    }

    #[test]
    fn ingress_table_parses_all_supported_shapes() {
        let host_only =
            Russelfile::load_from_str(&minimal_russelfile(Some("host = \"ABC.com\""))).unwrap();
        let ingress = host_only.ingress.unwrap();
        assert_eq!(ingress.host.as_deref(), Some("abc.com"));
        assert_eq!(ingress.port, None);

        let port_only =
            Russelfile::load_from_str(&minimal_russelfile(Some("port = 4000"))).unwrap();
        let ingress = port_only.ingress.unwrap();
        assert_eq!(ingress.host, None);
        assert_eq!(ingress.port, Some(4000));

        let both = Russelfile::load_from_str(&minimal_russelfile(Some(
            "host = \"api.abc.com\"\nport = 4000",
        )))
        .unwrap();
        let ingress = both.ingress.unwrap();
        assert_eq!(ingress.host.as_deref(), Some("api.abc.com"));
        assert_eq!(ingress.port, Some(4000));

        let empty = Russelfile::load_from_str(&minimal_russelfile(Some(""))).unwrap();
        let ingress = empty.ingress.unwrap();
        assert_eq!(ingress.host, None);
        assert_eq!(ingress.port, None);

        let omitted = Russelfile::load_from_str(&minimal_russelfile(None)).unwrap();
        assert!(omitted.ingress.is_none());
    }

    #[test]
    fn ingress_unknown_fields_are_rejected() {
        for field in [
            "domain = \"abc.com\"",
            "host_port = 4000",
            "hosts = [\"abc.com\"]",
            "tunnel = \"cloudflared\"",
        ] {
            let err = Russelfile::load_from_str(&minimal_russelfile(Some(field))).unwrap_err();
            assert!(
                err.to_string().contains("unknown field"),
                "expected {field:?} to be rejected as unknown: {err}"
            );
        }
    }

    #[test]
    fn ingress_hosts_are_validated_and_normalized_at_load() {
        let config =
            Russelfile::load_from_str(&minimal_russelfile(Some("host = \"  API.Example.COM  \"")))
                .unwrap();
        assert_eq!(
            config.ingress.unwrap().host.as_deref(),
            Some("api.example.com")
        );

        let too_long = format!(
            "host = \"{}.{}.{}.{}\"",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(63)
        );
        for (host, expected) in [
            ("", "ingress.host must not be empty"),
            ("   ", "ingress.host must not be empty"),
            ("bad`name", "is not a valid DNS name"),
            ("bad name", "is not a valid DNS name"),
            ("-bad.example", "is not a valid DNS name"),
            ("bad-.example", "is not a valid DNS name"),
            ("bad..example", "is not a valid DNS name"),
            ("bad.example.", "is not a valid DNS name"),
            ("bad_example", "is not a valid DNS name"),
            ("éxample.com", "is not a valid DNS name"),
        ] {
            let err =
                Russelfile::load_from_str(&minimal_russelfile(Some(&format!("host = {host:?}"))))
                    .unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "expected {host:?} to produce {expected:?}: {err}"
            );
        }
        let err = Russelfile::load_from_str(&minimal_russelfile(Some(&too_long))).unwrap_err();
        assert!(err.to_string().contains("is not a valid DNS name"));
    }

    #[test]
    fn ingress_ports_are_validated_at_load() {
        for (port, expected) in [
            (0, "ingress.port must not be 0"),
            (
                80,
                "ingress.port 80 is privileged (< 1024); Traefik owns 80/443",
            ),
            (
                443,
                "ingress.port 443 is privileged (< 1024); Traefik owns 80/443",
            ),
            (
                7878,
                "ingress.port 7878 collides with the default ctrl (7878) or agent (7946) listen port",
            ),
            (
                7946,
                "ingress.port 7946 collides with the default ctrl (7878) or agent (7946) listen port",
            ),
        ] {
            let err =
                Russelfile::load_from_str(&minimal_russelfile(Some(&format!("port = {port}"))))
                    .unwrap_err();
            assert_eq!(err.to_string(), expected);
        }

        let config = Russelfile::load_from_str(&minimal_russelfile(Some("port = 4000"))).unwrap();
        assert_eq!(config.ingress.unwrap().port, Some(4000));
    }

    #[test]
    fn valid_dns_names_are_accepted() {
        assert!(is_valid_dns_name("russel.local"));
        assert!(is_valid_dns_name("example.com"));
        assert!(is_valid_dns_name("sub.example.com"));
        assert!(is_valid_dns_name("a-b.c-123.local"));
        assert!(is_valid_dns_name("Example.COM"));
        assert!(is_valid_dns_name("localhost"));
        assert!(is_valid_dns_name(&format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        )));
    }

    #[test]
    fn invalid_dns_names_are_rejected() {
        assert!(!is_valid_dns_name(""));
        assert!(!is_valid_dns_name("russel local"));
        assert!(!is_valid_dns_name("russel`local"));
        assert!(!is_valid_dns_name("-russel.local"));
        assert!(!is_valid_dns_name("russel-.local"));
        assert!(!is_valid_dns_name("russel..local"));
        assert!(!is_valid_dns_name(".russel.local"));
        assert!(!is_valid_dns_name("russel.local."));
        assert!(!is_valid_dns_name("russel_local"));
        assert!(!is_valid_dns_name("éxample.com"));
        assert!(!is_valid_dns_name(&"a".repeat(64)));
        assert!(!is_valid_dns_name(&format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(63)
        )));
    }

    #[test]
    fn resolve_ingress_host_normalizes_and_validates_the_file_value() {
        assert_eq!(resolve_ingress_host(None).unwrap(), None);
        assert_eq!(
            resolve_ingress_host(Some("  ABC.com  ")).unwrap(),
            Some("abc.com".to_string())
        );
        for value in ["", "   "] {
            let err = resolve_ingress_host(Some(value)).unwrap_err();
            assert_eq!(err.to_string(), "ingress.host must not be empty");
        }
        for bad in ["bad`name", "not a host", "-lead.example.com", "a..b"] {
            let err = resolve_ingress_host(Some(bad)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("ingress.host {bad:?} is not a valid DNS name")
            );
        }
    }

    fn publish_config(extra: &str) -> Russelfile {
        Russelfile::load_from_str(&format!(
            "[service]\nname = \"api\"\nsource = \".\"\nport = 3000\nmemory = \"256mb\"\ntype = \"container\"\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn primary_publish_guest_is_always_service_port() {
        assert!(resolve_primary_publish(&publish_config("")).is_none());
        let p = resolve_primary_publish(&publish_config("[ingress]\nport = 8080\n")).unwrap();
        assert_eq!((p.host, p.guest), (8080, 3000));
    }

    #[test]
    fn primary_publish_host_cannot_reuse_a_ports_row() {
        let err = Russelfile::load_from_str(
            "[service]\nname = \"api\"\nsource = \".\"\nport = 3000\nmemory = \"256mb\"\n\
             type = \"container\"\n[ingress]\nport = 8080\n[[ports]]\nhost = 8080\nguest = 9000\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("8080"), "{err}");
    }

    #[test]
    fn podman_args_are_container_only_and_bounded() {
        let config = publish_config("podman_args = [\"-v\", \"/data:/data:ro\"]\n");
        assert_eq!(config.service.podman_args, vec!["-v", "/data:/data:ro"]);

        let err = Russelfile::load_from_str(
            "[service]\nname = \"api\"\nsource = \".\"\nport = 3000\nmemory = \"256mb\"\n\
             type = \"microvm\"\npodman_args = [\"-v\"]\n",
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("service.podman_args requires service.type = \"container\""),
            "{err}"
        );

        let many = vec!["\"-q\""; 33].join(", ");
        let err = Russelfile::load_from_str(&format!(
            "[service]\nname = \"api\"\nsource = \".\"\nport = 3000\nmemory = \"256mb\"\n\
             type = \"container\"\npodman_args = [{many}]\n"
        ))
        .unwrap_err();
        assert!(err.to_string().contains("too many entries"), "{err}");
    }

    #[test]
    fn service_env_defaults_to_empty() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert!(config.service.env.is_empty());
    }

    #[test]
    fn service_env_parses_table() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"

[service.env]
LOG_LEVEL = "info"
FEATURE_X = "1"
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(
            config.service.env.get("LOG_LEVEL"),
            Some(&"info".to_string())
        );
        assert_eq!(config.service.env.get("FEATURE_X"), Some(&"1".to_string()));
        assert_eq!(config.service.env.len(), 2);
    }

    #[test]
    fn validate_env_key_accepts_valid() {
        for key in &["FOO", "_bar", "A_B", "MY_VAR_1"] {
            validate_env_key(key).unwrap();
        }
    }

    #[test]
    fn validate_env_key_rejects_invalid() {
        for key in &["", "1FOO", "MY-VAR", "a.b", "BAZ!"] {
            assert!(
                validate_env_key(key).is_err(),
                "expected rejection for {key:?}"
            );
        }
    }

    #[test]
    fn validate_env_map_rejects_reserved_keys() {
        for key in &[
            "PORT",
            "VM_IP",
            "HOST_IP",
            "APP",
            "IFS",
            "PATH",
            "LD_PRELOAD",
            "LD_AUDIT",
            "LD_LIBRARY_PATH",
            "BASH_ENV",
            "ENV",
            "SHELL",
        ] {
            let mut map = HashMap::new();
            map.insert(key.to_string(), "val".to_string());
            let err = validate_env_map(&map).unwrap_err();
            assert!(
                err.to_string().contains("reserved"),
                "expected reserved for {key}: {err}"
            );
        }
    }

    #[test]
    fn validate_env_map_rejects_too_many_keys() {
        let mut map = HashMap::new();
        for i in 0..65 {
            map.insert(format!("KEY_{i}"), "v".to_string());
        }
        let err = validate_env_map(&map).unwrap_err();
        assert!(err.to_string().contains("too many env keys"));
    }

    #[test]
    fn validate_env_map_rejects_nul_byte() {
        let mut map = HashMap::new();
        map.insert("FOO".to_string(), "val\0nul".to_string());
        let err = validate_env_map(&map).unwrap_err();
        assert!(err.to_string().contains("NUL"));
    }

    #[test]
    fn validate_env_map_rejects_newline_in_value() {
        let mut map = HashMap::new();
        map.insert("FOO".to_string(), "val\nbar".to_string());
        let err = validate_env_map(&map).unwrap_err();
        assert!(
            err.to_string().contains("newline"),
            "expected newline rejection: {err}"
        );
    }

    #[test]
    fn validate_env_map_rejects_cr_in_value() {
        let mut map = HashMap::new();
        map.insert("FOO".to_string(), "val\rbar".to_string());
        let err = validate_env_map(&map).unwrap_err();
        assert!(
            err.to_string().contains("carriage return"),
            "expected cr rejection: {err}"
        );
    }

    #[test]
    fn validate_env_map_rejects_long_value() {
        let mut map = HashMap::new();
        map.insert("FOO".to_string(), "a".repeat(4097));
        let err = validate_env_map(&map).unwrap_err();
        assert!(err.to_string().contains("too long"));
    }

    #[test]
    fn reject_port_zero() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 0
memory = "256mb"
"#;
        let err = Russelfile::load_from_str(toml).unwrap_err();
        assert!(
            err.to_string().contains("port must not be 0"),
            "expected port 0 rejection: {err}"
        );
    }

    #[test]
    fn reject_memory_below_16mb() {
        for mem in &["0mb", "1mb", "15mb", "15mib"] {
            let toml = format!(
                r#"
[service]
name = "app"
source = "."
port = 3000
memory = "{mem}"
"#
            );
            let err = toml::from_str::<Russelfile>(&toml).unwrap_err();
            assert!(
                err.to_string().contains("at least 16mb"),
                "expected minimum rejection for {mem}: {err}"
            );
        }
    }

    #[test]
    fn accept_memory_16mb_min() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "16mb"
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(config.service.memory.as_mebibytes(), 16);
    }

    #[test]
    fn validate_source_accepts_dot() {
        validate_source_path(".").unwrap();
    }

    #[test]
    fn validate_source_accepts_relative() {
        validate_source_path("subdir").unwrap();
        validate_source_path("sub/dir").unwrap();
    }

    #[test]
    fn validate_source_rejects_empty() {
        let err = validate_source_path("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validate_source_rejects_absolute() {
        let err = validate_source_path("/etc").unwrap_err();
        assert!(err.to_string().contains("must be a relative path"));
    }

    #[test]
    fn validate_source_rejects_parent_dir() {
        let err = validate_source_path("../escape").unwrap_err();
        assert!(err.to_string().contains("must not contain '..'"));
        let err = validate_source_path("sub/../../escape").unwrap_err();
        assert!(err.to_string().contains("must not contain '..'"));
    }

    #[test]
    fn cpus_defaults_to_1() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(config.service.cpus, 1);
    }

    #[test]
    fn cpus_parses_custom() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
cpus = 2
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(config.service.cpus, 2);
    }

    #[test]
    fn cpus_rejects_zero() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
cpus = 0
"#;
        let err = Russelfile::load_from_str(toml).unwrap_err();
        assert!(
            err.to_string().contains("cpus"),
            "expected cpus rejection: {err}"
        );
    }

    #[test]
    fn cpus_rejects_33() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
cpus = 33
"#;
        let err = Russelfile::load_from_str(toml).unwrap_err();
        assert!(
            err.to_string().contains("cpus"),
            "expected cpus rejection: {err}"
        );
    }

    #[test]
    fn package_and_volumes_load_for_container() {
        let toml = r#"
[service]
name = "navidrome"
source = "."
port = 4533
memory = "512mb"
type = "container"
package = "navidrome"
user = "root"
restart = "unless-stopped"
args = ["--loglevel", "info"]

[[volumes]]
name = "data"
guest = "/data"
rw = true
keep = true

[[ports]]
host = 4534
guest = 4534
"#;
        let config = Russelfile::load_from_str(toml).unwrap();
        assert_eq!(config.service.package.as_deref(), Some("navidrome"));
        assert_eq!(config.service.bin_name(), "navidrome");
        assert_eq!(config.volumes.len(), 1);
        assert!(config.volumes[0].keep);
        assert_eq!(config.ports[0].host, 4534);
        assert_eq!(config.service.user.as_deref(), Some("root"));
    }

    #[test]
    fn userns_fails_load_with_a_pointer_to_user() {
        let toml = r#"
[service]
name = "pg"
source = "."
port = 5432
memory = "256mb"
userns = "keep-id"
"#;
        let err = Russelfile::load_from_str(toml).unwrap_err().to_string();
        assert!(err.contains("service.user"), "{err}");
    }

    #[test]
    fn navidrome_and_vaultwarden_examples_pin_data_dirs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let navidrome = Russelfile::load(&root.join("examples/navidrome/Russelfile.toml")).unwrap();
        assert_eq!(
            navidrome
                .service
                .env
                .get("ND_DATAFOLDER")
                .map(String::as_str),
            Some("/data")
        );
        assert_eq!(
            navidrome
                .service
                .env
                .get("ND_MUSICFOLDER")
                .map(String::as_str),
            Some("/music")
        );
        let data = navidrome
            .volumes
            .iter()
            .find(|v| v.guest == "/data")
            .unwrap();
        assert!(data.rw && data.keep);
        let music = navidrome
            .volumes
            .iter()
            .find(|v| v.guest == "/music")
            .unwrap();
        assert!(!music.rw && music.keep);

        let vaultwarden =
            Russelfile::load(&root.join("examples/vaultwarden/Russelfile.toml")).unwrap();
        assert_eq!(
            vaultwarden
                .service
                .env
                .get("DATA_FOLDER")
                .map(String::as_str),
            Some("/data")
        );
        assert_eq!(vaultwarden.volumes.len(), 1);
        assert_eq!(vaultwarden.volumes[0].guest, "/data");
        assert!(vaultwarden.volumes[0].rw && vaultwarden.volumes[0].keep);
    }

    #[test]
    fn volumes_and_ports_load_on_microvm() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
type = "microvm"

[[volumes]]
name = "data"
guest = "/data"

[[ports]]
host = 50300
guest = 50300
"#;
        let config = Russelfile::load_from_str(toml).unwrap();
        assert_eq!(config.volumes.len(), 1);
        assert_eq!(config.ports.len(), 1);
    }

    fn service_with(fields: &str) -> String {
        format!("[service]\nsource = \".\"\nport = 3000\nmemory = \"256mb\"\n{fields}\n",)
    }

    #[test]
    fn service_name_follows_the_service_id_rule_at_load() {
        for name in ["app", "my-app", "svc_01", &"a".repeat(128)] {
            Russelfile::load_from_str(&service_with(&format!("name = {name:?}"))).unwrap();
        }
        for name in [
            "",
            "my app",
            "my.app",
            "a/b",
            "g++",
            "сервис",
            "café",
            "secrets",
            "_pool",
            &"a".repeat(129),
        ] {
            let err = Russelfile::load_from_str(&service_with(&format!("name = {name:?}")))
                .unwrap_err()
                .to_string();
            assert!(
                err.starts_with("service.name"),
                "expected {name:?} to be rejected as service.name: {err}"
            );
        }
    }

    #[test]
    fn service_bin_keeps_the_wider_rule_at_load() {
        for bin in ["my.app", "g++", "gcc-12.2", "a.out", &"b".repeat(256)] {
            let toml = service_with(&format!("name = \"app\"\nbin = {bin:?}"));
            let config = Russelfile::load_from_str(&toml).unwrap();
            assert_eq!(config.service.bin_name(), bin);
        }
        for bin in ["", ".", "..", "a b", "a/b", "$(x)", &"b".repeat(257)] {
            let toml = service_with(&format!("name = \"app\"\nbin = {bin:?}"));
            let err = Russelfile::load_from_str(&toml).unwrap_err().to_string();
            assert!(
                err.starts_with("service.bin"),
                "expected bin {bin:?} to be rejected: {err}"
            );
        }
    }

    #[test]
    fn service_env_is_validated_at_load() {
        let ok = service_with(
            "name = \"app\"\n[service.env]\nLOG_LEVEL = \"info\"\nTOKEN = \"secret://TOKEN\"",
        );
        assert_eq!(Russelfile::load_from_str(&ok).unwrap().service.env.len(), 2);

        for (entry, expected) in [
            ("PORT = \"1\"", "reserved"),
            ("LD_AUDIT = \"x\"", "reserved"),
            ("\"1FOO\" = \"x\"", "must start with a letter"),
            ("\"MY-VAR\" = \"x\"", "invalid characters"),
            ("FOO = \"a\\nb\"", "newline"),
        ] {
            let toml = service_with(&format!("name = \"app\"\n[service.env]\n{entry}"));
            let err = Russelfile::load_from_str(&toml).unwrap_err().to_string();
            assert!(
                err.starts_with("[service.env]") && err.contains(expected),
                "expected {entry:?} to fail with {expected:?}: {err}"
            );
        }

        let long = service_with(&format!(
            "name = \"app\"\n[service.env]\nFOO = \"{}\"",
            "a".repeat(4097)
        ));
        let err = Russelfile::load_from_str(&long).unwrap_err().to_string();
        assert!(err.contains("too long"), "{err}");

        let many: String = (0..65).map(|i| format!("K{i} = \"v\"\n")).collect();
        let toml = service_with(&format!("name = \"app\"\n[service.env]\n{many}"));
        let err = Russelfile::load_from_str(&toml).unwrap_err().to_string();
        assert!(err.contains("too many env keys"), "{err}");
    }

    /// Every Russelfile shipped under `examples/` must load. CI runs this,
    /// so an example that drifts from the schema fails the build.
    #[test]
    fn every_shipped_russelfile_loads() {
        fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path.file_name().and_then(|n| n.to_str()) == Some("Russelfile.toml") {
                    out.push(path);
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = Vec::new();
        collect(&root.join("examples"), &mut files);
        assert!(
            files.len() >= 10,
            "expected the shipped examples, found {files:?}"
        );
        for file in files {
            if let Err(e) = Russelfile::load(&file) {
                panic!("{} does not load: {e:#}", file.display());
            }
        }
    }

    #[test]
    fn service_args_load_on_both_runtimes() {
        let container =
            service_with("name = \"redis\"\ntype = \"container\"\nargs = [\"--dir\", \"/data\"]");
        let config = Russelfile::load_from_str(&container).unwrap();
        assert_eq!(config.service.args, vec!["--dir", "/data"]);

        // Omitted type is container, so args load.
        let omitted = service_with("name = \"app\"\nargs = [\"--flag\"]");
        Russelfile::load_from_str(&omitted).unwrap();

        let toml = service_with("name = \"app\"\ntype = \"microvm\"\nargs = [\"--flag\"]");
        let config = Russelfile::load_from_str(&toml).unwrap();
        assert_eq!(config.service.args, vec!["--flag"]);
        // Shape limits still apply on a microVM.
        let newline = service_with("name = \"app\"\ntype = \"microvm\"\nargs = [\"a\\nb\"]");
        assert!(Russelfile::load_from_str(&newline).is_err());
        let empty = service_with("name = \"app\"\ntype = \"microvm\"\nargs = []");
        Russelfile::load_from_str(&empty).unwrap();
    }

    #[test]
    fn bin_name_ignores_package_unless_build_uses_it() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
type = "container"
package = "nodePackages.foo-bar"
"#;
        let config = Russelfile::load_from_str(toml).unwrap();
        // Committed flake / default path: package must not override name.
        assert_eq!(config.service.bin_name(), "app");
        assert_eq!(config.service.bin_name_for_build(false), "app");
        assert_eq!(config.service.bin_name_for_build(true), "foo-bar");
    }
}
