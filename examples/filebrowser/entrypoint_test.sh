#!/bin/sh
# Stub filebrowser and check demo warning plus hash/exec args.
set -eu

root="$(CDPATH= cd -- "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/bin"
log="$work/log"

cat > "$work/bin/filebrowser" << 'EOF'
#!/bin/sh
log="${FILEBROWSER_STUB_LOG:?}"
if [ "${1:-}" = "hash" ]; then
  printf 'hashed:%s\n' "${2:-}"
  printf 'hash %s\n' "${2:-}" >> "$log"
  exit 0
fi
{
  printf 'exec'
  for a in "$@"; do
    printf ' %s' "$a"
  done
  printf '\n'
} >> "$log"
exit 0
EOF
chmod +x "$work/bin/filebrowser"

fail() {
  echo "FAIL: $*" >&2
  echo "--- stderr ---" >&2
  cat "$work/stderr" >&2 || true
  echo "--- log ---" >&2
  cat "$log" >&2 || true
  exit 1
}

run() {
  : > "$log"
  : > "$work/stderr"
  env "$@" FILEBROWSER_STUB_LOG="$log" PATH="$work/bin:$PATH" \
    sh "$root/entrypoint.sh" > "$work/stdout" 2> "$work/stderr" \
    || fail "entrypoint exited $?"
}

# Unset FILEBROWSER_PASSWORD: demo fallback, warn, hash that password.
run -u FILEBROWSER_PASSWORD
grep -F -q "WARNING: demo login admin / demo-only-not-for-production" "$work/stderr" \
  || fail "expected demo warning when password is unset"
grep -F -q "hash demo-only-not-for-production" "$log" \
  || fail "expected hash of demo password when unset"
grep -F -q "exec --address 0.0.0.0 --port 8080 --database /tmp/filebrowser.db --root /tmp/files --username admin --password hashed:demo-only-not-for-production" "$log" \
  || fail "unexpected exec args when unset"
grep -F -q -- "--noauth" "$log" && fail "must not pass --noauth" || true

# Explicit demo string (shipped Russelfile): same warning.
run FILEBROWSER_PASSWORD=demo-only-not-for-production
grep -F -q "WARNING: demo login admin / demo-only-not-for-production" "$work/stderr" \
  || fail "expected demo warning when FILEBROWSER_PASSWORD is the public demo string"
grep -F -q "hash demo-only-not-for-production" "$log" \
  || fail "expected hash of demo password when set explicitly"
grep -F -q -- "--password hashed:demo-only-not-for-production" "$log" \
  || fail "expected hashed demo password on exec"

# Real password: no warning, hash the given value.
run FILEBROWSER_PASSWORD=s3cret
grep -F -q "WARNING:" "$work/stderr" && fail "did not expect warning for a non-demo password" || true
grep -F -q "hash s3cret" "$log" || fail "expected hash of configured password"
grep -F -q -- "--password hashed:s3cret" "$log" || fail "expected hashed configured password on exec"
grep -F -q -- "--address 0.0.0.0" "$log" || fail "expected guest bind 0.0.0.0"

echo "ok"
