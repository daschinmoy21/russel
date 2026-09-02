# Install

Two binaries:

| Binary | Where it runs | Job |
|--------|---------------|-----|
| `russel` | Your laptop (or any client) | Deploy, `ps`, logs, login |
| `russel-ctrl` | The Linux host that runs workloads | Build + boot microVMs / containers |

The crate is still `russel-cli`. The **command** is `russel`. Control-plane ops stay `russel-ctrl`.

Build each binary on the OS that will run it. A `russel-ctrl` linked against NixOS glibc will not start on Debian (`required file not found`). Build ctrl on the VPS, or copy from a matching distro.

## 1. Build

```bash
git clone https://github.com/daschinmoy21/russel.git
cd russel
nix develop          # or a Rust toolchain + pkg-config + openssl
cargo build --release -p russel-cli -p russel-ctrl
# → target/release/russel
# → target/release/russel-ctrl
```

Helper (from repo root, after a release build):

```bash
./contrib/install.sh cli    # ~/.local/bin/russel
./contrib/install.sh ctrl   # /usr/local/bin/russel-ctrl (sudo if needed)
```

## 2. Laptop: `russel`

```bash
install -Dm755 target/release/russel ~/.local/bin/russel
# fish/bash: ~/.local/bin must be on PATH
hash -r 2>/dev/null || true
russel --help
```

Point it at a control plane and store the token (file mode 0600):

```bash
# SSH tunnel if ctrl is loopback-only on a remote host:
ssh -f -N -L 7878:127.0.0.1:7878 user@host

russel login http://127.0.0.1:7878 --token-file ~/.config/russel/token.env
russel origin
russel ps
```

`russel login` writes `~/.config/russel/config.toml`. Fish does not load `KEY=VALUE` files; do not `source` an env file. Env vars still win over the config file if you set them.

## 3. Server: `russel-ctrl`

Needs Nix (flakes) and, for containers, rootless Podman. State dir `/var/lib/russel` (mode 0700, owned by the service user).

```bash
# on the server, same git revision as the CLI
cargo build --release -p russel-ctrl
sudo install -Dm755 target/release/russel-ctrl /usr/local/bin/russel-ctrl
sudo mkdir -p /var/lib/russel
sudo chown "$USER:" /var/lib/russel
sudo chmod 700 /var/lib/russel
```

Token + bind (do not expose `:7878`):

```bash
umask 077
mkdir -p ~/.config/russel
printf 'RUSSEL_API_TOKEN=%s\n' "$(openssl rand -hex 32)" > ~/.config/russel/env
# optional: RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1
# optional: RUSSEL_PUBLISH_BIND=100.x.x.x   # Tailscale, or 0.0.0.0 for public -p
```

**NixOS host.** Import `nixosModules.russel`, set `bin` or `package` and `environmentFile`:

```nix
{
  services.russel.enable = true;
  services.russel.bin = "/usr/local/bin/russel-ctrl";
  services.russel.environmentFile = "/etc/russel/env"; # 0600, RUSSEL_API_TOKEN=
}
```

**Other Linux.** Copy [contrib/russel-ctrl.service](../contrib/russel-ctrl.service) to `~/.config/systemd/user/` and follow the comments in that file (`systemctl --user enable --now russel-ctrl`).

Ctrl is HTTP-only. Terminate TLS in front of `127.0.0.1:7878` ([security-tls.md](security-tls.md)) or use an SSH tunnel from the laptop.

## 4. Check

On the laptop:

```bash
russel origin     # url, auth, reachable
russel ps         # list workloads (aliases: list, vms)
```

Typical VPS has no KVM. Set `type = "container"` in every Russelfile (`russel init --type container`). Full operator checklist: [vps-one-dev.md](vps-one-dev.md).
