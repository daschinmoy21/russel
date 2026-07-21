use std::sync::Arc;

/// Backend address for a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    pub host: String, // typically "127.0.0.1"
    pub port: u16,
}

impl Backend {
    pub fn localhost(port: u16) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }
}

/// A host-based routing rule (e.g. `api.russel.local`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRule {
    /// Full hostname e.g. "api.russel.local"
    pub host: String,
}

/// Pluggable ingress / reverse-proxy control plane.
///
/// Deploy, stop, destroy talk only to this trait — never Traefik types.
/// `TraefikFileIngress` is the default implementation. Future proxies
/// (Caddy, Envoy, NGINX) implement the same trait.
#[async_trait::async_trait]
pub trait Ingress: Send + Sync {
    /// Advertise a service backend with host rules.
    ///
    /// If `host_rules` is empty the implementation may derive a default
    /// host from `service_id` and the configured domain.
    async fn register(
        &self,
        service_id: &str,
        backend: &Backend,
        host_rules: &[HostRule],
    ) -> anyhow::Result<()>;

    /// Remove all routes for the service.
    async fn deregister(&self, service_id: &str) -> anyhow::Result<()>;

    /// Point an existing service at a new backend without dropping the route
    /// (zero-downtime generation swap). v1 may re-write the same file.
    async fn swap(
        &self,
        service_id: &str,
        new_backend: &Backend,
    ) -> anyhow::Result<()>;

    /// Primary public host for CLI display (first rule or derived default).
    fn primary_host(&self, service_id: &str) -> Option<String>;
}

/// Build the default ingress from env. Today: Traefik file provider.
pub fn default_ingress() -> Arc<dyn Ingress> {
    Arc::new(crate::traefik::TraefikFileIngress::from_env())
}
