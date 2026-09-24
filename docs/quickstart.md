---
title: Quickstart
description: Deploy your first Russel service in about five minutes on an installed host.
sidebar_position: 2
keywords: [quickstart, deploy, russel init, basic-http, health check]
---

# Quickstart

Deploy `examples/basic-http` on the same Linux host that runs `russel-ctrl`. Takes ~5 minutes plus the Nix app build. You use installed binaries throughout — nothing here compiles Russel itself (Russel compiles *your app* with Nix; that is the product working as intended).

Assumes [Installation](getting-started/installation.md) is done: `russel` on PATH, control plane installed and running.

## What you'll need

- The control-plane host from Installation (Topology A, same host), with rootless Podman (`podman info` reports rootless) and a writable `/var/lib/russel`.
- Example apps: your checkout of the public mirror (`git clone https://github.com/daschinmoy21/russel`).
- This quickstart uses **containers**, so KVM is optional.

## 1. Confirm the install

```bash
./contrib/install.sh status   # from your checkout: endpoint, listener, unit, CLI origin
russel origin                 # url, auth source, reachable?
russel ps                     # list workloads (empty is fine)
```

If you have not logged in yet (the host installer generates the token for you):

```bash
russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env
russel origin
```

> **Note:** Loopback without a token runs in dev mode (warns, no auth). The `host` installer always configures a token plus `RUSSEL_REQUIRE_AUTH=1`, so you skip dev mode entirely.

## 2. Allow local example deploys (this host only)

Local paths resolve on the **control-plane host**, and ctrl accepts them only with an explicit opt-in. Same-host quickstart qualifies; a remote ctrl does not (use git URLs there — see [First deploy](getting-started/first-deploy.md)):

```bash
printf 'RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1\n' >> ~/.config/russel/env
systemctl --user restart russel-ctrl
```

## 3. Deploy the example

From your checkout:

```bash
russel deploy examples/basic-http -p 8080:3000 --vm-id test-api --runtime container
```

`examples/basic-http` already sets `type = "container"`. `--runtime` must match `service.type` when both are given — it is a check, not an override.

The CLI streams progress (`resolve → build → create → start → ready → complete`) and exits `0` on `deployed`. The `build` stage is Nix building the example app on the host.

## 4. Verify

```bash
curl http://127.0.0.1:8080/health
# → ok

russel status test-api
russel ps
russel logs test-api
```

Without `-p`, Traefik is the primary HTTP ingress: open `http://test-api.russel.local` once Traefik watches `/var/lib/russel/traefik/dynamic`. See [Traefik ingress](guides/traefik-ingress.md).

## 5. Update and roll back

```bash
russel update test-api
russel deploy examples/basic-http -p 8080:3000 --vm-id test-api --runtime container  # redeploy
```

Each successful deploy appends a versioned row to `/var/lib/russel/test-api/deployments.json` (cap 20). If a redeploy fails after a prior success, Russel attempts automatic rollback and reports `rolled_back` with a non-zero CLI exit so CI notices. Explicit rollback:

```bash
curl -X POST http://127.0.0.1:7878/vm/test-api/rollback -H 'Content-Type: application/json' -d '{}'
```

See [Update and rollback](guides/update-rollback.md).

## 6. Tear down

```bash
russel stop test-api
russel destroy test-api
```

Restarting `russel-ctrl` (`systemctl --user restart russel-ctrl`) does **not** destroy workloads — they keep running detached. Startup reconcile re-adopts live processes and marks the rest `stopped`.

## Your own app next

```bash
cd my-app
russel init --type container   # writes Russelfile.toml
russel init --with-flake       # also writes a starter flake.nix
russel deploy . -p 8080:3000 --vm-id my-app --runtime container
```

`russel init` documents every `Russelfile.toml` field and CLI flag in comments. Full schema: [Russelfile reference](reference/russelfile.md).

## Next steps

- [Installation topologies A/B/C](getting-started/installation.md) — put the client and control plane on different machines.
- [First deploy](getting-started/first-deploy.md) — `init`, env, secrets, `--env-file`, podman passthrough, git-URL deploys.
- [Single-VPS checklist](guides/vps-one-dev.md) — the supported one-operator remote path (containers only).
- [Troubleshooting](guides/troubleshooting.md) — `russel origin`, `install.sh status`, cleartext/401/forward errors.
