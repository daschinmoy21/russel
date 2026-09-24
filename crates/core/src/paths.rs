//! Host state root. `RUSSEL_DATA_DIR` overrides the default `/var/lib/russel`.

use std::path::{Path, PathBuf};

pub const DEFAULT_DATA_ROOT: &str = "/var/lib/russel";

/// State directory for services, the catalog, and managed volumes.
///
/// A relative or empty `RUSSEL_DATA_DIR` is ignored so a bad env value cannot
/// park service data in the process working directory.
pub fn data_root() -> PathBuf {
    data_root_from(std::env::var("RUSSEL_DATA_DIR").ok().as_deref())
}

pub fn data_root_from(value: Option<&str>) -> PathBuf {
    if let Some(raw) = value {
        let raw = raw.trim();
        let path = Path::new(raw);
        if !raw.is_empty() && path.is_absolute() {
            return path.to_path_buf();
        }
    }
    PathBuf::from(DEFAULT_DATA_ROOT)
}

pub fn service_dir(service_id: &str) -> PathBuf {
    data_root().join(service_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_relative_env_uses_default() {
        assert_eq!(data_root_from(None), PathBuf::from("/var/lib/russel"));
        assert_eq!(data_root_from(Some("")), PathBuf::from("/var/lib/russel"));
        assert_eq!(data_root_from(Some("  ")), PathBuf::from("/var/lib/russel"));
        assert_eq!(
            data_root_from(Some("relative/russel")),
            PathBuf::from("/var/lib/russel")
        );
    }

    #[test]
    fn absolute_env_is_the_root() {
        assert_eq!(
            data_root_from(Some(" /srv/russel ")),
            PathBuf::from("/srv/russel")
        );
        assert_eq!(
            data_root_from(Some("/srv/russel")).join("api"),
            PathBuf::from("/srv/russel/api")
        );
    }
}
