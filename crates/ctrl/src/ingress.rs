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

    /// Traefik backend for a published host port.
    ///
    /// Host is `RUSSEL_TRAEFIK_BACKEND` when set, else the publish bind
    /// (`RUSSEL_PUBLISH_BIND`). Wildcard binds map to loopback so a Traefik
    /// in the same netns can reach the port. Rootless Traefik in another
    /// netns needs `RUSSEL_TRAEFIK_BACKEND` (e.g. `10.89.0.1`).
    pub fn from_publish(port: u16) -> Self {
        Self {
            host: ingress_backend_host(),
            port,
        }
    }

    pub fn url(&self) -> String {
        // IPv6 literals need brackets in URL authorities (`http://[::1]:80`).
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("http://[{}]:{}", self.host, self.port)
        } else {
            format!("http://{}:{}", self.host, self.port)
        }
    }
}

/// Map a publish bind to the host Traefik should dial.
///
/// Wildcard binds are not reachable as a destination; same-netns Traefik
/// dials loopback instead.
fn backend_host_for_bind(bind: &str) -> String {
    if bind == "0.0.0.0" || bind == "::" {
        "127.0.0.1".into()
    } else {
        bind.to_string()
    }
}

/// Host Traefik should dial for a published backend port.
pub fn ingress_backend_host() -> String {
    if let Ok(v) = std::env::var("RUSSEL_TRAEFIK_BACKEND") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    backend_host_for_bind(&crate::network::publish_bind_addr())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn with_traefik_backend_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _lock = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        struct EnvRestore(Option<std::ffi::OsString>);
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                // SAFETY: exclusive LOCK held; only test code mutates this var.
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("RUSSEL_TRAEFIK_BACKEND", v),
                        None => std::env::remove_var("RUSSEL_TRAEFIK_BACKEND"),
                    }
                }
            }
        }

        let prev = std::env::var_os("RUSSEL_TRAEFIK_BACKEND");
        let _restore = EnvRestore(prev);
        // SAFETY: exclusive lock held for the duration of the mutation + body.
        unsafe {
            match value {
                Some(v) => std::env::set_var("RUSSEL_TRAEFIK_BACKEND", v),
                None => std::env::remove_var("RUSSEL_TRAEFIK_BACKEND"),
            }
        }
        f()
    }

    #[test]
    fn url_brackets_ipv6_hosts() {
        let bare = Backend {
            host: "::1".into(),
            port: 8080,
        };
        assert_eq!(bare.url(), "http://[::1]:8080");

        let full = Backend {
            host: "2001:db8::1".into(),
            port: 3000,
        };
        assert_eq!(full.url(), "http://[2001:db8::1]:3000");

        let already = Backend {
            host: "[::1]".into(),
            port: 80,
        };
        assert_eq!(already.url(), "http://[::1]:80");
    }

    #[test]
    fn url_leaves_ipv4_and_hostnames_unchanged() {
        assert_eq!(Backend::localhost(3000).url(), "http://127.0.0.1:3000");
        let named = Backend {
            host: "backend.local".into(),
            port: 9000,
        };
        assert_eq!(named.url(), "http://backend.local:9000");
    }

    #[test]
    fn backend_host_for_bind_maps_wildcards_to_loopback() {
        assert_eq!(backend_host_for_bind("0.0.0.0"), "127.0.0.1");
        assert_eq!(backend_host_for_bind("::"), "127.0.0.1");
        assert_eq!(backend_host_for_bind("10.89.0.1"), "10.89.0.1");
        assert_eq!(backend_host_for_bind("::1"), "::1");
    }

    #[test]
    fn ingress_backend_host_prefers_env_override() {
        with_traefik_backend_env(Some("10.89.0.1"), || {
            assert_eq!(ingress_backend_host(), "10.89.0.1");
            let b = Backend::from_publish(8080);
            assert_eq!(b.host, "10.89.0.1");
            assert_eq!(b.port, 8080);
        });
        with_traefik_backend_env(Some("  ::1  "), || {
            assert_eq!(ingress_backend_host(), "::1");
            assert_eq!(Backend::from_publish(80).url(), "http://[::1]:80");
        });
    }

    #[test]
    fn ingress_backend_host_empty_or_whitespace_falls_back_to_publish_bind() {
        let expected = backend_host_for_bind(&crate::network::publish_bind_addr());
        with_traefik_backend_env(None, || {
            assert_eq!(ingress_backend_host(), expected);
        });
        with_traefik_backend_env(Some(""), || {
            assert_eq!(ingress_backend_host(), expected);
        });
        with_traefik_backend_env(Some("   "), || {
            assert_eq!(ingress_backend_host(), expected);
        });
    }
}

/// A host-based routing rule (e.g. `api.russel.local`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRule {
    /// Full hostname e.g. "api.russel.local"
    pub host: String,
}

/// What [`Ingress::wait_served`] found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Served {
    /// The proxy answered a request for the route from the new backend.
    Confirmed,
    /// Nothing could be asked; the reason says why. Deploy goes on and says
    /// so in its progress output.
    Unchecked(String),
}

/// Pluggable ingress / reverse-proxy control plane.
///
/// Deploy, stop, destroy talk only to this trait — never Traefik types.
/// `TraefikFileIngress` is the default implementation. Future proxies
/// (Caddy, Envoy, NGINX) implement the same trait.
// Clippy 1.99 reports double_must_use inside the async_trait expansion of
// this trait (CI only; the expansion is not ours to change).
#[allow(clippy::double_must_use)]
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

    /// Whether `register` would accept these host rules, without writing
    /// anything. Deploy calls it before it stops the running generation, so a
    /// route that cannot be registered fails the deploy with nothing touched.
    /// `register` still checks for itself. The default accepts everything.
    async fn check_route(&self, _service_id: &str, _host_rules: &[HostRule]) -> anyhow::Result<()> {
        Ok(())
    }

    /// Remove all routes for the service.
    async fn deregister(&self, service_id: &str) -> anyhow::Result<()>;

    /// Point an existing service at a new backend without dropping the route
    /// (zero-downtime generation swap). v1 may re-write the same file. The
    /// proxy may still serve the old backend when this returns; see
    /// [`Ingress::wait_served`].
    async fn swap(
        &self,
        service_id: &str,
        new_backend: &Backend,
        host_rules: &[HostRule],
    ) -> anyhow::Result<()>;

    /// Wait until the proxy serves the route from `backend` (#562). Deploy
    /// calls it after `swap` and retires the previous generation only after
    /// it returns. An error means the proxy kept serving something else.
    /// The default has no way to ask and reports [`Served::Unchecked`].
    async fn wait_served(
        &self,
        _service_id: &str,
        _backend: &Backend,
        _host_rules: &[HostRule],
    ) -> anyhow::Result<Served> {
        Ok(Served::Unchecked(
            "this ingress cannot report what it serves".into(),
        ))
    }

    /// Primary public host for CLI display (first rule or derived default).
    fn primary_host(&self, service_id: &str) -> Option<String>;
}

/// Build the default ingress from env. Today: Traefik file provider.
pub fn default_ingress() -> Arc<dyn Ingress> {
    Arc::new(crate::traefik::TraefikFileIngress::from_env())
}
