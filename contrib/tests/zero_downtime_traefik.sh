#!/usr/bin/env bash
# Run the zero-downtime traffic test (#562) through a real Traefik.
#
# Starts Traefik with a file provider on a temp dir and an HTTP entry point
# on 127.0.0.1, then runs the ignored test `real_traefik_update_under_traffic`.
# It updates a service from one backend to another under keep-alive traffic
# with long requests, the way a dual-live deploy does, and fails on any
# dropped request or broken connection. It then repeats the update retiring
# the old backend as soon as the route file is written (the behavior before
# #562) and expects drops, to show the measurement catches them.
#
# Needs `traefik` on PATH (or nix, to fetch it) and cargo (run it inside
# `nix develop`). PORT picks the entry point port (default 18080).
set -euo pipefail

PORT="${PORT:-18080}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d)"
TRAEFIK_PID=
cleanup() {
  if [[ -n "$TRAEFIK_PID" ]]; then
    kill "$TRAEFIK_PID" 2>/dev/null || true
    wait "$TRAEFIK_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

mkdir -p "$WORK/dynamic"
cat >"$WORK/traefik.yml" <<EOF
entryPoints:
  web:
    address: "127.0.0.1:${PORT}"
providers:
  file:
    directory: "${WORK}/dynamic"
    watch: true
log:
  level: WARN
EOF

if command -v traefik >/dev/null; then
  traefik --configFile="$WORK/traefik.yml" >"$WORK/traefik.log" 2>&1 &
else
  nix shell nixpkgs#traefik -c traefik --configFile="$WORK/traefik.yml" >"$WORK/traefik.log" 2>&1 &
fi
TRAEFIK_PID=$!

for _ in $(seq 1 300); do
  if curl -s -o /dev/null "http://127.0.0.1:${PORT}/"; then
    break
  fi
  if ! kill -0 "$TRAEFIK_PID" 2>/dev/null; then
    cat "$WORK/traefik.log" >&2
    echo "traefik exited before it listened on ${PORT}" >&2
    exit 1
  fi
  sleep 0.2
done

cd "$ROOT"
RUSSEL_TEST_TRAEFIK_DIR="$WORK/dynamic" RUSSEL_TEST_TRAEFIK_PORT="$PORT" \
  cargo test -p russel-ctrl --lib real_traefik_update_under_traffic -- --ignored --nocapture
