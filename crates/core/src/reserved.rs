//! Reserved (non-service) directory names under the Russel data root.

/// Directories under `/var/lib/russel` (and peers) that are **not** user services.
///
/// Used by list/reconcile/cleanup discovery so internal layout never appears as
/// deployable services (e.g. dashboard `GET /vms`).
///
/// - `*.bak` — dual-live / destroy backups
/// - `traefik` — ingress dynamic config root
/// - `secrets` — host secrets store
/// - `_pool` — warm-pool snapshot state
pub fn is_reserved_service_dir(name: &str) -> bool {
    name.ends_with(".bak") || name == "traefik" || name == "secrets" || name == "_pool"
}

#[cfg(test)]
mod tests {
    use super::is_reserved_service_dir;

    #[test]
    fn reserved_names_rejected() {
        for name in ["traefik", "secrets", "_pool", "api.bak", "svc.bak"] {
            assert!(is_reserved_service_dir(name), "{name:?} should be reserved");
        }
    }

    #[test]
    fn normal_ids_accepted() {
        for name in [
            "api",
            "pooltpl",
            "basic-http-tester-another",
            "my-service",
            "app1",
        ] {
            assert!(
                !is_reserved_service_dir(name),
                "{name:?} should be accepted"
            );
        }
    }
}
