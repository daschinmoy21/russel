#!/usr/bin/env bash
set -euo pipefail

# ──────────────────────────────────────────────────────────────────────────────
# russel bench — systems benchmark + Russel vs Docker application boot race
#
# Measures compile times, test speeds, binary sizes, and then races Russel
# (microVM deployment via cloud-hypervisor) against Docker/podman for each
# example application — end to end: build → spawn → first HTTP response.
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

# ponytail: results arrays — indexed by example order
declare -a EXAMPLES=()
declare -a RUSSEL_DEPLOY_MS=()
declare -a RUSSEL_CURL_MS=()
declare -a DOCKER_BUILD_MS=()
declare -a DOCKER_BOOT_MS=()
declare -a WINNER=()
declare -a RUSSEL_SPAWN_MS=()
RUSSEL_VMS_CREATED=()
LAST_RUSSEL_COMPLETE=""

# ── 0. Dependency check ─────────────────────────────────────────────────────
header "Dependency Check"
RUNTIME=""
if command -v podman &>/dev/null; then
	RUNTIME="podman"
	pass "container runtime: podman ($(podman --version 2>/dev/null | head -1))"
elif command -v docker &>/dev/null; then
	RUNTIME="docker"
	pass "container runtime: docker ($(docker --version 2>/dev/null | head -1))"
else
	warn "no container runtime found — skipping Docker comparison"
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
# RUSSEL vs DOCKER — Application Boot Race
# ══════════════════════════════════════════════════════════════════════════════
#
# For each example application we measure two end-to-end paths:
#
#   Russel:  russel-ctrl deploys (nix build → initramfs → TAP → socat →
#            cloud-hypervisor boot → TCP ready) → curl app endpoint
#
#   Docker:  $RUNTIME build → $RUNTIME run → resolve host port → curl app endpoint
#
# The "race" shows both times side-by-side so you can see which platform
# serves the first HTTP response faster.
#
# NOTE: Russel requires KVM + root + nix + cloud-hypervisor.
#       If any prerequisite is missing this section is skipped with a note.
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

RUSSEL_CAPABLE=0
if [ "$has_kvm" -eq 1 ] && [ "$has_root" -eq 1 ] && [ "$has_nix" -eq 1 ] &&
	[ "$has_ch" -eq 1 ] && [ "$has_socat" -eq 1 ] && [ "$has_ip" -eq 1 ]; then
	RUSSEL_CAPABLE=1
fi

if [ "$RUSSEL_CAPABLE" -eq 0 ] && [ -z "$RUNTIME" ]; then
	header "Russel vs Docker: Application Boot Race"
	warn "both Russel (needs KVM+root+nix+cloud-hypervisor) and Docker (needs podman/docker)"
	warn "prerequisites are missing — skipping comparison section entirely."
else

	# ── Start benchmarking ──────────────────────────────────────────────────────
	header "Russel vs Docker: Application Boot Race"

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

	# ── 10a. Russel setup ──────────────────────────────────────────────────────
	RUSSEL_SKIPPED=0
	if [ "$RUSSEL_CAPABLE" -eq 1 ]; then
		info "Russel prerequisites met (KVM ✓ root ✓ nix ✓ cloud-hypervisor ✓)"

		# Build release binaries if missing
		if [ ! -f "$RELEASE_DIR/russel-ctrl" ] || [ ! -f "$RELEASE_DIR/russel-cli" ]; then
			info "building russel (release) for deploy comparison..."
			cargo build --release -q 2>&1
		fi

		# Copy binaries to PATH for easy access
		export PATH="$RELEASE_DIR:$PATH"

		# Create a temp state directory for this benchmark run
		RUSSEL_STATE_DIR=$(mktemp -d /tmp/russel-bench-XXXXXX)
		export RUSSEL_CTRL_ADDR="127.0.0.1:7878"

		# Create required subdirs for russel-ctrl
		mkdir -p "$RUSSEL_STATE_DIR/lib/russel" "$RUSSEL_STATE_DIR/lib/microvms"
		# ponytail: redirect /var/lib/{russel,microvms} to temp dir so we don't
		# touch the host's real state. Move the entire existing entry aside so
		# symlinks, hidden files, and empty directories are restored exactly.
		# Requires root — we already checked.
		VAR_LIB_REDIRECTED=1
		if [ -e /var/lib/russel ] || [ -L /var/lib/russel ]; then
			RUSSEL_STATE_BAK=$(mktemp /tmp/russel-var-lib-bak-XXXXXX)
			rm -f "$RUSSEL_STATE_BAK"
			mv /var/lib/russel "$RUSSEL_STATE_BAK"
		fi
		ln -s "$RUSSEL_STATE_DIR/lib/russel" /var/lib/russel
		if [ -e /var/lib/microvms ] || [ -L /var/lib/microvms ]; then
			MICROVMS_STATE_BAK=$(mktemp /tmp/russel-var-lib-bak-XXXXXX)
			rm -f "$MICROVMS_STATE_BAK"
			mv /var/lib/microvms "$MICROVMS_STATE_BAK"
		fi
		ln -s "$RUSSEL_STATE_DIR/lib/microvms" /var/lib/microvms

		# Start control plane in background
		info "starting russel-ctrl (pid in background)..."
		RUSSEL_LOG=$(mktemp /tmp/russel-ctrl-log-XXXXXX)
		russel-ctrl &>"$RUSSEL_LOG" &
		CTRL_PID=$!

		# Wait for control plane to be listening (max 30s)
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
		else
			pass "russel-ctrl ready after ${i}s"
		fi
	else
		info "Russel prerequisites not met:"
		[ "$has_kvm" -eq 0 ] && info "  ✗ KVM (/dev/kvm not found)"
		[ "$has_root" -eq 0 ] && info "  ✗ root (EUID != 0)"
		[ "$has_nix" -eq 0 ] && info "  ✗ nix (not on PATH)"
		[ "$has_ch" -eq 0 ] && info "  ✗ cloud-hypervisor (not on PATH)"
		[ "$has_socat" -eq 0 ] && info "  ✗ socat (not on PATH)"
		[ "$has_ip" -eq 0 ] && info "  ✗ ip (not on PATH)"
		warn "Russel microVM comparison skipped"
		RUSSEL_SKIPPED=1
	fi

	# ── 10b. Benchmark each example ─────────────────────────────────────────────
	# ponytail: examples that have both a Russelfile and a Dockerfile
	EXAMPLE_APPS=()
	for ex in basic-http static-test filebrowser; do
		if [ -f "examples/$ex/Russelfile.toml" ] && [ -f "examples/$ex/Dockerfile" ]; then
			EXAMPLE_APPS+=("$ex")
		fi
	done

	# Port base: Russel gets explicit ports starting here, Docker gets -P
	PORT_BASE=18080

	for example in "${EXAMPLE_APPS[@]}"; do
		header "  Race: ${example}"

		repo_path="$(realpath "examples/$example")"
		guest_port=$(grep -oP '(?<=^port = )\d+' "examples/$example/Russelfile.toml" | head -1)
		ready_path=$(get_ready_path "$example")
		host_port=$PORT_BASE
		PORT_BASE=$((PORT_BASE + 1))
		vm_id="bench-${example}"
		EXAMPLES+=("$example")

		# ── Russel deploy ───────────────────────────────────────────────────
		russel_deploy_ms=0
		russel_curl_ms=0

		if [ "$RUSSEL_SKIPPED" -eq 0 ]; then
			# Destroy any previous VM with this id
			russel-cli destroy "$vm_id" &>/dev/null || true

			# ponytail: cold mode — delete nix store path to force rebuild
			if [ "$COLD" -eq 1 ]; then
				local_result="examples/$example/result"
				if [ -L "$local_result" ]; then
					result_path=$(readlink -f "$local_result")
					rm -f "$local_result"
					nix-store --delete "$result_path" 2>/dev/null || true
				fi
			fi

			set +e
			# Call the deploy API directly (avoids parsing CLI output)
			# ponytail: single curl call, parse NDJSON for timing
			response=$(curl -s -X POST "http://${RUSSEL_CTRL_ADDR}/deploy" \
				-H "Content-Type: application/json" \
				-d "{
				\"repo_url\": \"$repo_path\",
				\"config_path\": \"Russelfile.toml\",
				\"vm_id\": \"$vm_id\",
				\"port\": {\"host\": $host_port, \"guest\": $guest_port}
			}" 2>&1)

			# Parse the Complete event
			complete_line=$(echo "$response" | grep '"type":"Complete"' | tail -1)
			deploy_status=$(echo "$complete_line" | grep -oP '"status":"([^"]*)"' | cut -d'"' -f4 || echo "failed")

			if [ "$deploy_status" = "deployed" ]; then
				russel_elapsed=$(json_int "elapsed_ms" "$complete_line")
				russel_deploy_ms=$((russel_elapsed))
				russel_build_phase=$(json_int "build_ms" "$complete_line")
				[ -z "$russel_build_phase" ] && russel_build_phase=0
				LAST_RUSSEL_COMPLETE="$complete_line"

				RUSSEL_VMS_CREATED+=("$vm_id")

				# Confirm with curl (HTTP readiness, not just TCP)
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
					russel_curl_ms=$(((curl_end - curl_start) / 1000000))
					RUSSEL_DEPLOY_MS+=("$russel_deploy_ms")
					RUSSEL_CURL_MS+=("$russel_curl_ms")
					# ponytail: spawn-to-ready = total - nix build + curl check
					russel_spawn_ms=$((russel_elapsed - russel_build_phase + russel_curl_ms))
					RUSSEL_SPAWN_MS+=("$russel_spawn_ms")
					pass "russel: deployed in ${russel_deploy_ms}ms, curl OK in +${russel_curl_ms}ms"
				else
					RUSSEL_DEPLOY_MS+=("$russel_deploy_ms")
					RUSSEL_CURL_MS+=("timeout")
					RUSSEL_SPAWN_MS+=("timeout")
					warn "russel: deployed in ${russel_deploy_ms}ms but curl timeout (HTTP never ready)"
				fi
			else
				err_msg=$(echo "$complete_line" | grep -oP '"message":"([^"]*)"' | cut -d'"' -f4 || echo "unknown")
				RUSSEL_DEPLOY_MS+=("failed")
				RUSSEL_CURL_MS+=("failed")
				RUSSEL_SPAWN_MS+=("failed")
				fail "russel: deploy failed (${err_msg})"
			fi
			set -euo pipefail
		else
			RUSSEL_DEPLOY_MS+=("skipped")
			RUSSEL_CURL_MS+=("skipped")
			RUSSEL_SPAWN_MS+=("skipped")
		fi

		# ── Docker deploy ───────────────────────────────────────────────────
		docker_boot_ms=0

		if [ -n "$RUNTIME" ]; then
			image_tag="russel-bench-$example"
			container_name="russel-bench-$example"

			set +e
			# Build
			info "docker: building image..."
			build_start=$(date +%s%N)
			build_out=$("$RUNTIME" build -t "$image_tag" "$repo_path" 2>&1) || true
			build_end=$(date +%s%N)
			build_ms=$(((build_end - build_start) / 1000000))

			if ! "$RUNTIME" image inspect "$image_tag" &>/dev/null; then
				build_err=$(echo "$build_out" | grep -i 'error:' | head -1 || echo "unknown error")
				DOCKER_BUILD_MS+=("failed")
				DOCKER_BOOT_MS+=("failed")
				fail "docker: build failed (${build_ms}ms): $build_err"
			else
				pass "docker: image built in ${build_ms}ms"
				DOCKER_BUILD_MS+=("$build_ms")

				# Run + readiness
				"$RUNTIME" rm -f "$container_name" &>/dev/null || true

				boot_start=$(date +%s%N)
				cid=$("$RUNTIME" run -d --name "$container_name" -P --memory=256m "$image_tag" 2>/dev/null || echo "")
				if [ -z "$cid" ]; then
					DOCKER_BOOT_MS+=("failed")
					fail "docker: failed to start container"
				else
					# Resolve host port
					dhp=""
					# Port publication is asynchronous; poll frequently instead of
					# adding up to 2.5s of fixed sleeps to every sample.
					for _ in $(seq 1 100); do
						dhp=$("$RUNTIME" port "$container_name" "$guest_port" 2>/dev/null | head -1 | grep -oP '\d+$' || echo "")
						if [ -n "$dhp" ]; then break; fi
						sleep 0.1
					done
					if [ -z "$dhp" ]; then
						DOCKER_BOOT_MS+=("failed")
						WINNER+=("—")
						warn "docker: could not determine host port"
					else
						# Wait for HTTP readiness
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
							pass "docker: container ready in ${docker_boot_ms}ms"

							# Determine winner for this example
							if [ "$RUSSEL_SKIPPED" -eq 0 ] && [ "${RUSSEL_DEPLOY_MS[-1]}" != "failed" ]; then
								r_total=$((russel_deploy_ms + russel_curl_ms))
								d_total=$((build_ms + docker_boot_ms))
								if [ "$d_total" -lt "$r_total" ]; then
									WINNER+=("${RUNTIME^}")
								elif [ "$d_total" -gt "$r_total" ]; then
									WINNER+=("Russel")
								else
									WINNER+=("Tie")
								fi
							else
								WINNER+=("—")
							fi
						else
							DOCKER_BOOT_MS+=("timeout")
							WINNER+=("—")
							warn "docker: container started but HTTP never ready"
						fi
					fi
				fi
			fi

			# Cleanup container (only remove image on cold runs)
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
	header "Russel vs Docker: Results"

	if [ "${#EXAMPLES[@]}" -gt 0 ]; then
		echo ""
		echo -e "  ${BOLD}App Boot Race — End-to-End (build + spawn + HTTP ready)${NC}"
		echo ""
		echo -e "  ${DIM}┌──────────────┬──────────────────────────┬──────────────────────────┬──────────┐${NC}"
		runtime_label="${RUNTIME^}"
		[ -z "$runtime_label" ] && runtime_label="Docker"
		echo -e "  ${DIM}│ App          │ Russel (deploy+curl)     │ ${runtime_label} (build+run+curl)  │ Winner   │${NC}"
		echo -e "  ${DIM}├──────────────┼──────────────────────────┼──────────────────────────┼──────────┤${NC}"
		for i in "${!EXAMPLES[@]}"; do
			name="${EXAMPLES[$i]}"

			rus_d="${RUSSEL_DEPLOY_MS[$i]}"
			rus_c="${RUSSEL_CURL_MS[$i]}"
			if [ "$rus_d" = "skipped" ]; then
				rus_col="     skipped      "
			elif [ "$rus_d" = "failed" ]; then
				rus_col="     failed       "
			elif [ "$rus_c" = "timeout" ]; then
				rus_col="timeout (${rus_d}+timeout)"
			else
				rus_total=$((rus_d + rus_c))
				rus_col="${rus_total}ms  (${rus_d}+${rus_c})"
			fi

			dc_b="${DOCKER_BUILD_MS[$i]}"
			dc_r="${DOCKER_BOOT_MS[$i]}"
			if [ "$dc_b" = "skipped" ]; then
				dc_col="     skipped      "
			elif [ "$dc_b" = "failed" ]; then
				dc_col="     failed       "
			elif [ "$dc_r" = "timeout" ]; then
				dc_col="timeout (${dc_b}+timeout)"
			elif [ "$dc_r" = "failed" ]; then
				dc_col="     failed       "
			else
				dc_total=$((dc_b + dc_r))
				dc_col="${dc_total}ms  (${dc_b}+${dc_r})"
			fi

			winner="${WINNER[$i]:-—}"
			if [ "$winner" = "Russel" ]; then
				w_col=" ${GREEN}Russel${NC} "
			elif [ "$winner" = "${runtime_label}" ]; then
				w_col=" ${YELLOW}${runtime_label}${NC} "
			elif [ "$winner" = "Tie" ]; then
				w_col="  Tie  "
			else
				w_col="  —   "
			fi

			printf "  ${DIM}│${NC} %-12s ${DIM}│${NC} %-24s ${DIM}│${NC} %-24s ${DIM}│${NC} %-8s ${DIM}│${NC}\n" \
				"$name" "$rus_col" "$dc_col" "$w_col"
		done
		echo -e "  ${DIM}└──────────────┴──────────────────────────┴──────────────────────────┴──────────┘${NC}"

		echo ""
		echo -e "  ${DIM}Note:${NC}"
		echo -e "  ${DIM}  Russel times: deploy (nix build → initramfs → TAP/socat → kernel boot → TCP ready)${NC}"
		echo -e "  ${DIM}                + curl (first HTTP 200 response)${NC}"
		echo -e "  ${DIM}  ${runtime_label} times: build + run (container start → first HTTP 200)${NC}"
		echo -e "  ${DIM}  Both use --memory=256m (or the Russelfile memory cap)${NC}"
		echo -e "  ${DIM}  ${runtime_label} run time includes readiness polling (100ms interval, bounded at 10s).${NC}"
		echo -e "  ${DIM}  Russel skips the nix build step on cache hits — real cold-build times may be higher.${NC}"
		echo ""
		if [ "$RUSSEL_SKIPPED" -eq 1 ]; then
			echo -e "  ${YELLOW}  ⚠ Russel microVM comparison was skipped — see prerequisite notes above.${NC}"
		fi

		# ── Spawn-to-Ready comparison (excluding build) ───────────────────
		if [ "${#EXAMPLES[@]}" -gt 0 ]; then
			echo ""
			echo -e "  ${BOLD}Spawn-to-Ready (excluding build time)${NC}"
			echo -e "  ${DIM}(Russel subtracts nix build phase; ${runtime_label} start→HTTP already excludes build)${NC}"
			echo ""
			echo -e "  ${DIM}┌──────────────┬──────────────────────────┬──────────────────────────┐${NC}"
			echo -e "  ${DIM}│ App          │ Russel (spawn→ready)     │ ${runtime_label} (spawn→ready)   │${NC}"
			echo -e "  ${DIM}├──────────────┼──────────────────────────┼──────────────────────────┤${NC}"
			for i in "${!EXAMPLES[@]}"; do
				name="${EXAMPLES[$i]}"
				rus_s="${RUSSEL_SPAWN_MS[$i]}"
				dc_s="${DOCKER_BOOT_MS[$i]}"

				# Format Russel column
				if [ "$rus_s" = "skipped" ]; then
					rus_col="        skipped         "
				elif [ "$rus_s" = "failed" ]; then
					rus_col="        failed          "
				elif [ "$rus_s" = "timeout" ]; then
					rus_col="        timeout         "
				else
					rus_col="${rus_s}ms"
				fi

				# Format container column
				if [ "$dc_s" = "skipped" ]; then
					dc_col="        skipped         "
				elif [ "$dc_s" = "failed" ]; then
					dc_col="        failed          "
				elif [ "$dc_s" = "timeout" ]; then
					dc_col="        timeout         "
				else
					dc_col="${dc_s}ms"
				fi

				printf "  ${DIM}│${NC} %-12s ${DIM}│${NC} %-24s ${DIM}│${NC} %-24s ${DIM}│${NC}\n" \
					"$name" "$rus_col" "$dc_col"
			done
			echo -e "  ${DIM}└──────────────┴──────────────────────────┴──────────────────────────┘${NC}"
		fi

		# ── Timing breakdown for Russel (last successful deploy) ──────────
		if [ "$RUSSEL_SKIPPED" -eq 0 ] && [ -n "$LAST_RUSSEL_COMPLETE" ]; then
			echo ""
			echo -e "  ${BOLD}Last Russel deploy — phase breakdown:${NC}"
			echo ""
			res_ms=$(json_int "resolve_ms" "$LAST_RUSSEL_COMPLETE")
			bld_ms=$(json_int "build_ms" "$LAST_RUSSEL_COMPLETE")
			crt_ms=$(json_int "create_ms" "$LAST_RUSSEL_COMPLETE")
			net_ms=$(json_int "network_ms" "$LAST_RUSSEL_COMPLETE")
			stt_ms=$(json_int "start_ms" "$LAST_RUSSEL_COMPLETE")
			rdy_ms=$(json_int "ready_ms" "$LAST_RUSSEL_COMPLETE")
			echo -e "  ${DIM}┌────────────┬────────┬──────────────────────────────────────────────┐${NC}"
			echo -e "  ${DIM}│ Phase      │ Time   │ Detail                                      │${NC}"
			echo -e "  ${DIM}├────────────┼────────┼──────────────────────────────────────────────┤${NC}"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "resolve" "${res_ms}ms" "repo + Russelfile"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "build" "${bld_ms}ms" "nix build (package)"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "create" "${crt_ms}ms" "build minimal initramfs"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "network" "${net_ms}ms" "TAP + socat port fwd"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "start" "${stt_ms}ms" "virtiofsd + cloud-hypervisor"
			printf "  ${DIM}│${NC} %-10s │ %5s │ %-44s ${DIM}│${NC}\n" "ready" "${rdy_ms}ms" "guest TCP socket live"
			r_total=$((res_ms + bld_ms + crt_ms + net_ms + stt_ms + rdy_ms))
			echo -e "  ${DIM}├────────────┼────────┼──────────────────────────────────────────────┤${NC}"
			printf "  ${DIM}│${NC} ${BOLD}%-10s${NC} ${DIM}│${NC} ${BOLD}%5s${NC} │ ${BOLD}%-44s${NC} ${DIM}│${NC}\n" "total" "${r_total}ms" "sum of phases"
			echo -e "  ${DIM}└────────────┴────────┴──────────────────────────────────────────────┘${NC}"
			# ponytail: gap between phase sum and deploy total = Traefik registration + metadata writes + cleanup
			r_gap=$((russel_deploy_ms - r_total))
			if [ "$r_gap" -gt 0 ]; then
				echo -e "  ${DIM}  (${r_gap}ms gap: Traefik registration, metadata writes, VM cleanup)${NC}"
			fi
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
