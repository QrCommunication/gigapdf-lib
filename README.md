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
| **Edit content** | Text edit/remove, elements (text/image/shape) list/remove/move/duplicate/add; draw text/rect/line/ellipse/polygon/SVG-path/image (opacity + PNG alpha); hit-test |
| **Text extraction** | Font-aware, zero-tofu via WinAnsi + `/ToUnicode` CMap (CID/Type0) |
| **Annotations** | Highlight, underline, strike-out, free-text, square, line, ink, stamp, link; **flatten** |
| **Forms (AcroForm)** | Text/checkbox/radio/combo/list/signature fields — **read · fill · create** (build widgets from scratch with appearance streams + `NeedAppearances`) |
| **Pages** | Rotate, delete, move, extract, merge; bookmarks/outline; metadata |
| **Security** | Encrypt/permissions, **self-signed digital signature** (RSA/X.509/CMS), **PKCS#12 signing** (import a user `.p12`/`.pfx` natively — PBES2 AES + PBES1 3DES/RC2, MAC-verified — no node-forge/@signpdf), **true redaction** (delete from stream, no opaque cover) |
| **Render** | Rasterize a page to PNG (vector + TrueType/CFF glyphs + images) |
| **Text intelligence** | Font-aware extraction, **structured text** (reading-order lines + boxes), **full-text search** with highlight boxes |
| **OCR** | Built-in recognizer — Otsu → connected components → line/word segmentation → MLP trained on **EMNIST handwriting + synthetic font glyphs** (Latin + accents). No Tesseract, no model download at runtime |
| **Convert →** | PDF → **TXT, HTML, DOCX, PPTX, ODP, ODT, XLSX, ODS, RTF** (real editable elements, not a page image) |
| **Convert ←** | **TXT, HTML, RTF, DOCX, ODT, ODP, PPTX, XLSX, ODS** → PDF (ODF `.odt`/`.ods`/`.odp` are fully bidirectional) |
| **HTML rendering** | Native **HTML + CSS → PDF** engine (parser, selector cascade, block / inline / table / **flex** (direction · justify-content · grow) / **grid** layout, pagination, **`page-break-*` + `<pagebreak>`**) — no headless browser. Text set in **embedded Google fonts** (real glyphs + metrics, identical or nearest match) |
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

- **Trained today:** group **`alpha`** — **Latin-extended + Cyrillic + Greek** printed
  (Polish, Czech, Turkish, Vietnamese, Russian, Ukrainian, Greek, …). On a synthetic
  multi-script clean-print benchmark it lands **within ~2 CER points of Tesseract 5.3.4**
  (CER 0.278 vs 0.258, WER 0.68 vs 0.62 — see [`docs/OCR_TRAINING_LOG.md`](docs/OCR_TRAINING_LOG.md)),
  with **homoglyph script disambiguation** snapping Latin/Greek/Cyrillic lookalikes (A/Α/А).
- **Infra ready, not yet trained:** `cjk` (Chinese/Japanese/Korean), `arabic`
  (Arabic/Hebrew, RTL), `deva`/`beng`/`taml` (Indic) — class sets, fonts and the trainer
  are in place; each is one training run away, with **no runtime change**.
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
docs/         API.md · SDK.md · USAGE.md · INSTALL.md · OCR_ARCHITECTURE.md · OCR_TRAINING_DATA.md · OCR_TRAINING_LOG.md
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
| [`docs/USAGE.md`](docs/USAGE.md) | Cookbook: the buffer ABI plus a worked example for every feature area. |
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
