# Untrusted Nix builds — threat model & operator guidance

**Issue:** [#197](https://github.com/daschinmoy21/russel-dev/issues/197) (audit H-09)  
**Code:** `crates/ctrl/src/build.rs` (`NixBuilder`)  
**Opt-in gate:** `RUSSEL_NIX_RESTRICTED=1`

This document describes what happens when Russel deploys a git repository (or
local path) that contains a Nix flake — and what that implies for operators who
might one day accept **untrusted** source.

---

## 1. Product trust model (today)

Russel is designed primarily for **trusted operators deploying their own code**
(single-admin / single-tenant). On every deploy the control plane:

1. Clones or opens the source (`git` / local path).
2. Optionally **auto-generates** a `flake.nix` if none is present
   ([docs/auto-generation.md](../auto-generation.md)).
3. Runs `nix build path:<checkout>#packages.<system>.default` (with a
   `defaultPackage.<system>` fallback) as the **same user** that runs
   `russel-ctrl` / the Nix daemon client.

There is **no** per-tenant build isolation, no flake-input allowlist, and no
mandatory resource caps beyond what the host Nix daemon already enforces.

| Deployment style | Residual risk | Recommendation |
|------------------|---------------|----------------|
| **Single-tenant** — you deploy repos you control | Medium: a bug or compromised dependency in *your* flake still runs as the build user | Acceptable for MVP/self-host; enable restricted mode + host sandbox (below) |
| **Multi-tenant** — third parties can trigger deploys of arbitrary git URLs | **High** — treated as a supply-chain RCE against the build host | **Do not** expose deploy of untrusted repos until isolation is productized; only deploy **trusted** repositories |

**Bottom line for multi-tenant:** only allow deploys of repositories you trust
(private org repos, signed/reviewed flakes). Arbitrary public git URLs from
untrusted tenants are out of scope for a safe multi-tenant product today.

---

## 2. Threat model

### Assets

- Host credentials and secrets available to the ctrl / Nix user (`RUSSEL_API_TOKEN`,
  `/var/lib/russel/secrets`, SSH keys, cloud metadata if reachable).
- Host filesystem writability of the build user and of any paths Nix is allowed
  to use (store, build dirs, `/tmp`).
- Network egress from evaluation and from builders (fetch fixed-output
  derivations, `builtins.fetch*`, flake inputs).
- Shared Nix store contents visible to later tenants / guests (see virtiofs
  share of `/nix/store` on the microVM path).
- CPU, memory, and disk (build DoS).

### Attack surface

| Surface | How it is reached | Typical impact |
|---------|-------------------|----------------|
| Malicious `flake.nix` / `flake.lock` | Deploy of attacker-controlled repo | Arbitrary Nix evaluation; fixed-output fetches; builder scripts |
| Auto-generated flake | Missing `flake.nix` → Russel writes a template pinning **`nixos-unstable`** | Moving input (supply-chain drift); still runs full `nix build` |
| Flake inputs / substituters | Evaluation + binary cache | Dependency substitution, cache poisoning if caches are untrusted |
| Unsandboxed builders | Host `nix.conf` with `sandbox = false` or `sandbox-fallback = true` | Builder can see more of the host than intended |
| `trusted-users` | Users listed as trusted can pass restricted options and often bypass sandbox policy | Privilege escalation relative to untrusted Nix clients |
| Remote `builders` | Derivations shipped to other machines | Those builders must be as trusted as the ctrl host |
| Shared store → guest | microVM virtiofs of `/nix/store` | Other tenants’ store paths may be readable in-guest (related: #194) |

### What the Nix sandbox does *and does not* do

When builders run with **`sandbox = true`** (Nix default on modern NixOS;
Linux only):

- Build scripts run in a mount/user/PID namespace with a filtered view of the
  filesystem and (usually) no ambient network except for fixed-output
  derivations that declare it.
- This reduces the blast radius of a malicious `buildPhase` / `installPhase`.

The sandbox **does not**:

- Stop malicious **evaluation** (pure Nix code before the build starts), including
  many `builtins.fetchTarball` / flake input resolutions under default settings.
- Replace multi-tenant policy (who may deploy what).
- Protect against a compromised **Nix daemon** or a user in `trusted-users`.
- Bound wall-clock CPU or store growth by itself (use cgroups / Nix
  `max-jobs` / disk quotas).

Upstream references:

- [Nix manual — build sandbox](https://nix.dev/manual/nix/stable/command-ref/conf-file.html#conf-sandbox)
- [Nix manual — `trusted-users`](https://nix.dev/manual/nix/stable/command-ref/conf-file.html#conf-trusted-users)
- [Nix manual — `builders`](https://nix.dev/manual/nix/stable/command-ref/conf-file.html#conf-builders)
- [NixOS Wiki — Builders / remote builds](https://wiki.nixos.org/wiki/Distributed_build)

---

## 3. Operator guidance

### Single-tenant / self-hosted MVP

Acceptable residual risk if you only deploy **your** code:

1. Run `russel-ctrl` as a dedicated unprivileged user when possible; avoid
   building as root.
2. Keep host Nix policy strict (see §4).
3. Prefer committed `flake.nix` + `flake.lock` with **pinned** inputs (not floating
   `nixos-unstable` without a lock).
4. Enable Russel restricted mode (below) on production hosts.
5. Do not put untrusted Unix users in `trusted-users`.

### Multi-tenant (or untrusted git deploys)

Until stronger isolation lands (dedicated build users/VMs, input allowlists,
resource quotas, optional remote builder farm):

1. **Only deploy trusted repositories** (allowlist orgs/hosts; no open
   “deploy any URL” API for tenants).
2. Treat a malicious flake as **host compromise of the build user**.
3. Do not share one ctrl + one Nix store across mutually distrusting tenants
   without additional isolation.
4. Plan for remote builders / separate build hosts that never hold production
   secrets.

### Auto-generation

Auto-flakes pin `github:NixOS/nixpkgs/nixos-unstable` without a lockfile at
generation time. That is convenient for demos and **unsafe as a multi-tenant
default**. Restricted mode disables auto-generation entirely.

---

## 4. Host Nix configuration (sandbox, builders, trusted-users)

Recommended baseline on the host that runs deploy builds
(`/etc/nix/nix.conf` or NixOS `nix.settings`):

```ini
# Build isolation (Linux)
sandbox = true
sandbox-fallback = false

# Who may change restricted settings / bypass some checks
# Keep this list short. The user running russel-ctrl should NOT need to be
# trusted if the daemon already enforces sandbox for untrusted clients.
# trusted-users = root

# Prefer explicit remote builders over ad-hoc machines
# builders = ssh://builder@build-host?cores=8
# require-sigs = true

# Optional: limit parallelism to reduce noisy-neighbour DoS
max-jobs = auto
cores = 0
```

Notes:

| Setting | Why it matters for Russel |
|---------|---------------------------|
| `sandbox` | Confines builder processes; pair with `sandbox-fallback = false` so builds fail closed instead of silently running unsandboxed |
| `trusted-users` | Trusted clients can pass many `--option` flags and are a privilege boundary; do not add tenant-facing accounts |
| `builders` / `builders-use-substitutes` | Remote builders execute the same untrusted derivations — harden and trust them like the ctrl host |
| Substituters / `trusted-public-keys` | Only use caches whose keys you trust; a malicious cache is a supply-chain vector |

Verify at runtime:

```bash
nix show-config | egrep '^(sandbox|sandbox-fallback|trusted-users|builders) '
```

---

## 5. Russel opt-in: `RUSSEL_NIX_RESTRICTED=1`

Light control-plane gate (not a full multi-tenant sandbox product):

```bash
export RUSSEL_NIX_RESTRICTED=1   # also accepts true / yes
./target/debug/russel-ctrl
```

When set, `NixBuilder`:

1. **Refuses auto-generated flakes** — deploy fails if the checkout has no
   `flake.nix` (error points here).
2. Passes to every deploy `nix build`:
   - `--option sandbox true`
   - `--option sandbox-fallback false`

Truthiness: `1`, `true`, `yes` (case-insensitive, trimmed). Unset or any other
value leaves the previous behaviour (auto-flake + ambient host Nix policy).

**Limitations of this flag:**

- Does not implement flake-input allowlists, pure-eval, or network isolation
  for evaluation.
- Host `trusted-users` can still override options.
- Does not move builds to another user or machine.
- Does not add cgroup/disk quotas by itself.

For production single-tenant hosts, set the env **and** fix host `nix.conf` as
in §4 so non-Russel `nix build` invocations are equally constrained.

---

## 6. Related work / future hardening

Tracked under the security epic (#185) and audit notes:

| Direction | Notes |
|-----------|--------|
| Dedicated unprivileged build user / Nix daemon split | Builds never share the API process credentials |
| Remote builder farm + binary cache | Scale + isolate (# horizontal build scale) |
| Flake lock required / input allowlists | Block surprise github: inputs |
| Resource limits (cgroup, `min-free`, store GC policy) | Mitigate DoS |
| Guest store share narrowing (#194) | Reduce cross-tenant store visibility after build |

---

## 7. Checklist (operators)

- [ ] Only trusted repos may be deployed on this ctrl (process or technical allowlist).
- [ ] `sandbox = true` and `sandbox-fallback = false` on the build host.
- [ ] `trusted-users` reviewed; no untrusted accounts.
- [ ] Remote `builders` (if any) are trusted and patched.
- [ ] Production: `RUSSEL_NIX_RESTRICTED=1`.
- [ ] Production apps ship pinned `flake.nix` + `flake.lock` (no relying on auto-flake).
- [ ] Secrets for production not mounted into the build environment unless required.

See also: [deployment.md](../deployment.md) (validation), [architecture.md](../architecture.md)
(pipeline), [auto-generation.md](../auto-generation.md) (generated flakes).
