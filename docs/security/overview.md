---
title: Security overview
description: Who Russel trusts, what protects the server and the apps, what's still open, and the rules for running it safely.
sidebar_position: 1
keywords: [security, threat model, auth, token, ssrf, secrets, isolation]
---

Russel is built for **one trusted operator on one server**. Anyone with the API token can deploy code that runs on the server, so the token is as sensitive as an SSH key. Russel doesn't separate users or teams from each other.

## What protects what

| Area | Protection |
|---|---|
| The server | The control plane and every app run as the unprivileged `russel` account. Nothing runs as root after install. An app that escaped its sandbox would land in that account, with no `sudo`, no SSH keys, and none of your files. |
| The API | Every request needs the token: at least 32 characters, compared in constant time. The control plane refuses to listen on a public address without one. |
| The connection | The control plane speaks plain HTTP on `127.0.0.1` only. Reach it through an SSH tunnel or an HTTPS proxy. The CLI and dashboard refuse to send the token over plain HTTP to another host. |
| Apps | Containers are rootless, drop all Linux capabilities, can't gain privileges, and have a read-only root. MicroVMs (experimental) add a separate kernel. |
| Secrets | Stored with mode `0600` in a `0700` folder, never returned by the API, and never put on a command line. Containers receive them as Podman secrets. |
| Input | Service names, paths, env vars, secret names, and Podman flags are checked before use. The Russelfile must be a real file in the repo, under 1 MiB. |
| Git URLs | Only `https://`, `http://`, `ssh://`, and `git@` URLs. Hosts that are, or resolve to, private, loopback, link-local, or cloud-metadata addresses are refused, and HTTP redirects are not followed. |

## Known limits

- **Builds run repo code.** A Nix build runs code from the repo as the `russel` account. Deploy only repos you trust, or see [Nix build security](./nix-builds.md).
- **DNS rebinding.** Russel resolves a git host and checks the address before cloning, but `git` resolves it again. A malicious DNS server could answer differently the second time.
- **Plain env vars are visible** to `podman inspect` as the `russel` account. Use `secret://` for anything sensitive.
- **No built-in HTTPS** in the control plane. Use a proxy or the tunnel.
- **No isolation between users or teams.** Everyone with the token has full control.

## Rules for running it

1. Keep the control plane on `127.0.0.1:7878`, and block 7878 in the firewall.
2. Keep `/etc/russel/env` private: the installer makes it `root:russel`, mode `0640`. Anyone in the `russel` group can read the token, so add only people you trust with the server.
3. Reach the API through the SSH tunnel or an HTTPS proxy only.
4. Deploy from git URLs. Leave `RUSSEL_ALLOW_LOCAL_PATH_DEPLOY` off unless you are the server's only user.
5. Put every password and key behind `secret://`.
6. Use containers on servers without KVM. Treat microVMs as experimental.

To report a vulnerability, see `SECURITY.md` in the repo.

## Related

- [Nix build security](./nix-builds.md) · [TLS reverse proxy](../guides/tls-reverse-proxy.md) · [Env and secrets](../guides/env-secrets.md) · [Environment reference](../reference/environment.md)
