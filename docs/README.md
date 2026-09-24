---
title: Docs
description: How this docs/ folder maps to the docs website (Mintlify, Docusaurus, Starlight).
sidebar_position: 100
---

# Docs folder

Website-ready, Tailscale/Cloudflare-style. `index.md` is the landing page; `mint.json` is the Mintlify nav; `sidebar.ts` is the Starlight/Docusaurus nav snippet; `_category_.json` files label each section for Docusaurus.

## Consume it

| Target | File |
|---|---|
| Mintlify | `docs/mint.json` (nav) + `*.md` (content) |
| Docusaurus | `*.md` frontmatter (`title`, `description`, `sidebar_position`) + `*/_category_.json` |
| Astro Starlight | Import `docs/sidebar.ts` (`sidebar` export) into `astro.config.mjs` |

Top-level legacy files (`architecture.md`, `deployment.md`, `examples.md`, `install.md`, `russelfile.md`, `security-tls.md`, `traefik.md`, `vps-one-dev.md`, `auto-generation.md`) are **redirect stubs** to the new IA — do not expand them. Live content lives in `getting-started/`, `concepts/`, `guides/`, `reference/`, `security/`, `operations/`.

## Link rules

- New docs link to `getting-started/`, `concepts/`, `guides/`, `reference/`, `security/`, `operations/` only.
- Every page has `title` + `description` frontmatter, a `What you'll need` (or equivalent), copyable `bash` blocks, and a `Related` section.
