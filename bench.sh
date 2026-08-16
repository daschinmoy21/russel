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
#   3. Raw podman/docker — Dockerfile build + run (baseline; skipped if no Dockerfile)
#
# All examples/*/Russelfile.toml are raced. Temp Russelfiles inject type for the
# race (file type is ignored for fairness). Apps without Dockerfile skip path 3.
#
# Run from repo root after `nix develop` or with Rust toolchain on PATH.
# Use --cold to force cold builds (no layer/nix cache); default is --warm.
# --warm keeps images and nix store paths for fair warm-vs-warm comparison.
#
# MicroVM races REQUIRE the Russel-compiled kernel (flake .#microvm-kernel,
# virtio drivers built-in). The script builds it and exports RUSSEL_KERNEL_PATH
# so russel-ctrl never falls back to a stock nixpkgs kernel. You may pre-set
# RUSSEL_KERNEL_PATH to an existing bzImage; it must exist or microVM is skipped.
# ──────────────────────────────────────────────────────────────────────────────

# shellcheck source=bench-common.sh disable=SC1091
source "$(dirname "$(readlink -f "$0")")/bench-common.sh"

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
		echo "  MicroVM path requires flake .#microvm-kernel (sets RUSSEL_KERNEL_PATH)." >&2
		exit 0
		;;
	*)
		echo "Unknown: $1" >&2
		exit 1
		;;
	esac
done

bench_start=$(date +%s%N)

# One bench run at a time: they swap the host's /var/lib state.
bench_run_lock

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
		# Skip empty slots left when the array is cleared with "${arr[@]:-}"
		[ -n "$vm" ] || continue
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
	bench_restore_var_lib
	# Remove temp files
	bench_cleanup_tmp_paths
	# Remove the bench secret if this run set it (best effort)
	if [ "${BENCH_SECRET_SET:-0}" -eq 1 ] && command -v russel-cli &>/dev/null; then
		russel-cli secrets delete DEMO_SECRET &>/dev/null || warn "failed to delete DEMO_SECRET"
	fi
}
trap cleanup EXIT

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

# Inject flake devShell PATH so cloud-hypervisor/virtiofsd/socat are found
# without a global install. Works under sudo when run from the repo root.
if [ "$has_nix" -eq 1 ] && ! command -v cloud-hypervisor >/dev/null 2>&1; then
	# PATH must expand inside the nix develop shell, not here.
	# shellcheck disable=SC2016
	FLAKE_PATH=$(nix develop -c sh -c 'printf %s "$PATH"' 2>/dev/null || echo "")
	if [ -n "$FLAKE_PATH" ]; then
		export PATH="$FLAKE_PATH:$PATH"
	fi
fi

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
		basic-http | hello-rust | env-config | shortlink | microvm-http) echo "/health" ;;
		*) echo "/" ;;
		esac
	}

	# Ensure env-config can resolve secret://DEMO_SECRET at deploy time.
	ensure_bench_secrets() {
		local example="$1"
		if [ "$example" != "env-config" ]; then
			return 0
		fi
		if ! command -v russel-cli &>/dev/null; then
			warn "env-config: russel-cli missing; DEMO_SECRET not set"
			return 0
		fi
		if printf '%s' 'bench-secret' | russel-cli secrets set DEMO_SECRET &>/dev/null; then
			BENCH_SECRET_SET=1
			info "env-config: set DEMO_SECRET for secret:// resolution"
		else
			warn "env-config: failed to set DEMO_SECRET (deploy may fail)"
		fi
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
		ensure_bench_secrets "$example"

		if [ "$COLD" -eq 1 ]; then
			local local_result="examples/$example/result"
			if [ -L "$local_result" ]; then
				local result_path
				result_path=$(readlink -f "$local_result")
				rm -f "$local_result"
				nix-store --delete "$result_path" 2>/dev/null || true
			fi
		fi

		# Inject type under [service] so each race path is forced (microvm|container),
		# independent of the example's committed type default.
		local cfg_name="Russelfile.bench-${runtime_kind}.toml"
		local cfg_path="$repo_path/$cfg_name"
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
			# Track for EXIT trap; removed again after successful per-race destroy.
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
			# Surface guest/ctrl diagnostics when deploy fails.
			if [ -n "${RUSSEL_LOG:-}" ] && [ -f "$RUSSEL_LOG" ]; then
				warn "last 60 lines of russel-ctrl log ($RUSSEL_LOG):"
				tail -60 "$RUSSEL_LOG" 2>/dev/null | while IFS= read -r line; do info "  $line"; done || true
			fi
			if [ -n "${RUSSEL_STATE_DIR:-}" ]; then
				clog=$(find "$RUSSEL_STATE_DIR" -name console.log 2>/dev/null | head -1 || true)
				if [ -n "$clog" ] && [ -f "$clog" ]; then
					warn "guest console tail ($clog):"
					tail -40 "$clog" 2>/dev/null | while IFS= read -r line; do info "  $line"; done || true
				fi
			fi
		fi
		set -euo pipefail
	}

	# ── 10a. Russel setup ──────────────────────────────────────────────────────
	RUSSEL_MICROVM_SKIPPED=0
	RUSSEL_CONTAINER_SKIPPED=0

	if [ "$RUSSEL_CTRL_NEEDED" -eq 1 ]; then
		info "Russel paths: microVM=$RUSSEL_MICROVM_CAPABLE container=$RUSSEL_CONTAINER_CAPABLE"

		# ── Custom microvm kernel (required for fair microVM bench numbers) ──
		# Stock nixpkgs kernels load virtio modules slowly / may miss modules.
		# Always pin RUSSEL_KERNEL_PATH to flake .#microvm-kernel (or a prebuilt
		# path the user already exported).
		if [ "$RUSSEL_MICROVM_CAPABLE" -eq 1 ]; then
			if [ -n "${RUSSEL_KERNEL_PATH:-}" ] && [ -f "$RUSSEL_KERNEL_PATH" ]; then
				pass "using RUSSEL_KERNEL_PATH=$RUSSEL_KERNEL_PATH"
			elif [ "$has_nix" -eq 1 ] && [ -f "flake.nix" ]; then
				info "building required microvm kernel (nix build .#microvm-kernel)..."
				kernel_out=$(nix build .#microvm-kernel --print-out-paths --no-link 2>/dev/null || true)
				if [ -z "$kernel_out" ]; then
					warn "nix build .#microvm-kernel failed"
				else
					# Package may be the dir containing bzImage, or the bzImage itself.
					if [ -f "$kernel_out/bzImage" ]; then
						RUSSEL_KERNEL_PATH="$kernel_out/bzImage"
					elif [ -f "$kernel_out" ]; then
						RUSSEL_KERNEL_PATH="$kernel_out"
					else
						RUSSEL_KERNEL_PATH=""
					fi
				fi
				if [ -n "${RUSSEL_KERNEL_PATH:-}" ] && [ -f "$RUSSEL_KERNEL_PATH" ]; then
					export RUSSEL_KERNEL_PATH
					pass "RUSSEL_KERNEL_PATH=$RUSSEL_KERNEL_PATH (custom microvm-kernel)"
				else
					warn "could not resolve bzImage from .#microvm-kernel (out=${kernel_out:-none})"
					RUSSEL_KERNEL_PATH=""
				fi
			else
				warn "nix/flake unavailable and RUSSEL_KERNEL_PATH unset"
				RUSSEL_KERNEL_PATH=""
			fi

			if [ -z "${RUSSEL_KERNEL_PATH:-}" ] || [ ! -f "$RUSSEL_KERNEL_PATH" ]; then
				fail "microVM bench requires the compiled kernel (.#microvm-kernel → RUSSEL_KERNEL_PATH)"
				info "hint: nix build .#microvm-kernel -o result-kernel"
				info "      export RUSSEL_KERNEL_PATH=\$(readlink -f result-kernel/bzImage)"
				info "      sudo -E env RUSSEL_KERNEL_PATH=\"\$RUSSEL_KERNEL_PATH\" ./bench.sh"
				RUSSEL_MICROVM_CAPABLE=0
				RUSSEL_MICROVM_SKIPPED=1
			else
				export RUSSEL_KERNEL_PATH
			fi
		fi

		# Build release binaries if missing
		if [ ! -f "$RELEASE_DIR/russel-ctrl" ] || [ ! -f "$RELEASE_DIR/russel-cli" ]; then
			info "building russel (release) for deploy comparison..."
			cargo build --release -q 2>&1
		fi

		export PATH="$RELEASE_DIR:$PATH"

		# Pass podman user identity to russel-ctrl (Issue #278598)
		export RUSSEL_PODMAN_USER
		# Ensure custom kernel is visible to the control plane process tree.
		export RUSSEL_KERNEL_PATH="${RUSSEL_KERNEL_PATH:-}"

		RUSSEL_STATE_DIR=$(mktemp -d /tmp/russel-bench-XXXXXX)
		export RUSSEL_CTRL_ADDR="127.0.0.1:7878"

		mkdir -p "$RUSSEL_STATE_DIR/lib/russel" "$RUSSEL_STATE_DIR/lib/microvms"
		# mktemp is 0700 root-owned; rootless podman (SUDO_USER) must traverse
		# /var/lib/russel → this tree to faccessat the prepared rootfs.
		chmod 755 "$RUSSEL_STATE_DIR" "$RUSSEL_STATE_DIR/lib" \
			"$RUSSEL_STATE_DIR/lib/russel" "$RUSSEL_STATE_DIR/lib/microvms"
		# Consumed by bench_restore_var_lib in the EXIT trap (bench-common.sh).
		# shellcheck disable=SC2034
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
	# Prefer a stable explicit order covering all shipped demos; also pick up any
	# future examples/*/Russelfile.toml not in the list.
	EXAMPLE_APPS=()
	for ex in basic-http microvm-http hello-rust env-config shortlink static-test filebrowser; do
		if [ -f "examples/$ex/Russelfile.toml" ]; then
			EXAMPLE_APPS+=("$ex")
		fi
	done
	for rf in examples/*/Russelfile.toml; do
		[ -f "$rf" ] || continue
		ex=$(basename "$(dirname "$rf")")
		already=0
		for a in "${EXAMPLE_APPS[@]:-}"; do
			if [ "$a" = "$ex" ]; then already=1; break; fi
		done
		if [ "$already" -eq 0 ]; then
			EXAMPLE_APPS+=("$ex")
		fi
	done
	info "racing ${#EXAMPLE_APPS[@]} examples: ${EXAMPLE_APPS[*]}"

	# Pick winner among successful e2e totals for the current example (last array slots).
	pick_winner() {
		local best_name="—"
		local best_ms=999999999
		local t d_total
		if [ "${RUSSEL_MICROVM_DEPLOY_MS[-1]}" != "skipped" ] && [ "${RUSSEL_MICROVM_DEPLOY_MS[-1]}" != "failed" ] &&
			[ "${RUSSEL_MICROVM_CURL_MS[-1]}" != "timeout" ] && [ "${RUSSEL_MICROVM_CURL_MS[-1]}" != "failed" ] &&
			[ "${RUSSEL_MICROVM_CURL_MS[-1]}" != "skipped" ]; then
			t=$((RUSSEL_MICROVM_DEPLOY_MS[-1] + RUSSEL_MICROVM_CURL_MS[-1]))
			if [ "$t" -lt "$best_ms" ]; then
				best_ms=$t
				best_name="Russel-mVM"
			fi
		fi
		if [ "${RUSSEL_CONTAINER_DEPLOY_MS[-1]}" != "skipped" ] && [ "${RUSSEL_CONTAINER_DEPLOY_MS[-1]}" != "failed" ] &&
			[ "${RUSSEL_CONTAINER_CURL_MS[-1]}" != "timeout" ] && [ "${RUSSEL_CONTAINER_CURL_MS[-1]}" != "failed" ] &&
			[ "${RUSSEL_CONTAINER_CURL_MS[-1]}" != "skipped" ]; then
			t=$((RUSSEL_CONTAINER_DEPLOY_MS[-1] + RUSSEL_CONTAINER_CURL_MS[-1]))
			if [ "$t" -lt "$best_ms" ]; then
				best_ms=$t
				best_name="Russel-ctr"
			fi
		fi
		if [ "${DOCKER_BUILD_MS[-1]}" != "skipped" ] && [ "${DOCKER_BUILD_MS[-1]}" != "failed" ] &&
			[ "${DOCKER_BOOT_MS[-1]}" != "skipped" ] && [ "${DOCKER_BOOT_MS[-1]}" != "failed" ] &&
			[ "${DOCKER_BOOT_MS[-1]}" != "timeout" ]; then
			d_total=$((DOCKER_BUILD_MS[-1] + DOCKER_BOOT_MS[-1]))
			if [ "$d_total" -lt "$best_ms" ]; then
				best_name="${RUNTIME^}"
			fi
		fi
		echo "$best_name"
	}

	# Port base: Russel microVM and container each get distinct host ports
	PORT_BASE=18080

	for example in "${EXAMPLE_APPS[@]}"; do
		header "  Race: ${example}"

		repo_path="$(realpath "examples/$example")"
		guest_port=$(grep -oP '(?<=^port = )\d+' "examples/$example/Russelfile.toml" | head -1)
		if [ -z "$guest_port" ]; then
			warn "no port = N in Russelfile; skipping $example"
			continue
		fi
		ready_path=$(get_ready_path "$example")
		has_dockerfile=0
		[ -f "examples/$example/Dockerfile" ] && has_dockerfile=1
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
			# Already destroyed; drop from EXIT cleanup list so trap does not re-destroy.
			_kept=()
			for _v in "${RUSSEL_VMS_CREATED[@]:-}"; do
				[ -n "$_v" ] && [ "$_v" != "$vm_id" ] && _kept+=("$_v")
			done
			RUSSEL_VMS_CREATED=()
			if [ "${#_kept[@]}" -gt 0 ]; then
				RUSSEL_VMS_CREATED=("${_kept[@]}")
			fi
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
			_kept=()
			for _v in "${RUSSEL_VMS_CREATED[@]:-}"; do
				[ -n "$_v" ] && [ "$_v" != "$vm_id" ] && _kept+=("$_v")
			done
			RUSSEL_VMS_CREATED=()
			if [ "${#_kept[@]}" -gt 0 ]; then
				RUSSEL_VMS_CREATED=("${_kept[@]}")
			fi
		else
			RUSSEL_CONTAINER_DEPLOY_MS+=("skipped")
			RUSSEL_CONTAINER_CURL_MS+=("skipped")
			RUSSEL_CONTAINER_SPAWN_MS+=("skipped")
		fi

		# ── Raw Docker/podman baseline (Dockerfile required) ────────────────
		docker_boot_ms=0

		if [ -n "$RUNTIME" ] && [ "$has_dockerfile" -eq 1 ]; then
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
				fail "$RUNTIME: build failed (${build_ms}ms): $build_err"
			else
				pass "$RUNTIME: image built in ${build_ms}ms"
				DOCKER_BUILD_MS+=("$build_ms")

				"$RUNTIME" rm -f "$container_name" &>/dev/null || true

				boot_start=$(date +%s%N)
				cid=$("$RUNTIME" run -d --name "$container_name" -P --memory=256m "$image_tag" 2>/dev/null || echo "")
				if [ -z "$cid" ]; then
					DOCKER_BOOT_MS+=("failed")
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
						else
							DOCKER_BOOT_MS+=("timeout")
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
			if [ -n "$RUNTIME" ] && [ "$has_dockerfile" -eq 0 ]; then
				info "baseline $RUNTIME: skipped (no Dockerfile)"
			fi
			DOCKER_BUILD_MS+=("skipped")
			DOCKER_BOOT_MS+=("skipped")
		fi

		WINNER+=("$(pick_winner)")

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
		echo -e "  ${DIM}  ${runtime_label} baseline: Dockerfile build + run → curl (not Russel; skipped if no Dockerfile)${NC}"
		echo -e "  ${DIM}  Russel deploy API sets runtime=microvm|container; ports via -p style host/guest.${NC}"
		echo -e "  ${DIM}  Warm runs reuse nix/store and image layers; use --cold for cold builds.${NC}"
		echo -e "  ${DIM}  Examples raced: all examples/*/Russelfile.toml (new apps included).${NC}"
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
