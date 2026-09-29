---
title: Quickstart
description: Deploy an example app, update it, roll it back, and remove it, in about five minutes.
sidebar_position: 2
keywords: [quickstart, deploy, apply, russel init, basic-http, health check, rollback]
---

You'll deploy `examples/basic-http`, a small Go web server, as a rootless container on your own server. Then you'll redeploy it, roll it back, and remove it.

## Before you start

- Finish [Installation](./getting-started/installation.md): `russel ps` should work.
- Run these steps **on the server**. The quickstart deploys from a folder on the server. For deploying from a laptop, see [First deploy](./getting-started/first-deploy.md).
- Get the examples into a folder the `russel` account owns. The control plane runs as `russel`, so it can't read your home directory:

  ```bash
  sudo install -d -o russel -g russel /srv/russel-apps
  sudo -u russel git clone https://github.com/daschinmoy21/russel /srv/russel-apps/russel
  cd /srv/russel-apps/russel
  ```

## 1. Allow deploys from a local folder

By default the control plane only builds from git URLs. Deploying from a folder on the server needs a one-line opt-in:

```bash
echo 'RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1' | sudo tee -a /etc/russel/env
sudo systemctl restart russel-ctrl
```

Only do this on a server you alone control.

## 2. Deploy

```bash
russel apply examples/basic-http
```

`russel` sends the folder's path to the control plane, which reads `Russelfile.toml`, builds the app with Nix, and starts it. The first build takes a minute or two while Nix fetches Go; later deploys take about a second.

When it finishes you'll see something like:

```text
  note: container russel-api running. localhost:3100 -> guest:3000

  Test:  curl -I http://127.0.0.1:3100/
```

The service is called `api` because that's the `name` in the example's Russelfile.

## 3. Check it

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

## 4. Redeploy and roll back

Running `apply` again with the same commit and Russelfile does nothing:

```bash
russel apply examples/basic-http
# note: already running 96ef3e713a89 with this Russelfile; nothing to apply
```

Commit a change to the app or edit the Russelfile and `apply` again, or force a redeploy:

```bash
russel apply --force examples/basic-http
```

A redeploy starts the new version next to the old one and switches traffic over once the new one answers, so the app keeps serving. The old one keeps running for 2 more seconds (`switch · New version is live; …`). If the new version fails to start, or crashes within those 2 seconds, traffic stays on (or goes back to) the old one and `apply` exits non-zero. [When a deploy counts as ready](./concepts/lifecycle.md#when-a-deploy-counts-as-ready) has the details.

Go back to the previous version yourself:

```bash
russel rollback api
```

Russel keeps the last 20 deployments. `russel rollback api --version N` picks an older one. More: [Update and rollback](./guides/update-rollback.md).

## 5. Clean up

```bash
russel destroy api
```

`russel stop api` stops the app but keeps its history, so a later `apply` or `rollback` can bring it back.

Restarting the control plane (`sudo systemctl restart russel-ctrl`) doesn't stop your apps.

## Deploy your own app

```bash
cd my-app
russel init        # writes a commented Russelfile.toml
git add Russelfile.toml && git commit -m "Add Russelfile" && git push
russel apply https://github.com/you/my-app.git
```

The server clones the repo itself, so this works the same from your laptop. To deploy a folder that is already on the server instead, keep it somewhere the `russel` account owns, like `/srv/russel-apps`, and run `russel apply /srv/russel-apps/my-app`.

If your project has no `flake.nix`, Russel generates one for Rust (`Cargo.toml`), Go (`go.mod`), and static sites. `russel init --with-flake` writes a starter flake you can edit instead. See [Builds](./concepts/builds.md) and the [Russelfile reference](./reference/russelfile.md).

## Next steps

- [First deploy](./getting-started/first-deploy.md): deploy from git, and add env vars, secrets, and volumes.
- [Single-VPS checklist](./guides/vps-one-dev.md): run Russel on a VPS and reach apps from the internet.
- [Troubleshooting](./guides/troubleshooting.md)
