# OCR architecture

gigapdf-lib's OCR is a **host-side** engine in the **`gigapdf-ocr-rten`** crate. It runs
**PaddleOCR PP-OCR** models through **RTen** — a pure-Rust ONNX inference runtime (the engine
behind `ocrs`), with **no C++ and no Tesseract dependency**. The lean pure-`std` `core`/`wasm`
crates stay dependency-free; the heavy ML weights live in this native crate and the host exposes
OCR as a service/endpoint.

> Historical note: earlier versions shipped a from-scratch, hand-trained int8 CRNN/CTC recognizer
> embedded in the pure-`std` core. It was retired — per-tensor int8 quantization collapsed recurrent
> non-Latin recognizers, and the small models lost to Tesseract on complex scripts. The pivot to
> pre-trained PaddleOCR on RTen replaced it entirely. See [`OCR_TRAINING_LOG.md`](OCR_TRAINING_LOG.md)
> for that engine's archived log.

## 1. Pipeline

```
PDF page ──render_page(scale)──▶ PNG ──▶ RGB image
                                          │
                                          ▼
                              ┌─ DBNet text detector (shared, language-agnostic)
                              │     prob map → binarize → connected-component boxes → unclip
                              ▼
                        line crops (reading order: top→bottom, left→right)
                              │
                              ▼
              ┌─ per line: run EVERY loaded recognizer (PaddleOCR SVTR/CRNN + CTC)
              │     pick the result with the highest mean CTC confidence  ◀── auto script selection
              ▼
        decoded text (RTL recognizers reverse visual→logical) + box + confidence + winning model
```

- **Detection** — one **DBNet** model (`det.rten`) for all scripts: preprocess (ImageNet
  normalize, long side ≤ 960, dims ×32) → probability map `[1,1,H,W]` → threshold 0.3 →
  4-connected components → axis-aligned boxes → unclip (expand ~30% of box height) → original-image
  coords.
- **Recognition** — per-language **SVTR/CRNN + CTC** (`<lang>/model.rten` + `dict.txt`). Input is a
  line crop resized to height 48, RGB, normalized `(px/255 − 0.5)/0.5`, `[1,3,48,W]`. Output
  `[1,T,C]` → CTC greedy decode (argmax per step, collapse repeats, drop blank).
- **Automatic script selection** — with several recognizers loaded, each line is run through all of
  them and the highest-mean-confidence result wins. No separate script-classifier model is needed;
  the shared detector is script-agnostic, only the recognizer + dict vary.

## 2. CTC charlist convention

Every recognizer's class list is `[blank] + dict + [space]`:

```
class 0      = CTC blank
class 1..=N  = dict.txt lines (one char per line)
class N+1    = space (PaddleOCR use_space_char)
```

`OcrEngine::load` / `load_models_dir` build this list from `dict.txt`; the model's output dimension
is `len(dict) + 2`.

## 3. RTL scripts (Arabic, Hebrew)

A CTC model scans left→right and emits glyphs in **visual** order. For RTL scripts that is the
reverse of logical (reading) order, so RTL recognizers are flagged `rtl: true` and the decoded token
sequence is reversed back to logical order. Embedded LTR runs (digits, Latin) are handled by the
BiDi algorithm at training time (the Hebrew model is trained on visual-order labels via
`python-bidi`).

## 4. Languages

13 recognizers (shared DBNet detector). PaddleOCR PP-OCRv3/v4 covers ~12; **Hebrew** is our own
model (PaddleOCR/EasyOCR/MMOCR ship none).

| key | script | RTL | source |
|---|---|---|---|
| `ar` | Arabic | ✔ | PaddleOCR `arabic_PP-OCRv3_rec` |
| `he` | Hebrew | ✔ | **our model** (`tools/train_hebrew.py`) |
| `zh` / `zh_tw` | Chinese (Simplified / Traditional) | | `ch_PP-OCRv4_rec` / `chinese_cht_PP-OCRv3_rec` |
| `ja` / `ko` | Japanese / Korean | | `japan_PP-OCRv3_rec` / `korean_PP-OCRv3_rec` |
| `cyrillic` | Russian/Ukrainian/… | | `cyrillic_PP-OCRv3_rec` |
| `devanagari` | Hindi/Marathi/… | | `devanagari_PP-OCRv3_rec` |
| `en` / `latin` | English / French·German·Spanish·… | | `en_PP-OCRv4_rec` / `latin_PP-OCRv3_rec` |
| `ta` / `te` / `kn` | Tamil / Telugu / Kannada | | `ta`/`te`/`ka_PP-OCRv3_rec` |

The manifest is `REC_MODELS` in `crates/ocr-rten/src/lib.rs`. Add a language by appending an entry
and dropping its `<subdir>/{model.rten,dict.txt}` into the models dir — PaddleOCR covers 100+ scripts.

## 5. Public API

```rust
use gigapdf_ocr_rten::{OcrEngine, OcrWord};

// Load the shared detector + every available recognizer from a models dir.
let eng = OcrEngine::load_models_dir("models")?;

// OCR a raw image:
for line in eng.recognize_page(&rgb_image)? { /* line.text, .bbox, .confidence, .model */ }

// OCR a PDF page (rasterized via gigapdf-core), boxes in PDF user space:
let words: Vec<OcrWord> = eng.ocr_pdf_page(&doc, page, 2.0)?;
let text: String        = eng.ocr_pdf_page_text(&doc, page, 2.0)?;
```

- `OcrEngine::new(det)` + `add_rec(name, rec, dict, rtl)` — build incrementally.
- `OcrEngine::load(det, rec, dict)` — single-recognizer convenience.
- `OcrWord { text, x, y, width, height, confidence, model }` — PDF user space (bottom-left origin),
  the replacement for the old `Document::ocr_page`.

## 6. Models & deployment

Models are **not committed** (kept out of the lean package, like fonts). At deploy time:

1. `crates/ocr-rten/tools/fetch_models.sh [out_dir]` downloads PaddleOCR ONNX (det + 12 rec) from
   `deepghs/paddleocr` on Hugging Face and converts each to `.rten` (`pip install rten-convert`).
2. The Hebrew model is produced by `crates/ocr-rten/tools/train_hebrew.py` (ONNX → `rten-convert`)
   and dropped into `<out_dir>/hebrew/{model.rten,dict.txt}`.

Layout consumed by `load_models_dir`:

```
models/
  det.rten
  arabic_PP-OCRv3_rec/{model.rten,dict.txt}
  hebrew/{model.rten,dict.txt}
  …
```

## 7. WASM boundary

The OCR engine is **native only** (RTen + the models are far heavier than the lean ~540 KB WASM
core, and run server-side). The WASM SDK keeps the text-layer/extraction/search APIs
(`extractText`, `structuredText`, `search`, `addTextLayer`) — to make a scan searchable, OCR it
host-side with `ocr_pdf_page` and stamp the words back with `addTextLayer`.

## 8. Quality

PaddleOCR is state of the art and beats Tesseract on most scripts. Validated so far: a Chinese line
decoded 100% (confidence 0.999); multilingual auto-routing correct on a mixed page (Korean→`ko`,
Japanese→`ja`, Russian→`cyrillic`), Korean & Latin perfect, Cyrillic ~90%. Detection + recognition
both run through RTen with no external binary.
