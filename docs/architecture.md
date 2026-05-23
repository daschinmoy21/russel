# Architecture Notes

The MVP control plane owns the deploy pipeline:

1. resolve a local or remote Git repository
2. parse `Russelfile.toml`
3. run `nix build --print-out-paths`
4. generate a microvm.nix definition for the service
5. register the VM backend with Traefik
6. track status and logs

Cloud Hypervisor is reached through microvm.nix. Russel should keep this module
boundary clean so later phases can add warm pools, health replacement, and
network proxy rules without rewriting deploy orchestration.
