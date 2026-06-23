# gigapdf-lib

A **near-zero-dependency** PDF engine, written from scratch in Rust and compiled
to WebAssembly — read, edit, render, secure, **and convert** PDFs with **no
native libraries** and no third-party PDF/Office/image crate. The PDF core,
rasterizer, image codecs, HTML/CSS layout and format conversions are **all
hand-written**; the only third-party crates are **RustCrypto** (audited,
standards-compliant signatures & crypto) and **Boa** (the JavaScript engine).

The TypeScript SDK is published as **[`@qrcommunication/gigapdf-lib`](https://www.npmjs.com/package/@qrcommunication/gigapdf-lib)**
(see [`sdk/`](sdk/)); the self-contained `.wasm` ships inside it.

📖 **Full SDK API reference: <https://qrcommunication.github.io/gigapdf-lib/>** —
auto-generated from the source with [TypeDoc](https://typedoc.org): every exported
class, method, parameter, return type and model type, always current.

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

## Dependencies

The whole document toolkit is **hand-written** and compiles straight to
`wasm32` with **no native libraries** (no MuPDF, no LibreOffice, no fontkit, no
libwebp/dav1d). Written from scratch in pure `std`:

- Lexer, object parser, xref-streams, object-streams.
- `FlateDecode`/zlib **inflate *and* deflate** (RFC 1950/1951) from scratch.
- Content-stream interpreter + editor; renumbering serializer.
- Rasterizer: scanline fill (AA), PNG encoder, TrueType `glyf` + CFF Type2 glyph
  outlines, OpenType shaping (GSUB/GPOS), image XObject blit.
- Image codecs from scratch: PNG, JPEG, **WebP (incl. VP8L lossless)**, GIF
  (multi-frame), **AVIF (AV1 intra, multi-tile)**, SVG (incl. `<text>`).
- ZIP reader/writer, OOXML/ODF builders, a from-scratch PDF page builder.
- HTML + CSS layout engine and the PDF↔Office/HTML/RTF converters.

Two third-party crates are used **on purpose**, where rolling our own would be
irresponsible:

- **[RustCrypto](https://github.com/RustCrypto)** for standards-compliant
  cryptography & digital signatures — `rsa`, `sha2`/`sha1`/`md-5`, `hmac`,
  `aes`/`cbc`/`des`/`rc2`, and `cms`/`x509-cert`/`der`/`spki`/`const-oid` for
  CMS/PKCS#7/PKCS#12. Audited, constant-time primitives instead of hand-rolled
  crypto.
- **[Boa](https://github.com/boa-dev/boa)** (`boa_engine`) — the JavaScript
  engine that runs a document's inline `<script>`s before HTML layout (replaces
  the earlier from-scratch interpreter).

The WebAssembly sandbox has **no network and no entropy** — those come from the
host through a tiny port (the host supplies `crypto.getRandomValues` bytes and
performs Google-Fonts downloads). Everything else is in the engine.

## Feature matrix

| Area | Capabilities |
|------|--------------|
| **Read** | PDF 1.7, xref + object streams, FlateDecode, encrypted (RC4/AESV2/AESV3) |
| **Write** | Renumbering serializer, `save`, `save_compressed` (Flate streams) |
| **Edit content** | Text edit/remove (with **underline / strikethrough** decorations), elements (text/image/shape) list/remove/move/**affine-transform** (move + resize + rotate in place)/**reorder** (native z-order: bring to front / send to back)/duplicate/add; **in-place vector restyle** (`setPathStyle`: fill/stroke/width/dash + **real opacity**); **constant opacity on any element** (`setElementOpacity` — text/image/shape); draw text/rect/line/ellipse/polygon/SVG-path/image (opacity + PNG alpha); hit-test |
| **Text extraction** | Font-aware, zero-tofu via WinAnsi + `/ToUnicode` CMap (CID/Type0); per-run colour/size/rotation/direction; document language detection |
| **Headers / footers** | Bake a running header/footer onto an existing PDF (`{{page}}`/`{{pages}}` tokens) and **read back** what's baked; per-page margins read/write |
| **Annotations** | Highlight, underline, strike-out, squiggly, free-text, square, line, ink, sticky note, stamp, link; rich read-back metadata; **flatten** |
| **Forms (AcroForm)** | Text/checkbox/radio/combo/list/signature fields — **read · fill · create** (build widgets from scratch with appearance streams + `NeedAppearances`) |
| **Pages** | Rotate, delete, move, extract, merge, resize, insert, copy; bookmarks/outline; metadata; embedded-file attachments |
| **Security** | Encrypt/permissions; **digital signatures at four PAdES levels** — **B** self-signed (RSA/X.509/CMS), **B** PKCS#12 (import a user `.p12`/`.pfx` natively — PBES2 AES + PBES1 3DES/RC2, MAC-verified — no node-forge/@signpdf), **B-T** RFC 3161 trusted timestamp, **B-LT / B-LTA** long-term validation (`/DSS` with the cert chain + OCSP/CRL revocation material, optional archival `/DocTimeStamp`) — the host fetches TSA/OCSP/CRL (pure-data two-phase, no network in the WASM core); **true redaction** (delete from stream) + **`redactPii`** *(v0.52.4)* — irreversible redaction that also **erases image pixels** (safe on scans/OCR) under an opaque mark |
| **Render** | Rasterize a page to PNG (vector + TrueType/CFF glyphs + images + **OpenType shaping**: GPOS marks, GSUB contextual, Arabic joining), **without its text** (`renderPageNoText`) or **omitting specific elements** (`renderPageExcluding`) for live-overlay editing; **run highlight** (character background) painted across PDF/HTML/Office; **non-Device colorspaces** (Separation/ICCBased/Pattern fills) resolved; native image codecs — encode/decode PNG · JPEG · **WebP (incl. VP8L lossless)**, decode **GIF (multi-frame)** + **AVIF (AV1 intra, multi-tile)** + **SVG (incl. `<text>`)**; alpha-correct resize |
| **Text intelligence** | Font-aware extraction, **structured text** (reading-order lines + boxes), **full-text search** with highlight boxes |
| **OCR** | **`gigapdf-ocr-rten`** crate (host-side) — **PaddleOCR PP-OCR** (DBNet detect + SVTR/CRNN recognize) on **RTen**, a **pure-Rust ONNX runtime (no C++, no Tesseract)**. 13 printed languages incl. **Hebrew** (own model) + Arabic (RTL), CJK, Cyrillic, Devanagari, Tamil/Telugu/Kannada, Latin — with **automatic per-line script selection** — **plus opt-in Latin handwriting** (our own trained CRNN — IAM/RIMES/…). State-of-the-art (PaddleOCR beats Tesseract on most scripts) |
| **Convert →** | PDF / model → **TXT, MD (Markdown), CSV, EPUB, HTML, DOCX, PPTX, ODP, ODT, XLSX, ODS, RTF** (real editable elements, not a page image). DOCX/XLSX/PPTX/ODF import preserves **images, hyperlinks, strikethrough, highlighting, formulas, grouped shapes, charts, SmartArt text, and master/layout inheritance** |
| **Convert ←** | **TXT, MD, HTML, RTF, DOCX, ODT, ODP, PPTX, XLSX, ODS** → PDF (ODF `.odt`/`.ods`/`.odp` are fully bidirectional) · raster **PNG, JPEG, GIF, WebP, AVIF** → PDF (one A4 page, centred & shrink-to-fit) |
| **Unified editable model** | Format-neutral document tree (sections → pages → blocks → runs) with full **Markdown** modelling (code blocks, block-quotes, horizontal rules): lower **any** format in (`toModel`/`officeToModel`/`htmlToModel`), edit with structured ops (`applyModelOps`), raise to **any** format (`modelTo{Docx,Xlsx,Pptx,Odt,Ods,Odp,Pdf,Html,Rtf,Md,Csv,Epub}`) — edit every format the same way |
| **HTML rendering** | Native **HTML + CSS → PDF** engine (parser, selector cascade, block / inline / table / **flex** (direction · justify-content · grow) / **grid** / **multi-column** layout, pagination, **`page-break-*` + `<pagebreak>`**, running header/footer in the page margins) — no headless browser. Rich CSS: **linear / radial / conic gradients**, **box-shadow** (blur), **border-radius** (elliptical), dashed/dotted borders, **font-weight 100–900**, **`position: sticky`**, RTL/bidi. Text set in **embedded Google fonts** (real glyphs + metrics, identical or nearest match) |
| **JavaScript** | Built-in **[Boa](https://github.com/boa-dev/boa)** JS engine runs a document's inline `<script>`s before layout — **no Chromium/Playwright**. Full ES with classes, closures, destructuring, generators, `async`/`await` + `Promise`, `RegExp`, `Map`/`Set`/`Symbol`, and standard built-ins. **DOM bindings** (ours): `getElementById`, `querySelector(All)` (`#id`/`.class`/`tag`/`>`/`+`/`~`/`[attr]`), `textContent`, `innerHTML`, `createElement`/`appendChild`, `classList`, `style`, … |
| **Archival** | **PDF/A-2b** metadata (XMP + sRGB OutputIntent + ID) |
| **Fonts** | Draw **and edit** real text in **every font source & any font file** — built-in **base-14 standard fonts** (no embedding), any family / **Google Font** (1951-family catalog + URL builder + **TrueType *and* OpenType-CFF embedding**: glyf→Type0/CIDFontType2+FontFile2, `.otf`/`OTTO`→Type0/CIDFontType0+FontFile3, Identity-H + full widths + ToUnicode), and the **document's own embedded faces** (`embeddedFonts` + `extractFont` → re-embed). `addText` **and** font-aware `replaceText` resolve any face's char→glyph map (`FontFile2`/`FontFile3`); needed-font detection |

All of it is exercised by `cargo test` (**1262 tests**, all green, `clippy`
clean — image codecs validated bit-exact against reference decoders, e.g. AVIF
vs `dav1d`), a Node WASM smoke test (end-to-end, all green), and **validated
externally**: generated Office files (DOCX/PPTX/XLSX **and ODT/ODS/ODP**) open
and round-trip in LibreOffice; embedded fonts verify as `emb=yes` under
poppler's `pdffonts`.

## Honest scope

Conversions are **content-and-layout faithful**, not pixel-perfect re-typesetting.
PDF→Office reconstructs **real, editable objects** (positioned text boxes,
re-embedded images, table cells) the way an office suite's PDF import does — not a
rendered page image. Office→PDF is **text-faithful** (all content, reading order,
pagination) using the standard-14 fonts; pixel-perfect re-layout of an arbitrary,
richly-styled document stays the job of a full layout engine. Full PDF/A
conformance additionally requires every font embedded (the engine can do that).

The **JavaScript engine is [Boa](https://github.com/boa-dev/boa)**, a mature,
spec-focused Rust JS implementation — so script-driven HTML templates get full
ECMAScript (classes, closures, destructuring/spread, `RegExp`, `Map`/`Set`/
`Symbol`, generators, `async`/`await` + `Promise`). The **DOM bindings** that
expose the document to those scripts (`document.*`, element manipulation,
`classList`, `style`) are ours. By design the sandbox has **no network and no
real timers** — scripts run once, before layout. CSS **flex** supports
direction/grow/shrink/wrap/justify/align, **grid** lays out
`grid-template-columns` (`fr`/`minmax`/`repeat`/`span`), **multi-column** is
supported, and **float** maps to inline-block; gradients, box-shadow,
border-radius and `position: sticky` are honoured at render.

## OCR & text intelligence

Text already in a PDF is extracted **font-aware** (zero tofu) with reading-order
lines and bounding boxes, and is searchable with highlight boxes.

For **scanned, image-only pages**, OCR is the **`gigapdf-ocr-rten`** crate — a
**host-side** engine running **PaddleOCR PP-OCR** models through **RTen**, a
pure-Rust ONNX runtime (**no C++, no Tesseract**). It carries the ML weights and
runs natively; the lean pure-`std` `core`/`wasm` stay dependency-free and the host
exposes OCR as an endpoint.

- **Pipeline:** a shared **DBNet** text detector → per-line **SVTR/CRNN+CTC**
  recognition → automatic **per-line script selection** (each line is routed to the
  highest-confidence recognizer — no separate script classifier needed).
- **Languages (13 printed):** Arabic (RTL), **Hebrew** (RTL — our own trained model,
  since PaddleOCR ships none), Simplified & Traditional Chinese, Cyrillic, Devanagari,
  English, Japanese, Kannada, Korean, Latin (French/German/Spanish/…), Tamil, Telugu.
  PaddleOCR PP-OCRv4/v5 covers 100+ scripts — add one by dropping its `.rten` + dict
  into the models dir (`REC_MODELS` in `crates/ocr-rten/src/lib.rs`).
- **Handwriting (opt-in):** a Latin/Cyrillic/Greek **handwriting** recognizer (`latin_hw`) — our
  own CRNN trained on real IAM/RIMES/NorHand/… lines + synthetic (standard `nn.LSTM` →
  **dynamic-width** ONNX). Excluded from auto-selection (a HW model is overconfident on printed
  input); call `recognize_page_handwriting(img)` for handwriting-heavy input.
- **State of the art:** PaddleOCR beats Tesseract on most scripts. Validated: a Chinese line
  decoded 100% (conf 0.999); multilingual auto-routing (Korean→ko, Japanese→ja,
  Russian→cyrillic) correct on a mixed page.
- **Models** are fetched/trained at deploy time (`crates/ocr-rten/tools/fetch_models.sh`,
  ONNX→`.rten` via `rten-convert`); Hebrew by `tools/train_hebrew.py`, handwriting by
  `tools/train_handwriting.py`. **Not committed** (lean package, like fonts).

```rust
let eng = gigapdf_ocr_rten::OcrEngine::load_models_dir("models")?;
for line in eng.recognize_page(&rgb_image)? {
    println!("[{:.2}|{}] {}", line.confidence, line.model, line.text);
}
```

See [`crates/ocr-rten/README.md`](crates/ocr-rten/README.md) for the full pipeline.

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
let signed = doc.sign(&signer, "Me", "Approval", "D:20260614120000Z")?; // B (self-signed)
let out = doc.save();
```

> Signing comes in **four PAdES levels** — **B** (self-signed or PKCS#12), **B-T**
> (RFC 3161 timestamp), and **B-LT / B-LTA** (long-term validation: `/DSS` +
> OCSP/CRL, optional archival timestamp). From the SDK: `doc.signP12`,
> `doc.signTimestamped`, `doc.signLtv` — see the
> [signing recipes](docs/COOKBOOK.md#sign-a-pdf-b--b-t--ltv) in the Cookbook.

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
| [**SDK API reference** (hosted)](https://qrcommunication.github.io/gigapdf-lib/) | **Auto-generated TypeDoc** — the complete, always-current API surface: every exported class, method, parameter, return type and model type (`Giga*`). |
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

The release `.wasm` is ~5.6 MB (the hand-written PDF/Office/image/HTML engine is
~0.5 MB; the rest is the bundled **Boa** JS engine), versus ~14 MB for MuPDF —
and it carries **no native libraries**.

## License & provenance

PolyForm Noncommercial 1.0.0. Built clean-room from the ISO 32000 specification;
**no AGPL code (e.g. MuPDF) was ever read or copied.** See [`LICENSE`](LICENSE).
