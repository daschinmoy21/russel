#!/usr/bin/env bash
# bench-common.sh — shared library for the russel bench scripts.
#
# Source from bench.sh / bench-load.sh (NOT executable on its own):
#   source "$(dirname "$(readlink -f "$0")")/bench-common.sh"
#
# Provides: color vars, header/pass/info/warn/fail, podman_as_deploy_user,
# the run-lock, and the /var/lib restore + temp-path cleanup helpers used by
# both scripts' EXIT traps.

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
	echo "bench-common.sh is a library — source it from a bench script." >&2
	exit 1
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
# BOLD is used by bench.sh / bench-load.sh (not by helpers in this file).
# shellcheck disable=SC2034
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

header() { echo -e "\n${CYAN}━━━ $1 ━━━${NC}"; }
pass() { echo -e "  ${GREEN}✓${NC} $1"; }
info() { echo -e "  ${DIM}→${NC} $1"; }
warn() { echo -e "  ${YELLOW}⚠${NC} $1"; }
fail() { echo -e "  ${RED}✗${NC} $1"; }

# ── Podman identity helper (Issue #278598) ────────────────────────────────────
# Run a command as the invoking user under `sudo` (the rootless Podman
# identity), or directly otherwise.
as_deploy_user() {
	if [ "${EUID:-$(id -u)}" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
		sudo -u "$SUDO_USER" -H "$@"
	else
		"$@"
	fi
}

# When root via sudo, check SUDO_USER's rootless podman (not root's rootful).
podman_as_deploy_user() {
	if [ "${EUID:-$(id -u)}" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
		local uid home
		uid=$(id -u "$SUDO_USER" 2>/dev/null) || return 1
		home=$(getent passwd "$SUDO_USER" | cut -d: -f6)
		[ -n "$home" ] || home="/home/$SUDO_USER"
		# XDG_RUNTIME_DIR required for rootless
		sudo -u "$SUDO_USER" -H env \
			"HOME=$home" \
			"XDG_RUNTIME_DIR=/run/user/$uid" \
			"DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus" \
			podman "$@"
	else
		podman "$@"
	fi
}

# ── Run-lock ──────────────────────────────────────────────────────────────────
# The benches move the host's real /var/lib/{russel,microvms} aside; two
# concurrent runs would fight over the displaced state (or the second would
# treat the first's temp state as "original"). Hold an exclusive lock for the
# whole run.
bench_run_lock() {
	exec 9>/tmp/russel-bench.lock
	if ! flock -n 9; then
		fail "another russel bench run is active (/tmp/russel-bench.lock)"
		exit 1
	fi
}

# ── Cleanup helpers (called from each script's EXIT trap) ─────────────────────
# Both scripts use the same globals: VAR_LIB_REDIRECTED, RUSSEL_STATE_BAK,
# MICROVMS_STATE_BAK, RUSSEL_STATE_DIR, BENCH_CARGO_TARGET, RUSSEL_LOG.
bench_restore_var_lib() {
	if [ "$VAR_LIB_REDIRECTED" -eq 1 ]; then
		rm -f /var/lib/russel /var/lib/microvms
		if [ -n "$RUSSEL_STATE_BAK" ] && [ -e "$RUSSEL_STATE_BAK" ]; then
			mv "$RUSSEL_STATE_BAK" /var/lib/russel
		fi
		if [ -n "$MICROVMS_STATE_BAK" ] && [ -e "$MICROVMS_STATE_BAK" ]; then
			mv "$MICROVMS_STATE_BAK" /var/lib/microvms
		fi
	fi
}

bench_cleanup_tmp_paths() {
	if [ -n "${RUSSEL_STATE_DIR:-}" ] && [ -d "$RUSSEL_STATE_DIR" ]; then
		rm -rf "$RUSSEL_STATE_DIR"
	fi
	if [ -n "$BENCH_CARGO_TARGET" ] && [ -d "$BENCH_CARGO_TARGET" ]; then
		rm -rf "$BENCH_CARGO_TARGET"
	fi
	if [ -n "${RUSSEL_LOG:-}" ] && [ -f "$RUSSEL_LOG" ]; then
		rm -f "$RUSSEL_LOG"
	fi
}
