//! Deploy pipeline: cold deploy, dual-live generations, and rollback helpers.

use std::collections::HashMap;

mod config;
mod env;
mod pipeline;
mod rollback;
mod runtime;

// Re-export the pre-split public surface so callers keep `crate::deploy::…`.
pub use crate::metadata::prior_runtime_from_disk;
pub use env::{build_container_env, shell_quote, validate_bin_name};
pub use pipeline::DeployPipeline;

/// Write `deploy.env` into `cfg_dir` (created with mode `0700`) for the guest
/// agent init: `VM_IP`/`HOST_IP`/`PORT`/`APP` plus the shell-quoted user env.
///
/// Shared by cold deploy and microVM rollback so the file format, quoting, and
/// atomic write semantics stay identical across both paths.
pub(crate) fn write_deploy_env(
    cfg_dir: &str,
    vm_ip: &str,
    host_ip: &str,
    guest_port: u16,
    app_path: &str,
    env: &HashMap<String, String>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(cfg_dir)
        .map_err(|e| anyhow::anyhow!("failed to create config dir {}: {}", cfg_dir, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(cfg_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| anyhow::anyhow!("chmod config dir: {e}"))?;
    }
    // Shell-quote APP path to prevent injection through deploy.env.
    let mut deploy_env = format!(
        "VM_IP={}\nHOST_IP={}\nPORT={}\nAPP={}\n",
        vm_ip,
        host_ip,
        guest_port,
        shell_quote(app_path)
    );
    // Append user env vars, shell-quoted (may include expanded secrets).
    for (key, value) in env {
        deploy_env.push_str(&format!("{}={}\n", key, shell_quote(value)));
    }
    let deploy_env_path = std::path::PathBuf::from(format!("{cfg_dir}/deploy.env"));
    crate::secrets::secure_write(&deploy_env_path, deploy_env.as_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
