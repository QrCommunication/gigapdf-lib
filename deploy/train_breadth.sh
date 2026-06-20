#!/usr/bin/env bash
# Breadth campaign — the writing systems Tesseract covers but we didn't, to close the coverage
# gap (Thai, Telugu, Kannada, Malayalam, Gujarati, Gurmukhi, Oriya, Sinhala, Georgian, Armenian,
# Khmer, Lao, Myanmar, Ethiopic). Each is a dedicated model:
#   • synthetic = Noto fonts + the Tesseract langdata_lstm corpus (all confirmed available),
#   • MIXED = the script charset already carries Latin + digits (scripts.py) and we render Latin
#     synthetic lines (GIGA_OCR_LANGS=<lang>,eng) → dates / numbers / codes are read,
#   • handwriting partly covered via Handwriting-category fonts (GIGA_OCR_HW_FRAC),
#   • real data where an ungated mirror exists (Myanmar = 7.5M lines, capped) — most new scripts
#     have no ungated real OCR corpus, so they're synthetic-only (honest gap).
# Runs after the HW non-Latin queue. Detached.
#
# Usage (VPS):  nohup bash deploy/train_breadth.sh > ~/breadth_provision.log 2>&1 &
set -uo pipefail
cd "$HOME/gigapdf-lib"
VENV="${VENV:-$HOME/ocrvenv}"
export GIGA_OCR_DL_WORKERS="${GIGA_OCR_DL_WORKERS:-16}"
THREADS="${GIGA_OCR_THREADS:-$(nproc)}"

# group | corpus lang (langdata_lstm) | real dataset alias ("-" = synthetic only)
SPECS=(
  "thai tha -" "telu tel -" "kann kan -" "mlym mal -" "gujr guj -"
  "guru pan -" "orya ori -" "sinh sin -" "geor kat -" "armn hye -"
  "khmr khm -" "laoo lao -" "mymr mya myanmar" "ethi amh -"
)

rm -f ~/breadth_queue.done
for spec in "${SPECS[@]}"; do
  # shellcheck disable=SC2086
  set -- $spec; G=$1; LANG=$2; DS=$3
  echo "=== $G start $(date -u) (lang=$LANG real=$DS) ===" >> ~/breadth_queue.log
  # Fonts: Noto print family + Handwriting-category faces covering the script (cmap-guarded).
  ( cd tools/ocr && "$VENV/bin/python" fonts.py "$G" >/dev/null 2>&1 ) || true
  ( cd tools/ocr && "$VENV/bin/python" fonts.py "$G" --handwriting >/dev/null 2>&1 ) || true
  REAL=""; REALN=0
  if [ "$DS" != "-" ]; then
    REAL="$DS"; REALN=40000
    echo "=== $G download $DS (parallel) $(date -u) ===" >> ~/breadth_queue.log
    "$VENV/bin/python" tools/ocr/hw_datasets.py "$DS" "$REALN" > ~/dl_${DS}.log 2>&1
  fi
  echo "=== $G train $(date -u) ===" >> ~/breadth_queue.log
  env GIGA_OCR_HW_FRAC=0.35 \
      GIGA_OCR_HW_REAL="$REAL" GIGA_OCR_HW_REAL_N="$REALN" \
      GIGA_OCR_NLINES=25000 \
      GIGA_OCR_LANGS="$LANG,eng" \
      GIGA_OCR_C1=32 GIGA_OCR_C2=64 GIGA_OCR_HID=128 GIGA_OCR_BATCH=256 \
      OMP_NUM_THREADS="$THREADS" MKL_NUM_THREADS="$THREADS" \
      "$VENV/bin/python" tools/train_ocr_crnn.py "$G" 50 > ~/train_$G.log 2>&1
  echo "=== $G done $(date -u) ===" >> ~/breadth_queue.log
done
echo ALLDONE > ~/breadth_queue.done
