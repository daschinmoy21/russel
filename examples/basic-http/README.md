# basic-http

Go stdlib HTTP server with embedded static assets and `GET /health` → `ok`.

| Field | Value |
|-------|--------|
| Runtime (default) | `container` |
| Port | 3000 |
| Binary | `basic-http` |

## Deploy (container — default)

```bash
# terminal 1
./target/debug/russel-ctrl

# terminal 2
./target/debug/russel-cli deploy examples/basic-http -p 8080:3000 --vm-id basic

curl http://127.0.0.1:8080/health   # ok
# Traefik (optional): http://basic.russel.local  after Host routing is set up
```

## Deploy (microVM)

```toml
# temporarily in Russelfile.toml, or use examples/microvm-http
type = "microvm"
```

```bash
sudo -E ./target/debug/russel-ctrl   # TAP/KVM usually needs privileges
./target/debug/russel-cli deploy examples/microvm-http -p 8080:3000 --vm-id basic-vm
```

## Local Nix (no Russel)

```bash
nix build path:examples/basic-http
PORT=3000 ./result/bin/basic-http
```
