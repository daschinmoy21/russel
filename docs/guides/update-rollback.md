---
title: Update and rollback
description: Redeploy from recorded desired state and roll back to prior versions.
sidebar_position: 5
keywords: [update, rollback, redeploy, deployments, desired state]
---

# Update and rollback

Redeploying an existing id kills + waits for the old generation before reusing ports. If the new deploy fails after a prior success, Russel attempts automatic rollback and reports `rolled_back` (CLI still exits non-zero so CI notices).

## Update (re-apply desired state)

`russel update` redeploys from the `repo_url` / `config_path` recorded at the last successful deploy, preserving env/podman args unless overridden:

```bash
russel update <id> [--repo REPO] [--config PATH]
```

Same NDJSON stream, semaphore, and shutdown guards as `deploy`. API: `POST /vm/{id}/update` with optional `{"repo_url":…, "config_path":…}`.

## Deployment history

Each success appends a versioned row to `/var/lib/russel/<id>/deployments.json` (cap 20, newest first). Prior `active` becomes `previous` (one); older `previous` rows become `superseded`.

| Field | Meaning |
|---|---|
| `version` | Monotonic per-service number |
| `status` | `active` / `previous` / `superseded` / `rolled_back` |
| `desired_state` | Snapshot (`repo_url`, `config_path`, `runtime`, `env`, `podman_args`, ports) for rebuilds |
| `rollback_ready` | Whether this row can be a rollback target |

```bash
curl http://127.0.0.1:7878/vm/my-app/deployments
```

## Explicit rollback

```bash
curl -X POST http://127.0.0.1:7878/vm/my-app/rollback \
  -H 'Content-Type: application/json' -d '{}'          # latest previous with rollback_ready
curl -X POST http://127.0.0.1:7878/vm/my-app/rollback \
  -H 'Content-Type: application/json' -d '{"version": 3}'
```

`POST /vm/{id}/rollback` redeploys from history through the normal pipeline (NDJSON stream). No recorded source → `409` with a clear message. Rollback validates the `.bak` metadata **before** restoring directories, re-reserves ports with checked conversions, restores persisted `desired_state` env, re-boots, and only reports `rolled_back` after readiness.

## Zero-downtime note

Live-prior redeploys use dual-live candidates (`<id>_g<gen>` + fresh backend port + `Ingress::swap` cutover, then drain + promote). Instant retain-N=2 cutover (keeping the previous artifact hot without a rebuild) is a follow-up — current rollback re-runs the pipeline.

## Related

- [Lifecycle](../concepts/lifecycle.md) · [API](../reference/api.md) · [Troubleshooting](troubleshooting.md)
