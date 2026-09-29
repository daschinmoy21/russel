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
  # A freshly started ctrl: refused for the first 3 probes, then up.
  slow-start-401)
    count_file="${MOCK_CURL_MODE_FILE}.count"
    count=0
    [[ -f "$count_file" ]] && count=$(<"$count_file")
    count=$((count + 1))
    printf '%s\n' "$count" >"$count_file"
    ((count > 3)) || exit 7
    : >"$output"
    write_headers 'WWW-Authenticate: Bearer realm="russel-ctrl"'
    printf '401'
    ;;
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
  # `host` runs as root through sudo; alice is the operator who ran it.
  FAKE_USER=root
  FAKE_GROUP=root
  FAKE_UID=0
  FAKE_GID=0
  SUDO_USER=alice
  export SUDO_USER
  FAKE_OS=Linux
  FAKE_NIXOS_MARKER=0
  FAKE_NIXOS_RELEASE=0
  SYSTEM_UNIT_ACTIVE=0
  SYSTEM_FRAGMENT=
  SYSTEM_FRAGMENT_UNIT=
  SYSTEM_ACTIVE=0
  SYSTEM_UNIT_INSTALLED=0
  USER_UNIT_EXISTS=0
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
  # The russel account gets uid/gid 990 from the useradd mock.
  FAKE_STATE_OWNER=990:990
  FAKE_ENV_OWNER=0:990
  # Prerequisite probes all pass unless a case breaks one.
  FAKE_MISSING=
  FAKE_CGROUP_V2=1
  FAKE_USER_DBUS=1
  FAKE_NIX=1
  FAKE_NIX_DAEMON=1
  FAKE_FLAKES=1
  FAKE_PODMAN_MAJOR=5
  FAKE_DISK_GIB=100
  FAKE_CH_MAJOR=53
  FAKE_MICROVM_MISSING=
  FAKE_KVM_GROUP=1
  mkdir -p "$CASE_ROOT/etc/systemd/system" "$CASE_ROOT/home-alice"
  printf '%s\n' "alice:x:1000:1000:Alice:${CASE_ROOT}/home-alice:/bin/bash" >"$CASE_ROOT/passwd"
  printf '%s\n' 'alice:x:1000:' 'users:x:100:' >"$CASE_ROOT/group"
  printf '%s\n' 'alice:100000:65536' >"$CASE_ROOT/etc/subuid"
  printf '%s\n' 'alice:100000:65536' >"$CASE_ROOT/etc/subgid"
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
  rm -f "${CASE_ROOT}/systemctl-user.log" "${CASE_ROOT}/systemctl-system.log"
  rm -f "${CASE_ROOT}/loginctl.log" "${CASE_ROOT}/sudo.log" "${CASE_ROOT}/usermod.log"
  rm -f "${CASE_ROOT}/chown.log"
}

UNIT_FILE_REL=etc/systemd/system/russel-ctrl.service

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
  grep -Fq -- "$text" <<<"$LAST_OUTPUT" || { printf '%s\n' "$LAST_OUTPUT" >&2; die "output does not contain: $text"; }
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
  if [[ "$actual" != "$expected" ]]; then
    printf '%s\n' "$LAST_OUTPUT" >&2
    die "unexpected rc $actual, expected $expected (args: $*)"
  fi
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
system_unit_path() { printf '%s\n' "${CASE_ROOT}/${UNIT_FILE_REL}"; }
env_dir_path() { printf '%s\n' "${CASE_ROOT}/etc/russel"; }
subuid_file_path() { printf '%s\n' "${CASE_ROOT}/etc/subuid"; }
subgid_file_path() { printf '%s\n' "${CASE_ROOT}/etc/subgid"; }
path_owner() {
  if [[ "$1" == "$(env_file_path)" ]]; then
    printf '%s\n' "$FAKE_ENV_OWNER"
  else
    printf '%s\n' "$FAKE_STATE_OWNER"
  fi
}
run_chown() { printf '%s\n' "$*" >>"${CASE_ROOT}/chown.log"; }
run_sleep() { printf '%s\n' "$*" >>"${CASE_ROOT}/sleep.log"; }
run_sudo() {
  printf '%s\n' "$*" >>"${CASE_ROOT}/sudo.log"
  case "$1" in
    chown) : ;;
    chmod|mkdir|install|mv|rm|mktemp|cp) "${@}" ;;
    *) die "unexpected sudo operation: $1" ;;
  esac
}
command_available() {
  case " $FAKE_MISSING " in
    *" $1 "*) return 1 ;;
  esac
  case "$1" in
    ss) [[ "$MOCK_SS_AVAILABLE" == 1 ]] ;;
    lsof) [[ "$MOCK_LSOF_AVAILABLE" == 1 ]] ;;
    systemctl) [[ "$MOCK_SYSTEMCTL_AVAILABLE" == 1 ]] ;;
    podman|newuidmap|newgidmap|git|getent|useradd|usermod|loginctl) return 0 ;;
    *) command -v "$1" >/dev/null 2>&1 ;;
  esac
}

# A file-backed passwd/group database, so accounts that useradd creates in one
# `main` call (a subshell) are visible to the next.
run_getent() {
  local database=$1 key=$2
  awk -F: -v key="$key" '$1 == key { print; found = 1; exit } END { exit !found }' \
    "${CASE_ROOT}/${database}"
}
run_groupadd() {
  printf 'groupadd %s\n' "$*" >>"${CASE_ROOT}/usermod.log"
  printf '%s\n' "${*: -1}:x:990:" >>"${CASE_ROOT}/group"
}
run_useradd() {
  printf 'useradd %s\n' "$*" >>"${CASE_ROOT}/usermod.log"
  printf '%s\n' "${*: -1}:x:990:990::${CASE_STATE}:/usr/sbin/nologin" >>"${CASE_ROOT}/passwd"
}
run_usermod() {
  printf 'usermod %s\n' "$*" >>"${CASE_ROOT}/usermod.log"
  local user=${*: -1}
  case "$1" in
    --add-subuids) printf '%s:%s:65536\n' "$user" "${2%%-*}" >>"$(subuid_file_path)" ;;
    --add-subgids) printf '%s:%s:65536\n' "$user" "${2%%-*}" >>"$(subgid_file_path)" ;;
  esac
}

# Prerequisite probes.
cgroup_v2_available() { [[ "$FAKE_CGROUP_V2" == 1 ]]; }
user_dbus_available() { [[ "$FAKE_USER_DBUS" == 1 ]]; }
nix_command_path() {
  [[ "$FAKE_NIX" == 1 ]] || return 1
  printf '%s\n' /nix/var/nix/profiles/default/bin/nix
}
nix_daemon_active() { [[ "$FAKE_NIX_DAEMON" == 1 ]]; }
nix_flakes_enabled() { [[ "$FAKE_FLAKES" == 1 ]]; }
podman_major_version() { printf '%s\n' "$FAKE_PODMAN_MAJOR"; }
run_podman() { printf 'podman version %s.0.0\n' "$FAKE_PODMAN_MAJOR"; }
disk_free_gib() { printf '%s\n' "$FAKE_DISK_GIB"; }
cloud_hypervisor_major_version() { printf '%s\n' "$FAKE_CH_MAJOR"; }
microvm_missing_tools() { printf '%s' "$FAKE_MICROVM_MISSING"; }
group_exists() {
  if [[ "$1" == kvm ]]; then
    [[ "$FAKE_KVM_GROUP" == 1 ]]
    return
  fi
  run_getent group "$1" >/dev/null 2>&1
}

run_systemctl_system() {
  local operation=$1
  local unit=${*: -1}
  printf '%s %s\n' "$operation" "$*" >>"${CASE_ROOT}/systemctl-system.log"
  case "$operation" in
    is-active)
      if [[ "$unit" == russel-ctrl ]]; then
        if [[ "$SYSTEM_ACTIVE" == 1 ]]; then
          [[ "$*" == *--quiet* ]] || printf '%s\n' active
          return 0
        fi
        [[ "$*" == *--quiet* ]] || printf '%s\n' inactive
        return 3
      fi
      [[ "$SYSTEM_UNIT_ACTIVE" == 1 && "$unit" == russel.service ]]
      ;;
    is-enabled)
      [[ "$SYSTEM_UNIT_ACTIVE" == 1 && "$unit" == russel.service ]]
      ;;
    show)
      if [[ "$unit" == "$SYSTEM_FRAGMENT_UNIT" ]]; then
        printf '%s\n' "$SYSTEM_FRAGMENT"
      fi
      ;;
    cat) [[ "$SYSTEM_UNIT_INSTALLED" == 1 ]] ;;
    daemon-reload) : ;;
    restart)
      if [[ "$RESTART_FAIL_FIRST" == 1 && $(grep -c '^restart ' "${CASE_ROOT}/systemctl-system.log") == 1 ]]; then
        printf '%s\n' 'original restart diagnostic' >&2
        return 1
      fi
      ;;
    enable)
      if [[ "$ENABLE_RESULT" != 0 ]]; then
        printf '%s\n' 'enable diagnostic' >&2
        return "$ENABLE_RESULT"
      fi
      ;;
    *) return 1 ;;
  esac
}
run_systemctl_user() {
  local operation=$1
  printf '%s %s\n' "$operation" "$*" >>"${CASE_ROOT}/systemctl-user.log"
  case "$operation" in
    cat) [[ "$USER_UNIT_EXISTS" == 1 ]] ;;
    is-active)
      if [[ "$USER_ACTIVE" == 1 ]]; then
        [[ "$*" == *--quiet* ]] || printf '%s\n' active
        return 0
      fi
      [[ "$*" == *--quiet* ]] || printf '%s\n' inactive
      return 3
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
run_pkill() { printf '%s\n' "$*" >>"${CASE_ROOT}/pkill.log"; }
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

# The unit `host` should install: the shipped one with russel's uid (990).
expected_unit() {
  sed 's/@RUSSEL_UID@/990/g' "${SCRIPT_DIR}/russel-ctrl.service"
}

# Parser and topology refusal coverage.
new_case
expect_main_rc 2 unknown
expect_main_rc 2 --bad
expect_main_rc 2 connect
expect_main_rc 2 connect ""
expect_main_rc 2 connect -host
expect_main_rc 2 status extra
expect_main_rc 2 check extra
expect_main_rc 2 --skip-checks cli
expect_main_rc 2 --skip-checks status
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
cmp -s "${CASE_ROOT}/${UNIT_FILE_REL}" <(expected_unit) || die "downloaded unit was not rendered and installed"
[[ "$(<"$(kernel_pool_image_path)")" == 'kernel-download' ]] || die "kernel was not installed into the pool"
assert_file_mode "$CASE_STATE/_pool/kernel" 700
assert_file_mode "$CASE_STATE/_pool/kernel/bzImage" 644
grep -Fq 'russel:russel' "${CASE_ROOT}/chown.log" || die "kernel pool was not chowned to the service account"
grep -Fq 'usermod --append --groups kvm russel' "${CASE_ROOT}/usermod.log" || die "russel was not added to kvm"
assert_output_contains 'installed microVM kernel'
pass "host download mode fetches the KVM-gated kernel owned by the service account"

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
if grep -q 'kvm' "${CASE_ROOT}/usermod.log"; then
  die "russel joined kvm without /dev/kvm"
fi
pass "host skips the kernel on a container-only host"

# A fresh unit that takes a few seconds to listen is waited for, not failed.
new_case
RUSSEL_VERSION=v1.2.3
export RUSSEL_VERSION
make_release_assets v1.2.3
FAKE_KVM=0
rm -f "${MOCK_CURL_MODE_FILE}.count"
set_curl_mode slow-start-401
expect_main_rc 0 host
[[ -s "${CASE_ROOT}/sleep.log" ]] || die "host did not wait for the unit to listen"
rm -f "${MOCK_CURL_MODE_FILE}.count"
pass "host waits for a slow-starting ctrl instead of reporting it unhealthy"

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

# Topology refusals: not root, not Linux, NixOS, a NixOS-module unit, a
# Nix-store unit, and the old per-user install.
new_case
FAKE_UID=1000
expect_main_rc 2 host
assert_output_contains 'host must run as root'
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
SYSTEM_FRAGMENT_UNIT=russel-ctrl.service
expect_main_rc 2 host
new_case
make_host_binary legacy-binary
mkdir -p "$CASE_ROOT/home-alice/.config/systemd/user"
: >"$CASE_ROOT/home-alice/.config/systemd/user/russel-ctrl.service"
expect_main_rc 2 host
assert_output_contains 'found the old per-user install'
assert_output_contains 'systemctl --user disable --now russel-ctrl'
assert_output_contains '--take-state-ownership host'
[[ ! -e "${CASE_ROOT}/usermod.log" ]] || die "legacy refusal changed accounts"
pass "root, OS, NixOS, module-unit, Nix-store, and legacy per-user refusals"

# `check` reports every problem with a fix, and `host` stops on failures.
new_case
expect_main_rc 0 check
assert_output_contains 'Ready.'
new_case
FAKE_MISSING='podman newuidmap'
FAKE_FLAKES=0
FAKE_USER_DBUS=0
expect_main_rc 1 check
assert_output_contains 'podman is not installed'
assert_output_contains 'sudo apt install podman'
assert_output_contains 'sudo apt install uidmap'
assert_output_contains 'sudo apt install dbus-user-session'
assert_output_contains "experimental-features = nix-command flakes"
assert_output_contains '4 problem(s) must be fixed'
new_case
FAKE_NIX=0
expect_main_rc 1 check
assert_output_contains 'no multi-user Nix'
assert_output_contains '--daemon'
new_case
FAKE_PODMAN_MAJOR=3
FAKE_CGROUP_V2=0
FAKE_NIX_DAEMON=0
expect_main_rc 1 check
assert_output_contains 'podman 4 or newer is required (found: 3)'
assert_output_contains 'cgroup v2 is required'
assert_output_contains 'nix-daemon is not running'
new_case
FAKE_KVM=1
FAKE_CH_MAJOR=51
FAKE_DISK_GIB=4
expect_main_rc 0 check
assert_output_contains 'cloud-hypervisor v51 hangs when cpus > 1'
assert_output_contains 'only 4 GiB free for /nix'
assert_output_contains 'Ready, with 3 warning(s).'
new_case
FAKE_KVM=1
FAKE_MICROVM_MISSING='passt'
expect_main_rc 0 check
assert_output_contains 'microVMs (optional): missing passt'
pass "check reports failures with fixes and optional microVM warnings"

new_case
make_host_binary checked-binary
FAKE_FLAKES=0
set_curl_mode 401
expect_main_rc 1 host
assert_output_contains 'fix the problems above'
[[ ! -e "$CASE_DEST" ]] || die "host installed a binary despite failed checks"
[[ ! -e "${CASE_ROOT}/usermod.log" ]] || die "host changed accounts despite failed checks"
expect_main_rc 0 --skip-checks host
assert_output_contains 'skipping host checks'
[[ -e "$CASE_DEST" ]] || die "--skip-checks did not install"
pass "host stops on failed checks unless --skip-checks"

# First host install: the service account, its id ranges, linger, the operator
# in the group, the token file, the rendered system unit, and the binary.
new_case
make_host_binary first-binary
set_curl_mode 401
expect_main_rc 0 host
ENV_FILE="$(env_file_path)"
UNIT_FILE="${CASE_ROOT}/${UNIT_FILE_REL}"
assert_file_contains "${CASE_ROOT}/usermod.log" 'groupadd --system russel'
assert_file_contains "${CASE_ROOT}/usermod.log" "useradd --system --gid russel --home-dir ${CASE_STATE} --no-create-home"
assert_file_contains "${CASE_ROOT}/usermod.log" 'usermod --add-subuids 165536-231071 russel'
assert_file_contains "${CASE_ROOT}/usermod.log" 'usermod --add-subgids 165536-231071 russel'
assert_file_contains "${CASE_ROOT}/usermod.log" 'usermod --append --groups russel alice'
assert_file_contains "${CASE_ROOT}/loginctl.log" 'enable-linger russel'
[[ "$(<"$ENV_FILE")" == 'RUSSEL_API_TOKEN=known-fake-token' ]] || die "unexpected token file"
assert_file_mode "$ENV_FILE" 640
assert_file_mode "$(env_dir_path)" 750
grep -Fq "root:russel -- $(env_dir_path)" "${CASE_ROOT}/chown.log" || die "env dir was not given to root:russel"
assert_file_mode "$CASE_STATE" 700
grep -Fq "russel:russel -- ${CASE_STATE}" "${CASE_ROOT}/chown.log" || die "state dir was not given to russel"
cmp -s "$UNIT_FILE" <(expected_unit) || die "unit was not rendered with russel's uid"
if grep -q '@RUSSEL_UID@' "$UNIT_FILE"; then
  die "unit still has the uid placeholder"
fi
cmp -s "$CASE_DEST" "${RELEASE}/russel-ctrl" || die "host binary was not installed"
[[ "$(call_count "${CASE_ROOT}/systemctl-system.log" enable)" == 1 ]] || die "first install enable call missing"
[[ "$(call_count "${CASE_ROOT}/systemctl-system.log" daemon-reload)" == 1 ]] || die "first install daemon-reload missing"
[[ ! -e "${CASE_ROOT}/systemctl-user.log" ]] || die "host touched a user manager"
assert_output_contains 'alice is now in the russel group'
assert_output_contains "--token-file ${ENV_FILE}"
assert_output_lacks known-fake-token
pass "first host install: account, id ranges, linger, token, unit, binary, enable"

# Re-running keeps the account and its id ranges instead of adding more.
new_case
make_host_binary rerun-binary
set_curl_mode 401
expect_main_rc 0 host
SYSTEM_ACTIVE=1
expect_main_rc 0 host
[[ "$(grep -c '^useradd ' "${CASE_ROOT}/usermod.log")" == 1 ]] || die "rerun created the account again"
[[ "$(grep -c -- '--add-subuids' "${CASE_ROOT}/usermod.log")" == 1 ]] || die "rerun added another subuid range"
[[ "$(grep -c '^russel:' "${CASE_ROOT}/etc/subuid")" == 1 ]] || die "subuid file has a duplicate range"
pass "rerun is idempotent for the service account"

# Moving from the old per-user install keeps the operator's token.
new_case
make_host_binary migrated-binary
mkdir -p "$CASE_ROOT/home-alice/.config/russel"
printf '%s\n' 'RUSSEL_API_TOKEN=legacy-token-value' >"$CASE_ROOT/home-alice/.config/russel/env"
set_curl_mode 401
expect_main_rc 0 host
[[ "$(<"$(env_file_path)")" == 'RUSSEL_API_TOKEN=legacy-token-value' ]] || die "legacy token was not carried over"
assert_output_lacks legacy-token-value
pass "legacy token carried into /etc/russel/env"

# A token file anyone can read, or one not owned by root, is refused.
new_case
make_host_binary loose-binary
mkdir -p "$(env_dir_path)"
printf '%s\n' 'RUSSEL_API_TOKEN=loose-token' >"$(env_file_path)"
chmod 644 "$(env_file_path)"
set_curl_mode 401
expect_main_rc 1 host
assert_output_contains 'world-accessible'
chmod 640 "$(env_file_path)"
FAKE_ENV_OWNER=1000:990
expect_main_rc 1 host
assert_output_contains 'must be owned by root'
pass "token file permission and ownership checks"

# A differing but valid unit is preserved, along with the token and state.
new_case
make_host_binary old-binary
set_curl_mode 401
expect_main_rc 0 host
ENV_FILE="$(env_file_path)"
UNIT_FILE="${CASE_ROOT}/${UNIT_FILE_REL}"
printf '%s\n' 'RUSSEL_API_TOKEN=preserved-token' >"$ENV_FILE"
chmod 640 "$ENV_FILE"
printf '%s\n' '# operator edit' >>"$UNIT_FILE"
make_host_binary new-binary
SYSTEM_ACTIVE=1
expect_main_rc 0 host
[[ "$(<"$ENV_FILE")" == 'RUSSEL_API_TOKEN=preserved-token' ]] || die "existing token changed"
grep -Fq '# operator edit' "$UNIT_FILE" || die "existing unit changed"
[[ "$(call_count "${CASE_ROOT}/systemctl-system.log" restart)" == 1 ]] || die "active unit was not restarted"
cmp -s "$CASE_DEST.previous" <(printf '%s\n' old-binary) || die "previous binary copy missing"
assert_output_lacks preserved-token
pass "existing token, unit, state, and binary backup preservation"

# Force replaces a differing valid unit and reloads it.
new_case
make_host_binary force-binary
set_curl_mode 401
expect_main_rc 0 host
UNIT_FILE="${CASE_ROOT}/${UNIT_FILE_REL}"
printf '%s\n' '# operator edit' >>"$UNIT_FILE"
SYSTEM_ACTIVE=1
expect_main_rc 0 --force-unit host
cmp -s "$UNIT_FILE" <(expected_unit) || die "force did not replace unit"
[[ "$(call_count "${CASE_ROOT}/systemctl-system.log" daemon-reload)" == 2 ]] || die "force did not daemon-reload"
pass "force-unit replacement"

# A unit that no longer runs as russel is refused rather than started.
new_case
make_host_binary user-binary
set_curl_mode 401
expect_main_rc 0 host
UNIT_FILE="${CASE_ROOT}/${UNIT_FILE_REL}"
sed -i 's/^User=russel$/User=root/' "$UNIT_FILE"
expect_main_rc 1 host
assert_output_contains 'fails required User=russel'
pass "unit validation requires User=russel"

# State ownership is refused unless explicitly taken; taking it is recursive.
new_case
make_host_binary state-binary
mkdir -p "$CASE_STATE"
FAKE_STATE_OWNER=1000:1000
set_curl_mode 401
expect_main_rc 1 host
assert_output_contains 'use --take-state-ownership'
if grep -Fq -- "-R russel:russel" "${CASE_ROOT}/chown.log" 2>/dev/null; then
  die "state refusal chowned"
fi
expect_main_rc 0 --take-state-ownership host
grep -Fq -- "-R russel:russel -- ${CASE_STATE}" "${CASE_ROOT}/chown.log" || die "takeover did not chown recursively"
pass "state ownership refusal and recursive takeover"

# A failed active-unit restart restores the old binary and retries restart once.
new_case
make_host_binary old-active-binary
set_curl_mode 401
expect_main_rc 0 host
make_host_binary new-active-binary
SYSTEM_ACTIVE=1
RESTART_FAIL_FIRST=1
expect_main_rc 1 host
[[ "$(<"$CASE_DEST")" == 'old-active-binary' ]] || die "binary rollback failed"
[[ "$(<"$CASE_DEST.previous")" == 'old-active-binary' ]] || die "rollback backup changed"
[[ "$(call_count "${CASE_ROOT}/systemctl-system.log" restart)" == 2 ]] || die "restoration restart was not attempted exactly once"
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
SYSTEM_ACTIVE=1
RESTART_FAIL_FIRST=1
expect_main_rc 1 host
[[ "$(<"$CASE_DEST")" == 'ctrl-download' ]] || die "binary rollback failed"
[[ "$(<"$CASE_KERNEL")" == 'kernel-download' ]] \
  || die "failed upgrade left the replacement kernel in the pool"
[[ "$(<"${CASE_KERNEL}.previous")" == 'kernel-download' ]] || die "kernel backup missing"
pass "failed upgrade restores the previous pool kernel with the binary"

# The pre-release unit had PrivateTmp=yes: rootless Podman's pause process pinned
# its private /tmp and every container failed (#524). Upgrading replaces that
# unit without --force-unit and resets the pause process before restarting.
new_case
make_host_binary old-binary
set_curl_mode 401
expect_main_rc 0 host
UNIT_FILE="${CASE_ROOT}/${UNIT_FILE_REL}"
printf '%s\n' 'PrivateTmp=yes' >>"$UNIT_FILE"
make_host_binary new-binary
SYSTEM_ACTIVE=1
expect_main_rc 0 host
assert_output_contains 'breaks rootless Podman'
if grep -q '^PrivateTmp=' "$UNIT_FILE"; then die "pre-release unit was not replaced"; fi
grep -Fq -- '-u russel -x catatonit' "${CASE_ROOT}/pkill.log" || die "pause process was not reset"
[[ "$(call_count "${CASE_ROOT}/systemctl-system.log" restart)" == 1 ]] || die "unit was not restarted"
pass "upgrade replaces the pre-release PrivateTmp unit and resets Podman's pause process"

# A stopped pre-release unit still leaves the pause process behind (KillMode=process).
new_case
make_host_binary old-binary
set_curl_mode 401
expect_main_rc 0 host
UNIT_FILE="${CASE_ROOT}/${UNIT_FILE_REL}"
printf '%s\n' 'PrivateTmp=yes' >>"$UNIT_FILE"
make_host_binary new-binary
expect_main_rc 0 host
grep -Fq -- '-u russel -x catatonit' "${CASE_ROOT}/pkill.log" || die "pause process was not reset for a stopped unit"
pass "upgrade resets Podman's pause process when the pre-release unit is stopped"

# An operator-edited unit without PrivateTmp is still kept, and Podman is left alone.
new_case
make_host_binary old-binary
set_curl_mode 401
expect_main_rc 0 host
UNIT_FILE="${CASE_ROOT}/${UNIT_FILE_REL}"
printf '%s\n' '# operator edit' >>"$UNIT_FILE"
make_host_binary new-binary
SYSTEM_ACTIVE=1
expect_main_rc 0 host
grep -Fq '# operator edit' "$UNIT_FILE" || die "operator unit was replaced"
[[ ! -s "${CASE_ROOT}/pkill.log" ]] || die "pause process reset without a PrivateTmp unit"
pass "operator-edited units without PrivateTmp are kept and Podman is untouched"

# Linger is what gives the account /run/user and a user bus, so failing to
# enable it fails the install before anything starts.
new_case
make_host_binary linger-binary
set_curl_mode 200-valid
LINGER_FAIL=1
expect_main_rc 1 host
assert_output_contains 'cannot enable linger for russel'
[[ "$(call_count "${CASE_ROOT}/systemctl-system.log" enable)" == 0 ]] || die "unit started without linger"
pass "linger failure stops the install"

# Subuid allocation starts after every existing range, never below 100000.
new_case
printf '%s\n' 'alice:100000:65536' 'bob:165536:65536' 'odd:300000:10' >"$CASE_ROOT/etc/subuid"
[[ "$(next_subid_start "$CASE_ROOT/etc/subuid")" == 300010 ]] || die "subuid start after ranges is wrong"
: >"$CASE_ROOT/etc/subuid"
[[ "$(next_subid_start "$CASE_ROOT/etc/subuid")" == 100000 ]] || die "subuid start for an empty file is wrong"
[[ "$(next_subid_start "$CASE_ROOT/etc/missing")" == 100000 ]] || die "subuid start for a missing file is wrong"
pass "subuid range allocation"

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

new_case
SYSTEM_UNIT_INSTALLED=1
SYSTEM_ACTIVE=1
set_curl_mode 401
expect_main_rc 0 status
assert_output_contains 'unit: russel-ctrl.service active'
new_case
USER_UNIT_EXISTS=1
USER_ACTIVE=1
set_curl_mode 401
expect_main_rc 0 status
assert_output_contains 'unit: old per-user russel-ctrl.service active'
new_case
set_curl_mode 401
expect_main_rc 0 status
assert_output_contains 'unit: not installed on this machine'
pass "status reports the system unit and flags the old per-user unit"

echo "all installer tests passed"
