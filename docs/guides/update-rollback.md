---
title: Update and rollback
description: Ship new commits with russel update, see past deployments, and go back to an earlier one.
sidebar_position: 5
keywords: [update, rollback, redeploy, deployments, history]
---

Use `russel deploy` once per service. After that, `russel update` ships changes and `russel rollback` goes back.

## Every deployment is pinned to a commit

Each deployment records the commit it was built from. When the working tree was clean, that commit also pins the Russelfile. So:

- Running `russel deploy` again with the commit and Russelfile the service already runs does nothing and reports `unchanged`. Pass `--force` to redeploy anyway.
- `update` (without `--refresh`) rebuilds a recorded commit exactly, even after the branch has moved on.
- A deployment made from a folder with uncommitted changes isn't pinned. Rebuilding it uses whatever the folder holds at that time.

## Update

Ship the latest commit on the repo's default branch:

```bash
russel update my-app --refresh
```

Rebuild the commit that's already running, for example after changing a secret:

```bash
russel update my-app
```

Deploy from a different repo or Russelfile path (this implies `--refresh`). The Russelfile's `service.name` must still be `my-app`:

```bash
russel update my-app --repo https://github.com/you/fork.git --config Russelfile.toml
```

The new version starts next to the old one and traffic switches once it answers. If the new version fails, or crashes within 2 s, the old one keeps serving and `update` exits non-zero. Services with `[[ports]]` or a pinned `[ingress].port` stop first and have a short gap, and keep their ports. [When a deploy counts as ready](../concepts/lifecycle.md#when-a-deploy-counts-as-ready) covers each case.

## See past deployments

Russel keeps the last 20 deployments of each service, newest first:

```bash
TOKEN="$(sed -n 's/^RUSSEL_API_TOKEN=//p' ~/.config/russel/env)"
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7878/vm/my-app/deployments
```

| Field | Meaning |
|---|---|
| `version` | Number that goes up with each deployment of this service |
| `status` | `active` (running now), `previous` (the one before), `superseded` (older), or `rolled_back` |
| `desired_state` | What was deployed: repo, Russelfile path, commit (`rev`), whether the tree was dirty, runtime, env, ports |
| `rollback_ready` | Whether you can roll back to it |

## Roll back

```bash
russel rollback my-app              # the previous deployment
russel rollback my-app --version 3  # a specific one
```

A rollback starts that deployment's recorded build again, with the Russelfile it ran. Nothing is fetched or built, so it works when the repo or the network is gone, and an old commit can't pick up different dependencies. It still goes through the normal checks: the version must answer, and a side-by-side rollback keeps the current version until it does. `secret://` values are read from the secret store as it is at rollback time.

The previous deployment's build is always kept (see [Builds](../concepts/builds.md)). Older builds can be garbage-collected by Nix; rolling back to one of those fails with a message to rebuild instead:

```bash
russel rollback my-app --version 3 --rebuild  # build that deployment's commit from source
```

`--rebuild` builds the recorded commit through the normal pipeline, so it needs the repo and the network and takes as long as a deploy. Deployments recorded before Russel kept builds (v0.1.0 and earlier) are always rebuilt this way. A deployment with neither a recorded build nor a recorded source can't be rolled back, and the command says so.

Russel also rolls back on its own: if an update fails after an earlier version worked, it restores that version, waits for it to answer, and reports `rolled_back`. The CLI exits non-zero so scripts still see the failure.

## Related

- [Lifecycle](../concepts/lifecycle.md) · [CLI reference](../reference/cli.md) · [API reference](../reference/api.md) · [Troubleshooting](./troubleshooting.md)
