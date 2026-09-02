#!/usr/bin/env bash
# Install Russel binaries from a release build in this repo.
#
#   ./contrib/install.sh cli     # ~/.local/bin/russel
#   ./contrib/install.sh ctrl    # /usr/local/bin/russel-ctrl (sudo if needed)
#   ./contrib/install.sh all
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RELEASE="${ROOT}/target/release"
WHAT="${1:-}"

usage() {
  echo "usage: $0 cli|ctrl|all" >&2
  exit 2
}

install_cli() {
  local src="${RELEASE}/russel"
  local dest="${HOME}/.local/bin/russel"
  if [[ ! -x "$src" ]]; then
    echo "missing ${src}; run: cargo build --release -p russel-cli" >&2
    exit 1
  fi
  install -Dm755 "$src" "$dest"
  echo "installed ${dest}"
}

install_ctrl() {
  local src="${RELEASE}/russel-ctrl"
  local dest="${RUSSEL_CTRL_DEST:-/usr/local/bin/russel-ctrl}"
  if [[ ! -x "$src" ]]; then
    echo "missing ${src}; run: cargo build --release -p russel-ctrl" >&2
    exit 1
  fi
  if [[ -w "$(dirname "$dest")" ]]; then
    install -Dm755 "$src" "$dest"
  else
    sudo install -Dm755 "$src" "$dest"
  fi
  echo "installed ${dest}"
}

case "$WHAT" in
  cli) install_cli ;;
  ctrl) install_ctrl ;;
  all)
    install_cli
    install_ctrl
    ;;
  *) usage ;;
esac
