---
title: Update and rollback
description: Re-apply the Russelfile from the recorded source and roll back to prior versions.
sidebar_position: 5
keywords: [update, rollback, redeploy, deployments, desired state]
---

# Update and rollback

Redeploying an existing id kills + waits for the old generation before reusing ports. If the new deploy fails after a prior success, Russel attempts automatic rollback and reports `rolled_back` (CLI still exits non-zero so CI notices).

## Generations are pinned to commits

Every deploy records the commit it built (`rev`) and whether the deployed tree had uncommitted changes (`dirty`). A clean tree means the Russelfile is committed too, so the commit pins it. That makes deploys work like Nix generations:

- `russel apply` (alias `deploy`) of the commit and Russelfile a running service already has returns `unchanged` without rebuilding or restarting. Pass `--force` to redeploy anyway. A dirty tree or a source outside git always deploys.
- `update` and `rollback` rebuild a clean generation's recorded commit exactly, even after the branch has moved. A dirty generation builds what its source holds now. A local path is cloned for this, so your working tree is never touched.

## Update (redeploy the recorded commit)

`russel update` redeploys the running generation's `repo_url`, `config_path`, and commit. Secrets are resolved again, so this is how to restart with a rotated secret. `--refresh` builds the source's current commit instead, and so does passing `--repo` or `--config`. The file's `service.name` must equal `<id>`:

```bash
russel update <id> [--refresh] [--repo REPO] [--config PATH]
```

Same NDJSON stream, semaphore, and shutdown guards as `apply`. API: `POST /vm/{id}/update` with optional `{"repo_url":…, "config_path":…, "refresh":true}`.

## Deployment history

Each success appends a versioned row to `/var/lib/russel/<id>/deployments.json` (cap 20, newest first). Prior `active` becomes `previous` (one); older `previous` rows become `superseded`.

| Field | Meaning |
|---|---|
| `version` | Monotonic per-service number |
| `status` | `active` / `previous` / `superseded` / `rolled_back` |
| `desired_state` | What the deploy resolved (`repo_url`, `config_path`, `rev`, `dirty`, `runtime`, `env`, `podman_args`, ports). Rebuilds reuse only `repo_url`, `config_path`, and `rev` |
| `rollback_ready` | Whether this row can be a rollback target |

```bash
curl http://127.0.0.1:7878/vm/my-app/deployments
```

## Explicit rollback

```bash
russel rollback my-app               # latest previous with rollback_ready
russel rollback my-app --version 3
```

or through the API:

```bash
curl -X POST http://127.0.0.1:7878/vm/my-app/rollback \
  -H 'Content-Type: application/json' -d '{}'          # latest previous with rollback_ready
curl -X POST http://127.0.0.1:7878/vm/my-app/rollback \
  -H 'Content-Type: application/json' -d '{"version": 3}'
```

`POST /vm/{id}/rollback` redeploys the target row's `repo_url`, `config_path`, and `rev` through the normal pipeline (NDJSON stream), so it builds the target generation's code even if the branch has moved. A row marked `dirty` is not pinned (its commit may not hold the Russelfile or the changes it ran with) and builds what its source holds now, as do `update` and health restarts of a dirty generation. A row from before commits were recorded has no `rev` and builds what its source points to now. No recorded source → `409` with a clear message. Rollback validates the `.bak` metadata **before** restoring directories, re-reserves ports with checked conversions, restores persisted `desired_state` env, re-boots, and only reports `rolled_back` after readiness.

## Zero-downtime note

Live-prior redeploys use dual-live candidates (`<id>_g<gen>` + fresh backend port + `Ingress::swap` cutover, then drain + promote). Instant retain-N=2 cutover (keeping the previous artifact hot without a rebuild) is a follow-up — current rollback re-runs the pipeline.

## Related

- [Lifecycle](../concepts/lifecycle.md) · [API](../reference/api.md) · [Troubleshooting](troubleshooting.md)
