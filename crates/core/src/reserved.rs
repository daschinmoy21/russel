//! Reserved (non-service) directory names under the Russel data root.

/// Directories under `/var/lib/russel` (and peers) that are **not** user services.
///
/// Used by list/reconcile/cleanup discovery so internal layout never appears as
/// deployable services (e.g. dashboard `GET /vms`).
///
/// - `*.bak` — dual-live / destroy backups
/// - `*.volumes-stash` — managed volumes parked outside the service dir during redeploy
/// - `traefik` — ingress dynamic config root
/// - `secrets` — host secrets store
/// - `_pool` — warm-pool snapshot state
/// - `_checkouts` — git clones that deploys build from
/// - `_microvms` — microVM marker dirs under a relocated data root
/// - `.*` — dot-directories. `install.sh host` makes the data root the
///   `russel` account's home, so Podman and systemd write `.config`,
///   `.local`, and `.cache` there. Service ids never start with `.` (#526).
pub fn is_reserved_service_dir(name: &str) -> bool {
    name.starts_with('.')
        || name.ends_with(".bak")
        || name.ends_with(".volumes-stash")
        || name == "traefik"
        || name == "secrets"
        || name == "_pool"
        || name == "_checkouts"
        || name == crate::paths::MICROVMS_SUBDIR
}

#[cfg(test)]
mod tests {
    use super::is_reserved_service_dir;

    #[test]
    fn reserved_names_rejected() {
        for name in [
            "traefik",
            "secrets",
            "_pool",
            "_checkouts",
            "_microvms",
            "api.bak",
            "svc.bak",
            "api.volumes-stash",
            ".config",
            ".local",
            ".cache",
        ] {
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
