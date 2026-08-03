# Examples

Example apps under `examples/` cover **container** (default) and **microVM** deploys,
env/secrets, and a small multi-app tour. Index: [examples/README.md](../examples/README.md).

## Prerequisites

1. Nix with flakes; enter the dev shell:

   ```bash
   nix develop
   cargo build
   ```

2. **Containers (most examples):** rootless Podman (`podman info` → rootless).  
   **MicroVMs:** KVM (`/dev/kvm`), TAP, often `sudo -E ./target/debug/russel-ctrl` so containers still run as `SUDO_USER`.

3. Start the control plane (terminal 1):

   ```bash
   export RUSSEL_API_TOKEN="$(openssl rand -hex 32)"   # optional on loopback; min 32 chars
   ./target/debug/russel-ctrl                  # container-only
   # sudo -E ./target/debug/russel-ctrl        # hybrid microVM + container
   ```

4. Optional Traefik: point `providers.file.directory` at `/var/lib/russel/traefik/dynamic`
   (see [traefik.md](traefik.md)). Then open `http://<vm-id>.russel.local`.

`type` in `Russelfile.toml` is the runtime source of truth. CLI `--runtime` must **match** if set.

---

## 1. basic-http — Go HTTP (default container)

| Field | Value |
|-------|--------|
| Language | Go (stdlib + embed) |
| Port | 3000 |
| Binary | `basic-http` |
| Health | `GET /health` → `ok` |
| Config | `examples/basic-http/Russelfile.toml` (`type = "container"`) |

```bash
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id basic
curl http://127.0.0.1:8080/health
./target/debug/russel-cli status basic
./target/debug/russel-cli destroy basic
```

Hardened containers (`debug = false`): no bash/curl; this app is a static ELF so it is fine.

---

## 2. microvm-http — same app as microVM

| Field | Value |
|-------|--------|
| Runtime | `type = "microvm"` |
| Binary | `basic-http` (self-contained copy of the basic-http Go app) |

```bash
sudo -E ./target/debug/russel-ctrl
./target/debug/russel-cli deploy examples/microvm-http -p 8080:3000 --vm-id basic-vm
curl http://127.0.0.1:8080/health
```

### Runtime switch (dual-live)

Redeploy the **same** service id with the other example (or change `type` and redeploy):

```bash
# was microVM; switch to container (candidate boots, Traefik swap, old gen drained)
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id demo
# later:
./target/debug/russel-cli deploy examples/microvm-http -p 8080:3000 --vm-id demo
```

Prefer Traefik Host names over a fixed `-p` for stable URLs across generations.

---

## 3. hello-rust — pure-std Rust

| Field | Value |
|-------|--------|
| Port | 3000 |
| Binary | `hello-rust` |
| Health | `GET /health` |

```bash
./target/debug/russel-cli deploy examples/hello-rust -p 8083:3000 --vm-id hello
curl http://127.0.0.1:8083/health
curl http://127.0.0.1:8083/
```

---

## 4. env-config — env + secrets

| Field | Value |
|-------|--------|
| Port | 3000 |
| Env | `GREETING`, `LOG_LEVEL`, `DEMO_SECRET=secret://DEMO_SECRET` |

```bash
printf '%s' 's3cret' | ./target/debug/russel-cli secrets set DEMO_SECRET
./target/debug/russel-cli deploy examples/env-config -p 8084:3000 --vm-id envdemo
curl -s http://127.0.0.1:8084/ | jq .
# → greeting, log_level, secret_set=true, secret_len=… (value never returned)
```

---

## 5. shortlink — URL shortener

```bash
./target/debug/russel-cli deploy examples/shortlink -p 8085:3000 --vm-id link
curl -s -X POST --data 'https://example.com' http://127.0.0.1:8085/
# → {"id":"…","url":"https://example.com"}
curl -sI "http://127.0.0.1:8085/<id>"   # 302
```

---

## 6. filebrowser — nixpkgs binary wrapper

| Field | Value |
|-------|--------|
| Port | 8080 |
| Binary | `filebrowser` (shell wrapper; store shebang) |

Uses `/tmp` for data (writable tmpfs under read-only rootfs).

```bash
./target/debug/russel-cli deploy examples/filebrowser -p 8081:8080 --vm-id files
curl -sI http://127.0.0.1:8081/ | head -5
```

---

## 7. static-test — Python static site

| Field | Value |
|-------|--------|
| Port | 8000 |
| Binary | `app` |

```bash
./target/debug/russel-cli deploy examples/static-test -p 8082:8000 --vm-id static
curl http://127.0.0.1:8082/
```

---

## Direct Nix build (no control plane)

```bash
nix build path:examples/basic-http && PORT=3000 ./result/bin/basic-http
nix build path:examples/hello-rust && PORT=3000 ./result/bin/hello-rust
nix build path:examples/shortlink && PORT=3000 ./result/bin/shortlink
nix build path:examples/env-config && PORT=3000 ./result/bin/env-config
```

---

## Tips

| Topic | Guidance |
|--------|----------|
| `debug = true` | Only if entrypoints need bash/`#!/usr/bin/env` |
| Publish bind | Default backend bind is often loopback-friendly; Traefik on host is preferred ingress |
| Secrets | `russel-cli secrets set/list/delete`; never put raw secrets in git |
| Auth | Same `RUSSEL_API_TOKEN` on ctrl and CLI when set |
