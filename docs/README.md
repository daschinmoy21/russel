---
title: Docs
description: How this docs/ folder becomes the docs website (Mintlify), and the rules that keep it building.
---

# Docs folder

This folder is the docs website. [Mintlify](https://mintlify.com) builds it straight from `docs/` on every push to `main`, so a merged change is live a minute or two later. Nothing else deploys it.

| File | Role |
|---|---|
| `docs.json` | Site config: name, colors, the sidebar (`navigation`), and `redirects` for old URLs |
| `*.md` | Pages. A page's URL is its path without `.md`: `concepts/lifecycle.md` → `/concepts/lifecycle` |
| `.mintignore` | Files here that are **not** pages: this README and the legacy stubs |

A page shows up in the sidebar only when `docs.json` lists it. A page that isn't listed still builds and is reachable by URL.

## Preview and check locally

```bash
cd docs
npx mint dev             # http://localhost:3000, reloads on save
npx mint broken-links    # what CI runs on every PR that touches docs/
```

## Rules for pages

- **Frontmatter:** every page has `title` and `description`. Mintlify renders `title` as the page heading, so **don't start the body with a `# Title`** (it would show twice). `sidebarTitle` overrides the sidebar label.
- **Links between pages:** relative, with `.md`, and always starting with `./` or `../`: `[Lifecycle](../concepts/lifecycle.md#health)`, `[Runtimes](./runtimes.md)`. Mintlify turns these into site URLs, and they still work when browsing the repo on GitHub. A bare `runtimes.md` (no `./`) is **not** rewritten and 404s on the site.
- **MDX:** Mintlify parses `.md` as MDX. Put placeholders like `<id>` and literal braces like `{"version": N}` inside backticks, or the page fails to build.
- **Diagrams:** ` ```mermaid ` blocks render as diagrams.
- Live content lives in `getting-started/`, `concepts/`, `guides/`, `reference/`, `security/`, `operations/`, `project/`.
- Each page has a `What you'll need` (or equivalent), copyable `bash` blocks, and a `Related` section.

## Legacy stubs

`architecture.md`, `auto-generation.md`, `deployment.md`, `examples.md`, `install.md`, `russelfile.md`, `security-tls.md`, `traefik.md`, and `vps-one-dev.md` at the top level are short pointers kept because the repo still links to them (CONTRIBUTING, CLI error text). Don't expand them. On the site they are ignored, and `redirects` in `docs.json` sends their old URLs to the new pages.

## Adding a page

1. Write `docs/<section>/<page>.md` with `title` and `description` frontmatter.
2. Add `"<section>/<page>"` to the right group in `docs.json`.
3. `npx mint broken-links`, then open it in `npx mint dev`.
