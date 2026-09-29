use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use crate::ingress::{Backend, HostRule, Ingress};
use russel_core::config::is_valid_dns_name;

const DEFAULT_DOMAIN: &str = "russel.local";

/// One write lock per Traefik dynamic dir. `default_ingress()` builds a new
/// client per request, so the lock cannot live on the struct.
static DIR_WRITE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn dir_write_lock(dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = DIR_WRITE_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(dir.to_path_buf())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Writes Traefik dynamic configuration files (file provider, JSON format).
///
/// One file per service: `{dynamic_dir}/{service_id}.json`.
/// Traefik watches the directory; no reload signal needed.
///
/// ## Environment
/// - `RUSSEL_TRAEFIK_DYNAMIC_DIR` — directory for dynamic config files
///   (default `$RUSSEL_DATA_DIR/traefik/dynamic`, i.e. `/var/lib/russel/traefik/dynamic`)
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
            .unwrap_or_else(|_| crate::paths::data_root().join("traefik/dynamic"));
        let domain = std::env::var("RUSSEL_TRAEFIK_DOMAIN")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| DEFAULT_DOMAIN.to_string());
        let domain = if is_valid_dns_name(&domain) {
            domain
        } else {
            tracing::error!(
                domain = %domain,
                "RUSSEL_TRAEFIK_DOMAIN is invalid; falling back to {DEFAULT_DOMAIN}"
            );
            DEFAULT_DOMAIN.to_string()
        };
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

    async fn ensure_host_is_unique(&self, service_id: &str, host: &str) -> anyhow::Result<()> {
        let mut entries = match tokio::fs::read_dir(&self.dynamic_dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "failed to scan Traefik dynamic dir {}: {e}",
                    self.dynamic_dir.display()
                ));
            }
        };
        let own_file = format!("{service_id}.json");

        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            anyhow::anyhow!(
                "failed to scan Traefik dynamic dir {}: {e}",
                self.dynamic_dir.display()
            )
        })? {
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            if file_name == own_file || !file_name.ends_with(".json") || file_name.ends_with(".tmp")
            {
                continue;
            }
            let Some(peer_id) = file_name.strip_suffix(".json") else {
                continue;
            };

            let Ok(raw) = tokio::fs::read_to_string(entry.path()).await else {
                continue;
            };
            let Ok(config) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let Ok(peer_host) = host_from_config(&config, peer_id) else {
                continue;
            };
            if peer_host.eq_ignore_ascii_case(host) {
                anyhow::bail!("ingress host \"{host}\" is already routed by service \"{peer_id}\"");
            }
        }

        Ok(())
    }

    /// Read the routed Host value from this service's Traefik JSON, without
    /// deriving a default when the file is absent or malformed.
    pub fn host_from_dynamic_config(&self, service_id: &str) -> Option<String> {
        let file_path = self.dynamic_dir.join(format!("{service_id}.json"));
        std::fs::read_to_string(file_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .and_then(|config| host_from_config(&config, service_id).ok())
    }
}

/// Atomically write a Traefik dynamic config file with `mode 0644`.
///
/// Writes `{service_id}.json.<nonce>.tmp` in the same directory, fsyncs,
/// then renames it into place. On failure the temporary file is removed.
async fn atomic_write_dynamic_config(
    path: &Path,
    content: &[u8],
    service_id: &str,
) -> anyhow::Result<()> {
    let nonce = crate::metadata::write_nonce();
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("traefik config path has no parent directory"))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.json");
    let tmp = parent.join(format!("{file_name}.{nonce:x}.tmp"));

    let write_result = async {
        #[allow(unused_imports)]
        use std::os::unix::fs::OpenOptionsExt;
        use tokio::io::AsyncWriteExt;

        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            opts.mode(0o644)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        }
        let mut f = opts
            .open(&tmp)
            .await
            .map_err(|e| anyhow::anyhow!("create tmp traefik config for {service_id}: {e}"))?;
        f.write_all(content)
            .await
            .map_err(|e| anyhow::anyhow!("write tmp traefik config for {service_id}: {e}"))?;
        f.sync_all()
            .await
            .map_err(|e| anyhow::anyhow!("fsync tmp traefik config for {service_id}: {e}"))?;
        anyhow::Result::<()>::Ok(())
    }
    .await;

    if let Err(e) = write_result {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }

    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(anyhow::anyhow!(
            "rename tmp traefik config for {service_id}: {e}"
        ));
    }

    Ok(())
}

#[async_trait::async_trait]
impl Ingress for TraefikFileIngress {
    async fn register(
        &self,
        service_id: &str,
        backend: &Backend,
        host_rules: &[HostRule],
    ) -> anyhow::Result<()> {
        if host_rules.len() > 1 {
            anyhow::bail!(
                "v1 ingress supports one Host rule (got {})",
                host_rules.len()
            );
        }

        let host = if let Some(rule) = host_rules.first() {
            rule.host.clone()
        } else {
            self.public_host(service_id)
        };
        if !is_valid_dns_name(&host) {
            anyhow::bail!("ingress.host {host:?} is not a valid DNS name");
        }

        let router_key = router_name(service_id);
        let service_name = router_name(service_id);

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
                    &router_key: router
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

        let lock = dir_write_lock(&self.dynamic_dir);
        let _guard = lock.lock().await;
        tokio::fs::create_dir_all(&self.dynamic_dir)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to create Traefik dynamic dir {}: {e}",
                    self.dynamic_dir.display()
                )
            })?;
        self.ensure_host_is_unique(service_id, &host).await?;
        atomic_write_dynamic_config(&file_path, content.as_bytes(), service_id).await?;

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
        let lock = dir_write_lock(&self.dynamic_dir);
        let _guard = lock.lock().await;
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

    async fn swap(
        &self,
        service_id: &str,
        new_backend: &Backend,
        host_rules: &[HostRule],
    ) -> anyhow::Result<()> {
        self.register(service_id, new_backend, host_rules).await
    }

    fn primary_host(&self, service_id: &str) -> Option<String> {
        let host = self.host_from_dynamic_config(service_id);
        Some(host.unwrap_or_else(|| self.public_host(service_id)))
    }
}

fn host_from_config(config: &serde_json::Value, service_id: &str) -> anyhow::Result<String> {
    let rule = config
        .get("http")
        .and_then(|http| http.get("routers"))
        .and_then(|routers| routers.get(router_name(service_id)))
        .and_then(|router| router.get("rule"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing Traefik router rule"))?;
    let prefix = "Host(`";
    let start = rule
        .find(prefix)
        .ok_or_else(|| anyhow::anyhow!("Traefik rule does not contain Host(`...`)"))?
        + prefix.len();
    let capture = &rule[start..];

    for (index, character) in capture.char_indices() {
        if character != '`' {
            continue;
        }
        if capture[index + character.len_utf8()..].starts_with(')') {
            let host = &capture[..index];
            if host.is_empty() {
                anyhow::bail!("Traefik Host rule has an empty host");
            }
            if !is_valid_dns_name(host) {
                anyhow::bail!("Traefik Host rule has an invalid host");
            }
            return Ok(host.to_string());
        }
        anyhow::bail!("Traefik Host rule has an extra backtick");
    }

    anyhow::bail!("Traefik Host rule has no closing backtick")
}

/// Traefik router name for a service: `russel-{service_id}`.
fn router_name(service_id: &str) -> String {
    format!("russel-{service_id}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
    async fn primary_host_reads_custom_host_from_json() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir, "russel.local");
        let rules = vec![HostRule {
            host: "custom.example.com".to_string(),
        }];

        Ingress::register(&ing, "api", &Backend::localhost(3100), &rules)
            .await
            .unwrap();

        assert_eq!(
            Ingress::primary_host(&ing, "api"),
            Some("custom.example.com".to_string())
        );
    }

    #[test]
    fn primary_host_falls_back_for_malformed_json() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        std::fs::create_dir_all(&dir).unwrap();
        let ing = test_ingress(&dir, "russel.local");

        std::fs::write(dir.join("api.json"), "not json").unwrap();
        assert_eq!(
            Ingress::primary_host(&ing, "api"),
            Some("api.russel.local".to_string())
        );

        let config = serde_json::json!({
            "http": {
                "routers": {
                    "russel-api": {"rule": "Host(`bad`host`)"}
                }
            }
        });
        std::fs::write(dir.join("api.json"), serde_json::to_vec(&config).unwrap()).unwrap();
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
    async fn register_rejects_multiple_host_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let ing = test_ingress(tmp.path().join("dynamic"), "russel.local");
        let rules = vec![
            HostRule {
                host: "one.example.com".to_string(),
            },
            HostRule {
                host: "two.example.com".to_string(),
            },
        ];

        let error = Ingress::register(&ing, "api", &Backend::localhost(3100), &rules)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "v1 ingress supports one Host rule (got 2)"
        );
    }

    #[tokio::test]
    async fn swap_rejects_multiple_host_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let ing = test_ingress(tmp.path().join("dynamic"), "russel.local");
        let rules = vec![
            HostRule {
                host: "one.example.com".to_string(),
            },
            HostRule {
                host: "two.example.com".to_string(),
            },
        ];

        let error = Ingress::swap(&ing, "api", &Backend::localhost(3100), &rules)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "v1 ingress supports one Host rule (got 2)"
        );
    }

    #[tokio::test]
    async fn register_rejects_invalid_host_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let ing = test_ingress(tmp.path().join("dynamic"), "russel.local");
        let rules = vec![HostRule {
            host: "bad`host".to_string(),
        }];

        let error = Ingress::register(&ing, "api", &Backend::localhost(3100), &rules)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "ingress.host \"bad`host\" is not a valid DNS name"
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
        let rules = vec![HostRule {
            host: "custom.example.com".to_string(),
        }];
        Ingress::swap(&ing, "api", &new, &rules).await.unwrap();

        let raw = std::fs::read_to_string(dir.join("api.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            config["http"]["routers"]["russel-api"]["rule"],
            "Host(`custom.example.com`)"
        );
        assert_eq!(
            config["http"]["services"]["russel-api"]["loadBalancer"]["servers"][0]["url"],
            "http://127.0.0.1:3200"
        );
    }

    #[tokio::test]
    async fn swap_with_empty_rules_writes_default_host_after_custom_host() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir.clone(), "russel.local");
        let custom_rules = vec![HostRule {
            host: "custom.example.com".to_string(),
        }];

        Ingress::register(&ing, "api", &Backend::localhost(3100), &custom_rules)
            .await
            .unwrap();
        Ingress::swap(&ing, "api", &Backend::localhost(3200), &[])
            .await
            .unwrap();

        let raw = std::fs::read_to_string(dir.join("api.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            config["http"]["routers"]["russel-api"]["rule"],
            "Host(`api.russel.local`)"
        );
    }

    #[tokio::test]
    async fn swap_does_not_parse_previous_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("api.json"), "not json").unwrap();
        let ing = test_ingress(dir.clone(), "russel.local");
        let rules = vec![HostRule {
            host: "custom.example.com".to_string(),
        }];

        Ingress::swap(&ing, "api", &Backend::localhost(3200), &rules)
            .await
            .unwrap();

        let raw = std::fs::read_to_string(dir.join("api.json")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            config["http"]["routers"]["russel-api"]["rule"],
            "Host(`custom.example.com`)"
        );
    }

    #[tokio::test]
    async fn uniqueness_rejects_same_host_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir, "russel.local");
        let first_rules = vec![HostRule {
            host: "ABC.example.com".to_string(),
        }];
        let second_rules = vec![HostRule {
            host: "abc.EXAMPLE.com".to_string(),
        }];

        Ingress::register(&ing, "first", &Backend::localhost(3100), &first_rules)
            .await
            .unwrap();
        let error = Ingress::register(&ing, "second", &Backend::localhost(3200), &second_rules)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "ingress host \"abc.EXAMPLE.com\" is already routed by service \"first\""
        );
    }

    #[tokio::test]
    async fn uniqueness_skips_tmp_and_unparseable_peers() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        std::fs::create_dir_all(&dir).unwrap();
        let peer_config = serde_json::json!({
            "http": {
                "routers": {
                    "russel-peer": {"rule": "Host(`custom.example.com`)"}
                }
            }
        });
        let peer_bytes = serde_json::to_vec(&peer_config).unwrap();
        std::fs::write(dir.join("peer.json.123.tmp"), &peer_bytes).unwrap();
        std::fs::write(dir.join("broken.json"), "not json").unwrap();

        let ing = test_ingress(dir.clone(), "russel.local");
        let rules = vec![HostRule {
            host: "custom.example.com".to_string(),
        }];
        Ingress::register(&ing, "target", &Backend::localhost(3100), &rules)
            .await
            .unwrap();
        assert!(dir.join("target.json").exists());
    }

    #[tokio::test]
    async fn uniqueness_skips_self() {
        let tmp = tempfile::tempdir().unwrap();
        let ing = test_ingress(tmp.path().join("dynamic"), "russel.local");
        let first_rules = vec![HostRule {
            host: "custom.example.com".to_string(),
        }];
        let second_rules = vec![HostRule {
            host: "CUSTOM.EXAMPLE.COM".to_string(),
        }];

        Ingress::register(&ing, "api", &Backend::localhost(3100), &first_rules)
            .await
            .unwrap();
        Ingress::register(&ing, "api", &Backend::localhost(3200), &second_rules)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn uniqueness_rejects_custom_host_matching_peer_default() {
        let tmp = tempfile::tempdir().unwrap();
        let ing = test_ingress(tmp.path().join("dynamic"), "russel.local");

        Ingress::register(&ing, "api", &Backend::localhost(3100), &[])
            .await
            .unwrap();
        let rules = vec![HostRule {
            host: "api.russel.local".to_string(),
        }];
        let error = Ingress::register(&ing, "other", &Backend::localhost(3200), &rules)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "ingress host \"api.russel.local\" is already routed by service \"api\""
        );
    }

    #[tokio::test]
    async fn uniqueness_ok_when_dynamic_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let ing = test_ingress(&missing, "russel.local");
        ing.ensure_host_is_unique("api", "api.example.com")
            .await
            .unwrap();
        assert!(!missing.exists());
    }

    #[tokio::test]
    async fn concurrent_registers_same_host_only_one_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        // Two clients, matching default_ingress() constructing per request.
        let left = test_ingress(dir.clone(), "russel.local");
        let right = test_ingress(dir.clone(), "russel.local");
        let rules = vec![HostRule {
            host: "shared.example.com".to_string(),
        }];

        let left_backend = Backend::localhost(3100);
        let right_backend = Backend::localhost(3200);
        let (left_result, right_result) = tokio::join!(
            Ingress::register(&left, "alpha", &left_backend, &rules),
            Ingress::register(&right, "beta", &right_backend, &rules),
        );

        let reject = match (&left_result, &right_result) {
            (Err(error), Ok(())) | (Ok(()), Err(error)) => error.to_string(),
            other => panic!("expected one success and one uniqueness error, got {other:?}"),
        };
        assert!(reject.contains("already routed by service"), "{reject}");
        assert_ne!(
            dir.join("alpha.json").exists(),
            dir.join("beta.json").exists()
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

    #[test]
    fn from_env_falls_back_to_default_for_bad_domain() {
        let _lock = env_lock();
        let prev = std::env::var_os("RUSSEL_TRAEFIK_DOMAIN");
        unsafe { std::env::set_var("RUSSEL_TRAEFIK_DOMAIN", "bad ` domain") };
        let ing = TraefikFileIngress::from_env();
        assert_eq!(ing.domain, "russel.local");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("RUSSEL_TRAEFIK_DOMAIN", v),
                None => std::env::remove_var("RUSSEL_TRAEFIK_DOMAIN"),
            }
        }
    }
}
