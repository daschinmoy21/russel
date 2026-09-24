# caddy

Stateless `caddy file-server` demo on port 8080, built from nixpkgs with
`service.package = "caddy"`.

```bash
./target/debug/russel-ctrl
./target/debug/russel deploy examples/caddy -p 8080:8080 --vm-id caddy
curl http://127.0.0.1:8080/
./target/debug/russel destroy caddy
```

It serves the directory it starts in; there is no document root and no volume.
A host Caddyfile would need an absolute `host =` bind under
`RUSSEL_VOLUME_ROOTS`, so this example stays stateless.
