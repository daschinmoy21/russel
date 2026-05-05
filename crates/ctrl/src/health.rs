#[derive(Debug, Default)]
pub struct HealthChecker;

impl HealthChecker {
    pub async fn check(&self, _url: &str) -> bool {
        true
    }
}
