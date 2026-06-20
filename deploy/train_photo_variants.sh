#!/usr/bin/env bash
# Photo / degraded variants per script — the model-side half of beating Tesseract on heavily
# degraded captures (the front-end is the input-side half). Each variant re-trains a script with
# the heavy in-the-wild degradation augmentation (`GIGA_OCR_DEGRADE=1`: curl/shear/illumination/
# blur/low-res/JPEG/noise — render_lines._degrade) and writes a NO-CLOBBER `ocr_<group>_photo.gpocr`
# (`GIGA_OCR_VARIANT=photo`); the host loads it for noisy/photographed input while the clean
# primary stays default. Reuses each script's real corpus (cached from the primary run) so the
# degraded model still sees real glyph shapes, and stays MIXED (Latin+digits) for dates/numbers.
# Runs after the breadth queue. `alpha` already has its photo variant. Detached.
#
# Usage (VPS):  nohup bash deploy/train_photo_variants.sh > ~/photo_provision.log 2>&1 &
set -uo pipefail
cd "$HOME/gigapdf-lib"
VENV="${VENV:-$HOME/ocrvenv}"
export GIGA_OCR_DL_WORKERS="${GIGA_OCR_DL_WORKERS:-16}"
THREADS="${GIGA_OCR_THREADS:-$(nproc)}"

# group | corpus langs | real dataset(s) ("-" none) | charset file ("-" = built-in group charset)
SPECS=(
  "arabic ara             -              -"
  "deva   hin,eng         iiit_hindi     -"
  "beng   ben,eng         -              -"
  "taml   tam,eng         iiit_tamil     -"
  "cjk    eng             chinese,casia  tools/ocr/cjk_charset.txt"
  "jpn    jpn,eng         japanese       tools/ocr/jpn_charset.txt"
  "kor    kor,eng         korean         tools/ocr/kor_charset.txt"
)

rm -f ~/photo_queue.done
for spec in "${SPECS[@]}"; do
  # shellcheck disable=SC2086
  set -- $spec; G=$1; LANGS=$2; DS=$3; CS=$4
  echo "=== ${G}_photo start $(date -u) (real=$DS) ===" >> ~/photo_queue.log
  ( cd tools/ocr && "$VENV/bin/python" fonts.py "$G" --handwriting >/dev/null 2>&1 ) || true
  REAL=""; REALN=0
  if [ "$DS" != "-" ]; then
    REAL="$DS"; REALN=60000
    for d in ${DS//,/ }; do "$VENV/bin/python" tools/ocr/hw_datasets.py "$d" "$REALN" > ~/dl_${d}.log 2>&1; done
  fi
  CHARSET_ENV=""
  [ "$CS" != "-" ] && CHARSET_ENV="GIGA_OCR_CHARSET_$(echo "$G" | tr '[:lower:]' '[:upper:]')=$HOME/gigapdf-lib/$CS"
  echo "=== ${G}_photo train $(date -u) ===" >> ~/photo_queue.log
  env GIGA_OCR_VARIANT=photo GIGA_OCR_DEGRADE=1 \
      GIGA_OCR_HW_FRAC=0.4 \
      GIGA_OCR_HW_REAL="$REAL" GIGA_OCR_HW_REAL_N="$REALN" \
      GIGA_OCR_NLINES=25000 \
      GIGA_OCR_LANGS="$LANGS" \
      $CHARSET_ENV \
      GIGA_OCR_C1=32 GIGA_OCR_C2=64 GIGA_OCR_HID=128 GIGA_OCR_BATCH=256 \
      OMP_NUM_THREADS="$THREADS" MKL_NUM_THREADS="$THREADS" \
      "$VENV/bin/python" tools/train_ocr_crnn.py "$G" 45 > ~/train_${G}_photo.log 2>&1
  echo "=== ${G}_photo done $(date -u) ===" >> ~/photo_queue.log
done
echo ALLDONE > ~/photo_queue.done
