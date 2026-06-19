#!/usr/bin/env bash
# Provision a fresh Ubuntu box (e.g. Hetzner CCX63 — 48 vCPU / 192 GB) and launch the
# gigapdf OCR "mega" handwriting training **detached** (tmux) so it survives SSH
# disconnect, the operator's local shutdown, and terminal close. Idempotent: safe to
# re-run (skips finished steps, reuses caches). Assumes the repo is already cloned at
# $REPO_DIR (clone happens in the deploy step before this script runs).
#
# Usage (on the VPS):
#   HF_TOKEN=hf_xxx bash deploy/train_vps.sh
#
# Monitor:   tmux attach -t megatrain          (Ctrl-b d to detach)
#            tail -f ~/megatrain.log
#            grep epoch ~/megatrain.log | tail
set -uo pipefail

REPO_DIR="${REPO_DIR:-$HOME/gigapdf-lib}"
VENV="${VENV:-$HOME/ocrvenv}"
GROUP="${GROUP:-alpha}"
EPOCHS="${EPOCHS:-50}"   # 50 over ~148k samples ≈ 3× the local 60-epoch exposure; bucketing keeps it ~1 day
SESSION="${SESSION:-megatrain${GIGA_OCR_VARIANT:+_${GIGA_OCR_VARIANT}}}"
LOG="$HOME/$SESSION.log"
NPROC="$(nproc)"

# ── "mega" config — scaled for 48 vCPU / 192 GB (override via env) ──────────────────
export GIGA_OCR_C1="${GIGA_OCR_C1:-32}"          # larger backbone than the 24/48/96 local run
export GIGA_OCR_C2="${GIGA_OCR_C2:-64}"
export GIGA_OCR_HID="${GIGA_OCR_HID:-128}"
export GIGA_OCR_NLINES="${GIGA_OCR_NLINES:-40000}"   # synthetic printed/HW-font lines
export GIGA_OCR_MAXCHARS="${GIGA_OCR_MAXCHARS:-16}"
export GIGA_OCR_FONTLIMIT="${GIGA_OCR_FONTLIMIT:-120}"
export GIGA_OCR_HW_FRAC="${GIGA_OCR_HW_FRAC:-0.4}"   # share of synthetic lines using HW fonts
export GIGA_OCR_HW_REAL="${GIGA_OCR_HW_REAL:-iam,rimes,norhand,newseye,belfort,popp,esposalles,cyrillic}"
export GIGA_OCR_HW_REAL_N="${GIGA_OCR_HW_REAL_N:-30000}"   # per-dataset cap (reuse-largest cache)
export GIGA_OCR_LANGS="${GIGA_OCR_LANGS:-eng,fra,deu,spa,ita,por,pol,ces,tur,vie,rus,ukr,bul,srp,ell}"
export GIGA_OCR_DEGRADE="${GIGA_OCR_DEGRADE:-0}"   # photo variant: 1 = heavy in-the-wild degradation aug
export GIGA_OCR_VARIANT="${GIGA_OCR_VARIANT:-}"    # output suffix, e.g. 'photo' → models/ocr_<group>_photo.gpocr
export GIGA_OCR_BATCH="${GIGA_OCR_BATCH:-256}"     # length-bucketed → large batch is efficient on CPU
export OMP_NUM_THREADS="$NPROC" MKL_NUM_THREADS="$NPROC"   # PyTorch CPU intra-op = all cores

# per-dataset download volume (real handwriting lines); reuse-largest cache means a bigger
# cached pull is reused, a smaller request is truncated — so these are upper bounds.
# "max data": full IAM/RIMES/POPP/Esposalles + large slices of the big corpora (NorHand
# 222k, NewsEye 51k, Belfort 25k available) — ~108k real lines, well within 192 GB RAM.
declare -A NDL=( [iam]=6482 [rimes]=10188 [norhand]=30000 [newseye]=20000
                 [belfort]=20000 [popp]=3835 [esposalles]=2328 [cyrillic]=15000 )

log() { echo "[$(date -u +%H:%M:%S)] $*"; }

# ── 1. System packages (sudo; honours $SUDO_PASS if passwordless sudo is unavailable) ──
SUDO="sudo"
if ! sudo -n true 2>/dev/null; then
    if [ -n "${SUDO_PASS:-}" ]; then SUDO="sudo -S"; else
        log "WARN: no passwordless sudo and no SUDO_PASS — skipping apt (assuming deps present)"; SUDO=""
    fi
fi
if [ -n "$SUDO" ]; then
    log "Installing system packages (python, tmux, fonts)…"
    echo "${SUDO_PASS:-}" | $SUDO apt-get update -qq || true
    echo "${SUDO_PASS:-}" | $SUDO apt-get install -y -qq \
        python3-venv python3-dev build-essential git tmux curl \
        fonts-noto-core fonts-noto-extra fonts-noto-ui-core \
        fonts-dejavu-core fonts-liberation fonts-freefont-ttf || \
        log "WARN: apt install partial — continuing"
fi

# ── 2. Python venv + Torch (CPU) ───────────────────────────────────────────────────
if [ ! -x "$VENV/bin/python" ]; then
    log "Creating venv at $VENV…"; python3 -m venv "$VENV"
fi
"$VENV/bin/pip" install -q --upgrade pip
if ! "$VENV/bin/python" -c "import torch" 2>/dev/null; then
    log "Installing torch (CPU)…"
    "$VENV/bin/pip" install -q torch==2.12.0 --index-url https://download.pytorch.org/whl/cpu
fi
"$VENV/bin/pip" install -q numpy pillow fonttools

# ── 3. HuggingFace token (unlocks/streams the handwriting corpora) ──────────────────
if [ -n "${HF_TOKEN:-}" ]; then
    mkdir -p "$HOME/.huggingface"; printf '%s' "$HF_TOKEN" > "$HOME/.huggingface/token"
    chmod 600 "$HOME/.huggingface/token"; log "HF token written."
fi

# ── 4. Handwriting fonts (Google Fonts, cmap-guarded) ───────────────────────────────
log "Fetching handwriting fonts for '$GROUP'…"
( cd "$REPO_DIR/tools/ocr" && "$VENV/bin/python" fonts.py "$GROUP" --handwriting ) || \
    log "WARN: handwriting-font fetch partial — continuing"

# ── 5. Real handwriting corpus — bounded concurrency (the datasets-server rate-limits
#       under heavy parallelism → HTTP 429; 3 streams + in-code retry-on-429 is reliable) ──
MAXJ="${GIGA_OCR_DL_CONCURRENCY:-3}"
log "Downloading real handwriting corpus (concurrency=$MAXJ, retry-on-429)…"
cd "$REPO_DIR"
for ds in "${!NDL[@]}"; do
    "$VENV/bin/python" tools/ocr/hw_datasets.py "$ds" "${NDL[$ds]}" > "$HOME/dl_$ds.log" 2>&1 &
    while [ "$(jobs -rp | wc -l)" -ge "$MAXJ" ]; do wait -n 2>/dev/null || break; done
done
wait
log "Corpus download finished. Cached lines:"
ls -1 /tmp/ocr_hw/*_train_*.npz 2>/dev/null | sed 's#.*/##' || true

# ── 6. Launch training DETACHED in tmux (survives disconnect / local shutdown) ──────
RUN="$HOME/run_$SESSION.sh"
cat > "$RUN" <<EOF
#!/usr/bin/env bash
set -uo pipefail
export GIGA_OCR_C1=$GIGA_OCR_C1 GIGA_OCR_C2=$GIGA_OCR_C2 GIGA_OCR_HID=$GIGA_OCR_HID
export GIGA_OCR_NLINES=$GIGA_OCR_NLINES GIGA_OCR_MAXCHARS=$GIGA_OCR_MAXCHARS
export GIGA_OCR_FONTLIMIT=$GIGA_OCR_FONTLIMIT GIGA_OCR_HW_FRAC=$GIGA_OCR_HW_FRAC
export GIGA_OCR_HW_REAL="$GIGA_OCR_HW_REAL" GIGA_OCR_HW_REAL_N=$GIGA_OCR_HW_REAL_N
export GIGA_OCR_LANGS="$GIGA_OCR_LANGS"
export GIGA_OCR_DEGRADE=$GIGA_OCR_DEGRADE GIGA_OCR_VARIANT="$GIGA_OCR_VARIANT" GIGA_OCR_BATCH=$GIGA_OCR_BATCH
export OMP_NUM_THREADS=$NPROC MKL_NUM_THREADS=$NPROC
cd "$REPO_DIR"
echo "=== $SESSION start \$(date -u) — backbone $GIGA_OCR_C1/$GIGA_OCR_C2/$GIGA_OCR_HID, $EPOCHS epochs, $NPROC threads, degrade=$GIGA_OCR_DEGRADE variant='$GIGA_OCR_VARIANT' ==="
exec "$VENV/bin/python" tools/train_ocr_crnn.py "$GROUP" "$EPOCHS"
EOF
chmod +x "$RUN"

if tmux has-session -t "$SESSION" 2>/dev/null; then
    log "tmux session '$SESSION' already exists — not relaunching. Attach: tmux attach -t $SESSION"
else
    tmux new-session -d -s "$SESSION" "bash '$RUN' 2>&1 | tee $LOG"
    log "Launched detached training in tmux '$SESSION'."
fi
log "Monitor:  tmux attach -t $SESSION   |   tail -f $LOG   |   grep epoch $LOG | tail"
