# Single-VPS, one-developer deploy checklist

**Audience:** one trusted operator running `russel-ctrl` on a single Linux VPS
(or home server), deploying with `russel-cli` from a laptop.

**Scope:** container runtime on typical no-KVM VPS images. MicroVMs need
`/dev/kvm` and are optional on bare metal / nested virt only.

**Related:** [TLS reverse-proxy runbook](security-tls.md) · [Traefik app ingress](traefik.md) ·
[app packaging](deployment.md)

---

## Readiness summary

| Area | Ready for one-dev VPS? | Notes |
|------|------------------------|--------|
| Core loop (deploy / status / logs / stop / destroy) | **Yes** | CLI → HTTP API → Nix build → rootless Podman |
| Auth on remote bind | **Yes** | `RUSSEL_API_TOKEN` ≥32 ASCII chars; non-loopback refuses without token |
| TLS in ctrl binary | **No** | Terminate TLS at Caddy/nginx/Traefik ([security-tls.md](security-tls.md)) |
| Typical cheap VPS (no KVM) | **Containers only** | Set `type = "container"` in every `Russelfile.toml` |
| Install package / systemd unit | **Yes** | NixOS: `nixosModules.russel`. Others: `contrib/russel-ctrl.service` |
| Multi-tenant / multi-user | **No** | Single trusted operator model |
| Managed databases | **No** | `[database.*]` is a placeholder |
| Dashboard | **Yes (DIY)** | Static or dev; set API URL + token; prefer same-origin proxy |

**Bottom line:** ready as a **hands-on self-host** for your own apps. Not a
turnkey PaaS or multi-tenant product.

---

## Operator checklist

Copy this into your runbook and tick as you go.

### A. Host prerequisites

- [ ] Linux x86_64 or aarch64 (match app flake `system`)
- [ ] Nix with flakes enabled
- [ ] Rootless Podman: `podman info` reports rootless
- [ ] `loginctl enable-linger $USER` (headless hosts so user runtime persists)
- [ ] Writable state dir: `sudo mkdir -p /var/lib/russel && sudo chown "$USER:" /var/lib/russel`
- [ ] Firewall: allow SSH + 443 (and 80 for ACME); **do not** expose `:7878` publicly
- [ ] Disk free for Nix store (cold builds need room; 20+ GiB comfortable)
- [ ] **No KVM path:** plan container-only (skip microVM deps)
- [ ] **KVM path (optional):** `/dev/kvm`, TAP, `cloud-hypervisor`, `virtiofsd`, `socat`, `ip`, `iptables`

### B. Build and install ctrl + CLI

This is the install story. Pick one host path, then put the CLI on the laptop.

- [ ] Clone this repo on the server (or a CI artifact you trust)
- [ ] `nix develop` then `cargo build --release` (or equivalent toolchain)
- [ ] `russel-ctrl` and `russel-cli` binaries exist (`target/release/…` or a package)
- [ ] NixOS. Import `nixosModules.russel` (flake output `nixosModules.default`). Set `services.russel.enable`, `environmentFile` (0600, `RUSSEL_API_TOKEN=`), and `bin` or `package`. Defaults already bind `127.0.0.1:7878`, set `RUSSEL_REQUIRE_AUTH=1`, use `/var/lib/russel` mode 0700, leave the warm pool off, put `/run/wrappers` on PATH, enable `virtualisation.podman`, set linger on the service user, and set `rootlessPodman = true` (`NoNewPrivileges` off for `newuidmap`). For an existing login user set `user`, `group` (or omit `group` to use the primary group), and `createUser = false`. `rootlessPodman = false` only keeps NNP on; it does not configure microVMs. See [nix/modules/russel-host.nix](../nix/modules/russel-host.nix).
- [ ] Non-NixOS. Install [contrib/russel-ctrl.service](../contrib/russel-ctrl.service) as a systemd user unit (comments in the file). Same env defaults.
- [ ] Container-only VPS: skip KVM, TAP, and cloud-hypervisor. Use a lingering user with rootless Podman. MicroVMs stay optional on bare metal / nested virt.
- [ ] (Optional) Install CLI on the laptop the same way or copy the binary

### C. Secure control plane

- [ ] Generate token: `export RUSSEL_API_TOKEN="$(openssl rand -hex 32)"` and store it offline (or write it into the 0600 env file the unit/module loads)
- [ ] Bind loopback: `127.0.0.1:7878` (module/unit default `RUSSEL_CTRL_ADDR`)
- [ ] Fail-closed auth: `RUSSEL_REQUIRE_AUTH=1` (module/unit default)
- [ ] Reverse proxy TLS → `127.0.0.1:7878` (Caddy/nginx — see [security-tls.md](security-tls.md); app HTTP: [traefik.md](traefik.md))
- [ ] Clients use `https://…` for `RUSSEL_CONTROL_PLANE` (CLI refuses cleartext Bearer to non-loopback unless `--insecure`)
- [ ] Process supervised by the NixOS module or `contrib/russel-ctrl.service`

### D. App reachability

Pick one:

- [ ] **Publish public ports:** `export RUSSEL_PUBLISH_BIND=0.0.0.0` and open chosen host ports, **or**
- [ ] **Traefik (preferred for HTTP):** Traefik watches `/var/lib/russel/traefik/dynamic`; apps on Host rules ([traefik.md](traefik.md)); keep publish bind loopback

### E. First deploy from laptop

- [ ] Same `RUSSEL_API_TOKEN` on laptop and server
- [ ] `export RUSSEL_CONTROL_PLANE=https://russel.example.com`
- [ ] App `Russelfile.toml` has `type = "container"` (default runtime is **microvm**)
- [ ] Deploy from a **git URL** (HTTPS/SSH), not a laptop filesystem path
- [ ] Avoid reserved service IDs: `secrets`, `traefik`, `_pool`, and similar host dirs
- [ ] Smoke:

```bash
russel-cli vms
russel-cli deploy https://github.com/YOU/APP.git \
  --vm-id demo -p 8080:3000 --runtime container
russel-cli status demo
russel-cli logs demo
# curl health on published port or via Traefik Host rule
russel-cli destroy demo
```

### F. Dashboard (optional)

- [ ] Build: `cd dashboard && bun install && bun run build` (or `npm`)
- [ ] Serve `dashboard/dist` behind the same TLS host **or** run dev against HTTPS API
- [ ] Settings: API URL = public HTTPS control plane; paste token (sessionStorage — do not bake `PUBLIC_RUSSEL_API_TOKEN`)
- [ ] CORS: ctrl has **no** CORS; use same-origin reverse proxy for `/api` if the browser origin differs

### G. Ops hygiene

- [ ] Local path deploys stay **off** on remote ctrl (`RUSSEL_ALLOW_LOCAL_PATH_DEPLOY` unset)
- [ ] Prefer git deploys; never put laptop paths in deploy requests to a remote server
- [ ] Secrets via `russel-cli secrets set` + `secret://NAME` (files under `/var/lib/russel/secrets/` mode `0600`)
- [ ] Expect slow **first** Nix builds on small VPS; later deploys hit the store cache
- [ ] Backup `/var/lib/russel` if you care about metadata, secrets, and deployment history

---

## Minimal server commands

Prefer the module or user unit above. Manual equivalent:

```bash
# After Nix + rootless Podman + /var/lib/russel are ready:
export RUSSEL_API_TOKEN="$(openssl rand -hex 32)"
echo "Save this token: $RUSSEL_API_TOKEN"
export RUSSEL_CTRL_ADDR=127.0.0.1:7878
export RUSSEL_REQUIRE_AUTH=1
# Optional: public host ports for -p
# export RUSSEL_PUBLISH_BIND=0.0.0.0

./target/release/russel-ctrl
# Put Caddy/nginx TLS in front of 127.0.0.1:7878
```

## Minimal laptop commands

```bash
export RUSSEL_CONTROL_PLANE=https://russel.example.com
export RUSSEL_API_TOKEN='…same as server…'

russel-cli vms
russel-cli deploy https://github.com/you/app.git \
  --vm-id app -p 8080:3000 --runtime container
```

SSH tunnel alternative (no public TLS yet):

```bash
ssh -L 7878:127.0.0.1:7878 user@vps
export RUSSEL_CONTROL_PLANE=http://127.0.0.1:7878
export RUSSEL_API_TOKEN='…'
russel-cli vms
```

---

## Code / product checklist (maintainers)

What must be true in the tree for the operator path above. Tick when verifying a release.

### P0 for one-dev remote use

- [x] Reserved `service_id` rejected on deploy/destroy (#186)
- [x] Non-loopback bind requires `RUSSEL_API_TOKEN`
- [x] Min token length ≥32 + header-safe charset (#190)
- [x] `RUSSEL_REQUIRE_AUTH` fail-closed on loopback
- [x] CLI refuses cleartext Bearer to non-loopback (#189); TLS proxy docs
- [x] Local absolute path deploy gated (`RUSSEL_ALLOW_LOCAL_PATH_DEPLOY`) (#196)
- [x] Container path (rootless Podman) works without KVM
- [x] Secrets API + host store
- [x] Dashboard does not bake public API token (#198); default bind localhost (#188)
- [x] Operator docs: this file + README “Remote VPS” section
- [x] NixOS module + systemd user unit with loopback / token / 0700 defaults (#207)

### Still DIY / later

- [ ] deb/OCI packages
- [ ] `russel login` / config file for URL + token
- [ ] Native TLS in `russel-ctrl` (proxy is enough for one-dev)
- [ ] Fresh VPS e2e automated in CI
- [ ] Multi-tenant isolation, horizontal scale, managed DBs

---

## Example services for VPS smoke tests

Use examples that set `type = "container"`:

| Example | Notes |
|---------|--------|
| `examples/basic-http` | Go `/health` + static assets |
| `examples/hello-rust` | Minimal Rust HTTP |
| `examples/static-test` | Static files |
| `examples/shortlink` | In-memory shortener |
| `examples/env-config` | Env + `secret://` |
| `examples/microvm-http` | **microVM only** — needs KVM |

Deploy a public fork/clone of the repo (or your app) by **git URL** so the
control plane can clone on the server.

---

*Security backlog beyond this checklist still applies for production hardening;
it is not required to prove CLI → remote ctrl → container deploy for a single
trusted developer.*
