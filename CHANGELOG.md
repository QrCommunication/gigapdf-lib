# Changelog

All notable changes to **gigapdf-lib** (the Rust engine + the
`@qrcommunication/gigapdf-lib` TypeScript SDK) are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/) and the project adheres
to [Semantic Versioning](https://semver.org/).

The per-release SDK detail also lives in [`sdk/CHANGELOG.md`](sdk/CHANGELOG.md).

## [0.98.0] - 2026-06-25

Closes the **Office-export** ([#2](https://github.com/qrcommunication/gigapdf-lib/issues/2))
and **other-format conversion** ([#4](https://github.com/qrcommunication/gigapdf-lib/issues/4))
fidelity roadmaps, adds a from-scratch **WMF/EMF metafile decoder**, and advances
the Office-import ([#3](https://github.com/qrcommunication/gigapdf-lib/issues/3))
and PDF→model ([#5](https://github.com/qrcommunication/gigapdf-lib/issues/5)) roadmaps.

### Added

- **WMF + EMF metafile decoder, from scratch** (no third-party codec): a GDI
  rasterizer — placeable/standard WMF + EMF `ENHMETAHEADER`, pen/brush/font objects,
  poly/rect/ellipse/arc/bezier records, EMF affine world transforms, DIB blit
  (1/4/8/24/32-bpp + RLE) → RGBA. Wired into **RTF import** (`{\pict\wmetafile/\emfblip/\dibitmap}`
  + the `\bin` binary form) and **Office import** (embedded `.wmf`/`.emf` media → PNG).

### Completed — Office export ([#2], now closed)

- Multi-section page setup (DOCX per-section `w:sectPr` + headers/footers; ODT
  master-pages); PPTX/ODP **non-slide content** keeps real lists/tables/headings
  (no longer flattened to paragraphs); super/subscript (ODT+PPTX); spreadsheet
  underline/strike (XLSX+ODS); ODT block shapes; internal page links (all four
  exporters); explicit-vs-unset run colour. (With the formulas/image-format/speaker-
  notes/inline-images/hyperlinks/list-nesting/borders shipped across 0.94–0.98.)

### Completed — other-format conversions ([#4], now closed)

- RTF import decodes WMF/EMF/DIB pictures + the `\bin` binary form — completing #4
  alongside the earlier rich RTF↔model, model-aware `to_text`/`to_rtf`, CSV
  typed-cell import + standard multi-sheet export, Markdown colour/shapes, nested
  EPUB TOC + unique identifier, and veraPDF-conformant PDF/A.

### Improved — Office import ([#3], roadmap)

- PPTX/ODP run styling (underline/strike/highlight) + paragraph formatting + bullet/
  number lists; ODS cell merges, number formats (incl. date/time) + fills at XLSX
  parity; internal hyperlink anchors resolve to their bookmark page; DOCX `w:vMerge`
  real row spans via grid-column tracking; PPTX/ODP speaker notes → `Slide.notes`;
  embedded WMF/EMF images decoded to PNG + magic-byte format detection for
  GIF/BMP/TIFF/SVG media.

### Improved — PDF → model ([#5], roadmap)

- Page `/Rotate` (90/180/270) applied to reconstructed blocks (dimensions swap,
  text reads upright); tagged-PDF blocks distributed onto their real `/Pg` pages.

## [0.97.0] - 2026-06-25

The third from-scratch image codec — **JPEG 2000** — lands and is wired into the
render/extract pipeline, **PDF/A export is now veraPDF-validated conformant**, and
a second broad pass over the conversion-fidelity roadmaps
([#2](https://github.com/qrcommunication/gigapdf-lib/issues/2)/[#3](https://github.com/qrcommunication/gigapdf-lib/issues/3)/[#4](https://github.com/qrcommunication/gigapdf-lib/issues/4)/[#5](https://github.com/qrcommunication/gigapdf-lib/issues/5),
which stay open for their long tails).

### Added — PDF reading

- **JPEG 2000 (`JPXDecode`) decoder, from scratch** (no third-party codec): JP2
  box container + raw codestream, all markers, tier-2 packet decoding (tag-trees,
  all five progression orders), tier-1 EBCOT, inverse 5/3 + 9/7 DWT, inverse
  multi-component transform — **wired into the image pipeline** so JPEG 2000
  images (and their `/SMask`s) render and extract. Completes the set of
  hand-written image codecs (CCITTFax · JBIG2 · JPEG2000).
  ([#35](https://github.com/qrcommunication/gigapdf-lib/issues/35))

### Fixed — PDF/A is now genuinely conformant

- `toPdfA` now **embeds every font** — substituting a bundled metric-compatible
  standard face (flags + widths matched, `/ToUnicode` kept) for any face the
  source only references by name — **strips forbidden constructs** (encryption,
  document JavaScript) and sources **metadata from the document** (`/Info` + XMP
  agree). Validated **PASS, 0 failed rules by veraPDF** (1b + 2b), closing the
  former false-conformance gap.
  ([#4](https://github.com/qrcommunication/gigapdf-lib/issues/4))

### Improved — conversion fidelity (roadmap second pass)

- **PDF → model reconstruction** ([#5](https://github.com/qrcommunication/gigapdf-lib/issues/5)):
  bold/italic from the `/FontDescriptor` (not just the font name); multi-column
  detection robust to full-width lines; centroid line-grouping (superscripts no
  longer split off); table detection handles borderless merged spans, large/sparse
  and rotated tables; the tagged path honours `/ColSpan`/`/RowSpan`,
  `/ListNumbering`, `/Pg` page and `/BBox` geometry.
- **Office import** ([#3](https://github.com/qrcommunication/gigapdf-lib/issues/3),
  [#37](https://github.com/qrcommunication/gigapdf-lib/issues/37)): DOCX symbol
  runs, text boxes and field codes; footnotes/endnotes; text-less PPTX/ODP
  autoshapes (fill/line/geometry); ODT ordered lists + table cell spans.
- **Office export** ([#2](https://github.com/qrcommunication/gigapdf-lib/issues/2)):
  standard multi-sheet CSV; Markdown run colour + shapes (inline HTML/SVG); PPTX
  paragraph formatting; ODT/PPTX run images; PPTX/ODP external hyperlinks; ODT
  nested lists + table borders.
- **Other formats** ([#4](https://github.com/qrcommunication/gigapdf-lib/issues/4)):
  RTF and plain-text export are model-tree-aware (aligned tables, list markers,
  styled RTF) for tagged/imported docs; CSV import infers typed cells
  (number/bool/date) with conservative text guards.

## [0.96.0] - 2026-06-25

Two headline features close — **PDF linearization** and **from-scratch bilevel
image codecs** — alongside a broad first pass over the four conversion-fidelity
roadmaps ([#2](https://github.com/qrcommunication/gigapdf-lib/issues/2)/[#3](https://github.com/qrcommunication/gigapdf-lib/issues/3)/[#4](https://github.com/qrcommunication/gigapdf-lib/issues/4)/[#5](https://github.com/qrcommunication/gigapdf-lib/issues/5),
which remain open for their longer tails).

### Added — PDF

- **Linearization / Fast Web View.** `toLinearized()` / `saveLinearized()` —
  a byte-exact ISO 32000-1 Annex F implementation (the `/Linearized` parameter
  dict, first-page + main cross-reference sections, and bit-packed page-offset +
  shared-object **hint streams**, with a multi-pass offset solver), **validated
  clean by qpdf** (`qpdf --check` reports the file linearized, zero warnings).
  ([#67](https://github.com/qrcommunication/gigapdf-lib/issues/67))
- **CCITTFax + JBIG2 bilevel image decoders, from scratch** (no third-party
  codec). `CCITTFaxDecode` (G3/G4: modified-Huffman tables, 1-D + 2-D READ,
  `/K`/`/Columns`/`/BlackIs1`/byte-align/EOL/RTC). `JBIG2Decode` (full ITU-T
  T.88: MQ arithmetic + integer decoders, generic/refinement/halftone +
  pattern-dictionary/symbol-dictionary/text regions, arithmetic **and** Huffman
  coding incl. REFAGG, indirect `/JBIG2Globals`) — scanned-document PDFs now
  render and extract. ([#34](https://github.com/qrcommunication/gigapdf-lib/issues/34))

### Improved — conversion fidelity (roadmap first pass)

- **Office export** ([#2](https://github.com/qrcommunication/gigapdf-lib/issues/2)):
  XLSX/ODS **cell formulas** emitted (`<f>` / `table:formula`); media parts now
  carry their **real image format** (a JPEG no longer ships as a corrupt `.png`)
  across all exporters; **PPTX/ODP speaker notes**; EPUB gains a **nested TOC**,
  a **unique deterministic identifier**, and **inline-SVG shapes**.
- **Office import** ([#3](https://github.com/qrcommunication/gigapdf-lib/issues/3)):
  DOCX/ODT **running headers & footers** lowered to `Section.header/footer`;
  XLSX **per-cell character styling** (`applyFont` gate); DOCX **footnotes &
  endnotes** inlined at their reference points.
- **Other formats** ([#4](https://github.com/qrcommunication/gigapdf-lib/issues/4)):
  **RTF ↔ model** is now rich both ways (styling, tables, images, hyperlinks);
  **Markdown → model** handles GFM (strikethrough, images, task-lists,
  reference/footnote links, setext, inline HTML); vector **Shapes render as
  inline `<svg>`** in HTML/EPUB.
- **PDF → model reconstruction** ([#5](https://github.com/qrcommunication/gigapdf-lib/issues/5)):
  running **headers/footers stripped** from body prose (and preserved in
  `Section`); **heading levels** clustered into stable monotonic ranks; **list
  false positives** rejected via ordinal-sequence validation.

### Fixed

- Three exporter schema-conformance bugs the new XSD/RelaxNG CI gate surfaced —
  DOCX text-box shapes now MCE-valid (VML), XLSX inline `<t>` drops the illegal
  `xml:space`, ODS emits `table:table-column` before rows — so the gate passes
  with **zero waivers**. ([#19](https://github.com/qrcommunication/gigapdf-lib/issues/19) follow-up)

## [0.95.0] - 2026-06-25

Thirteen roadmap issues across PDF authoring, PDF reading, Office round-trip
fidelity and CI conformance — the post-[#1](https://github.com/qrcommunication/gigapdf-lib/issues/1)
cleanup, implemented in parallel.

### Added — PDF authoring

- **Page transitions.** `setPageTransition` / `getPageTransition` /
  `clearPageTransition` — the full ISO 32000-1 §12.4.4 `/Trans` set (12 styles +
  `/Dm`/`/M`/`/Di`/`/SS`/`/B`) plus per-page `/Dur` auto-advance, for
  kiosk/presentation PDFs. ([#65](https://github.com/qrcommunication/gigapdf-lib/issues/65))
- **Scale page content.** `scalePageContent` / `scalePageContentXy` / `scalePageTo`
  — a true `cm`-wrap of the content stream **plus** boxes and annotation rects (not
  just a box resize), and `/UserUnit` authoring for large-format pages.
  ([#68](https://github.com/qrcommunication/gigapdf-lib/issues/68))
- **Embedded-file portfolio `/Collection`.** `setCollection` / `collection` — view
  (details/tiles/hidden), `/Schema` columns, `/Sort`, default file, and per-file
  `/CI` metadata, built on the existing embedded-files plumbing.
  ([#66](https://github.com/qrcommunication/gigapdf-lib/issues/66))
- **Per-figure alternate text.** `setFigureAlt` / `figureCount` — real `/Alt` on
  `Figure` structure elements, flowing into **both** PDF/UA (`toTagged`) and PDF/A
  level-A exports (which previously emitted none).
  ([#20](https://github.com/qrcommunication/gigapdf-lib/issues/20))

### Added — PDF reading

- **Optional-content visibility** is enforced during render: content in OCGs that
  are OFF by default (`/OCProperties /D`) is hidden, OCMD `/P` membership policies
  (`AnyOn`/`AllOn`/`AnyOff`/`AllOff`) resolved, and `/OC` on marked content +
  XObjects honored (nested-correct).
  ([#54](https://github.com/qrcommunication/gigapdf-lib/issues/54))
- **Vertical writing mode** (`Identity-V` / CMap `/WMode 1`): text lays out
  top-to-bottom using `/W2`+`/DW2` vertical metrics and the glyph position vector,
  in both extraction and rendering.
  ([#49](https://github.com/qrcommunication/gigapdf-lib/issues/49))

### Added / improved — Office round-trip

- **Document outline / TOC** built from DOCX/ODT headings + bookmarks into
  `Document.outline` (internal `_Toc`/`_GoBack` bookmarks dropped).
  ([#31](https://github.com/qrcommunication/gigapdf-lib/issues/31))
- **Named styles** (DOCX `styles.xml`, ODT `office:styles`) lowered to
  `Document.styles` so `style_ref` resolves.
  ([#30](https://github.com/qrcommunication/gigapdf-lib/issues/30))
- **DOCX floating/inline drawings** keep size, anchored position, and image alt
  text. ([#40](https://github.com/qrcommunication/gigapdf-lib/issues/40))
- **Super/subscript** run position round-trips across DOCX/XLSX/PPTX/ODF →
  `CharStyle.vertical_align`.
  ([#32](https://github.com/qrcommunication/gigapdf-lib/issues/32))
- **Flat-XML ODF** (`.fodt`/`.fods`/`.fodp`/`.fodg`) and **`.odg`** drawings are now
  importable, reusing the existing ODF lowering.
  ([#53](https://github.com/qrcommunication/gigapdf-lib/issues/53))
- **Table/cell vertical alignment** modeled and round-tripped (import + export +
  render) across DOCX/XLSX/PPTX/ODF tables and spreadsheet cells.
  ([#27](https://github.com/qrcommunication/gigapdf-lib/issues/27))

### Tooling

- **Strong schema validation** in CI — exported Office files are validated against
  the official ECMA-376 XSD and ODF RelaxNG schemas (`xmllint`), not just OPC
  well-formedness; the gate also surfaced three pre-existing exporter conformance
  bugs (baselined for a follow-up).
  ([#19](https://github.com/qrcommunication/gigapdf-lib/issues/19))

## [0.94.0] - 2026-06-25

The largest release so far. **Issue [#1](https://github.com/qrcommunication/gigapdf-lib/issues/1)
— the HTML/CSS + inline-SVG rendering engine — is complete**, capped by inline
`@font-face` backed by a from-scratch **WOFF2/brotli** font decoder, plus **21**
independent PDF, font and Office fixes implemented in parallel.

### Added — HTML/CSS + SVG engine ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1), complete)

- **`@font-face` with inline `src`.** `data:` font URIs (`ttf`/`otf`/`woff`/
  **`woff2`**) are decoded and registered as render fonts, matched by family /
  weight / style. Backed by a **from-scratch brotli decompressor (RFC 7932)** and
  a **WOFF/WOFF2 → sfnt reconstructor** (full `glyf`/`loca` transform reverse,
  validated coordinate-exact against fontTools), both zero-dependency.
- **Full UAX#9 bidirectional text** — explicit isolates/embeddings (X1–X10) +
  bracket pairing (N0) for `direction: rtl` mixed runs.
- **SVG `filter`** — a complete `fe*` pipeline (Gaussian blur, colour-matrix,
  composite, blend, merge, turbulence, morphology, displacement, drop-shadow,
  component-transfer) rasterised and emitted as a soft-masked image.
- **COLRv1** colour-font glyphs (linear/radial/**sweep** gradients, composite
  blend modes, variable deltas) + **CBDT/CBLC** bitmap strikes.
- **`<pattern>` tiling** (contour clip, `patternTransform`, nested patterns).
- **`position: sticky`** paged running headers/footers; **`float`** shrink-to-fit
  + block wrap + cross-container context; **flexbox** column axis; **box-shadow**
  true Gaussian blur + `inset`; **3-D border styles** (inset/outset/groove/ridge);
  smoother **conic-gradient** (360° sectors); nearest-weight **`font-weight`**
  matching + per-weight embedding; **`border-radius`** content clipping;
  **`background-image`**; per-stop varying gradient alpha via a luminosity soft mask.

### Added — PDF authoring

- **Document-level JavaScript** (`/Names /JavaScript`) authoring API:
  `addDocumentJavascript` / `documentJavascripts` / `removeDocumentJavascript`.
  ([#64](https://github.com/qrcommunication/gigapdf-lib/issues/64))
- **Optional-content (OCG) layers** — `beginOptionalContent` / `endOptionalContent`
  assign page content to a toggleable layer via marked content.
  ([#59](https://github.com/qrcommunication/gigapdf-lib/issues/59))
- **N-up / imposition** — `placePage` / `placePageMatrix` / `nUp`: place a source
  page as a scaled Form XObject onto another page.
  ([#60](https://github.com/qrcommunication/gigapdf-lib/issues/60))
- **In-place image XObject replacement.**
  ([#62](https://github.com/qrcommunication/gigapdf-lib/issues/62))
- **Default appearance streams** synthesised for FreeText/Stamp/Squiggly/Text/
  Link/FileAttachment annotations.
  ([#55](https://github.com/qrcommunication/gigapdf-lib/issues/55))
- **Document properties + language** emitted to the catalog/metadata.
  ([#21](https://github.com/qrcommunication/gigapdf-lib/issues/21))

### Added — PDF reading

- **Inline images** (`BI`/`ID`/`EI`) decoded through the shared filter pipeline.
  ([#38](https://github.com/qrcommunication/gigapdf-lib/issues/38))
- **Hybrid `/XRefStm` + `/Prev`** incremental-revision cross-reference resolution
  (newest-wins, free entries, cycle-guarded).
  ([#56](https://github.com/qrcommunication/gigapdf-lib/issues/56))
- **Function-based (type 1) shadings.**
  ([#50](https://github.com/qrcommunication/gigapdf-lib/issues/50))
- **Embedded Type1 (`FontFile`) glyph** outline rasterisation.
  ([#43](https://github.com/qrcommunication/gigapdf-lib/issues/43))
- **In-page rotated/vertical text** preserved during PDF→model reconstruction.
  ([#28](https://github.com/qrcommunication/gigapdf-lib/issues/28))

### Added — Office import

- **Document metadata** (`docProps/core+app`, ODF `meta.xml`) → `DocMeta`
  (title/author/subject/keywords/lang + dates/description/revision/app/company).
  ([#29](https://github.com/qrcommunication/gigapdf-lib/issues/29))
- **DOCX paragraph & table styling** (alignment/spacing/indent/borders/shading/
  spans) lowered to the model.
  ([#36](https://github.com/qrcommunication/gigapdf-lib/issues/36))
- **DOCX hard page breaks + multi-level numbering** (`numbering.xml`).
  ([#39](https://github.com/qrcommunication/gigapdf-lib/issues/39))
- **Slide/page background fill** (PPTX `p:bg`, ODP `draw:page`).
  ([#51](https://github.com/qrcommunication/gigapdf-lib/issues/51))
- **ODP frame rotation, placeholder role, image alt text.**
  ([#48](https://github.com/qrcommunication/gigapdf-lib/issues/48))

### Added — Office export

- **PPTX `slideLayout`/`slideMaster` chain** wired to every slide.
  ([#23](https://github.com/qrcommunication/gigapdf-lib/issues/23))
- **Real tables on slides** (`a:tbl` / `table:table`) instead of flattened
  paragraphs. ([#26](https://github.com/qrcommunication/gigapdf-lib/issues/26))
- **PPTX run highlight/background** (`a:highlight`).
  ([#24](https://github.com/qrcommunication/gigapdf-lib/issues/24))
- **ODP placeholder role** (`presentation:class`).
  ([#25](https://github.com/qrcommunication/gigapdf-lib/issues/25))
- **Named style table** + paragraph style references (DOCX `styles.xml`, ODT
  `style:style`). ([#22](https://github.com/qrcommunication/gigapdf-lib/issues/22))

## [0.93.0] - 2026-06-24

PDF-read, Office-import and PDF-edit. Three independent fixes implemented in
parallel ([#46](https://github.com/qrcommunication/gigapdf-lib/issues/46),
[#47](https://github.com/qrcommunication/gigapdf-lib/issues/47),
[#61](https://github.com/qrcommunication/gigapdf-lib/issues/61)).

### Added

- **`appendPages(otherPdf, pages?)` / `mergePdfs([... | {pdf, pages?}])`** — merge
  or append with **page-range selection**: pass 1-based page numbers to bring in
  only those source pages (content, resources, annotations and box geometry deep-
  copy unchanged); omit for all pages. ([#61](https://github.com/qrcommunication/gigapdf-lib/issues/61))
- **Type0 CJK fonts.** Predefined CMaps (Identity-H/V, the `Uni*-UCS2` families,
  and 2-byte legacy `GBK`/`B5`/`KSC`/RKSJ codespaces) and **embedded CMap
  streams** now decode code→CID, and a non-Identity `/CIDToGIDMap` resolves
  CID→GID, so composite-font text extracts and renders with correct glyphs and
  CID-keyed widths. ([#46](https://github.com/qrcommunication/gigapdf-lib/issues/46))
- **PPTX import fidelity.** Run hyperlinks, table cell fill + borders,
  theme/scheme colour resolution (`a:schemeClr` + `lumMod`/`lumOff`/`shade`/`tint`,
  `hslClr`/`sysClr`), first-stop gradient fallback, and 180° (double) shape mirror.
  ([#47](https://github.com/qrcommunication/gigapdf-lib/issues/47))

## [0.92.0] - 2026-06-24

PDF-read, Office-import and catalog-authoring. Three independent fixes
implemented in parallel
([#42](https://github.com/qrcommunication/gigapdf-lib/issues/42),
[#52](https://github.com/qrcommunication/gigapdf-lib/issues/52),
[#63](https://github.com/qrcommunication/gigapdf-lib/issues/63)).

### Added

- **`setViewerPreferences()` / `setPageLayout()` / `setPageMode()`** author the
  document catalog's reading/UX hints (ISO 32000-1 §12.2): `/ViewerPreferences`
  (`HideToolbar`/`HideMenubar`/`HideWindowUI`/`FitWindow`/`CenterWindow`/
  `DisplayDocTitle` tri-state booleans + `/Direction`), `/PageLayout`, `/PageMode`.
  ([#63](https://github.com/qrcommunication/gigapdf-lib/issues/63))
- **Type3 fonts.** Glyphs defined as `/CharProcs` content streams now render —
  each shown code runs its glyph procedure through the engine's content-stream
  interpreter under `FontMatrix · textMatrix`, reusing the page/form machinery
  (fills, strokes, clips, nested forms); `d0`/`d1` accepted; width via `/Widths`
  through the `/FontMatrix`. ([#42](https://github.com/qrcommunication/gigapdf-lib/issues/42))
- **ODT import fidelity.** Paragraph styling (`fo:text-align`, margins/indents,
  `fo:text-indent`, `fo:line-height` with parent-style inheritance), footnotes/
  endnotes (`text:note`) inlined at the reference, body text boxes
  (`draw:text-box`), and table column widths + cell shading.
  ([#52](https://github.com/qrcommunication/gigapdf-lib/issues/52))

## [0.91.0] - 2026-06-24

PDF-read & Office-import fidelity. Two independent fixes implemented in parallel
([#33](https://github.com/qrcommunication/gigapdf-lib/issues/33),
[#45](https://github.com/qrcommunication/gigapdf-lib/issues/45)).

### Added

- **Stream filters `LZWDecode` / `ASCII85Decode` / `ASCIIHexDecode` /
  `RunLengthDecode`.** All four classic PDF filters are decoded in-house and
  dispatched through the shared `decode_stream`, which now walks the `/Filter`
  array with its parallel `/DecodeParms` (so chains like
  `[/ASCII85Decode /FlateDecode]` and per-filter predictors decode correctly).
  `/F`/`/DP` abbreviations and LZW `/EarlyChange` honoured.
  ([#33](https://github.com/qrcommunication/gigapdf-lib/issues/33))
- **ODS import fidelity.** The OpenDocument-spreadsheet importer now resolves
  per-cell styling (font, fill, border, alignment, wrap), reconstructs
  number/percent/currency format codes from `<number:*-style>`, applies
  cell→row→column style precedence, and emits `table:number-columns/rows-spanned`
  merges plus column widths and row heights. (Date/time data-style patterns and
  vertical-align remain unmapped.)
  ([#45](https://github.com/qrcommunication/gigapdf-lib/issues/45))

## [0.90.0] - 2026-06-24

PDF-read & Office-import fidelity. Two independent fixes implemented in parallel
([#41](https://github.com/qrcommunication/gigapdf-lib/issues/41),
[#44](https://github.com/qrcommunication/gigapdf-lib/issues/44)).

### Added

- **Image `/ImageMask` and `/Mask`.** A 1-bpc `/ImageMask` stencil now paints the
  current fill colour through its unmasked bits (honouring `/Decode`); a `/Mask`
  **explicit** stencil stream is resampled and folded into the base image's alpha;
  a `/Mask` **colour-key** array (`[min max …]`) makes matching raw samples
  transparent. All compose through the engine's existing coverage-alpha path.
  ([#41](https://github.com/qrcommunication/gigapdf-lib/issues/41))
- **XLSX import fidelity.** The spreadsheet importer now carries per-cell styling
  (font, fill, border, alignment, wrap from `cellXfs`), preserves number/date
  format codes (builtin + custom `<numFmts>`) for serial→display formatting,
  expands shared formulas (`<f t="shared">` followers via relative-ref
  translation), and attaches cell hyperlinks (external + in-workbook). Workbook
  `<definedName>` ranges remain unsupported.
  ([#44](https://github.com/qrcommunication/gigapdf-lib/issues/44))

## [0.89.0] - 2026-06-24

PDF-read fidelity. Two independent fixes implemented in parallel
([#57](https://github.com/qrcommunication/gigapdf-lib/issues/57),
[#58](https://github.com/qrcommunication/gigapdf-lib/issues/58)).

### Added

- **`/DecodeParms` predictors for FlateDecode/LZWDecode** (TIFF Predictor 2 and
  PNG predictors 10–15), applied in the shared `decode_stream` so both image
  streams and cross-reference / object streams (`/Type /XRef`, `/ObjStm`)
  de-predict correctly. Honours `/Predictor`, `/Columns`, `/Colors`,
  `/BitsPerComponent`; lenient on sub-byte TIFF widths. ([#57](https://github.com/qrcommunication/gigapdf-lib/issues/57))
- **CalGray / CalRGB colour** now applies `/Gamma`, the CalRGB `/Matrix` →
  CIE-XYZ map and a dependency-free Bradford white-point→D65 + XYZ→sRGB
  conversion (identity matrix takes a fast linear-sRGB path); ICCBased keeps its
  `/N` (or `/Alternate`) device fallback, read correctly from the profile stream
  dict. Improves text, vector and image colour at one seam. ([#58](https://github.com/qrcommunication/gigapdf-lib/issues/58))

## [0.88.1] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item B).

### Added

- **`box-shadow: inset`** is now painted (it was parsed then dropped). The inner
  area — the box inset by `spread + blur` and shifted by the offset — is left
  clear and the surrounding frame is filled with the shadow colour, clipped to
  the box, so it reads as recessed. Blur is approximated like the outset path.

## [0.88.0] - 2026-06-24

HTML/CSS renderer — column-axis flex sizing ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Added

- **`flex-basis` / `flex-grow` / `flex-shrink` on the column axis.** A
  `flex-direction: column` container now flexes item **heights**: `flex-basis`
  (or a definite `height`) sets an item's main size; with a definite container
  `height`, `flex-grow` distributes the leftover and `flex-shrink × basis` absorbs
  overflow — exactly as the row axis already did for widths. `flex_column_axis`
  became a two-pass layout (measure content → re-flex heights → reposition),
  preserving the existing content-sized + `justify-content` behaviour when no
  flex sizing applies.

## [0.87.2] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item C).

### Added

- **3-D `border-style`s** — `inset`, `outset`, `groove`, `ridge` (previously they
  rendered as flat `solid`). The top/left and bottom/right sides take a darker or
  lighter shade of the colour to fake depth: `inset`/`outset` shade each side as
  one tone; `groove`/`ridge` split each side into an outer and inner half-width
  band with opposite tones (a carved groove / raised ridge).

## [0.87.1] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Added

- **`aspect-ratio`** (`16/9`, `1.5`, `auto 16/9`). When a block has no definite
  `height`, its height is derived from the resolved width as `width / ratio`
  (taller content then overflows, and is clipped under `overflow: hidden`).
  `min-height` still applies as a floor.

## [0.87.0] - 2026-06-24

HTML/CSS renderer — colour alpha ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item C).

### Added

- **Colour alpha is now applied** instead of parsed-then-dropped. `rgba()`,
  `hsla()`, `#rgba` and `#rrggbbaa` carry their alpha through to the paint: it is
  folded into the opacity of whatever the colour paints — text, background,
  border, rounded box and table cell — and composes with the element `opacity`.
  New public `parse_color_alpha()` returns `(rgb, alpha)`; `parse_color()` stays a
  thin wrapper that drops the alpha.

### Fixed

- A function colour with **internal spaces** (`rgba(0, 0, 0, .5)`,
  `hsl(0 100% 50%)`) is now parsed in the `background` and `border` shorthands
  (the whitespace tokeniser previously split it mid-function and dropped it).

## [0.86.1] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Added

- **`grid-template-rows` with `%` and `fr`** (previously only fixed `pt` rows were
  honoured). `%` rows resolve against the grid's definite `height`; `fr` rows share
  the leftover space after the fixed / `%` / `auto` rows and gaps, growing the rows
  (and shifting their already-placed content) to fill the container. With no
  definite grid height `%`/`fr` fall back to content sizing — the correct
  auto-height behaviour. `auto` and `minmax()` unchanged.

## [0.86.0] - 2026-06-24

HTML/CSS renderer — real `overflow` clipping ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Added

- **`overflow: hidden` / `clip` now emit a real PDF clip** (`q … re W n … Q`)
  instead of whole-fragment culling only. Content straddling a box edge — text,
  images, backgrounds, gradients — is pixel-clipped to the padding box; nested
  clipping boxes intersect. New `Fragment::Clipped` carries the clip through
  pagination and band offsetting; new public `Document::push_clip_rect` /
  `Document::restore_graphics` emit the clip ops.
- **Definite `height`.** `height` is now a definite box height (taller content
  overflows, and is clipped under `overflow: hidden`) rather than an alias of
  `min-height`. `min-height` remains a pure floor; both compose.
- **Text runs carry their advance width** (`Fragment::Text.w`), so horizontally
  overflowing text registers as straddling and is clipped (previously a run was a
  zero-width point and never clipped).

## [0.85.4] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Added

- **`flex-direction: row-reverse` / `column-reverse`** now run the main axis from
  the far end (the items are reversed after the `order` sort) instead of
  collapsing to the forward axis. New `Style::flex_reverse`, parsed from both the
  `flex-direction` longhand and the `flex-flow` shorthand.

## [0.85.3] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item A).

### Fixed

- **`justify-content: space-evenly`** now distributes `n + 1` equal gaps (one
  before each item and one after the last) instead of being aliased to
  `space-around` (which puts half-size gaps at the ends). New `Justify::SpaceEvenly`.

## [0.85.2] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item C).

### Added

- **`currentColor`** now resolves to the element's cascaded `color` in the
  HTML→PDF renderer (case-insensitive): `border-color: currentColor`,
  `background: currentColor`, and as a sub-token of the `border` shorthand
  (`1px solid currentColor`). Previously it was unrecognised → the property was
  left unset.

## [0.85.1] - 2026-06-24

HTML/CSS renderer fidelity ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) roadmap, item E).

### Added

- **Absolute & relative CSS length units** in the HTML→PDF renderer: `cm`, `mm`,
  `in`, `pc`, `q` (anchored at `1in = 72pt`) and `ex`/`ch` (0.5em approximation).
  Resolved by `parse_len_px`; a single `LENGTH_UNITS` table keeps unit detection
  (flex-basis / font-size) in lock-step with resolution.

## [0.85.0] - 2026-06-24

Accessibility: **standalone tagged-PDF / PDF-UA authoring**. Resolves
[#18](https://github.com/qrcommunication/gigapdf-lib/issues/18).

### Added

- **`Document::to_tagged(pdf_ua)`** — author a tagged (accessible) PDF: a
  `/StructTreeRoot` logical-structure tree (`P`/`H1`–`H6`/`Table`/`TR`/`TH`/`TD`/
  `L`/`LI`/`Figure`) with marked content (`/MCID`), `/MarkInfo /Marked true`,
  `/Lang`, an (empty) `/RoleMap`, and `/Alt` on every `Figure` — **without**
  forcing PDF/A (no OutputIntent / ICC / `pdfaid`). `pdf_ua` stamps the PDF/UA-1
  identifier (ISO 14289) in XMP. ISO 32000-1 §14.7/§14.8.
- WASM `gp_to_tagged`; SDK `doc.toTagged({ pdfUa? })`.

### Notes

- Reuses the structure builder added for PDF/A level A (`convert/tagged.rs`); the
  standalone path post-processes the tree (figure `/Alt`, `/RoleMap`) without
  altering the PDF/A output. Figures get a non-empty `/Alt` placeholder so the
  file is structurally PDF/UA-valid — meaningful alternate text still requires
  author input (a per-figure alt-text API is future work). For archival +
  accessibility together, `to_pdfa_level(Pdfa2a)` emits the same tree as PDF/A-2a.

## [0.84.0] - 2026-06-24

Security: **public-key (certificate) encryption** + **password management**.
Resolves [#17](https://github.com/qrcommunication/gigapdf-lib/issues/17).

### Added

- **`Document::encrypt_for_recipients(&[cert_der], perms, aes256, encrypt_metadata,
  seed, rng)`** — public-key (certificate) security (`/Filter /Adobe.PubSec`,
  `/SubFilter /adbe.pkcs7.s5`, ISO 32000-1 §7.6.5): a random seed is wrapped per
  X.509 recipient in a CMS `EnvelopedData` (RSA key transport), the file key is
  `Hash(seed || recipients)`, and objects are AESV2/AESV3-encrypted. Only a
  recipient private key can open the file — no shared password.
- **`Document::open_with_private_key(bytes, cert_der, key_der)`** — the read
  counterpart: recovers the seed from the recipient list and decrypts.
- **`Document::change_passwords(…)`** and **`remove_encryption()`** — re-key or
  decrypt an already-opened document.
- **`Document::save_encrypted_ex(…, encrypt_metadata)`** — exposes
  `/EncryptMetadata` for RC4/AESV2/AESV3 (folded into the file-key derivation).
- `RsaPrivateKey::to_pkcs1_der`. WASM `gp_encrypt_for_recipients` /
  `gp_open_with_private_key` / `gp_change_passwords` / `gp_remove_encryption`; SDK
  `encryptForRecipients` / `openWithPrivateKey` / `changePasswords` /
  `removeEncryption`.

### Notes

- Built on the same RustCrypto `cms`/`x509-cert`/`rsa` primitives as the signing
  stack; no new dependency. The engine has no RNG, so public-key encryption takes
  host randomness (`seed` ≥ 20 B, `rng_seed` ≥ 32 B); the SDK fills these from Web
  Crypto by default.
- Verified by a full in-engine round-trip (encrypt to a self-signed recipient →
  open with its private key; a stranger's key is rejected) for both AES-128 and
  AES-256.

## [0.83.0] - 2026-06-24

Press-ready **colour authoring** — fills and text are no longer limited to
DeviceRGB. Resolves
[#11](https://github.com/qrcommunication/gigapdf-lib/issues/11) (CMYK / spot
(`Separation`) / ICC `OutputIntent` / overprint).

### Added

- **`Color`** enum — `Rgb` · `Cmyk` · `Gray` · `Separation { name, tint, cmyk }`
  (a spot ink with its `DeviceCMYK` tint transform) · `IccBased { components,
  profile }` (ISO 32000-1 §8.6).
- **`Document::add_filled_rectangle(page, [x,y,w,h], &Color, opacity)`** and
  **`add_filled_polygon(page, &points, &Color, opacity)`** — fill shapes in any
  colour space (a `Separation`/`ICCBased` colour registers its colour-space
  resource on the page).
- **`add_text_color(page, x, y, size, text, font, &Color, …)`** — base-14 text in
  any colour space (the text-drawing core was refactored to take colour-setting
  operators, so RGB text is unchanged).
- **`set_overprint(page, fill, stroke, mode)`** — an `/ExtGState` with `/op`,
  `/OP`, `/OPM` for prepress trapping (ISO 32000-1 §8.6.7).
- **`add_output_intent(&profile, condition)`** — a document `OutputIntent`
  (`/S /GTS_PDFX`) embedding an ICC profile, decoupled from the PDF/A path; `/N`
  is read from the profile's data-colour-space signature.
- `gp_add_filled_rectangle` / `gp_add_filled_polygon` / `gp_add_text_color` /
  `gp_set_overprint` / `gp_add_output_intent`; SDK `addFilledRectangle` /
  `addFilledPolygon` / `addTextColor` / `setOverprint` / `addOutputIntent` + a
  `Color` union type.

### Notes

- CMYK/Separation/ICC fills + the OutputIntent pass `qpdf --check` (clean).
- Existing `add_rectangle`/`add_ellipse`/`add_polygon`/`add_text*` keep their RGB
  signatures (no breaking change); the new `*_color`/`*_filled_*` methods are the
  any-colour-space path.

## [0.82.0] - 2026-06-24

Gradient **authoring** — the rasterizer could already render shadings, but there
was no API to *produce* them. Resolves
[#12](https://github.com/qrcommunication/gigapdf-lib/issues/12) (gradients; tiling
patterns / blend-mode authoring deferred — see notes).

### Added

- **`Document::add_gradient(page, &GradientSpec)`** — paints a **linear** (axial,
  shading type 2) or **radial** (type 3) gradient over a rectangle, as a
  `PatternType 2` shading pattern (ISO 32000-1 §8.7.4 / §8.7.3). The colour stops
  compile to a PDF interpolation function (a type-2 exponential for two stops, a
  type-3 stitching function for more). New `GradientSpec` / `GradientKind` /
  `GradientStop` types.
- `gp_add_gradient` (kind + flat coords + parallel stop offset/colour arrays) /
  `doc.addGradient(page, { kind, coords, stops, rect, extend?, opacity? })`.

### Notes

- The produced shading patterns + PDF functions pass `qpdf --check` ("No syntax or
  stream encoding errors found").
- **Tiling patterns** (PatternType 1), **blend-mode authoring** (`/BM`, `/CA`),
  transparency-group authoring and the renderer's four non-separable blend modes
  are deferred — only gradient fills ship here.

## [0.81.0] - 2026-06-24

Compact output — **object streams** + a **cross-reference stream** (PDF 1.5+,
ISO 32000-1 §7.5.7/§7.5.8). The serializer previously only wrote the classic
PDF 1.4 structure. Resolves
[#10](https://github.com/qrcommunication/gigapdf-lib/issues/10) (linearization
excepted — see notes).

### Added

- **`serialize::to_pdf_compressed(objects, trailer, use_object_streams)`** — packs
  every non-stream object into Flate-compressed `/Type /ObjStm` object streams
  (type-2 cross-reference entries) and writes a `/Type /XRef` cross-reference
  stream (`/W [1 4 2]`). With `use_object_streams = false` it writes the xref as a
  stream while keeping classic object bodies. Stream objects always stay direct.
- **`Document::save_optimized(object_streams, xref_streams)`** — the compact save
  path (uncompressed streams are Flate-compressed first, like `save_compressed`).
  `gp_save_optimized` / `doc.saveOptimized({ objectStreams, xrefStreams })`.
- Factored the shared `Document::flate_streams()` helper out of `save_compressed`.

### Notes

- **Conformance-validated**: both modes pass `qpdf --check` ("No syntax or stream
  encoding errors found") and a `qpdf --qdf` decompression round-trip — an
  independent authoritative validator, not just the engine's own parser.
- **Linearization** (Fast Web View, ISO 32000-1 Annex F) is a separate byte-layout
  optimization and is **not** produced here.

## [0.80.0] - 2026-06-24

Signature **verification** and **DocMDP certification** (ISO 32000-1 §12.8) — the
signing stack could produce signatures but not check them. Resolves
[#16](https://github.com/qrcommunication/gigapdf-lib/issues/16).

### Added

- **`crates/core/src/sign/verify.rs`** — a detached-CMS verifier built on the
  existing RustCrypto `cms`/`x509-cert`/`rsa` stack: recompute the `/ByteRange`
  SHA-256, check the CMS `messageDigest`, and validate the SignerInfo RSA
  signature under the signer certificate's key (trimming the `/Contents`
  zero-padding to the actual DER element first).
- **`Document::signatures() -> Vec<SignatureInfo>`** — list every `/Sig` field's
  `/V` with its metadata and `/ByteRange`. `gp_signatures_json` / `doc.signatures`.
- **`Document::verify_signatures(&pdf_bytes) -> Vec<SignatureReport>`** — verify
  each signature against the original bytes: `byte_range_ok`, `digest_ok`
  (integrity), `signature_ok`, `covers_whole_document`, signer CN, cert count,
  algorithm. `gp_verify_signatures` / `doc.verifySignatures`.
- **`Document::sign_certify(&Signer, name, reason, date, docmdp_p)`** — produce a
  **certified** PDF: a certifying signature plus the catalog `/Perms /DocMDP` and
  a `/Reference` DocMDP transform with the permission level (1 = no changes,
  2 = fill + sign, 3 = also annotate). `gp_sign_certify` / `doc.certify`.

### Notes

- **RSA + SHA-256** (what this engine produces) is verified; other algorithms are
  reported as `unsupported`. Live OCSP/CRL revocation checking, full
  chain-to-trusted-root validation, FieldMDP field-locking and ECDSA are out of
  scope (they need a trust store / network / clock the engine doesn't have).
  Verification needs the **original file bytes** (the `Document` doesn't retain
  them).

## [0.79.0] - 2026-06-24

Closes the interactive-forms gaps (ISO 32000-1 §12.7): signature fields,
field-level JavaScript, calculation order, field deletion and appearance
regeneration. Resolves
[#15](https://github.com/qrcommunication/gigapdf-lib/issues/15).

### Added

- **`Document::add_signature_field(page, name, rect, &style)`.** Lay out a
  *visible* signature field (`/FT /Sig`) the PAdES signing stack can target, and
  flag the AcroForm `/SigFlags`. `gp_add_signature_field` / `doc.addSignatureField`.
- **`Document::set_field_action(name, FieldTrigger, js)`.** Field-level
  JavaScript in a field's `/AA` for the `Keystroke` (`K`), `Format` (`F`),
  `Validate` (`V`) and `Calculate` (`C`) triggers — input masks, formatting,
  validation and computed values. `gp_set_field_script` / `doc.setFieldScript`.
- **`Document::set_calculation_order(&[name])`.** The AcroForm `/CO` calculation
  order. `gp_set_calculation_order` / `doc.setCalculationOrder`.
- **`Document::remove_field(name)`.** Delete a field from `/Fields`, `/CO` and
  every page's `/Annots`. `gp_remove_field` / `doc.removeField`.
- **`Document::regenerate_field_appearance(name)`.** Rebuild a field's `/AP` from
  its current value and style (text / choice / checkbox) after a programmatic
  value change. `gp_regenerate_field_appearance` / `doc.regenerateFieldAppearance`.
- New `form::FieldTrigger` enum (with `pdf_key` / `from_name`).

### Notes

- Hierarchical fields (`/Kids`, dotted names), per-field `/DR` font resources and
  XFA remain out of scope (XFA intentionally). `regenerate_field_appearance`
  returns `false` for a `/Kids` parent (e.g. a radio group).

## [0.78.0] - 2026-06-24

A single, shared **action & destination model** (ISO 32000-1 §12.6 / §12.3.2)
reused by links, the document open-action and outline bookmarks. Resolves
[#14](https://github.com/qrcommunication/gigapdf-lib/issues/14).

### Added

- **`Action` and `Destination` model (`crates/core/src/action`).** An `Action`
  enum — `GoTo`, `GoToR` (remote), `Uri`, `Named` (NextPage/PrevPage/FirstPage/
  LastPage), `Launch`, `JavaScript`, `SubmitForm`, `ResetForm` — and a
  `Destination` enum with **every** fit mode (`XYZ`, `Fit`, `FitH`, `FitV`,
  `FitR`, `FitB`, `FitBH`, `FitBV`, plus named destinations). `Action::from_json`
  parses the SDK's tagged-object shape; `build_dict` emits the PDF `/A` dictionary
  (and the `/D` array/name).
- **`Document::add_link(page, rect, &Action)`.** A general link carrying any
  action and any destination fit mode (previously links were URI- and
  page-jump-only). `gp_add_link` / `doc.addLink`.
- **`Document::set_open_action(&Action)`.** The document `/OpenAction`, performed
  when the file is opened. `gp_set_open_action` / `doc.setOpenAction`.
- **`Document::remove_link(page, index)`.** Remove the *n*-th `/Link` annotation
  on a page (links counted in order, other annotations untouched).
  `gp_remove_link` / `doc.removeLink`.
- **`Document::set_bookmarks(&[Bookmark])`.** Replace the outline with bookmarks
  that carry **any** `Action` (a `GoTo` becomes a `/Dest`, anything else an
  `/A`). `gp_set_bookmarks` / `doc.setBookmarks`. `set_outline` now delegates to
  it (each `page` → a `/Fit` GoTo), so its behaviour is unchanged.

### Notes

- Form-field widget actions are deferred to the forms issue (#15); they reuse
  this same `Action` model.

## [0.77.0] - 2026-06-24

Adds the missing geometric annotation subtypes and appearance regeneration.
Resolves [#13](https://github.com/qrcommunication/gigapdf-lib/issues/13).

### Added

- **Circle / Polygon / PolyLine / Caret annotations (core + WASM + SDK).**
  `add_circle_annotation`, `add_polygon_annotation`, `add_polyline_annotation` and
  `add_caret_annotation` create the geometric annotation subtypes with border
  width and interior colour (`/IC`), each with a generated `/AP` appearance stream.
  Exposed as `gp_add_circle_annot` / `gp_add_polygon_annot` /
  `gp_add_polyline_annot` / `gp_add_caret_annot` (WASM) and
  `doc.addCircleAnnotation` / `addPolygonAnnotation` / `addPolylineAnnotation` /
  `addCaretAnnotation` (SDK).
- **`regenerate_appearance(page, index)`.** Rebuild an existing annotation's
  `/AP /N` appearance from its stored geometry/style (after editing its colour,
  border or geometry), leaving every other key untouched — for Square, Circle,
  Line, Polygon, PolyLine, Highlight, Underline, StrikeOut, Ink and Caret.
  `gp_regenerate_appearance` / `doc.regenerateAppearance`.

### Notes

- Annotation **actions** (`/A`) are deferred to the action-model issue (#14);
  Sound/Screen/RichMedia subtypes remain out of scope (low priority). A **text
  watermark** already shipped as `add_watermark` / `doc.addWatermark` (positioned,
  with colour/opacity/rotation), so no new watermark API was added.

## [0.76.0] - 2026-06-24

General document metadata: read/write the catalog `/Metadata` **XMP** packet and
set the typed Info-dictionary fields, keeping `/Info` and XMP in sync. Resolves
[#7](https://github.com/qrcommunication/gigapdf-lib/issues/7).

### Added

- **XMP + typed Info metadata (core + WASM + SDK).** `Document::xmp()` reads the
  catalog `/Metadata` XMP packet (decoded) and `Document::set_xmp(&[u8])`
  replaces/creates it (stored uncompressed). `Document::set_info(&InfoFields)`
  writes the standard fields (Title/Author/Subject/Keywords/Creator/Producer/
  CreationDate/ModDate) to **both** the `/Info` dictionary **and** a regenerated
  XMP packet (`dc:`/`xmp:`/`pdf:` namespaces, PDF dates → ISO 8601), as a partial
  merge; `Document::info_fields()` reads them back, and `InfoFields::from_json`
  parses the SDK object. Exposed as `gp_get_xmp` / `gp_set_xmp` /
  `gp_set_info_json` (WASM) and `doc.getXmp()` / `doc.setXmp()` / `doc.setInfo()`
  (SDK), with the new `InfoFields` type. The existing single-key
  `set_metadata(key, value)` is unchanged (Info only).

### Changed

- The internal JSON object reader (`ObjReader`) is now `pub(crate)` so metadata
  (and future config parsers) can reuse it instead of duplicating a parser.

## [0.75.0] - 2026-06-24

Embedded file attachments become **writable** — add/replace/remove document-level
files, anchor `FileAttachment` annotations, and link **associated files** (`/AF`,
PDF/A-3) for hybrid e-invoices (Factur-X / ZUGFeRD / Order-X). Resolves
[#9](https://github.com/qrcommunication/gigapdf-lib/issues/9).

### Added

- **Attachment write API (core + WASM + SDK).** `Document::add_attachment(name,
  bytes, mime, desc)` embeds a file in `/Names /EmbeddedFiles` (FlateDecode-
  compressed; re-using a name replaces it); `Document::remove_attachment(name)`
  drops it (and its `/AF` link), returning whether one was removed;
  `Document::add_associated_file(name, bytes, mime, desc, relationship)` adds the
  file as an **associated file** — its filespec carries `/AFRelationship` and is
  linked from the catalog `/AF` array (the Factur-X/ZUGFeRD invoice-XML mechanism);
  `Document::add_file_attachment_annot(page, rect, name, icon)` anchors a visible
  `FileAttachment` annotation to an embedded file. Exposed as `gp_add_attachment` /
  `gp_add_associated_file` / `gp_remove_attachment` / `gp_add_file_attachment_annot`
  (WASM) and `doc.addAttachment` / `doc.addAssociatedFile` / `doc.removeAttachment`
  / `doc.addFileAttachmentAnnot` (SDK), with the new `AfRelationship` type. Sibling
  `/Names` subtrees (`/Dests`, `/JavaScript`, …) are preserved when the embedded-
  files tree is rewritten.

## [0.74.0] - 2026-06-24

Adds **page labels** (`/PageLabels`) — reading, authoring and resolving the
page-numbering schemes (roman front matter, prefixed appendices, …) that viewers
show in the page navigator. Resolves
[#8](https://github.com/qrcommunication/gigapdf-lib/issues/8).

### Added

- **Page labels — read/write + resolve (core + WASM + SDK).**
  `Document::page_labels()` reads the `/PageLabels` number tree (ISO 32000-1
  §12.4.2; traverses `/Nums` leaves and `/Kids` intermediates) into a sorted
  `Vec<PageLabelRange>` (`start_page` 1-based, `style`, `prefix`, `start_number`);
  `Document::set_page_labels(&[…])` writes a flat-`/Nums` tree (an empty slice
  clears `/PageLabels`); `Document::page_label(page)` formats the viewer-visible
  string (decimal, lower/upper roman, the repeating `a…z, aa…` letter scheme,
  with prefix and `/St` offset), falling back to the decimal page number outside
  any range. Exposed as `gp_page_labels_json` / `gp_set_page_labels` /
  `gp_page_label` (WASM) and `doc.getPageLabels()` / `doc.setPageLabels([…])` /
  `doc.pageLabel(n)` (SDK), with the `PageLabelRange` / `PageLabelStyle` types.

## [0.73.0] - 2026-06-24

Print-production release: the engine now reads and writes **all five ISO 32000-1
page boundary boxes** (MediaBox/CropBox/BleedBox/TrimBox/ArtBox), the prerequisite
for PDF/X export and any commercial-print pipeline (imposition, bleed, finished-size
trimming). Resolves [#6](https://github.com/qrcommunication/gigapdf-lib/issues/6).

### Added

- **Page boxes — full read/write (core + WASM + SDK).** `Document::page_boxes(page)`
  returns a `PageBoxes` with every box (`media`/`crop`/`bleed`/`trim`/`art`) as
  `[x0, y0, x1, y1]` points, with **ISO 32000-1 §14.11.2 inheritance and the per-box
  default chain applied** (CropBox→MediaBox; Bleed/Trim/Art→CropBox; MediaBox &
  CropBox inherited from an ancestor `/Pages` node), plus a `declared` set flagging
  which boxes are explicitly on the page vs inherited/defaulted.
  `Document::set_page_box(page, kind, [x0,y0,x1,y1])` writes one box, normalises the
  rectangle (reversed corners accepted) and **preserves sibling boxes** — boxes are
  no longer dropped on round-trip. Exposed as `gp_page_boxes_json` /
  `gp_set_page_box` (WASM, `kind` 0=media 1=crop 2=bleed 3=trim 4=art) and
  `doc.getPageBoxes(n)` / `doc.setPageBox(n, "trim", { x, y, w, h })` (SDK), with the
  `PAGE_BOX_KINDS` / `PageBoxKind` / `PageBoxes` types.
- **`EngineError::InvalidArgument`.** A new error variant for malformed caller
  arguments (e.g. a degenerate page-box rectangle or an unknown enum discriminant).

## [0.68.0] - 2026-06-23

Format-reach + import/render fidelity release: the unified model now exports
**Markdown / CSV / EPUB** end to end, Office/ODF import preserves far more
structure, the HTML→PDF renderer gains the remaining common CSS, and several
image-codec and rendering bugs are fixed.

### Added

- **Markdown / CSV / EPUB model export — FFI + SDK.** The unified editable model
  can now be raised to **Markdown** (`gp_model_to_md` / `modelToMd`), **CSV**
  (RFC 4180, `gp_model_to_csv` / `modelToCsv`) and **EPUB 3**
  (`gp_model_to_epub` / `modelToEpub`), alongside the existing
  DOCX/XLSX/PPTX/ODT/ODS/ODP/PDF/HTML/RTF targets.
- **Complete Markdown modelling.** `CodeBlock`, `Blockquote` and
  `HorizontalRule` are first-class in the model, giving a full Markdown
  round-trip — headings, runs, links, images, nested lists, GFM tables, code
  blocks, block-quotes, horizontal rules, footnotes and front-matter — rendered
  and exported consistently across every format.
- **Office / ODF import fidelity.** DOCX/XLSX/PPTX and **ODF (`.odt`/`.ods`/
  `.odp`)** import now preserves **images, hyperlinks, strikethrough, text
  highlighting, spreadsheet formulas, grouped shapes, charts, SmartArt text and
  master/layout (theme) inheritance**.
- **HTML / CSS → PDF — remaining common CSS.** **Radial** and **conic**
  gradients, **`font-weight` 100–900**, **`box-shadow`** (blur), **elliptical
  `border-radius`**, dashed/dotted borders, **`linear-gradient`** and
  **`position: sticky`**. (Earlier in the cycle: linear gradient + box-shadow +
  border-radius + dashed/dotted borders.)
- **OpenType text shaping.** **GPOS** mark positioning, **GSUB** contextual
  substitution, script selection and **Arabic joining**, wired into the text
  render path for complex scripts only (Latin output is byte-for-byte
  unchanged).
- **Image codecs.** **SVG `<text>`** rendering and **GIF multi-frame** decoding.
- **Run highlight.** Character-level `background` (`CharStyle.background`) is now
  painted and emitted across the HTML, PDF and Office paths.
- **`set_text_run_style`.** Run-level style bake exposed in core, FFI and the
  SDK.
- **Mermaid flowchart renderer.** `graph TD/LR` with node shapes, typed edges
  and arrowheads, laid out (Sugiyama) into PDF vectors in the HTML engine.

### Fixed

- **AVIF multi-tile decode (corrupt images > 9.4 MP).** A multi-tile AVIF
  (`tile_cols_log2 > 0 || tile_rows_log2 > 0`) was decoded as a single
  continuous tile, garbling pixels. The AV1 spec **forces** multi-tile on any
  picture above 4096×2304 px (~9.4 MP), so essentially every modern phone /
  camera AVIF was silently corrupted. Each tile is now decoded as the
  independent entropy + prediction unit it is (per-tile `Msac` + CDF set, tile
  MI bounds for partition geometry, intra prediction stopped at the tile
  boundary, neighbour contexts reset per tile); the in-loop filters still run
  once over the full frame. Single-tile (≤ 9.4 MP) and all existing fixtures
  are **byte-for-byte unchanged**; validated bit-exact against `dav1d`.
- **WebP lossless (VP8L).** Lossless transforms + meta-Huffman decoding — real
  `cwebp`/libwebp lossless images now decode correctly.

### Changed

- **Non-Device colorspaces resolved.** **Pattern** fills, and `Separation` /
  `ICCBased` colours used in content streams, are now unified through the raster
  colour resolver (consistent with the rasterizer) instead of falling back to a
  device default.
- **Docs honesty.** README corrected: the engine is **near-zero-dependency**
  (hand-written PDF/render/conversion core; **RustCrypto** for standardized
  crypto/signatures; **Boa** for the JS engine — the earlier from-scratch JS
  interpreter is gone), the suite is **1198 tests** (was “284”), and the
  released `.wasm` is **~5.6 MB** (was “~540 KB”, before Boa was bundled).

### Tests

- **1198** `gigapdf-core` tests green, `clippy` clean. New coverage for AVIF
  multi-tile (bit-exact vs `dav1d`, incl. loop filtering across tile seams),
  VP8L, SVG `<text>`, GIF multi-frame, OpenType shaping, the Markdown/CSV/EPUB
  exporters, Office/ODF import fidelity, and the new CSS.

## [0.67.0] - 2026-06-23

See [`sdk/CHANGELOG.md`](sdk/CHANGELOG.md) for 0.67.0 and earlier.
