#!/usr/bin/env bash
set -euo pipefail

# ──────────────────────────────────────────────────────────────────────────────
# russel bench — systems benchmark + app boot race
#
# Measures compile times, test speeds, binary sizes, then races three paths
# per example app (end to end: build → spawn → first HTTP response):
#
#   1. Russel microVM   — Cloud Hypervisor (service.type / runtime microvm)
#   2. Russel container — rootless Podman --rootfs (runtime container)
#   3. Raw podman/docker — Dockerfile build + run (baseline)
#
# Run from repo root after `nix develop` or with Rust toolchain on PATH.
# Use --cold to force cold builds (no layer/nix cache); default is --warm.
# --warm keeps images and nix store paths for fair warm-vs-warm comparison.
# ──────────────────────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

# ── Argument parsing ───────────────────────────────────────────────────────────
COLD=0
while [[ $# -gt 0 ]]; do
	case "$1" in
	--cold)
		COLD=1
		shift
		;;
	--warm)
		COLD=0
		shift
		;;
	-h | --help)
		echo "Usage: $0 [--cold|--warm]" >&2
		echo "  Benchmarks Russel microVM, Russel container (rootless podman), and raw docker/podman." >&2
		exit 0
		;;
	*)
		echo "Unknown: $1" >&2
		exit 1
		;;
	esac
done

bench_start=$(date +%s%N)

header() { echo -e "\n${CYAN}━━━ $1 ━━━${NC}"; }
pass() { echo -e "  ${GREEN}✓${NC} $1"; }
info() { echo -e "  ${DIM}→${NC} $1"; }
warn() { echo -e "  ${YELLOW}⚠${NC} $1"; }
fail() { echo -e "  ${RED}✗${NC} $1"; }

echo -e "${BOLD}
  ┌──────────────────────────────────────────────┐
  │         russel bench — systems bench          │
  │         $(date +%Y-%m-%d\ %H:%M)                         │
  └──────────────────────────────────────────────┘${NC}"

# ── Cleanup trap ─────────────────────────────────────────────────────────────
CTRL_PID=""
CLEANUP_RAN=0
# Track whether we redirected /var/lib paths
VAR_LIB_REDIRECTED=0
RUSSEL_STATE_BAK=""
MICROVMS_STATE_BAK=""
BENCH_CARGO_TARGET=""

# `sudo ./bench.sh` must not leave root-owned artifacts in the checkout. The
# benchmark already asks Cargo for a clean build, so isolate that work in /tmp
# when root runs it and remove it at the end.
if [ "$EUID" -eq 0 ]; then
	BENCH_CARGO_TARGET=$(mktemp -d /tmp/russel-bench-target-XXXXXX)
	export CARGO_TARGET_DIR="$BENCH_CARGO_TARGET"
fi
RELEASE_DIR="${CARGO_TARGET_DIR:-$PWD/target}/release"

cleanup() {
	if [ "$CLEANUP_RAN" -eq 1 ]; then return; fi
	CLEANUP_RAN=1
	# Destroy any lingering bench VMs first. Cleanup must never hang the
	# benchmark (for example if a VMM is already wedged).
	for vm in "${RUSSEL_VMS_CREATED[@]:-}"; do
		if command -v russel-cli &>/dev/null; then
			info "cleaning up VM: $vm"
			if command -v timeout &>/dev/null; then
				timeout --signal=TERM --kill-after=2s 15s russel-cli destroy "$vm" &>/dev/null || warn "cleanup timed out or failed for $vm"
			else
				russel-cli destroy "$vm" &>/dev/null || warn "cleanup failed for $vm"
			fi
		fi
	done
	# Kill control plane
	if [ -n "$CTRL_PID" ] && kill -0 "$CTRL_PID" 2>/dev/null; then
		info "shutting down russel-ctrl (pid $CTRL_PID)"
		kill "$CTRL_PID" 2>/dev/null || true
		wait "$CTRL_PID" 2>/dev/null || true
	fi
	# Restore original /var/lib/{russel,microvms} if we moved them
	if [ "$VAR_LIB_REDIRECTED" -eq 1 ]; then
		rm -f /var/lib/russel /var/lib/microvms
		if [ -n "$RUSSEL_STATE_BAK" ] && [ -e "$RUSSEL_STATE_BAK" ]; then
			mv "$RUSSEL_STATE_BAK" /var/lib/russel
		fi
		if [ -n "$MICROVMS_STATE_BAK" ] && [ -e "$MICROVMS_STATE_BAK" ]; then
			mv "$MICROVMS_STATE_BAK" /var/lib/microvms
		fi
	fi
	# Remove temp files
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
trap cleanup EXIT

# ── Podman identity helper (Issue #278598) ────────────────────────────────────
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

# ponytail: results arrays — indexed by example order
declare -a EXAMPLES=()
declare -a RUSSEL_MICROVM_DEPLOY_MS=()
declare -a RUSSEL_MICROVM_CURL_MS=()
declare -a RUSSEL_MICROVM_SPAWN_MS=()
declare -a RUSSEL_CONTAINER_DEPLOY_MS=()
declare -a RUSSEL_CONTAINER_CURL_MS=()
declare -a RUSSEL_CONTAINER_SPAWN_MS=()
declare -a DOCKER_BUILD_MS=()
declare -a DOCKER_BOOT_MS=()
declare -a WINNER=()
RUSSEL_VMS_CREATED=()
LAST_RUSSEL_COMPLETE=""
LAST_RUSSEL_RUNTIME=""

# ── 0. Dependency check ─────────────────────────────────────────────────────
header "Dependency Check"
RUNTIME=""
if command -v podman &>/dev/null; then
	RUNTIME="podman"
	pass "baseline container runtime: podman ($(podman --version 2>/dev/null | head -1))"
elif command -v docker &>/dev/null; then
	RUNTIME="docker"
	pass "baseline container runtime: docker ($(docker --version 2>/dev/null | head -1))"
else
	warn "no podman/docker found — skipping raw image baseline"
fi

# Rootless Podman is required for Russel containers (not docker, not rootful).
HAS_ROOTLESS_PODMAN=0
RUSSEL_PODMAN_USER=""
if command -v podman &>/dev/null; then
	rootless_val=$(podman_as_deploy_user info --format '{{.Host.Security.Rootless}}' 2>/dev/null | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]')
	if [ "$rootless_val" = "true" ]; then
		HAS_ROOTLESS_PODMAN=1
		if [ "${EUID:-0}" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
			RUSSEL_PODMAN_USER="$SUDO_USER"
			pass "rootless podman: yes (user=$RUSSEL_PODMAN_USER; Russel containers)"
		else
			pass "rootless podman: yes (Russel containers)"
		fi
	else
		warn "podman present but not rootless — Russel container path will be skipped"
		if [ "${EUID:-0}" -eq 0 ]; then
			info "hint: root's podman is rootful; run via sudo from a user with rootless podman (SUDO_USER) or without sudo if only testing containers"
		fi
	fi
else
	warn "podman missing — Russel container path will be skipped"
fi

# ── 1. Build profile ─────────────────────────────────────────────────────────
header "Build Profile"
info "target: $(rustc -vV | grep host | cut -d' ' -f2)"
info "rustc:   $(rustc -V)"
info "edition: 2024"

# ── 2. Clean build time (debug) ──────────────────────────────────────────────
header "Clean Build (debug)"

cargo clean -q 2>/dev/null || true
clean_start=$(date +%s%N)
cargo build -q 2>&1
clean_end=$(date +%s%N)
clean_ms=$(((clean_end - clean_start) / 1000000))
pass "clean build: ${clean_ms}ms"

# ── 3. Incremental build time ────────────────────────────────────────────────
header "Incremental Build"

inc_start=$(date +%s%N)
touch crates/ctrl/src/main.rs
cargo build -q 2>&1
inc_end=$(date +%s%N)
inc_ms=$(((inc_end - inc_start) / 1000000))
pass "incremental (touch 1 file): ${inc_ms}ms"

# ── 4. Test time ──────────────────────────────────────────────────────────────
header "Tests"

test_start=$(date +%s%N)
set +e
test_out=$(cargo test 2>&1)
test_status=$?
set -e
test_end=$(date +%s%N)
test_ms=$(((test_end - test_start) / 1000000))

test_passed=$(echo "$test_out" | grep -oP '\d+(?= passed)' | paste -sd+ | head -1 || echo "0")
test_failed=$(echo "$test_out" | grep -oP '\d+(?= failed)' | paste -sd+ | head -1 || echo "0")
if [ "$test_status" -eq 0 ]; then
	pass "tests: ${test_passed} passed, ${test_failed} failed in ${test_ms}ms"
else
	warn "tests failed (exit ${test_status}): ${test_passed} passed, ${test_failed} failed in ${test_ms}ms"
fi

# ── 5. Release build ──────────────────────────────────────────────────────────
header "Release Build"

rel_start=$(date +%s%N)
cargo build --release -q 2>&1
rel_end=$(date +%s%N)
rel_ms=$(((rel_end - rel_start) / 1000000))
pass "release build: ${rel_ms}ms"

# ── 6. Binary sizes ──────────────────────────────────────────────────────────
header "Binary Sizes"

for bin in russel-cli russel-ctrl; do
	path="${RELEASE_DIR}/${bin}"
	if [ -f "$path" ]; then
		size=$(stat --printf="%s" "$path")
		size_kb=$((size / 1024))
		stripped_size=$(strip "$path" -o "$path.stripped" 2>/dev/null && stat --printf="%s" "$path.stripped" 2>/dev/null || echo "$size")
		rm -f "$path.stripped"
		stripped_kb=$((stripped_size / 1024))
		pass "${bin}: ${size_kb}KB (stripped: ${stripped_kb}KB)"
	else
		warn "${bin}: binary not found"
	fi
done

# ── 7. Dependency count ──────────────────────────────────────────────────────
header "Dependency Tree"

dep_count=$(cargo tree -e normal --prefix none 2>/dev/null | grep -v '^$' | grep -cE '^\S' || true)
pass "direct deps: ${dep_count}"

# ── 8. LOC ───────────────────────────────────────────────────────────────────
header "Lines of Code"

rust_files=$(find crates -name '*.rs' -not -path '*/target/*' | wc -l)
rust_lines=$(find crates -name '*.rs' -not -path '*/target/*' -exec wc -l {} + | tail -1 | awk '{print $1}')
doc_files=$(find docs -name '*.md' | wc -l)
doc_lines=$(find docs -name '*.md' -exec wc -l {} + | tail -1 | awk '{print $1}')
docker_files=$(find examples -name 'Dockerfile' | wc -l)
pass "Rust: ${rust_files} files, ${rust_lines} lines"
pass "Docs: ${doc_files} files, ${doc_lines} lines"
pass "Dockerfiles: ${docker_files}"

# ── 9. Code quality snapshot ─────────────────────────────────────────────────
header "Code Quality"

clippy_warns=$(cargo clippy 2>&1 | grep -c 'warning:' || true)
pass "clippy warnings: ${clippy_warns}"

# ══════════════════════════════════════════════════════════════════════════════
# APP BOOT RACE — Russel microVM | Russel container | raw Docker/podman
# ══════════════════════════════════════════════════════════════════════════════
#
# For each example application we measure end-to-end paths:
#
#   Russel microVM:   nix build → initramfs → TAP/socat → cloud-hypervisor → HTTP
#   Russel container: nix build → rootfs adapter → rootless podman --rootfs → HTTP
#   Baseline:         $RUNTIME build (Dockerfile) → run → HTTP
#
# MicroVM needs KVM + root + nix + cloud-hypervisor + socat + ip.
# Container needs root (bench /var/lib redirect) + nix + rootless podman.
# Baseline needs podman or docker.
# ──────────────────────────────────────────────────────────────────────────────

has_kvm=0
has_root=0
has_nix=0
has_ch=0
has_socat=0
has_ip=0

[ -c /dev/kvm ] 2>/dev/null && has_kvm=1
[ "$EUID" -eq 0 ] 2>/dev/null && has_root=1
command -v nix &>/dev/null && has_nix=1
command -v cloud-hypervisor &>/dev/null && has_ch=1
command -v socat &>/dev/null && has_socat=1
command -v ip &>/dev/null && has_ip=1

RUSSEL_MICROVM_CAPABLE=0
if [ "$has_kvm" -eq 1 ] && [ "$has_root" -eq 1 ] && [ "$has_nix" -eq 1 ] &&
	[ "$has_ch" -eq 1 ] && [ "$has_socat" -eq 1 ] && [ "$has_ip" -eq 1 ]; then
	RUSSEL_MICROVM_CAPABLE=1
fi

RUSSEL_CONTAINER_CAPABLE=0
if [ "$has_root" -eq 1 ] && [ "$has_nix" -eq 1 ] && [ "$HAS_ROOTLESS_PODMAN" -eq 1 ]; then
	RUSSEL_CONTAINER_CAPABLE=1
fi

RUSSEL_CTRL_NEEDED=0
if [ "$RUSSEL_MICROVM_CAPABLE" -eq 1 ] || [ "$RUSSEL_CONTAINER_CAPABLE" -eq 1 ]; then
	RUSSEL_CTRL_NEEDED=1
fi

if [ "$RUSSEL_CTRL_NEEDED" -eq 0 ] && [ -z "$RUNTIME" ]; then
	header "App Boot Race: Russel microVM | Russel container | Docker"
	warn "no deploy path available (need microVM prereqs and/or rootless podman and/or docker)"
	warn "skipping comparison section entirely."
else

	# ── Start benchmarking ──────────────────────────────────────────────────────
	header "App Boot Race: Russel microVM | Russel container | Docker"

	# Helper: extract a single integer field from a JSON line
	json_int() {
		local field="$1" json="$2"
		echo "$json" | grep -oP "\"${field}\":\d+" | head -1 | cut -d: -f2
	}

	get_ready_path() {
		case "$1" in
		basic-http) echo "/health" ;;
		*) echo "/" ;;
		esac
	}

	# Deploy one Russel service (microvm|container).
	# Sets: out_deploy_ms out_curl_ms out_spawn_ms out_status (ok|failed|timeout|failed:msg)
	russel_deploy_one() {
		local runtime_kind="$1"
		local repo_path="$2"
		local vm_id="$3"
		local host_port="$4"
		local guest_port="$5"
		local ready_path="$6"
		local example="$7"

		out_deploy_ms=0
		out_curl_ms=0
		out_spawn_ms=0
		out_status="failed"

		russel-cli destroy "$vm_id" &>/dev/null || true

		if [ "$COLD" -eq 1 ]; then
			local_result="examples/$example/result"
			if [ -L "$local_result" ]; then
				result_path=$(readlink -f "$local_result")
				rm -f "$local_result"
				nix-store --delete "$result_path" 2>/dev/null || true
			fi
		fi

		# File is source of truth for runtime: temp Russelfile with type under [service]
		# (examples omit type → default microvm).
		cfg_name="Russelfile.bench-${runtime_kind}.toml"
		cfg_path="$repo_path/$cfg_name"
		awk -v rt="$runtime_kind" '
			BEGIN { in_service=0; injected=0 }
			/^type[[:space:]]*=/ { next }
			/^\[service\]/ {
				print
				print "type = \"" rt "\""
				in_service=1
				injected=1
				next
			}
			/^\[/ { in_service=0 }
			{ print }
			END {
				if (!injected) {
					print "[service]"
					print "type = \"" rt "\""
				}
			}
		' "$repo_path/Russelfile.toml" >"$cfg_path"

		set +e
		response=$(curl -s -X POST "http://${RUSSEL_CTRL_ADDR}/deploy" \
			-H "Content-Type: application/json" \
			-d "{
				\"repo_url\": \"$repo_path\",
				\"config_path\": \"$cfg_name\",
				\"vm_id\": \"$vm_id\",
				\"port\": {\"host\": $host_port, \"guest\": $guest_port},
				\"runtime\": \"$runtime_kind\",
				\"podman_args\": []
			}" 2>&1)
		rm -f "$cfg_path"

		complete_line=$(echo "$response" | grep '"type":"Complete"' | tail -1)
		deploy_status=$(echo "$complete_line" | grep -oP '"status":"([^"]*)"' | cut -d'"' -f4 || echo "failed")

		if [ "$deploy_status" = "deployed" ]; then
			russel_elapsed=$(json_int "elapsed_ms" "$complete_line")
			out_deploy_ms=$((russel_elapsed))
			russel_build_phase=$(json_int "build_ms" "$complete_line")
			[ -z "$russel_build_phase" ] && russel_build_phase=0
			LAST_RUSSEL_COMPLETE="$complete_line"
			LAST_RUSSEL_RUNTIME="$runtime_kind"
			RUSSEL_VMS_CREATED+=("$vm_id")

			curl_start=$(date +%s%N)
			curl_ok=0
			for _ in $(seq 1 100); do
				if curl -sf "http://127.0.0.1:$host_port$ready_path" >/dev/null 2>&1; then
					curl_ok=1
					break
				fi
				sleep 0.1
			done
			curl_end=$(date +%s%N)
			if [ "$curl_ok" -eq 1 ]; then
				out_curl_ms=$(((curl_end - curl_start) / 1000000))
				out_spawn_ms=$((russel_elapsed - russel_build_phase + out_curl_ms))
				out_status="ok"
			else
				out_status="timeout"
			fi
		else
			err_msg=$(echo "$complete_line" | grep -oP '"message":"([^"]*)"' | cut -d'"' -f4 || echo "unknown")
			out_status="failed:$err_msg"
		fi
		set -euo pipefail
	}

	# ── 10a. Russel setup ──────────────────────────────────────────────────────
	RUSSEL_SKIPPED=0
	RUSSEL_MICROVM_SKIPPED=0
	RUSSEL_CONTAINER_SKIPPED=0

	if [ "$RUSSEL_CTRL_NEEDED" -eq 1 ]; then
		info "Russel paths: microVM=$RUSSEL_MICROVM_CAPABLE container=$RUSSEL_CONTAINER_CAPABLE"

		if [ ! -f "$RELEASE_DIR/russel-ctrl" ] || [ ! -f "$RELEASE_DIR/russel-cli" ]; then
			info "building russel (release) for deploy comparison..."
			cargo build --release -q 2>&1
		fi

		export PATH="$RELEASE_DIR:$PATH"

		# Pass podman user identity to russel-ctrl (Issue #278598)
		export RUSSEL_PODMAN_USER

		RUSSEL_STATE_DIR=$(mktemp -d /tmp/russel-bench-XXXXXX)
		export RUSSEL_CTRL_ADDR="127.0.0.1:7878"

		mkdir -p "$RUSSEL_STATE_DIR/lib/russel" "$RUSSEL_STATE_DIR/lib/microvms"
		# mktemp is 0700 root-owned; rootless podman (SUDO_USER) must traverse
		# /var/lib/russel → this tree to faccessat the prepared rootfs.
		chmod 755 "$RUSSEL_STATE_DIR" "$RUSSEL_STATE_DIR/lib" \
			"$RUSSEL_STATE_DIR/lib/russel" "$RUSSEL_STATE_DIR/lib/microvms"
		# ponytail: redirect /var/lib/{russel,microvms} to temp dir (requires root)
		VAR_LIB_REDIRECTED=1
		if [ -e /var/lib/russel ] || [ -L /var/lib/russel ]; then
			RUSSEL_STATE_BAK=$(mktemp /tmp/russel-var-lib-bak-XXXXXX)
			rm -f "$RUSSEL_STATE_BAK"
			mv /var/lib/russel "$RUSSEL_STATE_BAK"
		fi
		ln -sfn "$RUSSEL_STATE_DIR/lib/russel" /var/lib/russel
		if [ -e /var/lib/microvms ] || [ -L /var/lib/microvms ]; then
			MICROVMS_STATE_BAK=$(mktemp /tmp/russel-var-lib-bak-XXXXXX)
			rm -f "$MICROVMS_STATE_BAK"
			mv /var/lib/microvms "$MICROVMS_STATE_BAK"
		fi
		ln -sfn "$RUSSEL_STATE_DIR/lib/microvms" /var/lib/microvms

		info "starting russel-ctrl (pid in background)..."
		RUSSEL_LOG=$(mktemp /tmp/russel-ctrl-log-XXXXXX)
		russel-ctrl &>"$RUSSEL_LOG" &
		CTRL_PID=$!

		info "waiting for control plane on $RUSSEL_CTRL_ADDR ..."
		ctrl_ready=0
		for i in $(seq 1 30); do
			if curl -sf "http://${RUSSEL_CTRL_ADDR}/vms" >/dev/null 2>&1; then
				ctrl_ready=1
				break
			fi
			sleep 1
		done
		if [ "$ctrl_ready" -eq 0 ]; then
			warn "russel-ctrl did not start within 30s (check $RUSSEL_LOG)"
			RUSSEL_SKIPPED=1
			RUSSEL_MICROVM_SKIPPED=1
			RUSSEL_CONTAINER_SKIPPED=1
		else
			pass "russel-ctrl ready after ${i}s"
			[ "$RUSSEL_MICROVM_CAPABLE" -eq 0 ] && RUSSEL_MICROVM_SKIPPED=1
			[ "$RUSSEL_CONTAINER_CAPABLE" -eq 0 ] && RUSSEL_CONTAINER_SKIPPED=1
			[ "$RUSSEL_MICROVM_CAPABLE" -eq 1 ] && pass "Russel microVM path enabled"
			[ "$RUSSEL_CONTAINER_CAPABLE" -eq 1 ] && pass "Russel container path enabled (rootless podman)"
		fi
	else
		info "Russel control plane not started:"
		[ "$has_root" -eq 0 ] && info "  ✗ root (EUID != 0)"
		[ "$has_nix" -eq 0 ] && info "  ✗ nix (not on PATH)"
		[ "$has_kvm" -eq 0 ] && info "  ✗ KVM (microVM)"
		[ "$has_ch" -eq 0 ] && info "  ✗ cloud-hypervisor (microVM)"
		[ "$has_socat" -eq 0 ] && info "  ✗ socat (microVM)"
		[ "$has_ip" -eq 0 ] && info "  ✗ ip (microVM)"
		[ "$HAS_ROOTLESS_PODMAN" -eq 0 ] && info "  ✗ rootless podman (container)"
		RUSSEL_SKIPPED=1
		RUSSEL_MICROVM_SKIPPED=1
		RUSSEL_CONTAINER_SKIPPED=1
	fi

	if [ "$RUSSEL_MICROVM_SKIPPED" -eq 1 ]; then
		warn "Russel microVM comparison skipped"
	fi
	if [ "$RUSSEL_CONTAINER_SKIPPED" -eq 1 ]; then
		warn "Russel container comparison skipped"
	fi

	# ── 10b. Benchmark each example ─────────────────────────────────────────────
	EXAMPLE_APPS=()
	for ex in basic-http static-test filebrowser; do
		if [ -f "examples/$ex/Russelfile.toml" ] && [ -f "examples/$ex/Dockerfile" ]; then
			EXAMPLE_APPS+=("$ex")
		fi
	done

	# Port base: Russel microVM and container each get distinct host ports
	PORT_BASE=18080

	for example in "${EXAMPLE_APPS[@]}"; do
		header "  Race: ${example}"

		repo_path="$(realpath "examples/$example")"
		guest_port=$(grep -oP '(?<=^port = )\d+' "examples/$example/Russelfile.toml" | head -1)
		ready_path=$(get_ready_path "$example")
		EXAMPLES+=("$example")

		# ── Russel microVM ──────────────────────────────────────────────────
		if [ "$RUSSEL_MICROVM_SKIPPED" -eq 0 ]; then
			host_port=$PORT_BASE
			PORT_BASE=$((PORT_BASE + 1))
			vm_id="bench-mvm-${example}"
			info "russel microVM: deploy $vm_id :$host_port → :$guest_port"
			russel_deploy_one "microvm" "$repo_path" "$vm_id" "$host_port" "$guest_port" "$ready_path" "$example"
			case "$out_status" in
			ok)
				RUSSEL_MICROVM_DEPLOY_MS+=("$out_deploy_ms")
				RUSSEL_MICROVM_CURL_MS+=("$out_curl_ms")
				RUSSEL_MICROVM_SPAWN_MS+=("$out_spawn_ms")
				pass "russel microVM: ${out_deploy_ms}ms deploy, +${out_curl_ms}ms curl"
				;;
			timeout)
				RUSSEL_MICROVM_DEPLOY_MS+=("$out_deploy_ms")
				RUSSEL_MICROVM_CURL_MS+=("timeout")
				RUSSEL_MICROVM_SPAWN_MS+=("timeout")
				warn "russel microVM: deployed ${out_deploy_ms}ms but curl timeout"
				;;
			*)
				RUSSEL_MICROVM_DEPLOY_MS+=("failed")
				RUSSEL_MICROVM_CURL_MS+=("failed")
				RUSSEL_MICROVM_SPAWN_MS+=("failed")
				fail "russel microVM: ${out_status#failed:}"
				;;
			esac
			russel-cli destroy "$vm_id" &>/dev/null || true
		else
			RUSSEL_MICROVM_DEPLOY_MS+=("skipped")
			RUSSEL_MICROVM_CURL_MS+=("skipped")
			RUSSEL_MICROVM_SPAWN_MS+=("skipped")
		fi

		# ── Russel container (rootless podman) ──────────────────────────────
		if [ "$RUSSEL_CONTAINER_SKIPPED" -eq 0 ]; then
			host_port=$PORT_BASE
			PORT_BASE=$((PORT_BASE + 1))
			vm_id="bench-ctr-${example}"
			info "russel container: deploy $vm_id :$host_port → :$guest_port"
			russel_deploy_one "container" "$repo_path" "$vm_id" "$host_port" "$guest_port" "$ready_path" "$example"
			case "$out_status" in
			ok)
				RUSSEL_CONTAINER_DEPLOY_MS+=("$out_deploy_ms")
				RUSSEL_CONTAINER_CURL_MS+=("$out_curl_ms")
				RUSSEL_CONTAINER_SPAWN_MS+=("$out_spawn_ms")
				pass "russel container: ${out_deploy_ms}ms deploy, +${out_curl_ms}ms curl"
				;;
			timeout)
				RUSSEL_CONTAINER_DEPLOY_MS+=("$out_deploy_ms")
				RUSSEL_CONTAINER_CURL_MS+=("timeout")
				RUSSEL_CONTAINER_SPAWN_MS+=("timeout")
				warn "russel container: deployed ${out_deploy_ms}ms but curl timeout"
				;;
			*)
				RUSSEL_CONTAINER_DEPLOY_MS+=("failed")
				RUSSEL_CONTAINER_CURL_MS+=("failed")
				RUSSEL_CONTAINER_SPAWN_MS+=("failed")
				fail "russel container: ${out_status#failed:}"
				;;
			esac
			russel-cli destroy "$vm_id" &>/dev/null || true
		else
			RUSSEL_CONTAINER_DEPLOY_MS+=("skipped")
			RUSSEL_CONTAINER_CURL_MS+=("skipped")
			RUSSEL_CONTAINER_SPAWN_MS+=("skipped")
		fi

		# ── Raw Docker/podman baseline ──────────────────────────────────────
		docker_boot_ms=0

		if [ -n "$RUNTIME" ]; then
			image_tag="russel-bench-$example"
			container_name="russel-bench-$example"

			set +e
			info "baseline $RUNTIME: building image..."
			build_start=$(date +%s%N)
			build_out=$("$RUNTIME" build -t "$image_tag" "$repo_path" 2>&1) || true
			build_end=$(date +%s%N)
			build_ms=$(((build_end - build_start) / 1000000))

			if ! "$RUNTIME" image inspect "$image_tag" &>/dev/null; then
				build_err=$(echo "$build_out" | grep -i 'error:' | head -1 || echo "unknown error")
				DOCKER_BUILD_MS+=("failed")
				DOCKER_BOOT_MS+=("failed")
				WINNER+=("—")
				fail "$RUNTIME: build failed (${build_ms}ms): $build_err"
			else
				pass "$RUNTIME: image built in ${build_ms}ms"
				DOCKER_BUILD_MS+=("$build_ms")

				"$RUNTIME" rm -f "$container_name" &>/dev/null || true

				boot_start=$(date +%s%N)
				cid=$("$RUNTIME" run -d --name "$container_name" -P --memory=256m "$image_tag" 2>/dev/null || echo "")
				if [ -z "$cid" ]; then
					DOCKER_BOOT_MS+=("failed")
					WINNER+=("—")
					fail "$RUNTIME: failed to start container"
				else
					dhp=""
					for _ in $(seq 1 100); do
						dhp=$("$RUNTIME" port "$container_name" "$guest_port" 2>/dev/null | head -1 | grep -oP '\d+$' || echo "")
						if [ -n "$dhp" ]; then break; fi
						sleep 0.1
					done
					if [ -z "$dhp" ]; then
						DOCKER_BOOT_MS+=("failed")
						WINNER+=("—")
						warn "$RUNTIME: could not determine host port"
					else
						boot_ok=0
						for _ in $(seq 1 100); do
							if curl -sf "http://127.0.0.1:$dhp$ready_path" >/dev/null 2>&1; then
								boot_ok=1
								break
							fi
							sleep 0.1
						done
						boot_end=$(date +%s%N)
						if [ "$boot_ok" -eq 1 ]; then
							docker_boot_ms=$(((boot_end - boot_start) / 1000000))
							DOCKER_BOOT_MS+=("$docker_boot_ms")
							pass "$RUNTIME: container ready in ${docker_boot_ms}ms"

							# Winner among successful e2e totals (deploy+curl or build+boot)
							best_name="—"
							best_ms=999999999
							if [ "${RUSSEL_MICROVM_DEPLOY_MS[-1]}" != "skipped" ] && [ "${RUSSEL_MICROVM_DEPLOY_MS[-1]}" != "failed" ] &&
								[ "${RUSSEL_MICROVM_CURL_MS[-1]}" != "timeout" ] && [ "${RUSSEL_MICROVM_CURL_MS[-1]}" != "failed" ]; then
								t=$((RUSSEL_MICROVM_DEPLOY_MS[-1] + RUSSEL_MICROVM_CURL_MS[-1]))
								if [ "$t" -lt "$best_ms" ]; then
									best_ms=$t
									best_name="Russel-mVM"
								fi
							fi
							if [ "${RUSSEL_CONTAINER_DEPLOY_MS[-1]}" != "skipped" ] && [ "${RUSSEL_CONTAINER_DEPLOY_MS[-1]}" != "failed" ] &&
								[ "${RUSSEL_CONTAINER_CURL_MS[-1]}" != "timeout" ] && [ "${RUSSEL_CONTAINER_CURL_MS[-1]}" != "failed" ]; then
								t=$((RUSSEL_CONTAINER_DEPLOY_MS[-1] + RUSSEL_CONTAINER_CURL_MS[-1]))
								if [ "$t" -lt "$best_ms" ]; then
									best_ms=$t
									best_name="Russel-ctr"
								fi
							fi
							d_total=$((build_ms + docker_boot_ms))
							if [ "$d_total" -lt "$best_ms" ]; then
								best_name="${RUNTIME^}"
							fi
							WINNER+=("$best_name")
						else
							DOCKER_BOOT_MS+=("timeout")
							WINNER+=("—")
							warn "$RUNTIME: container started but HTTP never ready"
						fi
					fi
				fi
			fi

			"$RUNTIME" rm -f "$container_name" &>/dev/null || true
			if [ "$COLD" -eq 1 ]; then
				"$RUNTIME" rmi "$image_tag" &>/dev/null || true
			fi
			set -euo pipefail
		else
			DOCKER_BUILD_MS+=("skipped")
			DOCKER_BOOT_MS+=("skipped")
			WINNER+=("—")
		fi

		echo ""
	done

	# ── 10c. Comparison table ──────────────────────────────────────────────────
	header "App Boot Race: Results"

	fmt_e2e() {
		local d="$1" c="$2"
		if [ "$d" = "skipped" ]; then
			echo "skipped"
		elif [ "$d" = "failed" ]; then
			echo "failed"
		elif [ "$c" = "timeout" ]; then
			echo "timeout (${d}+?)"
		elif [ "$c" = "failed" ]; then
			echo "failed"
		else
			echo "$((d + c))ms (${d}+${c})"
		fi
	}

	fmt_spawn() {
		local s="$1"
		if [ "$s" = "skipped" ] || [ "$s" = "failed" ] || [ "$s" = "timeout" ]; then
			echo "$s"
		else
			echo "${s}ms"
		fi
	}

	if [ "${#EXAMPLES[@]}" -gt 0 ]; then
		runtime_label="${RUNTIME^}"
		[ -z "$runtime_label" ] && runtime_label="Docker"

		echo ""
		echo -e "  ${BOLD}End-to-End (build/deploy + HTTP ready)${NC}"
		echo ""
		echo -e "  ${DIM}┌──────────────┬──────────────────┬──────────────────┬──────────────────┬────────────┐${NC}"
		echo -e "  ${DIM}│ App          │ Russel microVM   │ Russel container │ ${runtime_label} baseline  │ Winner     │${NC}"
		echo -e "  ${DIM}├──────────────┼──────────────────┼──────────────────┼──────────────────┼────────────┤${NC}"
		for i in "${!EXAMPLES[@]}"; do
			name="${EXAMPLES[$i]}"
			mvm_col=$(fmt_e2e "${RUSSEL_MICROVM_DEPLOY_MS[$i]}" "${RUSSEL_MICROVM_CURL_MS[$i]}")
			ctr_col=$(fmt_e2e "${RUSSEL_CONTAINER_DEPLOY_MS[$i]}" "${RUSSEL_CONTAINER_CURL_MS[$i]}")
			dc_b="${DOCKER_BUILD_MS[$i]}"
			dc_r="${DOCKER_BOOT_MS[$i]}"
			if [ "$dc_b" = "skipped" ]; then
				dc_col="skipped"
			elif [ "$dc_b" = "failed" ] || [ "$dc_r" = "failed" ]; then
				dc_col="failed"
			elif [ "$dc_r" = "timeout" ]; then
				dc_col="timeout"
			else
				dc_col="$((dc_b + dc_r))ms (${dc_b}+${dc_r})"
			fi
			winner="${WINNER[$i]:-—}"
			printf "  ${DIM}│${NC} %-12s ${DIM}│${NC} %-16s ${DIM}│${NC} %-16s ${DIM}│${NC} %-16s ${DIM}│${NC} %-10s ${DIM}│${NC}\n" \
				"$name" "$mvm_col" "$ctr_col" "$dc_col" "$winner"
		done
		echo -e "  ${DIM}└──────────────┴──────────────────┴──────────────────┴──────────────────┴────────────┘${NC}"

		echo ""
		echo -e "  ${DIM}Note:${NC}"
		echo -e "  ${DIM}  Russel microVM:  nix build → initramfs → TAP/socat → cloud-hypervisor → curl${NC}"
		echo -e "  ${DIM}  Russel container: nix build → rootfs adapter → rootless podman --rootfs → curl${NC}"
		echo -e "  ${DIM}  ${runtime_label} baseline: Dockerfile build + run → curl (not Russel)${NC}"
		echo -e "  ${DIM}  Russel deploy API sets runtime=microvm|container; ports via -p style host/guest.${NC}"
		echo -e "  ${DIM}  Warm runs reuse nix/store and image layers; use --cold for cold builds.${NC}"
		echo ""
		[ "$RUSSEL_MICROVM_SKIPPED" -eq 1 ] && echo -e "  ${YELLOW}  ⚠ Russel microVM skipped — see prereqs above.${NC}"
		[ "$RUSSEL_CONTAINER_SKIPPED" -eq 1 ] && echo -e "  ${YELLOW}  ⚠ Russel container skipped — need rootless podman + root for this script.${NC}"

		echo ""
		echo -e "  ${BOLD}Spawn-to-Ready (excluding build)${NC}"
		echo -e "  ${DIM}(Russel subtracts nix build_ms; baseline is run→HTTP only)${NC}"
		echo ""
		echo -e "  ${DIM}┌──────────────┬──────────────────┬──────────────────┬──────────────────┐${NC}"
		echo -e "  ${DIM}│ App          │ Russel microVM   │ Russel container │ ${runtime_label} baseline  │${NC}"
		echo -e "  ${DIM}├──────────────┼──────────────────┼──────────────────┼──────────────────┤${NC}"
		for i in "${!EXAMPLES[@]}"; do
			name="${EXAMPLES[$i]}"
			printf "  ${DIM}│${NC} %-12s ${DIM}│${NC} %-16s ${DIM}│${NC} %-16s ${DIM}│${NC} %-16s ${DIM}│${NC}\n" \
				"$name" \
				"$(fmt_spawn "${RUSSEL_MICROVM_SPAWN_MS[$i]}")" \
				"$(fmt_spawn "${RUSSEL_CONTAINER_SPAWN_MS[$i]}")" \
				"$(fmt_spawn "${DOCKER_BOOT_MS[$i]}")"
		done
		echo -e "  ${DIM}└──────────────┴──────────────────┴──────────────────┴──────────────────┘${NC}"

		# ── Timing breakdown for last successful Russel deploy ──────────
		if [ -n "$LAST_RUSSEL_COMPLETE" ]; then
			echo ""
			echo -e "  ${BOLD}Last Russel deploy (${LAST_RUSSEL_RUNTIME:-?}) — phase breakdown:${NC}"
			echo ""
			res_ms=$(json_int "resolve_ms" "$LAST_RUSSEL_COMPLETE")
			bld_ms=$(json_int "build_ms" "$LAST_RUSSEL_COMPLETE")
			crt_ms=$(json_int "create_ms" "$LAST_RUSSEL_COMPLETE")
			net_ms=$(json_int "network_ms" "$LAST_RUSSEL_COMPLETE")
			stt_ms=$(json_int "start_ms" "$LAST_RUSSEL_COMPLETE")
			rdy_ms=$(json_int "ready_ms" "$LAST_RUSSEL_COMPLETE")
			if [ "${LAST_RUSSEL_RUNTIME:-}" = "container" ]; then
				crt_detail="prepare Docker-like rootfs"
				net_detail="(n/a for container; often 0)"
				stt_detail="rootless podman run --rootfs"
				rdy_detail="host TCP/HTTP ready"
			else
				crt_detail="build minimal initramfs"
				net_detail="TAP + socat port fwd"
				stt_detail="virtiofsd + cloud-hypervisor"
				rdy_detail="guest TCP socket live"
			fi
			echo -e "  ${DIM}┌────────────┬────────┬──────────────────────────────────────────────┐${NC}"
			echo -e "  ${DIM}│ Phase      │ Time   │ Detail                                      │${NC}"
			echo -e "  ${DIM}├────────────┼────────┼──────────────────────────────────────────────┤${NC}"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "resolve" "${res_ms}ms" "repo + Russelfile"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "build" "${bld_ms}ms" "nix build (package)"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "create" "${crt_ms}ms" "$crt_detail"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "network" "${net_ms}ms" "$net_detail"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "start" "${stt_ms}ms" "$stt_detail"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "ready" "${rdy_ms}ms" "$rdy_detail"
			r_total=$((res_ms + bld_ms + crt_ms + net_ms + stt_ms + rdy_ms))
			echo -e "  ${DIM}├────────────┼────────┼──────────────────────────────────────────────┤${NC}"
			printf "  ${DIM}│${NC} ${BOLD}%-10s${NC} ${DIM}│${NC} ${BOLD}%5s${NC} │ ${BOLD}%-44s${NC} ${DIM}│${NC}\n" "total" "${r_total}ms" "sum of phases"
			echo -e "  ${DIM}└────────────┴────────┴──────────────────────────────────────────────┘${NC}"
		fi
	else
		warn "no examples were benchmarked"
	fi

# ── Cleanup is handled by the EXIT trap ──────────────────────────────────────

fi # end of comparison section guard

# ── Summary ──────────────────────────────────────────────────────────────────
bench_end=$(date +%s%N)
bench_total=$(((bench_end - bench_start) / 1000000000))

echo -e "\n${BOLD}
  ┌──────────────────────────────────────────────┐
  │  ${GREEN}✓ bench complete${NC}${BOLD}                         │
  │  ${DIM}total: ${bench_total}s                           ${NC}${BOLD}│
  │                                              │
  │  ${CYAN}clean build${NC}${BOLD}       ${clean_ms}ms                     │
  │  ${CYAN}release build${NC}${BOLD}     ${rel_ms}ms                     │
  │  ${CYAN}tests${NC}${BOLD}            ${test_ms}ms                     │
  │  ${CYAN}test count${NC}${BOLD}       ${test_passed}                         │
  └──────────────────────────────────────────────┘${NC}
"
