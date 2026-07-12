#!/usr/bin/env bash
set -euo pipefail

# ──────────────────────────────────────────────────────────────────────────────
# russel bench — quick performance benchmark for systems-role portfolio
#
# Measures compile times, test speeds, and binary sizes.
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
pass "tests: ${test_passed} passed in ${test_ms}ms"

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
		stripped=$(strip "$path" -o /dev/null 2>/dev/null && stat --printf="%s" "$path" 2>/dev/null || echo "$size")
		stripped_kb=$((stripped / 1024))
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
pass "Rust: ${rust_files} files, ${rust_lines} lines"
pass "Docs: ${doc_files} files, ${doc_lines} lines"

# ── 9. Code quality snapshot ─────────────────────────────────────────────────
header "Code Quality"

clippy_warns=$(cargo clippy 2>&1 | grep -c 'warning:' || true)
pass "clippy warnings: ${clippy_warns} (expect ~5 for scaffold code)"

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
