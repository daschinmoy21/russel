# Russel Basic HTTP Test App

This is the smallest app intended to exercise Russel's MVP deploy pipeline.

It provides:

- `flake.nix` that builds a package with `/bin/api`
- `Russelfile.toml` using service name `api`
- a small Go static-site server using only the standard library
- static dashboard response on `/`
- health response on `/health`
- graceful shutdown on `Ctrl-C` / `SIGTERM`

Build it directly:

```sh
nix build path:$PWD
PORT=3000 ./result/bin/api
```

Once these example files are tracked by Git, plain `nix build` works too.

Run it through the current Russel scaffold:

```sh
cargo run -p russel-ctrl
cargo run -p russel-cli -- deploy -p 3000:3000 --vm-id test-vm examples/basic-http
cargo run -p russel-cli -- status
cargo run -p russel-cli -- logs
```

The current Russel scaffold builds and generates microvm.nix config, but it
does not boot the microVM yet.
