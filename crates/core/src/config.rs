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

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
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
            other => anyhow::bail!(
                "unknown runtime {other:?}, expected \"microvm\" or \"container\""
            ),
        }
    }
}

/// Resolve effective runtime: Russelfile `service.type` is source of truth.
/// CLI `--runtime` must match when provided; otherwise the file value is used.
pub fn resolve_runtime(
    file: RuntimeKind,
    cli: Option<RuntimeKind>,
) -> anyhow::Result<RuntimeKind> {
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
}

impl ServiceConfig {
    pub fn bin_name(&self) -> &str {
        self.bin.as_deref().unwrap_or(&self.name)
    }
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
}
