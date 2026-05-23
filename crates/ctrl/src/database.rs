use russel_core::config::DatabaseConfig;

#[derive(Debug, Default)]
pub struct DatabaseProvisioner;

impl DatabaseProvisioner {
    #[allow(dead_code)]
    pub async fn ensure(&self, _config: &DatabaseConfig) -> anyhow::Result<()> {
        Ok(())
    }
}
