---
title: "Decision: what comes after v0.1"
description: The order after v0.1, the first sandbox host and workflow, and where the lifecycle fixes fit.
keywords: [roadmap, decision, sandboxes, microvm, gvisor, v0.2]
---

Decided **2026-10-02**. Sandbox tracker: #71.

## Decision

1. **Order:** v0.1 (#56), then agent sandboxes (#71), then v0.2 production readiness (#83).
2. **Lifecycle fixes come first.** The issues from the 2026-10-01 audit (russel-dev#554 to russel-dev#562) are v0.1 blockers, so sandbox work starts after they land. Sandboxes reuse deploy, rollback, health and resource accounting, and a sandbox quota needs effective limits to build on (russel-dev#561).
3. **First target host:** a machine with `/dev/kvm`, running the Cloud Hypervisor microVM backend. It already runs unprivileged with passt, virtiofs, args, ports, volumes and a read-only root (#67).
4. **Hosts without KVM**: gVisor `runsc --platform=systrap` or PVM, picked by the Phase 0 spikes in #71. Until then the VPS runs services only. Rootless Podman is the trusted profile only, never a silent fallback.
5. **First workflow: agent outside, tools inside.** The agent loop runs where the user already runs it. The sandbox exposes exec, files, processes and ports through ctrl's API, and an MCP server wraps that API. Provider keys never enter the guest. Running an agent kit inside the guest comes next, on top of the same API plus the egress credential proxy.
6. **Single node first.** The plan's multi-node prerequisites (node extraction, enrollment, leases, fencing) move to later.

## Why

- The microVM backend works today. gVisor would start from zero, and it would ship first on the host where sandboxes matter least to the user.
- The lifecycle bugs from the audit sit on paths a sandbox uses constantly: cold replace, health claims, rollback, resource records. Fixing them once in v0.1 is cheaper than working around them in a new API.
- Agent outside first keeps credentials out of the guest from day one and needs no new protocol beyond the sandbox API.

## Related

- [v0.1 status](../features/v0.1.md) · #56 · #71 · #83
