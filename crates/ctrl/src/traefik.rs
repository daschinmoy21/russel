use std::path::PathBuf;

use crate::ingress::{Backend, HostRule, Ingress};

/// Writes Traefik dynamic configuration files (file provider, JSON format).
///
/// One file per service: `{dynamic_dir}/{service_id}.json`.
/// Traefik watches the directory; no reload signal needed.
///
/// ## Environment
/// - `RUSSEL_TRAEFIK_DYNAMIC_DIR` — directory for dynamic config files
///   (default `/var/lib/russel/traefik/dynamic`)
/// - `RUSSEL_TRAEFIK_DOMAIN` — domain suffix for Host rules
///   (default `russel.local`)
/// - `RUSSEL_TRAEFIK_TLS` — when `1`/`true`, attach TLS to routers (websecure)
/// - `RUSSEL_TRAEFIK_CERT_RESOLVER` — ACME cert resolver name (default `letsencrypt`)
#[derive(Debug, Clone)]
pub struct TraefikFileIngress {
    dynamic_dir: PathBuf,
    domain: String,
    tls_enabled: bool,
    cert_resolver: String,
}

impl Default for TraefikFileIngress {
    fn default() -> Self {
        Self::from_env()
    }
}

impl TraefikFileIngress {
    /// Build a client from environment variables.
    pub fn from_env() -> Self {
        let dynamic_dir = std::env::var("RUSSEL_TRAEFIK_DYNAMIC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/var/lib/russel/traefik/dynamic"));
        let domain =
            std::env::var("RUSSEL_TRAEFIK_DOMAIN").unwrap_or_else(|_| "russel.local".to_string());
        let tls_enabled = matches!(
            std::env::var("RUSSEL_TRAEFIK_TLS")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes" | "on"
        );
        let cert_resolver = std::env::var("RUSSEL_TRAEFIK_CERT_RESOLVER")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "letsencrypt".to_string());
        Self {
            dynamic_dir,
            domain,
            tls_enabled,
            cert_resolver,
        }
    }

    /// Always true — writing config files is harmless even without Traefik.
    #[allow(dead_code)]
    pub fn enabled(&self) -> bool {
        true
    }

    /// Build the public hostname for a service: `<service_id>.<domain>`.
    pub fn public_host(&self, service_id: &str) -> String {
        format!("{}.{}", service_id, self.domain)
    }
}

#[async_trait::async_trait]
impl Ingress for TraefikFileIngress {
    async fn register(
        &self,
        service_id: &str,
        backend: &Backend,
        host_rules: &[HostRule],
    ) -> anyhow::Result<()> {
        let host = if let Some(rule) = host_rules.first() {
            rule.host.clone()
        } else {
            self.public_host(service_id)
        };

        let router_name = router_name(service_id);
        let service_name = svc_name(service_id);

        tokio::fs::create_dir_all(&self.dynamic_dir)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to create Traefik dynamic dir {}: {e}",
                    self.dynamic_dir.display()
                )
            })?;

        let mut router = serde_json::json!({
            "rule": format!("Host(`{}`)", host),
            "entryPoints": if self.tls_enabled {
                serde_json::json!(["web", "websecure"])
            } else {
                serde_json::json!(["web"])
            },
            "service": &service_name,
        });
        if self.tls_enabled {
            router["tls"] = serde_json::json!({
                "certResolver": self.cert_resolver,
            });
        }

        let config = serde_json::json!({
            "http": {
                "routers": {
                    &router_name: router
                },
                "services": {
                    &service_name: {
                        "loadBalancer": {
                            "servers": [
                                {"url": backend.url()}
                            ]
                        }
                    }
                }
            }
        });

        let file_path = self.dynamic_dir.join(format!("{service_id}.json"));
        let content = serde_json::to_string_pretty(&config).map_err(|e| {
            anyhow::anyhow!("failed to serialize Traefik config for {service_id}: {e}")
        })?;
        tokio::fs::write(&file_path, &content).await.map_err(|e| {
            anyhow::anyhow!(
                "failed to write Traefik config {}: {e}",
                file_path.display()
            )
        })?;

        tracing::info!(
            service_id = %service_id,
            path = %file_path.display(),
            host = %host,
            backend = %backend.url(),
            "Traefik dynamic config written"
        );

        Ok(())
    }

    async fn deregister(&self, service_id: &str) -> anyhow::Result<()> {
        let file_path = self.dynamic_dir.join(format!("{service_id}.json"));
        match tokio::fs::remove_file(&file_path).await {
            Ok(()) => {
                tracing::info!(
                    service_id = %service_id,
                    path = %file_path.display(),
                    "Traefik dynamic config removed"
                );
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    service_id = %service_id,
                    "Traefik dynamic config not found (already removed)"
                );
                Ok(())
            }
            Err(e) => Err(anyhow::anyhow!(
                "failed to remove Traefik config {}: {e}",
                file_path.display()
            )),
        }
    }

    async fn swap(&self, service_id: &str, new_backend: &Backend) -> anyhow::Result<()> {
        // v1: re-write the file with the default host rule and new backend.
        // A future version may read existing rules from the file and preserve
        // custom Host/Path rules while only swapping the backend URL.
        let default_rule = HostRule {
            host: self.public_host(service_id),
        };
        self.register(service_id, new_backend, &[default_rule])
            .await
    }

    fn primary_host(&self, service_id: &str) -> Option<String> {
        Some(self.public_host(service_id))
    }
}

/// Traefik router name for a service: `russel-{service_id}`.
fn router_name(service_id: &str) -> String {
    format!("russel-{service_id}")
}

/// Traefik service name for a service: `russel-{service_id}`.
fn svc_name(service_id: &str) -> String {
    router_name(service_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn test_ingress(dir: impl Into<PathBuf>, domain: &str) -> TraefikFileIngress {
        TraefikFileIngress {
            dynamic_dir: dir.into(),
            domain: domain.to_string(),
            tls_enabled: false,
            cert_resolver: "letsencrypt".to_string(),
        }
    }

    #[test]
    fn router_service_naming() {
        assert_eq!(router_name("api"), "russel-api");
        assert_eq!(svc_name("api"), "russel-api");
        assert_eq!(router_name("my-service"), "russel-my-service");
    }

    #[test]
    fn public_host_default_domain() {
        let ing = test_ingress("/tmp/traefik", "russel.local");
        assert_eq!(ing.public_host("api"), "api.russel.local");
        assert_eq!(ing.public_host("demo"), "demo.russel.local");
    }

    #[test]
    fn public_host_custom_domain() {
        let ing = test_ingress("/tmp/traefik", "example.com");
        assert_eq!(ing.public_host("api"), "api.example.com");
    }

    #[test]
    fn enabled_is_always_true() {
        let ing = TraefikFileIngress::default();
        assert!(ing.enabled());
    }

    #[test]
    fn primary_host_returns_public_host() {
        let ing = test_ingress("/tmp/traefik", "russel.local");
        assert_eq!(
            Ingress::primary_host(&ing, "api"),
            Some("api.russel.local".to_string())
        );
    }

    #[tokio::test]
    async fn register_writes_json_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir.clone(), "russel.local");

        let backend = Backend::localhost(3100);
        Ingress::register(&ing, "api", &backend, &[]).await.unwrap();

        let file_path = dir.join("api.json");
        assert!(file_path.exists());

        let raw = std::fs::read_to_string(&file_path).unwrap();
        let config: serde_json::Value = serde_json::from_str(&raw).unwrap();

        // Router
        let router = &config["http"]["routers"]["russel-api"];
        assert_eq!(router["rule"], "Host(`api.russel.local`)");
        assert_eq!(router["entryPoints"][0], "web");
        assert_eq!(router["service"], "russel-api");

        // Service backend
        let svc = &config["http"]["services"]["russel-api"];
        assert_eq!(
            svc["loadBalancer"]["servers"][0]["url"],
            "http://127.0.0.1:3100"
        );
    }

    #[tokio::test]
    async fn register_with_custom_host_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir.clone(), "russel.local");

        let backend = Backend {
            host: "10.0.0.1".to_string(),
            port: 8080,
        };
        let rules = vec![HostRule {
            host: "custom.example.com".to_string(),
        }];
        Ingress::register(&ing, "custom-svc", &backend, &rules)
            .await
            .unwrap();

        let file_path = dir.join("custom-svc.json");
        assert!(file_path.exists());

        let raw = std::fs::read_to_string(&file_path).unwrap();
        let config: serde_json::Value = serde_json::from_str(&raw).unwrap();

        let router = &config["http"]["routers"]["russel-custom-svc"];
        assert_eq!(router["rule"], "Host(`custom.example.com`)");

        let svc = &config["http"]["services"]["russel-custom-svc"];
        assert_eq!(
            svc["loadBalancer"]["servers"][0]["url"],
            "http://10.0.0.1:8080"
        );
    }

    #[tokio::test]
    async fn deregister_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir.clone(), "russel.local");

        // Register then deregister
        let backend = Backend::localhost(3100);
        Ingress::register(&ing, "api", &backend, &[]).await.unwrap();
        assert!(dir.join("api.json").exists());

        Ingress::deregister(&ing, "api").await.unwrap();
        assert!(!dir.join("api.json").exists());
    }

    #[tokio::test]
    async fn deregister_ignores_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let ing = test_ingress(tmp.path().join("dynamic"), "russel.local");

        // Should not error on missing file
        Ingress::deregister(&ing, "nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn swap_rewrites_file_with_new_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir.clone(), "russel.local");

        // First register with old backend
        let old = Backend::localhost(3100);
        Ingress::register(&ing, "api", &old, &[]).await.unwrap();

        // Swap to new backend
        let new = Backend::localhost(3200);
        Ingress::swap(&ing, "api", &new).await.unwrap();

        let raw = std::fs::read_to_string(dir.join("api.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            config["http"]["services"]["russel-api"]["loadBalancer"]["servers"][0]["url"],
            "http://127.0.0.1:3200"
        );
    }

    #[test]
    fn host_rule_string_correct() {
        let ing = test_ingress("/tmp", "russel.local");
        // Service id validated as alphanumeric earlier in pipeline
        assert_eq!(ing.public_host("my-app"), "my-app.russel.local");
    }

    #[tokio::test]
    async fn register_with_tls_adds_cert_resolver() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let mut ing = test_ingress(dir.clone(), "example.com");
        ing.tls_enabled = true;
        ing.cert_resolver = "myresolver".to_string();

        Ingress::register(&ing, "api", &Backend::localhost(3100), &[])
            .await
            .unwrap();
        let raw = std::fs::read_to_string(dir.join("api.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let router = &config["http"]["routers"]["russel-api"];
        assert_eq!(router["entryPoints"][0], "web");
        assert_eq!(router["entryPoints"][1], "websecure");
        assert_eq!(router["tls"]["certResolver"], "myresolver");
    }

    #[test]
    fn from_env_defaults_when_unset() {
        let _lock = env_lock();
        let previous = std::env::var_os("RUSSEL_TRAEFIK_CERT_RESOLVER");
        unsafe { std::env::remove_var("RUSSEL_TRAEFIK_CERT_RESOLVER") };
        let ing = TraefikFileIngress::default();
        assert!(
            ing.dynamic_dir
                .to_string_lossy()
                .contains("traefik/dynamic")
        );
        assert!(!ing.domain.is_empty());
        unsafe {
            match previous {
                Some(value) => std::env::set_var("RUSSEL_TRAEFIK_CERT_RESOLVER", value),
                None => std::env::remove_var("RUSSEL_TRAEFIK_CERT_RESOLVER"),
            }
        }
    }

    #[test]
    fn from_env_blank_resolver_defaults() {
        let _lock = env_lock();
        let previous = std::env::var_os("RUSSEL_TRAEFIK_CERT_RESOLVER");
        unsafe { std::env::set_var("RUSSEL_TRAEFIK_CERT_RESOLVER", "  ") };
        let ing = TraefikFileIngress::from_env();
        assert_eq!(ing.cert_resolver, "letsencrypt");
        unsafe {
            match previous {
                Some(value) => std::env::set_var("RUSSEL_TRAEFIK_CERT_RESOLVER", value),
                None => std::env::remove_var("RUSSEL_TRAEFIK_CERT_RESOLVER"),
            }
        }
    }

    #[test]
    fn backend_localhost_constructor() {
        let b = Backend::localhost(3000);
        assert_eq!(b.host, "127.0.0.1");
        assert_eq!(b.port, 3000);
        assert_eq!(b.url(), "http://127.0.0.1:3000");
    }

    #[test]
    fn backend_custom_host_url() {
        let b = Backend {
            host: "10.0.0.5".to_string(),
            port: 9000,
        };
        assert_eq!(b.url(), "http://10.0.0.5:9000");
    }
}
