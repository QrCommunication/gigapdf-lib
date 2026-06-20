# OCR architecture — from mono-glyph CNN to line-level CRNN+CTC

> Companion to [`OCR_TRAINING_DATA.md`](./OCR_TRAINING_DATA.md). This document explains the
> **current** recognizer, **why** it cannot reach Tesseract-level accuracy by adding data
> alone, and the **target** design — a line-level **CRNN + CTC** recognizer that still runs
> with a **zero-dependency, pure-`std`** int8 forward pass at runtime.

## 1. Current pipeline (mono-glyph classifier)

Source: `crates/core/src/raster/ocr.rs`, model `crates/core/src/raster/ocr_model.rs`,
trainer `tools/train_ocr_cnn.py` (shared data pipeline in `tools/train_ocr.py`).

```
grayscale page
  → otsu_threshold()            global binarization
  → connected_components()      8-connected ink blobs (1 blob ≈ 1 glyph)
  → filter by size / median h   drop speckle, rules, borders
  → group blobs into lines      sort by vertical centre (line_tol)
  → segment words by gaps       space_gap heuristic
  → normalize() each blob       scale to INK_BOX, centre in 28×28 (ink=1)
  → classify()                  int8 CNN → argmax over 110 classes
```

The classifier is a compact CNN (pure-`std` forward pass):
`conv1 1→16 (3×3) → maxpool2 → conv2 16→32 (3×3) → maxpool2 → fc 1568→128 → fc 128→110`,
int8 weights + f32 bias/scales (`conv2d_relu`, `maxpool2`, `dense` in `ocr.rs`).
**State:** ~61 % per-glyph validation accuracy; Latin only (`0-9 A-Z a-z`, punctuation,
accented Latin).

## 2. Why this has a structural ceiling

Adding fonts/data lifts per-glyph accuracy (≈61 % → 85-90 % on clean print) but **cannot**
reach Tesseract on real material, because the *unit of recognition is a single connected
component*:

1. **Touching / broken glyphs.** Real scans merge (`rn`→`m`) or split (`m`→`r`+`i`+`i`)
   components. A perfect per-glyph classifier still mis-reads them — the segmentation already
   lost. `connected_components()` can't recover this.
2. **No language context.** Each glyph is classified independently; there is no implicit
   language model to disambiguate `0/O`, `1/l/I`, `rn/m`. Tesseract 4/5's LSTM models the
   *sequence*.
3. **Cursive / connected scripts are unsegmentable.** Arabic (4 contextual forms, ligatures),
   Devanagari (conjuncts + matras), Nastaliq, and cursive handwriting do **not** decompose into
   isolated glyph blobs. Per-glyph classification is *structurally impossible* here — this is
   the decisive reason a sequence model is required for the non-Latin goal.

**Conclusion:** data is *necessary but not sufficient*. Tesseract-parity needs a **line-level
sequence recognizer**.

## 3. Target: CRNN + CTC line recognizer (Tesseract 4/5 paradigm)

Recognize a **whole text line** as a sequence, no per-glyph segmentation:

```
text-line strip (H=32, variable W, ink-normalized)
  → CNN backbone           int8 convs + pools → feature map (C × 1 × W')
  → collapse height        → sequence of W' feature vectors
  → bidirectional GRU      small hidden size (e.g. 2×64), per-timestep context
  → linear → logits        per timestep over (alphabet + blank)
  → CTC greedy decode      argmax per step → collapse repeats → drop blank → text
```

- **Line extraction** reuses the existing blob→line grouping in `ocr.rs`, but emits a
  **full-line raster strip** instead of isolated blobs (the mono-glyph path stays as fallback).
- **CTC** removes the need for character boundaries: the network emits a label/blank per
  horizontal step and CTC merges them — exactly what makes touching/cursive text work.
- **Why GRU over LSTM:** fewer gates → smaller int8 model and a simpler pure-`std` forward
  pass, with comparable accuracy at this scale.

### 3.1 Per-script models (not one giant alphabet)

A single unified alphabet is impractical (CJK alone = thousands of classes → huge int8 file,
slow softmax). Mirror Tesseract's *per-language traineddata*:

| Model | Scripts | ~classes | Notes |
|---|---|---|---|
| `alpha` | Latin-extended + Cyrillic + Greek | ~300 | Segmentable; shared LTR model. |
| `cjk` | Chinese / Japanese / Korean | 3,000–6,500 | Common-char set; larger backbone. |
| `arabic` | Arabic / Urdu / Hebrew | ~200 | **RTL**; Arabic contextual shaping. |
| `indic` | Devanagari / Bengali / Tamil / … | per-script | Conjuncts/matras; one per script-group. |

A fast **script detector** (`script_detect.rs`) routes each line strip to the right model and
sets reading direction (LTR/RTL). Models are **feature-gated** in `Cargo.toml`
(`ocr-latin`, `ocr-cjk`, `ocr-arabic`, `ocr-indic`) so a WASM build only embeds what it needs.

### 3.2 Pure-`std` int8 inference (no ML dependency)

Everything stays implementable in `std`, in the style of today's `ocr.rs`:

- **Conv / pool / dense:** already exist (`conv2d_relu`, `maxpool2`, `dense`) — reuse/extend.
- **GRU cell:** per timestep, `z = σ(Wz·x + Uz·h)`, `r = σ(Wr·x + Ur·h)`,
  `n = tanh(Wn·x + Un·(r⊙h))`, `h = (1−z)⊙n + z⊙h`. Just matvec + `σ`/`tanh` + elementwise —
  int8 weights, f32 state. Bidirectional = run forward and backward, concat.
- **CTC greedy decode:** `argmax` per timestep → collapse consecutive equals → drop the blank
  index. (Optional beam + dictionary later.)

No external crate; the new file `crates/core/src/raster/ocr_crnn.rs` mirrors the existing
quantization contract (`*_W: [i8]`, `*_SCALE: f32`, `*_B: [f32]`).

## 4. Training workflow (offline, build-time only)

```
fonts (Noto + Google + system)  ─┐
corpora (langdata/Leipzig/Wiki) ─┤→ render_lines.py → (line image, transcription) pairs
augmentation (blur/noise/skew)  ─┘                     + real datasets (per script)
        → train_ocr_crnn.py (PyTorch, CTC loss, per script)
        → int8 quantize → emit crates/core/src/raster/ocr_model_<script>.rs
        → record source + licence + CER/WER in the file header
```

Reuses `tools/train_ocr.py` helpers (`usable_fonts`, quantize/emit pattern). Seeded
(`torch.manual_seed(7)`) and dataset-cached for reproducibility. The **runtime never changes**
when retraining — only the embedded int8 weights.

## 5. Benchmark methodology — defining "Tesseract level"

"Tesseract level" must be **measured**, not asserted:

- **Metric:** Character Error Rate (CER) and Word Error Rate (WER) per script, on a held-out
  labelled eval set (`fixtures/ocr/` + ground truth).
- **Baseline:** run **Tesseract** (`tessdata_best`) on the *same* fixtures via
  `tools/ocr/eval.py`; report a side-by-side CER/WER table per script and print quality
  (clean / degraded / handwritten / scene).
- **Target:** CER within a small margin of Tesseract on clean print first, then close the gap
  on degraded scans and non-Latin scripts.
- **Regression:** the mono-glyph path and `ocr()`/`OcrWord` API stay green; per-feature WASM
  build stays under size budget.

## 6. Migration & coexistence

**Status (built):** `ocr_crnn.rs` (pure-`std` CNN + bidirectional GRU + CTC greedy),
Sauvola adaptive binarization, projection-profile line bands, and the `tools/ocr/`
trainer/data pipeline are in place. Group **`alpha`** (Latin-extended + Cyrillic + Greek)
is **trained** (`ocr_model_alpha.rs`, val_CER 0.120) and **matches/edges out Tesseract on
CER** (0.248 vs 0.258) on clean multi-script print (see
[`OCR_TRAINING_LOG.md`](./OCR_TRAINING_LOG.md)). `ocr()`
routes to the CRNN when a per-script model is embedded and **falls back** to the
mono-glyph classifier otherwise.

- Per-script models are **feature-gated** (`ocr-alpha`, `ocr-cjk`, …); the default build
  embeds none, so it stays at the base size and behaviour.
- **Trained:** `alpha`, `taml`, `arabic` (RTL), `deva`, `beng`, and **`cjk`** (`.gpocr` blobs) —
  see the [training log](./OCR_TRAINING_LOG.md). **`cjk` is now a real model**: a data-driven
  **2401-class** charset (top-frequency Han + ASCII, `tools/ocr/build_cjk_charset.py`), Noto CJK
  faces (`.ttc`), 32/64/128 backbone, trained on ~93k real lines (priyank-m printed + CASIA
  handwriting) — **CER 0.206 on CASIA handwritten Chinese**. Japanese/Korean share the group but
  need dedicated data; the charset can grow (more samples → more classes).
- Public API (`Document::ocr_page`, `OcrWord`, WASM `gp_ocr_*`, SDK `doc.ocr`) is preserved.

**Host-loaded models (built).** Weights ship as a compact **`.gpocr`** blob the host loads
at runtime via the **`gp_ocr_load_model(ptr,len)`** WASM export (like the fonts/randomness
ports) — so a single lean `.wasm` stays ~540 KB and OCR is opt-in at runtime, with **no
weights baked**. Verified **bit-identical** to the feature-baked path (alpha: a 118 KB blob
gives the same CER/WER). Format: `LoadedModel::from_bytes` in `ocr_crnn.rs`; emitted by
`tools/train_ocr_crnn.py` (and `tools/ocr/rs_to_gpocr.py` converts an existing baked model
without retraining). The Cargo `ocr-*` features remain as an optional build-time embed.

**Script disambiguation (built).** `disambiguate_line` (in `ocr_crnn.rs`) votes each
token's script from its **unambiguous** letters and snaps homoglyphs (Latin A / Greek Α /
Cyrillic А) to it — fixed most of the multi-script lookalike confusion (CER 0.295 → 0.278 at
that step, before the improved retrain; e.g. `«FRAΝΚFURTΕR` → `«FRANKFURTER`).

**Deskew + despeckle (built).** `extract_line_strips` estimates page skew (projection-
variance over ±5.7°, centred shear), deskews via a bilinear rotation, and despeckles small
connected components — robust to tilted/noisy scans, no-ops on clean print (skew ≈ 0).

**Configurable backbone (built).** Conv/GRU sizes are env-overridable (`GIGA_OCR_C1` /
`C2` / `HID`); the `.gpocr` format and the runtime read every dimension from the blob, so a
**larger model** (for dense Indic, or a future CJK) trains and host-loads with no runtime change.

**Degraded / photographed documents (crumpled, receipts, phone photos) — strategy.** Poor
real-world input fails on three axes: **geometry** (perspective, curl, crumple), **photometry**
(shadows, glare, blur, noise, JPEG, low-res), and **domain gap** (the model has only seen clean
synthetic). Plan, by ROI — all staying pure-`std` (no ML dewarp net):

1. **Front-end restoration in `ocr.rs` (no retrain) — illumination done; dewarp planned.**
   **Illumination normalization is implemented**: `normalize_illumination` flat-fields the page
   (divide by a large-window local-mean background → shadows/glare/paper-gradients flatten to a
   uniform bright background, text contrast preserved; O(1)/px via an integral image), gated by
   `illumination_is_uneven` (4×4 brightness-spread test) so clean scans/print are byte-for-byte
   unchanged. It runs at the top of `ocr()`, feeding the **grayscale** strip extractor
   (`extract_line_strips` already samples grayscale, not a hard binarization). **Still planned:**
   **perspective dewarp** (document quadrilateral → homography) + **per-line baseline dewarp** for
   curled receipts, denoise + local contrast (CLAHE) + light super-resolution for small receipt
   text. (Unit-tested in `ocr.rs`; an end-to-end CER gain on degraded full-page fixtures is the
   next validation — pairs with the photo variant below: augmentation hardens the model, the
   front-end fixes the input.)
2. **Photo/degraded model variant — tooling built.** A heavy "in-the-wild" domain-randomization
   augmentation (curl wave, shear, uneven illumination, background haze, blur, low-res, JPEG,
   noise, contrast jitter) lives in `render_lines.py::_degrade`, gated by `GIGA_OCR_DEGRADE=1`.
   Train it as a **separate** model (no clobber of the clean primary) via `GIGA_OCR_VARIANT=photo`
   → `models/ocr_alpha_photo.gpocr`; the host loads it for noisy input. Launch on a VPS:
   `GIGA_OCR_DEGRADE=1 GIGA_OCR_VARIANT=photo bash deploy/train_vps.sh`. Add real receipt/photo
   corpora (SROIE, CORD, FUNSD) — see [`OCR_TRAINING_DATA.md`](./OCR_TRAINING_DATA.md).
3. **Lexicon / char-n-gram beam CTC — planned.** LM rescoring fixes garbled characters when the
   visual signal is weak (the biggest decoder lever on degraded input); the `disambiguate_line`
   homoglyph vote is the lexicon-lite first step.
4. **Test-time augmentation** — decode a few preprocessing variants, keep the highest CTC confidence.

**Also planned:** multi-column **XY-cut** layout analysis; larger backbones to push the
competitive Indic models past Tesseract. **CJK Chinese is now trained** (CER 0.206 on CASIA);
**Japanese/Korean** extend the same group once dedicated data is added.
