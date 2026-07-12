/// Health checker — periodic health monitoring for deployed microVMs.
///
/// ponytail: single-shot TCP check, no periodic loop yet.
/// Add periodic health-checks with exponential backoff when monitoring matters.

#[derive(Debug, Default)]
pub struct HealthChecker;

impl HealthChecker {
    /// Check if a service is reachable via TCP at the given socket address.
    /// Accepts a raw `host:port` string (e.g. `"10.0.5.2:3000"`) or a full
    /// HTTP URL — the scheme and path are stripped automatically.
    /// Returns true if the connection succeeds within a short timeout.
    pub async fn check(&self, addr: &str) -> bool {
        // Strip scheme and path so callers can pass http://... URLs directly
        let addr = addr
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or(addr);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::net::TcpStream::connect(addr).await.is_ok()
        })
        .await
        .unwrap_or(false)
    }
}
