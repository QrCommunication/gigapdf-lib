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
| **Edit content** | Text edit/remove, elements (text/image/shape) list/remove/move/duplicate/add; add text/rect/line; hit-test |
| **Text extraction** | Font-aware, zero-tofu via WinAnsi + `/ToUnicode` CMap (CID/Type0) |
| **Annotations** | Highlight, underline, strike-out, free-text, square, line, ink, stamp, link; **flatten** |
| **Forms (AcroForm)** | Text/checkbox/radio/combo/list/signature fields — fill, create, flatten |
| **Pages** | Rotate, delete, move, extract, merge; bookmarks/outline; metadata |
| **Security** | Encrypt/permissions, **self-signed digital signature** (RSA/X.509/CMS), **true redaction** (delete from stream, no opaque cover) |
| **Render** | Rasterize a page to PNG (vector + TrueType/CFF glyphs + images) |
| **Text intelligence** | Font-aware extraction, **structured text** (reading-order lines + boxes), **full-text search** with highlight boxes |
| **OCR** | Built-in recognizer — Otsu → connected components → line/word segmentation → MLP trained on **EMNIST handwriting + synthetic font glyphs** (Latin + accents). No Tesseract, no model download at runtime |
| **Convert →** | PDF → **TXT, HTML, DOCX, PPTX, ODT, XLSX, ODS, RTF** (real editable elements, not a page image) |
| **Convert ←** | **TXT, HTML, RTF, DOCX, ODT, PPTX, XLSX, ODS** → PDF |
| **Archival** | **PDF/A-2b** metadata (XMP + sRGB OutputIntent + ID) |
| **Fonts** | **1951-family catalog**, Google-Fonts URL builder, **TrueType embedding** (Type0/CIDFontType2 + ToUnicode), needed-font detection |

All of it is exercised by `cargo test` (**141 tests**), a Node WASM smoke test
(end-to-end, all green), and **validated externally**: generated Office files open
and round-trip in LibreOffice; embedded fonts verify as `emb=yes` under poppler's
`pdffonts`.

## Honest scope

Conversions are **content-and-layout faithful**, not pixel-perfect re-typesetting.
PDF→Office reconstructs **real, editable objects** (positioned text boxes,
re-embedded images, table cells) the way an office suite's PDF import does — not a
rendered page image. Office→PDF is **text-faithful** (all content, reading order,
pagination) using the standard-14 fonts; pixel-perfect re-layout of an arbitrary,
richly-styled document stays the job of a full layout engine. Full PDF/A
conformance additionally requires every font embedded (the engine can do that).

## OCR & text intelligence

Text already in a PDF is extracted **font-aware** (zero tofu) with reading-order
lines and bounding boxes, and is searchable with highlight boxes. For **scanned,
image-only pages** the engine has a built-in OCR following the classic Tesseract
pipeline — Otsu binarization → connected-component blobs → line/word segmentation
→ per-glyph classification — but with a from-scratch, dependency-free classifier:

- The classifier is a small MLP **trained offline** on two public sources:
  **EMNIST** (NIST handwritten digits + letters, public domain) for **handwriting**,
  and **synthetic glyphs rendered from ~220 system fonts** (the Tesseract
  `text2image` approach) for **printed text, punctuation and accented Latin**.
- Training is build-time only (`tools/train_ocr.py`); the engine ships the
  **int8-quantized weights** and runs a pure-`std` forward pass — no ML library,
  no model download at runtime.
- **Scripts/languages:** Latin — `0-9 A-Z a-z`, common punctuation, and accented
  Latin (`é è à ç ñ ü …`) for French, Spanish, German, Portuguese, etc. Both
  **printed and handwritten** Latin are recognized. Other scripts (Cyrillic,
  Greek, CJK, Arabic) are not covered yet — they're a matter of adding classes +
  data to the trainer, with **no runtime change**.
- **Honest accuracy:** strong on clean machine print, decent on tidy handwriting
  (EMNIST-grade); noisy scans and dense layouts are harder. Retrain with more data
  to improve — the runtime never changes.

## Layout

```
crates/core   gigapdf-core  — the whole engine (parse, inflate, edit, render, crypto, convert)
crates/wasm   gigapdf-wasm  — extern "C" WebAssembly bindings (zero-dep ABI)
fixtures/     test PDFs
test/         wasm-smoke.mjs — end-to-end Node harness
tools/        catalog/ICC generators + snapshots
docs/         API.md · USAGE.md · INSTALL.md
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

See [`docs/USAGE.md`](docs/USAGE.md) for the full buffer ABI and an example for
every feature, and [`docs/API.md`](docs/API.md) for the complete reference.

## Build

```bash
cargo test -p gigapdf-core                                    # native tests (real fixtures)
cargo build -p gigapdf-wasm --target wasm32-unknown-unknown --release
node test/wasm-smoke.mjs                                       # end-to-end WASM smoke test
```

The release `.wasm` is ~540 KB — **zero dependencies**, versus ~14 MB for MuPDF.

## License & provenance

PolyForm Noncommercial 1.0.0. Built clean-room from the ISO 32000 specification;
**no AGPL code (e.g. MuPDF) was ever read or copied.** See [`LICENSE`](LICENSE).
