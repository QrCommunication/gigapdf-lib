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
# `cargo wasm` is a repo alias (.cargo/config.toml) for the full target build.
( cd "$ENGINE_DIR" && cargo wasm )

SRC="$ENGINE_DIR/target/wasm32-unknown-unknown/release/gigapdf_wasm.wasm"
test -f "$SRC" || { echo "❌ wasm not produced: $SRC" >&2; exit 1; }

cp "$SRC" "$PKG_DIR/gigapdf.wasm"
cp "$ENGINE_DIR/LICENSE" "$PKG_DIR/LICENSE" 2>/dev/null || true
echo "✓ copied $(du -h "$PKG_DIR/gigapdf.wasm" | cut -f1) → $PKG_DIR/gigapdf.wasm"

# Host-loaded OCR model blobs (.gpocr): the wasm embeds NO weights, so consumers
# load a per-script line-OCR model at runtime via `gp_ocr_load_model`. Ship them
# next to the SDK so `@qrcommunication/gigapdf-lib/models/ocr_<script>.gpocr`
# resolves for Node hosts (the per-script CRNN handles non-Latin scripts —
# Cyrillic, Greek, Arabic, Devanagari, Bengali, Tamil — that the built-in
# mono-glyph classifier cannot).
MODELS_SRC="$ENGINE_DIR/models"
if compgen -G "$MODELS_SRC/*.gpocr" > /dev/null; then
  mkdir -p "$PKG_DIR/models"
  cp "$MODELS_SRC"/*.gpocr "$PKG_DIR/models/"
  echo "✓ copied $(ls "$PKG_DIR/models"/*.gpocr | wc -l) OCR model(s) → $PKG_DIR/models/"
else
  echo "ℹ no .gpocr models in $MODELS_SRC — OCR limited to the mono-glyph classifier"
fi
