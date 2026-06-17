# OCR training log — line-level CRNN+CTC

> Chronological record of training the line-level recognizer (see
> [`OCR_ARCHITECTURE.md`](./OCR_ARCHITECTURE.md) for the design and
> [`OCR_TRAINING_DATA.md`](./OCR_TRAINING_DATA.md) for the data sources). All runs are
> **CPU-only**, offline; the runtime stays zero-dependency (int8 forward pass in
> `ocr_crnn.rs`). Reproduce with the commands below.

## Pipeline

```
corpus (Tesseract langdata_lstm) ─┐
coverage-filtered fonts (Noto/…)  ─┤→ render_lines.py → (32×W grayscale strip, text)
augmentation (blur + sensor noise)─┘            → train_ocr_crnn.py (CRNN + CTC)
        → int8 quantize → crates/core/src/raster/ocr_model_<group>.rs
```

- **Model** (`tools/train_ocr_crnn.py`, mirrors `ocr_crnn.rs`): conv1 1→16, conv2
  16→32 (3×3, pad 1, ReLU) + two 2×2 max-pools → collapse the 8 remaining rows to a
  width-`T` sequence → **bidirectional GRU** (hidden 64) → linear → `K+1` logits → CTC.
- **GRU formulation** matches the Rust runtime exactly (reset gate applied to the
  hidden state *before* the recurrent matmul, `n = tanh(Wn·x + Un·(r⊙h))`) — a custom
  cell, **not** `torch.nn.GRU` (whose order differs). Swapping in `nn.GRU` would make
  the trained weights wrong at inference.
- **Quantization**: symmetric int8 per tensor (conv/fc), one shared scale per GRU
  direction's input weights and one for its recurrent weights (matches `GruSpec`).
- **Decode**: CTC greedy (argmax → collapse runs → drop blank); blank = last index.

## Runs

| # | Config | Result | Takeaway |
|---|--------|--------|----------|
| 1 | 3 ep, 2 Google-API fonts, lr 1e-3 | val_CER **1.000** | All-blank CTC collapse **and** the API fonts were Latin-only → Cyrillic/Greek lines rendered as tofu (corrupt targets). |
| 2 | 30 ep, system fonts, lr 1e-3 | escaped at ep 8 (0.998) → **0.89** by ep 13 | Coverage-filtered system fonts fixed the tofu; lr 1e-3 escaped the all-blank basin but converged too slowly. |
| 3 | 60 ep, lr **3e-3 + StepLR**, no space class | val_CER **0.156** | Higher LR + decay: 0.64 (ep 8) → 0.29 (ep 17) → 0.156 (ep 60). Strong on clean strips. |
| 4 | 60 ep, lr 3e-3, **+ space class**, Sauvola inference | val_CER **0.174** | Space as a class fixed word boundaries (pipeline WER 1.00 → 0.70). Competitive with Tesseract — see below. |
| 5 | 60 ep, **60 fonts / 16k lines**, + disambiguation | val_CER **0.120** | More font/data diversity. **Beats Tesseract on CER** (0.248 vs 0.258) — see below. |
| 6 | 60 ep, group **`taml`** (Tamil, 121 classes, 120 fonts) | val_CER **0.045** | First non-Latin model. **Beats Tesseract** on Tamil (CER 0.091 vs 0.101, WER 0.39 vs 0.60). |

CER here is **per-character on held-out validation strips** (same render distribution),
measured inside the trainer — it isolates the *model*, not the full image pipeline.

## Diagnostics & fixes (the hard part)

1. **All-blank CTC collapse** (CER ≡ 1.0, exact). Signature: empty predictions for the
   first N epochs. Cause: gross undertraining (~200 steps) **+** Latin-only fonts. Fix:
   coverage-filtered system fonts (`fonts.system_fonts_for_group`, fontTools cmap) +
   many more steps. The 500 locally-installed Noto faces cover Latin+Cyrillic+Greek.
2. **Slow convergence** at lr 1e-3 (~1 %/epoch). Fix: lr 3e-3 + `StepLR` (halve every
   `epochs/3`). Escaped to 0.64 by epoch 8.
3. **Train/inference grayscale skew**. Training strips are antialiased grayscale
   (`render_lines`), but inference (`extract_line_strips`) emitted a hard 0/1 mask. Fix:
   sample grayscale ink intensity `(255−gray)/255` at inference so input statistics agree.
4. **Pipeline ≫ model error** (0.80 vs model's 0.156). Cause: line *over-segmentation* —
   the reused mono-glyph blob-center grouping split one line into several on
   ascenders/descenders/diacritics (`professionnel show` → `…show\nProTesslonne…`). Fix:
   **horizontal projection-profile** line bands → CER **0.80 → 0.37** in one change.
5. **Missing spaces** (WER ≈ 1.0). `_dedup` stripped the space from the class set, so the
   model never learned word boundaries. Fix: add space as an explicit class → run 4.
6. **Robustness to scans**: Sauvola adaptive binarization (`ocr::sauvola_ink`, integral
   images, O(1)/px) replaces global Otsu for line/column detection — handles uneven
   illumination where a global threshold collapses. Neutral on clean print.
7. **Script lookalike confusion** (Latin A / Greek Α / Cyrillic А): a single multi-script
   model can't disambiguate without context. `disambiguate_line` votes each token's script
   from its **unambiguous** letters and snaps ambiguous homoglyphs to it → CER 0.295 → 0.278,
   e.g. `«FRAΝΚFURTΕR` → `«FRANKFURTER`. A lexicon-lite step; full n-gram beam is future.

## Benchmark vs Tesseract 5.3.4

Method: `tools/ocr/bench.py` renders a held-out labelled test set (different seed, no
augmentation, dark-on-white, ×3 upscale), runs both engines on identical PNGs
(gigapdf via the `ocr_image` example built `--features ocr-<group>`; Tesseract
`--psm 7`), and reports micro-averaged CER/WER.

| Milestone | gigapdf CER | gigapdf WER | Tesseract CER | Tesseract WER |
|-----------|-------------|-------------|---------------|---------------|
| Run 3 + blob-grouping front-end | 0.80 | 1.04 | 0.26 | 0.62 |
| Run 3 + projection-profile front-end | 0.37 | 1.00 | 0.26 | 0.62 |
| Run 4 (+ space class, + Sauvola) | 0.295 | 0.70 | 0.258 | 0.624 |
| Run 4 + script disambiguation | 0.278 | 0.683 | 0.258 | 0.624 |
| **Run 5 (60 fonts / 16k lines) + disambiguation** | **0.248** | **0.637** | 0.258 | 0.624 |

Honest reading: the dependency-free CRNN now **matches and edges out Tesseract on CER**
(0.248 vs 0.258), WER essentially tied (0.637 vs 0.624) — having started at 0.80. The path:
projection-profile lines (0.80→0.37), space class (WER 1.00→0.70), homoglyph disambiguation
(0.295→0.278), then the improved retrain — 60 coverage-filtered fonts + 16k corpus lines,
val_CER 0.120 — closing the recognition-quality gap (0.278→0.248). **Caveats:** this is
synthetic, clean, machine-print text at the training distribution, on the four trained
languages (en/fr/ru/el). On real degraded scans, handwriting, and untrained languages
Tesseract is broader and likely still leads. A full lexicon/n-gram beam CTC and the other
script groups remain future work.

### Tamil (`taml` group) — first non-Latin model

| Engine | CER | WER |
|--------|-----|-----|
| **gigapdf** (`taml`, 121 classes, val_CER 0.045) | **0.091** | **0.390** |
| Tesseract 5.3.4 (`tam`, tessdata_best) | 0.101 | 0.602 |

A second script, a second win: the Tamil CRNN+CTC **beats Tesseract on both CER and WER**
on synthetic clean print. Tamil's smaller alphabet (121 vs alpha's 557) and the 120
coverage-filtered Noto Tamil faces let the same tiny backbone (16/32/64) reach val_CER
0.045. Shaping is correct because PIL has **raqm** (HarfBuzz) — Tamil matras/ligatures
render properly, not as isolated forms. CJK is deferred (a 16/32/64 backbone can't hold
3 000+ classes, and only one system CJK face is installed); Arabic/Hebrew/Indic-other need
their datasets. Same caveat as alpha: synthetic clean print at the training distribution.

## Reproduce

```bash
python3 -m venv --system-site-packages /tmp/ocrvenv && /tmp/ocrvenv/bin/pip install torch
python3 tools/ocr/fonts.py alpha                      # (optional) Google/Noto fonts
GIGA_OCR_NLINES=12000 GIGA_OCR_MAXCHARS=16 GIGA_OCR_LANGS=eng,fra,rus,ell \
  /tmp/ocrvenv/bin/python tools/train_ocr_crnn.py alpha 60     # → ocr_model_alpha.rs
cargo build --release -p gigapdf-core --features ocr-alpha --example ocr_image
/tmp/ocrvenv/bin/python tools/ocr/bench.py alpha 100 --lang=eng+fra+rus+ell
```

Knobs (env): `GIGA_OCR_NLINES`, `GIGA_OCR_MAXCHARS`, `GIGA_OCR_LANGS`,
`GIGA_OCR_FONTLIMIT`, `GIGA_OCR_LR`. Seeded (`7`) for reproducibility.

## Status of script groups

| Group | Scripts | Infra | Model |
|-------|---------|-------|-------|
| `alpha` | Latin-ext + Cyrillic + Greek | ✅ | ✅ trained — **beats Tesseract** (CER 0.248 vs 0.258) |
| `taml` | Tamil | ✅ | ✅ trained — **beats Tesseract** (CER 0.091 vs 0.101) |
| `cjk` | Chinese / Japanese / Korean | ✅ class sets + fonts | ⏳ not trained (capacity; CASIA-HWDB2 HW data available) |
| `arabic` | Arabic / Hebrew (RTL) | ✅ | ⏳ not trained |
| `deva` / `beng` | Indic (Devanagari, Bengali) | ✅ | ⏳ not trained |

Each group trains with the same command (`train_ocr_crnn.py <group>`) and wires in via
its `ocr-<group>` Cargo feature; no runtime code change.
