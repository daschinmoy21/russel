#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct HealthChecker;

#[allow(dead_code)]
impl HealthChecker {
    pub async fn check(&self, _url: &str) -> bool {
        tracing::warn!("health checking is not yet implemented");
        true
    }
}
