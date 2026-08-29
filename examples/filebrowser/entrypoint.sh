#!/bin/sh
# Demo entrypoint. Auth on. Bind 0.0.0.0 in the guest so -p works.
# Isolation is host-side (RUSSEL_PUBLISH_BIND, default 127.0.0.1).
# Not for production.
set -eu

mkdir -p /tmp/files
echo "Welcome to Russel File Manager inside your Container!" > /tmp/files/README.txt
echo "This container booted quickly using a lightweight Alpine image." >> /tmp/files/README.txt

# Warn whenever the effective password is the public demo string, including
# when FILEBROWSER_PASSWORD is set explicitly (the shipped Russelfile does).
demo_password="demo-only-not-for-production"
password="${FILEBROWSER_PASSWORD:-$demo_password}"
if [ "$password" = "$demo_password" ]; then
  echo "WARNING: demo login admin / demo-only-not-for-production. Not for production. Set FILEBROWSER_PASSWORD." >&2
fi

hash="$(filebrowser hash "$password")"

# Guest 0.0.0.0 is required for ordinary Podman/Docker -p (slirp/pasta hit the
# container IP, not guest loopback). Do not copy --noauth. Do not publish the
# host port on 0.0.0.0.
exec filebrowser \
  --address 0.0.0.0 \
  --port "${PORT:-8080}" \
  --database /tmp/filebrowser.db \
  --root /tmp/files \
  --username admin \
  --password "$hash"
