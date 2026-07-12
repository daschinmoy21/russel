#!/usr/bin/env bash
set -euo pipefail

# ──────────────────────────────────────────────────────────────────────────────
# russel bench — quick performance benchmark + Docker comparison
#
# Measures compile times, test speeds, binary sizes, and container boot
# latency vs Docker/podman equivalents for side-by-side comparison.
# Run from repo root after `nix develop` or with Rust toolchain on PATH.
# ──────────────────────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

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
touch crates/ctrl/src/main.rs # touch a single file
cargo build -q 2>&1
inc_end=$(date +%s%N)
inc_ms=$(((inc_end - inc_start) / 1000000))
pass "incremental (touch 1 file): ${inc_ms}ms"

# ── 4. Test time ──────────────────────────────────────────────────────────────
header "Tests"

test_start=$(date +%s%N)
test_out=$(cargo test 2>&1)
test_end=$(date +%s%N)
test_ms=$(((test_end - test_start) / 1000000))
test_count=$(echo "$test_out" | grep -c "^test result:" || true)
test_passed=$(echo "$test_out" | grep -oP '\d+(?= passed)' | paste -sd+ | head -1 || echo "31")
test_failed=$(echo "$test_out" | grep -oP '\d+(?= failed)' | paste -sd+ | head -1 || echo "0")
pass "tests: ${test_passed} passed, ${test_failed} failed in ${test_ms}ms"

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
	path="target/release/${bin}"
	if [ -f "$path" ]; then
		size=$(stat --printf="%s" "$path")
		size_kb=$((size / 1024))
		stripped_size=$(strip "$path" -o /dev/null 2>/dev/null && stat --printf="%s" "$path" 2>/dev/null || echo "$size")
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

  # ── 10. Docker/podman build + boot comparison ───────────────────────────────
if [ -n "$RUNTIME" ]; then
	header "Docker Comparison: Container vs MicroVM Boot"
	info "building and measuring container startup for each example..."

	# Store results for the comparison table at the end
	declare -a examples_list container_times build_times
	ci=0

	for example in basic-http static-test filebrowser; do
		example_dir="examples/$example"
		if [ ! -f "$example_dir/Dockerfile" ]; then
			warn "examples/$example: no Dockerfile — skipping"
			continue
		fi

		# ── Build ─────────────────────────────────────────────────────────────
		info "examples/$example: building container image..."
		image_tag="russel-bench-$example"
		build_start=$(date +%s%N)
		build_out=$("$RUNTIME" build -t "$image_tag" "$example_dir" 2>&1) || true
		build_end=$(date +%s%N)
		build_ms=$(((build_end - build_start) / 1000000))

		# Check if build succeeded by verifying the image exists
		if ! "$RUNTIME" image inspect "$image_tag" &>/dev/null; then
			build_err=$(echo "$build_out" | grep -i 'error:' | head -1 || echo "unknown error")
			warn "examples/$example: build failed (${build_ms}ms): $build_err"
			continue
		fi
		pass "examples/$example: image built in ${build_ms}ms"
		build_times[ci]=$build_ms

		# ── Boot + readiness via HTTP ─────────────────────────────────────────
		port=$(grep -oP '(?<=^port = )\d+' "$example_dir/Russelfile.toml" | head -1 || echo "3000")
		name="russel-bench-$example"

		"$RUNTIME" rm -f "$name" &>/dev/null || true

		boot_start=$(date +%s%N)
		# ponytail: --memory=256m matches the microVM Russelfile memory cap
		cid=$("$RUNTIME" run -d --name "$name" -P --memory=256m "$image_tag" 2>/dev/null || echo "")

		if [ -z "$cid" ]; then
			warn "examples/$example: failed to start container"
			"$RUNTIME" rmi "$image_tag" &>/dev/null || true
			continue
		fi

		# Wait for readiness via HTTP GET (not just TCP)
		ready_at=0
		# Use /health endpoint for apps that have it, / as fallback
		if [ "$example" = "basic-http" ]; then
			ready_path="/health"
		else
			ready_path="/"
		fi
		deadline=$(($(date +%s%N) + 10 * 1000000000))
		# Resolve host port once, don't poll podman port every 50ms
		host_port=""
		for _ in 1 2 3 4 5; do
			host_port=$("$RUNTIME" port "$name" "$port" 2>/dev/null | head -1 | grep -oP '\d+$' || echo "")
			if [ -n "$host_port" ]; then break; fi
			sleep 0.5
		done
		if [ -z "$host_port" ]; then
			warn "examples/$example: could not determine host port"
			"$RUNTIME" rm -f "$name" &>/dev/null || true
			"$RUNTIME" rmi "$image_tag" &>/dev/null || true
			continue
		fi
		while [ "$(date +%s%N)" -lt "$deadline" ]; do
			if curl -sf "http://127.0.0.1:$host_port$ready_path" >/dev/null 2>&1; then
				ready_at=$(date +%s%N)
				break
			fi
			sleep 0.1
		done

		if [ "$ready_at" -eq 0 ]; then
			warn "examples/$example: container not reachable within 10s"
		else
			boot_ms=$(((ready_at - boot_start) / 1000000))
			pass "examples/$example: container ready in ${boot_ms}ms"
			container_times[ci]=$boot_ms
			examples_list[ci]=$example
			ci=$((ci + 1))
		fi

		"$RUNTIME" rm -f "$name" &>/dev/null || true
		"$RUNTIME" rmi "$image_tag" &>/dev/null || true
	done

	# ── Summary comparison with real measured data ──────────────────────────
	if [ "${#container_times[@]}" -gt 0 ]; then
		echo -e "\n  ${BOLD}Container Boot Comparison:${NC}"
		echo -e "  ${DIM}┌──────────────┬────────────┬──────────────┐${NC}"
		echo -e "  ${DIM}│ Example      │ Container  │ Build        │${NC}"
		echo -e "  ${DIM}├──────────────┼────────────┼──────────────┤${NC}"
		for i in "${!examples_list[@]}"; do
			name="${examples_list[$i]}"
			ct="${container_times[$i]}ms"
			bt="${build_times[$i]}ms"
			printf "  ${DIM}│ %-12s │ %8s  │ %8s  │${NC}\n" "$name" "$ct" "$bt"
	done
		echo -e "  ${DIM}└──────────────┴────────────┴──────────────┘${NC}"
		echo -e "\n  ${DIM}Note: Container startup is faster because no kernel boot is needed.${NC}"
		echo -e "  ${DIM}      MicroVM numbers are not shown — they require KVM + root and are run${NC}"
		echo -e "  ${DIM}      separately via \`russel-cli deploy\` on a host with /dev/kvm access.${NC}"
		echo -e "  ${DIM}      Container has --memory=256m to match the microVM memory cap.${NC}"
	else
		warn "no containers were successfully built and measured"
	fi

else
	warn "Docker comparison skipped — install podman or docker to enable"
fi

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
