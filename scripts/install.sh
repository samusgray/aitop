#!/usr/bin/env bash
set -euo pipefail

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo is required to build aitop" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_DIR="${AITOP_INSTALL_DIR:-$HOME/.local/bin}"

echo "building aitop release binary..."
cargo build --release --manifest-path "$ROOT/Cargo.toml"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$ROOT/target/release/aitop" "$INSTALL_DIR/aitop"

echo "installed aitop to $INSTALL_DIR/aitop"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo "warning: $INSTALL_DIR is not on PATH" >&2
    echo "add this to your shell profile:" >&2
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\"" >&2
    ;;
esac
