#!/usr/bin/env bash
# Deploy every example on one runtime against a real, non-root russel-ctrl,
# check that the app itself answers, destroy it, and check nothing is left
# (#474, #470).
#
# Usage: contrib/tests/deploy_examples.sh [example ...]
#   RUNTIME          container (default) or microvm; every example runs on it,
#                    whatever its own `type` says
#   RUSSEL_BIN_DIR   dir with russel and russel-ctrl (default: target/release)
#   CTRL_PORT        ctrl listen port (default: 7990)
#
# Needs Nix with flakes, curl, and jq, plus rootless Podman for containers,
# or /dev/kvm, cloud-hypervisor, virtiofsd, passt, and RUSSEL_KERNEL_PATH (or
# a buildable .#microvm-kernel) for microVMs.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
BIN=${RUSSEL_BIN_DIR:-$ROOT/target/release}
PORT=${CTRL_PORT:-7990}
RUNTIME=${RUNTIME:-container}
READY_SECS=${READY_SECS:-60}
WORK=$(mktemp -d)
DATA=$WORK/data
VOLUMES=$WORK/volumes
export RUSSEL_CONTROL_PLANE=http://127.0.0.1:$PORT

case "$RUNTIME" in
container | microvm) ;;
*)
  echo "RUNTIME must be container or microvm (got $RUNTIME)"
  exit 2
  ;;
esac

CTRL_PID=
FAILED=()
PASSED=()

log() { printf '\n==> %s\n' "$*"; }

cleanup() {
  local status=$?
  if [ -n "$CTRL_PID" ]; then
    kill "$CTRL_PID" 2>/dev/null || true
    wait "$CTRL_PID" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ] || [ "${#FAILED[@]}" -gt 0 ]; then
    log "ctrl log (last 80 lines)"
    tail -n 80 "$WORK/ctrl.log" 2>/dev/null || true
  fi
  # A failed destroy can leave subordinate-uid files (userns = keep-id).
  podman unshare rm -rf "$WORK" 2>/dev/null || rm -rf "$WORK"
}
trap cleanup EXIT

service_field() {
  # First `key = "value"` inside [service] of a Russelfile.
  awk -v key="$1" '
    /^[[:space:]]*\[/ { in_svc = ($0 ~ /^[[:space:]]*\[service\][[:space:]]*(#.*)?$/) }
    in_svc && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" && match($0, /"[^"]*"/) {
      print substr($0, RSTART + 1, RLENGTH - 2); exit
    }
  ' "$2"
}

# Why an example cannot run on $RUNTIME yet; empty when it can.
skip_reason() {
  case "$RUNTIME/$1" in
  *) ;;
  esac
}

# The app answers, not the port forwarder: redis and postgres must reply to
# their protocol, HTTP apps to a request (any status on "/").
probe() {
  local example=$1 port=$2 reply code
  case "$example" in
  redis)
    reply=$(timeout 2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port; printf 'PING\r\n' >&3; head -c1 <&3" 2>/dev/null) || true
    [ "$reply" = "+" ] || [ "$reply" = "-" ]
    ;;
  postgres)
    reply=$(timeout 2 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port; printf '\x00\x00\x00\x08\x04\xd2\x16\x2f' >&3; head -c1 <&3" 2>/dev/null) || true
    [ "$reply" = "N" ] || [ "$reply" = "S" ]
    ;;
  basic-http | hello-rust | env-config | shortlink | meilisearch | microvm-http)
    curl -sf --max-time 2 "http://127.0.0.1:$port/health" >/dev/null
    ;;
  *)
    code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://127.0.0.1:$port/" 2>/dev/null) || true
    [ -n "$code" ] && [ "$code" != "000" ]
    ;;
  esac
}

# Copy the example's tracked files and set `type` to $RUNTIME. Container-only
# keys go on a microVM. Deploys read a git checkout, so commit the copy.
stage_example() {
  local example=$1 dst=$2
  mkdir -p "$dst"
  git -C "$ROOT" archive HEAD "examples/$example" | tar -x -C "$dst" --strip-components=2
  sed -i -E "s/^type = \"[a-z]+\"/type = \"$RUNTIME\"/" "$dst/Russelfile.toml"
  if [ "$RUNTIME" = "microvm" ]; then
    sed -i -E '/^podman_args *=/d' "$dst/Russelfile.toml"
  fi
  grep -q "^type = \"$RUNTIME\"" "$dst/Russelfile.toml" &&
    git -C "$dst" init -q &&
    git -C "$dst" add -A &&
    git -C "$dst" -c user.email=ci@example.com -c user.name=ci commit -qm "$example"
}

# postgres binds an initialized PGDATA from a host path, which Russel does not
# create. Prepare one the way examples/postgres documents and point the staged
# copy at it.
prepare_postgres() {
  local dst=$1 pg_out pgdata=$VOLUMES/postgres
  pg_out=$(nix build --no-link --print-out-paths 'nixpkgs#postgresql^out')
  "$pg_out/bin/initdb" -D "$pgdata" -U postgres --no-locale -E UTF8 --auth=trust >/dev/null
  printf "listen_addresses = '*'\nunix_socket_directories = '/tmp'\n" >>"$pgdata/postgresql.conf"
  echo 'host all all 0.0.0.0/0 trust' >>"$pgdata/pg_hba.conf"
  sed -i "s|^host = \"/srv/russel/postgres-data\"|host = \"$pgdata\"|" "$dst/Russelfile.toml"
  grep -qxF "host = \"$pgdata\"" "$dst/Russelfile.toml" &&
    git -C "$dst" -c user.email=ci@example.com -c user.name=ci commit -qam pgdata
}

app_log() {
  case "$RUNTIME" in
  container) echo "$DATA/$1/container.log" ;;
  microvm) echo "$DATA/$1/console.log" ;;
  esac
}

# Whether the workload recorded before destroy is still around. Container
# names are per Podman user, not per data dir, so check the recorded id.
workload_left() {
  local id=$1 container=$2
  case "$RUNTIME" in
  container)
    [ -n "$container" ] && podman container exists "$container"
    ;;
  microvm)
    pgrep -a -f "(cloud-hypervisor|virtiofsd|passt).*$DATA/$id(_g[0-9a-f]+)?/"
    ;;
  esac
}

run_example() {
  local example=$1 src=$WORK/src/$1 id host_port container i
  # errexit is off inside `if run_example`, so check each step.
  if ! stage_example "$example" "$src"; then
    echo "FAIL $example: could not stage the example"
    return 1
  fi
  id=$(service_field name "$src/Russelfile.toml")
  if [ "$example" = "postgres" ] && ! prepare_postgres "$src"; then
    echo "FAIL $example: could not prepare PGDATA"
    return 1
  fi

  log "$example: deploy $RUNTIME ($id)"
  if ! timeout 900 "$BIN/russel" deploy "$src"; then
    echo "FAIL $example: deploy"
    tail -n 40 "$(app_log "$id")" 2>/dev/null || true
    return 1
  fi

  host_port=$(jq -r '.host_port // empty' "$DATA/$id/metadata.json")
  container=$(jq -r '.container_id // empty' "$DATA/$id/metadata.json")
  if [ -z "$host_port" ]; then
    echo "FAIL $example: no host_port in $DATA/$id/metadata.json"
    "$BIN/russel" destroy --delete-volumes "$id" || true
    return 1
  fi
  log "$example: probe 127.0.0.1:$host_port"
  for ((i = 0; i < READY_SECS; i++)); do
    probe "$example" "$host_port" && break
    sleep 1
  done
  if [ "$i" -ge "$READY_SECS" ]; then
    echo "FAIL $example: deployed but the app never answered on $host_port"
    tail -n 40 "$(app_log "$id")" 2>/dev/null || true
    "$BIN/russel" destroy --delete-volumes "$id" || true
    return 1
  fi

  # --delete-volumes: `keep = true` volumes would otherwise stay, by design.
  log "$example: destroy"
  "$BIN/russel" destroy --delete-volumes "$id"
  if [ -e "$DATA/$id" ]; then
    echo "FAIL $example: $DATA/$id still exists after destroy"
    find "$DATA/$id" | head -20
    return 1
  fi
  if workload_left "$id" "$container"; then
    echo "FAIL $example: $RUNTIME workload still running after destroy"
    return 1
  fi
}

mkdir -p "$DATA" "$VOLUMES" "$WORK/src" "$WORK/secrets"
for b in russel russel-ctrl; do
  [ -x "$BIN/$b" ] || { echo "missing $BIN/$b (cargo build --release)"; exit 1; }
done

if [ "$#" -gt 0 ]; then
  examples=("$@")
else
  examples=()
  for f in "$ROOT"/examples/*/Russelfile.toml; do
    examples+=("$(basename "$(dirname "$f")")")
  done
fi

log "starting russel-ctrl as $(id -un) on 127.0.0.1:$PORT ($RUNTIME examples)"
RUSSEL_DATA_DIR=$DATA \
  RUSSEL_SECRETS_DIR=$WORK/secrets \
  RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1 \
  RUSSEL_VOLUME_ROOTS=$VOLUMES \
  RUSSEL_CTRL_ADDR=127.0.0.1:$PORT \
  "$BIN/russel-ctrl" >"$WORK/ctrl.log" 2>&1 &
CTRL_PID=$!
for _ in $(seq 1 30); do
  curl -sf "$RUSSEL_CONTROL_PLANE/vms" >/dev/null && break
  kill -0 "$CTRL_PID" 2>/dev/null || { echo "russel-ctrl exited"; exit 1; }
  sleep 1
done
curl -sf "$RUSSEL_CONTROL_PLANE/vms" >/dev/null || { echo "russel-ctrl never became ready"; exit 1; }

# examples/env-config reads DEMO_SECRET through secret://.
printf '%s' 'ci-demo-secret' | "$BIN/russel" secrets set DEMO_SECRET

SKIPPED=()
for example in "${examples[@]}"; do
  reason=$(skip_reason "$example")
  if [ -n "$reason" ]; then
    log "$example: skipped on $RUNTIME: $reason"
    SKIPPED+=("$example")
  elif run_example "$example"; then
    PASSED+=("$example")
  else
    FAILED+=("$example")
  fi
done

log "$RUNTIME passed (${#PASSED[@]}): ${PASSED[*]:-}"
[ "${#SKIPPED[@]}" -eq 0 ] || log "$RUNTIME skipped (${#SKIPPED[@]}): ${SKIPPED[*]}"
if [ "${#FAILED[@]}" -gt 0 ]; then
  log "$RUNTIME failed (${#FAILED[@]}): ${FAILED[*]}"
  exit 1
fi
