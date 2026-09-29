---
title: Quickstart
description: Deploy an example app, update it, roll it back, and remove it, in about five minutes.
sidebar_position: 2
keywords: [quickstart, deploy, apply, russel init, basic-http, health check, rollback]
---

You'll deploy `examples/basic-http`, a small Go web server, as a rootless container on your own server. Then you'll redeploy it, roll it back, and remove it.

## Before you start

- Finish [Installation](./getting-started/installation.md): `russel ps` should work, from the server or from your laptop.

## 1. Deploy

```bash
russel deploy https://github.com/daschinmoy21/russel.git --config examples/basic-http/Russelfile.toml
```

The control plane clones the repo, reads the Russelfile that `--config` points at, builds the folder it sits in (`examples/basic-http`) with Nix, and starts it. Nothing is cloned on your side, so this works the same from a laptop. The first build takes a minute or two while Nix fetches Go; later deploys take about a second.

When it finishes you'll see something like:

```text
  note: container russel-api running. localhost:3100 -> guest:3000

  Test:  curl -I http://127.0.0.1:3100/
```

The service is called `api` because that's the `name` in the example's Russelfile.

## 2. Check it

```bash
russel ps
```

```text
  ID   RUNTIME    STATUS    STATE    PORTS      UPTIME
  api  container  deployed  running  3100→3000  12s
```

`PORTS` shows host port → app port. Call the app on the host port:

```bash
curl http://127.0.0.1:3100/health
# → ok
russel logs api
```

> **Note:** Russel picks a free host port (3100 and up) unless the Russelfile pins one. It can change on the next deploy, so use `russel ps` to find it. To pin it, add `[ingress]` with `port = 8080` to the Russelfile. For a stable name instead of a port, see [Traefik ingress](./guides/traefik-ingress.md).

## 3. Update and roll back

After the first deploy, ship changes with `russel update`. Commit a change to the app or the Russelfile, then build the source's latest commit:

```bash
russel update api --refresh
```

Without `--refresh`, `update` rebuilds the commit that is already running, which is how you pick up a changed secret. Running `deploy` again with the same commit and Russelfile does nothing.

A redeploy starts the new version next to the old one and switches traffic over once the new one answers, so the app keeps serving. The old one keeps running for 2 more seconds (`switch · New version is live; …`). If the new version fails to start, or crashes within those 2 seconds, traffic stays on (or goes back to) the old one and `update` exits non-zero. [When a deploy counts as ready](./concepts/lifecycle.md#when-a-deploy-counts-as-ready) has the details.

Go back to the previous version yourself:

```bash
russel rollback api
```

Russel keeps the last 20 deployments. `russel rollback api --version N` picks an older one. More: [Update and rollback](./guides/update-rollback.md).

## 4. Clean up

```bash
russel destroy api
```

`russel stop api` stops the app but keeps its history, so a later `update` or `rollback` can bring it back.

Restarting the control plane (`sudo systemctl restart russel-ctrl`) doesn't stop your apps.

## Deploy your own app

```bash
cd my-app
russel init        # writes a commented Russelfile.toml
git add Russelfile.toml && git commit -m "Add Russelfile" && git push
russel deploy https://github.com/you/my-app.git
```

The server clones the repo itself, so this works the same from your laptop. To deploy a folder that is already on the server instead, keep it somewhere the `russel` account owns, like `/srv/russel-apps`, and run `russel deploy /srv/russel-apps/my-app`. After that, `russel update my-app --refresh` ships each new commit.

If your project has no `flake.nix`, Russel generates one for Rust (`Cargo.toml`), Go (`go.mod`), and static sites. `russel init --with-flake` writes a starter flake you can edit instead. See [Builds](./concepts/builds.md) and the [Russelfile reference](./reference/russelfile.md).

## Next steps

- [First deploy](./getting-started/first-deploy.md): deploy from git, and add env vars, secrets, and volumes.
- [Single-VPS checklist](./guides/vps-one-dev.md): run Russel on a VPS and reach apps from the internet.
- [Troubleshooting](./guides/troubleshooting.md)
