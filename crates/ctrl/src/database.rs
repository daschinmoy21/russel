#[allow(dead_code)]
use russel_core::config::DatabaseConfig;

#[derive(Debug, Default)]
pub struct DatabaseProvisioner;

impl DatabaseProvisioner {
    pub async fn ensure(&self, _config: &DatabaseConfig) -> anyhow::Result<()> {
        todo!("Impl database support!");
    }
}
