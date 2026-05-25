#[derive(Debug, Default)]
pub struct TraefikClient;

impl TraefikClient {
    pub async fn register(&self, _service_id: &str, _backend_port: u16) -> anyhow::Result<()> {
        tracing::warn!("Traefik reverse-proxy registration is not yet implemented");
        Ok(())
    }
}
