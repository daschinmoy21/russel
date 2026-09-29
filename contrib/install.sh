#!/usr/bin/env bash
# Install Russel binaries and manage a non-NixOS control plane.
#
#   ./contrib/install.sh cli
#   ./contrib/install.sh ctrl
#   ./contrib/install.sh all
#   ./contrib/install.sh check
#   sudo ./contrib/install.sh [--force-unit] [--take-state-ownership] [--skip-checks] host
#   ./contrib/install.sh connect <user@host-or-ssh-alias>
#   ./contrib/install.sh status
#
# `host` runs as root. It creates the unprivileged `russel` service account
# (subuid/subgid range, linger), installs a system unit that runs russel-ctrl
# as that account, and adds the invoking sudo user to the `russel` group so it
# can read the API token. Nothing runs as root after the install.
#
# Without a local target/release build the installer downloads release assets
# for RUSSEL_VERSION and verifies each one against the release SHA256SUMS:
#
#   curl -fsSL <raw install.sh url> | sudo RUSSEL_VERSION=v0.1.0 bash -s -- host
#
# RUSSEL_RELEASE_BASE overrides the artifact base URL. Private GitHub
# releases also need RUSSEL_GITHUB_TOKEN, GH_TOKEN, or GITHUB_TOKEN.
# The script is safe to pipe on stdin: it never assumes BASH_SOURCE
# points at a checkout root.
set -euo pipefail

# Debian and Ubuntu keep useradd/usermod in /usr/sbin, which is not on a
# regular user's PATH. `check` runs without sudo, so append the sbin dirs
# (sudo's secure_path has them already) instead of reporting them missing.
for sbin in /usr/sbin /sbin; do
  case ":${PATH}:" in
    *":${sbin}:"*) ;;
    *) PATH="${PATH}:${sbin}" ;;
  esac
done

SCRIPT_PATH="${BASH_SOURCE[0]:-}"
ROOT=
if [[ -n "$SCRIPT_PATH" && -f "$SCRIPT_PATH" ]]; then
  ROOT="$(cd -- "$(dirname -- "$SCRIPT_PATH")/.." && pwd)"
fi
RELEASE="${ROOT:+$ROOT/}target/release"
PROBE_TIMEOUT_SECONDS=2
PROBE_ATTEMPTS=2
HOST_READY_SECONDS=30
DOWNLOAD_TIMEOUT_SECONDS=600
RELEASE_BASE="${RUSSEL_RELEASE_BASE:-https://github.com/daschinmoy21/russel/releases/download}"
RELEASE_ARCH=
DOWNLOAD_DIR=
DOWNLOAD_SUMS=
DOWNLOAD_DASHBOARD_SRC=
UNIT_SOURCE_OVERRIDE=
FORCE_UNIT=0
TAKE_STATE_OWNERSHIP=0
SKIP_CHECKS=0

usage() {
  echo "usage: $0 cli|ctrl|all|check" >&2
  echo "       sudo $0 [--force-unit] [--take-state-ownership] [--skip-checks] host" >&2
  echo "       $0 connect <user@host-or-ssh-alias>" >&2
  echo "       $0 status" >&2
  return 2
}

# These wrappers are deliberately small so the installer can be exercised
# without root, a live systemd manager, an SSH server, or a running ctrl.
run_install() { install "$@"; }
run_mkdir() { mkdir "$@"; }
run_chmod() { chmod "$@"; }
run_chown() { chown "$@"; }
run_mv() { mv "$@"; }
run_mv_noclobber() { mv -n "$@"; }
run_rm() { rm "$@"; }
run_cmp() { cmp "$@"; }
run_stat() { stat "$@"; }
run_mktemp() { mktemp "$@"; }
run_sudo() { sudo "$@"; }
run_systemctl_user() { systemctl --user "$@"; }
run_systemctl_system() { systemctl "$@"; }
run_loginctl() { loginctl "$@"; }
run_getent() { getent "$@"; }
run_useradd() { useradd "$@"; }
run_usermod() { usermod "$@"; }
run_groupadd() { groupadd "$@"; }
run_podman() { podman "$@"; }
run_nix() { nix "$@"; }
run_df_avail_kib() { df -Pk -- "$1" | awk 'NR == 2 { print $4 }'; }
run_ssh() { ssh "$@"; }
run_pkill() { pkill "$@"; }
run_with_timeout() { timeout "$@"; }
run_sleep() { sleep "$@"; }
run_curl() { run_with_timeout "${PROBE_TIMEOUT_SECONDS}s" curl "$@"; }
run_openssl() { openssl "$@"; }
run_ss() { ss -ltnp 'sport = :7878'; }
run_lsof() { lsof -nP -iTCP:7878 -sTCP:LISTEN; }
run_russel() { russel origin; }
run_tar() { tar "$@"; }
run_sha256sum() { sha256sum "$@"; }
run_shasum() { shasum "$@"; }
run_curl_download() { run_with_timeout "${DOWNLOAD_TIMEOUT_SECONDS}s" curl "$@"; }

command_available() {
  command -v "$1" >/dev/null 2>&1
}

current_user() {
  id -un
}

current_group() {
  id -gn
}

current_uid() {
  id -u
}

current_gid() {
  id -g
}

host_os_name() {
  uname -s
}

machine_arch() {
  uname -m
}

kvm_available() {
  [[ -e /dev/kvm ]]
}

# MicroVM tools missing from PATH, space-separated (empty when all present).
microvm_missing_tools() {
  local tool missing=()
  for tool in cloud-hypervisor virtiofsd passt; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
  done
  printf '%s' "${missing[*]}"
}

# Published assets are x86_64-only today. Local builds on any architecture
# keep working through the target/release path.
release_arch() {
  local arch
  arch="$(machine_arch)"
  case "$arch" in
    x86_64|amd64) printf '%s\n' x86_64 ;;
    *)
      echo "no published release for architecture ${arch}; build locally with: cargo build --release" >&2
      return 1
      ;;
  esac
}

nixos_marker_present() {
  [[ -e /etc/NIXOS || -L /etc/NIXOS ]]
}

os_release_is_nixos() {
  local release_file
  for release_file in /etc/os-release /usr/lib/os-release; do
    [[ -r "$release_file" ]] || continue
    if awk -F= '
      $1 == "ID" {
        value = $2
        gsub(/^"|"$/, "", value)
        if (tolower(value) == "nixos") found = 1
      }
      END { exit !found }
    ' "$release_file"; then
      return 0
    fi
  done
  return 1
}

host_binary_path() {
  printf '%s\n' /usr/local/bin/russel-ctrl
}

state_dir_path() {
  printf '%s\n' /var/lib/russel
}

kernel_pool_dir_path() {
  printf '%s\n' "$(state_dir_path)/_pool/kernel"
}

kernel_pool_image_path() {
  printf '%s\n' "$(kernel_pool_dir_path)/bzImage"
}

operator_home() {
  [[ -n "${HOME:-}" ]] || return 1
  printf '%s\n' "$HOME"
}

# The unprivileged account russel-ctrl and every workload run as. Only `host`
# (running as root) creates it; everything after the install runs as it.
service_user() {
  printf '%s\n' russel
}

service_group() {
  printf '%s\n' russel
}

system_unit_path() {
  printf '%s\n' /etc/systemd/system/russel-ctrl.service
}

env_dir_path() {
  printf '%s\n' /etc/russel
}

env_file_path() {
  printf '%s\n' "$(env_dir_path)/env"
}

subuid_file_path() {
  printf '%s\n' /etc/subuid
}

subgid_file_path() {
  printf '%s\n' /etc/subgid
}

nix_daemon_profile_bin() {
  printf '%s\n' /nix/var/nix/profiles/default/bin
}

# The person who ran `sudo ./install.sh host`. They join the service group so
# they can read the token; empty when root ran the installer directly.
operator_user() {
  local user=${SUDO_USER:-}
  [[ -n "$user" && "$user" != root ]] || return 1
  printf '%s\n' "$user"
}

# Home directory of a local account, from the passwd database.
account_home() {
  run_getent passwd "$1" | awk -F: 'NR == 1 { print $6 }'
}

account_uid() {
  run_getent passwd "$1" | awk -F: 'NR == 1 { print $3 }'
}

account_exists() {
  run_getent passwd "$1" >/dev/null 2>&1
}

group_exists() {
  run_getent group "$1" >/dev/null 2>&1
}

# Paths of the per-user install that `host` used before the russel account.
legacy_user_unit_path() {
  local home
  home="$(account_home "$1")" || return 1
  [[ -n "$home" ]] || return 1
  printf '%s\n' "${home}/.config/systemd/user/russel-ctrl.service"
}

legacy_env_file_path() {
  local home
  home="$(account_home "$1")" || return 1
  [[ -n "$home" ]] || return 1
  printf '%s\n' "${home}/.config/russel/env"
}

# How to run this script again, for messages: the checkout path, or the
# pipe form when it came from curl.
installer_command() {
  if [[ -n "$ROOT" ]]; then
    printf '%s\n' "sudo ${ROOT}/contrib/install.sh"
  else
    printf '%s\n' "curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh | sudo RUSSEL_VERSION=<version> bash -s --"
  fi
}

unit_source_path() {
  if [[ -n "$UNIT_SOURCE_OVERRIDE" ]]; then
    printf '%s\n' "$UNIT_SOURCE_OVERRIDE"
  else
    printf '%s\n' "${ROOT}/contrib/russel-ctrl.service"
  fi
}

path_mode() {
  run_stat -c '%a' -- "$1"
}

path_owner() {
  run_stat -c '%u:%g' -- "$1"
}

require_command() {
  local command_name
  for command_name in "$@"; do
    if ! command_available "$command_name"; then
      echo "missing required command: ${command_name}" >&2
      return 2
    fi
  done
}

release_version() {
  local version=${RUSSEL_VERSION:-}
  if [[ -z "$version" ]]; then
    return 1
  fi
  case "$version" in
    v*) printf '%s\n' "$version" ;;
    *) printf 'v%s\n' "$version" ;;
  esac
}

local_release_has() {
  [[ -n "$RELEASE" && -x "${RELEASE}/$1" ]]
}

cleanup_download_dir() {
  if [[ -n "$DOWNLOAD_DIR" && -d "$DOWNLOAD_DIR" ]]; then
    run_rm -rf -- "$DOWNLOAD_DIR" 2>/dev/null || true
  fi
}

release_asset_url() {
  printf '%s/%s/%s\n' "${RELEASE_BASE%/}" "$1" "$2"
}

github_release_token() {
  printf '%s\n' "${RUSSEL_GITHUB_TOKEN:-${GH_TOKEN:-${GITHUB_TOKEN:-}}}"
}

fetch_release_file() {
  local version=$1 asset=$2 destination=$3 url token
  url="$(release_asset_url "$version" "$asset")"
  token="$(github_release_token)"
  if [[ -n "$token" ]]; then
    if ! run_curl_download --fail --location --silent --show-error \
      -H "Authorization: Bearer ${token}" \
      --output "$destination" -- "$url"; then
      echo "cannot download ${url}" >&2
      return 1
    fi
  elif ! run_curl_download --fail --location --silent --show-error \
    --output "$destination" -- "$url"; then
    echo "cannot download ${url}" >&2
    return 1
  fi
}

sha256_of_file() {
  local file=$1
  if command_available sha256sum; then
    run_sha256sum -- "$file" | awk '{ print $1 }'
  elif command_available shasum; then
    run_shasum -a 256 -- "$file" | awk '{ print $1 }'
  elif command_available openssl; then
    run_openssl dgst -sha256 "$file" | awk '{ print $NF }'
  else
    echo "no sha256 tool available (need sha256sum, shasum, or openssl)" >&2
    return 1
  fi
}

sha256sums_entry() {
  local asset=$1 sums=$2 entry
  [[ -f "$sums" ]] || return 1
  entry="$(awk -v asset="$asset" '$2 == asset { print $1; exit }' "$sums")"
  [[ -n "$entry" ]] || return 1
  printf '%s\n' "$entry"
}

verify_release_asset() {
  local asset=$1 file=$2 expected actual
  if ! expected="$(sha256sums_entry "$asset" "$DOWNLOAD_SUMS")"; then
    echo "${asset} is missing from SHA256SUMS" >&2
    return 1
  fi
  if ! actual="$(sha256_of_file "$file")"; then
    echo "cannot hash ${file}" >&2
    return 1
  fi
  if [[ "$expected" != "$actual" ]]; then
    echo "checksum mismatch for ${asset}" >&2
    return 1
  fi
}

fetch_release_sums() {
  local version=$1 sums="${DOWNLOAD_DIR}/SHA256SUMS"
  if ! fetch_release_file "$version" SHA256SUMS "$sums"; then
    echo "cannot download SHA256SUMS for ${version}" >&2
    return 1
  fi
  DOWNLOAD_SUMS=$sums
}

release_binary_asset() {
  case "$1" in
    russel) printf 'russel-%s-%s\n' "$2" "$RELEASE_ARCH" ;;
    russel-ctrl) printf 'russel-ctrl-%s-%s\n' "$2" "$RELEASE_ARCH" ;;
    *) return 1 ;;
  esac
}

fetch_release_binary() {
  local binary=$1 version=$2 asset destination
  asset="$(release_binary_asset "$binary" "$version")" || return 1
  destination="${DOWNLOAD_DIR}/${binary}"
  if ! fetch_release_file "$version" "$asset" "$destination"; then
    return 1
  fi
  if ! verify_release_asset "$asset" "$destination"; then
    run_rm -f -- "$destination" 2>/dev/null || true
    return 1
  fi
  run_chmod 0755 -- "$destination"
}

# Download mode runs when RUSSEL_VERSION is set or the required local build is
# absent. Success points RELEASE at a verified scratch directory.
prepare_release() {
  local binary=$1 version arch
  if [[ -z "${RUSSEL_VERSION:-}" ]] && local_release_has "$binary"; then
    return 0
  fi
  if ! version="$(release_version)"; then
    echo "no ${RELEASE}/${binary} and RUSSEL_VERSION is unset" >&2
    echo "set RUSSEL_VERSION (for example v0.1.0), or build with: cargo build --release" >&2
    return 1
  fi
  require_command curl || return $?
  if ! arch="$(release_arch)"; then
    return 1
  fi

  # `all` reuses one scratch directory and one SHA256SUMS for both binaries.
  if [[ -z "$DOWNLOAD_DIR" ]]; then
    if ! DOWNLOAD_DIR="$(run_mktemp -d "${TMPDIR:-/tmp}/russel-release.XXXXXX")"; then
      echo "cannot create download directory" >&2
      return 1
    fi
    trap cleanup_download_dir EXIT
    RELEASE="$DOWNLOAD_DIR"
    RELEASE_ARCH=$arch
    fetch_release_sums "$version" || return 1
  fi
  if [[ -x "${DOWNLOAD_DIR}/${binary}" ]]; then
    return 0
  fi
  fetch_release_binary "$binary" "$version" || return 1
}

prepare_release_dashboard() {
  local version asset tarball directory
  if [[ -z "$DOWNLOAD_DIR" || -n "${RUSSEL_DASHBOARD_SRC:-}" || -n "$DOWNLOAD_DASHBOARD_SRC" ]]; then
    return 0
  fi
  if ! version="$(release_version)"; then
    return 1
  fi
  asset="russel-dashboard-${version}.tar.gz"
  tarball="${DOWNLOAD_DIR}/${asset}"
  directory="${DOWNLOAD_DIR}/dashboard"
  fetch_release_file "$version" "$asset" "$tarball" || return 1
  verify_release_asset "$asset" "$tarball" || return 1
  if ! run_mkdir -p -- "$directory"; then
    echo "cannot create dashboard directory" >&2
    return 1
  fi
  if ! run_tar -xzf "$tarball" -C "$directory"; then
    echo "cannot extract ${asset}" >&2
    return 1
  fi
  DOWNLOAD_DASHBOARD_SRC=$directory
}

prepare_release_unit() {
  local version asset destination
  if [[ -z "$DOWNLOAD_DIR" || -n "$UNIT_SOURCE_OVERRIDE" ]]; then
    return 0
  fi
  if ! version="$(release_version)"; then
    return 1
  fi
  asset="russel-ctrl-${version}.service"
  destination="${DOWNLOAD_DIR}/${asset}"
  fetch_release_file "$version" "$asset" "$destination" || return 1
  verify_release_asset "$asset" "$destination" || return 1
  UNIT_SOURCE_OVERRIDE=$destination
}

host_system_unit_active_or_enabled() {
  local unit=$1
  if run_systemctl_system is-active --quiet "$unit" >/dev/null 2>&1; then
    return 0
  fi
  run_systemctl_system is-enabled --quiet "$unit" >/dev/null 2>&1
}

host_system_unit_fragment() {
  local unit=$1
  run_systemctl_system show -p FragmentPath --value "$unit" 2>/dev/null || true
}

check_host_topology() {
  local unit fragment operator legacy_unit

  if [[ "$(current_uid)" != 0 ]]; then
    echo "host must run as root; it creates the russel service account and a system unit" >&2
    echo "run: $(installer_command) host" >&2
    return 2
  fi
  if [[ "$(host_os_name)" != Linux ]]; then
    echo "host supports Linux only" >&2
    return 2
  fi
  if nixos_marker_present || os_release_is_nixos; then
    echo "host refuses NixOS; use services.russel instead" >&2
    return 2
  fi

  require_command install systemctl openssl getent useradd usermod loginctl || return $?

  # The NixOS module's unit is russel.service; a Nix-managed russel-ctrl.service
  # also belongs to services.russel, not to this installer.
  if host_system_unit_active_or_enabled russel.service; then
    echo "host refuses system-managed russel.service; use services.russel instead" >&2
    return 2
  fi
  for unit in russel.service russel-ctrl.service; do
    fragment="$(host_system_unit_fragment "$unit")"
    if [[ "$fragment" == /nix/store/* ]]; then
      echo "host refuses Nix-managed ${unit}; use services.russel instead" >&2
      return 2
    fi
  done

  # Before the russel account existed, `host` installed a user unit under the
  # operator's own account. Two control planes would fight over :7878 and
  # /var/lib/russel, so the old one has to go first.
  if operator="$(operator_user)" \
    && legacy_unit="$(legacy_user_unit_path "$operator")" \
    && [[ -e "$legacy_unit" || -L "$legacy_unit" ]]; then
    print_legacy_migration "$operator" "$legacy_unit" >&2
    return 2
  fi
}

print_legacy_migration() {
  local operator=$1 legacy_unit=$2
  cat <<EOF
found the old per-user install: ${legacy_unit}
russel-ctrl now runs as a dedicated 'russel' account. To move over, as ${operator}:
  1. russel ps, then russel destroy each service (the new account cannot adopt
     containers that ${operator}'s Podman started)
  2. systemctl --user disable --now russel-ctrl
  3. rm ${legacy_unit}
Then run again: $(installer_command) --take-state-ownership host
Your token in ~/.config/russel/env and the secrets in /var/lib/russel are kept.
EOF
}

# --- Prerequisite checks (`check`, and the first step of `host`) -------------

CHECK_FAILURES=0
CHECK_WARNINGS=0

check_ok() { printf '  ok    %s\n' "$1"; }

check_fail() {
  CHECK_FAILURES=$((CHECK_FAILURES + 1))
  printf '  FAIL  %s\n' "$1"
  shift
  local line
  for line in "$@"; do
    printf '        %s\n' "$line"
  done
}

check_warn() {
  CHECK_WARNINGS=$((CHECK_WARNINGS + 1))
  printf '  warn  %s\n' "$1"
  shift
  local line
  for line in "$@"; do
    printf '        %s\n' "$line"
  done
}

# Nix as the service will see it: the daemon profile, never a user profile.
nix_command_path() {
  local candidate
  candidate="$(nix_daemon_profile_bin)/nix"
  if [[ -x "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi
  return 1
}

nix_daemon_active() {
  run_systemctl_system is-active --quiet nix-daemon.socket >/dev/null 2>&1 \
    || run_systemctl_system is-active --quiet nix-daemon.service >/dev/null 2>&1
}

# Flakes as the service account sees them: system nix.conf only. HOME and
# XDG_CONFIG_HOME point nowhere so a per-user nix.conf cannot hide a gap.
nix_flakes_enabled() {
  local nix_path=$1 features
  features="$(HOME=/nonexistent XDG_CONFIG_HOME=/nonexistent run_nix_at "$nix_path" \
    config show experimental-features 2>/dev/null)" \
    || features="$(HOME=/nonexistent XDG_CONFIG_HOME=/nonexistent run_nix_at "$nix_path" \
      show-config 2>/dev/null | awk -F' = ' '$1 == "experimental-features" { print $2 }')" \
    || return 1
  [[ " ${features} " == *" flakes "* && " ${features} " == *" nix-command "* ]]
}

run_nix_at() {
  local nix_path=$1
  shift
  "$nix_path" "$@"
}

podman_major_version() {
  run_podman --version 2>/dev/null | awk '{ split($NF, v, "."); print v[1]; exit }'
}

cgroup_v2_available() {
  [[ -f /sys/fs/cgroup/cgroup.controllers ]]
}

# Rootless Podman under a system unit uses the systemd cgroup manager through
# the service account's user bus. Without a user bus it falls back to cgroupfs,
# and `podman run --memory` fails.
user_dbus_available() {
  local dir
  for dir in /usr/lib/systemd/user /lib/systemd/user /etc/systemd/user; do
    [[ -e "${dir}/dbus.socket" ]] && return 0
  done
  return 1
}

cloud_hypervisor_major_version() {
  cloud-hypervisor --version 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^v[0-9]/) { sub(/^v/, "", $i); split($i, v, /[.-]/); print v[1]; exit } }'
}

disk_free_gib() {
  local path=$1 kib
  while [[ ! -e "$path" && "$path" != / ]]; do
    path="$(dirname -- "$path")"
  done
  kib="$(run_df_avail_kib "$path" 2>/dev/null)" || return 1
  [[ "$kib" =~ ^[0-9]+$ ]] || return 1
  printf '%s\n' $((kib / 1024 / 1024))
}

# Every check the host needs before `host` changes anything. Failures block the
# install; warnings are for optional features. Prints a fix for each problem,
# written for Debian and Ubuntu.
check_host_prereqs() {
  local nix_path major free
  CHECK_FAILURES=0
  CHECK_WARNINGS=0
  echo "Checking this host for Russel:"

  if [[ "$(host_os_name)" == Linux ]] && command_available systemctl; then
    check_ok "Linux with systemd"
  else
    check_fail "Linux with systemd is required"
  fi

  if cgroup_v2_available; then
    check_ok "cgroup v2"
  else
    check_fail "cgroup v2 is required for rootless Podman resource limits" \
      "Debian 11+ and Ubuntu 21.10+ use it by default; boot with systemd.unified_cgroup_hierarchy=1"
  fi

  if command_available podman; then
    major="$(podman_major_version)"
    if [[ "$major" =~ ^[0-9]+$ ]] && (( major >= 4 )); then
      check_ok "podman $(run_podman --version 2>/dev/null | awk '{ print $NF }')"
    else
      check_fail "podman 4 or newer is required (found: ${major:-unknown})"
    fi
  else
    check_fail "podman is not installed" "sudo apt install podman"
  fi

  if command_available newuidmap && command_available newgidmap; then
    check_ok "newuidmap and newgidmap (rootless Podman)"
  else
    check_fail "newuidmap/newgidmap are missing; rootless Podman needs them" "sudo apt install uidmap"
  fi

  if user_dbus_available; then
    check_ok "systemd user bus (Podman cgroup limits)"
  else
    check_fail "no systemd user bus; rootless Podman cannot apply memory limits without it" \
      "sudo apt install dbus-user-session"
  fi

  if nix_path="$(nix_command_path)"; then
    check_ok "nix at ${nix_path}"
    if nix_daemon_active; then
      check_ok "nix-daemon is running"
    else
      check_fail "nix-daemon is not running; Russel builds through it" \
        "sudo systemctl enable --now nix-daemon.socket"
    fi
    if nix_flakes_enabled "$nix_path"; then
      check_ok "nix flakes enabled in /etc/nix/nix.conf"
    else
      check_fail "nix flakes are not enabled for all users" \
        "echo 'experimental-features = nix-command flakes' | sudo tee -a /etc/nix/nix.conf" \
        "sudo systemctl restart nix-daemon"
    fi
  else
    check_fail "no multi-user Nix at $(nix_daemon_profile_bin)/nix" \
      "sh <(curl -L https://nixos.org/nix/install) --daemon" \
      "A single-user Nix install cannot be used by the russel service account."
  fi

  if command_available git; then
    check_ok "git"
  else
    check_fail "git is not installed; Russel clones app repos with it" "sudo apt install git"
  fi

  if command_available getent && command_available useradd && command_available usermod; then
    check_ok "user management tools (getent, useradd, usermod)"
  else
    check_fail "getent/useradd/usermod are missing" "sudo apt install passwd"
  fi

  if free="$(disk_free_gib /nix)" && (( free < 10 )); then
    check_warn "only ${free} GiB free for /nix; Nix builds usually need 10 GiB or more"
  fi
  if free="$(disk_free_gib "$(state_dir_path)")" && (( free < 5 )); then
    check_warn "only ${free} GiB free for $(state_dir_path)"
  fi

  if kvm_available; then
    local missing ch_major
    missing="$(microvm_missing_tools)"
    if [[ -n "$missing" ]]; then
      check_warn "microVMs (optional): missing ${missing}" \
        "containers work without them; see docs/concepts/runtimes.md"
    else
      ch_major="$(cloud_hypervisor_major_version)"
      if [[ "$ch_major" =~ ^[0-9]+$ ]] && (( ch_major < 52 )); then
        check_warn "microVMs (optional): cloud-hypervisor v${ch_major} hangs when cpus > 1; v52 or newer is needed"
      else
        check_ok "microVM tools (cloud-hypervisor, virtiofsd, passt)"
      fi
    fi
    if ! group_exists kvm; then
      check_warn "microVMs (optional): /dev/kvm exists but there is no kvm group to grant access"
    fi
  else
    check_ok "no /dev/kvm: containers only (microVMs are optional)"
  fi

  if (( CHECK_FAILURES )); then
    echo "${CHECK_FAILURES} problem(s) must be fixed before installing."
    return 1
  fi
  if (( CHECK_WARNINGS )); then
    echo "Ready, with ${CHECK_WARNINGS} warning(s)."
  else
    echo "Ready."
  fi
}

# --- The russel service account ------------------------------------------------

# First id after every range already in a subuid/subgid file, never below
# 100000 (useradd's SUB_UID_MIN default).
next_subid_start() {
  local file=$1
  awk -F: '
    BEGIN { next_free = 100000 }
    NF >= 3 && $2 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ {
      end = $2 + $3
      if (end > next_free) next_free = end
    }
    END { print next_free }
  ' "$file" 2>/dev/null || printf '%s\n' 100000
}

has_subid_range() {
  local file=$1 user=$2
  [[ -f "$file" ]] && awk -F: -v user="$user" '$1 == user { found = 1 } END { exit !found }' "$file"
}

ensure_subid_ranges() {
  local user=$1 start
  if ! has_subid_range "$(subuid_file_path)" "$user"; then
    start="$(next_subid_start "$(subuid_file_path)")"
    run_usermod --add-subuids "${start}-$((start + 65535))" "$user" || {
      echo "cannot add a subuid range for ${user}" >&2
      return 1
    }
  fi
  if ! has_subid_range "$(subgid_file_path)" "$user"; then
    start="$(next_subid_start "$(subgid_file_path)")"
    run_usermod --add-subgids "${start}-$((start + 65535))" "$user" || {
      echo "cannot add a subgid range for ${user}" >&2
      return 1
    }
  fi
}

nologin_shell() {
  local shell
  for shell in /usr/sbin/nologin /sbin/nologin /usr/bin/nologin; do
    if [[ -x "$shell" ]]; then
      printf '%s\n' "$shell"
      return 0
    fi
  done
  printf '%s\n' /bin/false
}

# Create (or adopt) the unprivileged account that runs russel-ctrl, give it a
# rootless Podman id range and linger, and let the operator read the token.
ensure_service_account() {
  local user group operator
  user="$(service_user)"
  group="$(service_group)"

  if ! group_exists "$group"; then
    run_groupadd --system "$group" || {
      echo "cannot create group ${group}" >&2
      return 1
    }
  fi
  if ! account_exists "$user"; then
    run_useradd --system --gid "$group" --home-dir "$(state_dir_path)" --no-create-home \
      --shell "$(nologin_shell)" --comment "Russel control plane" "$user" || {
      echo "cannot create user ${user}" >&2
      return 1
    }
    echo "created service account ${user}"
  fi
  ensure_subid_ranges "$user" || return 1

  if kvm_available && group_exists kvm; then
    run_usermod --append --groups kvm "$user" || {
      echo "cannot add ${user} to the kvm group" >&2
      return 1
    }
  fi
  if operator="$(operator_user)"; then
    run_usermod --append --groups "$group" "$operator" || {
      echo "cannot add ${operator} to the ${group} group" >&2
      return 1
    }
  fi
  # Linger starts the account's user manager at boot, which creates
  # /run/user/<uid> and the user bus rootless Podman needs.
  if ! run_loginctl enable-linger "$user" >/dev/null 2>&1; then
    echo "cannot enable linger for ${user}" >&2
    return 1
  fi
}

# --- Token file -----------------------------------------------------------------

# /etc/russel/env is root:russel 0640: systemd reads it as root, and the
# russel group (the operator) can read the token to log in. Nothing else can.
validate_existing_env() {
  local env_file=$1
  local mode mode_number owner
  if [[ -L "$env_file" || ! -f "$env_file" ]]; then
    echo "token file must be a regular non-symlink file: ${env_file}" >&2
    return 1
  fi
  mode="$(path_mode "$env_file")" || {
    echo "cannot inspect token file permissions: ${env_file}" >&2
    return 1
  }
  if [[ ! "$mode" =~ ^[0-7]+$ ]]; then
    echo "cannot inspect token file permissions: ${env_file}" >&2
    return 1
  fi
  mode_number=$((8#$mode))
  if (( mode_number & 037 )); then
    echo "token file is group-writable or world-accessible; fix permissions on ${env_file} (want 0640)" >&2
    return 1
  fi
  owner="$(path_owner "$env_file")" || {
    echo "cannot inspect token file ownership: ${env_file}" >&2
    return 1
  }
  if [[ "${owner%%:*}" != 0 ]]; then
    echo "token file must be owned by root: ${env_file}" >&2
    return 1
  fi
}

# The token of the old per-user install, so existing `russel login`s keep
# working after the move. Empty when there is none.
legacy_token_line() {
  local operator legacy_env
  operator="$(operator_user)" || return 1
  legacy_env="$(legacy_env_file_path "$operator")" || return 1
  [[ -f "$legacy_env" && ! -L "$legacy_env" ]] || return 1
  awk '/^RUSSEL_API_TOKEN=[^[:space:]]+$/ { print; exit }' "$legacy_env"
}

create_env_file() {
  local env_file=$1
  local env_dir tmp token_line

  env_dir="$(dirname -- "$env_file")"
  if ! tmp="$(umask 077; run_mktemp -- "${env_dir}/.env.tmp.XXXXXX")"; then
    echo "cannot create token file temporary" >&2
    return 1
  fi
  if ! (
    umask 077
    if ! token_line="$(legacy_token_line)" || [[ -z "$token_line" ]]; then
      if ! token_line="RUSSEL_API_TOKEN=$(run_openssl rand -hex 32 2>/dev/null)"; then
        exit 1
      fi
      [[ "$token_line" != RUSSEL_API_TOKEN= ]] || exit 1
    fi
    printf '%s\n' "$token_line" >"$tmp"
    run_chown "root:$(service_group)" -- "$tmp"
    run_chmod 640 -- "$tmp"
  ); then
    run_rm -f -- "$tmp" 2>/dev/null || true
    echo "cannot create token file" >&2
    return 1
  fi

  # GNU mv -n gives the missing-file path an atomic, no-replace publish. If a
  # concurrent operator won the race, validate and retain that file instead.
  if ! run_mv_noclobber -- "$tmp" "$env_file"; then
    run_rm -f -- "$tmp" 2>/dev/null || true
    echo "cannot publish token file" >&2
    return 1
  fi
  if [[ -e "$tmp" || -L "$tmp" ]]; then
    run_rm -f -- "$tmp" 2>/dev/null || true
    validate_existing_env "$env_file"
  fi
}

ensure_env_dir() {
  local env_dir=$1
  if [[ -L "$env_dir" ]]; then
    echo "refusing symlink directory: ${env_dir}" >&2
    return 1
  fi
  if [[ -e "$env_dir" && ! -d "$env_dir" ]]; then
    echo "not a directory: ${env_dir}" >&2
    return 1
  fi
  run_mkdir -p -- "$env_dir"
  run_chown "root:$(service_group)" -- "$env_dir"
  run_chmod 750 -- "$env_dir"
}

ensure_env_file() {
  local env_file=$1
  ensure_env_dir "$(dirname -- "$env_file")" || return 1
  if [[ -L "$env_file" ]]; then
    echo "refusing symlink token file: ${env_file}" >&2
    return 1
  fi
  if [[ -e "$env_file" ]]; then
    validate_existing_env "$env_file"
  else
    create_env_file "$env_file"
  fi
}

# --- State directory --------------------------------------------------------------

ensure_state_directory() {
  local state_dir=$1
  local user group expected_owner actual_owner

  user="$(service_user)"
  group="$(service_group)"
  if [[ -L "$state_dir" ]]; then
    echo "refusing symlink state directory: ${state_dir}" >&2
    return 1
  fi
  if [[ ! -e "$state_dir" ]]; then
    run_mkdir -p -- "$state_dir"
    run_chown "${user}:${group}" -- "$state_dir"
    run_chmod 700 -- "$state_dir"
    return 0
  fi
  if [[ ! -d "$state_dir" ]]; then
    echo "state path is not a directory: ${state_dir}" >&2
    return 1
  fi

  expected_owner="$(account_uid "$user"):$(run_getent group "$group" | awk -F: 'NR == 1 { print $3 }')"
  actual_owner="$(path_owner "$state_dir")" || {
    echo "cannot inspect state directory ownership: ${state_dir}" >&2
    return 1
  }
  if [[ "$actual_owner" == "$expected_owner" ]]; then
    return 0
  fi
  if (( ! TAKE_STATE_OWNERSHIP )); then
    echo "state directory ${state_dir} has owner ${actual_owner}; use --take-state-ownership to give it to ${user}" >&2
    return 1
  fi
  # Everything under it was written by the previous owner; the service account
  # has to be able to read and rewrite all of it (secrets, history, volumes).
  run_chown -R "${user}:${group}" -- "$state_dir"
  run_chmod 700 -- "$state_dir"
  echo "gave ${state_dir} to ${user}"
}

ensure_kernel_pool_directories() {
  local state_dir=$1 directory=$2 pool_dir
  pool_dir="$(state_dir_path)/_pool"
  if ! run_mkdir -p -- "$directory"; then
    echo "cannot create kernel pool directory: ${directory}" >&2
    return 1
  fi
  # The installer runs as root, so mkdir leaves root-owned directories. Hand
  # the pool tree to the service account like the state directory, so ctrl can
  # read and rewrite its own kernel cache.
  run_chown "$(service_user):$(service_group)" -- "$pool_dir" "$directory"
  run_chmod 700 -- "$directory"
}

install_kernel_image() {
  local state_dir=$1 source=$2 destination backup tmp backup_tmp
  destination="$(kernel_pool_image_path)"
  backup="${destination}.previous"
  KERNEL_CHANGED=0
  KERNEL_HAD_PREVIOUS=0
  KERNEL_BACKUP="$backup"
  ensure_kernel_pool_directories "$state_dir" "$(kernel_pool_dir_path)" || return 1

  if [[ -e "$destination" ]] && run_cmp -s -- "$source" "$destination"; then
    return 0
  fi

  # Keep the kernel we are about to replace so a failed upgrade can put it back
  # (restore_host_kernel). Copying straight over the live bzImage would let a
  # full disk or an interrupted copy truncate a bootable kernel.
  if [[ -e "$destination" ]]; then
    if ! backup_tmp="$(run_mktemp -- "$(dirname -- "$destination")/.russel-kernel.tmp.XXXXXX")"; then
      echo "cannot create kernel backup temporary" >&2
      return 1
    fi
    if ! run_install -m 0644 -- "$destination" "$backup_tmp" \
      || ! run_mv -f -- "$backup_tmp" "$backup"; then
      run_rm -f -- "$backup_tmp" 2>/dev/null || true
      echo "cannot keep a copy of the previous kernel" >&2
      return 1
    fi
    KERNEL_HAD_PREVIOUS=1
  fi

  if ! tmp="$(run_mktemp -- "$(dirname -- "$destination")/.russel-kernel.tmp.XXXXXX")"; then
    echo "cannot create kernel temporary" >&2
    return 1
  fi
  # Stage the verified kernel next to its destination, then rename: the pool
  # bzImage is only ever a complete file.
  if ! run_install -m 0644 -- "$source" "$tmp" \
    || ! run_mv -f -- "$tmp" "$destination"; then
    run_rm -f -- "$tmp" 2>/dev/null || true
    echo "cannot install microVM kernel at ${destination}" >&2
    return 1
  fi
  run_chown "$(service_user):$(service_group)" -- "$destination"
  KERNEL_CHANGED=1
}

install_host_kernel() {
  local state_dir=$1 version asset destination
  # Defaults for the rollback state, so restore_host_kernel is safe on the
  # paths below that skip the kernel entirely (`set -u` is on).
  KERNEL_CHANGED=0
  KERNEL_HAD_PREVIOUS=0
  KERNEL_BACKUP=''
  if ! kvm_available; then
    echo "no /dev/kvm on this host; skipping microVM kernel (containers only)"
    return 0
  fi
  if ! version="$(release_version)"; then
    echo "RUSSEL_VERSION unset; skipping microVM kernel fetch (local build: nix build .#microvm-kernel)"
    return 0
  fi
  if [[ -z "$DOWNLOAD_DIR" ]]; then
    if ! DOWNLOAD_DIR="$(run_mktemp -d "${TMPDIR:-/tmp}/russel-release.XXXXXX")"; then
      echo "cannot create download directory" >&2
      return 1
    fi
    trap cleanup_download_dir EXIT
    fetch_release_sums "$version" || return 1
  fi
  # The release publishes an x86_64 bzImage named for the tag.
  asset="russel-kernel-${version}-x86_64.bzImage"
  destination="${DOWNLOAD_DIR}/${asset}"
  fetch_release_file "$version" "$asset" "$destination" || return 1
  verify_release_asset "$asset" "$destination" || return 1
  install_kernel_image "$state_dir" "$destination" || return 1
  echo "installed microVM kernel $(kernel_pool_image_path)"
}

unit_has_required_line() {
  local expression=$1
  local unit_file=$2
  grep -Eq "$expression" "$unit_file"
}

# The shipped unit with the service account's uid filled in.
render_system_unit() {
  local source_file=$1 destination=$2 uid
  uid="$(account_uid "$(service_user)")"
  if [[ ! "$uid" =~ ^[0-9]+$ ]]; then
    echo "cannot resolve the uid of $(service_user)" >&2
    return 1
  fi
  sed "s/@RUSSEL_UID@/${uid}/g" -- "$source_file" >"$destination"
}

validate_system_unit() {
  local unit_file=$1
  local reason

  if ! unit_has_required_line '^[[:space:]]*ExecStart=/usr/local/bin/russel-ctrl([[:space:]]|$)' "$unit_file"; then
    reason="ExecStart=/usr/local/bin/russel-ctrl"
  elif ! unit_has_required_line "^[[:space:]]*User=$(service_user)[[:space:]]*$" "$unit_file"; then
    reason="User=$(service_user)"
  elif ! unit_has_required_line '^[[:space:]]*EnvironmentFile=/etc/russel/env[[:space:]]*$' "$unit_file"; then
    reason="EnvironmentFile=/etc/russel/env"
  elif ! unit_has_required_line '^[[:space:]]*Environment=["]?RUSSEL_REQUIRE_AUTH=1["]?([[:space:]]|$)' "$unit_file"; then
    reason="RUSSEL_REQUIRE_AUTH=1"
  elif ! unit_has_required_line '^[[:space:]]*Environment=["]?RUSSEL_CTRL_ADDR=127\.0\.0\.1:7878["]?([[:space:]]|$)' "$unit_file"; then
    reason="loopback RUSSEL_CTRL_ADDR=127.0.0.1:7878"
  elif unit_has_required_line '@RUSSEL_UID@' "$unit_file"; then
    reason="a filled-in @RUSSEL_UID@"
  else
    return 0
  fi
  echo "system unit ${unit_file} fails required ${reason}; use --force-unit after reviewing it" >&2
  return 1
}

install_system_unit() {
  local unit_file=$1
  local source_file=$2
  local unit_dir rendered tmp
  UNIT_CHANGED=0

  unit_dir="$(dirname -- "$unit_file")"
  if [[ -L "$unit_file" ]]; then
    echo "refusing symlink system unit: ${unit_file}" >&2
    return 1
  fi
  if ! rendered="$(run_mktemp -- "${unit_dir}/.russel-ctrl.service.render.XXXXXX")"; then
    echo "cannot create system unit temporary" >&2
    return 1
  fi
  if ! render_system_unit "$source_file" "$rendered"; then
    run_rm -f -- "$rendered" 2>/dev/null || true
    return 1
  fi

  # Only the pre-release unit had PrivateTmp=yes, and it breaks every container
  # (#524), so replace it even without --force-unit. Drop-ins are untouched.
  local replace_broken=0
  if unit_pins_private_tmp "$unit_file" && ! run_cmp -s -- "$unit_file" "$rendered"; then
    replace_broken=1
    echo "replacing ${unit_file}: its PrivateTmp=yes breaks rootless Podman (#524)"
  fi
  if [[ ! -e "$unit_file" ]] || { ! run_cmp -s -- "$unit_file" "$rendered" && (( FORCE_UNIT || replace_broken )); }; then
    if ! tmp="$(run_mktemp -- "${unit_dir}/.russel-ctrl.service.XXXXXX")"; then
      run_rm -f -- "$rendered" 2>/dev/null || true
      echo "cannot create system unit temporary" >&2
      return 1
    fi
    if ! run_install -m 0644 -- "$rendered" "$tmp" || ! run_mv -f -- "$tmp" "$unit_file"; then
      run_rm -f -- "$tmp" "$rendered" 2>/dev/null || true
      echo "cannot install system unit ${unit_file}" >&2
      return 1
    fi
    UNIT_CHANGED=1
  fi
  run_rm -f -- "$rendered" 2>/dev/null || true

  validate_system_unit "$unit_file" || return 1
  if (( UNIT_CHANGED )); then
    if ! run_systemctl_system daemon-reload >/dev/null 2>&1; then
      echo "systemctl daemon-reload failed" >&2
      return 1
    fi
  fi
}

install_host_binary() {
  local source=$1
  local destination=$2
  local backup="${destination}.previous"
  local tmp backup_tmp
  BINARY_CHANGED=0
  BINARY_HAD_PREVIOUS=0
  BINARY_BACKUP="$backup"

  if [[ -L "$destination" ]]; then
    echo "refusing symlink binary destination: ${destination}" >&2
    return 1
  fi
  if [[ -e "$destination" ]] && run_cmp -s -- "$source" "$destination"; then
    return 0
  fi

  if [[ -e "$destination" ]]; then
    if ! backup_tmp="$(run_mktemp -- "$(dirname -- "$destination")/.russel-ctrl.tmp.XXXXXX")"; then
      echo "cannot create binary backup temporary" >&2
      return 1
    fi
    if ! run_install -m 0755 -- "$destination" "$backup_tmp" || ! run_mv -f -- "$backup_tmp" "$backup"; then
      run_rm -f -- "$backup_tmp" 2>/dev/null || true
      echo "cannot keep a copy of the previous binary" >&2
      return 1
    fi
    BINARY_HAD_PREVIOUS=1
  fi

  if ! tmp="$(run_mktemp -- "$(dirname -- "$destination")/.russel-ctrl.tmp.XXXXXX")"; then
    echo "cannot create binary temporary" >&2
    return 1
  fi
  if ! run_install -m 0755 -- "$source" "$tmp" || ! run_mv -f -- "$tmp" "$destination"; then
    run_rm -f -- "$tmp" 2>/dev/null || true
    echo "cannot install ${destination}" >&2
    return 1
  fi
  BINARY_CHANGED=1
}

restore_host_binary() {
  local destination=$1
  local tmp
  if (( ! BINARY_CHANGED )); then
    return 0
  fi
  if (( BINARY_HAD_PREVIOUS )); then
    if ! tmp="$(run_mktemp -- "$(dirname -- "$destination")/.russel-ctrl.tmp.XXXXXX")"; then
      return 1
    fi
    if ! run_install -m 0755 -- "$BINARY_BACKUP" "$tmp" || ! run_mv -f -- "$tmp" "$destination"; then
      run_rm -f -- "$tmp" 2>/dev/null || true
      return 1
    fi
  else
    run_rm -f -- "$destination"
  fi
}

# Put the pool kernel back after a failed upgrade. A no-op unless this run
# replaced it, so callers can invoke it unconditionally on error paths.
restore_host_kernel() {
  local state_dir destination tmp
  state_dir="$(state_dir_path)"
  destination="$(kernel_pool_image_path)"
  if (( ! KERNEL_CHANGED )); then
    return 0
  fi
  if (( KERNEL_HAD_PREVIOUS )); then
    if ! tmp="$(run_mktemp -- "$(dirname -- "$destination")/.russel-kernel.tmp.XXXXXX")"; then
      return 1
    fi
    if ! run_install -m 0644 -- "$KERNEL_BACKUP" "$tmp" \
      || ! run_mv -f -- "$tmp" "$destination"; then
      run_rm -f -- "$tmp" 2>/dev/null || true
      return 1
    fi
    run_chown "$(service_user):$(service_group)" -- "$destination"
  else
    run_rm -f -- "$destination"
  fi
  KERNEL_CHANGED=0
}

json_has_vms_array() {
  local body_file=$1
  if command_available python3; then
    python3 - "$body_file" <<'PY' >/dev/null 2>&1
import json
import sys

with open(sys.argv[1], encoding="utf-8") as body:
    value = json.load(body)
raise SystemExit(0 if isinstance(value, dict) and isinstance(value.get("vms"), list) else 1)
PY
    return $?
  fi
  if command_available jq; then
    jq -e 'type == "object" and (.vms | type == "array")' "$body_file" >/dev/null 2>&1
    return $?
  fi
  # Minimal fallback for small Linux hosts without Python or jq. The API
  # response is required to be a JSON object and to expose vms as an array;
  # keep this check deliberately narrow rather than accepting arbitrary 200s.
  local compact_body
  compact_body="$(tr -d '[:space:]' <"$body_file")"
  [[ "$compact_body" =~ ^\{\"vms\":\[.*\].*\}$ ||
    "$compact_body" =~ ^\{.*,\"vms\":\[.*\].*\}$ ]]
}

# True when 401 response headers carry the Russel Bearer marker sent by
# russel-ctrl's auth middleware (`WWW-Authenticate: Bearer realm="russel-ctrl"`).
# A bare 401 from an unrelated listener on 127.0.0.1:7878 must not pass
# compatibility in host/connect/status.
probe_headers_identify_russel() {
  local headers_file=$1
  [[ -f "$headers_file" ]] && grep -Eiq '^www-authenticate:.*Bearer.*russel' -- "$headers_file"
}

probe_local_endpoint() {
  local attempt body_file headers_file http_code
  PROBE_RESULT=unreachable
  PROBE_HTTP_STATUS=

  if ! command_available curl; then
    PROBE_RESULT=unavailable
    return 1
  fi

  for (( attempt = 1; attempt <= PROBE_ATTEMPTS; attempt++ )); do
    if ! body_file="$(run_mktemp "${TMPDIR:-/tmp}/russel-probe.XXXXXX")"; then
      PROBE_RESULT=unreachable
      return 1
    fi
    if ! headers_file="$(run_mktemp "${TMPDIR:-/tmp}/russel-probe-headers.XXXXXX")"; then
      run_rm -f -- "$body_file" 2>/dev/null || true
      PROBE_RESULT=unreachable
      return 1
    fi
    if http_code="$(run_curl --silent --show-error --connect-timeout "$PROBE_TIMEOUT_SECONDS" --max-time "$PROBE_TIMEOUT_SECONDS" --dump-header "$headers_file" --output "$body_file" --write-out '%{http_code}' http://127.0.0.1:7878/vms 2>/dev/null)"; then
      PROBE_HTTP_STATUS=$http_code
      if [[ "$http_code" == 401 ]]; then
        if probe_headers_identify_russel "$headers_file"; then
          run_rm -f -- "$body_file" "$headers_file" 2>/dev/null || true
          PROBE_RESULT=unauthorized
          return 0
        fi
        run_rm -f -- "$body_file" "$headers_file" 2>/dev/null || true
        PROBE_RESULT=foreign_unauthorized
        return 1
      fi
      if [[ "$http_code" == 200 ]]; then
        if json_has_vms_array "$body_file"; then
          run_rm -f -- "$body_file" "$headers_file" 2>/dev/null || true
          PROBE_RESULT=compatible
          return 0
        fi
        run_rm -f -- "$body_file" "$headers_file" 2>/dev/null || true
        PROBE_RESULT=invalid_json
        return 1
      fi
      run_rm -f -- "$body_file" "$headers_file" 2>/dev/null || true
      PROBE_RESULT=other_http
      return 1
    fi
    run_rm -f -- "$body_file" "$headers_file" 2>/dev/null || true
  done
  PROBE_RESULT=unreachable
  return 1
}

# A unit that was just started can take a few seconds to listen (its first
# start sets up rootless Podman for the account). A refused connection
# returns at once, so keep probing while the port is unreachable instead of
# giving up within milliseconds.
wait_for_local_endpoint() {
  local waited=0
  while ! probe_local_endpoint; do
    [[ "$PROBE_RESULT" == unreachable ]] || return 1
    (( waited < HOST_READY_SECONDS )) || return 1
    run_sleep 1
    waited=$(( waited + 1 ))
  done
}

probe_is_compatible() {
  [[ "$PROBE_RESULT" == compatible || "$PROBE_RESULT" == unauthorized ]]
}

probe_result_text() {
  case "$PROBE_RESULT" in
    compatible) printf '%s\n' "compatible (HTTP 200 /vms JSON)" ;;
    unauthorized) printf '%s\n' "unauthorized (HTTP 401; compatible Russel endpoint)" ;;
    foreign_unauthorized) printf '%s\n' "unauthorized (HTTP 401; unrecognized endpoint — not Russel)" ;;
    other_http) printf '%s\n' "other HTTP (${PROBE_HTTP_STATUS:-unknown})" ;;
    invalid_json) printf '%s\n' "other HTTP (200; invalid /vms JSON)" ;;
    unavailable) printf '%s\n' "unavailable (curl not found)" ;;
    *) printf '%s\n' "unreachable" ;;
  esac
}

system_unit_is_active() {
  run_systemctl_system is-active --quiet russel-ctrl >/dev/null 2>&1
}

# The pre-release unit had PrivateTmp=yes. Rootless Podman's pause process
# (`catatonit -P`) outlives the unit and keeps that first ctrl's private /tmp,
# which systemd deleted, so every podman call fails until it is gone (#524).
# Pre-release builds could not start containers, so none depend on it yet.
unit_pins_private_tmp() {
  local unit=$1
  [[ -f "$unit" ]] && grep -Eq '^PrivateTmp=(yes|true|1)' -- "$unit"
}

reset_podman_pause_process() {
  local user
  user="$(service_user)"
  run_systemctl_system stop russel-ctrl >/dev/null 2>&1 || true
  run_pkill -u "$user" -x catatonit >/dev/null 2>&1 || true
  echo "reset rootless Podman's pause process for ${user} (left over from the old unit)"
}

restart_host_unit() {
  local destination=$1
  local restart_diagnostic restoration_diagnostic='' restored=0 kernel_restored=0

  if restart_diagnostic="$(run_systemctl_system restart russel-ctrl 2>&1)"; then
    return 0
  fi

  if restore_host_binary "$destination"; then
    restored=1
  fi
  # Kernel and controller binary ship from the same release, so the rollback
  # boundary undoes both.
  if restore_host_kernel; then
    kernel_restored=1
  fi
  restoration_diagnostic="$(run_systemctl_system restart russel-ctrl 2>&1 || true)"
  if (( restored )); then
    echo "russel-ctrl restart failed; the previous binary was restored" >&2
  else
    echo "russel-ctrl restart failed; binary restoration did not complete" >&2
  fi
  if (( ! kernel_restored )); then
    echo "russel-ctrl restart failed; kernel restoration did not complete" >&2
  fi
  if [[ -n "$restart_diagnostic" ]]; then
    printf '%s\n' "$restart_diagnostic" >&2
  fi
  if [[ -n "$restoration_diagnostic" ]]; then
    echo "restoration restart diagnostic:" >&2
    printf '%s\n' "$restoration_diagnostic" >&2
  fi
  return 1
}

enable_host_unit() {
  local enable_diagnostic
  if enable_diagnostic="$(run_systemctl_system enable --now russel-ctrl.service 2>&1)"; then
    return 0
  fi
  if probe_local_endpoint && probe_is_compatible; then
    echo "russel-ctrl.service start reported a conflict, but a compatible local Russel endpoint is already running" >&2
    return 0
  fi
  if [[ -n "$enable_diagnostic" ]]; then
    printf '%s\n' "$enable_diagnostic" >&2
  fi
  echo "could not start russel-ctrl.service (local API: $(probe_result_text))" >&2
  echo "see: journalctl -u russel-ctrl and $(state_dir_path)/ctrl.log" >&2
  return 1
}

print_host_next_steps() {
  local operator
  echo "Host installation is ready. russel-ctrl runs as the '$(service_user)' account."
  if kvm_available && [[ -f "$(kernel_pool_image_path)" ]]; then
    echo "MicroVM kernel: $(kernel_pool_image_path)"
  elif ! kvm_available; then
    echo "No /dev/kvm; this host is containers only."
  fi
  if kvm_available; then
    local missing
    missing="$(microvm_missing_tools)"
    if [[ -n "$missing" ]]; then
      echo "MicroVMs (experimental) also need on PATH: ${missing} (distro packages, or nix profile install nixpkgs#passt ...)."
    fi
  fi
  echo "Next steps:"
  if operator="$(operator_user)"; then
    echo "  ${operator} is now in the $(service_group) group, which can read the token."
    echo "  Log out and back in (or run: newgrp $(service_group)) for that to take effect, then:"
  else
    echo "  Add your login user to the $(service_group) group so it can read the token:"
    echo "    sudo usermod -aG $(service_group) <you>, then log in again"
  fi
  echo "  russel login http://127.0.0.1:7878 --token-file $(env_file_path)"
  echo "  From a laptop: copy $(env_file_path) privately, then"
  if [[ -n "$ROOT" ]]; then
    echo "    ./contrib/install.sh connect <user@host>"
  else
    echo "    curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh | bash -s -- connect <user@host>"
  fi
  echo "  Open http://127.0.0.1:7878/ for the dashboard (token in Settings)."
}

install_host() {
  local release_binary destination state_dir env_file source_binary
  local active=0 unit_changed=0 pinned_tmp=0

  check_host_topology || return $?
  if (( SKIP_CHECKS )); then
    echo "skipping host checks (--skip-checks)"
  elif ! check_host_prereqs; then
    echo "fix the problems above and run again, or pass --skip-checks if you know better" >&2
    return 1
  fi
  prepare_release russel-ctrl || return 1
  prepare_release_unit || return 1
  destination="$(host_binary_path)"
  state_dir="$(state_dir_path)"
  release_binary="${RELEASE}/russel-ctrl"

  if [[ -x "$release_binary" ]]; then
    source_binary="$release_binary"
  elif [[ -x "$destination" ]]; then
    source_binary="$destination"
  else
    echo "missing ${release_binary} and ${destination}; run: cargo build --release -p russel-ctrl" >&2
    return 1
  fi

  ensure_service_account || return 1
  env_file="$(env_file_path)"
  ensure_env_file "$env_file" || return 1
  ensure_state_directory "$state_dir" || return 1
  install_host_kernel "$state_dir" || return 1

  if unit_pins_private_tmp "$(system_unit_path)"; then
    pinned_tmp=1
  fi
  # A failed upgrade must not leave a replacement kernel behind, so every error
  # path after the kernel step undoes it (restore_host_kernel is a no-op when
  # this run did not replace the pool kernel).
  install_system_unit "$(system_unit_path)" "$(unit_source_path)" || {
    restore_host_kernel || true
    return 1
  }
  unit_changed=$UNIT_CHANGED

  install_host_binary "$source_binary" "$destination" || {
    restore_host_kernel || true
    return 1
  }
  install_dashboard_dist || {
    restore_host_kernel || true
    return 1
  }

  if system_unit_is_active; then
    active=1
  fi
  if (( active )); then
    if (( ! BINARY_CHANGED && ! unit_changed )); then
      if probe_local_endpoint && probe_is_compatible; then
        print_host_next_steps
        return 0
      fi
    fi
    if (( pinned_tmp && unit_changed )); then
      reset_podman_pause_process
    fi
    restart_host_unit "$destination" || return 1
  else
    # The pause process outlives a stopped unit too (KillMode=process).
    if (( pinned_tmp && unit_changed )); then
      reset_podman_pause_process
    fi
    enable_host_unit || {
      restore_host_kernel || true
      return 1
    }
  fi

  if ! wait_for_local_endpoint || ! probe_is_compatible; then
    echo "local Russel endpoint is not healthy: $(probe_result_text)" >&2
    echo "see: journalctl -u russel-ctrl and $(state_dir_path)/ctrl.log" >&2
    restore_host_kernel || true
    return 1
  fi
  print_host_next_steps
}

listener_output_has_port() {
  local output=$1
  [[ -n "$output" ]] && grep -Eq '(^|[[:space:]])[^[:space:]]*:7878([[:space:]]|$)' <<<"$output"
}

listener_snapshot() {
  local detail
  LISTENER_STATE=unavailable
  LISTENER_TOOL=
  LISTENER_DETAIL=

  if command_available ss; then
    if detail="$(run_ss 2>/dev/null)"; then
      LISTENER_TOOL=ss
      LISTENER_DETAIL=$detail
      if listener_output_has_port "$detail"; then
        LISTENER_STATE=present
      else
        LISTENER_STATE=absent
      fi
      return 0
    fi
  fi
  if command_available lsof; then
    if detail="$(run_lsof 2>/dev/null)"; then
      LISTENER_TOOL=lsof
      LISTENER_DETAIL=$detail
      if listener_output_has_port "$detail"; then
        LISTENER_STATE=present
      else
        LISTENER_STATE=absent
      fi
      return 0
    fi
  fi
  LISTENER_TOOL="ss/lsof"
  return 0
}

connect_host() {
  local host=$1
  local ssh_diagnostic=

  if probe_local_endpoint && probe_is_compatible; then
    echo "a compatible local Russel endpoint is already available; no tunnel opened"
    echo "Next check: russel origin"
    return 0
  fi
  listener_snapshot
  if [[ "$LISTENER_STATE" == present ]]; then
    echo "127.0.0.1:7878 is already owned by a non-compatible listener (${LISTENER_TOOL})" >&2
    [[ -n "$LISTENER_DETAIL" ]] && printf '%s\n' "$LISTENER_DETAIL" >&2
    return 1
  fi

  if ! ssh_diagnostic="$(run_ssh -f -N -o BatchMode=yes -o ExitOnForwardFailure=yes -L 127.0.0.1:7878:127.0.0.1:7878 "$host" 2>&1)"; then
    :
  fi
  if probe_local_endpoint && probe_is_compatible; then
    echo "SSH forward is connected to a compatible Russel endpoint"
    echo "Next check: russel origin"
    return 0
  fi
  if [[ -n "$ssh_diagnostic" ]]; then
    printf '%s\n' "$ssh_diagnostic" >&2
  fi
  echo "SSH forward did not produce a compatible local Russel endpoint: $(probe_result_text)" >&2
  listener_snapshot
  [[ -n "$LISTENER_DETAIL" ]] && printf '%s\n' "$LISTENER_DETAIL" >&2
  return 1
}

status_listener() {
  listener_snapshot
  case "$LISTENER_STATE" in
    present)
      echo "listener: present via ${LISTENER_TOOL}"
      [[ -n "$LISTENER_DETAIL" ]] && printf '%s\n' "$LISTENER_DETAIL"
      ;;
    absent) echo "listener: none reported by ${LISTENER_TOOL}" ;;
    *) echo "listener: unavailable (${LISTENER_TOOL} not found or unusable)" ;;
  esac
}

status_unit() {
  local active
  if ! command_available systemctl; then
    echo "unit: unavailable (systemctl not found)"
    return 0
  fi
  if run_systemctl_system cat russel-ctrl.service >/dev/null 2>&1; then
    active="$(run_systemctl_system is-active russel-ctrl 2>/dev/null || true)"
    [[ -n "$active" ]] || active=unknown
    echo "unit: russel-ctrl.service ${active}"
    return 0
  fi
  # The per-user unit that `host` installed before the russel account.
  if run_systemctl_user cat russel-ctrl.service >/dev/null 2>&1; then
    active="$(run_systemctl_user is-active russel-ctrl 2>/dev/null || true)"
    [[ -n "$active" ]] || active=unknown
    echo "unit: old per-user russel-ctrl.service ${active}; run: $(installer_command) host (it explains the move)"
    return 0
  fi
  echo "unit: not installed on this machine"
}

status_origin() {
  if ! command_available russel; then
    echo "russel origin: unavailable (russel not found on PATH)"
    return 0
  fi
  local origin
  if origin="$(run_russel 2>/dev/null)"; then
    echo "russel origin: ${origin}"
  else
    echo "russel origin: unavailable (command failed)"
  fi
}

status_host() {
  status_listener
  status_unit
  if probe_local_endpoint; then
    echo "API: $(probe_result_text)"
  else
    echo "API: $(probe_result_text)"
  fi
  status_origin
  probe_is_compatible
}

install_cli() {
  local source destination
  prepare_release russel || return 1
  source="${RELEASE}/russel"
  destination="$(operator_home)/.local/bin/russel"
  if [[ ! -x "$source" ]]; then
    echo "missing ${source}; run: cargo build --release -p russel-cli" >&2
    return 1
  fi
  run_install -Dm755 -- "$source" "$destination"
  echo "installed ${destination}"
}

dashboard_dist_source() {
  if [[ -n "${RUSSEL_DASHBOARD_SRC:-}" ]]; then
    printf '%s\n' "$RUSSEL_DASHBOARD_SRC"
  elif [[ -n "$DOWNLOAD_DASHBOARD_SRC" ]]; then
    printf '%s\n' "$DOWNLOAD_DASHBOARD_SRC"
  else
    printf '%s\n' "${ROOT}/dashboard/dist"
  fi
}

install_dashboard_dist() {
  local source destination
  prepare_release_dashboard || return 1
  source="$(dashboard_dist_source)"
  destination="${RUSSEL_DASHBOARD_DEST:-/usr/local/share/russel/dashboard}"
  if [[ ! -f "${source}/index.html" ]]; then
    echo "warning: ${source}/index.html missing; russel-ctrl will start without a dashboard" >&2
    echo "build it with: cd dashboard && bun install && bun run build" >&2
    return 0
  fi
  if run_mkdir -p -- "$destination" 2>/dev/null; then
    cp -a -- "${source}/." "$destination"/
  else
    run_sudo mkdir -p -- "$destination"
    run_sudo cp -a -- "${source}/." "$destination"/
  fi
  echo "installed dashboard ${destination}"
}

install_ctrl() {
  local source destination parent
  prepare_release russel-ctrl || return 1
  source="${RELEASE}/russel-ctrl"
  destination="${RUSSEL_CTRL_DEST:-/usr/local/bin/russel-ctrl}"
  if [[ ! -x "$source" ]]; then
    echo "missing ${source}; run: cargo build --release -p russel-ctrl" >&2
    return 1
  fi
  parent="$(dirname -- "$destination")"
  if [[ -w "$parent" ]]; then
    run_install -Dm755 -- "$source" "$destination"
  else
    run_sudo install -Dm755 -- "$source" "$destination"
  fi
  echo "installed ${destination}"
  install_dashboard_dist
}

main() {
  local command=
  local destination=

  FORCE_UNIT=0
  TAKE_STATE_OWNERSHIP=0
  SKIP_CHECKS=0

  while (( $# )); do
    case "$1" in
      --force-unit) FORCE_UNIT=1 ;;
      --take-state-ownership) TAKE_STATE_OWNERSHIP=1 ;;
      --skip-checks) SKIP_CHECKS=1 ;;
      -*) usage; return 2 ;;
      *) command=$1; shift; break ;;
    esac
    shift
  done
  if [[ -z "$command" ]]; then
    usage
    return 2
  fi

  case "$command" in
    cli|ctrl|all)
      if (( $# != 0 || FORCE_UNIT || TAKE_STATE_OWNERSHIP || SKIP_CHECKS )); then
        usage
        return 2
      fi
      case "$command" in
        cli) install_cli ;;
        ctrl) install_ctrl ;;
        all)
          install_cli
          install_ctrl
          ;;
      esac
      ;;
    host)
      if (( $# != 0 )); then
        usage
        return 2
      fi
      install_host
      ;;
    connect)
      if (( $# != 1 || FORCE_UNIT || TAKE_STATE_OWNERSHIP || SKIP_CHECKS )); then
        usage
        return 2
      fi
      destination=$1
      if [[ -z "$destination" || "$destination" == -* ]]; then
        usage
        return 2
      fi
      connect_host "$destination"
      ;;
    status)
      if (( $# != 0 || FORCE_UNIT || TAKE_STATE_OWNERSHIP || SKIP_CHECKS )); then
        usage
        return 2
      fi
      status_host
      ;;
    check)
      if (( $# != 0 || FORCE_UNIT || TAKE_STATE_OWNERSHIP || SKIP_CHECKS )); then
        usage
        return 2
      fi
      check_host_prereqs
      ;;
    *)
      usage
      return 2
      ;;
  esac
}

# Piped on stdin (`curl | bash`) leaves BASH_SOURCE empty while $0 is "bash";
# run the installer there too, and only stay quiet when sourced by a test.
if [[ -z "${BASH_SOURCE[0]:-}" || "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
