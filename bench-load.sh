#!/usr/bin/env bash
set -euo pipefail

# ──────────────────────────────────────────────────────────────────────────────
# bench-load.sh — Go sequential load benchmark (basic-http only)
#
# Load-tests examples/basic-http (Go) against three runtimes one after another:
#   1. Russel microVM
#   2. Russel container (rootless Podman --rootfs via control plane)
#   3. Raw Podman (Dockerfile baseline)
#
# Each path: deploy/start → wait /health → warm-up → main load → resources → destroy.
# All paths pinned: configurable VCPUs (default 1), 256 MiB memory.
# ──────────────────────────────────────────────────────────────────────────────

# shellcheck source=bench-common.sh disable=SC1091
source "$(dirname "$(readlink -f "$0")")/bench-common.sh"

# ── Defaults (overridable via env) ───────────────────────────────────────────
: "${LOAD_DURATION:=30s}"
: "${LOAD_CONCURRENCY:=50}"
: "${HOST_PORT:=19080}"
: "${MEM_MB:=256}"
: "${CPU_LIMIT:=1}"
: "${VCPUS:=1}"
: "${CPUSET:=0}"

USE_CPU_CAP=1
case "${CPU_LIMIT}" in
0 | none | off | false | "") USE_CPU_CAP=0 ;;
esac

if [ "$USE_CPU_CAP" -eq 1 ]; then
	CPU_LABEL="${CPU_LIMIT} CPU"
else
	CPU_LABEL="no CPU cap"
fi

usage() {
	cat <<'EOF'
Usage: ./bench-load.sh [--help]

  Sequential load benchmark: Go basic-http against Russel microVM,
  Russel container (rootless podman), and raw Podman baseline.
  All paths pinned to configurable VCPUs (default 1), 256 MiB memory, same host port (sequential).

Environment (overridable):
  LOAD_DURATION        Duration-based load e.g. 30|30s|3m|500ms|1m30s (default: 30s)
  LOAD_CONCURRENCY     Concurrent connections (default: 50)
  HOST_PORT            Host port for all paths (default: 19080)
  CPU_LIMIT            CPU quota for --cpus=N (default: 1). 0/none/off = no
                     CPU cap, no cpuset, no microVM taskset.
  VCPUS                microVM guest vCPUs (default: 1). Injected into
                     Russelfile for microVM path.
  MEM_MB               Memory limit in MiB (default: 256)
  CPUSET               CPU set for pinning (default: 0). Ignored when
                     CPU_LIMIT=0 or cpuset controller unavailable.

Requires: root (for /var/lib redirect), hey (or nix), podman (rootless for
Russel container), cloud-hypervisor + KVM (microVM), Russel kernel
(RUSSEL_KERNEL_PATH or nix build .#microvm-kernel).

Paths run strictly one after another — never in parallel.
EOF
	exit 0
}

while [[ $# -gt 0 ]]; do
	case "$1" in
	--help | -h) usage ;;
	*)
		echo "Unknown: $1" >&2
		exit 1
		;;
	esac
done

# ── Duration normalisation (hey accepts original via -z, sampler needs int secs) ─
duration_to_seconds() {
	local d="$1" secs=0
	local rest="$d"
	while [ -n "$rest" ]; do
		if [[ "$rest" =~ ^([0-9]+)ms ]]; then
			local ms=${BASH_REMATCH[1]}
			if [ "$ms" -gt 0 ] 2>/dev/null; then
				secs=$((secs + (ms + 999) / 1000))
			fi
			rest="${rest#"${BASH_REMATCH[0]}"}"
		elif [[ "$rest" =~ ^([0-9]+)s ]]; then
			secs=$((secs + BASH_REMATCH[1]))
			rest="${rest#"${BASH_REMATCH[0]}"}"
		elif [[ "$rest" =~ ^([0-9]+)m ]]; then
			secs=$((secs + BASH_REMATCH[1] * 60))
			rest="${rest#"${BASH_REMATCH[0]}"}"
		elif [[ "$rest" =~ ^[0-9]+$ ]]; then
			secs=$((secs + rest))
			rest=""
		else
			return 1
		fi
	done
	echo "$secs"
}
LOAD_DURATION_SECS=$(duration_to_seconds "$LOAD_DURATION") || {
	fail "Invalid LOAD_DURATION: '$LOAD_DURATION' (expected e.g. 30, 30s, 3m, 500ms, 1m30s)"
	exit 1
}

# One bench run at a time: they swap the host's /var/lib state.
bench_run_lock

echo -e "${BOLD}
  ┌──────────────────────────────────────────────┐
  │    bench-load — Go basic-http load test       │
  │    $(date +%Y-%m-%d\ %H:%M)                         │
  │    ${CPU_LABEL} · VCPUs=${VCPUS} · ${MEM_MB}MiB · ${LOAD_DURATION} · c${LOAD_CONCURRENCY}                   │
  └──────────────────────────────────────────────┘${NC}"

# ── Cleanup trap ─────────────────────────────────────────────────────────────
CTRL_PID=""
CLEANUP_RAN=0
VAR_LIB_REDIRECTED=0
RUSSEL_STATE_BAK=""
MICROVMS_STATE_BAK=""
BENCH_CARGO_TARGET=""
RUSSEL_STATE_DIR=""
LOAD_RAW_CONTAINER="bench-load-raw"

if [ "$EUID" -eq 0 ]; then
	BENCH_CARGO_TARGET=$(mktemp -d /tmp/russel-benchload-target-XXXXXX)
	export CARGO_TARGET_DIR="$BENCH_CARGO_TARGET"
fi
RELEASE_DIR="${CARGO_TARGET_DIR:-$PWD/target}/release"

cleanup() {
	if [ "$CLEANUP_RAN" -eq 1 ]; then return; fi
	CLEANUP_RAN=1

	# Destroy any bench VMs
	for vm in "${BENCH_VMS_CREATED[@]:-}"; do
		[ -n "$vm" ] || continue
		if command -v russel &>/dev/null; then
			info "cleanup: destroy $vm"
			timeout --signal=TERM --kill-after=2s 15s russel destroy "$vm" &>/dev/null || true
		fi
	done

	# Kill raw podman container if still running
	if command -v podman &>/dev/null; then
		podman_as_deploy_user rm -f "$LOAD_RAW_CONTAINER" &>/dev/null || true
		podman_as_deploy_user rmi -f bench-load-basic-http &>/dev/null || true
	fi

	# Kill ctrl
	if [ -n "$CTRL_PID" ] && kill -0 "$CTRL_PID" 2>/dev/null; then
		info "cleanup: shutting down russel-ctrl (pid $CTRL_PID)"
		kill "$CTRL_PID" 2>/dev/null || true
		wait "$CTRL_PID" 2>/dev/null || true
	fi

	# Restore /var/lib
	bench_restore_var_lib

	bench_cleanup_tmp_paths
}
trap cleanup EXIT

# (podman_as_deploy_user lives in bench-common.sh)

# ── Result accumulators ──────────────────────────────────────────────────────
declare -A PATH_RPS PATH_P50 PATH_P95 PATH_P99 PATH_MEAN PATH_MAX_LAT PATH_ERR_PCT
declare -A PATH_CPU_AVG PATH_CPU_PEAK PATH_MEM_AVG PATH_MEM_PEAK PATH_DISK
BENCH_VMS_CREATED=()
RUN_ORDER=()

# ── 0. Dependency checks ─────────────────────────────────────────────────────
header "Dependency Check"

# hey
HAS_HEY=0
HEY_ARGS=()
if command -v hey &>/dev/null; then
	HAS_HEY=1
	HEY_ARGS=(hey)
	pass "hey: $(command -v hey)"
elif command -v nix &>/dev/null; then
	# nix run --impure test: does it resolve?
	if nix run --impure nixpkgs#hey -- -h &>/dev/null 2>&1; then
		HAS_HEY=1
		HEY_ARGS=(nix run --impure nixpkgs#hey --)
		pass "hey: via nix run --impure nixpkgs#hey"
	else
		warn "hey not found; install: nix profile install nixpkgs#hey or go install github.com/rakyll/hey@latest"
	fi
else
	warn "hey not found and nix unavailable; install hey to continue"
fi

# podman
HAS_PODMAN=0
if command -v podman &>/dev/null; then
	HAS_PODMAN=1
	pass "podman: $(podman --version | head -1)"
else
	warn "podman missing — raw podman path will be skipped"
fi

# rootless podman (for Russel container)
HAS_ROOTLESS_PODMAN=0
RUSSEL_PODMAN_USER=""
if [ "$HAS_PODMAN" -eq 1 ]; then
	rootless_val=$(podman_as_deploy_user info --format '{{.Host.Security.Rootless}}' 2>/dev/null | tr -d '[:space:]' | tr '[:upper:]' '[:lower:]')
	if [ "$rootless_val" = "true" ]; then
		HAS_ROOTLESS_PODMAN=1
		if [ "${EUID:-0}" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != "root" ]; then
			RUSSEL_PODMAN_USER="$SUDO_USER"
		fi
		pass "rootless podman: yes"
	else
		warn "podman present but not rootless — Russel container path will be skipped"
	fi
fi

# Probe cpuset availability for rootless podman --cpuset-cpus
HAS_CPUSET=0
uid="${SUDO_UID:-$(id -u)}"
[ -n "${SUDO_USER:-}" ] && uid=$(id -u "$SUDO_USER")
ctrl_file="/sys/fs/cgroup/user.slice/user-${uid}.slice/user@${uid}.service/cgroup.controllers"
# Also check parent user.slice cgroup.controllers
if [ -r "$ctrl_file" ] && grep -qw cpuset "$ctrl_file"; then
	HAS_CPUSET=1
fi
if [ "$HAS_CPUSET" -eq 1 ]; then
	pass "cpuset controller: yes"
else
	if [ "$USE_CPU_CAP" -eq 1 ]; then
		warn "cpuset controller: no (will use --cpus=${CPU_LIMIT} only)"
	else
		warn "cpuset controller: CPU uncapped (CPU_LIMIT=0)"
	fi
fi

# MicroVM prereqs
HAS_MICROVM=0
has_kvm=0 has_ch=0 has_socat=0 has_ip=0 has_nix=0
[ -c /dev/kvm ] 2>/dev/null && has_kvm=1
command -v nix &>/dev/null && has_nix=1

# Inject flake devShell PATH for cloud-hypervisor/virtiofsd/socat
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

# Root check for Russel paths (var/lib redirect)
HAS_ROOT=0
[ "$EUID" -eq 0 ] && HAS_ROOT=1

if [ "$has_kvm" -eq 1 ] && [ "$HAS_ROOT" -eq 1 ] && [ "$has_nix" -eq 1 ] &&
	[ "$has_ch" -eq 1 ] && [ "$has_socat" -eq 1 ] && [ "$has_ip" -eq 1 ]; then
	HAS_MICROVM=1
fi

if [ "$HAS_MICROVM" -eq 1 ]; then
	pass "microVM prereqs: ok"
else
	warn "microVM prereqs missing (need KVM+root+nix+cloud-hypervisor+socat+ip)"
fi

# ── 1. Hey available? ────────────────────────────────────────────────────────
if [ "$HAS_HEY" -eq 0 ]; then
	fail "hey is required for load generation. Install: nix profile install nixpkgs#hey or go install github.com/rakyll/hey@latest"
	exit 1
fi

# ── 2. Build release binaries if missing ─────────────────────────────────────
header "Build Check"
NEED_CTRL=0
[ "$HAS_MICROVM" -eq 1 ] && NEED_CTRL=1
[ "$HAS_ROOTLESS_PODMAN" -eq 1 ] && [ "$HAS_ROOT" -eq 1 ] && NEED_CTRL=1

if [ "$NEED_CTRL" -eq 1 ]; then
	if [ ! -f "$RELEASE_DIR/russel-ctrl" ] || [ ! -f "$RELEASE_DIR/russel" ]; then
		info "building russel (release)..."
		cargo build --release -q 2>&1
	fi
	export PATH="$RELEASE_DIR:$PATH"
	pass "russel binaries: ready"
else
	info "no Russel paths available — skipping control plane build"
fi

# ── 3. Russel setup (kernel + ctrl + var/lib redirect) ───────────────────────
RUSSEL_CTRL_READY=0
RUSSEL_CTRL_ADDR="127.0.0.1:7878"

if [ "$NEED_CTRL" -eq 1 ]; then
	# Kernel for microVM
	if [ "$HAS_MICROVM" -eq 1 ]; then
		if [ -n "${RUSSEL_KERNEL_PATH:-}" ] && [ -f "$RUSSEL_KERNEL_PATH" ]; then
			pass "RUSSEL_KERNEL_PATH=$RUSSEL_KERNEL_PATH"
		elif [ "$has_nix" -eq 1 ] && [ -f "flake.nix" ]; then
			info "building microvm kernel (nix build .#microvm-kernel)..."
			kernel_out=$(nix build .#microvm-kernel --print-out-paths --no-link 2>/dev/null || true)
			if [ -n "$kernel_out" ]; then
				if [ -f "$kernel_out/bzImage" ]; then
					RUSSEL_KERNEL_PATH="$kernel_out/bzImage"
				elif [ -f "$kernel_out" ]; then
					RUSSEL_KERNEL_PATH="$kernel_out"
				fi
			fi
			if [ -n "${RUSSEL_KERNEL_PATH:-}" ] && [ -f "$RUSSEL_KERNEL_PATH" ]; then
				export RUSSEL_KERNEL_PATH
				pass "RUSSEL_KERNEL_PATH=$RUSSEL_KERNEL_PATH"
			else
				warn "kernel build failed; microVM path will be skipped"
				HAS_MICROVM=0
			fi
		else
			warn "no kernel and no nix/flake; microVM path will be skipped"
			HAS_MICROVM=0
		fi
	fi

	# var/lib redirect
	if [ "$HAS_ROOT" -eq 1 ]; then
		RUSSEL_STATE_DIR=$(mktemp -d /tmp/russel-benchload-XXXXXX)
		mkdir -p "$RUSSEL_STATE_DIR/lib/russel" "$RUSSEL_STATE_DIR/lib/microvms"
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
		pass "state dirs redirected to $RUSSEL_STATE_DIR"
	fi

	# Export podman user for ctrl
	export RUSSEL_PODMAN_USER
	export RUSSEL_KERNEL_PATH="${RUSSEL_KERNEL_PATH:-}"
	# Deploys examples/basic-http by absolute path. That is gated off by
	# default (#196); the bench ctrl is single-tenant and short-lived.
	export RUSSEL_ALLOW_LOCAL_PATH_DEPLOY=1

	# Start russel-ctrl
	info "starting russel-ctrl on $RUSSEL_CTRL_ADDR..."
	RUSSEL_LOG=$(mktemp /tmp/russel-ctrl-log-XXXXXX)
	russel-ctrl &>"$RUSSEL_LOG" &
	CTRL_PID=$!

	ctrl_ready=0
	for i in $(seq 1 30); do
		if curl -sf "http://${RUSSEL_CTRL_ADDR}/vms" >/dev/null 2>&1; then
			ctrl_ready=1
			break
		fi
		sleep 1
	done

	if [ "$ctrl_ready" -eq 0 ]; then
		warn "russel-ctrl did not start within 30s; Russel paths disabled"
		HAS_MICROVM=0
		HAS_ROOTLESS_PODMAN=0
	else
		RUSSEL_CTRL_READY=1
		pass "russel-ctrl ready after ${i}s"
	fi
fi

# ── 4. Helper: parse hey output ──────────────────────────────────────────────
# Returns: RPS, p50, p95, p99, mean, max_lat, err_pct (via global vars)
parse_hey() {
	local output="$1"
	HEY_RPS=$(echo "$output" | grep -oP 'Requests/sec:\s*\K[\d.]+' || echo "0")

	# hey reports latencies in seconds -- convert to ms
	local _raw
	_raw=$(echo "$output" | grep -oP '50% in\s+\K[\d.]+' || echo "0")
	HEY_P50=$(awk "BEGIN{printf \"%.2f\", $_raw*1000}")
	_raw=$(echo "$output" | grep -oP '95% in\s+\K[\d.]+' || echo "0")
	HEY_P95=$(awk "BEGIN{printf \"%.2f\", $_raw*1000}")
	_raw=$(echo "$output" | grep -oP '99% in\s+\K[\d.]+' || echo "0")
	HEY_P99=$(awk "BEGIN{printf \"%.2f\", $_raw*1000}")
	_raw=$(echo "$output" | grep -oP 'Average:\s+\K[\d.]+' || echo "0")
	HEY_MEAN=$(awk "BEGIN{printf \"%.2f\", $_raw*1000}")
	_raw=$(echo "$output" | grep -oP '(Longest|100% in)\s+\K[\d.]+' | head -1 || echo "0")
	HEY_MAX=$(awk "BEGIN{printf \"%.2f\", $_raw*1000}")

	# Sum all status-code response counts; err% = non-2xx / total * 100
	local total=0 non2xx=0 code count
	while IFS= read -r line; do
		code=$(echo "$line" | grep -oP '\[\K\d+(?=\])' || echo "")
		count=$(echo "$line" | grep -oP '\]\s+\K\d+' || echo "0")
		[ -z "$code" ] && continue
		total=$((total + count))
		case "$code" in
		2??) ;;
		*) non2xx=$((non2xx + count)) ;;
		esac
	done < <(echo "$output" | grep -oP '\[\d+\]\s+\d+')

	if [ "$total" -gt 0 ] 2>/dev/null; then
		HEY_ERR_PCT=$(awk "BEGIN { printf \"%.1f\", $non2xx/$total * 100 }")
	else
		HEY_ERR_PCT="0"
	fi
}

# ── 5. Helper: run load+sample for a path ────────────────────────────────────
# Arguments: runtime_label runtime_kind container_identifier
# runtime_kind: microvm|container|raw
# container_identifier: vm_id for Russel paths, container name for raw podman
run_load_cycle() {
	local label="$1" kind="$2" cid="$3"
	local csv cpu mem stats cpu_avg cpu_peak mem_avg mem_peak

	# ── Warm-up ──
	info "$label: warm-up (3s @ c10)..."
	"${HEY_ARGS[@]}" -z 3s -c 10 "http://127.0.0.1:${HOST_PORT}/health" >/dev/null 2>&1 || true

	# ── Start sampler ──
	csv=$(mktemp /tmp/benchload-res-XXXXXX.csv)
	echo "ts,cpu_pct,mem_kb" >"$csv"

	local sampler_pid=""
	local ch_pid=""

	if [ "$kind" = "microvm" ]; then
		# Find cloud-hypervisor PID
		ch_pid=$(pgrep -f "cloud-hypervisor.*$cid" 2>/dev/null | head -1 || echo "")
		if [ -z "$ch_pid" ]; then
			ch_pid=$(pgrep cloud-hypervisor 2>/dev/null | head -1 || echo "")
		fi
	fi

	local clk_tck
	clk_tck=$(getconf CLK_TCK 2>/dev/null || echo 100)

	(
		end_ts=$(($(date +%s) + LOAD_DURATION_SECS + 2))
		prev_ticks=0 prev_wall=0
		while [ "$(date +%s)" -lt "$end_ts" ]; do
			cpu="" mem=""
			if [ "$kind" = "microvm" ] && [ -n "$ch_pid" ]; then
				mem=$(ps -p "$ch_pid" -o rss= 2>/dev/null || echo "0")
				mem="${mem:-0}"
				cpu=0
				if [ -r "/proc/$ch_pid/stat" ]; then
					stat_line=$(cat "/proc/$ch_pid/stat" 2>/dev/null || echo "")
					if [ -n "$stat_line" ]; then
						after_comm="${stat_line##*)}"
						# Intentional word-split of /proc/<pid>/stat fields after comm.
						# shellcheck disable=SC2086
						set -- $after_comm
						utime=${12:-0}
						stime=${13:-0}
						now=$(date +%s%N)
						ticks=$((utime + stime))
						if [ -n "${prev_ticks:-}" ] && [ "${prev_wall:-0}" -ne 0 ] 2>/dev/null; then
							delta_ticks=$((ticks - prev_ticks))
							delta_wall_ns=$((now - prev_wall))
							if [ "$delta_wall_ns" -gt 0 ] 2>/dev/null; then
								cpu=$(awk "BEGIN { printf \"%.1f\", 100.0 * $delta_ticks / ($clk_tck * $delta_wall_ns / 1000000000) }")
							fi
						fi
						prev_ticks=$ticks
						prev_wall=$now
					fi
				fi
			elif [ "$kind" = "container" ] || [ "$kind" = "raw" ]; then
				stats=$(podman_as_deploy_user stats --no-stream \
					--format '{{.CPUPerc}},{{.MemUsage}}' "$cid" 2>/dev/null || echo "0,0")
				cpu=$(echo "$stats" | cut -d, -f1 | tr -d '%' | tr -d ' ')
				mem=$(echo "$stats" | cut -d, -f2 | grep -oP '^[\d.]+' || echo "0")
				cpu="${cpu:-0}"
				mem="${mem:-0}"
			fi
			echo "$(date +%s.%N),${cpu},${mem}" >>"$csv"
			sleep 0.5
		done
	) &
	sampler_pid=$!

	# ── Main load ──
	info "$label: main load (${LOAD_DURATION} @ c${LOAD_CONCURRENCY})..."
	local hey_out
	hey_out=$("${HEY_ARGS[@]}" -z "$LOAD_DURATION" -c "$LOAD_CONCURRENCY" "http://127.0.0.1:${HOST_PORT}/health" 2>&1) || true

	# ── Stop sampler ──
	if [ -n "$sampler_pid" ]; then
		kill "$sampler_pid" 2>/dev/null || true
		wait "$sampler_pid" 2>/dev/null || true
	fi

	# ── Parse hey ──
	parse_hey "$hey_out"

	# ── Aggregate resource samples ──
	if [ -s "$csv" ] && [ "$(wc -l <"$csv")" -gt 1 ]; then
		cpu_avg=$(awk -F, 'NR>1 {sum+=$2; n++} END {if(n) printf "%.1f", sum/n; else print "0"}' "$csv")
		cpu_peak=$(awk -F, 'NR>1 {if($2+0>max) max=$2+0} END {printf "%.1f", max+0}' "$csv")
		mem_avg=$(awk -F, 'NR>1 {sum+=$3; n++} END {if(n) printf "%.1f", sum/n; else print "0"}' "$csv")
		mem_peak=$(awk -F, 'NR>1 {if($3+0>max) max=$3+0} END {printf "%.1f", max+0}' "$csv")
	else
		cpu_avg="0" cpu_peak="0" mem_avg="0" mem_peak="0"
	fi

	# mem_avg/mem_peak are in KB from sampler; podman stats gives MiB decimal
	# Normalize: if from podman (kind=container|raw), values are in MiB (decimal).
	# Convert podman MB→KB: multiply by 1024.
	# Actually podman MemUsage may include "MiB" suffix but our awk extracted just the number.
	# The number from podman stats is in whatever unit it reports — typically MiB with decimal.
	# For microVM, ps rss is in KB.
	# We'll store raw and convert at display time. For now keep as-is.

	# Store results
	PATH_RPS["$label"]="$HEY_RPS"
	PATH_P50["$label"]="$HEY_P50"
	PATH_P95["$label"]="$HEY_P95"
	PATH_P99["$label"]="$HEY_P99"
	PATH_MEAN["$label"]="$HEY_MEAN"
	PATH_MAX_LAT["$label"]="$HEY_MAX"
	PATH_ERR_PCT["$label"]="$HEY_ERR_PCT"
	PATH_CPU_AVG["$label"]="$cpu_avg"
	PATH_CPU_PEAK["$label"]="$cpu_peak"
	# Store raw + unit tag for display logic
	if [ "$kind" = "microvm" ]; then
		PATH_MEM_AVG["$label"]="$mem_avg" # KB from ps rss
		PATH_MEM_PEAK["$label"]="$mem_peak"
	else
		# podman stats returns MiB decimal — convert to KB for consistency
		PATH_MEM_AVG["$label"]=$(awk "BEGIN { printf \"%.0f\", $mem_avg * 1024 }")
		PATH_MEM_PEAK["$label"]=$(awk "BEGIN { printf \"%.0f\", $mem_peak * 1024 }")
	fi

	RUN_ORDER+=("$label")
	rm -f "$csv"
}

# ── 6. Helper: deploy Russel path (microvm or container) ─────────────────────
russel_deploy_and_wait() {
	local runtime="$1" vm_id="$2"
	local repo_path cfg_name cfg_path

	repo_path="$(realpath examples/basic-http)"
	cfg_name="Russelfile.benchload-${runtime}.toml"
	cfg_path="$repo_path/$cfg_name"

	# CPU cap for the container path, as a TOML array for service.podman_args.
	local podman_args=""
	if [ "$runtime" = "container" ] && [ "$USE_CPU_CAP" -eq 1 ]; then
		if [ "$HAS_CPUSET" -eq 1 ]; then
			podman_args='["--cpus", "'"$CPU_LIMIT"'", "--cpuset-cpus", "'"$CPUSET"'"]'
		else
			podman_args='["--cpus", "'"$CPU_LIMIT"'"]'
		fi
	fi

	# The Russelfile is the whole desired state (#454): service.name (the
	# service id), type, memory, cpus, podman_args and [ingress].port (the
	# host port hey hits) replace the committed values.
	awk -v rt="$runtime" -v id="$vm_id" -v mem="${MEM_MB}mb" -v cpus="${VCPUS}" \
		-v pargs="$podman_args" -v hp="$HOST_PORT" '
		BEGIN { section=""; injected=0; ingress=0 }
		/^[[:space:]]*\[/ {
			section = ""
			if ($0 ~ /^[[:space:]]*\[service\][[:space:]]*(#.*)?$/) section = "service"
			else if ($0 ~ /^[[:space:]]*\[ingress\][[:space:]]*(#.*)?$/) section = "ingress"
		}
		section == "service" && /^[[:space:]]*(name|type|memory|cpus|podman_args)[[:space:]]*=/ { next }
		section == "ingress" && /^[[:space:]]*port[[:space:]]*=/ { next }
		section == "service" && /^[[:space:]]*\[/ {
			print
			print "name = \"" id "\""
			print "type = \"" rt "\""
			print "memory = \"" mem "\""
			print "cpus = " cpus
			if (pargs != "") print "podman_args = " pargs
			injected=1
			next
		}
		section == "ingress" && /^[[:space:]]*\[/ {
			print
			print "port = " hp
			ingress=1
			next
		}
		{ print }
		END {
			if (!injected) {
				print "[service]"
				print "name = \"" id "\""
				print "type = \"" rt "\""
				print "memory = \"" mem "\""
				print "cpus = " cpus
				if (pargs != "") print "podman_args = " pargs
			}
			if (!ingress) {
				print ""
				print "[ingress]"
				print "port = " hp
			}
		}
	' "$repo_path/Russelfile.toml" >"$cfg_path"

	info "deploy $vm_id (runtime=$runtime)..."
	set +e
	response=$(curl -s -X POST "http://${RUSSEL_CTRL_ADDR}/deploy" \
		-H "Content-Type: application/json" \
		-d "{
			\"repo_url\": \"$repo_path\",
			\"config_path\": \"$cfg_name\",
			\"vm_id\": \"$vm_id\"
		}" 2>&1)
	set -euo pipefail

	rm -f "$cfg_path"

	complete_line=$(echo "$response" | grep '"type":"Complete"' | tail -1 || echo "")
	deploy_status=$(echo "$complete_line" | grep -oP '"status":"([^"]*)"' | cut -d'"' -f4 || echo "failed")

	if [ "$deploy_status" != "deployed" ]; then
		err_msg=$(echo "$complete_line" | grep -oP '"message":"([^"]*)"' | cut -d'"' -f4 || echo "unknown")
		fail "$vm_id: deploy failed: $err_msg"
		return 1
	fi

	BENCH_VMS_CREATED+=("$vm_id")

	# Wait for /health
	local ok=0
	for _ in $(seq 1 120); do
		if curl -sf "http://127.0.0.1:${HOST_PORT}/health" >/dev/null 2>&1; then
			ok=1
			break
		fi
		sleep 0.25
	done

	if [ "$ok" -eq 0 ]; then
		fail "$vm_id: /health never ready"
		return 1
	fi

	pass "$vm_id: /health ready"
	return 0
}

# ── 7. Helper: destroy Russel path ───────────────────────────────────────────
russel_destroy() {
	local vm_id="$1"
	info "destroying $vm_id..."
	russel destroy "$vm_id" &>/dev/null || true
	# Remove from cleanup list
	local _kept=()
	for _v in "${BENCH_VMS_CREATED[@]:-}"; do
		[ -n "$_v" ] && [ "$_v" != "$vm_id" ] && _kept+=("$_v")
	done
	BENCH_VMS_CREATED=("${_kept[@]}")
	info "path complete; next path..."
}

# ── 8. Helper: measure disk ─────────────────────────────────────────────────
measure_disk() {
	local kind="$1" vm_id="$2"
	if [ "$kind" = "raw" ]; then
		podman_as_deploy_user image inspect bench-load-basic-http --format '{{.Size}}' 2>/dev/null | awk '{printf "%.1f", $1/1024/1024}' || echo "0.0"
	else
		local d="0"
		for dir in "/var/lib/russel/$vm_id" "/var/lib/microvms/$vm_id"; do
			if [ -d "$dir" ]; then
				d=$(du -sk "$dir" 2>/dev/null | cut -f1 || echo "0")
				break
			fi
		done
		if [ "$d" = "0" ] 2>/dev/null; then
			# du may report 0 for tiny state dirs; check if files exist
			for dir in "/var/lib/russel/$vm_id" "/var/lib/microvms/$vm_id"; do
				if [ -d "$dir" ] && [ -n "$(find "$dir" -type f 2>/dev/null | head -1)" ]; then
					echo "<0.1"
					return
				fi
			done
			echo "0.0"
		else
			awk "BEGIN { printf \"%.1f\", $d/1024 }"
		fi
	fi
}

# ══════════════════════════════════════════════════════════════════════════════
# PATH 1: Russel microVM
# ══════════════════════════════════════════════════════════════════════════════
header "Path 1/3: Russel microVM"

if [ "$HAS_MICROVM" -eq 1 ] && [ "$RUSSEL_CTRL_READY" -eq 1 ]; then
	VM_ID="benchload-mvm"
	if russel_deploy_and_wait "microvm" "$VM_ID"; then
		# CPU pin (best-effort)
		if [ "$USE_CPU_CAP" -eq 1 ]; then
			CH_PID=$(pgrep -f "cloud-hypervisor.*$VM_ID" 2>/dev/null | head -1 || echo "")
			if [ -z "$CH_PID" ]; then
				CH_PID=$(pgrep cloud-hypervisor 2>/dev/null | head -1 || echo "")
			fi
			if [ -n "$CH_PID" ] && command -v taskset &>/dev/null; then
				taskset -cp "$CPUSET" "$CH_PID" >/dev/null 2>&1 && info "CPU pinned: taskset -cp $CPUSET $CH_PID" || true
			else
				info "CPU pin skipped (taskset or PID not available)"
			fi
		fi

		# Disk
		DISK_MB=$(measure_disk "microvm" "$VM_ID")
		info "disk footprint: ${DISK_MB} MB"

		# Load cycle
		run_load_cycle "microvm" "microvm" "$VM_ID"

		PATH_DISK["microvm"]="$DISK_MB"
		pass "microVM: RPS=${PATH_RPS["microvm"]} p50=${PATH_P50["microvm"]}ms p99=${PATH_P99["microvm"]}ms cpu=${PATH_CPU_AVG["microvm"]}% mem=${PATH_MEM_AVG["microvm"]}KB"
	else
		warn "microVM deploy failed — skipping path"
		PATH_DISK["microvm"]="—"
	fi

	russel_destroy "$VM_ID" || true
else
	warn "microVM path skipped (prereqs missing)"
	PATH_DISK["microvm"]="—"
fi

# ══════════════════════════════════════════════════════════════════════════════
# PATH 2: Russel container (rootless Podman via control plane)
# ══════════════════════════════════════════════════════════════════════════════
header "Path 2/3: Russel container"

if [ "$HAS_ROOTLESS_PODMAN" -eq 1 ] && [ "$RUSSEL_CTRL_READY" -eq 1 ] && [ "$HAS_ROOT" -eq 1 ]; then
	VM_ID="benchload-ctr"
	if russel_deploy_and_wait "container" "$VM_ID"; then
		# Find podman container name for sampling
		CTR_NAME=$(podman_as_deploy_user ps --format '{{.Names}}' --filter "name=$VM_ID" 2>/dev/null | head -1 || echo "")
		if [ -z "$CTR_NAME" ]; then
			CTR_NAME=$(podman_as_deploy_user ps --format '{{.Names}}' 2>/dev/null | head -1 || echo "")
		fi
		[ -z "$CTR_NAME" ] && CTR_NAME="$VM_ID"

		DISK_MB=$(measure_disk "container" "$VM_ID")
		info "disk footprint: ${DISK_MB} MB"

		run_load_cycle "container" "container" "$CTR_NAME"

		PATH_DISK["container"]="$DISK_MB"
		pass "container: RPS=${PATH_RPS["container"]} p50=${PATH_P50["container"]}ms p99=${PATH_P99["container"]}ms cpu=${PATH_CPU_AVG["container"]}% mem=${PATH_MEM_AVG["container"]}KB"
	else
		warn "container deploy failed — skipping path"
		PATH_DISK["container"]="—"
	fi

	russel_destroy "$VM_ID" || true
else
	warn "Russel container path skipped (need rootless podman + root)"
	PATH_DISK["container"]="—"
fi

# ══════════════════════════════════════════════════════════════════════════════
# PATH 3: Raw Podman (Dockerfile baseline)
# ══════════════════════════════════════════════════════════════════════════════
header "Path 3/3: Raw Podman"

if [ "$HAS_PODMAN" -eq 1 ]; then
	REPO_PATH="$(realpath examples/basic-http)"
	IMAGE_TAG="bench-load-basic-http"

	# Build
	info "building Dockerfile image..."
	set +e
	build_out=$(podman_as_deploy_user build -t "$IMAGE_TAG" "$REPO_PATH" 2>&1)
	build_ok=$?
	set -euo pipefail

	if [ "$build_ok" -ne 0 ] || ! podman_as_deploy_user image inspect "$IMAGE_TAG" &>/dev/null; then
		echo "$build_out" >&2
		fail "raw podman: build failed"
		PATH_DISK["podman"]="—"
	else
		pass "raw podman: image built"

		# Remove lingering container
		podman_as_deploy_user rm -f "$LOAD_RAW_CONTAINER" &>/dev/null || true

		# Run
		if [ "$USE_CPU_CAP" -eq 1 ]; then
			if [ "$HAS_CPUSET" -eq 1 ]; then
				info "starting container (mem=${MEM_MB}m, cpus=${CPU_LIMIT}, cpuset-cpus=$CPUSET)..."
			else
				info "starting container (mem=${MEM_MB}m, cpus=${CPU_LIMIT})..."
			fi
		else
			info "starting container (mem=${MEM_MB}m, no CPU cap)..."
		fi
		set +e
		if [ "$USE_CPU_CAP" -eq 1 ]; then
			if [ "$HAS_CPUSET" -eq 1 ]; then
				cid=$(podman_as_deploy_user run -d \
					--name "$LOAD_RAW_CONTAINER" \
					-p "${HOST_PORT}:3000" \
					--memory="${MEM_MB}m" \
					--cpus="$CPU_LIMIT" \
					--cpuset-cpus="$CPUSET" \
					"$IMAGE_TAG" 2>&1)
			else
				cid=$(podman_as_deploy_user run -d \
					--name "$LOAD_RAW_CONTAINER" \
					-p "${HOST_PORT}:3000" \
					--memory="${MEM_MB}m" \
					--cpus="$CPU_LIMIT" \
					"$IMAGE_TAG" 2>&1)
			fi
		else
			cid=$(podman_as_deploy_user run -d \
				--name "$LOAD_RAW_CONTAINER" \
				-p "${HOST_PORT}:3000" \
				--memory="${MEM_MB}m" \
				"$IMAGE_TAG" 2>&1)
		fi
		run_ok=$?
		set -euo pipefail

		if [ "$run_ok" -ne 0 ]; then
			fail "raw podman: run failed: $cid"
			PATH_DISK["podman"]="—"
		else
			# Wait /health
			ok=0
			for _ in $(seq 1 60); do
				if curl -sf "http://127.0.0.1:${HOST_PORT}/health" >/dev/null 2>&1; then
					ok=1
					break
				fi
				sleep 0.25
			done

			if [ "$ok" -eq 0 ]; then
				fail "raw podman: /health never ready"
				PATH_DISK["podman"]="—"
			else
				pass "raw podman: /health ready"

				DISK_MB=$(measure_disk "raw" "")
				info "disk footprint: ${DISK_MB} MB"

				run_load_cycle "podman" "raw" "$LOAD_RAW_CONTAINER"

				PATH_DISK["podman"]="$DISK_MB"
				pass "podman: RPS=${PATH_RPS["podman"]} p50=${PATH_P50["podman"]}ms p99=${PATH_P99["podman"]}ms cpu=${PATH_CPU_AVG["podman"]}% mem=${PATH_MEM_AVG["podman"]}KB"
			fi

			# Cleanup
			podman_as_deploy_user rm -f "$LOAD_RAW_CONTAINER" &>/dev/null || true
		fi

		# Remove image
		podman_as_deploy_user rmi -f "$IMAGE_TAG" &>/dev/null || true
	fi
else
	warn "raw podman path skipped (podman missing)"
	PATH_DISK["podman"]="—"
fi

# ══════════════════════════════════════════════════════════════════════════════
# COMPARISON TABLE
# ══════════════════════════════════════════════════════════════════════════════
header "Load Benchmark Results"

# Convert KB to MiB for display
fmt_mem() {
	local kb="$1"
	if [ "$kb" = "—" ] || [ -z "$kb" ]; then
		echo "—"
		return
	fi
	awk "BEGIN { printf \"%.1f\", $kb/1024 }"
}

echo ""
echo -e "  ${BOLD}Go basic-http · sequential · ${CPU_LABEL} · ${MEM_MB} MiB · ${LOAD_DURATION} @ c${LOAD_CONCURRENCY}${NC}"
echo -e "  ${DIM}$(date)${NC}"
echo ""
echo -e "  ${DIM}┌──────────┬──────────┬───────┬───────┬───────┬───────┬──────┬───────┬──────────┬──────────┬──────────┬──────────┬────────┐${NC}"
echo -e "  ${DIM}│ path     │   RPS    │  p50  │  p95  │  p99  │  mean │  max │ err%  │ cpu_avg  │ cpu_peak │ mem_avg  │ mem_peak │ diskMB │${NC}"
echo -e "  ${DIM}├──────────┼──────────┼───────┼───────┼───────┼───────┼──────┼───────┼──────────┼──────────┼──────────┼──────────┼────────┤${NC}"

for path in microvm container podman; do
	rps="${PATH_RPS[$path]:-—}"
	p50="${PATH_P50[$path]:-—}"
	p95="${PATH_P95[$path]:-—}"
	p99="${PATH_P99[$path]:-—}"
	mean="${PATH_MEAN[$path]:-—}"
	maxl="${PATH_MAX_LAT[$path]:-—}"
	errp="${PATH_ERR_PCT[$path]:-—}"
	cpu_avg="${PATH_CPU_AVG[$path]:-—}"
	cpu_peak="${PATH_CPU_PEAK[$path]:-—}"
	mem_avg=$(fmt_mem "${PATH_MEM_AVG[$path]:-—}")
	mem_peak=$(fmt_mem "${PATH_MEM_PEAK[$path]:-—}")
	disk="${PATH_DISK[$path]:-—}"

	printf "  ${DIM}│${NC} %-8s ${DIM}│${NC} %8s ${DIM}│${NC} %5s ${DIM}│${NC} %5s ${DIM}│${NC} %5s ${DIM}│${NC} %5s ${DIM}│${NC} %4s ${DIM}│${NC} %5s ${DIM}│${NC} %8s ${DIM}│${NC} %8s ${DIM}│${NC} %8s ${DIM}│${NC} %8s ${DIM}│${NC} %5s ${DIM}│${NC}\n" \
		"$path" "$rps" "$p50" "$p95" "$p99" "$mean" "$maxl" "$errp" \
		"$cpu_avg" "$cpu_peak" "$mem_avg" "$mem_peak" "$disk"
done

echo -e "  ${DIM}└──────────┴──────────┴───────┴───────┴───────┴───────┴──────┴───────┴──────────┴──────────┴──────────┴──────────┴────────┘${NC}"

echo ""
echo -e "  ${DIM}Units: RPS=req/s, latencies=ms, cpu%=% of 1 core, mem=MiB, disk=MB${NC}"
if [ "$USE_CPU_CAP" -eq 1 ]; then
	echo -e "  ${DIM}Declared limits: --cpus=${CPU_LIMIT} mem=${MEM_MB}m · VCPUs=${VCPUS} (microVM)${NC}"
	if [ "$HAS_CPUSET" -eq 0 ]; then
		echo -e "  ${DIM}CPU pin: --cpus=${CPU_LIMIT} (cpuset unavailable under rootless user slice)${NC}"
	fi
else
	echo -e "  ${DIM}Declared limits: mem=${MEM_MB}m · CPU=uncapped · VCPUs=${VCPUS} (microVM)${NC}"
fi
echo -e "  ${DIM}Sampler: every 0.5s during load window; avg/peak computed from samples${NC}"

echo ""
echo -e "${BOLD}  bench-load complete.${NC}"
