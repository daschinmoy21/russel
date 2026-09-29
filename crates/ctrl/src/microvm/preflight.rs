//! Host privileges a microVM deploy needs, checked before the build so a
//! ctrl that cannot boot VMs fails in seconds with the reason (#461, #473).

use std::path::Path;

use crate::network::MicrovmNetMode;

/// `CAP_NET_ADMIN` bit in `/proc/<pid>/status` `CapEff` (linux/capability.h).
const CAP_NET_ADMIN: u32 = 12;

/// Whether this process holds `CAP_NET_ADMIN` (root, or an ambient capability).
pub(crate) fn ctrl_has_net_admin() -> bool {
    has_net_admin(&std::fs::read_to_string("/proc/self/status").unwrap_or_default())
}

/// Fail unless this process can open `/dev/kvm` read-write and can network
/// the VM: `CAP_NET_ADMIN` for a TAP, or `passt` on PATH for an unprivileged
/// ctrl (#461).
pub(crate) fn check_host_privileges() -> anyhow::Result<()> {
    let mode = MicrovmNetMode::for_host()?;
    check(
        kvm_accessible(Path::new("/dev/kvm")),
        mode,
        match mode {
            MicrovmNetMode::Tap => ctrl_has_net_admin(),
            MicrovmNetMode::Passt => on_path("passt"),
        },
    )
}

fn check(kvm: bool, mode: MicrovmNetMode, net_ok: bool) -> anyhow::Result<()> {
    let mut missing = Vec::new();
    if !kvm {
        missing.push("read-write access to /dev/kvm (kvm group)");
    }
    if !net_ok {
        missing.push(match mode {
            MicrovmNetMode::Tap => {
                "CAP_NET_ADMIN (RUSSEL_MICROVM_NET=tap: TAP devices, iptables FORWARD rules)"
            }
            MicrovmNetMode::Passt => "passt on PATH (unprivileged microVM networking)",
        });
    }
    if missing.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "microVMs are experimental and need {}, or set type = \"container\" in the Russelfile",
        missing.join(" and ")
    )
}

/// First Cloud Hypervisor major release that activates a virtio device only
/// once (cloud-hypervisor#7906). Before it, a second vCPU writing to a device
/// mid-activation panics the VMM thread and the guest hangs at boot (#510).
const CH_MIN_MAJOR_FOR_SMP: u32 = 52;

/// Fail a multi-vCPU passt (vhost-user NIC) cold boot on a Cloud Hypervisor older than
/// [`CH_MIN_MAJOR_FOR_SMP`] instead of letting it hang until the readiness
/// timeout. An unreadable version only warns: the real spawn reports a
/// missing binary, and an unknown build should not block deploys.
pub(crate) async fn check_ch_supports_cpus(cpus: u8) -> anyhow::Result<()> {
    if cpus <= 1 {
        return Ok(());
    }
    let Ok(out) = tokio::process::Command::new("cloud-hypervisor")
        .arg("--version")
        .output()
        .await
    else {
        return Ok(());
    };
    check_ch_version(ch_major(&String::from_utf8_lossy(&out.stdout)), cpus)
}

/// Major version from `cloud-hypervisor --version` (`cloud-hypervisor v51.1.0`).
fn ch_major(version_output: &str) -> Option<u32> {
    version_output
        .lines()
        .next()?
        .split_whitespace()
        .find_map(|w| w.strip_prefix('v'))?
        .split(['.', '-'])
        .next()?
        .parse()
        .ok()
}

fn check_ch_version(major: Option<u32>, cpus: u8) -> anyhow::Result<()> {
    match major {
        Some(major) if major < CH_MIN_MAJOR_FOR_SMP => anyhow::bail!(
            "cpus = {cpus} needs Cloud Hypervisor v{CH_MIN_MAJOR_FOR_SMP} or newer; this host has \
             v{major}, whose multi-vCPU guests on the passt NIC hang at boot (#510). Upgrade cloud-hypervisor, \
             or set cpus = 1 in the Russelfile"
        ),
        Some(_) => Ok(()),
        None => {
            tracing::warn!(
                cpus,
                "could not read the cloud-hypervisor version; skipping the multi-vCPU check"
            );
            Ok(())
        }
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(program)).is_ok_and(|m| {
                m.is_file()
                    && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
            })
        })
    })
}

fn kvm_accessible(dev: &Path) -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev)
        .is_ok()
}

/// Whether the `CapEff` line of a `/proc/<pid>/status` dump has `CAP_NET_ADMIN`.
fn has_net_admin(status: &str) -> bool {
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << CAP_NET_ADMIN) != 0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn net_admin_from_cap_eff() {
        // Root: all caps.
        assert!(has_net_admin("Name:\tx\nCapEff:\t000001ffffffffff\n"));
        // Unprivileged user.
        assert!(!has_net_admin("CapEff:\t0000000000000000\n"));
        // Only CAP_NET_ADMIN (ambient capability on a systemd unit).
        assert!(has_net_admin("CapEff:\t0000000000001000\n"));
        assert!(!has_net_admin("CapInh:\t0000000000001000\n"));
        assert!(!has_net_admin("CapEff:\tzz\n"));
    }

    #[test]
    fn check_names_each_missing_privilege() {
        assert!(check(true, MicrovmNetMode::Passt, true).is_ok());
        let err = check(false, MicrovmNetMode::Tap, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("/dev/kvm") && err.contains("CAP_NET_ADMIN"),
            "{err}"
        );
        assert!(err.contains("type = \"container\""), "{err}");
        let err = check(true, MicrovmNetMode::Passt, false)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("/dev/kvm") && err.contains("passt"), "{err}");
    }

    #[test]
    fn ch_major_parses_version_output() {
        assert_eq!(ch_major("cloud-hypervisor v51.1.0\n"), Some(51));
        assert_eq!(
            ch_major("cloud-hypervisor v53.0.0\nMigration Protocol Versions: 0\n"),
            Some(53)
        );
        assert_eq!(ch_major("cloud-hypervisor v52-dirty"), Some(52));
        assert_eq!(ch_major("cloud-hypervisor unknown"), None);
        assert_eq!(ch_major(""), None);
    }

    #[test]
    fn multi_vcpu_needs_ch_52() {
        let err = check_ch_version(Some(51), 4).unwrap_err().to_string();
        assert!(err.contains("v52") && err.contains("v51"), "{err}");
        assert!(err.contains("cpus = 1"), "{err}");
        assert!(check_ch_version(Some(52), 4).is_ok());
        assert!(check_ch_version(Some(53), 8).is_ok());
        // Unknown builds warn instead of blocking.
        assert!(check_ch_version(None, 4).is_ok());
    }

    #[tokio::test]
    async fn single_vcpu_skips_the_version_check() {
        assert!(check_ch_supports_cpus(1).await.is_ok());
    }

    #[test]
    fn on_path_finds_sh_only() {
        assert!(on_path("sh"));
        assert!(!on_path("definitely-not-a-russel-binary"));
    }

    #[test]
    fn missing_kvm_device_is_inaccessible() {
        assert!(!kvm_accessible(Path::new("/nonexistent/kvm")));
    }
}
