#!/usr/bin/env bash
set -euo pipefail

TEST_ROOT="$(mktemp -d)"
MOCK_BIN="${TEST_ROOT}/mock-bin"
mkdir -p "$MOCK_BIN"
trap 'rm -rf "$TEST_ROOT"' EXIT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=contrib/install.sh
source "${SCRIPT_DIR}/install.sh"

cat >"${MOCK_BIN}/openssl" <<'MOCK_OPENSSL'
#!/usr/bin/env bash
printf '%s\n' known-fake-token
MOCK_OPENSSL

cat >"${MOCK_BIN}/curl" <<'MOCK_CURL'
#!/usr/bin/env bash
set -euo pipefail
output=
headers=
url=
while (( $# )); do
  if [[ "$1" == --output ]]; then
    output=$2
    shift 2
  elif [[ "$1" == --dump-header ]]; then
    headers=$2
    shift 2
  elif [[ "$1" == -* ]]; then
    shift
  else
    url=$1
    shift
  fi
done
write_headers() {
  [[ -n "$headers" ]] || return 0
  printf '%s\n' "$1" >"$headers"
}
# Release downloads are served from MOCK_RELEASE_DIR by asset basename. A
# missing file is a 404 (curl exit 22), like a real release without the asset.
if [[ -n "${MOCK_RELEASE_DIR:-}" && -n "$url" && "$url" == */releases/download/* ]]; then
  asset=${url##*/}
  if [[ ! -f "${MOCK_RELEASE_DIR}/${asset}" ]]; then
    exit 22
  fi
  cp "${MOCK_RELEASE_DIR}/${asset}" "$output"
  exit 0
fi
mode=${MOCK_CURL_MODE:-unreachable}
if [[ -n "${MOCK_CURL_MODE_FILE:-}" && -f "$MOCK_CURL_MODE_FILE" ]]; then
  mode=$(<"$MOCK_CURL_MODE_FILE")
fi
case "$mode" in
  hang) sleep 30 ;;
  401)
    : >"$output"
    write_headers 'WWW-Authenticate: Bearer realm="russel-ctrl"'
    printf '401'
    ;;
  401-foreign)
    : >"$output"
    write_headers 'Server: unrelated-listener'
    printf '401'
    ;;
  200-valid) printf '%s' '{"vms":[]}' >"$output"; printf '200' ;;
  200-garbage) printf '%s' '{"status":"ok"}' >"$output"; printf '200' ;;
  503) printf '%s' 'busy' >"$output"; printf '503' ;;
  *) exit 7 ;;
esac
MOCK_CURL

for mock in systemctl ssh ss sudo russel; do
  cat >"${MOCK_BIN}/$mock" <<'MOCK_COMMAND'
#!/usr/bin/env bash
exit 0
MOCK_COMMAND
done
chmod 755 "${MOCK_BIN}"/*

export PATH="${MOCK_BIN}:${PATH}"
export MOCK_CURL_MODE_FILE="${TEST_ROOT}/curl-mode"

die() {
  echo "FAIL: $*" >&2
  exit 1
}

pass() {
  echo "ok - $*"
}

new_case() {
  CASE_ROOT="${TEST_ROOT}/case"
  rm -rf "$CASE_ROOT"
  mkdir -p "$CASE_ROOT/dest" "$CASE_ROOT/state-parent" "$CASE_ROOT/home" "$CASE_ROOT/release"
  HOME="${CASE_ROOT}/home"
  export HOME
  RELEASE="${CASE_ROOT}/release"
  export RELEASE
  CASE_DEST="${CASE_ROOT}/dest/russel-ctrl"
  CASE_STATE="${CASE_ROOT}/state-parent/russel"
  export CASE_DEST CASE_STATE
  : >"$MOCK_CURL_MODE_FILE"
  MOCK_CURL_MODE=unreachable
  export MOCK_CURL_MODE
  FAKE_USER=alice
  FAKE_GROUP=users
  FAKE_UID=1000
  FAKE_GID=1000
  FAKE_OS=Linux
  FAKE_NIXOS_MARKER=0
  FAKE_NIXOS_RELEASE=0
  SYSTEM_UNIT_ACTIVE=0
  SYSTEM_FRAGMENT=
  SYSTEM_FRAGMENT_UNIT=
  USER_MANAGER_OK=1
  USER_UNIT_EXISTS=1
  USER_ACTIVE=0
  ENABLE_RESULT=0
  RESTART_FAIL_FIRST=0
  LINGER_FAIL=0
  SSH_RESULT=0
  SSH_ESTABLISH=0
  MOCK_LISTENER_STATE=absent
  MOCK_SS_AVAILABLE=1
  MOCK_LSOF_AVAILABLE=0
  MOCK_SYSTEMCTL_AVAILABLE=1
  FAKE_STATE_OWNER=1000:1000
  unset RUSSEL_CTRL_DEST
  RUSSEL_DASHBOARD_DEST="${CASE_ROOT}/share/russel/dashboard"
  export RUSSEL_DASHBOARD_DEST
  RUSSEL_DASHBOARD_SRC="${CASE_ROOT}/dash-src"
  mkdir -p "$RUSSEL_DASHBOARD_SRC"
  printf '%s\n' '<html>test</html>' >"${RUSSEL_DASHBOARD_SRC}/index.html"
  export RUSSEL_DASHBOARD_SRC
  FAKE_KVM=0
  unset RUSSEL_VERSION MOCK_RELEASE_DIR
  rm -f "${CASE_ROOT}/ssh-args"
  rm -f "${CASE_ROOT}/systemctl-user.log" "${CASE_ROOT}/loginctl.log" "${CASE_ROOT}/sudo.log"
  rm -f "${CASE_ROOT}/chown.log"
}

set_curl_mode() {
  MOCK_CURL_MODE=$1
  printf '%s\n' "$1" >"$MOCK_CURL_MODE_FILE"
  export MOCK_CURL_MODE
}

make_host_binary() {
  local contents=${1:-new-binary}
  printf '%s\n' "$contents" >"${RELEASE}/russel-ctrl"
  chmod 755 "${RELEASE}/russel-ctrl"
}

make_cli_binaries() {
  printf '%s\n' cli-binary >"${RELEASE}/russel"
  printf '%s\n' ctrl-binary >"${RELEASE}/russel-ctrl"
  chmod 755 "${RELEASE}/russel" "${RELEASE}/russel-ctrl"
}

# Stage a fake GitHub release. MOCK_RELEASE_DIR makes the curl mock serve these
# files for release URLs, and SHA256SUMS is real so verification passes.
make_release_assets() {
  local version=$1 directory="${CASE_ROOT}/release-assets"
  rm -rf "$directory"
  mkdir -p "$directory"
  RELEASE_ASSETS="$directory"
  export RELEASE_ASSETS
  printf '%s\n' cli-download >"${directory}/russel-${version}-x86_64"
  printf '%s\n' ctrl-download >"${directory}/russel-ctrl-${version}-x86_64"
  printf '%s\n' kernel-download >"${directory}/russel-kernel-${version}-x86_64.bzImage"
  cp "${SCRIPT_DIR}/russel-ctrl.service" "${directory}/russel-ctrl-${version}.service"
  tar -czf "${directory}/russel-dashboard-${version}.tar.gz" -C "$RUSSEL_DASHBOARD_SRC" .
  ( cd "$directory" && sha256sum russel-* >SHA256SUMS )
  MOCK_RELEASE_DIR="$directory"
  export MOCK_RELEASE_DIR
}

corrupt_checksum() {
  local asset=$1
  awk -v asset="$asset" '
    BEGIN { OFS = "  " }
    $2 == asset { $1 = "0000000000000000000000000000000000000000000000000000000000000000" }
    { print }
  ' "${RELEASE_ASSETS}/SHA256SUMS" >"${RELEASE_ASSETS}/SHA256SUMS.tmp"
  mv "${RELEASE_ASSETS}/SHA256SUMS.tmp" "${RELEASE_ASSETS}/SHA256SUMS"
}

drop_checksum() {
  local asset=$1
  awk -v asset="$asset" '$2 != asset { print }' \
    "${RELEASE_ASSETS}/SHA256SUMS" >"${RELEASE_ASSETS}/SHA256SUMS.tmp"
  mv "${RELEASE_ASSETS}/SHA256SUMS.tmp" "${RELEASE_ASSETS}/SHA256SUMS"
}

assert_output_contains() {
  local text=$1
  grep -Fq -- "$text" <<<"$LAST_OUTPUT" || die "output does not contain expected text"
}

assert_output_lacks() {
  local text=$1
  if grep -Fq -- "$text" <<<"$LAST_OUTPUT"; then
    die "output unexpectedly contained secret text"
  fi
}

assert_file_contains() {
  local file=$1
  local text=$2
  grep -Fq -- "$text" "$file" || die "${file} does not contain expected text"
}

assert_file_mode() {
  local file=$1
  local expected=$2
  local actual
  actual=$(stat -c '%a' -- "$file")
  [[ "$actual" == "$expected" ]] || die "${file} mode $actual, expected $expected"
}

expect_main_rc() {
  local expected=$1
  shift
  local actual
  set +e
  LAST_OUTPUT=$(main "$@" 2>&1)
  actual=$?
  set -e
  [[ "$actual" == "$expected" ]] || die "unexpected rc $actual, expected $expected"
}

call_count() {
  local file=$1
  local pattern=$2
  grep -c "^${pattern} " "$file" 2>/dev/null || true
}

current_user() { printf '%s\n' "$FAKE_USER"; }
current_group() { printf '%s\n' "$FAKE_GROUP"; }
current_uid() { printf '%s\n' "$FAKE_UID"; }
current_gid() { printf '%s\n' "$FAKE_GID"; }
host_os_name() { printf '%s\n' "$FAKE_OS"; }
machine_arch() { printf '%s\n' x86_64; }
kvm_available() { [[ "$FAKE_KVM" == 1 ]]; }
nixos_marker_present() { [[ "$FAKE_NIXOS_MARKER" == 1 ]]; }
os_release_is_nixos() { [[ "$FAKE_NIXOS_RELEASE" == 1 ]]; }
host_binary_path() { printf '%s\n' "$CASE_DEST"; }
state_dir_path() { printf '%s\n' "$CASE_STATE"; }
path_owner() { printf '%s\n' "$FAKE_STATE_OWNER"; }
run_chown() { printf '%s\n' "$*" >>"${CASE_ROOT}/chown.log"; }
run_sudo() {
  printf '%s\n' "$*" >>"${CASE_ROOT}/sudo.log"
  case "$1" in
    chown) : ;;
    chmod|mkdir|install|mv|rm|mktemp|cp) "${@}" ;;
    *) die "unexpected sudo operation: $1" ;;
  esac
}
command_available() {
  case "$1" in
    ss) [[ "$MOCK_SS_AVAILABLE" == 1 ]] ;;
    lsof) [[ "$MOCK_LSOF_AVAILABLE" == 1 ]] ;;
    systemctl) [[ "$MOCK_SYSTEMCTL_AVAILABLE" == 1 ]] ;;
    *) command -v "$1" >/dev/null 2>&1 ;;
  esac
}
run_systemctl_system() {
  local operation=$1
  local unit=${*: -1}
  case "$operation" in
    is-active|is-enabled)
      [[ "$SYSTEM_UNIT_ACTIVE" == 1 && "$unit" == russel.service ]] ||
        [[ "$SYSTEM_UNIT_ACTIVE" == 1 && "$unit" == russel-ctrl.service ]]
      ;;
    show)
      if [[ "$unit" == "$SYSTEM_FRAGMENT_UNIT" ]]; then
        printf '%s\n' "$SYSTEM_FRAGMENT"
      fi
      ;;
    *) return 1 ;;
  esac
}
run_systemctl_user() {
  local operation=$1
  printf '%s %s\n' "$operation" "$*" >>"${CASE_ROOT}/systemctl-user.log"
  case "$operation" in
    status) [[ "$USER_MANAGER_OK" == 1 ]] ;;
    cat) [[ "$USER_UNIT_EXISTS" == 1 ]] ;;
    daemon-reload) : ;;
    is-active)
      if [[ "$USER_ACTIVE" == 1 ]]; then
        [[ "$*" == *--quiet* ]] || printf '%s\n' active
        return 0
      fi
      [[ "$*" == *--quiet* ]] || printf '%s\n' inactive
      return 3
      ;;
    restart)
      if [[ "$RESTART_FAIL_FIRST" == 1 && $(grep -c '^restart ' "${CASE_ROOT}/systemctl-user.log") == 1 ]]; then
        printf '%s\n' 'original restart diagnostic' >&2
        return 1
      fi
      USER_ACTIVE=1
      ;;
    enable)
      if [[ "$ENABLE_RESULT" != 0 ]]; then
        printf '%s\n' 'enable diagnostic' >&2
        return "$ENABLE_RESULT"
      fi
      USER_ACTIVE=1
      ;;
    *) return 1 ;;
  esac
}
run_loginctl() {
  printf '%s\n' "$*" >>"${CASE_ROOT}/loginctl.log"
  if [[ "$LINGER_FAIL" == 1 ]]; then
    printf '%s\n' 'linger diagnostic' >&2
    return 1
  fi
}
run_ss() {
  if [[ "$MOCK_LISTENER_STATE" == present ]]; then
    printf '%s\n' 'LISTEN 0 128 127.0.0.1:7878 0.0.0.0:*'
  fi
}
run_lsof() { :; }
run_ssh() {
  {
    printf 'argc=%s\n' "$#"
    printf 'arg=%q\n' "$@"
  } >"${CASE_ROOT}/ssh-args"
  if [[ "$SSH_ESTABLISH" == 1 ]]; then
    printf '%s\n' 401 >"$MOCK_CURL_MODE_FILE"
  fi
  if [[ "$SSH_RESULT" != 0 ]]; then
    printf '%s\n' 'ssh diagnostic' >&2
    return "$SSH_RESULT"
  fi
}
run_russel() { printf '%s\n' "${MOCK_ORIGIN:-https://remote.example.invalid}"; }

# Parser and topology refusal coverage.
new_case
expect_main_rc 2 unknown
expect_main_rc 2 --bad
expect_main_rc 2 connect
expect_main_rc 2 connect ""
expect_main_rc 2 connect -host
expect_main_rc 2 status extra
pass "usage and argument validation"

new_case
make_cli_binaries
mkdir -p "$CASE_ROOT/legacy/bin"
RUSSEL_CTRL_DEST="$CASE_ROOT/legacy/bin/russel-ctrl"
export RUSSEL_CTRL_DEST
expect_main_rc 0 cli
[[ -x "$HOME/.local/bin/russel" ]] || die "cli binary was not copied"
expect_main_rc 0 ctrl
[[ -x "$RUSSEL_CTRL_DEST" ]] || die "ctrl binary was not copied"
[[ -f "${RUSSEL_DASHBOARD_DEST}/index.html" ]] || die "dashboard dist was not copied"
expect_main_rc 0 all
pass "legacy cli ctrl all copy behavior"

new_case
make_cli_binaries
mkdir -p "$CASE_ROOT/legacy/bin"
RUSSEL_CTRL_DEST="$CASE_ROOT/legacy/bin/russel-ctrl"
export RUSSEL_CTRL_DEST
RUSSEL_DASHBOARD_SRC="${CASE_ROOT}/empty-dash"
mkdir -p "$RUSSEL_DASHBOARD_SRC"
export RUSSEL_DASHBOARD_SRC
expect_main_rc 0 ctrl
[[ -x "$RUSSEL_CTRL_DEST" ]] || die "ctrl binary was not copied without dashboard dist"
[[ ! -e "${RUSSEL_DASHBOARD_DEST}/index.html" ]] || die "missing dist must not invent a dashboard"
assert_output_contains "index.html missing"
pass "ctrl install warns and continues without dashboard dist"

# Download mode needs an explicit version when no local build exists.
new_case
expect_main_rc 1 cli
assert_output_contains 'RUSSEL_VERSION is unset'
pass "download mode requires a version without a local build"

# A verified release asset installs the CLI.
new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
expect_main_rc 0 cli
[[ "$(<"$HOME/.local/bin/russel")" == 'cli-download' ]] || die "cli binary was not downloaded"
pass "cli download mode verifies SHA256"

# Ctrl download mode also extracts the dashboard dist tarball.
new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
unset RUSSEL_DASHBOARD_SRC
RUSSEL_CTRL_DEST="${CASE_ROOT}/download-bin/russel-ctrl"
export RUSSEL_CTRL_DEST
expect_main_rc 0 ctrl
[[ "$(<"$RUSSEL_CTRL_DEST")" == 'ctrl-download' ]] || die "ctrl binary was not downloaded"
[[ -f "${RUSSEL_DASHBOARD_DEST}/index.html" ]] || die "dashboard dist was not extracted"
pass "ctrl download mode fetches binary and dashboard"

new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
corrupt_checksum "russel-v1.2.3-x86_64"
expect_main_rc 1 cli
assert_output_contains 'checksum mismatch'
[[ ! -e "$HOME/.local/bin/russel" ]] || die "unverified binary was installed"
pass "download mode refuses a checksum mismatch"

new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
drop_checksum "russel-v1.2.3-x86_64"
expect_main_rc 1 cli
assert_output_contains 'missing from SHA256SUMS'
pass "download mode requires a SHA256SUMS entry"

# host download mode: binary, unit, dashboard, and the KVM-gated kernel.
new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
unset RUSSEL_DASHBOARD_SRC
FAKE_KVM=1
set_curl_mode 401
expect_main_rc 0 host
cmp -s "$CASE_DEST" <(printf '%s\n' ctrl-download) || die "downloaded ctrl binary was not installed"
cmp -s "$HOME/.config/systemd/user/russel-ctrl.service" "${SCRIPT_DIR}/russel-ctrl.service" \
  || die "downloaded unit was not installed"
[[ "$(<"$(kernel_pool_image_path)")" == 'kernel-download' ]] || die "kernel was not installed into the pool"
assert_file_mode "$CASE_STATE/_pool/kernel" 700
assert_file_mode "$CASE_STATE/_pool/kernel/bzImage" 644
grep -Fq 'alice:users' "${CASE_ROOT}/chown.log" || die "kernel pool was not chowned to the operator"
assert_output_contains 'installed microVM kernel'
pass "host download mode fetches the KVM-gated kernel with operator ownership"

# A container-only host skips the kernel instead of failing.
new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
FAKE_KVM=0
set_curl_mode 401
expect_main_rc 0 host
assert_output_contains 'containers only'
[[ ! -e "$CASE_STATE/_pool/kernel/bzImage" ]] || die "kernel was fetched without /dev/kvm"
pass "host skips the kernel on a container-only host"

# A KVM host must have the kernel asset. Missing it is a failed install, not a skip.
new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
rm -f "${RELEASE_ASSETS}/russel-kernel-v1.2.3-x86_64.bzImage"
FAKE_KVM=1
set_curl_mode 401
expect_main_rc 1 host
assert_output_contains 'cannot download'
[[ ! -e "$CASE_STATE/_pool/kernel/bzImage" ]] || die "failed kernel fetch still wrote a pool image"
pass "host download mode requires the kernel asset on a KVM host"

# `all` never fetches the kernel, even with KVM present.
new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
FAKE_KVM=1
mkdir -p "$CASE_ROOT/legacy/bin"
RUSSEL_CTRL_DEST="$CASE_ROOT/legacy/bin/russel-ctrl"
export RUSSEL_CTRL_DEST
expect_main_rc 0 all
[[ -x "$HOME/.local/bin/russel" ]] || die "cli binary was not downloaded"
[[ -x "$RUSSEL_CTRL_DEST" ]] || die "ctrl binary was not downloaded"
[[ ! -e "$CASE_STATE/_pool" ]] || die "all must not fetch the microVM kernel"
pass "all installs binaries without the kernel"

new_case
FAKE_USER=root
expect_main_rc 2 host
new_case
FAKE_OS=Darwin
expect_main_rc 2 host
new_case
FAKE_NIXOS_MARKER=1
expect_main_rc 2 host
new_case
FAKE_NIXOS_RELEASE=1
expect_main_rc 2 host
new_case
SYSTEM_UNIT_ACTIVE=1
expect_main_rc 2 host
new_case
SYSTEM_FRAGMENT=/nix/store/russel-unit
SYSTEM_FRAGMENT_UNIT=russel.service
expect_main_rc 2 host
pass "root, OS, NixOS, system-unit, and Nix-store refusals"

# First host install: the fake openssl token is captured into the env file,
# and must never be emitted by the installer.
new_case
make_host_binary first-binary
set_curl_mode 401
expect_main_rc 0 host
ENV_FILE="$HOME/.config/russel/env"
UNIT_FILE="$HOME/.config/systemd/user/russel-ctrl.service"
[[ "$(<"$ENV_FILE")" == 'RUSSEL_API_TOKEN=known-fake-token' ]] || die "unexpected token file"
assert_file_mode "$ENV_FILE" 600
assert_file_mode "$HOME/.config/russel" 700
assert_file_mode "$HOME/.config/systemd/user" 700
cmp -s "$UNIT_FILE" "${SCRIPT_DIR}/russel-ctrl.service" || die "unit was not installed"
cmp -s "$CASE_DEST" "${RELEASE}/russel-ctrl" || die "host binary was not installed"
[[ "$(call_count "${CASE_ROOT}/systemctl-user.log" enable)" == 1 ]] || die "first install enable call missing"
[[ "$(call_count "${CASE_ROOT}/systemctl-user.log" daemon-reload)" == 1 ]] || die "first install daemon-reload call missing"
[[ "$(wc -l <"${CASE_ROOT}/loginctl.log")" == 1 ]] || die "first install linger call missing"
assert_output_lacks known-fake-token
pass "first host install, token permissions, unit, binary, enable, linger"

# A differing but valid unit is preserved, along with the token and state.
new_case
make_host_binary old-binary
set_curl_mode 401
expect_main_rc 0 host
ENV_FILE="$HOME/.config/russel/env"
UNIT_FILE="$HOME/.config/systemd/user/russel-ctrl.service"
printf '%s\n' 'RUSSEL_API_TOKEN=preserved-token' >"$ENV_FILE"
chmod 600 "$ENV_FILE"
printf '%s\n' '# operator edit' >>"$UNIT_FILE"
make_host_binary new-binary
USER_ACTIVE=1
expect_main_rc 0 host
[[ "$(<"$ENV_FILE")" == 'RUSSEL_API_TOKEN=preserved-token' ]] || die "existing token changed"
grep -Fq '# operator edit' "$UNIT_FILE" || die "existing unit changed"
[[ "$(call_count "${CASE_ROOT}/systemctl-user.log" restart)" == 1 ]] || die "active unit was not restarted"
cmp -s "$CASE_DEST.previous" <(printf '%s\n' old-binary) || die "previous binary copy missing"
assert_output_lacks preserved-token
pass "existing token, unit, state, and binary backup preservation"

# Force replaces a differing valid unit and reloads it.
new_case
make_host_binary force-binary
mkdir -p "$HOME/.config/systemd/user" "$HOME/.config/russel"
cp "${SCRIPT_DIR}/russel-ctrl.service" "$HOME/.config/systemd/user/russel-ctrl.service"
printf '%s\n' '# operator edit' >>"$HOME/.config/systemd/user/russel-ctrl.service"
printf '%s\n' 'RUSSEL_API_TOKEN=preserved-token' >"$HOME/.config/russel/env"
chmod 600 "$HOME/.config/russel/env"
set_curl_mode 401
expect_main_rc 0 --force-unit host
cmp -s "$HOME/.config/systemd/user/russel-ctrl.service" "${SCRIPT_DIR}/russel-ctrl.service" || die "force did not replace unit"
[[ "$(call_count "${CASE_ROOT}/systemctl-user.log" daemon-reload)" == 1 ]] || die "force did not daemon-reload"
pass "force-unit replacement"

# State ownership is refused unless explicitly taken.
new_case
make_host_binary state-binary
mkdir -p "$CASE_STATE"
FAKE_STATE_OWNER=2000:2000
set_curl_mode 401
expect_main_rc 1 host
[[ ! -e "${CASE_ROOT}/sudo.log" || "$(grep -c '^chown ' "${CASE_ROOT}/sudo.log")" == 0 ]] || die "state refusal attempted chown"
expect_main_rc 0 --take-state-ownership host
[[ "$(grep -c '^chown ' "${CASE_ROOT}/sudo.log" 2>/dev/null || true)" == 1 ]] || die "state ownership takeover did not chown"
pass "state ownership refusal and takeover"

# A failed active-unit restart restores the old binary and retries restart once.
new_case
make_host_binary old-active-binary
set_curl_mode 401
expect_main_rc 0 host
make_host_binary new-active-binary
USER_ACTIVE=1
RESTART_FAIL_FIRST=1
expect_main_rc 1 host
[[ "$(<"$CASE_DEST")" == 'old-active-binary' ]] || die "binary rollback failed"
[[ "$(<"$CASE_DEST.previous")" == 'old-active-binary' ]] || die "rollback backup changed"
[[ "$(call_count "${CASE_ROOT}/systemctl-user.log" restart)" == 2 ]] || die "restoration restart was not attempted exactly once"
assert_output_contains 'original restart diagnostic'
pass "active restart rollback and restoration restart"

# The pool kernel is part of that rollback: a failed upgrade puts the previous
# kernel back instead of leaving the replacement in place.
new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
FAKE_KVM=1
set_curl_mode 401
expect_main_rc 0 host
CASE_KERNEL="$(kernel_pool_image_path)"
[[ "$(<"$CASE_KERNEL")" == 'kernel-download' ]] || die "kernel was not installed into the pool"
printf '%s\n' ctrl-download-v2 >"${RELEASE_ASSETS}/russel-ctrl-v1.2.3-x86_64"
printf '%s\n' kernel-download-v2 >"${RELEASE_ASSETS}/russel-kernel-v1.2.3-x86_64.bzImage"
( cd "$RELEASE_ASSETS" && sha256sum russel-* >SHA256SUMS )
USER_ACTIVE=1
RESTART_FAIL_FIRST=1
expect_main_rc 1 host
[[ "$(<"$CASE_DEST")" == 'ctrl-download' ]] || die "binary rollback failed"
[[ "$(<"$CASE_KERNEL")" == 'kernel-download' ]] \
  || die "failed upgrade left the replacement kernel in the pool"
[[ "$(<"${CASE_KERNEL}.previous")" == 'kernel-download' ]] || die "kernel backup missing"
pass "failed upgrade restores the previous pool kernel with the binary"

# Linger failure is a warning after a healthy endpoint, not an install failure.
new_case
make_host_binary linger-binary
set_curl_mode 200-valid
LINGER_FAIL=1
expect_main_rc 0 host
assert_output_contains 'Host installation is ready.'
assert_output_contains 'warning: could not enable login linger'
pass "linger warning after usable install"

# Probe classification: 401, valid 200 JSON, and invalid 200 JSON.
new_case
set_curl_mode 401
probe_local_endpoint || die "401 probe failed"
[[ "$PROBE_RESULT" == unauthorized ]] || die "401 probe classification wrong"
set_curl_mode 200-valid
probe_local_endpoint || die "valid 200 probe failed"
[[ "$PROBE_RESULT" == compatible ]] || die "valid 200 probe classification wrong"
set_curl_mode 200-garbage
if probe_local_endpoint; then
  die "garbage 200 probe unexpectedly passed"
fi
[[ "$PROBE_RESULT" == invalid_json ]] || die "garbage 200 probe classification wrong"
set_curl_mode 503
if probe_local_endpoint; then
  die "unrelated HTTP probe unexpectedly passed"
fi
[[ "$PROBE_RESULT" == other_http && "$PROBE_HTTP_STATUS" == 503 ]] || die "other HTTP classification wrong"
set_curl_mode 401-foreign
if probe_local_endpoint; then
  die "foreign 401 probe unexpectedly passed"
fi
[[ "$PROBE_RESULT" == foreign_unauthorized ]] || die "foreign 401 probe classification wrong"
if probe_is_compatible; then
  die "foreign 401 unexpectedly counts as compatible"
fi
pass "compatible and unrelated local endpoint probes"

# A hanging curl is bounded by the external timeout wrapper.
new_case
PROBE_TIMEOUT_SECONDS=1
PROBE_ATTEMPTS=1
set_curl_mode hang
started=$(date +%s)
set +e
probe_local_endpoint
probe_rc=$?
set -e
elapsed=$(( $(date +%s) - started ))
[[ "$probe_rc" != 0 && "$elapsed" -le 4 ]] || die "probe timeout was not bounded"
pass "bounded probe timeout"

# Connect does not open a duplicate tunnel for an existing compatible endpoint.
new_case
set_curl_mode 401
expect_main_rc 0 connect 'user@host'
[[ ! -e "${CASE_ROOT}/ssh-args" ]] || die "duplicate tunnel was opened"
assert_output_contains 'no tunnel opened'
pass "connect no-duplicate tunnel"

# Connect uses the anchored SSH argv and keeps the destination as one argument.
new_case
SSH_ESTABLISH=1
set_curl_mode unreachable
expect_main_rc 0 connect 'user@host alias'
[[ -e "${CASE_ROOT}/ssh-args" ]] || die "SSH was not called"
assert_file_contains "$CASE_ROOT/ssh-args" 'argc=9'
assert_file_contains "$CASE_ROOT/ssh-args" 'arg=-f'
assert_file_contains "$CASE_ROOT/ssh-args" 'arg=-N'
assert_file_contains "$CASE_ROOT/ssh-args" 'arg=127.0.0.1:7878:127.0.0.1:7878'
assert_file_contains "$CASE_ROOT/ssh-args" 'arg=user@host\ alias'
assert_output_contains 'Next check: russel origin'
pass "connect anchored SSH command and quoted destination"

# A garbage endpoint with a listener is not replaced by a tunnel.
new_case
set_curl_mode 200-garbage
MOCK_LISTENER_STATE=present
expect_main_rc 1 connect 'user@host'
[[ ! -e "${CASE_ROOT}/ssh-args" ]] || die "SSH ran with an unrelated listener"
assert_output_contains 'non-compatible listener'
pass "connect unrelated listener refusal"

# Status is independent and labels missing optional tools.
new_case
MOCK_SS_AVAILABLE=0
MOCK_LSOF_AVAILABLE=0
MOCK_SYSTEMCTL_AVAILABLE=0
set_curl_mode unreachable
expect_main_rc 1 status
assert_output_contains 'listener: unavailable'
assert_output_contains 'unit: unavailable'
assert_output_contains 'API: unreachable'
pass "status missing optional tools"

new_case
set_curl_mode 401
expect_main_rc 0 status
assert_output_contains 'API: unauthorized'
new_case
set_curl_mode 200-valid
expect_main_rc 0 status
assert_output_contains 'API: compatible'
new_case
set_curl_mode 200-garbage
expect_main_rc 1 status
assert_output_contains 'API: other HTTP (200; invalid /vms JSON)'
new_case
set_curl_mode 401-foreign
expect_main_rc 1 status
assert_output_contains 'API: unauthorized (HTTP 401; unrecognized endpoint'
new_case
MOCK_ORIGIN=https://elsewhere.example.invalid
set_curl_mode 401
expect_main_rc 0 status
assert_output_contains 'russel origin: https://elsewhere.example.invalid'
assert_output_contains 'API: unauthorized'
pass "status API exit and independent origin reporting"

echo "all installer tests passed"
