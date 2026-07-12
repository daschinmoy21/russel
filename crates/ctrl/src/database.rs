use russel_core::config::DatabaseConfig;

/// Database provisioner — ensures database containers (Postgres/Redis) are running.
///
/// ponytail: no-op placeholder, database support planned but not yet implemented.
/// Add Docker-based database provisioning when the MVP requires persistent storage.

#[derive(Debug, Default)]
pub struct DatabaseProvisioner;

impl DatabaseProvisioner {
    /// Ensure the configured databases are available.
    /// Currently a no-op; returns Ok without provisioning anything.
    pub async fn ensure(&self, _config: &DatabaseConfig) -> anyhow::Result<()> {
        tracing::info!("database provisioning not yet implemented — skipping");
        Ok(())
    }
}
