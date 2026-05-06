use std::{fs, path::Path};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Russelfile {
    pub service: ServiceConfig,
    pub database: Option<DatabaseConfig>,
}

impl Russelfile {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = fs::read_to_string(path)?;
        Ok(toml::from_str(&contents)?)
    }
}

#[derive(Debug, Clone, Deserialize)]
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
