//! Shell-safe env helpers for deploy (bin names, quoting, container env list).

use std::collections::HashMap;

pub use russel_core::ids::validate_bin_name;

/// Shell-safe single-quoted value for deploy.env: escapes embedded `'` as `'\''`.
pub fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

/// Build the env list for a container start spec: PORT first (managed),
/// then user env vars (already validated; PORT filtered out to prevent override).
pub fn build_container_env(
    guest_port: u16,
    user_env: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vec![("PORT".to_string(), guest_port.to_string())];
    for (key, value) in user_env {
        if key == "PORT" {
            continue; // managed by Russel, user cannot override
        }
        env.push((key.clone(), value.clone()));
    }
    env
}

/// Move the values that came from `secret://` refs in `declared` out of the
/// resolved `env`, so the container runner delivers them as Podman secrets
/// instead of `-e KEY=value` (#457). Returns `(plain, secret)`.
pub fn split_secret_env(
    env: Vec<(String, String)>,
    declared: &HashMap<String, String>,
) -> (Vec<(String, String)>, Vec<(String, String)>) {
    env.into_iter().partition(|(key, _)| {
        !declared
            .get(key)
            .is_some_and(|v| crate::secrets::is_secret_ref(v))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn split_secret_env_moves_only_secret_refs() {
        let declared: HashMap<String, String> = [
            (
                "DB_PASSWORD".to_string(),
                "secret://DB_PASSWORD".to_string(),
            ),
            ("LOG_LEVEL".to_string(), "info".to_string()),
        ]
        .into();
        let resolved: HashMap<String, String> = [
            ("DB_PASSWORD".to_string(), "hunter2".to_string()),
            ("LOG_LEVEL".to_string(), "info".to_string()),
        ]
        .into();
        let (mut plain, secret) = split_secret_env(build_container_env(3000, &resolved), &declared);
        plain.sort();
        assert_eq!(
            plain,
            vec![
                ("LOG_LEVEL".to_string(), "info".to_string()),
                ("PORT".to_string(), "3000".to_string()),
            ]
        );
        assert_eq!(
            secret,
            vec![("DB_PASSWORD".to_string(), "hunter2".to_string())]
        );
    }
}
