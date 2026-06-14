#!/usr/bin/env bash
# Build the Rust engine (crates/wasm) to wasm32 and copy the artifact next to the
# SDK's dist as `gigapdf.wasm`. The engine crates live at the repo root (this SDK
# is `<repo>/sdk`); override the location with ENGINE_DIR if needed.
set -euo pipefail

PKG_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENGINE_DIR="${ENGINE_DIR:-$(cd "$PKG_DIR/.." && pwd)}"

if [ ! -d "$ENGINE_DIR/crates/wasm" ]; then
  echo "❌ engine crates not found at: $ENGINE_DIR/crates/wasm (set ENGINE_DIR=...)" >&2
  exit 1
fi

echo "→ building gigapdf-wasm (release) in $ENGINE_DIR"
( cd "$ENGINE_DIR" && cargo build -p gigapdf-wasm --target wasm32-unknown-unknown --release )

SRC="$ENGINE_DIR/target/wasm32-unknown-unknown/release/gigapdf_wasm.wasm"
test -f "$SRC" || { echo "❌ wasm not produced: $SRC" >&2; exit 1; }

cp "$SRC" "$PKG_DIR/gigapdf.wasm"
cp "$ENGINE_DIR/LICENSE" "$PKG_DIR/LICENSE" 2>/dev/null || true
echo "✓ copied $(du -h "$PKG_DIR/gigapdf.wasm" | cut -f1) → $PKG_DIR/gigapdf.wasm"
