use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::ingress::{Backend, HostRule, Ingress, Served};
use russel_core::config::is_valid_dns_name;

const DEFAULT_DOMAIN: &str = "russel.local";

/// Response header every route adds (#562): a token for the backend the
/// route points at. A request through Traefik that carries the new
/// backend's token proves Traefik loaded the new route.
pub const ROUTE_HEADER: &str = "X-Russel-Route";

/// Where [`Ingress::wait_served`] probes when `RUSSEL_TRAEFIK_ENTRYPOINT` is
/// unset: the `web` entry point of a Traefik on the same host.
const DEFAULT_ENTRYPOINT: &str = "http://127.0.0.1:80";

/// How long [`Ingress::wait_served`] waits for Traefik to serve a new
/// backend. Traefik applies file changes at most every 2 s by default
/// (`providersThrottleDuration`).
const SERVE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_INTERVAL: Duration = Duration::from_millis(100);
const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

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
/// One file per service: `{dynamic_dir}/{service_id}.yaml` (JSON content;
/// Traefik reads only YAML and TOML names). Traefik watches the directory;
/// no reload signal needed.
///
/// ## Environment
/// - `RUSSEL_TRAEFIK_DYNAMIC_DIR` — directory for dynamic config files
///   (default `$RUSSEL_DATA_DIR/traefik/dynamic`, i.e. `/var/lib/russel/traefik/dynamic`)
/// - `RUSSEL_TRAEFIK_DOMAIN` — domain suffix for Host rules
///   (default `russel.local`)
/// - `RUSSEL_TRAEFIK_TLS` — when `1`/`true`, attach TLS to routers (websecure)
/// - `RUSSEL_TRAEFIK_CERT_RESOLVER` — ACME cert resolver name (default `letsencrypt`)
/// - `RUSSEL_TRAEFIK_ENTRYPOINT` — where [`Ingress::wait_served`] sends its
///   probe (default `http://127.0.0.1:80`, best effort); `off` skips it
#[derive(Debug, Clone)]
pub struct TraefikFileIngress {
    dynamic_dir: PathBuf,
    domain: String,
    tls_enabled: bool,
    cert_resolver: String,
    probe: RouteProbe,
    serve_timeout: Duration,
}

/// Where [`Ingress::wait_served`] asks Traefik which backend it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RouteProbe {
    /// `RUSSEL_TRAEFIK_ENTRYPOINT=off`: do not ask.
    Off,
    /// Send requests for the route to this entry point. `strict` when the
    /// operator set it: anything short of the new backend answering is an
    /// error. The default entry point is best effort: nothing listening
    /// there, or a proxy that never shows Russel's route header, means
    /// there is no Traefik to wait for.
    Entrypoint { url: reqwest::Url, strict: bool },
    /// The setting could not be parsed; every wait fails with this.
    Invalid(String),
}

impl RouteProbe {
    fn from_setting(value: Option<&str>) -> Self {
        let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            return match reqwest::Url::parse(DEFAULT_ENTRYPOINT) {
                Ok(url) => Self::Entrypoint { url, strict: false },
                Err(e) => Self::Invalid(e.to_string()),
            };
        };
        if matches!(
            raw.to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no" | "none"
        ) {
            return Self::Off;
        }
        let with_scheme = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("http://{raw}")
        };
        match reqwest::Url::parse(&with_scheme) {
            Ok(url)
                if matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.port_or_known_default().is_some() =>
            {
                Self::Entrypoint { url, strict: true }
            }
            _ => Self::Invalid(format!(
                "RUSSEL_TRAEFIK_ENTRYPOINT={raw:?} is not http://host:port or https://host:port"
            )),
        }
    }
}

/// The value of [`ROUTE_HEADER`] for a route to `backend`: FNV-1a of its
/// URL. Stable across processes, and two generations never share a port.
fn route_token(backend: &Backend) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in backend.url().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// One probe's answer.
#[derive(Debug)]
enum ProbeAnswer {
    /// Traefik answered with this route token.
    Route(String),
    /// Something answered without the route header.
    NoMarker(reqwest::StatusCode),
    /// Nothing accepted the connection.
    Unreachable(String),
    /// Connected, then failed (timeout, TLS, bad response).
    Failed(String),
}

impl std::fmt::Display for ProbeAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Route(token) => write!(f, "{ROUTE_HEADER}: {token}"),
            Self::NoMarker(status) => write!(f, "HTTP {status} without {ROUTE_HEADER}"),
            Self::Unreachable(e) => write!(f, "cannot connect: {e}"),
            Self::Failed(e) => write!(f, "request failed: {e}"),
        }
    }
}

/// A client that sends requests for `host` to the entry point at `url`.
/// The route's real name goes in the URL so TLS SNI and Host both match the
/// router; DNS for it is pinned to the entry point. Certificates are not
/// checked: the probe asks which backend answers, and the certificate for a
/// new host may not exist yet.
async fn probe_client(
    url: &reqwest::Url,
    host: &str,
) -> Result<(reqwest::Client, String), ProbeAnswer> {
    let (Some(entry_host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
        return Err(ProbeAnswer::Failed(format!("no host or port in {url}")));
    };
    let addrs: Vec<_> = tokio::net::lookup_host(format!("{entry_host}:{port}"))
        .await
        .ok()
        .map(|addrs| addrs.collect())
        .filter(|addrs: &Vec<_>| !addrs.is_empty())
        .ok_or_else(|| ProbeAnswer::Unreachable(format!("cannot resolve {entry_host}")))?;
    probe_client_with_addrs(url, host, &addrs)
}

fn probe_client_with_addrs(
    url: &reqwest::Url,
    host: &str,
    addrs: &[std::net::SocketAddr],
) -> Result<(reqwest::Client, String), ProbeAnswer> {
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ProbeAnswer::Failed(format!("no port in {url}")))?;
    let client = reqwest::Client::builder()
        .resolve_to_addrs(host, addrs)
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .pool_max_idle_per_host(0)
        .timeout(PROBE_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| ProbeAnswer::Failed(e.to_string()))?;
    Ok((client, format!("{}://{host}:{port}/", url.scheme())))
}

async fn probe_once(client: &reqwest::Client, target: &str, host: &str) -> ProbeAnswer {
    match client
        .head(target)
        .header(reqwest::header::HOST, host)
        .send()
        .await
    {
        Ok(response) => match response
            .headers()
            .get(ROUTE_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            Some(token) => ProbeAnswer::Route(token.to_string()),
            None => ProbeAnswer::NoMarker(response.status()),
        },
        Err(e) if e.is_connect() => ProbeAnswer::Unreachable(e.to_string()),
        Err(e) => ProbeAnswer::Failed(e.to_string()),
    }
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
        let probe =
            RouteProbe::from_setting(std::env::var("RUSSEL_TRAEFIK_ENTRYPOINT").ok().as_deref());
        if let RouteProbe::Invalid(e) = &probe {
            tracing::error!("{e}; updates fail until it is fixed or set to off");
        }
        Self {
            dynamic_dir,
            domain,
            tls_enabled,
            cert_resolver,
            probe,
            serve_timeout: SERVE_TIMEOUT,
        }
    }

    /// A client on `dynamic_dir` whose probe follows `entrypoint` the way
    /// `RUSSEL_TRAEFIK_ENTRYPOINT` would, and gives up after `serve_timeout`.
    #[cfg(test)]
    pub(crate) fn for_tests(
        dynamic_dir: impl Into<PathBuf>,
        domain: &str,
        entrypoint: &str,
        serve_timeout: Duration,
    ) -> Self {
        Self {
            dynamic_dir: dynamic_dir.into(),
            domain: domain.to_string(),
            tls_enabled: false,
            cert_resolver: "letsencrypt".to_string(),
            probe: RouteProbe::from_setting(Some(entrypoint)),
            serve_timeout,
        }
    }

    /// The Host a route for `service_id` would be served under, validated.
    fn route_host(&self, service_id: &str, host_rules: &[HostRule]) -> anyhow::Result<String> {
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
        Ok(host)
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
        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            anyhow::anyhow!(
                "failed to scan Traefik dynamic dir {}: {e}",
                self.dynamic_dir.display()
            )
        })? {
            let file_name = entry.file_name();
            let Some(peer_id) = file_name.to_str().and_then(route_file_service) else {
                continue;
            };
            if peer_id == service_id {
                continue;
            }

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
        [ROUTE_EXT, LEGACY_ROUTE_EXT].iter().find_map(|ext| {
            std::fs::read_to_string(self.route_file(service_id, ext))
                .ok()
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .and_then(|config| host_from_config(&config, service_id).ok())
        })
    }

    /// `{dynamic_dir}/{service_id}.{ext}`.
    fn route_file(&self, service_id: &str, ext: &str) -> PathBuf {
        self.dynamic_dir.join(format!("{service_id}.{ext}"))
    }
}

/// Extension of the route files. Traefik's file provider reads `.yml`,
/// `.yaml` and `.toml` and skips everything else. The content is JSON, which
/// is also YAML.
const ROUTE_EXT: &str = "yaml";

/// What route files were called before #562. Traefik skipped them. They
/// still count for host uniqueness, and go when the route is next written
/// or removed.
const LEGACY_ROUTE_EXT: &str = "json";

/// The service a file in the dynamic dir routes, by its name. `None` for
/// anything else, such as an unfinished `.tmp` write.
fn route_file_service(file_name: &str) -> Option<&str> {
    file_name
        .strip_suffix(".yaml")
        .or_else(|| file_name.strip_suffix(".json"))
}

/// Remove `path`; a missing file is fine.
async fn remove_if_present(path: &Path) -> std::io::Result<bool> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Atomically write a Traefik dynamic config file with `mode 0644`.
///
/// Writes `{service_id}.yaml.<nonce>.tmp` in the same directory, fsyncs,
/// then renames it into place. On failure the temporary file is removed.
/// Traefik skips the `.tmp` name, so it only ever reads whole files.
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
        .unwrap_or("config.yaml");
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
        let host = self.route_host(service_id, host_rules)?;

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
            "middlewares": [&service_name],
        });
        if self.tls_enabled {
            router["tls"] = serde_json::json!({
                "certResolver": self.cert_resolver,
            });
        }

        // The route header lives in the same file as the backend, and
        // Traefik loads a file whole, so its value says which backend
        // Traefik routes this host to (`wait_served`).
        let config = serde_json::json!({
            "http": {
                "routers": {
                    &router_key: router
                },
                "middlewares": {
                    &service_name: {
                        "headers": {
                            "customResponseHeaders": {
                                ROUTE_HEADER: route_token(backend)
                            }
                        }
                    }
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

        let file_path = self.route_file(service_id, ROUTE_EXT);
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
        let legacy = self.route_file(service_id, LEGACY_ROUTE_EXT);
        if let Err(e) = remove_if_present(&legacy).await {
            tracing::warn!(path = %legacy.display(), error = %e, "cannot remove the old route file");
        }

        tracing::info!(
            service_id = %service_id,
            path = %file_path.display(),
            host = %host,
            backend = %backend.url(),
            "Traefik dynamic config written"
        );

        Ok(())
    }

    async fn check_route(&self, service_id: &str, host_rules: &[HostRule]) -> anyhow::Result<()> {
        let host = self.route_host(service_id, host_rules)?;
        self.ensure_host_is_unique(service_id, &host).await
    }

    async fn deregister(&self, service_id: &str) -> anyhow::Result<()> {
        let lock = dir_write_lock(&self.dynamic_dir);
        let _guard = lock.lock().await;
        for ext in [ROUTE_EXT, LEGACY_ROUTE_EXT] {
            let file_path = self.route_file(service_id, ext);
            match remove_if_present(&file_path).await {
                Ok(true) => tracing::info!(
                    service_id = %service_id,
                    path = %file_path.display(),
                    "Traefik dynamic config removed"
                ),
                Ok(false) => tracing::debug!(
                    service_id = %service_id,
                    path = %file_path.display(),
                    "Traefik dynamic config not found (already removed)"
                ),
                Err(e) => anyhow::bail!(
                    "failed to remove Traefik config {}: {e}",
                    file_path.display()
                ),
            }
        }
        Ok(())
    }

    async fn swap(
        &self,
        service_id: &str,
        new_backend: &Backend,
        host_rules: &[HostRule],
    ) -> anyhow::Result<()> {
        self.register(service_id, new_backend, host_rules).await
    }

    /// Send requests for the route through Traefik until one comes back
    /// with `backend`'s route token. Traefik's other answers decide what a
    /// timeout means: the old token is a Traefik that has not picked up the
    /// new file (an error); no token at all, or nothing listening, at the
    /// default entry point means there is no Traefik reading these files.
    async fn wait_served(
        &self,
        service_id: &str,
        backend: &Backend,
        host_rules: &[HostRule],
    ) -> anyhow::Result<Served> {
        let (url, strict) = match &self.probe {
            RouteProbe::Off => {
                return Ok(Served::Unchecked("RUSSEL_TRAEFIK_ENTRYPOINT is off".into()));
            }
            RouteProbe::Invalid(e) => anyhow::bail!("{e}"),
            RouteProbe::Entrypoint { url, strict } => (url, *strict),
        };
        let host = self.route_host(service_id, host_rules)?;
        let want = route_token(backend);
        let deadline = tokio::time::Instant::now() + self.serve_timeout;
        let mut saw_old_route = false;
        let last = loop {
            let answer = match probe_client(url, &host).await {
                Ok((client, target)) => probe_once(&client, &target, &host).await,
                Err(answer) => answer,
            };
            match &answer {
                ProbeAnswer::Route(token) if *token == want => {
                    tracing::info!(service_id, host = %host, backend = %backend.url(), "Traefik serves the new backend");
                    return Ok(Served::Confirmed);
                }
                ProbeAnswer::Route(_) => saw_old_route = true,
                ProbeAnswer::Unreachable(_) if !strict && !saw_old_route => {
                    return Ok(Served::Unchecked(format!(
                        "no Traefik answers at {url} ({answer}); set RUSSEL_TRAEFIK_ENTRYPOINT \
                         to its HTTP entry point, or to off on a host without Traefik"
                    )));
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                break answer;
            }
            tokio::time::sleep(PROBE_INTERVAL).await;
        };
        let waited = self.serve_timeout.as_secs();
        if strict || saw_old_route {
            anyhow::bail!(
                "Traefik at {url} did not serve {host} from the new backend {} within {waited}s \
                 (last answer: {last})",
                backend.url()
            );
        }
        Ok(Served::Unchecked(format!(
            "the proxy at {url} never answered for {host} with Traefik's {ROUTE_HEADER} header \
             in {waited}s (last answer: {last}); if Traefik serves this host elsewhere, set \
             RUSSEL_TRAEFIK_ENTRYPOINT, else set it to off"
        )))
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
        TraefikFileIngress::for_tests(dir, domain, "off", Duration::from_secs(1))
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

        std::fs::write(dir.join("api.yaml"), "not json").unwrap();
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
        std::fs::write(dir.join("api.yaml"), serde_json::to_vec(&config).unwrap()).unwrap();
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

        let file_path = dir.join("api.yaml");
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

        let file_path = dir.join("custom-svc.yaml");
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
        assert!(dir.join("api.yaml").exists());

        Ingress::deregister(&ing, "api").await.unwrap();
        assert!(!dir.join("api.yaml").exists());
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

        let raw = std::fs::read_to_string(dir.join("api.yaml")).unwrap();
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

        let raw = std::fs::read_to_string(dir.join("api.yaml")).unwrap();
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
        std::fs::write(dir.join("api.yaml"), "not json").unwrap();
        let ing = test_ingress(dir.clone(), "russel.local");
        let rules = vec![HostRule {
            host: "custom.example.com".to_string(),
        }];

        Ingress::swap(&ing, "api", &Backend::localhost(3200), &rules)
            .await
            .unwrap();

        let raw = std::fs::read_to_string(dir.join("api.yaml")).unwrap();
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

    /// #557: deploy asks before it stops the running generation. A host
    /// another service owns is refused without touching the route files.
    #[tokio::test]
    async fn check_route_refuses_a_taken_host_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir.clone(), "russel.local");
        let rules = vec![HostRule {
            host: "shared.example.com".to_string(),
        }];
        Ingress::register(&ing, "owner", &Backend::localhost(3100), &rules)
            .await
            .unwrap();

        let error = Ingress::check_route(&ing, "newcomer", &rules)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "ingress host \"shared.example.com\" is already routed by service \"owner\""
        );
        assert!(!dir.join("newcomer.yaml").exists());

        // The owner updating its own route, or a free host, is fine.
        Ingress::check_route(&ing, "owner", &rules).await.unwrap();
        Ingress::check_route(&ing, "newcomer", &[]).await.unwrap();
        // Same validation as `register`.
        let two = vec![rules[0].clone(), rules[0].clone()];
        assert!(Ingress::check_route(&ing, "newcomer", &two).await.is_err());
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
        std::fs::write(dir.join("peer.yaml.123.tmp"), &peer_bytes).unwrap();
        std::fs::write(dir.join("broken.yaml"), "not json").unwrap();

        let ing = test_ingress(dir.clone(), "russel.local");
        let rules = vec![HostRule {
            host: "custom.example.com".to_string(),
        }];
        Ingress::register(&ing, "target", &Backend::localhost(3100), &rules)
            .await
            .unwrap();
        assert!(dir.join("target.yaml").exists());
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
            dir.join("alpha.yaml").exists(),
            dir.join("beta.yaml").exists()
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
        let raw = std::fs::read_to_string(dir.join("api.yaml")).unwrap();
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

    /// #562: the route carries a header naming its backend, so a request
    /// through Traefik shows which version of the file it loaded.
    #[tokio::test]
    async fn register_marks_responses_with_the_backend_token() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir.clone(), "russel.local");
        let backend = Backend::localhost(3100);
        Ingress::register(&ing, "api", &backend, &[]).await.unwrap();

        let raw = std::fs::read_to_string(dir.join("api.yaml")).unwrap();
        let config: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            config["http"]["routers"]["russel-api"]["middlewares"],
            serde_json::json!(["russel-api"])
        );
        let headers = &config["http"]["middlewares"]["russel-api"]["headers"];
        assert_eq!(
            headers["customResponseHeaders"][ROUTE_HEADER],
            route_token(&backend)
        );
        assert_ne!(
            route_token(&backend),
            route_token(&Backend::localhost(3101))
        );
        // Stable: the same backend always gets the same token.
        assert_eq!(
            route_token(&backend),
            route_token(&Backend::localhost(3100))
        );
    }

    /// Traefik skips `.json` names, so routes are `.yaml` now. A route file
    /// from before still counts for uniqueness and goes on the next write.
    #[tokio::test]
    async fn legacy_json_route_files_are_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("dynamic");
        let ing = test_ingress(dir.clone(), "russel.local");
        Ingress::register(&ing, "api", &Backend::localhost(3100), &[])
            .await
            .unwrap();
        Ingress::register(&ing, "peer", &Backend::localhost(3200), &[])
            .await
            .unwrap();
        // Turn both into what an older ctrl left behind.
        std::fs::rename(dir.join("api.yaml"), dir.join("api.json")).unwrap();
        std::fs::rename(dir.join("peer.yaml"), dir.join("peer.json")).unwrap();
        assert_eq!(
            ing.host_from_dynamic_config("api").as_deref(),
            Some("api.russel.local")
        );

        let taken = vec![HostRule {
            host: "peer.russel.local".into(),
        }];
        let error = Ingress::register(&ing, "api", &Backend::localhost(3300), &taken)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("already routed by service \"peer\"")
        );

        Ingress::register(&ing, "api", &Backend::localhost(3300), &[])
            .await
            .unwrap();
        assert!(dir.join("api.yaml").exists());
        assert!(!dir.join("api.json").exists());

        Ingress::deregister(&ing, "peer").await.unwrap();
        assert!(!dir.join("peer.json").exists());
    }

    #[test]
    fn entrypoint_setting_parses() {
        let default = RouteProbe::from_setting(None);
        assert!(
            matches!(&default, RouteProbe::Entrypoint { url, strict: false } if url.as_str() == "http://127.0.0.1/")
        );
        assert_eq!(RouteProbe::from_setting(Some(" ")), default);
        assert_eq!(RouteProbe::from_setting(Some("off")), RouteProbe::Off);
        assert_eq!(RouteProbe::from_setting(Some("OFF")), RouteProbe::Off);
        let explicit = RouteProbe::from_setting(Some("127.0.0.1:8000"));
        assert!(
            matches!(&explicit, RouteProbe::Entrypoint { url, strict: true } if url.as_str() == "http://127.0.0.1:8000/")
        );
        let tls = RouteProbe::from_setting(Some("https://10.89.0.1"));
        assert!(
            matches!(&tls, RouteProbe::Entrypoint { url, strict: true } if url.port_or_known_default() == Some(443))
        );
        assert!(matches!(
            RouteProbe::from_setting(Some("ftp://x")),
            RouteProbe::Invalid(_)
        ));
    }

    /// An entry point that answers every request with `header` (when set)
    /// as its route token, and records the Host it was asked for.
    async fn fake_entrypoint(
        header: Arc<Mutex<Option<String>>>,
        hosts: Arc<Mutex<Vec<String>>>,
    ) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (header, hosts) = (header.clone(), hosts.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
                    if let Some(host) = head.lines().find_map(|l| l.strip_prefix("host: ")) {
                        hosts.lock().unwrap().push(host.trim().to_string());
                    }
                    let marker = header
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|t| format!("{ROUTE_HEADER}: {t}\r\n"))
                        .unwrap_or_default();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\n{marker}Content-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        port
    }

    /// Waits for the new token, and fails when Traefik keeps the old one.
    #[tokio::test]
    async fn wait_served_waits_for_the_new_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let header = Arc::new(Mutex::new(None));
        let hosts = Arc::new(Mutex::new(Vec::new()));
        let port = fake_entrypoint(header.clone(), hosts.clone()).await;
        let ing = TraefikFileIngress::for_tests(
            tmp.path(),
            "russel.local",
            &format!("http://127.0.0.1:{port}"),
            Duration::from_millis(600),
        );
        let (old, new) = (Backend::localhost(3100), Backend::localhost(3200));

        // Traefik still on the old file: an error, after the timeout.
        *header.lock().unwrap() = Some(route_token(&old));
        let error = Ingress::wait_served(&ing, "api", &new, &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not serve api.russel.local"), "{error}");
        assert_eq!(hosts.lock().unwrap()[0], "api.russel.local");

        // Traefik picks the new file up while the probe waits.
        let flip = header.clone();
        let token = route_token(&new);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            *flip.lock().unwrap() = Some(token);
        });
        let ing = TraefikFileIngress::for_tests(
            tmp.path(),
            "russel.local",
            &format!("http://127.0.0.1:{port}"),
            Duration::from_secs(5),
        );
        let started = std::time::Instant::now();
        assert_eq!(
            Ingress::wait_served(&ing, "api", &new, &[]).await.unwrap(),
            Served::Confirmed
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    /// The default entry point is best effort: no proxy, or a proxy that is
    /// not Traefik with Russel's routes, does not fail the deploy. A set
    /// entry point does.
    #[tokio::test]
    async fn wait_served_default_entrypoint_is_best_effort() {
        let tmp = tempfile::tempdir().unwrap();
        let header = Arc::new(Mutex::new(None));
        let hosts = Arc::new(Mutex::new(Vec::new()));
        let port = fake_entrypoint(header, hosts).await;
        let new = Backend::localhost(3200);

        // A proxy with no route header.
        let mut ing = TraefikFileIngress::for_tests(
            tmp.path(),
            "russel.local",
            "",
            Duration::from_millis(300),
        );
        ing.probe = RouteProbe::Entrypoint {
            url: reqwest::Url::parse(&format!("http://127.0.0.1:{port}")).unwrap(),
            strict: false,
        };
        let served = Ingress::wait_served(&ing, "api", &new, &[]).await.unwrap();
        assert!(matches!(served, Served::Unchecked(ref why) if why.contains("never answered")));
        ing.probe = RouteProbe::from_setting(Some(&format!("http://127.0.0.1:{port}")));
        assert!(Ingress::wait_served(&ing, "api", &new, &[]).await.is_err());

        // Nothing listening: best effort returns at once, strict fails.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        ing.probe = RouteProbe::Entrypoint {
            url: reqwest::Url::parse(&format!("http://127.0.0.1:{closed}")).unwrap(),
            strict: false,
        };
        let started = std::time::Instant::now();
        let served = Ingress::wait_served(&ing, "api", &new, &[]).await.unwrap();
        assert!(matches!(served, Served::Unchecked(ref why) if why.contains("no Traefik answers")));
        assert!(started.elapsed() < Duration::from_millis(300));
        ing.probe = RouteProbe::from_setting(Some(&format!("http://127.0.0.1:{closed}")));
        assert!(Ingress::wait_served(&ing, "api", &new, &[]).await.is_err());

        ing.probe = RouteProbe::Off;
        assert!(matches!(
            Ingress::wait_served(&ing, "api", &new, &[]).await.unwrap(),
            Served::Unchecked(_)
        ));
    }
    #[tokio::test]
    async fn probe_falls_back_to_a_working_resolved_address() {
        let backend = Backend::localhost(3200);
        let header = Arc::new(Mutex::new(Some(route_token(&backend))));
        let port = fake_entrypoint(header, Arc::new(Mutex::new(vec![]))).await;
        let endpoint = reqwest::Url::parse(&format!("http://localhost:{port}")).unwrap();
        let addrs = [
            std::net::SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
            std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        ];
        let (client, target) =
            probe_client_with_addrs(&endpoint, "api.russel.local", &addrs).unwrap();
        assert!(
            matches!(probe_once(&client, &target, "api.russel.local").await,
            ProbeAnswer::Route(token) if token == route_token(&backend))
        );
        let tmp = tempfile::tempdir().unwrap();
        let ing = TraefikFileIngress::for_tests(
            tmp.path(),
            "russel.local",
            endpoint.as_str(),
            Duration::from_millis(500),
        );
        assert!(matches!(
            ing.wait_served("api", &backend, &[]).await.unwrap(),
            Served::Confirmed
        ));
    }
}
