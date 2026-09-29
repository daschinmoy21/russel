# basic-http

Go stdlib HTTP server with embedded static **service dashboard** and
`GET /health` → `ok`. The UI probes `/health` live and documents container vs
microVM deploy paths.

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
./target/debug/russel deploy examples/basic-http

curl http://127.0.0.1:8080/health   # ok
# Traefik (optional): http://api.russel.local  after Host routing is set up
```

## Deploy (microVM)

Use the paired microVM example, whose Russelfile sets `service.name = "microvm-http"`:

```bash
sudo -E ./target/debug/russel-ctrl   # TAP/KVM usually needs privileges
./target/debug/russel deploy examples/microvm-http
```

## Local Nix (no Russel)

```bash
nix build path:examples/basic-http
PORT=3000 ./result/bin/basic-http
```
