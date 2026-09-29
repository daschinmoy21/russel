# Security policy

## Supported versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |
| < 0.1   | No        |

Fixes land on `main` and ship in the next 0.1.x release. Only the latest 0.1.x release gets security fixes.

## Reporting a vulnerability

Do **not** open a public issue, pull request, or discussion for a security problem.

Report it privately through GitHub private vulnerability reporting on [daschinmoy21/russel](https://github.com/daschinmoy21/russel): open the **Security** tab and choose **Report a vulnerability** ([direct link](https://github.com/daschinmoy21/russel/security/advisories/new)). Only the maintainer can see the report.

If the form is unavailable, open a public issue titled "Security contact request" that contains **no details** about the vulnerability, and the maintainer will set up a private channel with you.

Include:

- the Russel version (`russel --version`) and how it was installed (release binaries, NixOS module, source);
- the runtime involved (container or microVM) and the host OS;
- steps to reproduce, or a proof of concept;
- the impact you expect (what an attacker gains, from which starting position).

What to expect:

- an acknowledgement within **7 days**;
- an initial assessment (accepted, needs more information, or out of scope) within **14 days**;
- for accepted reports, a fix and a GitHub security advisory, with credit unless you ask not to be named. Please keep the details private until the advisory is published.

Russel is maintained by one person, so these are best-effort targets, not an SLA.

## Threat model and scope

Russel is a privileged single-host deployer for **one trusted operator**. It is **not multi-tenant**. The control plane runs Podman, manages TAP devices and iptables, and runs Cloud Hypervisor under sudo for microVMs. The operator, the API token holder, and the source repos they deploy are trusted: Nix builds trust the source repo, so a malicious `flake.nix` runs code at build time. `RUSSEL_NIX_RESTRICTED=1` forces the Nix sandbox and refuses auto-generated flakes, but it is a light gate, not isolation for hostile repos: host `nix.conf` (trusted users, builders, substituters) still applies. The full `/nix/store` is visible to workloads. Workloads themselves are not trusted and are confined by the rootless-container or KVM boundary.

In scope, for example:

- reaching the control-plane API without a valid token, or bypassing the cleartext and loopback guards;
- a workload escaping its container or microVM, or affecting another service or the host;
- input validation bypasses (service IDs, paths, env, Podman passthrough arguments, repo URLs and the SSRF guard);
- secrets leaking through the API, logs, metadata, or argv;
- the installer or release artifacts installing something other than what `SHA256SUMS` lists.

Out of scope:

- attacks that need the operator's API token, shell, or sudo, or that rely on the operator deploying a malicious repo (with or without `RUSSEL_NIX_RESTRICTED=1`);
- multi-tenant isolation between mutually untrusted users;
- workloads reading `/nix/store`;
- the known limits listed under "What is still open" in the security overview (for example DNS rebinding around the SSRF guard, or `podman inspect` showing plain, non-secret container env);
- vulnerabilities in Podman, Cloud Hypervisor, Nix, or the Linux kernel themselves. Report those upstream; tell us if Russel's use of them makes things worse.

Details:

- [Security overview](docs/security/overview.md): trust boundaries, open issues, operator rules.
- [Nix build security](docs/security/nix-builds.md): the build threat model and restricted mode.
