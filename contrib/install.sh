#!/usr/bin/env bash
# Install Russel binaries and manage a non-NixOS user control plane.
#
#   ./contrib/install.sh cli
#   ./contrib/install.sh ctrl
#   ./contrib/install.sh all
#   ./contrib/install.sh [--force-unit] [--take-state-ownership] host
#   ./contrib/install.sh connect <user@host-or-ssh-alias>
#   ./contrib/install.sh status
#
# Without a local target/release build the installer downloads release assets
# for RUSSEL_VERSION and verifies each one against the release SHA256SUMS:
#
#   curl -fsSL <raw install.sh url> | RUSSEL_VERSION=v0.1.0 bash -s -- host
#
# RUSSEL_RELEASE_BASE overrides the artifact base URL. Private GitHub
# releases also need RUSSEL_GITHUB_TOKEN, GH_TOKEN, or GITHUB_TOKEN.
# The script is safe to pipe on stdin: it never assumes BASH_SOURCE
# points at a checkout root.
set -euo pipefail

SCRIPT_PATH="${BASH_SOURCE[0]:-}"
ROOT=
if [[ -n "$SCRIPT_PATH" && -f "$SCRIPT_PATH" ]]; then
  ROOT="$(cd -- "$(dirname -- "$SCRIPT_PATH")/.." && pwd)"
fi
RELEASE="${ROOT:+$ROOT/}target/release"
PROBE_TIMEOUT_SECONDS=2
PROBE_ATTEMPTS=2
DOWNLOAD_TIMEOUT_SECONDS=600
RELEASE_BASE="${RUSSEL_RELEASE_BASE:-https://github.com/daschinmoy21/russel/releases/download}"
RELEASE_ARCH=
DOWNLOAD_DIR=
DOWNLOAD_SUMS=
DOWNLOAD_DASHBOARD_SRC=
UNIT_SOURCE_OVERRIDE=
FORCE_UNIT=0
TAKE_STATE_OWNERSHIP=0
HOST_DEST_USE_SUDO=0
HOST_STATE_USE_SUDO=0

usage() {
  echo "usage: $0 cli|ctrl|all" >&2
  echo "       $0 [--force-unit] [--take-state-ownership] host" >&2
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
run_ssh() { ssh "$@"; }
run_with_timeout() { timeout "$@"; }
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

config_root_path() {
  printf '%s\n' "$(operator_home)/.config"
}

user_unit_dir_path() {
  printf '%s\n' "$(config_root_path)/systemd/user"
}

user_unit_path() {
  printf '%s\n' "$(user_unit_dir_path)/russel-ctrl.service"
}

env_dir_path() {
  printf '%s\n' "$(config_root_path)/russel"
}

env_file_path() {
  printf '%s\n' "$(env_dir_path)/env"
}

unit_source_path() {
  if [[ -n "$UNIT_SOURCE_OVERRIDE" ]]; then
    printf '%s\n' "$UNIT_SOURCE_OVERRIDE"
  else
    printf '%s\n' "${ROOT}/contrib/russel-ctrl.service"
  fi
}

path_parent_writable() {
  local path=$1
  local parent
  parent="$(dirname -- "$path")"
  [[ -d "$parent" && -w "$parent" ]]
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

require_sudo() {
  if ! command_available sudo; then
    echo "missing required command: sudo" >&2
    return 2
  fi
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
  local unit fragment

  if [[ "$(current_user)" == root ]]; then
    echo "host refuses root; use a non-root operator account" >&2
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

  require_command install systemctl openssl || return $?

  for unit in russel.service russel-ctrl.service; do
    if host_system_unit_active_or_enabled "$unit"; then
      echo "host refuses system-managed ${unit}; use services.russel instead" >&2
      return 2
    fi
    fragment="$(host_system_unit_fragment "$unit")"
    if [[ "$fragment" == /nix/store/* ]]; then
      echo "host refuses Nix-managed ${unit}; use services.russel instead" >&2
      return 2
    fi
  done

  if ! run_systemctl_user status >/dev/null 2>&1; then
    echo "user-systemd manager is unavailable; start a user systemd manager first" >&2
    return 2
  fi
}

check_host_privilege_tools() {
  local destination=$1
  local state_dir=$2
  local need_sudo=0
  HOST_DEST_USE_SUDO=0
  HOST_STATE_USE_SUDO=0

  if ! path_parent_writable "$destination"; then
    need_sudo=1
    HOST_DEST_USE_SUDO=1
  fi
  if [[ ! -e "$state_dir" && ! -L "$state_dir" ]] && ! path_parent_writable "$state_dir"; then
    need_sudo=1
    HOST_STATE_USE_SUDO=1
  fi
  if (( need_sudo )); then
    require_sudo || return $?
  fi
  return 0
}

host_destination_uses_sudo() {
  (( HOST_DEST_USE_SUDO ))
}

host_state_uses_sudo() {
  (( HOST_STATE_USE_SUDO ))
}

state_run_mkdir() {
  local state_dir=$1
  shift
  if host_state_uses_sudo "$state_dir"; then
    run_sudo mkdir "$@"
  else
    run_mkdir "$@"
  fi
}

state_run_chmod() {
  local state_dir=$1
  shift
  if host_state_uses_sudo "$state_dir"; then
    run_sudo chmod "$@"
  else
    run_chmod "$@"
  fi
}

state_run_chown() {
  local state_dir=$1
  shift
  if host_state_uses_sudo "$state_dir"; then
    run_sudo chown "$@"
  else
    run_chown "$@"
  fi
}

state_run_install() {
  local state_dir=$1
  shift
  if host_state_uses_sudo "$state_dir"; then
    run_sudo install "$@"
  else
    run_install "$@"
  fi
}

state_make_temp() {
  local state_dir=$1 destination=$2
  local directory
  directory="$(dirname -- "$destination")"
  if host_state_uses_sudo "$state_dir"; then
    run_sudo mktemp -- "${directory}/.russel-kernel.tmp.XXXXXX"
  else
    run_mktemp -- "${directory}/.russel-kernel.tmp.XXXXXX"
  fi
}

state_move() {
  local state_dir=$1
  shift
  if host_state_uses_sudo "$state_dir"; then
    run_sudo mv "$@"
  else
    run_mv "$@"
  fi
}

state_remove() {
  local state_dir=$1
  shift
  if host_state_uses_sudo "$state_dir"; then
    run_sudo rm "$@"
  else
    run_rm "$@"
  fi
}

ensure_private_directory() {
  local directory=$1
  if [[ -L "$directory" ]]; then
    echo "refusing symlink directory: ${directory}" >&2
    return 1
  fi
  if [[ -e "$directory" && ! -d "$directory" ]]; then
    echo "not a directory: ${directory}" >&2
    return 1
  fi
  run_mkdir -p -- "$directory"
  run_chmod 700 -- "$directory"
}

ensure_user_directories() {
  local config_root unit_dir env_dir
  config_root="$(config_root_path)"
  unit_dir="$(user_unit_dir_path)"
  env_dir="$(env_dir_path)"
  # The two private directories above are the security boundary. Keep the
  # shared .config directory creation separate so existing preferences remain.
  if [[ -L "$config_root" || ( -e "$config_root" && ! -d "$config_root" ) ]]; then
    echo "invalid config directory: ${config_root}" >&2
    return 1
  fi
  run_mkdir -p -- "$config_root"
  ensure_private_directory "$unit_dir" || return 1
  ensure_private_directory "$env_dir" || return 1
}

validate_existing_env() {
  local env_file=$1
  local mode mode_number
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
  if (( mode_number & 077 )); then
    echo "token file is group- or world-readable; fix permissions on ${env_file}" >&2
    return 1
  fi
}

create_env_file() {
  local env_file=$1
  local env_dir tmp token

  env_dir="$(dirname -- "$env_file")"
  if ! tmp="$(umask 077; run_mktemp -- "${env_dir}/.env.tmp.XXXXXX")"; then
    echo "cannot create token file temporary" >&2
    return 1
  fi
  if ! (
    umask 077
    if ! token="$(run_openssl rand -hex 32 2>/dev/null)"; then
      exit 1
    fi
    printf 'RUSSEL_API_TOKEN=%s\n' "$token" >"$tmp"
    run_chmod 600 -- "$tmp"
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

ensure_env_file() {
  local env_file=$1
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

ensure_state_directory() {
  local state_dir=$1
  local expected_owner actual_owner

  if [[ -L "$state_dir" ]]; then
    echo "refusing symlink state directory: ${state_dir}" >&2
    return 1
  fi
  if [[ ! -e "$state_dir" ]]; then
    state_run_mkdir "$state_dir" -p -- "$state_dir"
    state_run_chown "$state_dir" "$(current_user):$(current_group)" "$state_dir"
    state_run_chmod "$state_dir" 700 -- "$state_dir"
    return 0
  fi
  if [[ ! -d "$state_dir" ]]; then
    echo "state path is not a directory: ${state_dir}" >&2
    return 1
  fi

  expected_owner="$(current_uid):$(current_gid)"
  actual_owner="$(path_owner "$state_dir")" || {
    echo "cannot inspect state directory ownership: ${state_dir}" >&2
    return 1
  }
  if [[ "$actual_owner" == "$expected_owner" ]]; then
    return 0
  fi
  if (( ! TAKE_STATE_OWNERSHIP )); then
    echo "state directory ${state_dir} has owner ${actual_owner}; use --take-state-ownership to take it" >&2
    return 1
  fi
  require_sudo || return $?
  run_sudo chown "$(current_user):$(current_group)" "$state_dir"
  run_sudo chmod 700 -- "$state_dir"
}

ensure_kernel_pool_directories() {
  local state_dir=$1 directory=$2 pool_dir
  pool_dir="$(state_dir_path)/_pool"
  if ! state_run_mkdir "$state_dir" -p -- "$directory"; then
    echo "cannot create kernel pool directory: ${directory}" >&2
    return 1
  fi
  # mkdir under sudo leaves root-owned directories. Hand the pool tree to the
  # operator exactly like the state directory, so ctrl can read and rewrite its
  # own kernel cache instead of inheriting root:0700 nested dirs.
  state_run_chown "$state_dir" "$(current_user):$(current_group)" -- "$pool_dir" "$directory"
  state_run_chmod "$state_dir" 700 -- "$directory"
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
    if ! backup_tmp="$(state_make_temp "$state_dir" "$destination")"; then
      echo "cannot create kernel backup temporary" >&2
      return 1
    fi
    if ! state_run_install "$state_dir" -m 0644 -- "$destination" "$backup_tmp" \
      || ! state_move "$state_dir" -f -- "$backup_tmp" "$backup"; then
      state_remove "$state_dir" -f -- "$backup_tmp" 2>/dev/null || true
      echo "cannot keep a copy of the previous kernel" >&2
      return 1
    fi
    KERNEL_HAD_PREVIOUS=1
  fi

  if ! tmp="$(state_make_temp "$state_dir" "$destination")"; then
    echo "cannot create kernel temporary" >&2
    return 1
  fi
  # Stage the verified kernel next to its destination, then rename: the pool
  # bzImage is only ever a complete file.
  if ! state_run_install "$state_dir" -m 0644 -- "$source" "$tmp" \
    || ! state_move "$state_dir" -f -- "$tmp" "$destination"; then
    state_remove "$state_dir" -f -- "$tmp" 2>/dev/null || true
    echo "cannot install microVM kernel at ${destination}" >&2
    return 1
  fi
  state_run_chown "$state_dir" "$(current_user):$(current_group)" -- "$destination"
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

unit_has_environment_file() {
  local unit_file=$1
  local operator_env=$2
  awk -v operator_env="$operator_env" '
    /^[[:space:]]*EnvironmentFile=/ {
      value = $0
      sub(/^[[:space:]]*EnvironmentFile=/, "", value)
      gsub(/[[:space:]]+$/, "", value)
      gsub(/^"|"$/, "", value)
      if (value == "%h/.config/russel/env" || value == operator_env) found = 1
    }
    END { exit !found }
  ' "$unit_file"
}

validate_user_unit() {
  local unit_file=$1
  local operator_env=$2
  local reason

  if ! unit_has_required_line '^[[:space:]]*ExecStart=/usr/local/bin/russel-ctrl([[:space:]]|$)' "$unit_file"; then
    reason="ExecStart=/usr/local/bin/russel-ctrl"
  elif ! unit_has_environment_file "$unit_file" "$operator_env"; then
    reason="EnvironmentFile=%h/.config/russel/env (or the operator env path)"
  elif ! unit_has_required_line '^[[:space:]]*Environment=["]?RUSSEL_REQUIRE_AUTH=1["]?([[:space:]]|$)' "$unit_file"; then
    reason="RUSSEL_REQUIRE_AUTH=1"
  elif ! unit_has_required_line '^[[:space:]]*Environment=["]?RUSSEL_CTRL_ADDR=127\.0\.0\.1:7878["]?([[:space:]]|$)' "$unit_file"; then
    reason="loopback RUSSEL_CTRL_ADDR=127.0.0.1:7878"
  else
    return 0
  fi
  echo "user unit ${unit_file} fails required ${reason}; use --force-unit after reviewing it" >&2
  return 1
}

install_user_unit() {
  local unit_file=$1
  local source_file=$2
  local operator_env=$3
  local unit_dir tmp
  UNIT_CHANGED=0

  unit_dir="$(dirname -- "$unit_file")"
  if [[ -L "$unit_file" ]]; then
    echo "refusing symlink user unit: ${unit_file}" >&2
    return 1
  fi
  if [[ ! -e "$unit_file" ]]; then
    if ! tmp="$(run_mktemp -- "${unit_dir}/.russel-ctrl.service.XXXXXX")"; then
      echo "cannot create user unit temporary" >&2
      return 1
    fi
    if ! run_install -m 0644 -- "$source_file" "$tmp"; then
      run_rm -f -- "$tmp" 2>/dev/null || true
      echo "cannot install user unit" >&2
      return 1
    fi
    if ! run_mv -f -- "$tmp" "$unit_file"; then
      run_rm -f -- "$tmp" 2>/dev/null || true
      echo "cannot publish user unit" >&2
      return 1
    fi
    UNIT_CHANGED=1
  elif ! run_cmp -s -- "$unit_file" "$source_file"; then
    if (( FORCE_UNIT )); then
      if ! tmp="$(run_mktemp -- "${unit_dir}/.russel-ctrl.service.XXXXXX")"; then
        echo "cannot create user unit temporary" >&2
        return 1
      fi
      if ! run_install -m 0644 -- "$source_file" "$tmp" || ! run_mv -f -- "$tmp" "$unit_file"; then
        run_rm -f -- "$tmp" 2>/dev/null || true
        echo "cannot replace user unit" >&2
        return 1
      fi
      UNIT_CHANGED=1
    fi
  fi

  validate_user_unit "$unit_file" "$operator_env"
  if (( UNIT_CHANGED )); then
    if ! run_systemctl_user daemon-reload >/dev/null 2>&1; then
      echo "user-systemd daemon-reload failed" >&2
      return 1
    fi
  fi
}

dest_make_temp() {
  local destination=$1
  local directory
  directory="$(dirname -- "$destination")"
  if host_destination_uses_sudo; then
    run_sudo mktemp -- "${directory}/.russel-ctrl.tmp.XXXXXX"
  else
    run_mktemp -- "${directory}/.russel-ctrl.tmp.XXXXXX"
  fi
}

dest_install_binary() {
  local source=$1
  local destination=$2
  if host_destination_uses_sudo; then
    run_sudo install -m 0755 -- "$source" "$destination"
  else
    run_install -m 0755 -- "$source" "$destination"
  fi
}

dest_move() {
  if host_destination_uses_sudo; then
    run_sudo mv "$@"
  else
    run_mv "$@"
  fi
}

dest_remove() {
  if host_destination_uses_sudo; then
    run_sudo rm "$@"
  else
    run_rm "$@"
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
    if ! backup_tmp="$(dest_make_temp "$destination")"; then
      echo "cannot create binary backup temporary" >&2
      return 1
    fi
    if ! dest_install_binary "$destination" "$backup_tmp" || ! dest_move -f -- "$backup_tmp" "$backup"; then
      dest_remove -f -- "$backup_tmp" 2>/dev/null || true
      echo "cannot keep a copy of the previous binary" >&2
      return 1
    fi
    BINARY_HAD_PREVIOUS=1
  fi

  if ! tmp="$(dest_make_temp "$destination")"; then
    echo "cannot create binary temporary" >&2
    return 1
  fi
  if ! dest_install_binary "$source" "$tmp" || ! dest_move -f -- "$tmp" "$destination"; then
    dest_remove -f -- "$tmp" 2>/dev/null || true
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
    if ! tmp="$(dest_make_temp "$destination")"; then
      return 1
    fi
    if ! dest_install_binary "$BINARY_BACKUP" "$tmp" || ! dest_move -f -- "$tmp" "$destination"; then
      dest_remove -f -- "$tmp" 2>/dev/null || true
      return 1
    fi
  else
    dest_remove -f -- "$destination"
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
    if ! tmp="$(state_make_temp "$state_dir" "$destination")"; then
      return 1
    fi
    if ! state_run_install "$state_dir" -m 0644 -- "$KERNEL_BACKUP" "$tmp" \
      || ! state_move "$state_dir" -f -- "$tmp" "$destination"; then
      state_remove "$state_dir" -f -- "$tmp" 2>/dev/null || true
      return 1
    fi
    state_run_chown "$state_dir" "$(current_user):$(current_group)" -- "$destination"
  else
    state_remove "$state_dir" -f -- "$destination"
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

user_unit_is_active() {
  run_systemctl_user is-active --quiet russel-ctrl >/dev/null 2>&1
}

restart_host_unit() {
  local destination=$1
  local restart_diagnostic restoration_diagnostic='' restored=0 kernel_restored=0

  if restart_diagnostic="$(run_systemctl_user restart russel-ctrl 2>&1)"; then
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
  restoration_diagnostic="$(run_systemctl_user restart russel-ctrl 2>&1 || true)"
  if (( restored )); then
    echo "user-systemd restart failed; the previous binary was restored" >&2
  else
    echo "user-systemd restart failed; binary restoration did not complete" >&2
  fi
  if (( ! kernel_restored )); then
    echo "user-systemd restart failed; kernel restoration did not complete" >&2
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
  if enable_diagnostic="$(run_systemctl_user enable --now russel-ctrl.service 2>&1)"; then
    return 0
  fi
  if probe_local_endpoint && probe_is_compatible; then
    echo "user unit start reported a conflict, but a compatible local Russel endpoint is already running" >&2
    return 0
  fi
  if [[ -n "$enable_diagnostic" ]]; then
    printf '%s\n' "$enable_diagnostic" >&2
  fi
  echo "could not start russel-ctrl.service (local API: $(probe_result_text))" >&2
  return 1
}

host_linger() {
  local diagnostic user
  user="$(current_user)"
  if diagnostic="$(run_loginctl enable-linger "$user" 2>&1)"; then
    return 0
  fi
  echo "warning: could not enable login linger for ${user}" >&2
  [[ -n "$diagnostic" ]] && printf '%s\n' "$diagnostic" >&2
  return 0
}

print_host_next_steps() {
  echo "Host installation is ready."
  if kvm_available && [[ -f "$(kernel_pool_image_path)" ]]; then
    echo "MicroVM kernel: $(kernel_pool_image_path)"
  elif ! kvm_available; then
    echo "No /dev/kvm; this host is containers only."
  fi
  echo "Next steps:"
  echo "  Copy ~/.config/russel/env to the laptop (keep it private)."
  if [[ -n "$ROOT" ]]; then
    echo "  ./contrib/install.sh connect <user@host>"
  else
    echo "  curl -fsSL https://raw.githubusercontent.com/daschinmoy21/russel/main/contrib/install.sh | bash -s -- connect <user@host>"
  fi
  echo "  russel login http://127.0.0.1:7878 --token-file ~/.config/russel/env"
  echo "  Open http://127.0.0.1:7878/ for the dashboard (token in Settings)."
}

install_host() {
  local release_binary destination state_dir env_file unit_file source_binary
  local active=0 unit_changed=0

  check_host_topology || return $?
  prepare_release russel-ctrl || return 1
  prepare_release_unit || return 1
  destination="$(host_binary_path)"
  state_dir="$(state_dir_path)"
  release_binary="${RELEASE}/russel-ctrl"
  check_host_privilege_tools "$destination" "$state_dir" || return $?

  if [[ -x "$release_binary" ]]; then
    source_binary="$release_binary"
  elif [[ -x "$destination" ]]; then
    source_binary="$destination"
  else
    echo "missing ${release_binary} and ${destination}; run: cargo build --release -p russel-ctrl" >&2
    return 1
  fi

  ensure_user_directories || return 1
  env_file="$(env_file_path)"
  ensure_env_file "$env_file" || return 1
  ensure_state_directory "$state_dir" || return 1
  install_host_kernel "$state_dir" || return 1

  # A failed upgrade must not leave a replacement kernel behind, so every error
  # path after the kernel step undoes it (restore_host_kernel is a no-op when
  # this run did not replace the pool kernel).
  install_user_unit "$(user_unit_path)" "$(unit_source_path)" "$env_file" || {
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

  if user_unit_is_active; then
    active=1
  fi
  if (( active )); then
    if (( ! BINARY_CHANGED && ! unit_changed )); then
      if probe_local_endpoint && probe_is_compatible; then
        host_linger
        print_host_next_steps
        return 0
      fi
    fi
    restart_host_unit "$destination" || return 1
  else
    enable_host_unit || {
      restore_host_kernel || true
      return 1
    }
  fi

  if ! probe_local_endpoint || ! probe_is_compatible; then
    echo "local Russel endpoint is not healthy: $(probe_result_text)" >&2
    restore_host_kernel || true
    return 1
  fi
  host_linger
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

status_user_unit() {
  local active
  if ! command_available systemctl; then
    echo "unit: unavailable (systemctl not found)"
    return 0
  fi
  if ! run_systemctl_user cat russel-ctrl.service >/dev/null 2>&1; then
    echo "unit: not found or user-systemd unavailable"
    return 0
  fi
  active="$(run_systemctl_user is-active russel-ctrl 2>/dev/null || true)"
  [[ -n "$active" ]] || active=unknown
  echo "unit: ${active}"
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
  status_user_unit
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

  while (( $# )); do
    case "$1" in
      --force-unit) FORCE_UNIT=1 ;;
      --take-state-ownership) TAKE_STATE_OWNERSHIP=1 ;;
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
      if (( $# != 0 || FORCE_UNIT || TAKE_STATE_OWNERSHIP )); then
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
      if (( $# != 1 || FORCE_UNIT || TAKE_STATE_OWNERSHIP )); then
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
      if (( $# != 0 || FORCE_UNIT || TAKE_STATE_OWNERSHIP )); then
        usage
        return 2
      fi
      status_host
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
