//! Static dashboard served by `russel-ctrl` on the same HTTP listener as the API.
//!
//! Pages are public (the JS then sends Bearer for `/api/*`). API routes stay
//! authenticated. GET `/deploy` is the dashboard page; POST `/deploy` is the API.

use std::path::{Path, PathBuf};

use axum::http::Method;

/// Dist directory for the built Astro dashboard (`index.html` at the root).
#[derive(Clone, Debug)]
pub struct DashboardDir(pub PathBuf);

/// True when this request is a dashboard page or asset and must skip Bearer auth.
pub fn is_public_dashboard_request(method: &Method, path: &str) -> bool {
    if method != Method::GET && method != Method::HEAD {
        return false;
    }
    dashboard_page_rel(path).is_some() || is_astro_asset_path(path)
}

/// Hashed Vite/Astro assets live under `/_astro/`.
pub fn is_astro_asset_path(path: &str) -> bool {
    path == "/_astro" || path.starts_with("/_astro/")
}

/// Map a request path to a file relative to the dashboard dist root.
///
/// Only the known Astro pages and `favicon.svg`. Unknown paths return `None`
/// so `/vms` and friends stay on the API router.
pub fn dashboard_page_rel(uri_path: &str) -> Option<&'static str> {
    match uri_path {
        "/" | "" => Some("index.html"),
        "/services" | "/services/" => Some("services/index.html"),
        "/service-detail" | "/service-detail/" => Some("service-detail/index.html"),
        "/deploy" | "/deploy/" => Some("deploy/index.html"),
        "/settings" | "/settings/" => Some("settings/index.html"),
        "/favicon.svg" => Some("favicon.svg"),
        _ => None,
    }
}

/// Absolute path of a dashboard page file, if this URI is a known page.
pub fn dashboard_page_file(dir: &Path, uri_path: &str) -> Option<PathBuf> {
    dashboard_page_rel(uri_path).map(|rel| dir.join(rel))
}

/// `dir` is a dashboard dist when it contains `index.html`.
pub fn validate_dashboard_dir(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() || !dir.join("index.html").is_file() {
        return None;
    }
    Some(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()))
}

/// Locate `dashboard/dist` (or an installed share copy).
///
/// Search order:
/// 1. `explicit` (`--dashboard-dir` / `RUSSEL_DASHBOARD_DIR`)
/// 2. `{exe}/../share/russel/dashboard` (FHS next to `/usr/local/bin`)
/// 3. walk up from the executable looking for `dashboard/dist` (cargo run)
/// 4. `/usr/local/share/russel/dashboard`
/// 5. `./dashboard/dist` from the process cwd
pub fn find_dashboard_dir(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        return validate_dashboard_dir(dir);
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        candidates.push(exe_dir.join("../share/russel/dashboard"));
        candidates.push(exe_dir.join("dashboard/dist"));
        let mut cur = exe_dir.to_path_buf();
        for _ in 0..6 {
            candidates.push(cur.join("dashboard/dist"));
            if !cur.pop() {
                break;
            }
        }
    }
    candidates.push(PathBuf::from("/usr/local/share/russel/dashboard"));
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("dashboard/dist"));
    }

    candidates
        .into_iter()
        .find_map(|p| validate_dashboard_dir(&p))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn page_map_covers_astro_routes() {
        assert_eq!(dashboard_page_rel("/"), Some("index.html"));
        assert_eq!(
            dashboard_page_rel("/services/"),
            Some("services/index.html")
        );
        assert_eq!(dashboard_page_rel("/deploy"), Some("deploy/index.html"));
        assert_eq!(dashboard_page_rel("/favicon.svg"), Some("favicon.svg"));
        assert_eq!(dashboard_page_rel("/vms"), None);
        assert_eq!(dashboard_page_rel("/api/vms"), None);
        assert_eq!(dashboard_page_rel("/_astro/app.js"), None);
    }

    #[test]
    fn public_gets_skip_auth_only_for_ui() {
        assert!(is_public_dashboard_request(&Method::GET, "/"));
        assert!(is_public_dashboard_request(&Method::HEAD, "/settings"));
        assert!(is_public_dashboard_request(&Method::GET, "/deploy"));
        assert!(is_public_dashboard_request(
            &Method::GET,
            "/_astro/index.js"
        ));
        assert!(!is_public_dashboard_request(&Method::POST, "/deploy"));
        assert!(!is_public_dashboard_request(&Method::GET, "/vms"));
        assert!(!is_public_dashboard_request(&Method::GET, "/api/vms"));
        assert!(!is_public_dashboard_request(&Method::DELETE, "/"));
    }

    #[test]
    fn validate_requires_index_html() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_dashboard_dir(dir.path()).is_none());
        std::fs::write(dir.path().join("index.html"), "<html></html>").unwrap();
        let found = validate_dashboard_dir(dir.path()).expect("index.html present");
        assert!(found.join("index.html").is_file());
    }

    #[test]
    fn explicit_missing_dir_is_none() {
        assert!(find_dashboard_dir(Some(Path::new("/no/such/dashboard-dist"))).is_none());
    }
}
