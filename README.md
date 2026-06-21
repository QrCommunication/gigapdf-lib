# gigapdf-lib

A **zero-dependency** PDF engine, written from scratch in Rust and compiled to
WebAssembly — read, edit, render, secure, **and convert** PDFs with no third-party
crates and no native libraries.

The TypeScript SDK is published as **[`@qrcommunication/gigapdf-lib`](https://www.npmjs.com/package/@qrcommunication/gigapdf-lib)**
(see [`sdk/`](sdk/)); the self-contained `.wasm` ships inside it.

> Copyright 2025 Rony Licha / QR Communication.
> Licensed under the **PolyForm Noncommercial License 1.0.0** — see [`LICENSE`](LICENSE).
> Required Notice: Copyright 2025 Rony Licha / QR Communication.

## Why it exists

The previous editor used a Fabric.js **overlay + cosmetic mask**, which cannot
reconstruct a complex background (gradient, image, pattern) under edited text.
This engine edits the **real PDF content stream**: it physically removes/edits/adds
the page operators, so the background is preserved *by construction* and the
original glyphs never leak. It then grew into a self-contained PDF toolkit so the
product depends on **no** external PDF/Office/font library (no MuPDF, no
LibreOffice, no fontkit) for its core flows.

## Zero dependencies

**None.** Everything is pure `std` and compiles straight to `wasm32`:

- Lexer, object parser, xref-streams, object-streams.
- `FlateDecode`/zlib **inflate *and* deflate** (RFC 1950/1951) from scratch.
- Content-stream interpreter + editor; renumbering serializer.
- Crypto from scratch: MD5, RC4, AES-128/256, SHA-256/384/512, big-integer
  modular arithmetic (Montgomery), RSA, ASN.1 DER, X.509, CMS/PKCS#7.
- Rasterizer: scanline fill (AA), PNG encoder, TrueType `glyf` + CFF Type2 glyph
  outlines, image XObject blit.
- ZIP reader/writer, OOXML/ODF builders, a from-scratch PDF page builder.

The WebAssembly sandbox has **no network and no entropy** — those come from the
host through a tiny port (the host supplies `crypto.getRandomValues` bytes and
performs Google-Fonts downloads). Everything else is in the engine.

## Feature matrix

| Area | Capabilities |
|------|--------------|
| **Read** | PDF 1.7, xref + object streams, FlateDecode, encrypted (RC4/AESV2/AESV3) |
| **Write** | Renumbering serializer, `save`, `save_compressed` (Flate streams) |
| **Edit content** | Text edit/remove (with **underline / strikethrough** decorations), elements (text/image/shape) list/remove/move/**affine-transform** (move + resize + rotate in place)/duplicate/add; **in-place vector restyle** (`setPathStyle`: fill/stroke/width/dash); draw text/rect/line/ellipse/polygon/SVG-path/image (opacity + PNG alpha); hit-test |
| **Text extraction** | Font-aware, zero-tofu via WinAnsi + `/ToUnicode` CMap (CID/Type0); per-run colour/size/rotation/direction; document language detection |
| **Headers / footers** | Bake a running header/footer onto an existing PDF (`{{page}}`/`{{pages}}` tokens) and **read back** what's baked; per-page margins read/write |
| **Annotations** | Highlight, underline, strike-out, squiggly, free-text, square, line, ink, sticky note, stamp, link; rich read-back metadata; **flatten** |
| **Forms (AcroForm)** | Text/checkbox/radio/combo/list/signature fields — **read · fill · create** (build widgets from scratch with appearance streams + `NeedAppearances`) |
| **Pages** | Rotate, delete, move, extract, merge, resize, insert, copy; bookmarks/outline; metadata; embedded-file attachments |
| **Security** | Encrypt/permissions, **self-signed digital signature** (RSA/X.509/CMS), **PKCS#12 signing** (import a user `.p12`/`.pfx` natively — PBES2 AES + PBES1 3DES/RC2, MAC-verified — no node-forge/@signpdf), **true redaction** (delete from stream) + **`redactPii`** *(v0.52.4)* — irreversible redaction that also **erases image pixels** (safe on scans/OCR) under an opaque mark |
| **Render** | Rasterize a page to PNG (vector + TrueType/CFF glyphs + images); native image codecs — encode/decode PNG · JPEG · lossless WebP, decode GIF + **AVIF** (AV1 intra); alpha-correct resize |
| **Text intelligence** | Font-aware extraction, **structured text** (reading-order lines + boxes), **full-text search** with highlight boxes |
| **OCR** | Built-in recognizer — **photo auto-crop (4-corner perspective dewarp) + illumination flat-field** front-end → line/word segmentation → int8 **CRNN+CTC** line models per script (Latin/Cyrillic/Greek, Arabic/Hebrew, Devanagari, Bengali, Tamil, **Chinese**, **Japanese/Korean** in training) + handwriting & photo/degraded variants. Beats Tesseract on most trained scripts. No Tesseract, no model download at runtime |
| **Convert →** | PDF → **TXT, HTML, DOCX, PPTX, ODP, ODT, XLSX, ODS, RTF** (real editable elements, not a page image) |
| **Convert ←** | **TXT, HTML, RTF, DOCX, ODT, ODP, PPTX, XLSX, ODS** → PDF (ODF `.odt`/`.ods`/`.odp` are fully bidirectional) · raster **PNG, JPEG, GIF, WebP, AVIF** → PDF (one A4 page, centred & shrink-to-fit) |
| **Unified editable model** | Format-neutral document tree (sections → pages → blocks → runs): lower **any** format in (`toModel`/`officeToModel`/`htmlToModel`), edit with structured ops (`applyModelOps`), raise to **any** format (`modelTo{Docx,Xlsx,Pptx,Odt,Ods,Odp,Pdf,Html,Rtf}`) — edit every format the same way |
| **HTML rendering** | Native **HTML + CSS → PDF** engine (parser, selector cascade, block / inline / table / **flex** (direction · justify-content · grow) / **grid** layout, pagination, **`page-break-*` + `<pagebreak>`**, running header/footer in the page margins) — no headless browser. Text set in **embedded Google fonts** (real glyphs + metrics, identical or nearest match) |
| **JavaScript** | Built-in zero-dependency **JS engine** that runs a document's inline `<script>`s before layout — **no Chromium/Playwright**. Lexer → parser → tree-walking interpreter with **classes + `super`**, closures, destructuring, generators (`function*`/`yield`), **`async`/`await` + `Promise`** (microtask queue + `setTimeout`), and built-ins: `Object`/`Array`/`String`/`Number`/`Math`/`JSON`/`console`/`Map`/`Set`/**`RegExp`** + a backtracking regex engine. **DOM bindings**: `getElementById`, `querySelector(All)` (`#id`/`.class`/`tag`/`>`/`+`/`~`/`[attr]`), `textContent`, `innerHTML`, `createElement`/`appendChild`, `classList`, `style`, … |
| **Archival** | **PDF/A-2b** metadata (XMP + sRGB OutputIntent + ID) |
| **Fonts** | Draw **and edit** real text in **every font source & any font file** — built-in **base-14 standard fonts** (no embedding), any family / **Google Font** (1951-family catalog + URL builder + **TrueType *and* OpenType-CFF embedding**: glyf→Type0/CIDFontType2+FontFile2, `.otf`/`OTTO`→Type0/CIDFontType0+FontFile3, Identity-H + full widths + ToUnicode), and the **document's own embedded faces** (`embeddedFonts` + `extractFont` → re-embed). `addText` **and** font-aware `replaceText` resolve any face's char→glyph map (`FontFile2`/`FontFile3`); needed-font detection |

All of it is exercised by `cargo test` (**284 tests**, incl. a 100-test pure-Rust
JavaScript engine: lexer, parser, interpreter, built-ins, regex, DOM, and a
suspendable bytecode VM with lazy generators, spec-ordered async, and full
control-flow — `try`/`catch`/`finally`, `switch`, labels, destructuring,
spread), a Node WASM smoke test
(end-to-end, all green), and **validated externally**: generated Office files
(DOCX/PPTX/XLSX **and ODT/ODS/ODP**) open and round-trip in LibreOffice; embedded
fonts verify as `emb=yes` under poppler's `pdffonts`.

## Honest scope

Conversions are **content-and-layout faithful**, not pixel-perfect re-typesetting.
PDF→Office reconstructs **real, editable objects** (positioned text boxes,
re-embedded images, table cells) the way an office suite's PDF import does — not a
rendered page image. Office→PDF is **text-faithful** (all content, reading order,
pagination) using the standard-14 fonts; pixel-perfect re-layout of an arbitrary,
richly-styled document stays the job of a full layout engine. Full PDF/A
conformance additionally requires every font embedded (the engine can do that).

The **JavaScript engine** targets the language used by templating/report scripts:
classes/`super`, closures, destructuring/spread, `RegExp`, `Map`/`Set`, `Symbol`
(real, with the iterator protocol), `eval`/`Function`, tagged templates, and
`import`/`export` (parsed transparently). `function*`/`async` bodies compile to a
**suspendable bytecode VM**, so generators are **truly lazy** (infinite
`while (true) { yield … }` works, `.next(v)` is bidirectional, `yield*` delegates
lazily) and `await` **yields to the event loop** with spec microtask ordering.
The VM covers the full statement/expression language used by templates —
`try`/`catch`/`finally`, `for…of`/`for…in`, `switch`, labelled `break`/
`continue`, destructuring, compound assignment, and `...spread` — all able to
span a `yield`/`await`. A handful of corner cases (a `return`/`break` *through* a
`finally`, a logical `&&=`/`||=`/`??=` with an awaited right-hand side, sparse
array holes) transparently fall back to the eager generator / synchronous-await
model — same results, just not lazy.
By design the sandbox has **no network and no real timers** (`setTimeout`
resolves on the microtask queue). CSS **flex** supports `flex-direction`,
`justify-content` and `flex-grow`; **grid** lays out `grid-template-columns`;
**float** maps to inline-block.

## OCR & text intelligence

Text already in a PDF is extracted **font-aware** (zero tofu) with reading-order
lines and bounding boxes, and is searchable with highlight boxes. For **scanned,
image-only pages** the engine has a built-in OCR following the classic Tesseract
pipeline — Otsu binarization → connected-component blobs → line/word segmentation
→ per-glyph classification — but with a from-scratch, dependency-free classifier:

- The classifier is a **compact CNN trained offline** on two public sources:
  **EMNIST** (NIST handwritten digits + letters, public domain) for **handwriting**,
  and **synthetic glyphs rendered from thousands of fonts** (system + Google Fonts,
  the Tesseract `text2image` approach) for **printed text, punctuation and accented
  Latin**.
- Training is build-time only (`tools/train_ocr_cnn.py`); the engine ships the
  **int8-quantized weights** and runs a pure-`std` forward pass — no ML library,
  no model download at runtime.
- **Scripts/languages (mono-glyph engine):** Latin — `0-9 A-Z a-z`, common
  punctuation, and accented Latin (`é è à ç ñ ü …`) for French, Spanish, German,
  Portuguese, etc. Both **printed and handwritten** Latin are recognized.
- **Honest accuracy:** strong on clean machine print, decent on tidy handwriting
  (EMNIST-grade); noisy scans and dense layouts are harder.

**Line-level CRNN+CTC engine (opt-in, multi-script).** A second recognizer removes the
per-glyph segmentation that caps the classic pipeline (touching glyphs, cursive scripts,
noisy scans). It reads a whole text line as a sequence — Otsu **or Sauvola** binarization
→ projection-profile line bands → CNN → bidirectional GRU → CTC — still a **pure-`std`
int8** forward pass (`crates/core/src/raster/ocr_crnn.rs`), no ML dependency. Models are
per script group, trained offline (`tools/train_ocr_crnn.py`) and enabled via Cargo
features (`ocr-alpha`, …); `ocr()` uses the CRNN when a model is embedded and falls back
to the mono-glyph classifier otherwise.

### Benchmarks — CER vs Tesseract 5.3.4

Character Error Rate (CER, lower is better) on held-out benchmarks — a **dependency-free, in-WASM** recognizer measured against the reference engine. Full methodology and per-run history in [`docs/OCR_TRAINING_LOG.md`](docs/OCR_TRAINING_LOG.md).

| Script | gigapdf-lib | Tesseract 5.3.4 | |
|---|---|---|---|
| **Latin-ext + Cyrillic + Greek** (clean print) | **0.119** | 0.258 | ✅ ~2.2× better — WER 0.41 vs 0.62 |
| **Arabic / Hebrew** (RTL, non-mirrored) | **0.063** | 0.349 | ✅ ~5.5× better |
| **Tamil** | **0.077** | 0.101 | ✅ beats — WER 0.39 vs 0.60 |
| **Devanagari** (Hindi, …) | **0.078** | 0.089 | ✅ beats |
| **Bengali** | 0.097 | 0.073 | ≈ competitive (font/data-bound, not capacity) |
| **Latin handwriting** (IAM test) | **0.309** | 0.353 | ✅ first dependency-free engine to beat Tesseract on real handwriting — WER 0.737 vs 0.775 |
| **Chinese (CJK)** | **0.206** | — | CASIA handwritten, 2401-class data-driven charset |

Every model is an **int8 CRNN+CTC** running **client-side in WebAssembly** — **no Tesseract binary, no runtime model download**. The larger 32/64/128 backbone roughly **halves** Indic/Arabic validation CER (deva 0.039, beng 0.042, taml 0.011, arabic 0.030 — capacity, not data, was the bound). *Honest caveat:* heavily degraded or dense scans still favour Tesseract's breadth.

- **Trained today:** group **`alpha`** — **Latin-extended + Cyrillic + Greek** printed
  (Polish, Czech, Turkish, Vietnamese, Russian, Ukrainian, Greek, …). On a synthetic
  multi-script clean-print benchmark it **comfortably beats Tesseract 5.3.4** — CER **0.119
  vs 0.258** (~2.2×), WER 0.41 vs 0.62 (larger 24/48/96 backbone; see
  [`docs/OCR_TRAINING_LOG.md`](docs/OCR_TRAINING_LOG.md)) — with **homoglyph disambiguation**
  snapping Latin/Greek/Cyrillic lookalikes (A/Α/А).
  *Caveat:* synthetic clean print on the four trained languages; real degraded scans and
  untrained scripts still favour Tesseract's breadth.
- **Also trained (non-Latin):** **Tamil** (`taml`) — **beats Tesseract** (0.077 vs 0.101);
  **Arabic + Hebrew** (`arabic`, **RTL**) — beats Tesseract on synthetic (0.063 vs 0.349),
  output verified non-mirrored; **Devanagari** (`deva`, larger 24/48/96 backbone) — now
  **beats Tesseract** (0.078 vs 0.089); **Bengali** (`beng`) — competitive (0.097 vs 0.073),
  font/data-bound. **Chinese (CJK)** (`cjk`, 2401-class) — CER **0.206** on CASIA handwritten. Backbone is env-tunable (`GIGA_OCR_C1/C2/HID`); PIL **raqm**
  shaping handles Indic/Arabic forms.
- **Handwriting:** a handwriting variant **`ocr_alpha_hw.gpocr`** (32/64/128 backbone, trained
  on ~108k real handwriting lines — IAM/RIMES/NorHand/NewsEye/Belfort/POPP/Esposalles/Cyrillic
  via the HF datasets-server — plus synthetic *Handwriting* fonts) **beats Tesseract on real
  cursive: CER 0.309 vs 0.353** (WER 0.737 vs 0.775) on the IAM test set. The printed champion
  stays primary for clean scans; load the HW variant via `gp_ocr_load_model` for
  handwriting-heavy input — see [`docs/OCR_TRAINING_DATA.md`](docs/OCR_TRAINING_DATA.md).
- **CJK (Chinese):** **trained** — `ocr_cjk.gpocr` (data-driven **2401-class** charset, 32/64/128
  backbone, ~93k real lines: priyank-m printed + CASIA handwriting) reaches **CER 0.206 on CASIA
  handwritten Chinese**. Host-load via `gp_ocr_load_model`.
- **Japanese & Korean:** their own groups (`jpn`, `kor`) and datasets (synthetic **150k JP** /
  **200k KR**) are wired in; each gets a data-driven charset (kana+kanji / Hangul) **plus the full
  ASCII set** so mixed alphanumerics (prices, dates, codes) are read, trained on the real corpus +
  Latin synthetic lines so those glyphs are actually seen. *(Training in progress on a VPS; the
  real-dataset download is now concurrent — `GIGA_OCR_DL_WORKERS`, lifted by an HF Pro token.)*
- **Degraded-input front-end** (`ocr.rs` + `dewarp.rs`, **no retrain**): before recognition `ocr()`
  **auto-crops a photographed page** — finds the document's four corners on a contrasting background
  and perspective-warps it head-on (`rectify_document`: bright mask → largest component → 8×8 DLT
  homography → bilinear warp) — then **flattens uneven illumination** (flat-field divide by a local
  background; shadows/glare → uniform page). Both **gated to no-op on already-clean scans**, so they
  only act on phone photos / creased paper. Pairs with the photo variant `ocr_alpha_photo.gpocr`
  (degradation-augmented; beats the plain HW model on degraded input): augmentation hardens the
  model, the front-end fixes the input.
- Design: [`docs/OCR_ARCHITECTURE.md`](docs/OCR_ARCHITECTURE.md) · data catalogue:
  [`docs/OCR_TRAINING_DATA.md`](docs/OCR_TRAINING_DATA.md) · training log:
  [`docs/OCR_TRAINING_LOG.md`](docs/OCR_TRAINING_LOG.md).

## Layout

```
crates/core   gigapdf-core  — the whole engine (parse, inflate, edit, render, crypto, convert)
crates/wasm   gigapdf-wasm  — extern "C" WebAssembly bindings (zero-dep ABI)
fixtures/     test PDFs
test/         wasm-smoke.mjs — end-to-end Node harness
tools/        catalog/ICC generators + snapshots
docs/         SDK.md · COOKBOOK.md · USAGE.md · API.md · HTML-CSS.md · INSTALL.md · OCR_ARCHITECTURE.md · OCR_TRAINING_DATA.md · OCR_TRAINING_LOG.md
```

## Quickstart

### Rust

```rust
use gigapdf_core::Document;

let mut doc = Document::open(&bytes)?;
let docx = doc.to_docx();            // PDF → editable Word
let pdf  = gigapdf_core::convert::reverse::txt_to_pdf("Hello\nWorld"); // text → PDF
doc.embed_truetype_font("Roboto", &ttf)?; // host-downloaded font
let signed = doc.sign(&signer, "Me", "Approval", "D:20260614120000Z")?;
let out = doc.save();
```

### Browser / Node (WebAssembly)

```js
const { instance } = await WebAssembly.instantiate(wasmBytes, {});
const ex = instance.exports;
const handle = ex.gp_open(ptr, len);     // returns an opaque handle
const docx = callBuffer(() => ex.gp_to_docx(handle, lenPtr)); // → Uint8Array
ex.gp_close(handle);
```

### Documentation

| Doc | What's in it |
|-----|--------------|
| [`docs/SDK.md`](docs/SDK.md) | **Complete TypeScript SDK reference** — every `GigaPdfEngine`/`GigaPdfDoc` method, grouped by domain, with parameters, returns and notes. |
| [`docs/COOKBOOK.md`](docs/COOKBOOK.md) | **Task-oriented recipes** — redaction, styled text, headers/footers, conversions, OCR, forms, annotations, signing, encryption, and the editable model, each as a short runnable snippet. |
| [`docs/USAGE.md`](docs/USAGE.md) | Host integration: the raw `extern "C"` buffer ABI plus a worked example for every feature area. |
| [`docs/API.md`](docs/API.md) | The Rust ↔ WASM ABI mapping (every `gp_*` export and its Rust method). |
| [`docs/HTML-CSS.md`](docs/HTML-CSS.md) | The **exhaustive** list of supported HTML elements, CSS properties, units, colours, selectors and JS in the HTML→PDF renderer. |
| [`docs/INSTALL.md`](docs/INSTALL.md) | Install, build-from-source, and Next.js (`output: "standalone"`) wiring. |

## Build

```bash
cargo test -p gigapdf-core   # native tests (real fixtures)
cargo wasm                   # build the WASM engine (alias, see .cargo/config.toml)
node test/wasm-smoke.mjs     # end-to-end WASM smoke test
```

`cargo wasm` is a repo alias for the full target build, so you never type the
target triple by hand (`cargo wasm-dev` for a debug build).

The release `.wasm` is ~540 KB — **zero dependencies**, versus ~14 MB for MuPDF.

## License & provenance

PolyForm Noncommercial 1.0.0. Built clean-room from the ISO 32000 specification;
**no AGPL code (e.g. MuPDF) was ever read or copied.** See [`LICENSE`](LICENSE).
