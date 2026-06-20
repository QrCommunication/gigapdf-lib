# Changelog

All notable changes to `@qrcommunication/gigapdf-lib` are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [0.54.0] - 2026-06-20

### Added

- **OCR front-end restoration (no API change — automatic in `ocr`).** Before recognition the
  engine now (1) **auto-crops a photographed page** — detects the document's four corners on a
  contrasting background and perspective-warps it head-on (8×8 DLT homography + bilinear warp,
  pure `std`), and (2) **flattens uneven illumination** (flat-field divide by a local background:
  shadows/glare → uniform page). Both are **gated to no-op on already-clean scans**, so they only
  help phone photos / creased paper. Rescues real-world captures with zero caller changes.
- **Chinese OCR — new `cjk` script.** `loadBundledOcrModel("cjk")` / `ALL_OCR_SCRIPTS` now load
  `ocr_cjk.gpocr` (data-driven **2401-class** charset, 32/64/128 backbone) — **CER 0.206 on CASIA
  handwritten Chinese**, the first CJK model shipped.
- **Japanese & Korean scripts declared** (`"japanese"`, `"korean"` → `ocr_jpn.gpocr` /
  `ocr_kor.gpocr`). Their charsets include kana+kanji / Hangul **plus full ASCII** (mixed
  alphanumerics). Models train upstream and land in a follow-up release — `loadBundledOcrModel`
  now **returns `false` for an absent blob instead of throwing**, so `ALL_OCR_SCRIPTS` stays safe.
- **Handwriting & degraded variants** bundled: `ocr_alpha_hw.gpocr` (real-cursive, **beats
  Tesseract on IAM — CER 0.309 vs 0.353**) and `ocr_alpha_photo.gpocr` (degradation-augmented,
  beats the plain HW model on degraded input). Host-load via `loadOcrModel`.

### Changed

- **Non-Latin models rebuilt at the 32/64/128 backbone** — Devanagari, Bengali, Tamil and
  Arabic validation CER roughly **halved** (deva 0.039, beng 0.042, taml 0.011, arabic 0.030);
  the bundled `.gpocr` blobs are updated. Capacity, not data, was the bound.
- **Faster real-dataset training downloads** (dev tooling): `hw_datasets.py` fetches line images
  **concurrently** (`GIGA_OCR_DL_WORKERS`), ~16× quicker — pairs with an HF Pro token.

## [0.52.5] - 2026-06-19

### Added

- **Arrow line annotations — `addLineAnnotation(page, x1, y1, x2, y2, rgb, lineWidth, endArrow?)`.**
  The new `endArrow` flag draws an open arrowhead at the `(x2,y2)` end and records
  `/LE [/None /OpenArrow]` on the `/Line` annotation, so the arrowhead survives in
  any conforming reader (Adobe Reader, Preview, Chrome) — and stays editable, not
  baked. The `/Rect` is padded around the arrowhead so it is never clipped. Ideal
  for callouts that point at content. Backward compatible: `endArrow` defaults to `false`.

## [0.52.4] - 2026-06-19

### Added

- **True PII redaction — `redactPii(page, rects, opts?)`.** Physically removes
  the text operators in each rect, **overwrites the pixels of any image** that
  intersects the rect (so a scanned / OCR'd page is genuinely sanitised, not
  just covered), strips overlapping annotations + form-field values, and paints
  an opaque black mark (the PII default). Not recoverable by copy-paste, text
  extraction, or pulling the image back out — closing the gap where `redact()`
  left images intact. ABI `gp_redact_pii`; `rects` are `{ x, y, width, height }`
  in PDF user space, `opts: { cover?, coverRgb? }`.
- **Documentation — new `docs/COOKBOOK.md`** (task-oriented recipes) plus a full
  refresh of the README, SDK, API and usage docs covering the recent additions
  (text decorations, the running header/footer reader, the unified editable
  model and its `modelTo*` exporters, AVIF).

### Changed

- **OCR recognition models refreshed** — larger-backbone models and expanded
  training data for better accuracy.

## [0.52.3] - 2026-06-19

### Added

- **Bake underline & strikethrough into drawn text.** `addText` /
  `addStandardText` now accept a trailing `opts: { underline?, strikethrough? }`
  (backed by new `add_text_styled` / `gp_add_text_styled`); the rule is painted
  as a filled rectangle that follows the text rotation, its length taken from the
  run's real glyph advances. Existing calls stay byte-identical (flags off).
- **AVIF (AV1 intra) decode — loop restoration (§7.17 Wiener + SGR) and
  directional intra-edge filtering/upsampling.** Higher-fidelity AVIF decoding;
  the post-deblock / pre-CDEF stripe halo is used for restoration so stripe and
  frame edges are reconstructed correctly.

## [0.52.2] - 2026-06-19

### Added

- **Read baked running headers/footers.** `GigaPdfDoc.headerFooter()` returns
  `{ header, footer }` recovered from the `/GPHF` marked-content spans that
  `setHeader`/`setFooter` write, so a host can detect whether a PDF already
  carries a running header/footer (and recover its text) and reflect that in its
  UI — the read complement to the existing writer.

## [0.52.1] - 2026-06-18

### Fixed

- **JPEG encoder — final-byte padding no longer corrupts the last code.** The
  entropy writer's `flush` padded the trailing partial byte with a fixed 7-bit
  `0x7F`; for any partial byte holding more than one written bit, the extra
  1-bits bled into the already-written Huffman code (ITU-T T.81 §F.1.2.3
  requires padding *only* the free low bits with 1s). The lib's own decoder
  tolerated it, but strict third-party decoders could misread the last code or
  reject the non-conformant padding. `flush` now pads exactly the free bits.

## [0.52.0] - 2026-06-18

### Added

- **Unified editable document model — reconstruction, importers, exporters, edit
  operations, full JS round-trip.** A format-agnostic `model::Document`
  (Section → Page → Block{Paragraph, Heading, List, Table, Image, Shape, TextBox,
  Sheet, Slide} → Inline) that every format imports into and exports from. PDF →
  model via `reconstruct_model` (structural: positioned runs are rebuilt into
  paragraphs, headings, lists and tables, honouring the `/StructTree` tag tree when
  present); Office/HTML/image → model importers; model →
  DOCX/XLSX/PPTX/ODT/ODS/ODP/HTML/RTF/PDF structured exporters (real editable
  content, not a raster). New `model::edit` operations (`ModelOp`: set/restyle run,
  insert/delete/move block, table & sheet cells…) with `apply_ops`. Exposed to JS:
  `toModel`, `officeToModel`, `htmlToModel`, `applyModelOps`, and
  `modelToDocx/Xlsx/Pptx/Odt/Ods/Odp/Html/Rtf/Pdf`. Foundation for editing any
  document format through one editable model.
- **Text direction & document-language detection (RTL).** `documentLanguage()`
  reports the dominant script and reading direction (Arabic, Hebrew, Latin, CJK…),
  and each text element now carries its `direction` (`ltr`/`rtl`/`neutral`), so
  editors can switch the canvas and layer properties to right-to-left for correct
  editing.
- **Page margins + running headers/footers.** `pageMargins`/`setPageMargins`
  (CropBox-aware, falling back to the printable content box) and `setHeader`/
  `setFooter` with `{{page}}`/`{{pages}}` tokens, alignment, page ranges and a
  first-page toggle. Baking is idempotent (wrapped in marked content) and
  reversible via `removeHeaders`/`removeFooters`.
- **Pixel-perfect colour & images.** Full PDF colour-space resolution — Separation,
  DeviceN (with type-0/2/3 functions and a new PostScript type-4 calculator tint
  transform), Indexed, ICCBased (via `/N` + `/Alternate`), CalRGB/CalGray, Lab
  (D50) and accurate CMYK — applied to fills, strokes (`cs`/`CS` + `sc`/`scn`) and
  image XObjects (honouring `/BitsPerComponent` and `/Decode`), fixing
  blank/garbled non-RGB images. **Progressive (SOF2) JPEG** decoding lands in full
  (baseline already supported; arithmetic-coded JPEG is skipped, not blanked).

### Fixed

- **Subset-CFF glyphs no longer render as tofu.** Simple `/Type1` fonts embedding a
  CFF program (`/FontFile3 /Type1C`, e.g. subsetted MyriadPro/Nexa) now resolve
  glyphs through the CFF charset (`code → glyph name → gid`) instead of a Unicode
  `cmap` they do not carry; an unresolved code paints nothing rather than a box.
- **Baseline JPEG images now render.** `/DCTDecode` image XObjects (direct and
  nested inside form XObjects, with `/SMask`) are decoded and blitted — previously
  they were skipped, leaving blank slides and image-shaped holes.

## [0.51.0] - 2026-06-18

### Added

- **Rasterizer fidelity — form XObjects, clipping, shadings, soft masks, blend modes.**
  `renderPage` now paints page-content form XObjects (`Do`, cycle-guarded, clipped to
  `/BBox`), honours path clipping (`W`/`W*`), renders axial (type 2) and radial (type 3)
  **shadings** (the `sh` operator and shading-`/Pattern` fills) with `/Function` ramps and
  `/Extend`, stamps tiling patterns, and applies ExtGState separable **blend modes**
  (`/BM`), constant alpha (`/ca`) and luminosity **soft masks** (`/SMask`). Previously
  these were ignored (clips bled, gradients/patterns/transparency were missing).
- **OpenType shaping — GPOS kerning + GSUB ligatures.** Text measurement and layout now
  apply GPOS pair kerning and GSUB ligature/substitution; the embedded font subset keeps
  its `cmap` so text extraction stays correct.
- **Full-Unicode ToUnicode.** Type0/CFF fonts and supplementary-plane glyphs get a
  ToUnicode mapping derived from the font `cmap` — no more `U+FFFD` for composite fonts
  lacking `/ToUnicode`, and CFF ligature glyph names resolve.
- **Unified editable document model (foundation).** A new zero-dependency `model` module
  (`Document → Section → Page → Block → Inline`, plus spreadsheet/slide sub-models, named
  styles, page geometry) with a versioned JSON round-trip — the base for format-agnostic
  re-editability.

### Changed

- **HTML/CSS layout fidelity.** `position` (relative/absolute/fixed) + `z-index` +
  `overflow` clipping, real `float` text wrapping, flex `align-*`/`flex-wrap`/`order` and
  vertical `justify-content`, CSS grid rows + `gap`, table `rowspan`, plus `@font-face`,
  `letter-spacing`/`word-spacing`, `calc()`/`var()` and `rem`/`vw`/`vh` units.
- **Office import fidelity.** DOCX named-style inheritance (`styles.xml`/`docDefaults`) +
  headers/footers + `PAGE`/`NUMPAGES` fields; PPTX tables (`a:tbl`) render with column
  widths and theme fonts; real ordered-list numbering; XLSX theme/indexed cell colours,
  number formats (date serials and currency render formatted, not raw), and merged cells
  (`colspan`/`rowspan`).

## [0.50.0] - 2026-06-18

### Added

- **Bundled fallback font (offline rendering).** HTML→PDF and Office→PDF now embed
  a permissively-licensed fallback font (Liberation Sans, SIL OFL 1.1) when the
  host provides no matching font, so text renders with real, selectable glyphs and
  correct advance widths with **zero network** — instead of rough average-width
  estimates. Host-provided / Google fonts still take precedence; `needed_fonts`
  is unchanged.
- **Annotation appearances in the rasterizer.** `renderPage` now composites each
  annotation's normal appearance stream (`/AP /N`, selected by `/AS`) onto the
  page, mapping the appearance `/BBox`·`/Matrix` onto the annotation `/Rect`
  (ISO 32000-1 §12.5.5) and honouring `/CA` opacity plus the Hidden/NoView flags.
  Previously annotation appearances were not drawn.
- **Floating shapes in XLSX / ODS export.** Page vector shapes are now carried
  into a real drawing layer (XLSX `xl/drawings` `xdr:absoluteAnchor` + DrawingML
  geometry/fill/stroke/dash; ODS `draw:` shapes), matching the DOCX/PPTX/ODT/ODP
  exporters. Shape-less output stays byte-identical.

### Changed

- **Table column widths honoured.** The native HTML layout engine reads per-column
  widths (`<colgroup>`/`<col>` or first-row cell widths; pt/px/%) and positions
  cells proportionally, with `colspan` summing the spanned widths, instead of
  forcing equal columns. The DOCX (`w:tblGrid`/`w:gridCol`) and ODF
  (`table:table-column` / `style:column-width`) importers emit those widths.
  Tables without declared widths keep equal columns (no regression).

## [0.49.0] - 2026-06-18

### Changed

- **Office → PDF higher fidelity.** DOCX paragraph line spacing
  (`w:spacing@line/@lineRule` → CSS `line-height`), bullet/numbered lists
  (`w:numPr`, ODF `text:list` — indentation + bullet), and table cell merges
  (`w:gridSpan` expands to physical cells so the merge actually spans columns;
  `w:vMerge`) are now carried through, and XLSX cell fills (`xl/styles.xml`
  `cellXfs`→`patternFill`/`fgColor`) become `background-color`. Embedded images
  (DOCX/PPTX `a:blip`, ODF `draw:image`) render as inline `data:` URIs.
- **PDF → Office higher fidelity.** Vector strokes keep their **exact dash
  pattern** (DrawingML `a:custDash`, ODF `draw:stroke-dash`) instead of a generic
  preset, and shapes/text in **ICC-based / `cs`-`scn` colour spaces** now resolve
  to their real colour (DeviceRGB/Gray/CMYK and ICCBased by component count)
  instead of defaulting to black.

### Known limitations

- DOCX per-column table widths (`w:tblGrid`) are not yet honoured — the HTML
  table layout uses equal-width columns (a future layout-engine change).
- Floating shapes in XLSX/ODS spreadsheets are not yet exported.

## [0.48.0] - 2026-06-18

### Fixed

- **`Z_SYNC_FLUSH` deflate streams without a final block now decode.** Content
  streams flushed with `Z_SYNC_FLUSH` end with an empty stored block
  (`00 00 ff ff`, `BFINAL=0`) and no final block — common in signed PDFs (the
  `q`/`Q`/overlay content pieces of Adobe FillSign) and any deflate produced via
  a flush. The decoder looped past the flush expecting another block, hit
  end-of-input and errored, so affected pages extracted **nothing**. It now
  returns the bytes decoded so far when the input is exhausted at a block
  boundary (matching pdfjs/Acrobat leniency); mid-block truncation still errors,
  so genuinely corrupt data is not masked.

### Changed

- **Office → PDF: real page geometry + font names.** Conversions no longer
  hard-code US-Letter/0.5in. The page size and margins are read from the source
  document (DOCX `w:sectPr/w:pgSz`+`w:pgMar`, PPTX `p:sldSz`, ODF
  `style:page-layout-properties`) with sensible per-format fallbacks, and each
  run's real `font-family` (DOCX `w:rFonts`, PPTX `a:latin`, ODF `fo:font-name`)
  is emitted so the host font-resolution path embeds the correct faces with true
  metrics instead of a 0.5-em estimate. DOCX paragraph alignment, spacing and
  indentation (`w:jc`/`w:spacing`/`w:ind`) are carried through.
- **PDF → Office: vector shapes keep their geometry and colours.** The Office
  exporters (DOCX/PPTX DrawingML, ODT/ODP ODF) now emit real shapes — rectangles
  and `custGeom`/`draw:path` curves — sourced from `page_vector_paths`, with
  fill/stroke RGB, opacity, stroke width and dash, instead of a single grey
  bounding-box rectangle. Clip-only paths no longer leak stray rectangles.

## [0.47.0] - 2026-06-18

### Fixed

- **`'` and `"` (next-line-show) text operators are now extracted and
  positioned correctly.** The content interpreter behind `textElements`,
  `textRuns`, full-text search and the PDF→Office converters treated `Tj`/`TJ`
  as the only text-showing operators, so runs drawn with `'` (move to next line
  then show) or `"` (set spacing + next-line show) were **dropped entirely** and
  the implicit line move they perform was **skipped** — shifting every
  subsequent run in the same `BT…ET` block up by the accumulated leading.
  Real-world impact: invoice/letter bodies (generated with leading-based line
  breaks) lost lines and mis-placed the rest (e.g. a subtitle landing on the
  title's baseline), breaking click-to-edit hit targets and conversion fidelity.
  `'`/`"` now apply their implicit `T*` before showing and count as text runs,
  so extraction, the run index used by `replaceText`/`removeElement`/`moveElement`,
  and font resolution all stay consistent. Rendering (`renderPage`) was already
  correct. No API change. `flattenFormXObjects(page)`
  inlines every form XObject invoked via `Do` into the page content stream
  (applying its `/Matrix`, merging its resources with collision-safe name
  remapping, recursing into nested forms with a depth + cycle guard). Each
  placement is de-shared, so the former form text becomes ordinary page runs with
  real indices that `replaceText` / `moveElement` / `removeElement` can edit —
  letting hosts edit invoice/template text in place rather than redact-and-redraw.
  Distinct from `flattenForm` (AcroForm fields).

## [0.45.0] - 2026-06-18

### Added

- **Rich Office → PDF conversion.** `officeToPdf` now maps DOCX/XLSX/PPTX/
  ODT/ODS/ODP (and legacy OLE2 `.doc`/`.xls`/`.ppt`) to styled HTML — headings,
  bold/italic/size/colour runs, tables, lists and embedded images — and renders
  it through the native HTML→PDF engine instead of the old text-only flatten. No
  LibreOffice/soffice dependency.
- **More OCR languages.** New host-loaded `.gpocr` line models beyond
  Latin/Cyrillic/Greek: Arabic + Hebrew (RTL), Devanagari, Bengali and Tamil,
  plus a larger 24/48/96 backbone retrain (clean-print CER now well past
  Tesseract) and a handwriting-augmented `alpha` variant. Auto-discovered by
  `loadAllBundledOcrModels`; the wasm still ships no weights.

### Fixed

- **Text extraction recurses into form XObjects.** Text drawn via reusable form
  XObjects (the `Do` operator — common in invoice/template PDFs) was rasterised
  but never extracted, so it showed in the page image yet could not be selected
  or edited. The extractor now walks `Do` into `/Subtype /Form` XObjects
  (composing the form `/Matrix` with the CTM, with depth and cycle guards), so
  form text is recovered in page space.
- **Font-less HTML render no longer drops text.** The HTML renderer skipped
  every text run when no embedded font matched, so a render with no
  host-provided fonts produced a blank page; a base-14 standard-font fallback
  now always paints text.

## [0.44.0] - 2026-06-18

### Added

- **Raw Type1 (PFB/PFA) font embedding.** Classic encrypted Type1 programs
  (PDF `FontFile`, PFB, PFA) are parsed (eexec + charstring decryption),
  transcoded to Type2 and embedded through the bare-CFF → OpenType path — the
  last font format that required an external converter (FontForge).
- **Bundled per-script OCR models + multi-language recognition.** The `.gpocr`
  CRNN models ship under `models/`; `GigaPdfEngine.loadAllBundledOcrModels()`
  (plus `loadBundledOcrModel` / `loadBundledOcrModels` and the `OcrScript` type)
  load them so `doc.ocr` recognizes non-Latin scripts — Cyrillic, Greek, Arabic,
  Urdu, Hebrew (the RTL group), Devanagari, Bengali, Tamil — routed per line by
  the engine's script detector. The wasm still ships no weights.

### Fixed

- **Glyph counters are now hollow in `renderPage`.** Each glyph contour was
  filled separately, painting counters (the holes in O, e, a, 0, 8, B…) solid.
  Every contour of a glyph is now accumulated and filled once with the non-zero
  winding rule, so inner contours carve out correctly — fixing blobby,
  low-quality text in the rasterized page (editor background, OCR input).

## [0.43.0] - 2026-06-18

### Added

- **Native bare-CFF font embedding.** PDF `FontFile3 /Subtype /Type1C` programs
  (the common compact-font case) are embedded by wrapping the bare CFF into an
  OpenType-CFF (`OTTO`) sfnt with a synthesised `cmap`, so the engine no longer
  needs an external converter (e.g. FontForge) for Type1C faces. The cmap maps
  CFF Standard Strings 1–95 to ASCII and 96–228 to Latin-1 (covering French and
  Western-European accents), plus `uniXXXX` / single-character glyph names.
- **Glyphless Type0 OCR text layer for any script.** `add_text_layer` now
  carries non-WinAnsi text (Cyrillic, Greek, Arabic, Bengali, Devanagari,
  Tamil…) through an embedded glyphless Type0 font (`CIDFontType2`, empty
  `glyf`, Identity `CIDToGIDMap`, `Identity-H`) with a `/ToUnicode` CMap.
  Drawn in render mode 3 (invisible), the layer makes OCR output searchable and
  copyable regardless of script. WinAnsi runs keep the compact Helvetica path.
- **SDK `loadOcrModel(model)` / `clearOcrModels()`.** Hosts can load CRNN OCR
  model blobs at runtime (the wasm ships none), enabling multi-script
  recognition (Latin, Cyrillic, Greek, Arabic, Devanagari, Bengali, Tamil).

### Changed

- **SDK `load()` / `loadDefault()` are bundler-opaque.** The Node-only built-in
  imports in `loadDefault()` are indirected so bundlers (Turbopack, webpack,
  Vite) no longer try to statically resolve `node:fs/promises` / `node:url` in
  browser builds.

## [0.42.0] - 2026-06-18

### Changed

- **Cryptography now uses audited RustCrypto crates** (`rsa`, `sha2`, `aes`,
  `cbc`, `des`, `rc2`, `hmac`, `x509-cert`, `cms`) instead of hand-rolled
  primitives — for PDF signing (RSA/X.509/CMS) and the standard security handler
  (AES/RC4/3DES/RC2). The public ABI and SDK are unchanged.
- **The HTML→PDF inline-`<script>` engine is now Boa** (`boa_engine`), replacing
  the hand-written interpreter. `js::run_inline_scripts` / `js::eval` keep their
  signatures; Boa is a full ES2021+ engine, so generators, `async`/`await`,
  `RegExp`, `Map`/`Set` and the rest behave per spec. ~14k lines of the former
  engine were removed.
- The wasm engine stays **`wasm-bindgen`-free**. Entropy (RSA blinding, Boa
  `Math.random`) is drawn through a single host import, **`env.gp_host_random`**,
  which the SDK's `load()` fills from `crypto.getRandomValues`. **Hosts that
  instantiate the `.wasm` directly must now provide this import** (the import
  object was previously empty). Dropping the `wasm_js` getrandom backend also
  removed the transitive `wasm-bindgen`/`js-sys` dependency tree.

> Rationale: rolling your own cryptography (timing/oracle side-channels, eIDAS
> non-conformance) and maintaining a JS engine are liabilities better delegated
> to audited, wasm-compatible crates. The "no third-party PDF/Office/image
> library" invariant is unchanged — see `THIRD-PARTY-LICENSES.md`.

## [0.41.0] - 2026-06-17

### Fixed

- **AVIF: mixed 2D transforms (ADST/DCT) used the wrong row vs column axis.**
  The inverse transform applied the *vertical* 1D type to the rows and the
  *horizontal* one to the columns, so `ADST_DCT` (and every other mixed type)
  ran its ADST across the rows instead of down the columns. The symmetric types
  (DCT_DCT, ADST_ADST, FLIPADST_FLIPADST, IDTX) are swap-invariant, which hid
  the bug until a still used a mixed type — the common case for real-world
  AVIFs, whose intra residual leans heavily on ADST. The reconstruction is now
  bit-exact vs dav1d. Versions before this corrupted most photographic AVIFs.

### Changed

- **AVIF: multi-strength CDEF (`cdef_bits > 0`).** The CDEF stage now reads the
  per-64×64 `cdef_idx` from the tile stream (once per filter unit, after the
  skip flag) and selects the matching primary/secondary strength pair per plane,
  rather than assuming a single strength set. Bit-exact against dav1d on
  single-strength fixtures; the multi-strength read position is verified
  sync-correct (with-read vs no-read divergence proof).
- **AVIF: directional intra real-neighbour edges.** Directional predictors
  (Z1/Z2/Z3) gather the true top-right / bottom-left samples via a
  `BlockDecoded` availability grid instead of repeating the last edge sample.
  Together with the transform fix above, the whole AVIF intra path — mixed
  transforms, Z1/Z2/Z3 directional, palette, CDEF and deblocking in a single
  frame — is now validated bit-exact vs dav1d on a 64×64 noise still.

## [0.40.0] - 2026-06-17

### Changed

- **AVIF: full (non-reduced) sequence + frame header.** `decodeAvif` now decodes
  AVIFs whose AV1 sequence header is *not* `reduced_still_picture_header` — e.g.
  produced by ffmpeg/libaom without `-still-picture`, and various other encoders.
  Previously these failed to parse (the header path returned nothing). The
  streaming sequence header (timing/decoder-model info, operating-points loop,
  frame-id numbers, order-hint feature flags) and the KEY-frame frame-header
  preamble — including the `disable_frame_end_update_cdf` bit the reduced header
  omits — are parsed bit-exact against dav1d. Only shown KEY/intra stills are
  decoded; references to previously decoded frames and inter frames are rejected.

## [0.39.0] - 2026-06-17

### Changed

- **AVIF: palette mode (AV1 §5.11.46-50).** The AV1 intra decoder behind
  `decodeAvif` now decodes screen-content palette blocks (logos, UI, charts,
  flat-colour graphics), validated bit-exact against dav1d. Per palette block:
  the colour table (neighbour-palette prediction cache with merge/dedup, reuse
  flags, then delta-coded new entries; U plus delta/literal V for chroma), the
  per-pixel index map (anti-diagonal wave-front scan with the colour-order
  context model), and reconstruction from `palette[index]` — with the transform
  residual added on top for non-skipped blocks. Previously such AVIFs aborted on
  an unsupported-mode guard.

## [0.38.0] - 2026-06-17

### Changed

- **AVIF: CDEF in-loop filter (AV1 §7.15).** The AV1 intra decoder behind
  `decodeAvif` now applies the Constrained Directional Enhancement Filter after
  deblocking, so AVIFs encoded with CDEF (the common case) decode without ringing
  / directional artifacts. Per coded 8×8 luma block: an eight-way direction
  search, a variance-adjusted primary tap set along that direction plus secondary
  taps at ±45°, and the matching 4:2:0 chroma pass — each soft-thresholded by the
  signalled strength + damping. Validated **bit-exact** against dav1d on luma and
  chroma stills. Current scope: the single-strength (`cdef_bits == 0`) case;
  per-block strength indices and loop restoration remain pending.

## [0.37.0] - 2026-06-17

### Changed

- **AVIF: in-loop deblocking filter (AV1 §7.14).** The AV1 intra decoder behind
  `decodeAvif` now applies the deblocking loop filter after reconstruction, so
  AVIFs encoded with the loop filter enabled (the common case) decode without
  blocking artifacts at tx-block boundaries. A per-4×4 grid records each
  transform block's size and edge flags per plane; the apply pass runs the
  vertical then horizontal edge filters (4/6/8/14-tap), deriving thresholds and
  per-block levels exactly as the spec prescribes. Validated **bit-exact**
  against dav1d on a still with the loop filter on and CDEF + loop restoration
  off. CDEF and loop restoration remain pending.

## [0.36.0] - 2026-06-17

### Added

- **Still AVIF decoding (`decodeAvif`).** `decodeAvif` / `gp_decode_avif` decode
  a still AVIF image with a complete, from-scratch **AV1 intra decoder** — no
  third-party codec. Pipeline: ISOBMFF/OBU parse → sequence + frame headers →
  multi-symbol arithmetic (msac) entropy decode → coefficient decode, dequant and
  inverse transforms (DCT 4–64, ADST/FlipADST 4–16, identity, Walsh–Hadamard) →
  intra prediction (DC, V, H, Paeth, the Smooth family, filter-intra, CfL, and the
  Z1/Z2/Z3 directional predictors) → YUV→RGBA (BT.601/709/2020-NCL/Identity,
  limited or full range, 4:2:0/4:2:2/4:4:4 chroma upsample). Every transform and
  predictor is validated **bit-exact** against dav1d reference vectors.
  - Current scope: still images, 8-bit. In-loop filters (deblocking, CDEF,
    loop-restoration) and film-grain synthesis are not yet applied.

## [0.35.0] - 2026-06-17

### Added

- **Lossy WebP decoding (`decodeWebp`).** `decodeWebp` / `gp_decode_webp` now
  decodes lossy WebP (a `VP8 ` keyframe) in addition to the existing lossless
  (`VP8L`) path — a complete, from-scratch VP8 intra decoder (RFC 6386):
  boolean entropy decoder, coefficient token decode, dequantization, inverse
  WHT/DCT, all intra prediction modes (16×16 / 8×8 / the ten 4×4 sub-block
  modes), the deblocking loop filter (§15), and YUV→RGB. Validated **bit-exact**
  against libvpx's reference decode. No third-party codec.

### Added

- **External-resource host port for the HTML→PDF engine.** The native renderer
  is zero-network, so external `<img src>` images (not just `data:` URIs) are now
  fetched by the **host** in the same two-phase pattern as fonts:
  - `htmlNeededResources(html, header?, footer?)` / `gp_html_needed_resources` —
    one unified phase-1 list of everything the document needs: fonts
    (`{kind:"font",family,weight,italic,url}`) **and** external images
    (`{kind:"image",url}`). `data:` images are inlined and never listed.
  - `HtmlRenderOptions.resources` (`{ url, bytes }[]`) on `htmlRenderWith` /
    `RenderOptions.resources` on `render_with` / the `gp_html_render_opts`
    `resources` blob — the host hands the fetched image bytes back, keyed by the
    exact URL referenced in the HTML.

  This lets a host render documents with remote images while keeping the engine
  fully offline — the native replacement for a headless browser's autonomous
  resource loading, with every fetched URL auditable up-front (SSRF-friendly).

## [0.33.0] - 2026-06-17

### Added

- **`PageInfo` now carries the raw `/MediaBox`.** `pageInfo(page)` /
  `gp_page_info_json` gain a `mediaBox` field (`[x0, y0, x1, y1]` in user-space
  points), and `Document::page_media_box` exposes it natively. Unlike the
  derived `width`/`height` (the box size), this preserves the box **origin**, so
  a host can reconstruct a page's exact coordinate frame — the native
  replacement for a reader's `page.view` / MediaBox read.

## [0.32.0] - 2026-06-17

### Added

- **Image elements now carry rotation + opacity.** `imageElements()` /
  `Document::page_image_elements` enrich each `ImageElementInfo` with `rotation`
  (degrees, from the placement CTM) and `opacity` (the active `/ExtGState`
  `/ca`, `1` = opaque). The content walker now tracks fill alpha through `q`/`Q`
  and `gs`, so a host editor recreates an image's tilt and translucency without
  walking the operator list itself.
- **Rich annotations.** `annotations()` / `Document::page_annotations` now return
  the full markup metadata on each `AnnotationInfo`: `author` (`/T`), `subject`
  (`/Subj`), `created`/`modified` (`/CreationDate`/`/M`, raw PDF dates), `name`
  (stamp), `opacity` (`/CA`), `color` (`/C` normalised to RGB), `quadPoints`
  (`/QuadPoints` for text markup), `inkList` (`/InkList` freehand strokes), and
  the link target (`linkUri` / `linkPage`). The native replacement for a
  reader's annotation layer.
- **Vector path layer.** New `vectorPaths(page)` / `Document::page_vector_paths`
  return every painted path as geometry + style: `segments` (`M`/`L`/`C`/`Z` in
  user space), `bounds`, `fill`/`stroke` RGB (`null` when not painted),
  `strokeWidth`, `fillAlpha`/`strokeAlpha` and `dash`. Clip-only paths are
  omitted. Drives a host editor's shape layer without a rasteriser — the
  read-side counterpart of the SVG→PDF drawing helpers.

## [0.31.0] - 2026-06-17

### Added

- **Outline entries now carry style + destination detail.** `outline()` /
  `Document::outline_items` enrich each `OutlineItem`/`OutlineEntry` with `bold`
  + `italic` (`/F` flag bits), `color` (`/C` RGB), and the resolved destination
  fit: `destKind` (`xyz`/`fit`/`fith`/`fitv`/…) plus `x`/`y`/`zoom` for `/XYZ`.
  Destinations are resolved through explicit arrays, the `/Names`/inline `/Dests`
  name tree, and `/A /GoTo` actions. Lets a host rebuild a full bookmark tree
  (style + position/zoom) from the flat `level` list — the native replacement
  for a reader's `getOutline()`. The new fields are optional in `OutlineEntry`,
  so existing `setOutline` callers are unaffected.

## [0.30.0] - 2026-06-17

### Changed

- **Exact text widths.** Text-run bounding boxes (and the pen advance between
  runs) now measure by **real glyph advances** instead of a 0.5-em estimate:
  the content interpreter reads each font's `/Widths` (simple) or `/W`+`/DW`
  (Type0/CID) table, and base-14 Helvetica/Courier without `/Widths` fall back
  to built-in AFM/monospace metrics (`TextDecoder` gains a `CodeWidths` table;
  `TJ` kerning is applied). This makes `textElements`, `structuredText` and
  `search` bounding-box **widths** match a reference renderer — e.g. "Hello
  GigaPDF Test" at 24 pt now measures 213.4 pt (was the 216 pt estimate),
  matching pdfjs. Fonts whose metrics aren't embedded or built in (e.g. Times
  without `/Widths`) still fall back to the estimate. No API change.

## [0.29.0] - 2026-06-16

### Added

- **Image extraction — `imageElements(page)`** (ABI `gp_image_elements_json`,
  `Document::page_image_elements`). Returns each image placement as
  `{ index, x, y, width, height, format, pixelWidth, pixelHeight, data }` —
  bounds in user space (origin bottom-left), `data` the **embeddable encoded
  bytes**: DCTDecode/JPXDecode images pass through as `jpeg`/`jp2`, Flate/raw
  DeviceRGB|DeviceGray 8-bit images are re-encoded to `png` (honouring an 8-bit
  DeviceGray `/SMask` for alpha), anything else is reported `unknown` with empty
  bytes. The native replacement for a reader's image extraction (placement +
  bytes a host can display or re-embed, not just a render). New
  `ImageElementInfo` type. Closes the image-extraction gap versus pdfjs in the
  host's parse layer — both text (`textElements`, 0.28.0) and image gates now
  open.

## [0.28.0] - 2026-06-16

### Added

- **Rich per-run text extraction — `textElements(page)`** (ABI
  `gp_text_elements_json`, `Document::page_text_elements`). Returns every text
  run with everything a host editor needs to recreate it:
  `{ index, text, x, y, width, height, fontFamily, bold, italic, fontSize,
  color, rotation }` — bounds in user space (origin bottom-left), `fontFamily`
  resolved from `/BaseFont` (bold/italic parsed), `fontSize` the effective
  on-page point size, `color` the RGB fill (`0..1`), `rotation` the baseline
  angle. `index` is the **text-run index** accepted by `replaceText`, so a host
  can extract, render and edit from one model. The native replacement for a
  reader's per-run text layer (which `elements()` omitted font + colour). New
  `TextElementInfo` type.
- `ContentElement` now carries `font_size` and `rotation_deg` for text elements
  (computed from the text·CTM matrix), feeding the above. Validated against the
  app's pdfjs text extractor: 100% character-content parity across simple,
  mixed-font, embedded-font, CJK, RTL, table and rotated fixtures.

## [0.27.0] - 2026-06-16

### Changed

- **`namedDests()` now enumerates the `/Names /Dests` name tree** (PDF 1.2+),
  not just the legacy inline `/Dests` dictionary. Tree values that are dest
  arrays directly **or** wrapped in a `<< /D [dest] >>` dictionary both resolve.
  This brings the list to parity with a reader's `getDestinations()` — modern
  PDFs that register destinations through the name tree previously came back
  empty. Built on the `collect_name_tree` enumerator added in 0.26.0; no API or
  ABI change (`gp_named_dests_json` simply returns more entries).

## [0.26.0] - 2026-06-16

### Added

- **Embedded file attachments — `attachments()`** (ABI `gp_attachments_json`,
  `Document::attachments`). Walks the `/Names /EmbeddedFiles` name tree
  (ISO 32000-1 §7.11.4) and returns every extractable file as
  `{ name, filename, mime, description, creationDate, modDate, data }`, where
  `data` is the **decoded** bytes (stream filters applied) and the optional
  string fields are `null` when the PDF didn't record them. Filespec
  `/UF`/`/F` display names plus the embedded stream's `/Subtype` (MIME) and
  `/Params` dates are surfaced. The native replacement for a reader's
  `getAttachments()` — closes the last embedded-files gap versus pdfjs in the
  host's parse layer. New `Attachment` type.
- Internals supporting it: `Object::as_string()` accessor; a `collect_name_tree`
  enumerator (the all-entries counterpart of the existing name-tree search);
  `convert::base64` widened to `pub` so the WASM host receives decoded bytes as
  JSON; SDK `_fromBase64` (pure-JS Base64 decode, Node + browser).

## [0.25.0] - 2026-06-16

### Added

- **Native lossless WebP (VP8L) codec** — `encodeWebp(rgba, w, h)` and
  `decodeWebp(bytes)` (ABIs `gp_encode_webp` / `gp_decode_webp`;
  `raster::webp`). From-scratch RIFF/WebP container + VP8L bitstream: a
  full canonical-Huffman encoder (code-length-code RLE serialization) writing
  literal pixels, and a decoder for that stream (single Huffman group, optional
  colour cache). Exact lossless round-trip. Lossy VP8 and extended/animated WebP
  are out of scope (decode returns `null`). The native WebP path toward dropping
  a third-party image library.

## [0.24.0] - 2026-06-16

### Added

- **Native GIF decoder** — `decodeGif(bytes)` (ABI `gp_decode_gif`;
  `raster::gif::decode_gif`). Decodes the first frame (GIF87a/89a): global/local
  colour table, variable-width LZW, interlacing and a graphic-control
  transparency index → RGBA. Extends the native image-decode coverage
  (PNG/JPEG/GIF) so the host can convert GIFs without a third-party library.

## [0.23.0] - 2026-06-16

### Added

- **Native baseline JPEG codec + image decoders** — `encodeJpeg(rgba, w, h,
  quality?)`, `decodeJpeg(bytes)`, `decodePng(bytes)` (ABIs `gp_encode_jpeg` /
  `gp_decode_jpeg` / `gp_decode_png`; `raster::jpeg::{encode_jpeg, decode_jpeg}`).
  A from-scratch ISO 10918-1 baseline JPEG encoder **and** decoder (4:4:4,
  Annex-K quant/Huffman tables, orthonormal DCT-II/III, exact forward/inverse
  pair), validated by round-trip. With `rgbaToPng`/`resizeRgba` (v0.21/0.22) and
  the existing PNG decoder, the native raster toolkit now covers PNG⇄RGBA,
  JPEG⇄RGBA and resize — the host can re-encode/resize/convert rendered pages
  with **no third-party image library**. New `DecodedImage` type
  (`{ width, height, rgba }`).

## [0.22.0] - 2026-06-16

### Added

- **`resizeRgba(rgba, sw, sh, dw, dh)`** — native alpha-correct image resampling
  (ABI `gp_resize_rgba`; `raster::resize_rgba`). Separable triangle kernel whose
  support scales with the downscale factor (averages when shrinking, interpolates
  when enlarging); alpha is premultiplied during filtering so transparent/coloured
  edges don't fringe. Next piece of the native raster toolkit replacing `sharp`
  for resize/thumbnail work — no third-party image library.

## [0.21.0] - 2026-06-16

### Added

- **`rgbaToPng(rgba, width, height)`** — encode raw RGBA pixels to a PNG with the
  engine's native encoder (ABI `gp_rgba_to_png`; wraps `raster::encode_png`). No
  third-party image library. First piece of the native raster toolkit that lets
  hosts drop `canvas`/`sharp` for image work (more — resize, JPEG encode — to
  follow). Returns empty on a length mismatch (`≠ width*height*4`).

## [0.20.0] - 2026-06-16

### Added

- **Native `.xlsx` reader — `xlsxToGrids(bytes)`** (the inverse of
  `gridsToXlsx`/`toXlsx`). Reads a workbook back into per-sheet
  `{ name, rows: string[][] }` grids, in workbook order, decoding **inline
  strings** (this engine's output), **shared strings** (`sharedStrings.xml`, as
  Excel and most libraries emit) and plain numeric/`str` cells — pure std, no
  dependency. Rust `convert::office::xlsx_to_grids`; ABI `gp_xlsx_to_grids`
  (returns JSON `[{name, rows}]`). New `XlsxSheet` type.
  - Completes the spreadsheet round-trip and lets GigaPDF drop `exceljs`
    **entirely** (its xlsx tests now read back through `xlsxToGrids` instead of a
    third-party reader).

## [0.19.0] - 2026-06-16

### Added

- **Native spreadsheet writer for host-built grids** — `gridsToXlsx(grids,
  sheetNames?)` and `gridsToOds(grids, sheetNames?)` write a caller-provided
  table grid (`pages[rows][cells]`, `string[][][]`) to an `.xlsx`/`.ods`
  workbook, one sheet per page, with the engine's own zip/sheet writer. A host
  that already reconstructs tables (its own heuristic) can now emit Office output
  with **no third-party spreadsheet library**. `sheetNames` (index-aligned)
  overrides the default `Page <n>` titles (e.g. a single concatenated `Sheet1`).
  Rust: `convert::office::to_xlsx_named` / `to_ods_named` +
  `convert::grids::{from_json, strings_from_json}`; ABI `gp_grids_to_xlsx` /
  `gp_grids_to_ods` (grids JSON + optional names JSON).
  - This lets GigaPDF drop its runtime `exceljs` dependency: the app keeps its
    full table-reconstruction heuristic and every option, swapping only the
    workbook writer for `gridsToXlsx`.

## [0.18.0] - 2026-06-16

### Added

- **Text in *any* font — OpenType-CFF embedding.** `embedFont(family, font)`
  (Rust `embed_font`, alias `embed_truetype_font`) now accepts **any** outline
  program and auto-detects the flavour: a glyf `.ttf` embeds as Type0 /
  CIDFontType2 + `FontFile2` (as before), and an **OpenType-CFF** `.otf`
  (`OTTO`) embeds as Type0 / CIDFontType0 + `FontFile3` `/Subtype /OpenType`.
  Both are Identity-H with a full `/W` width array and a `/ToUnicode` CMap, so
  `addText` draws selectable, copy-able text in `.otf` fonts too.
- **Font-aware text editing.** `replaceText` (Rust `replace_text_run`) is now
  font-aware: a run set in an embedded Type0/Identity-H face (TrueType **or**
  OpenType-CFF) is re-encoded through that font's char→glyph map — so **modify**
  works with any font, not just base-14/WinAnsi. `addText` and `replaceText`
  also resolve a document's *own* embedded faces from `FontFile2` **or**
  `FontFile3`, completing "draw/edit text in the exact original face".
- **Named destinations.** `addNamedDest(name, page)`, `namedDests()` and
  `addGotoLinkNamed(page, x0,y0,x1,y1, name)` (Rust `add_named_dest` /
  `named_dests` / `add_goto_link_named`; ABI `gp_add_named_dest`,
  `gp_named_dests_json`, `gp_add_goto_link_named`) register and link to catalog
  `/Dests` by name. Resolution goes through the catalog, so a named anchor is
  retargetable and survives page split/extract while its page is kept. New
  `NamedDest` type.

## [0.17.0] - 2026-06-16

### Added

- **`doc.addStandardText(page, x, y, size, text, fontName, …)`** — draw real,
  selectable text in a built-in **base-14 standard font** (`Helvetica`/`Times`/
  `Courier` × 4 styles + `Symbol` + `ZapfDingbats`) with **no embedding**. Several
  different standard fonts can now coexist on one page. (`add_text` still covers
  arbitrary families via an embedded TrueType.)
- **`doc.embeddedFonts()`** — list the fonts a PDF already carries, each
  `{ baseFont, format: "truetype" | "cff" | "type1" }`. Paired with the existing
  `extractFont(name)`, you can pull a document's own font program out and
  re-embed it (`embedFont`) to draw new text in the exact original face — the
  complete "reuse the document's fonts" path, all native.

This rounds out native text drawing to **every font source**: the 14 standard
fonts (no files), any TrueType/Google Font (embed), and a document's own
embedded faces (extract + re-embed).

## [0.16.0] - 2026-06-16

### Added

- **Native PKCS#12 signing — `doc.signP12(p12, password, opts)`.** Sign a PDF
  with a user-supplied `.p12`/`.pfx` identity (a CA-issued / eIDAS certificate
  and its RSA key) producing an `adbe.pkcs7.detached` signature — with **no
  third-party crypto**. The whole pipeline is in the Rust core:
  - PKCS#12 import from scratch — DER reader, integrity-MAC verification
    (PKCS#12 KDF + HMAC-SHA1/256), and bag decryption for **PBES2** (PBKDF2 +
    AES-128/192/256-CBC) and **PBES1** (`3DES` and legacy 40-bit `RC2`), so both
    modern (OpenSSL 3 default) and legacy `.p12` files import;
  - the detached CMS `SignedData` is built over the document byte ranges with
    the imported key + certificate (issuer/serial taken verbatim from the X.509).
  - `opts` populates `/Name`, `/Reason`, `/M` (date), `/Location`, `/ContactInfo`.
  - A wrong password / malformed file / unsupported cipher throws one generic
    error (anti-enumeration — nothing about the credential leaks).
  - New crypto primitives, each pinned to FIPS/RFC/NIST known-answer vectors:
    SHA-1, HMAC-SHA1/256, PBKDF2, the PKCS#12 KDF, 3DES-CBC and RC2-CBC.
- **`doc.addTextLayer(page, runs)`** — stamp an invisible (render-mode 3) text
  layer over a page, e.g. a searchable OCR layer. One content append per page.

## [0.15.0] - 2026-06-16

### Changed

- **`extractPages` now produces self-contained chunks.** Page extraction (used
  by document *split*) prunes every reference that points at a page left behind,
  then garbage-collects the orphans:
  - cross-page GoTo **link** actions are neutralised — the annotation stays on
    its page but its `/A`/`/Dest` are stripped (no dangling ref);
  - **AcroForm fields** whose widgets all sit on dropped pages are removed, and
    for multi-widget fields only the on-dropped-page widget kids are dropped;
  - catalog named **`/Dests`** targeting dropped pages are omitted;
  - **outline** (bookmark) dests to dropped pages are cleared.

  A widget's page is located by `/Annots` membership (so widgets with no `/P`
  are still handled), and `/AcroForm`/`/Dests` are pruned whether stored inline
  in the catalog or as indirect references. Object ids are preserved, so
  in-chunk links, fields and bookmarks keep resolving natively.

## [0.14.1] - 2026-06-16

### Changed

- **Font subsetting now also drops unused tables and truncates the glyph space.**
  On save, an embedded font keeps only the tables a PDF Identity-H viewer needs
  (`head`/`hhea`/`maxp`/`hmtx`/`loca`/`glyf`) — dropping `cmap`, `OS/2`, `name`,
  `post`, `GPOS`/`GSUB`/`GDEF`, `DSIG` and the hinting programs — and truncates
  the glyph count to the highest used id, so `loca`/`hmtx` shrink too. A full
  ~411 KB family now embeds as ~30 KB for a short text run (×13). (Glyph ids are
  still preserved, not remapped — full GID compaction is a later enhancement.)

## [0.14.0] - 2026-06-16

### Changed

- **Embedded fonts are now subsetted on save.** Text drawn with `addText` tracks
  the glyph ids it uses per embedded font; `save`/`saveCompressed` rebuild each
  embedded `FontFile2` to keep only those glyph outlines (plus `.notdef` and any
  composite components). Glyph ids are **preserved** (no remap), so existing
  Identity-H content stays valid — only the outline data shrinks. A minimal edit
  that previously embedded a full ~300 KB family now adds only the glyphs it
  draws, fixing the round-trip size blow-up when re-baking edited text. No API
  change — the subsetting is automatic.

## [0.13.0] - 2026-06-16

### Added

- **`doc.addText(...)` gains `opacity` and `rotationDeg`** — baked text can now
  fade and rotate (text matrix), matching a host editor's `drawText` fidelity for
  edited/added text runs. ABI `gp_add_text` extended.
- **`doc.extractFont(name)`** — extract an embedded font program by (fuzzy)
  `/BaseFont` name, returning the raw decoded bytes + format (`truetype` embeds
  directly; `cff`/`type1` need a TTF conversion). Lets a host re-embed the
  document's **own** font when re-baking edited text and keep the original
  glyphs (no pdf-lib needed for source-font extraction). ABI `gp_extract_font`.
- **`doc.addMarkupAnnotation(page, subtype, quads, rgb, opacity, meta)`** —
  Highlight / Underline / StrikeOut / Squiggly spanning **multiple quads**
  (wrapped text), with full reviewer metadata (`contents`, `author`, `id`,
  `date`). ABI `gp_add_markup_annotation`.
- **`doc.addTextNote(page, rect, rgb, meta, icon, open)`** — sticky-note
  (`/Text`) annotation with popup contents + named icon. ABI `gp_add_text_note`.

## [0.12.0] - 2026-06-16

### Added

- **`doc.flattenForm()`** — flatten the whole interactive form: bake every field
  widget across **all pages** into the page content and drop `/AcroForm`, so the
  result is no longer fillable and `fields()` returns empty afterwards. Returns
  the number of widgets baked (0 when there is no form). Complements the
  per-page `flattenAnnotations(page)`. ABI `gp_flatten_form`.

## [0.11.0] - 2026-06-16

### Added

- **Form-field widget geometry** — `engine.open(pdf).fields()` (`FieldInfo`) now
  reports each field's `page` (1-based) and `bounds` (`[x, y, width, height]` in
  points, **top-left origin** — already Y-flipped from the PDF's bottom-left
  `/Rect`). Lets a host overlay/place field UI without re-parsing the PDF. Both
  are optional (absent when a field carries no widget rectangle). Read from the
  widget's `/Rect` and `/P`; falls back to page 1 when `/P` is missing.

## [0.10.0] - 2026-06-16

### Added

- **`doc.addWatermark(page, x, y, size, text, rgb?, opacity?, rotationDeg?)`** —
  stamp **rotated** text over an existing page in **standard Helvetica** (no font
  embedding needed), with opacity, for diagonal/corner watermarks.
- **`engine.helveticaWidth(size, text)`** — AFM text width in standard Helvetica,
  to position watermark/header text without a font. ABI `gp_add_watermark` /
  `gp_helvetica_width`.

## [0.9.0] - 2026-06-16

### Added

- **`engine.encryptionInfo(pdf)`** — inspect a PDF's encryption **without
  decrypting it** (no password needed): returns `{ encrypted, permissions,
  version, revision }`, read straight from the `/Encrypt` dictionary's `/P` /
  `/V` / `/R`. Works on password-protected files (where `open()` fails). ABI
  `gp_encryption_info`.

## [0.8.0] - 2026-06-16

### Added

- **AES PDF encryption** (`doc.saveEncrypted`). The Standard Security Handler can
  now *write* **AES-128 (V4/R4)** and **AES-256 (V5/R6)** in addition to RC4, with
  **separate user and owner passwords**:
  - `saveEncrypted(password, fileId, { algorithm: "aes256" | "aes128" | "rc4",
    ownerPassword, permissions, keySeed })` — defaults to **AES-256**.
  - AES-256 needs a **secret 32-byte file key** (the engine has no RNG): the SDK
    generates it with Web Crypto, or you pass `keySeed`. The decryption side
    already read AESV2/AESV3; `openEncrypted` now also accepts the **owner**
    password for R6 (Algorithm 2.A).
  - ABI `gp_save_encrypted` gains `owner`, `key` and an `algorithm` selector.

### Changed

- **Breaking (SDK):** `saveEncrypted(password, fileId, permissions?)` →
  `saveEncrypted(password, fileId, opts?)`. Pass `{ permissions }` (and
  `{ algorithm: "rc4" }` to keep the previous RC4 behaviour).

## [0.7.0] - 2026-06-15

### Added

- **Complete viewer zoom controls** (`@qrcommunication/gigapdf-lib/viewer`):
  `fitWidth()`, `fitPage()`, `actualSize()`, `setZoom()` / `setZoomPercent()` and a
  `zoom` getter; a toolbar **preset drop-down** (Fit width · Fit page · 50–400 %)
  with a live `%` readout; `Ctrl`/`⌘` + mouse-wheel zoom; and a `0` keyboard
  shortcut. A chosen **fit mode is sticky** — it re-applies when the viewport
  resizes (via `ResizeObserver`) and when paging to a page of a different
  orientation.
- **Editor rulers & margins** (`@qrcommunication/gigapdf-lib/editor`): every page
  shows graduated **millimetre rulers** (top + left) and four **margin guides**
  dragged **live** from handles in the ruler bands — or set via the palette's
  `T R B L` mm fields and the `setMargins()` / `getMargins()` / `showRulers()`
  API. Guides are drawn in page-point coordinates (on a second SVG layer) and kept
  a constant on-screen size at any zoom.

## [0.6.0] - 2026-06-15

### Added

- **Full page control for HTML→PDF** via `htmlRenderWith(html, fonts, options)`:
  - **named paper sizes** — `pageSize: "A4" | "a3-landscape" | "letter" | …`
    (ISO A0–A6, ISO B4/B5, US Letter/Legal/Tabloid/Executive; `-landscape`
    suffix swaps the axes). `giga.pageSize(name)` resolves one to `{ w, h }`
    points.
  - **per-side margins** — `margin: number | { top, right, bottom, left }`.
  - **running header & footer** — `header` / `footer` are full HTML+CSS
    snippets painted in the page margins on every page, with `{{page}}` /
    `{{pages}}` substitution and configurable `startPageNumber`,
    `headerOffset` / `footerOffset`.
- **`htmlNeededFontsWith(html, header, footer)`** — phase-1 font discovery that
  also scans the header/footer HTML so their Google fonts are fetched.
- New ABI exports: `gp_html_render_opts`, `gp_html_needed_fonts_ex`,
  `gp_page_size`.

### Images & SVG

- **SVG → native PDF vector** via `doc.addSvg(page, src, x, y, w, h)` (ABI
  `gp_add_svg`): shapes (`rect`/`circle`/`ellipse`/`line`/`polyline`/`polygon`),
  `<path>` (full `d` grammar with **exact `A` arc→Bézier conversion**), `<g>`
  groups, `transform`, `viewBox`, `fill`/`stroke`/`stroke-width`/`opacity`, and
  **gradients** (`<linearGradient>`/`<radialGradient>` → native PDF axial/radial
  shadings, with stops, `gradientUnits`, `gradientTransform` and `href`
  inheritance) — crisp at any zoom, not rasterized. In the HTML renderer, inline
  `<svg>` and `data:image/svg+xml` `<img>` sources render as native vector.
- **PNG transparency in the rasterizer**: `renderPage`/thumbnails now honour an
  image's `/SMask` (soft mask) as per-pixel alpha instead of treating it as
  opaque, so transparent PNGs composite correctly in every conversion (not just
  HTML→PDF).
- **Colour emoji** (COLR v0 + CPAL): when a text run uses a colour font (e.g.
  `font-family: "Noto Color Emoji"`), emoji are drawn as native vector colour
  layers in the HTML renderer — crisp, and rasterized for free. **Apple `sbix`
  bitmap emoji** are placed as PNG glyph bitmaps. Non-colour characters in the
  run still render as text. (COLRv1 gradient glyphs and `CBDT/CBLC` strikes are
  not yet drawn.)

### Viewer

- **`@qrcommunication/gigapdf-lib/viewer`** — a new zero-dependency browser
  document viewer (`GigaPdfViewer`) built on the engine (no pdf.js): opens PDF,
  Office (docx/xlsx/pptx, legacy, ODF) and HTML (auto-detected, converted
  in-engine), renders pages with `renderPage`, **detects per-page orientation**
  and adapts, with navigation, zoom, a thumbnail rail, keyboard control and a
  **fullscreen presentation mode**.
- **`@qrcommunication/gigapdf-lib/editor`** — an interactive **editing canvas**
  (`GigaPdfEditor`) extending the viewer: an SVG overlay per page with tools
  (text, rectangle, ellipse, line, freehand ink, image, highlight, redaction),
  select·move·delete, and `applyEdits()` that **bakes edits into the real PDF**
  through the engine (then re-renders); `save()` returns the result.

### CSS

- HTML→PDF renderer gained `min-width` / `max-width`, `height` / `min-height`,
  `box-sizing`, `text-indent` (first-line indent), `visibility: hidden`,
  `opacity` (backgrounds/borders/text rules), and `text-decoration: line-through`
  / `overline`. See [`docs/HTML-CSS.md`](../docs/HTML-CSS.md).

## [0.5.0] - 2026-06-15

### Changed

- **Suspendable JavaScript VM** for `<script>` execution (`htmlRender` /
  `runInlineScripts`). `function*` and `async` bodies now compile to a
  resumable bytecode machine, so:
  - **generators are truly lazy** — an infinite `while (true) { yield … }` is
    fine, `.next(v)` feeds a value back into the suspended `yield`, and `yield*`
    delegates lazily;
  - **`await` yields to the event loop** with spec microtask ordering (the
    synchronous tail runs before a deferred continuation), instead of draining
    the queue synchronously;
  - **full control flow** can span a `yield`/`await` — `try`/`catch`/`finally`
    (the handler survives suspension; a rejected `await` in a `try` is caught),
    `for…of`/`for…in`, `switch`, labelled `break`/`continue`, destructuring,
    compound assignment, and `...spread`.

  No API change — existing `htmlRender`/`runInlineScripts` calls simply behave
  correctly for script-driven, generator/async-heavy templates. A body the VM
  can't compile (e.g. `try`/`catch` around a `yield`/`await`) transparently
  falls back to the previous eager/synchronous model.

## [0.4.0] - 2026-06-15

### Added

- **AcroForm field creation.** Build interactive forms from scratch — no
  `pdf-lib`. New `GigaPdfDoc` methods, each taking a `[x0,y0,x1,y1]` rectangle
  and an optional [`FieldStyle`](src/index.ts) (font size, text/border/background
  colour, border width):
  - `addTextField(page, name, rect, value?, { maxLen, multiline, password, style })`
  - `addCheckbox(page, name, rect, checked?, { export, style })`
  - `addRadioGroup(page, name, options: RadioOption[], { selected, style })`
  - `addComboBox(page, name, rect, options, { selected, editable, style })`
  - `addListBox(page, name, rect, options, { selected, multi, style })`

  Every widget is given a real `/AP` appearance stream (text baseline, a vector
  tick for checkboxes, a filled dot for radios) **and** the form is flagged
  `NeedAppearances`, so fields display immediately and stay faithful when edited.
- **Advanced flexbox + real grid** in the HTML renderer:
  - `flex-direction: column`, `justify-content` (start/center/end/space-between/
    space-around) and per-item `flex-grow` (proportional column widths);
  - `display: grid` with `grid-template-columns` (fixed column count; children
    wrap into rows). `float` still maps to inline-block.
- **ES module syntax** is now parsed transparently by the JS engine (`import` is
  elided, `export` declarations run as normal statements).

## [0.3.0] - 2026-06-15

### Added

- **JavaScript engine** (zero-dependency, pure Rust → WASM). A document's inline
  `<script>`s now execute **before layout** inside `htmlRender` /
  `htmlNeededFonts` — no Chromium/Playwright — so script-driven content renders.
  The engine covers:
  - Language: classes + `super`, closures, destructuring, spread, optional
    chaining, template literals, `for…of`, generators (`function*`/`yield`,
    eager), `async`/`await` + `Promise` (deterministic synchronous microtask
    model), `try/catch/finally`, labelled loops, `arguments`.
  - Built-ins: `Object`/`Array`/`String`/`Number`/`Boolean`/`Math`/`JSON`/
    `console`/`Map`/`Set`/`RegExp` (a from-scratch backtracking regex engine)/
    `Error`, plus `parseInt`/`parseFloat`/`setTimeout`/`queueMicrotask`.
  - DOM bindings: `document.getElementById`/`getElementsByTagName`/
    `querySelector(All)` (combinators `>`/`+`/`~`, attribute selectors), and on
    elements `textContent`/`innerHTML`/`getAttribute`/`setAttribute`/
    `appendChild`/`removeChild`/`classList`/`style`/`children`.
- **Page breaks** in the HTML renderer: CSS `page-break-before|after: always`,
  `break-before|after: page`, a `<pagebreak>` element, or `class="page-break"`
  start the following content on a new page.
- **CSS flexbox** (`display: flex` / `inline-flex`) — a basic equal-column row;
  `grid` falls back to block flow and `float` to inline-block.

### Notes

- `htmlRender` / `htmlNeededFonts` are unchanged in signature — they simply run
  the document's scripts first. No new SDK call is required.

## [0.2.0] - 2026-06-15

### Added

- Vector drawing primitives on `GigaPdfDoc`: `drawLine`, `addEllipse`,
  `addPolygon`, and `addPath` — the latter accepts arbitrary SVG path data
  (`M`/`L`/`H`/`V`/`C`/`S`/`Q`/`T`/`A`/`Z`, absolute & relative), converting
  quadratic Béziers to cubics and flipping the Y axis like `pdf-lib`'s
  `drawSvgPath`. Covers freeform/polygon/triangle shapes.
- `addImage`: embed PNG or JPEG rasters as image XObjects. JPEG is stored
  losslessly via `/DCTDecode`; PNG is decoded in-engine (zero-dependency) with
  its alpha channel honoured through a `/SMask` soft mask.
- `opacity` (fill + stroke alpha through a transient `/ExtGState`) on every
  shape and image (`addRectangle`, `drawLine`, `addEllipse`, `addPolygon`,
  `addPath`, `addImage`).
- `toOdp`: convert a PDF to an editable OpenDocument Presentation (`.odp`) —
  one slide per page with positioned text boxes, pictures and shapes. This
  completes **bidirectional ODF** (`.odt` / `.ods` / `.odp` both ways, the
  reverse via `officeToPdf`), round-trip validated through LibreOffice Impress.
- **HTML → PDF rendering engine** (`htmlNeededFonts` + `htmlRender`): a
  zero-dependency in-engine pipeline — HTML parser → CSS cascade (selectors,
  specificity, inheritance, UA defaults) → block / inline / table layout with
  pagination → paint — that renders HTML + CSS to PDF **without a headless
  browser**. Text is set in **embedded Google fonts** resolved against the full
  catalogue (real glyphs + metrics → identical or nearest match). Validated
  end-to-end: Roboto downloaded, embedded (`emb=yes`, Identity-H) and the output
  opens in LibreOffice. JavaScript execution is not included (a separate engine).

### Changed

- `addRectangle` gains a trailing optional `opacity` argument — backward
  compatible (defaults to `1`).

[0.2.0]: https://github.com/qrcommunication/gigapdf-lib/releases/tag/v0.2.0

## [0.1.0] - 2026-06-14

### Added

- Initial public release of the TypeScript SDK for **gigapdf-lib**, a
  zero-dependency Rust→WASM PDF engine.
- `GigaPdfEngine`: `load()`, `loadDefault()` (Node), `open()`, `openEncrypted()`,
  stateless conversions (`txtToPdf`, `htmlToPdf`, `rtfToPdf`, `officeToPdf`), and
  font helpers (`fontCatalog`, `fontRequestUrl`, `parseCssFontUrl`).
- `GigaPdfDoc`: full document API — text intelligence (`textRuns`,
  `structuredText`, `search`, `ocr`, `ocrText`, `elements`, `elementAt`),
  editing (`replaceText`, `removeElement`, `moveElement`, `duplicateElement`,
  `addRectangle`, `redact`), pages (`rotatePage`, `deletePage`, `movePage`,
  `appendPages`, `extractPages`), rendering (`renderPage`), embedded fonts
  (`embedFont`, `addText`, `neededFonts`), conversions to
  text/HTML/DOCX/PPTX/ODT/XLSX/ODS/RTF/PDF-A, security (`saveEncrypted`, `sign`),
  metadata (`getMetadata`, `setMetadata`), annotations (square, highlight, line,
  free-text, underline, strike-out, ink, stamp, plus `annotations`,
  `removeAnnotation`, `flattenAnnotations`), hyperlinks (`links`, `addUriLink`,
  `addGotoLink`), outline (`outline`, `setOutline`), and AcroForm fields
  (`fields`, `setTextField`, `setCheckbox`, `setRadio`, `setChoice`).
- The engine `.wasm` is self-contained — no third-party runtime dependencies.

[0.1.0]: https://github.com/qrcommunication/gigapdf-lib/releases/tag/v0.1.0
