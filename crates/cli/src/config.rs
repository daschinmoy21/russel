//! Saved CLI login: control-plane URL + API token.
//!
//! File: `$RUSSEL_CONFIG_DIR/config.toml` or `~/.config/russel/config.toml`.
//! Directory mode `0700`, file mode `0600`. Env vars override the file:
//! `RUSSEL_CONTROL_PLANE`, `RUSSEL_API_TOKEN`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use russel_core::tokens::{check_token_min_length, normalize_token};

const DEFAULT_CONTROL_PLANE: &str = "http://127.0.0.1:7878";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    Config,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "--control-plane",
            Self::Env => "env",
            Self::Config => "config",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_plane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub control_plane: String,
    pub control_plane_source: Source,
    pub token: Option<String>,
    pub token_source: Option<Source>,
    pub config_path: PathBuf,
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("RUSSEL_CONFIG_DIR") {
        let d = dir.trim();
        if !d.is_empty() {
            return Ok(PathBuf::from(d));
        }
    }
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    if let Some(xdg) = xdg.filter(|s| !s.trim().is_empty()) {
        return Ok(PathBuf::from(xdg).join("russel"));
    }
    let home =
        std::env::var("HOME").map_err(|_| anyhow!("HOME is not set; cannot find config dir"))?;
    Ok(PathBuf::from(home).join(".config/russel"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

pub fn load() -> Result<FileConfig> {
    let path = config_path()?;
    load_from_path(&path)
}

pub fn load_from_path(path: &Path) -> Result<FileConfig> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))
}

pub fn save(cfg: &FileConfig) -> Result<PathBuf> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dir)?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&dir, perms)?;
    }
    let path = dir.join("config.toml");
    let body = toml::to_string_pretty(cfg).context("serialize config.toml")?;
    write_private_file(&path, body.as_bytes())?;
    Ok(path)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::fs::OpenOptions;
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn control_plane_flag_in_args<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|a| {
        let a = a.as_ref();
        a == "--control-plane" || a.starts_with("--control-plane=")
    })
}

/// Trim, drop trailing `/`, ASCII case-fold. `localhost` and `127.0.0.1` stay distinct.
fn origin_key(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn same_origin(request: &str, saved: &str) -> bool {
    let request = origin_key(request);
    let saved = origin_key(saved);
    !request.is_empty() && request == saved
}

/// Env token first (any origin). Else the file token, only when `control_plane`
/// matches the origin saved at login.
pub fn token_for(control_plane: &str) -> Option<(String, Source)> {
    if let Some(t) = normalize_token(std::env::var("RUSSEL_API_TOKEN").ok().as_deref()) {
        return Some((t, Source::Env));
    }
    let file = load().ok()?;
    let saved_url = file
        .control_plane
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    if !same_origin(control_plane, saved_url) {
        return None;
    }
    let t = normalize_token(file.token.as_deref())?;
    Some((t, Source::Config))
}

/// Resolve URL + token: flag/env, then config file, then built-in default.
///
/// `cli_control_plane` is clap's `--control-plane` / `RUSSEL_CONTROL_PLANE`
/// value when present.
pub fn resolve(cli_control_plane: Option<&str>) -> Result<Resolved> {
    let path = config_path()?;
    let file = load_from_path(&path)?;
    let flag_present = control_plane_flag_in_args(std::env::args());
    let env_cp = std::env::var("RUSSEL_CONTROL_PLANE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let (control_plane, control_plane_source) = if flag_present {
        let url = cli_control_plane
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("--control-plane requires a URL"))?;
        (url.to_string(), Source::Flag)
    } else if let Some(url) = env_cp {
        (url, Source::Env)
    } else if let Some(url) = file
        .control_plane
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        (url.to_string(), Source::Config)
    } else {
        (DEFAULT_CONTROL_PLANE.to_string(), Source::Default)
    };

    let (token, token_source) = match token_for(&control_plane) {
        Some((t, src)) => (Some(t), Some(src)),
        None => (None, None),
    };

    Ok(Resolved {
        control_plane,
        control_plane_source,
        token,
        token_source,
        config_path: path,
    })
}

pub fn validate_token(token: &str) -> Result<()> {
    check_token_min_length(token).map_err(anyhow::Error::msg)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use tempfile::TempDir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct Isolated {
        _dir: TempDir,
        _lock: MutexGuard<'static, ()>,
        prev_config_dir: Option<String>,
        prev_token: Option<String>,
        prev_control_plane: Option<String>,
    }

    fn restore_var(key: &str, prev: Option<&str>) {
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    impl Drop for Isolated {
        fn drop(&mut self) {
            restore_var("RUSSEL_CONFIG_DIR", self.prev_config_dir.as_deref());
            restore_var("RUSSEL_API_TOKEN", self.prev_token.as_deref());
            restore_var("RUSSEL_CONTROL_PLANE", self.prev_control_plane.as_deref());
        }
    }

    fn isolated() -> Isolated {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev_config_dir = std::env::var("RUSSEL_CONFIG_DIR").ok();
        let prev_token = std::env::var("RUSSEL_API_TOKEN").ok();
        let prev_control_plane = std::env::var("RUSSEL_CONTROL_PLANE").ok();
        let dir = TempDir::new().unwrap();
        unsafe {
            std::env::set_var("RUSSEL_CONFIG_DIR", dir.path());
            std::env::remove_var("RUSSEL_API_TOKEN");
            std::env::remove_var("RUSSEL_CONTROL_PLANE");
        }
        Isolated {
            _dir: dir,
            _lock: lock,
            prev_config_dir,
            prev_token,
            prev_control_plane,
        }
    }

    fn sample_token() -> String {
        "a".repeat(32)
    }

    #[test]
    fn roundtrip_config() {
        let _dir = isolated();
        let cfg = FileConfig {
            control_plane: Some("http://127.0.0.1:7878".into()),
            token: Some("a".repeat(32)),
        };
        let path = save(&cfg).unwrap();
        assert!(path.ends_with("config.toml"));
        let loaded = load().unwrap();
        assert_eq!(loaded, cfg);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let _dir = isolated();
        let cfg = load().unwrap();
        assert_eq!(cfg, FileConfig::default());
    }

    #[test]
    fn control_plane_flag_separate_form() {
        assert!(control_plane_flag_in_args([
            "russel",
            "--control-plane",
            "https://ctrl.example.com",
            "ps",
        ]));
    }

    #[test]
    fn control_plane_flag_equals_form() {
        assert!(control_plane_flag_in_args([
            "russel",
            "--control-plane=https://ctrl.example.com",
            "ps",
        ]));
    }

    #[test]
    fn control_plane_flag_absent() {
        assert!(!control_plane_flag_in_args(["russel", "ps"]));
        assert!(!control_plane_flag_in_args(["russel", "--insecure", "ps"]));
        assert!(!control_plane_flag_in_args([
            "russel",
            "--control-plane-url",
            "x"
        ]));
    }

    #[test]
    fn saved_token_used_when_origins_match_including_trailing_slash() {
        let _iso = isolated();
        let token = sample_token();
        save(&FileConfig {
            control_plane: Some("https://ctrl.example.com/".into()),
            token: Some(token.clone()),
        })
        .unwrap();
        assert_eq!(
            token_for("https://ctrl.example.com"),
            Some((token.clone(), Source::Config))
        );
        assert_eq!(
            token_for("HTTPS://CTRL.EXAMPLE.COM/"),
            Some((token.clone(), Source::Config))
        );
        let resolved = resolve(None).unwrap();
        assert_eq!(resolved.control_plane, "https://ctrl.example.com/");
        assert_eq!(resolved.token.as_deref(), Some(token.as_str()));
        assert_eq!(resolved.token_source, Some(Source::Config));
    }

    #[test]
    fn saved_token_not_used_for_different_https_origin() {
        let _iso = isolated();
        save(&FileConfig {
            control_plane: Some("https://ctrl.example.com".into()),
            token: Some(sample_token()),
        })
        .unwrap();
        assert!(token_for("https://other.example.com").is_none());
        unsafe {
            std::env::set_var("RUSSEL_CONTROL_PLANE", "https://other.example.com");
        }
        let resolved = resolve(Some("https://other.example.com")).unwrap();
        assert_eq!(resolved.control_plane, "https://other.example.com");
        assert_eq!(resolved.control_plane_source, Source::Env);
        assert!(resolved.token.is_none());
        assert!(resolved.token_source.is_none());
    }

    #[test]
    fn env_token_used_for_different_origin() {
        let _iso = isolated();
        save(&FileConfig {
            control_plane: Some("https://ctrl.example.com".into()),
            token: Some(sample_token()),
        })
        .unwrap();
        let env_token = "e".repeat(32);
        unsafe {
            std::env::set_var("RUSSEL_API_TOKEN", &env_token);
            std::env::set_var("RUSSEL_CONTROL_PLANE", "https://other.example.com");
        }
        assert_eq!(
            token_for("https://other.example.com"),
            Some((env_token.clone(), Source::Env))
        );
        let resolved = resolve(None).unwrap();
        assert_eq!(resolved.control_plane, "https://other.example.com");
        assert_eq!(resolved.token.as_deref(), Some(env_token.as_str()));
        assert_eq!(resolved.token_source, Some(Source::Env));
    }

    #[test]
    fn saved_token_localhost_is_not_loopback_ip() {
        let _iso = isolated();
        let token = sample_token();
        save(&FileConfig {
            control_plane: Some("http://127.0.0.1:7878".into()),
            token: Some(token.clone()),
        })
        .unwrap();
        assert_eq!(
            token_for("http://127.0.0.1:7878"),
            Some((token, Source::Config))
        );
        assert!(token_for("http://localhost:7878").is_none());
    }
}
