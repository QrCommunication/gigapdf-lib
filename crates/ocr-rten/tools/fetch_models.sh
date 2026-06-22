#!/bin/bash
# Fetch + convert all PaddleOCR models for the RTen engine into the layout OcrEngine::load_models_dir
# expects: <out>/det.rten  +  <out>/<subdir>/{model.rten,dict.txt}. Run at deploy time (models are
# not committed). Requires: wget, rten-convert (`pip install rten-convert`).
#
# Usage: fetch_models.sh [out_dir]   (default: crates/ocr-rten/assets/models)
set -e
OUT="${1:-crates/ocr-rten/assets/models}"
B="https://huggingface.co/deepghs/paddleocr/resolve/main"
RTEN_CONVERT="${RTEN_CONVERT:-rten-convert}"
mkdir -p "$OUT"

# Shared DBNet detector (language-agnostic) -> det.rten
wget -q "$B/det/ch_PP-OCRv4_det/model.onnx" -O "$OUT/det_model.onnx"
"$RTEN_CONVERT" "$OUT/det_model.onnx" >/dev/null && mv "$OUT/det_model.rten" "$OUT/det.rten" && rm -f "$OUT/det_model.onnx"
echo "ok det.rten"

# Per-language recognizers (subdir names must match REC_MODELS in src/lib.rs).
LANGS="arabic_PP-OCRv3_rec ch_PP-OCRv4_rec chinese_cht_PP-OCRv3_rec cyrillic_PP-OCRv3_rec \
devanagari_PP-OCRv3_rec en_PP-OCRv4_rec japan_PP-OCRv3_rec ka_PP-OCRv3_rec korean_PP-OCRv3_rec \
latin_PP-OCRv3_rec ta_PP-OCRv3_rec te_PP-OCRv3_rec"
for L in $LANGS; do
  d="$OUT/$L"; mkdir -p "$d"
  wget -q "$B/rec/$L/model.onnx" -O "$d/model.onnx"
  wget -q "$B/rec/$L/dict.txt" -O "$d/dict.txt"
  "$RTEN_CONVERT" "$d/model.onnx" >/dev/null && rm -f "$d/model.onnx"
  echo "ok $L"
done
# Hebrew (our own model — PaddleOCR ships none): produced by tools/train_hebrew.py, dropped into
# $OUT/hebrew/{model.rten,dict.txt}. Skipped here if absent.
echo "DONE -> $OUT"
