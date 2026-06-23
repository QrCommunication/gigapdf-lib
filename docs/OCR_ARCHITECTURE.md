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

Profile-dependent (see §4):

- **PaddleStd**: class 0 = blank, 1..=N = `dict.txt` lines, N+1 = space (`use_space_char`); output
  dim = `len(dict) + 2`.
- **Gray32**: `dict.txt` IS the alphabet (idx i → char), blank = `len(alphabet)` (last class);
  output dim = `len(alphabet) + 1`.

`load_charlist` builds the right list + blank index from the profile.

## 3. RTL scripts (Arabic, Hebrew)

A CTC model scans left→right and emits glyphs in **visual** order. For RTL scripts that is the
reverse of logical (reading) order, so RTL recognizers are flagged `rtl: true` and the decoded token
sequence is reversed back to logical order. Embedded LTR runs (digits, Latin) are handled by the
BiDi algorithm at training time (the Hebrew model is trained on visual-order labels via
`python-bidi`).

## 4. Languages & input profiles

14 recognizers (shared DBNet detector). PaddleOCR PP-OCRv3/v4 covers 12 printed scripts; **Hebrew**
is our own trained CRNN (PaddleOCR/EasyOCR/MMOCR ship none); **`latin_hw`** is our own handwriting
CRNN (trained on real IAM/RIMES/… lines + synthetic). Each recognizer declares an **input profile**
(`Profile` in `lib.rs`):

- **`PaddleStd`** — PaddleOCR convention: RGB, height 48, normalize `(px/255−0.5)/0.5`, `[1,3,48,W]`
  (dynamic width); CTC **blank = 0**, charlist `[blank] + dict + [space]`.
- **`Gray32`** — our handwriting CRNN: **grayscale**, height 32, ink `= 1 − gray` (dark text → 1),
  tight-cropped to the ink and resized at its **natural (dynamic) width** `[1,1,32,W]` (standard
  `nn.LSTM` → dynamic-width ONNX, so no padding); CTC **blank = last**, charlist = the dict alphabet.

| key | script | RTL | profile | source |
|---|---|---|---|---|
| `ar` | Arabic | ✔ | PaddleStd | PaddleOCR `arabic_PP-OCRv3_rec` |
| `he` | Hebrew | ✔ | PaddleStd | **our trained CRNN** (`tools/train_hebrew.py`, ONNX→RTen) |
| `zh` / `zh_tw` | Chinese (Simp./Trad.) | | PaddleStd | `ch_PP-OCRv4_rec` / `chinese_cht_PP-OCRv3_rec` |
| `ja` / `ko` | Japanese / Korean | | PaddleStd | `japan_PP-OCRv3_rec` / `korean_PP-OCRv3_rec` |
| `cyrillic` | Russian/Ukrainian/… | | PaddleStd | `cyrillic_PP-OCRv3_rec` |
| `devanagari` | Hindi/Marathi/… | | PaddleStd | `devanagari_PP-OCRv3_rec` |
| `en` / `latin` | English / French·German·… (printed) | | PaddleStd | `en_PP-OCRv4_rec` / `latin_PP-OCRv3_rec` |
| `ta` / `te` / `kn` | Tamil / Telugu / Kannada | | PaddleStd | `ta`/`te`/`ka_PP-OCRv3_rec` |
| `latin_hw` | **Latin/Cyrillic/Greek handwriting** | | Gray32 | **our trained CRNN** (`tools/train_handwriting.py`, real IAM/RIMES/… + synthetic) |

The manifest is `REC_MODELS` in `crates/ocr-rten/src/lib.rs`. Add a language by appending an entry
and dropping its `<subdir>/{model.rten,dict.txt}` into the models dir — PaddleOCR covers 100+ scripts.

### Handwriting is opt-in (not in auto-selection)

Auto script selection (`recognize_page`) competes **only PaddleStd printed recognizers** by mean
argmax-logit. Handwriting (`Gray32`) models are **excluded** from that competition: an
undertrained/specialized CRNN is overconfident on out-of-domain input and would hijack routing
(and mean-logit isn't comparable across alphabet sizes anyway). So handwriting is **explicit** —
the caller selects it when the input is known to be handwriting, via `recognize_page_with(img,
"latin_hw")` / `recognize_line_with`. This matches the old engine, where the host explicitly loaded
the HW model for handwriting-heavy input.

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

// Handwriting (opt-in — bypasses auto selection):
let hw: Vec<Line> = eng.recognize_page_handwriting(&rgb_image)?; // or recognize_page_with(.., "latin_hw")
```

- `OcrEngine::new(det)` + `add_rec(name, rec, dict, rtl)` / `add_rec_profiled(.., profile)` — build incrementally.
- `OcrEngine::load(det, rec, dict)` — single-recognizer convenience.
- `recognize_page` / `recognize_line_auto` — auto script selection over printed recognizers.
- `recognize_page_with(img, name)` / `recognize_line_with(line, name)` — force a specific recognizer
  (e.g. `"latin_hw"` for handwriting).
- `OcrWord { text, x, y, width, height, confidence, model }` — PDF user space (bottom-left origin),
  the replacement for the old `Document::ocr_page`.

## 6. Models & deployment

Models are **not committed** (kept out of the lean package, like fonts). At deploy time:

1. `crates/ocr-rten/tools/fetch_models.sh [out_dir]` downloads PaddleOCR ONNX (det + 12 rec) from
   `deepghs/paddleocr` on Hugging Face and converts each to `.rten` (`pip install rten-convert`).
2. **Hebrew** — pull the pre-trained weights from Hugging Face
   (**`ronylicha/gigapdf-ocr-hebrew`**: `model.rten` + `dict.txt`) into `<out_dir>/hebrew/`, or retrain
   with `crates/ocr-rten/tools/train_hebrew.py` (→ ONNX → `rten-convert`).
3. **Handwriting** (`latin_hw`) — `crates/ocr-rten/tools/train_handwriting.py` trains the CRNN on
   real handwriting (IAM/RIMES/NorHand/… via `hw_datasets`) + synthetic lines, exports a
   dynamic-width ONNX → `rten-convert` → `<out_dir>/latin_hw/{model.rten,dict.txt}`.

Layout consumed by `load_models_dir`:

```
models/
  det.rten
  arabic_PP-OCRv3_rec/{model.rten,dict.txt}
  hebrew/{model.rten,dict.txt}
  latin_hw/{model.rten,dict.txt}
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
