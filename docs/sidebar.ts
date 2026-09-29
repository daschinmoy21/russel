// Astro Starlight / Docusaurus sidebar snippet.
// Import into astro.config.mjs: `import { sidebar } from './docs/sidebar.ts'`
export const sidebar = [
  { label: 'Overview', link: 'index' },
  { label: 'Quickstart', link: 'quickstart' },
  {
    label: 'Getting started',
    items: [
      'getting-started/installation',
      'getting-started/first-deploy',
      'getting-started/dashboard',
    ],
  },
  {
    label: 'Concepts',
    items: [
      'concepts/architecture',
      'concepts/runtimes',
      'concepts/networking',
      'concepts/lifecycle',
      'concepts/builds',
    ],
  },
  {
    label: 'Guides',
    items: [
      'guides/vps-one-dev',
      'guides/tls-reverse-proxy',
      'guides/traefik-ingress',
      'guides/env-secrets',
      'guides/update-rollback',
      'guides/troubleshooting',
      'guides/benchmarks',
    ],
  },
  {
    label: 'Reference',
    items: [
      'reference/cli',
      'reference/api',
      'reference/russelfile',
      'reference/environment',
      'reference/examples',
    ],
  },
  {
    label: 'Security',
    items: ['security/overview', 'security/nix-builds'],
  },
  {
    label: 'Operations',
    items: ['operations/systemd-nixos', 'operations/upgrades-backup'],
  },
  {
    label: 'Project',
    items: ['project/development', 'project/releases', 'project/features/v0.1'],
  },
];
