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
| 7 | 60 ep, `alpha`+HW: 25 % HW fonts **+ 3 800 real IAM/RIMES** | val_CER 0.358 (mixed) | Handwriting variant `ocr_alpha_hw.gpocr`: real-HW CER 0.891→**0.499** (−44 %); clean print 0.248→0.282, so printed model stays primary — see below. |
| 8 | 60 ep, group **`deva`** (Devanagari, 138 classes, 100 fonts) | val_CER **0.108** | Competitive on Devanagari — CER 0.104 vs Tesseract `hin` 0.089 (trails ~1.5 pts; conjuncts are hard). |
| 9 | 60 ep, group **`beng`** (Bengali, 129 classes, 60 fonts) | val_CER **0.105** | Competitive on Bengali — CER 0.104 vs Tesseract `ben` 0.073 (Indic stacked conjuncts favour Tesseract). |
| 10 | 60 ep, group **`arabic`** (Arabic+Hebrew, **RTL**, 142 classes, 4 fonts) | val_CER **0.054** | First RTL model: targets reversed to visual order for the monotonic CTC, runtime reverses back. Output verified **not** mirror-flipped. CER 0.071 vs Tesseract 0.349 (in-distribution; see caveat). |
| 11 | 60 ep, **`deva` larger backbone 24/48/96** (vs 16/32/64) | val_CER **0.080** (was 0.108) | Capacity was the Indic bottleneck: deva **flips to beating Tesseract** (CER 0.104→**0.078** vs 0.089). `.gpocr` 124 KB (was 62). Backbone now env-tunable (`GIGA_OCR_C1/C2/HID`); being applied to all groups. |
| 12 | 60 ep, **`alpha` larger backbone 24/48/96**, 16k lines | val_CER **0.093** (was 0.120) | Big win: clean-print CER **0.248→0.119** — now **~2.2× better than Tesseract** (0.258). 557 classes were badly capacity-starved at 16/32/64. `.gpocr` 207 KB. |
| 13 | 60 ep, **`taml` larger backbone 24/48/96** | val_CER **0.030** (was 0.045) | Tamil CER 0.091→**0.077** vs Tesseract 0.101 — wider win. |

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
| **Larger backbone 24/48/96 (run 12)** | **0.119** | **0.406** | 0.258 | 0.624 |

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

### Handwriting — printed vs handwriting-augmented `alpha` (two specialised models)

Run 7 trained a handwriting mix into `alpha` — 25 % of synthetic lines rendered in
Google-Fonts *Handwriting* faces (199, cmap-guarded) **+ 3 800 real IAM/RIMES lines**
(HF datasets-server). One backbone can't fully excel at clean print **and** cursive at once,
so we ship **two artifacts** and let the host pick at load time (`gp_ocr_load_model`). Both
were retrained at the **larger 24/48/96 backbone**:

| Model | Clean-print CER | Real-handwriting CER (IAM test, n=80) |
|-------|-----------------|----------------------------------------|
| `ocr_alpha.gpocr` (printed champion, 24/48/96) | **0.119** (beats Tesseract 0.258) | 0.839 |
| `ocr_alpha_hw.gpocr` (handwriting-augmented, 24/48/96) | 0.187 | **0.440** |
| Tesseract 5.3.4 | 0.258 | 0.353 |

The handwriting mix roughly **halves** handwriting CER (0.839 → 0.440, **−48 %**) while still
beating Tesseract on clean print (0.187 vs 0.258); the printed champion stays the clean-print
leader (0.119). The larger backbone improved **both** axes over the original 16/32/64 variant
(clean 0.282→0.187, cursive 0.499→0.440). gigapdf's handwriting model still trails Tesseract
on cursive (0.440 vs 0.353): closing that needs **more real handwriting data** — an **HF
token** unlocks the gated IAM-full / CASIA / KHATT / IIIT-HW corpora (`hw_datasets._hf_token`).
Knobs: train with `GIGA_OCR_HW_FRAC` + `GIGA_OCR_HW_REAL="iam,rimes,…"`; eval with
`tools/ocr/bench_hw.py`. Bake a chosen variant into its Cargo feature with
`tools/ocr/gpocr_to_rs.py`.

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
| `alpha` | Latin-ext + Cyrillic + Greek | ✅ | ✅ trained, **larger backbone 24/48/96** — CER **0.119** vs Tesseract 0.258 (was 0.248 at 16/32/64 — capacity was the limit) |
| `taml` | Tamil | ✅ | ✅ trained, **24/48/96** — beats Tesseract (CER **0.077** vs 0.101; was 0.091 at 16/32/64) |
| `cjk` | Chinese / Japanese / Korean | ✅ class sets + fonts | ⛔ **deliberately not trained** (scope decision) — a 152-char fallback would be a toy; a real CJK model needs the full frequency charset (`load_charset`), many CJK faces (1 system), and a much larger backbone for 3 000+ classes |
| `arabic` | Arabic / Hebrew (**RTL**) | ✅ | ✅ trained **24/48/96**, RTL verified; CER **0.063** vs Tesseract 0.349 (was 0.071; only 4 fonts cap the gain) |
| `deva` | Devanagari | ✅ | ✅ trained, **larger backbone 24/48/96** — now **beats Tesseract** (CER **0.078** vs 0.089; was 0.104 at 16/32/64) |
| `beng` | Bengali | ✅ | ✅ trained **24/48/96** — competitive (CER **0.097** vs Tesseract `ben` 0.073; font/data-limited, not capacity) |

Each group trains with the same command (`train_ocr_crnn.py <group>`) and wires in via
its `ocr-<group>` Cargo feature; no runtime code change.
