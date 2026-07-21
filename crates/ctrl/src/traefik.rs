use std::path::PathBuf;

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
#[derive(Debug, Clone)]
pub struct TraefikClient {
    dynamic_dir: PathBuf,
    domain: String,
}

impl Default for TraefikClient {
    fn default() -> Self {
        Self::from_env()
    }
}

impl TraefikClient {
    /// Build a client from environment variables.
    pub fn from_env() -> Self {
        let dynamic_dir = std::env::var("RUSSEL_TRAEFIK_DYNAMIC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/var/lib/russel/traefik/dynamic"));
        let domain = std::env::var("RUSSEL_TRAEFIK_DOMAIN")
            .unwrap_or_else(|_| "russel.local".to_string());
        Self {
            dynamic_dir,
            domain,
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

    /// Write a Traefik dynamic config file for `service_id` → `host_port`.
    ///
    /// Creates the dynamic directory if it does not exist.
    /// On IO error the caller should fail the deploy.
    pub async fn register(&self, service_id: &str, host_port: u16) -> anyhow::Result<()> {
        let host = self.public_host(service_id);
        let router_name = router_name(service_id);
        let service_name = svc_name(service_id);

        tokio::fs::create_dir_all(&self.dynamic_dir).await.map_err(|e| {
            anyhow::anyhow!(
                "failed to create Traefik dynamic dir {}: {e}",
                self.dynamic_dir.display()
            )
        })?;

        let config = serde_json::json!({
            "http": {
                "routers": {
                    &router_name: {
                        "rule": format!("Host(`{}`)", host),
                        "entryPoints": ["web"],
                        "service": &service_name,
                    }
                },
                "services": {
                    &service_name: {
                        "loadBalancer": {
                            "servers": [
                                {"url": format!("http://127.0.0.1:{}", host_port)}
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
            backend = %format!("127.0.0.1:{host_port}"),
            "Traefik dynamic config written"
        );

        Ok(())
    }

    /// Remove the dynamic config file for `service_id`.
    ///
    /// Missing files are not an error (already removed or never registered).
    pub async fn unregister(&self, service_id: &str) -> anyhow::Result<()> {
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
}

/// Traefik router name for a service: `russel-{service_id}`.
fn router_name(service_id: &str) -> String {
    format!("russel-{service_id}")
}

/// Traefik service name for a service: `russel-{service_id}`.
fn svc_name(service_id: &str) -> String {
    format!("russel-{service_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_service_naming() {
        assert_eq!(router_name("api"), "russel-api");
        assert_eq!(svc_name("api"), "russel-api");
        assert_eq!(router_name("my-service"), "russel-my-service");
    }

    #[test]
    fn public_host_default_domain() {
        let client = TraefikClient {
            dynamic_dir: PathBuf::from("/tmp/traefik"),
            domain: "russel.local".to_string(),
        };
        assert_eq!(client.public_host("api"), "api.russel.local");
        assert_eq!(client.public_host("demo"), "demo.russel.local");
    }

    #[test]
    fn public_host_custom_domain() {
        let client = TraefikClient {
            dynamic_dir: PathBuf::from("/tmp/traefik"),
            domain: "example.com".to_string(),
        };
        assert_eq!(client.public_host("api"), "api.example.com");
    }

    #[test]
    fn enabled_is_always_true() {
        let client = TraefikClient::default();
        assert!(client.enabled());
    }

    #[tokio::test]
    async fn register_writes_json_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let client = TraefikClient {
            dynamic_dir: dir.clone(),
            domain: "russel.local".to_string(),
        };

        client.register("api", 3100).await.unwrap();

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
    async fn unregister_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let client = TraefikClient {
            dynamic_dir: dir.clone(),
            domain: "russel.local".to_string(),
        };

        // Register then unregister
        client.register("api", 3100).await.unwrap();
        assert!(dir.join("api.json").exists());

        client.unregister("api").await.unwrap();
        assert!(!dir.join("api.json").exists());
    }

    #[tokio::test]
    async fn unregister_ignores_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let client = TraefikClient {
            dynamic_dir: tmp.path().join("dynamic"),
            domain: "russel.local".to_string(),
        };

        // Should not error on missing file
        client.unregister("nonexistent").await.unwrap();
    }

    #[test]
    fn host_rule_string_correct() {
        let client = TraefikClient {
            dynamic_dir: PathBuf::from("/tmp"),
            domain: "russel.local".to_string(),
        };
        // Service id validated as alphanumeric earlier in pipeline
        assert_eq!(client.public_host("my-app"), "my-app.russel.local");
    }

    #[test]
    fn from_env_defaults_when_unset() {
        // Test constructor directly (env tests are fragile in parallel).
        let client = TraefikClient::default();
        // Default::default() delegates to from_env(); when vars are unset
        // the dynamic_dir is the default path.
        assert!(client
            .dynamic_dir
            .to_string_lossy()
            .contains("traefik/dynamic"));
        assert!(!client.domain.is_empty());
    }
}
