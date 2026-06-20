#!/usr/bin/env bash
# Handwriting variants for the non-Latin scripts that don't have one yet — to run on the VPS
# AFTER the JP/KR queue (chain it: `while [ ! -f ~/jpkr_queue.done ]; do sleep 300; done`).
#
# Strategy per script (honest about data availability):
#   • deva / taml — REAL handwriting words exist ungated (IIIT-INDIC-HW-WORDS, ~70k/76k) → strong.
#   • arabic / beng — no ungated real HW line corpus found → synthetic *Handwriting*-font lines
#     only (fewer Indic/Arabic handwriting faces exist, so these stay weaker; documented).
# All are **mixed**: the script charset now carries Latin + digits (scripts.py), and we render
# Latin synthetic lines (`GIGA_OCR_LANGS=<lang>,eng`) so dates / numbers / codes are recognized —
# the same lesson as the CJK ASCII fix. Output is a no-clobber `ocr_<group>_hw.gpocr` (VARIANT=hw).
#
# Usage (on the VPS):  nohup bash deploy/train_hw_nonlatin.sh > ~/hw_nonlatin_provision.log 2>&1 &
set -uo pipefail
cd "$HOME/gigapdf-lib"
VENV="${VENV:-$HOME/ocrvenv}"
export GIGA_OCR_DL_WORKERS="${GIGA_OCR_DL_WORKERS:-16}"
THREADS="${GIGA_OCR_THREADS:-$(nproc)}"

# group | corpus lang | real-HW dataset alias ("-" = synthetic Handwriting fonts only)
SPECS=(
  "deva   hin iiit_hindi"
  "taml   tam iiit_tamil"
  "arabic ara -"
  "beng   ben -"
)

rm -f ~/hw_nonlatin_queue.done
for spec in "${SPECS[@]}"; do
  # shellcheck disable=SC2086
  set -- $spec; G=$1; LANG=$2; DS=$3
  echo "=== ${G}_hw start $(date -u) (lang=$LANG real=$DS) ===" >> ~/hw_nonlatin_queue.log
  # Handwriting-category fonts covering this script (cmap-guarded; may be few for non-Latin).
  ( cd tools/ocr && "$VENV/bin/python" fonts.py "$G" --handwriting >/dev/null 2>&1 ) || true
  REAL=""; REALN=0
  if [ "$DS" != "-" ]; then
    REAL="$DS"; REALN=60000
    echo "=== ${G}_hw download $DS (parallel) $(date -u) ===" >> ~/hw_nonlatin_queue.log
    "$VENV/bin/python" tools/ocr/hw_datasets.py "$DS" "$REALN" > ~/dl_${DS}.log 2>&1
  fi
  echo "=== ${G}_hw train $(date -u) ===" >> ~/hw_nonlatin_queue.log
  env GIGA_OCR_VARIANT=hw \
      GIGA_OCR_HW_FRAC=0.5 \
      GIGA_OCR_HW_REAL="$REAL" GIGA_OCR_HW_REAL_N="$REALN" \
      GIGA_OCR_NLINES=25000 \
      GIGA_OCR_LANGS="$LANG,eng" \
      GIGA_OCR_C1=32 GIGA_OCR_C2=64 GIGA_OCR_HID=128 GIGA_OCR_BATCH=256 \
      OMP_NUM_THREADS="$THREADS" MKL_NUM_THREADS="$THREADS" \
      "$VENV/bin/python" tools/train_ocr_crnn.py "$G" 50 > ~/train_${G}_hw.log 2>&1
  echo "=== ${G}_hw done $(date -u) ===" >> ~/hw_nonlatin_queue.log
done
echo ALLDONE > ~/hw_nonlatin_queue.done
