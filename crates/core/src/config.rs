use std::{fs, path::Path};

use serde::Deserialize;

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
    fn load_from_str(contents: &str) -> anyhow::Result<Self> {
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub source: String,
    pub port: u16,
    pub memory: Memory,
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
}
