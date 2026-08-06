//! Stable node identity for agent heartbeats.
//!
//! Matches control-plane resolution (`RUSSEL_NODE_ID` → hostname → `local`)
//! so metadata `node_id` and agent identity stay aligned (Phase 0 / #212).

/// Env override for stable node identity.
pub const NODE_ID_ENV: &str = "RUSSEL_NODE_ID";

/// Resolve the node id for this agent process.
///
/// Order: `RUSSEL_NODE_ID` (trimmed, non-empty) → hostname → `"local"`.
pub fn resolve_node_id() -> String {
    resolve_node_id_with(
        std::env::var(NODE_ID_ENV).ok().as_deref(),
        hostname_for_node_id,
    )
}

/// Resolve node id with injectable env override and host fallback (tests).
pub fn resolve_node_id_with(
    env_override: Option<&str>,
    host_fallback: impl FnOnce() -> Option<String>,
) -> String {
    if let Some(v) = env_override {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    host_fallback().unwrap_or_else(|| "local".to_string())
}

fn hostname_for_node_id() -> Option<String> {
    // Prefer the portable HOSTNAME / COMPUTERNAME style env when present so
    // unit tests and containers without gethostname quirks stay simple.
    if let Ok(h) = std::env::var("HOSTNAME") {
        let t = h.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    // Fall back to /etc/hostname on Linux.
    if let Ok(contents) = std::fs::read_to_string("/etc/hostname") {
        let t = contents.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        assert_eq!(
            resolve_node_id_with(Some(" worker-a "), || Some("host".into())),
            "worker-a"
        );
    }

    #[test]
    fn blank_env_falls_through_to_host() {
        assert_eq!(
            resolve_node_id_with(Some("   "), || Some("host".into())),
            "host"
        );
    }

    #[test]
    fn missing_all_defaults_local() {
        assert_eq!(resolve_node_id_with(None, || None), "local");
    }
}
