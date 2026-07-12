/// Health checker — periodic health monitoring for deployed microVMs.
///
/// ponytail: single-shot TCP check, no periodic loop yet.
/// Add periodic health-checks with exponential backoff when monitoring matters.

#[derive(Debug, Default)]
pub struct HealthChecker;

impl HealthChecker {
    /// Check if a service is reachable via TCP at the given URL.
    /// Returns true if the connection succeeds within a short timeout.
    pub async fn check(&self, url: &str) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::net::TcpStream::connect(url).await.is_ok()
        })
        .await
        .unwrap_or(false)
    }
}
