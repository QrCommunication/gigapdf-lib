#!/usr/bin/env bash
# Parallel training orchestrator — runs ALL remaining OCR trainings concurrently. The box is
# 48 vCPU / 184 GB; chaining them sequentially wasted ~30 idle cores. Each job is independent
# (a distinct `ocr_<...>.gpocr`), so we run a concurrency-capped pool: MAXJOBS jobs ×
# THREADS_PER_JOB threads. Real datasets are pre-downloaded once (cached) so concurrent jobs on
# the same script don't double-fetch. Survives disconnect (run via nohup). Idempotent-ish: skips
# a job whose output already exists (resume after interruption).
#
# Usage (VPS):  MAXJOBS=3 nohup bash deploy/train_all_parallel.sh > ~/parallel_provision.log 2>&1 &
set -uo pipefail
cd "$HOME/gigapdf-lib"
VENV="${VENV:-$HOME/ocrvenv}"
NPROC="$(nproc)"
MAXJOBS="${MAXJOBS:-3}"
TPJ="${TPJ:-12}"                  # threads per job (MAXJOBS×TPJ ≤ NPROC, leave headroom for any other run)
export GIGA_OCR_DL_WORKERS="${GIGA_OCR_DL_WORKERS:-8}"

# label | group | variant | degrade | langs | real(csv or -) | charset(path or -) | epochs
JOBS=(
  # ── Handwriting variants for non-Latin (mixed) — VARIANT=hw → ocr_<g>_hw.gpocr
  "deva_hw|deva|hw|0|hin,eng|iiit_hindi|-|50"
  "taml_hw|taml|hw|0|tam,eng|iiit_tamil|-|50"
  "arabic_hw|arabic|hw|0|ara|-|-|50"
  "beng_hw|beng|hw|0|ben,eng|-|-|50"
  # ── Breadth: 14 new script primaries (mixed) — VARIANT="" → ocr_<g>.gpocr
  "thai|thai||0|tha,eng|-|-|50"
  "telu|telu||0|tel,eng|-|-|50"
  "kann|kann||0|kan,eng|-|-|50"
  "mlym|mlym||0|mal,eng|-|-|50"
  "gujr|gujr||0|guj,eng|-|-|50"
  "guru|guru||0|pan,eng|-|-|50"
  "orya|orya||0|ori,eng|-|-|50"
  "sinh|sinh||0|sin,eng|-|-|50"
  "geor|geor||0|kat,eng|-|-|50"
  "armn|armn||0|hye,eng|-|-|50"
  "khmr|khmr||0|khm,eng|-|-|50"
  "laoo|laoo||0|lao,eng|-|-|50"
  "mymr|mymr||0|mya,eng|myanmar|-|50"
  "ethi|ethi||0|amh,eng|-|-|50"
  # ── Photo / degraded variants (mixed) — VARIANT=photo + DEGRADE=1 → ocr_<g>_photo.gpocr
  "arabic_photo|arabic|photo|1|ara|-|-|45"
  "deva_photo|deva|photo|1|hin,eng|iiit_hindi|-|45"
  "beng_photo|beng|photo|1|ben,eng|-|-|45"
  "taml_photo|taml|photo|1|tam,eng|iiit_tamil|-|45"
  "cjk_photo|cjk|photo|1|eng|chinese,casia|tools/ocr/cjk_charset.txt|45"
  "jpn_photo|jpn|photo|1|jpn,eng|japanese|tools/ocr/jpn_charset.txt|45"
  "kor_photo|kor|photo|1|kor,eng|korean|tools/ocr/kor_charset.txt|45"
)

# ── Pre-download every unique real dataset once (cached) so concurrent same-script jobs reuse it.
log() { echo "[$(date -u +%H:%M:%S)] $*" >> ~/parallel_queue.log; }
log "pre-downloading real datasets (cached)…"
for d in iiit_hindi iiit_tamil myanmar chinese casia japanese korean; do
  ls /tmp/ocr_hw/${d}_train_*.npz >/dev/null 2>&1 && { log "  $d cached"; continue; }
  GIGA_OCR_DL_WORKERS=16 "$VENV/bin/python" tools/ocr/hw_datasets.py "$d" 60000 > ~/dl_${d}.log 2>&1 || log "  $d dl failed"
done

run_job() {
  local spec="$1"
  IFS='|' read -r label group variant degrade langs real charset epochs <<<"$spec"
  local outname="ocr_${group}${variant:+_$variant}.gpocr"
  if [ -s "$HOME/gigapdf-lib/models/$outname" ] && grep -q "=== $label done" ~/parallel_queue.log 2>/dev/null; then
    echo "=== $label skip (exists) ===" >> ~/parallel_queue.log; return 0
  fi
  echo "=== $label start $(date -u) ===" >> ~/parallel_queue.log
  ( cd tools/ocr && "$VENV/bin/python" fonts.py "$group" >/dev/null 2>&1 ) || true
  ( cd tools/ocr && "$VENV/bin/python" fonts.py "$group" --handwriting >/dev/null 2>&1 ) || true
  local cs_env="" real_env="" realn=0
  [ "$charset" != "-" ] && cs_env="GIGA_OCR_CHARSET_$(echo "$group" | tr '[:lower:]' '[:upper:]')=$HOME/gigapdf-lib/$charset"
  [ "$real" != "-" ] && { real_env="$real"; realn=40000; }
  env GIGA_OCR_VARIANT="$variant" GIGA_OCR_DEGRADE="$degrade" \
      GIGA_OCR_HW_FRAC=0.4 GIGA_OCR_HW_REAL="$real_env" GIGA_OCR_HW_REAL_N="$realn" \
      GIGA_OCR_NLINES=22000 GIGA_OCR_LANGS="$langs" $cs_env \
      GIGA_OCR_C1=32 GIGA_OCR_C2=64 GIGA_OCR_HID=128 GIGA_OCR_BATCH=256 \
      OMP_NUM_THREADS="$TPJ" MKL_NUM_THREADS="$TPJ" \
      "$VENV/bin/python" tools/train_ocr_crnn.py "$group" "$epochs" > ~/train_${label}.log 2>&1 \
      && echo "=== $label done $(date -u) ===" >> ~/parallel_queue.log \
      || echo "=== $label FAILED $(date -u) ===" >> ~/parallel_queue.log
}

log "launching ${#JOBS[@]} jobs, $MAXJOBS concurrent × $TPJ threads"
rm -f ~/parallel_queue.done
for spec in "${JOBS[@]}"; do
  while [ "$(jobs -rp | wc -l)" -ge "$MAXJOBS" ]; do wait -n 2>/dev/null || break; done
  run_job "$spec" &
done
wait
echo ALLDONE > ~/parallel_queue.done
log "ALL DONE"
