use std::collections::HashMap;
use std::{fmt, fs, path::Path, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Russelfile {
    pub service: ServiceConfig,
    /// ponytail: database provisioning is not yet implemented.
    /// When enabled = true, `load` returns an error telling the user
    /// databases are not supported yet, rather than silently ignoring
    /// their config.
    #[serde(default)]
    pub database: Option<DatabaseConfig>,
}

impl Russelfile {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = fs::read_to_string(path)?;
        Self::load_from_str(&contents)
    }

    /// Parse from a string — shared by `load` and tests.
    pub fn load_from_str(contents: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(contents)?;
        // Reject database config at parse time — it's not implemented yet.
        if let Some(ref db) = config.database
            && (db.postgres.as_ref().is_some_and(|p| p.enabled)
                || db.redis.as_ref().is_some_and(|r| r.enabled))
        {
            anyhow::bail!(
                "database provisioning is not yet supported (remove [database] from Russelfile)"
            );
        }
        Ok(config)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Microvm,
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

/// Resolve effective runtime: Russelfile `service.type` is source of truth.
/// CLI `--runtime` must match when provided; otherwise the file value is used.
pub fn resolve_runtime(file: RuntimeKind, cli: Option<RuntimeKind>) -> anyhow::Result<RuntimeKind> {
    match cli {
        None => Ok(file),
        Some(cli_kind) if cli_kind == file => Ok(file),
        Some(cli_kind) => anyhow::bail!(
            "CLI --runtime {cli_kind} does not match Russelfile service.type ({file})"
        ),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub source: String,
    pub port: u16,
    pub memory: Memory,
    /// Runtime kind (`microvm` or `container`). TOML field is `type`.
    #[serde(default, rename = "type")]
    pub runtime: RuntimeKind,
    /// Name of the binary produced by the build.
    /// Defaults to `name` if not specified.
    pub bin: Option<String>,
    /// User-defined environment variables injected at deploy time.
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
}

impl ServiceConfig {
    pub fn bin_name(&self) -> &str {
        self.bin.as_deref().unwrap_or(&self.name)
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
const RESERVED_ENV_KEYS: &[&str] = &["PORT", "VM_IP", "HOST_IP", "APP"];

/// Validate a full env map: keys, reserved keys, value lengths, key count.
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

/// Merge env maps: base first, then overlay (overlay wins on key conflict).
pub fn merge_env_maps(
    base: &std::collections::HashMap<String, String>,
    overlay: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let mut merged = base.clone();
    for (k, v) in overlay {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    pub postgres: Option<PostgresConfig>,
    pub redis: Option<RedisConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PostgresConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedisConfig {
    pub enabled: bool,
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
        let mebibytes = normalized
            .strip_suffix("mb")
            .or_else(|| normalized.strip_suffix("mib"))
            .ok_or_else(|| serde::de::Error::custom("memory must end in mb or mib"))?
            .parse()
            .map_err(|_| serde::de::Error::custom("memory must be a number followed by mb"))?;

        Ok(Self::Mebibytes(mebibytes))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

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
    fn parse_database_config_disabled_ok() {
        let toml = r#"
[service]
name = "db-app"
source = "."
port = 5432
memory = "512mb"

[database.postgres]
enabled = false
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        let db = config.database.unwrap();
        assert!(!db.postgres.unwrap().enabled);
        assert!(db.redis.is_none());
    }

    #[test]
    fn parse_database_enabled_rejected() {
        let toml = r#"
[service]
name = "db-app"
source = "."
port = 5432
memory = "512mb"

[database.postgres]
enabled = true
"#;
        let err = Russelfile::load_from_str(toml).unwrap_err();
        assert!(err.to_string().contains("not yet supported"));
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
    fn runtime_defaults_to_microvm_when_omitted() {
        let toml = r#"
[service]
name = "app"
source = "."
port = 3000
memory = "256mb"
"#;
        let config: Russelfile = toml::from_str(toml).unwrap();
        assert_eq!(config.service.runtime, RuntimeKind::Microvm);
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
    fn resolve_runtime_match_ok() {
        let resolved =
            resolve_runtime(RuntimeKind::Container, Some(RuntimeKind::Container)).unwrap();
        assert_eq!(resolved, RuntimeKind::Container);
    }

    #[test]
    fn resolve_runtime_mismatch_errors() {
        let err = resolve_runtime(RuntimeKind::Microvm, Some(RuntimeKind::Container)).unwrap_err();
        assert!(err.to_string().contains("does not match"));
        assert!(err.to_string().contains("container"));
        assert!(err.to_string().contains("microvm"));
    }

    #[test]
    fn resolve_runtime_cli_omitted_uses_file() {
        let resolved = resolve_runtime(RuntimeKind::Container, None).unwrap();
        assert_eq!(resolved, RuntimeKind::Container);
    }

    // ── env var tests ──────────────────────────────────────────────────

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
        for key in &["PORT", "VM_IP", "HOST_IP", "APP"] {
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
    fn validate_env_map_rejects_long_value() {
        let mut map = HashMap::new();
        map.insert("FOO".to_string(), "a".repeat(4097));
        let err = validate_env_map(&map).unwrap_err();
        assert!(err.to_string().contains("too long"));
    }

    #[test]
    fn merge_env_maps_overlay_wins() {
        let mut base = HashMap::new();
        base.insert("A".to_string(), "base".to_string());
        base.insert("B".to_string(), "base_b".to_string());
        let mut overlay = HashMap::new();
        overlay.insert("A".to_string(), "overlay".to_string());
        overlay.insert("C".to_string(), "overlay_c".to_string());
        let merged = merge_env_maps(&base, &overlay);
        assert_eq!(merged.get("A"), Some(&"overlay".to_string()));
        assert_eq!(merged.get("B"), Some(&"base_b".to_string()));
        assert_eq!(merged.get("C"), Some(&"overlay_c".to_string()));
    }
}
