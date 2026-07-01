# Changelog

All notable changes to `@qrcommunication/gigapdf-lib` are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.113.0] - 2026-07-01

### Added — every form-field widget placement (`FieldInfo.widgets`)

`fields()` now returns, for each AcroForm field, a **`widgets`** array listing
EVERY on-page widget of that field — not just the first. A field can have several:
the same field repeated on a duplicate page (an official form's carbon copy — fill
it once and it shows on both pages), or one widget per button of a radio group.
Each `WidgetPlacement` carries its `page`, its top-left `bounds`, and — for a button
— its on-state `export` (which radio button it is / the value stored when selected).
The flat `page`/`bounds` stay the first widget for backward compatibility.

Redundant or near-coincident widgets (a merged field also listed under `/Kids`, a
duplicate `/Kids` ref) are de-duplicated by (page, ~2pt bounds). A host renders one
field overlay per widget, so nothing is missing on page 2 and every radio button is
placed with the correct checked state.

## [0.112.0] - 2026-07-01

### Fixed — per-word tiling for space-justified & `Td`-positioned runs (footers now overlay-faithful)

`run_layout` previously split a run into positioned `segments` only at large `TJ`
kern jumps. A run justified by **word spacing** (`Tw`) or **wide space glyphs** —
or one whose words are placed by separate `Td` moves with wide leading gaps (a
CERFA-style legal footer) — stayed a single long box, so a host painting one
editable box per run rendered its glyphs at the browser's natural (non-justified)
advance: the text drifted and collided with the neighbouring runs.

Now **any positioned run** — one carrying a real `TJ` jump **or** a space wider
than half an em — is split at **word boundaries**, each word tiling at its exact
rasterizer pen `x`. Every space (a normal one or a wide justification gap) tiles
as an inter-segment **gap**, never inside a word box, and a **leading** gap is
folded so a lone word after it (e.g. a footer's `" financière"`) lands at its true
pen `x` instead of drifting left. Single-word positioned runs now emit their one
fragment too (the run box alone would sit at the pre-gap pen). Whitespace is
detected by the decoded glyph, so a font whose space isn't byte `0x20` still tiles
per word. Plain natural-flow runs (no jump, only normal spaces) stay a single
inline-editable box — byte-identical.

Result: a justified small-print legal footer reproduces the rasterizer
word-for-word in an editable overlay.

## [0.111.0] - 2026-07-01

### Added — per-run text `segments` for pixel-faithful editable overlays (`TextElementInfo.segments`)

`page_text_elements` / `textElements(page)` now return, for each **justified or
per-glyph-positioned run** (a legal footer, a spread-out table cell), its
visually contiguous **`segments`** — each with its own text and page-space box.
A run drawn as ONE `Tj`/`TJ` but with large internal `TJ` position jumps used to
collapse those jumps to literal spaces, so a host that painted one editable box
per run tiled it wrong (fragments overlapped / drifted). Each segment is now
positioned from the **same pen walk the rasterizer uses** — real glyph widths
(`/Widths`·`/W`) plus `Tc` (char spacing), `Tw` (word spacing), `Tz` (horizontal
scale) and `TJ` kerns — so a host paints one box per fragment, 1:1 with the
render, while still editing the whole run via its `index`. `segments` is **empty**
for a plain run (its own `x`/`y`/`width` already position it), so the common case
is unchanged.

Internally the content-stream extractor now tracks `Tc`/`Tw`/`Tz` and derives
each horizontal LTR run's advance from that exact pen walk (a single source of
truth shared by the run's own box, the next run's start position, and the
segments), fixing sub-point inter-run drift on spaced/scaled text. Byte-identical
for content without spacing operators or internal jumps.

## [0.110.4] - 2026-06-30

### Fixed — repacked CIDFont subset web-font cmap (`extractWebFont` no longer garbles labels)

- `extract_font_for_web` now **rebuilds** the served `cmap` for a Type0/CIDFontType2
  subset (e.g. `ECYBWA+TimesNewRoman`) from the **authoritative** `code → CID → gid`
  mapping (the same one the rasterizer uses) + the font's `/ToUnicode` (ASCII gaps
  affine-filled via `infer_ascii_gaps`) + the embedded program's `gid → Unicode` —
  instead of trusting the subset's **stale** embedded `(3,1)` cmap, which maps each
  Unicode to the original, now-blanked glyph id (the real outlines sit at the gids
  reached only by `code → CID → gid`). A repacked subset previously rendered its
  text as garbage in a host browser (e.g. "Nom et adresse de l'organisme" →
  "Nç ê Dçê D ê"); the cmap is now rebuilt **even when** the subset already has a
  Unicode cmap, and only the embedded cmap is kept when the rebuild is empty. PDF
  rasterisation (`code → CID → gid`) was always correct. Non-repacked subsets keep
  their correct cmap — no regression.
- New `TrueTypeFont::post_name_to_gid()` / `glyph_is_empty()` and
  `encoding::standard_mac_glyph_name()` back an authoritative TrueType `post`
  name→gid resolver for simple-font subsets. Test:
  `repacked_cid_subset_serves_real_glyphs` (skips unless `GIGAPDF_REPACKED_CID_FIXTURE`
  points at a repacked-CID PDF — the reproducing CERFA carries PII and is not committed).
- Known limitation: a few non-ASCII Latin accents (`è`/`à`/`ç`) absent from a
  subset's `/ToUnicode` and off the affine ASCII layout may stay unmapped; `é` and
  all ASCII render correctly.

## [0.110.3] - 2026-06-30

### Fixed — CFF advance widths (`extractWebFont` no longer jams text in the browser)

- `CffFont::advance_width` — and therefore the OpenType `hmtx` that `cff_to_otf`
  builds for `extractWebFont` — now reads the CFF Private DICT **`nominalWidthX`**
  (op 21) and **`defaultWidthX`** (op 20) instead of using `units_per_em * 0.5`
  as a one-size base for every glyph. A CFF/Type1 subset of a standard base font
  (e.g. **`Times-Bold`**) previously got a near-zero space advance
  (`500 + (250 − 718) = 32`) and wrong letter advances, so a host rendering the
  extracted web font in a browser **jammed every word together** (the CERFA
  "DEMANDEDERATTACHEMENT…" garbling). PDF rasterisation (which uses the PDF
  `/Widths` array) was unaffected — only the browser-loadable web font's metrics
  were wrong; `extractWebFont` (0.110.0) was the first consumer of `advance_width`
  for actual rendering, which is why the latent bug only surfaced then.
- `advance = nominalWidthX + operand` when a charstring carries a width operand,
  `defaultWidthX` otherwise; per-FD for CID-keyed / CFF2 fonts (keyed by FDSelect).
  Regression test: `cff_width_distinguishes_nominal_from_default_width_x`.

## [0.110.2] - 2026-06-30

### Added — `ocr_serve`: persistent host-side OCR microservice

- New `gigapdf-ocr-rten` binary **`ocr_serve`** + `OcrEngine::rec_names()`. It loads the
  PaddleOCR-on-RTen models **once** at boot and serves recognition over a minimal local
  HTTP/1.1 endpoint (zero web-framework dependency — `std::net` only): `GET /health`
  (`{ok,recCount,languages}`) and `POST /ocr` (PNG body, optional `X-Ocr-Model` header for a
  forced recognizer; auto per-line script selection otherwise) → NDJSON words in image pixel
  space (`{text,x,y,w,h,confidence,model}`). This is the "host-side endpoint" the lean wasm
  client calls — amortizing the multi-hundred-MB model load across requests instead of per call.
  The `ocr_serve` binary and the assembled `.rten` model set are distributed as GitHub
  **release assets** for host deployment (not part of the npm package).

### Fixed — npm package no longer bundles the legacy `.gpocr` models

- `0.110.1` accidentally bundled the 22 legacy `.gpocr` OCR weights into the npm tarball. Those
  belong to the **removed** client-side recognizer (≤0.63.x); `0.110.x` does OCR **host-side**
  via `gigapdf-ocr-rten` (RTen `.rten` models), so the `.gpocr` files are dead weight in the
  package. The release pipeline no longer fetches/bundles them — the npm package is lean again.

## [0.110.1] - 2026-06-30

### Fixed — releases now ship the OCR `.gpocr` models again

- The 22 per-script line-OCR model weights (`models/ocr_*.gpocr`, 9.6 MB) are
  **git-ignored** (`sdk/.gitignore: models/`), so a clean CI checkout had none —
  every CI-published version since the ignore (e.g. `0.109.1`, `0.110.0`) shipped
  **zero** OCR models, silently degrading runtime OCR to the mono-glyph
  classifier (`0.63.0` still had them because it predates the gap). The release
  workflow now **fetches the models from a durable GitHub Release asset
  (`ocr-models-v1`), verifies their SHA-256, and stages them** before
  `build-wasm.sh`, so every published package carries the full model set again —
  without committing 9.6 MB of weights to the repo. No engine/API change.

## [0.110.0] - 2026-06-30

### Added — `extractWebFont`: serve a document's own embedded fonts to a browser

- New engine method **`extractWebFont(name)`** (FFI `gp_extract_web_font`) returns
  a PDF's **own embedded font** as a **browser-loadable sfnt** for a `@font-face`
  overlay-text layer — keeping the document's **original glyphs**, never a
  substitute. It complements `extractFont` (which returns the raw program for
  re-embedding): where `extractFont` hands back bytes a browser's `FontFace`/OTS
  routinely rejects (a bare CFF, or a `cmap`-less TrueType subset),
  `extractWebFont` repairs them:
  - **CFF (Type1C)** → wrapped to OpenType (`OTTO`) with a `cmap` built from the
    PDF's `code → Unicode` decode mapping (the same one text extraction uses, so
    it resolves subset glyphs whose `/Differences` carries opaque `gNN` names and
    whose `/ToUnicode` is partial — the exact case that otherwise renders the
    wrong letters).
  - **TrueType** → passed through when it already maps Unicode; otherwise a `cmap`
    is synthesised (`code → Unicode` from the decode path, `code → gid` borrowed
    from a sibling's `cmap` for full-glyph-order subsets, else the `code == gid`
    convention) and the OTS-mandatory `OS/2`/`name`/`post` are stubbed in — the
    original `glyf` is kept verbatim.
  - Face selection prefers an **exact raw `/BaseFont`** match (subset prefix
    included), so a host that serves many disjoint subsets of one family gets
    *that* subset's own glyphs per run.
- New internal helper `cff_to_otf::repair_sfnt_with_cmap` (injects a Unicode
  `cmap` + the OTS-required tables around existing glyph data) and a hardened
  `build_cmap` (now sorts/dedupes segments, so a synthesised format-4 table is
  accepted by strict shapers — FreeType/HarfBuzz).
- Tests: `repair_injects_cmap_and_required_tables_keeping_originals` (the repair
  keeps the original tables and adds a working `cmap`).

### Added — bidirectional conversion symmetry: model importers for RTF, TXT and images

- New engine methods `rtfToModel(rtf)`, `txtToModel(text)` and
  `imageToModel(image)` complete the **lower-any-format-into-the-model** matrix.
  The core already exposed `rtf_to_model` / `txt_to_model` / `image_to_model`,
  but they had **no WASM/SDK binding** — so `modelToRtf` could write RTF yet
  nothing could read RTF *into* the model, and likewise for plain text and raster
  images. The three new FFI entry points (`gp_model_from_rtf` /
  `gp_model_from_txt` / `gp_model_from_image`) and their TypeScript wrappers close
  that asymmetry, matching the existing `officeToModel` / `htmlToModel` /
  `mdToModel` / `csvToModel`. `imageToModel` returns `null` on non-image bytes;
  `rtfToModel` routes through the rich RTF parser (run styling, tables, `\pict`
  images, `\field` links).
- Tests: `test/model_import.test.ts` — 7 round-trip cases through the production
  WASM (`txtToModel`→`modelToPdf`, `rtfToModel`→`modelToRtf`,
  `imageToModel`→`modelToPdf`, plus the non-image `null` guard).

## [0.109.1] - 2026-06-30

### Fixed — image-format documentation accuracy

- `addImage`, `replaceImage` and the image **watermark** path already accept the
  full raster set — **PNG, JPEG, WebP, GIF, TIFF and AVIF** — since 0.109 (every
  one routes through the shared `prepare_image` / `embeddable_image` decoders).
  But the core and WASM docstrings, the SDK `replaceImage` docs, and the
  `unsupported image` error messages still claimed **PNG/JPEG only** (or omitted
  TIFF). Corrected every stale claim so the documented capability matches the
  implementation. **No behaviour change** — purely documentation and error-text.

### Added

- Regression test `replace_image_accepts_every_raster_format_not_just_png_jpeg`:
  proves `replaceImage` swaps a non-PNG/JPEG raster (WebP) in place while keeping
  the image's object number and every `/Do` reference intact.

## [0.109.0] - 2026-06-30

### Added — image conversion completeness

- `imageToPdf` and direct image embedding now accept the full format set
  uniformly: **PNG, JPEG, GIF, WebP, AVIF and TIFF**. AVIF works for direct
  embedding (`addImage` / watermark) and TIFF works for `imageToPdf` — closing
  the gaps where one entry point supported a format the other rejected.
- New `image_conversion.test.ts` suite exercises every format → PDF through the
  bundled production WASM binary (PNG, WebP, AVIF, TIFF), the non-image rejection
  path, and `addImage`.

### Added — conversion fidelity (both directions)

- HTML `letter-spacing`, `visibility:hidden`/`display:none`, `<sup>`/`<sub>` and
  `<colgroup><col width>`; Markdown inline `<sup>`/`<sub>`; ODT/DOCX tab stops;
  RTF `\page` page breaks — all now round-trip through the model. See the engine
  [`CHANGELOG.md`](../CHANGELOG.md) for the full list.

## [0.107.1] - 2026-06-27

### Fixed

- **ODT/DOCX import run coalescing** — adjacent runs with visually-identical
  styles now merge in both the DOCX and ODT importers, fixing the "every word is
  a separate run" problem in imported Office documents.

## [0.107.0] - 2026-06-27

### Changed — PDF → Office conversion quality overhaul

- **All PDF→Office/HTML exports now produce flowing, editable documents** —
  `toDocx()`, `toOdt()`, `toPptx()`, `toOdp()`, `toHtml()` route through the
  reconstructed semantic model (real `<w:p>` paragraphs, `<w:tbl>` tables, list
  numbering, section geometry) instead of fixed-position VML text boxes.

- **Run coalescing** — adjacent text fragments with the same font/style are
  merged into clean contiguous spans, fixing the "every word is a separate run"
  problem that made exported Word documents uneditable.

- **Cross-page paragraph merging** — paragraphs split across page boundaries are
  stitched back together.

- **Page margins, multi-column layout, hard page breaks, and extended metadata**
  (creation/modification dates, creator, producer) are now recovered from the
  PDF and emitted in the Office output.

### Removed

- Dead VML-based export path (`office::to_docx/odt/pptx/odp`, `web::to_html`)
  has been deleted — the model-based exporters are now the only path.

## [0.106.0] - 2026-06-27

### Fixed

- **`textElements` now reports the scope-correct font for text inside form
  XObjects.** A run drawn through a reusable form XObject (CERFA / invoice /
  letterhead templates) was styled against the page's `/Font` table instead of the
  form's own, so its `fontFamily`/`bold`/`italic` collapsed to "Helvetica" regular
  — losing the embedded face and weight, and misplacing the editor overlay (wrong
  metrics). Each run is now styled against the font table of its own scope.

### Added

- **`TextElementInfo.baseFont`** — the run's raw `/BaseFont` with the subset prefix
  kept (e.g. `"ABCDEF+TimesNewRomanPSMT"`), resolved against the run's own scope, so
  a host editor can target the exact embedded subset rather than only the collapsed
  `fontFamily`. Empty when the font carried no `/BaseFont` (e.g. a Type3 font).

## [0.105.0] - 2026-06-26

### Fixed

- **Text extraction no longer corrupts subset fonts with repacked `/Differences`
  glyphs.** When a simple font assigns real glyphs to ASCII-punctuation codes via
  `/Encoding`+`/Differences` (`gNN` names) and ships a broken identity-aligned
  `/ToUnicode` omitting those codes, the ASCII gap-filler used to invent a
  `code → chr(code)` mapping that masked the real glyph (consulted first) — e.g.
  `extractText` returned `'&are%ts'` for `'parents'`. The filler now defers to the
  font's `/Encoding`+`/Differences` for those codes. Extraction-only fix; rendering
  was already correct, and composite (Type0) fonts are unaffected.

### Added

- **Baked running header/footer excluded from the editable views.** A header/footer
  baked with `setHeader`/`setFooter` (tagged `/GPHF`) is no longer returned by
  `elements`, `textRuns` or `blocks` — re-opening a header-baked document never
  turns the header into editable body content nor desyncs `replaceText` /
  `transformElement` / `removeElement` / `reorderElement` indices. `headerFooter()`
  still recovers it.
- **`renderPageExcludingMarkedContent(page, scale?, skipText?, marker?)`** — render
  a page in one pass with a baked marked-content band (default `"GPHF"` = the
  running header/footer) omitted, so the band shows in the raster but never doubles
  against an editable overlay; with `skipText` it is the editor's text-free
  background minus the band.
- **`setEditorMeta(json)` / `editorMeta()`** — store/read an opaque JSON
  editor-metadata sidecar (catalog `/GigaPDF /EditorMeta`, compressed; ignored by
  standard readers; survives save/open).
- **`setEditorMargins(page, m)` / `editorMargins(page)`** — per-page editor display
  margins persisted in the sidecar (under `margins`), **without** touching
  `/CropBox` (distinct from `setPageMargins`, the real recrop).
- **`setRunningHeaderFooter(def, opts?)` / `runningHeaderFooter()`** — a rich,
  Word-like running header/footer (types `RunningHeaderFooter`, `HFZone`, `HFItem`,
  `HFAlign`). The `def` is the source of truth (stored in the editor-meta sidecar
  under `headerFooter`); its visible `/GPHF` band is regenerated per page, so it is
  excluded from `elements`/`textRuns` and masked by
  `renderPageExcludingMarkedContent`. A zone (`default`/`firstPage`/`evenPage`/`oddPage`)
  holds `HFItem`s of `type: "text"` (drawn in an **embedded** font — the item's
  `fontRef`, else the bundled OFL face, never base-14) or `type: "image"` (pixels
  supplied via `opts.images`, an iterable of `[imageId, bytes]` — a `Map` works).
  Tokens `{{page}}`, `{{pages}}`, `{{date}}` (pass `opts.date`) and `{{title}}` are
  substituted at bake time; re-baking is idempotent. The flat
  `setHeader`/`setFooter` API is unchanged.

## [0.104.0] - 2026-06-25

Four issue lists closed — [#75], [#76], [#77], [#78]: PDF→model reconstruction,
annotation appearances, serialization (linearization + PDF 2.0), and RTF-export
fidelity — plus the previously-unreleased PDF/A archival conformance
([veraPDF](https://verapdf.org/)-validated, ISO 19005).

### Added

- **PDF version selector for compact / linearized output ([#77]).** `saveOptimized`
  accepts `{ version?: "1.7" | "2.0" }`, and `toLinearized(version)` /
  `saveLinearized(version)` take the same; new exported type
  `PdfVersion = "1.7" | "2.0"` (default `"1.7"`). The compact and Fast-Web-View
  writers can now declare a PDF 2.0 header.
- **Selectable PDF/A conformance level.** `toPdfA(level?)` accepts `"pdfa-1b"` ·
  `"pdfa-1a"` · `"pdfa-2b"` (default, backward-compatible) · `"pdfa-2u"` ·
  `"pdfa-2a"` · `"pdfa-3b"`; core `to_pdfa_level(level)` + `PdfaLevel` enum.
  Levels **1b/1a** emit a `%PDF-1.4` header (ISO 19005-1), the others `%PDF-1.7`;
  **2u** keeps every glyph mapped to Unicode (`/ToUnicode`); **3b** permits
  embedded files (`/AF`); **1a/2a** are level A (Tagged PDF, see below). All six
  pass veraPDF (`isCompliant = true`).
- **Tagged PDF — level A conformance (`pdfa-1a`, `pdfa-2a`).** ISO 19005 level A
  adds the logical-structure tree the engine already infers. On a level-A export
  the catalog gains `/MarkInfo << /Marked true >>`, `/Lang`, and a `/StructTreeRoot`
  whose tree is derived from the document structure (`Document` → `P` / `H1`…`H6`
  / `Table` / `TR` / `TH` / `TD` / `L` / `LI` / `LBody` / `Figure`), backed by a
  `/ParentTree`. Content is marked up in the page streams — each tagged run wrapped
  as `/<role> << /MCID n >> BDC … EMC`, non-tagged marks emitted as `/Artifact` —
  so the structure is render-neutral (pixels are identical to the untagged
  export). Validated `isCompliant = true` against veraPDF for **both** 2a and 1a.
- **Conformance CI gate** (`conformance.yml`). Generates fixtures from the SDK
  and validates them against reference validators only — veraPDF for PDF/A (the
  six levels), qpdf for PDF, structural OPC/ODF checks for the Office/ODF exports
  — failing the build on any regression.

### Fixed

- **PDF/A-2b trailer `/ID`** (ISO 19005-2 cl. 6.1.3). `to_pdfa` set a
  deterministic `/ID`, but `serialize::to_pdf` rebuilt the trailer and dropped
  it; the classic serializer now preserves `/ID`, matching the compressed and
  encrypted serializers.
- **PDF/A appearance & graphics-state sanitization.** On PDF/A export, annotation
  appearance dictionaries are reduced to `/N` only (cl. 6.3.3), the `/TR`
  transfer-function key is removed from ExtGState (cl. 6.2.5), and incomplete
  `/CIDSet` entries are dropped (cl. 6.2.11.4.2) — all render-neutral.

### Improved

- **Serialization — linearization, incremental updates & versioned output ([#77]).**
  The Fast-Web-View (linearized) hint stream now carries the true per-page
  content-stream length and adds the document-outline (`/O`) and thread-information
  (`/A`) hint tables (ISO 32000-1 Annex F.3.3); incremental updates (used by signing's
  DSS / document-timestamp appends) now match the base file's cross-reference form — an
  xref **stream** when the base uses one. `saveOptimized()` now emits a `%PDF-1.7`
  banner by default (was 1.5).
- **Annotations — appearance fidelity ([#76]).** `regenerateAppearance()` now rebuilds
  the appearance for FreeText, Stamp, Text, Link, Squiggly and FileAttachment, plus
  Redaction and placeholder 3D / RichMedia / Movie / Sound annotations (several
  previously returned “unsupported”; FreeText now succeeds). Rubber-stamp `/Name`
  follows the label (ISO standard stamps recognised); FreeText centre/right alignment
  uses the real Core-14 AFM advances of the `/DA` font; ink strokes are smoothed
  (Catmull-Rom → Bézier) and squiggly markup renders as a true sinusoid.
- **RTF export — fonts & images ([#78]).** `modelToRtf` / `toRtf` emit a real per-run
  font table (`\fonttbl` with `\froman` / `\fswiss` / `\fmodern` classes; each run
  selects its `\fN`) instead of a single hardcoded font, and transcode GIF / WebP /
  AVIF pictures to PNG (`\pict\pngblip`) instead of dropping them.
- **PDF → model reconstruction ([#75]).** Multi-paragraph table cells, header-row
  detection by font size / shading (and footers), `Cell.shading` / `Row.is_header`
  populated for ruled tables, decimal-column alignment on real glyph advances,
  single-gutter fallback, 3-line heading promotion with a bold-subhead guard,
  marker-format list nesting, ordered lists starting ≠ 1, document-wide leading
  fallback, image-only header/footer (logo) detection, and a page-number digit-fold
  guard — all flow through every PDF → Office/HTML/RTF/text conversion.

## [0.100.0] - 2026-06-25

Engine improvements only — no new SDK methods. The last in-scope Office-import
([#3](https://github.com/qrcommunication/gigapdf-lib/issues/3)) fidelity items.

### Improved

- **`officeToModel`** — XLSX/ODS sheet **column widths** are imported (and round-trip
  through export); **DOCX track-changes** are accepted to the final version (insertions
  kept, deletions dropped — deleted text no longer leaks), and comment markers no longer
  corrupt parsing.

## [0.99.0] - 2026-06-25

Engine improvements only — no new SDK methods; `officeToModel` and the PDF→model
reconstruction get higher fidelity, and one roadmap closes.

### Improved

- **DOCX Office Math ([#37]) complete** — `officeToModel` now linearizes OMML
  equations to readable Unicode math (fractions, radicals, sub/superscripts, ∑/∫,
  matrices…) instead of dropping them. ([#37](https://github.com/qrcommunication/gigapdf-lib/issues/37))
- **Table header rows** — `Row.is_header` is detected in PDF→model reconstruction and
  round-trips through DOCX/ODF/HTML/EPUB import+export+render (+ JSON).
- **PDF→model** — heading levels are clustered document-wide (consistent across pages),
  and page `/Rotate` now applies to tagged-PDF block geometry.

## [0.98.0] - 2026-06-25

Engine improvements only — no new SDK methods; existing conversion methods reach
markedly higher fidelity, and two conversion roadmaps close.

### Improved

- **Office export ([#2]) complete** — `modelTo{Docx,Xlsx,Pptx,Odt,Ods,Odp}` now
  preserve multi-section page setup, super/subscript, spreadsheet underline/strike,
  ODT block shapes, internal page links, real PPTX/ODP non-slide structures, and
  explicit run colour. ([#2](https://github.com/qrcommunication/gigapdf-lib/issues/2))
- **Other-format conversions ([#4]) complete** — RTF import decodes WMF/EMF/DIB
  pictures (`\bin` + hex), finishing the rich RTF↔model / `toText` / `toRtf` / CSV /
  Markdown / EPUB / PDF-A roadmap. ([#4](https://github.com/qrcommunication/gigapdf-lib/issues/4))
- **In-house WMF/EMF metafile decoder** — embedded Windows metafiles in `.rtf` and
  Office packages now rasterize (previously dropped).
- **Office import ([#3])** — PPTX/ODP run styling + paragraph + lists, ODS
  merges/number-formats/fills, DOCX `w:vMerge` row spans, internal hyperlink-anchor
  resolution, speaker notes, and embedded-image format detection.
- **PDF→model ([#5])** — page `/Rotate` honored, tagged blocks land on their real pages.

## [0.97.0] - 2026-06-25

Engine improvements only — no new SDK methods; existing conversion/render/read
methods get markedly higher fidelity.

### Improved

- **JPEG 2000 (`JPXDecode`) images** now render and extract (a from-scratch
  decoder wired into the image pipeline) — the third hand-written image codec
  after CCITTFax and JBIG2. ([#35](https://github.com/qrcommunication/gigapdf-lib/issues/35))
- **`toPdfA` is now veraPDF-validated conformant** — every font embedded (or
  metric-matched-substituted), encryption + JavaScript stripped, metadata from
  the document. ([#4](https://github.com/qrcommunication/gigapdf-lib/issues/4))
- **Conversion fidelity** advanced across the open roadmaps: PDF→model recon
  (FontDescriptor bold/italic, robust columns/lines, table spans/sparse/rotated,
  tagged `/ColSpan`/`/ListNumbering`/`/Pg`/`/BBox`); Office import (DOCX symbols/
  text-boxes/field-codes, footnotes, PPTX/ODP autoshapes, ODT lists/spans);
  Office export (standard CSV, Markdown colour/shapes, PPTX para formatting, run
  images, hyperlinks, ODT nested lists/borders); RTF/text model-aware export;
  CSV typed-cell import.

## [0.96.0] - 2026-06-25

PDF linearization + from-scratch bilevel codecs, plus a broad conversion-fidelity
pass. New SDK surface:

### Added

- **`toLinearized()` / `saveLinearized()`** — produce a linearized ("Fast Web
  View") PDF: ISO 32000-1 Annex F layout with `/Linearized` dict + page-offset
  and shared-object hint streams, byte-exact and **qpdf-clean**.
  ([#67](https://github.com/qrcommunication/gigapdf-lib/issues/67))

### Improved (engine, via existing SDK conversion/render/read methods)

- **Scanned-document PDFs** now render and extract: `CCITTFaxDecode` (G3/G4) and
  `JBIG2Decode` (full ITU-T T.88) are implemented from scratch — no third-party
  codec. ([#34](https://github.com/qrcommunication/gigapdf-lib/issues/34))
- **Conversion fidelity** advanced across the board (open roadmaps #2/#3/#4/#5):
  XLSX/ODS cell formulas, real image formats and PPTX/ODP speaker notes on export;
  DOCX/ODT headers/footers, XLSX cell styling and DOCX footnotes on import; rich
  RTF↔model and GFM Markdown→model; EPUB nested TOC + unique id + inline-SVG
  shapes; and PDF→model gains header/footer stripping, stable heading levels and
  list false-positive rejection.

## [0.95.0] - 2026-06-25

Thirteen roadmap issues (PDF authoring, PDF reading, Office round-trip, CI
conformance). New SDK surface:

### Added

- **`setPageTransition(page, opts)` / `getPageTransition(page)` /
  `clearPageTransition(page)`** — presentation page transitions (`/Trans`: 12
  styles + direction/dimension/motion/scale) and per-page `/Dur` auto-advance.
  ([#65](https://github.com/qrcommunication/gigapdf-lib/issues/65))
- **`scalePageContent(page, factor)` / `scalePageContentXy(page, sx, sy)` /
  `scalePageTo(page, w, h)` / `setUserUnit(page, unit)`** — true content scaling
  (stream + boxes + annotation rects) and `/UserUnit` for large-format pages.
  ([#68](https://github.com/qrcommunication/gigapdf-lib/issues/68))
- **`setCollection(config)` / `collection()`** — embedded-file portfolio
  `/Collection` (view, `/Schema` columns, sort, default file, per-file `/CI`).
  ([#66](https://github.com/qrcommunication/gigapdf-lib/issues/66))
- **`setFigureAlt(index, alt)` / `figureCount()`** — per-figure `/Alt` accessible
  alternate text for Tagged PDF / PDF/UA + PDF/A level-A exports.
  ([#20](https://github.com/qrcommunication/gigapdf-lib/issues/20))

### Improved (engine, via existing SDK conversion/render methods)

- PDF reading honors **optional-content (OCG/OCMD) visibility** during render
  ([#54](https://github.com/qrcommunication/gigapdf-lib/issues/54)) and **vertical
  writing mode** (`Identity-V`, `/W2`/`/DW2`)
  ([#49](https://github.com/qrcommunication/gigapdf-lib/issues/49)).
- Office conversions gain document **outline/TOC** (#31), **named-style** lowering
  (#30), **DOCX drawing** geometry + alt (#40), **super/subscript** (#32),
  **flat-XML ODF + `.odg`** import (#53) and **table/cell vertical alignment** (#27).
- CI now validates exports against **ECMA-376 XSD + ODF RelaxNG** schemas (#19).

## [0.94.0] - 2026-06-25

Issue [#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) — the native
HTML/CSS + inline-SVG renderer — is complete (inline `@font-face` with
ttf/otf/woff/**woff2** via a from-scratch WOFF2/brotli decoder, full UAX#9 bidi,
SVG filters, COLRv1, patterns, sticky/float/flex, …), plus 21 PDF/font/Office
fixes. New SDK surface:

### Added

- **`addDocumentJavascript(name, script)` / `documentJavascripts()` /
  `removeDocumentJavascript(name)`** — author document-level JavaScript actions in
  the catalog `/Names /JavaScript` name tree (run by viewers on open).
  ([#64](https://github.com/qrcommunication/gigapdf-lib/issues/64))
- **`beginOptionalContent(page, ocg)` / `endOptionalContent(page)`** — assign drawn
  page content to a toggleable optional-content (OCG) layer via marked content.
  ([#59](https://github.com/qrcommunication/gigapdf-lib/issues/59))
- **`placePage(target, source, x, y, sx, sy)` / `placePageMatrix(...)` /
  `nUp(cols, rows, opts?)`** — N-up / imposition: place a source page as a scaled
  Form XObject onto another page (2-up/4-up/contact sheets).
  ([#60](https://github.com/qrcommunication/gigapdf-lib/issues/60))

### Improved

- The HTML→PDF renderer now honours inline `@font-face` web fonts, RTL/bidi text,
  SVG `filter`, COLRv1 colour fonts, `<pattern>` fills, `position: sticky`, floats,
  flex column sizing, conic gradients and more — issue #1 in full.
- Office conversions gain: document metadata round-trip, DOCX paragraph/table
  styling + page breaks + numbering, real slide tables, named styles, placeholder
  roles, slide backgrounds; PDF reading gains inline images, hybrid `/XRefStm`,
  type-1 shadings and Type1 glyph rasterisation.

## [0.93.0] - 2026-06-24

### Added

- **`appendPages(otherPdf, pages?)`** appends selected source pages (1-based; omit
  for all); **`mergePdfs(parts)`** now accepts `(Uint8Array | { pdf, pages? })[]`,
  with the new exported `MergePart` type, for page-range merges.
  ([#61](https://github.com/qrcommunication/gigapdf-lib/issues/61))

### Fixed (read/import, no SDK signature change)

- Type0 CJK composite fonts (predefined/embedded CMaps + non-Identity
  `/CIDToGIDMap`) now decode/render correctly
  ([#46](https://github.com/qrcommunication/gigapdf-lib/issues/46)); PPTX import
  gains run hyperlinks, table cell fill/borders, theme-colour resolution, and
  mirror ([#47](https://github.com/qrcommunication/gigapdf-lib/issues/47)).

## [0.92.0] - 2026-06-24

### Added

- **`setViewerPreferences(prefs)` / `setPageLayout(layout)` / `setPageMode(mode)`**
  author the catalog reading/UX hints. `ViewerPreferences` = optional
  `hideToolbar`/`hideMenubar`/`hideWindowUI`/`fitWindow`/`centerWindow`/
  `displayDocTitle` (omit = leave untouched) + `direction` (`'L2R'`/`'R2L'`);
  `PageLayout` ∈ SinglePage/OneColumn/TwoColumn{Left,Right}/TwoPage{Left,Right};
  `PageMode` ∈ UseNone/UseOutlines/UseThumbs/FullScreen/UseOC/UseAttachments.
  ([#63](https://github.com/qrcommunication/gigapdf-lib/issues/63))

### Fixed (read/import, no SDK signature change)

- Type3 `/CharProcs` glyphs now render
  ([#42](https://github.com/qrcommunication/gigapdf-lib/issues/42)); ODT import
  gains paragraph styling, footnotes, body text boxes, table sizing/shading
  ([#52](https://github.com/qrcommunication/gigapdf-lib/issues/52)).

## [0.91.0] - 2026-06-24

PDF-read & Office-import fidelity (no SDK signature change): `LZWDecode`,
`ASCII85Decode`, `ASCIIHexDecode`, `RunLengthDecode` stream filters (chained
`/Filter` + `/DecodeParms`) now decode
([#33](https://github.com/qrcommunication/gigapdf-lib/issues/33)); ODS import
gains per-cell styling, number formats, merges, and column/row sizing
([#45](https://github.com/qrcommunication/gigapdf-lib/issues/45)).

## [0.90.0] - 2026-06-24

PDF-read & Office-import fidelity (no SDK signature change): image `/ImageMask`
stencils + `/Mask` (explicit & colour-key) now render
([#41](https://github.com/qrcommunication/gigapdf-lib/issues/41)); XLSX import
gains per-cell styling, number/date formats, shared formulas and hyperlinks
([#44](https://github.com/qrcommunication/gigapdf-lib/issues/44)).

## [0.89.0] - 2026-06-24

PDF-read fidelity (no SDK signature change): FlateDecode/LZW `/DecodeParms`
predictors (TIFF 2, PNG 10–15) for image **and** xref/object streams
([#57](https://github.com/qrcommunication/gigapdf-lib/issues/57)); CalGray/CalRGB
gamma + matrix + white-point colour conversion, ICCBased `/N` fallback corrected
([#58](https://github.com/qrcommunication/gigapdf-lib/issues/58)).

## [0.88.1] - 2026-06-24

HTML/CSS renderer: `box-shadow: inset` is now painted (a clipped shadow frame
inside the box) instead of dropped. HTML→PDF path only — no SDK signature change
([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item B).

## [0.88.0] - 2026-06-24

HTML/CSS renderer: `flex-basis` / `flex-grow` / `flex-shrink` now apply on the
**column** axis (flexing item heights against a definite container `height`), not
just the row axis. HTML→PDF path only — no SDK signature change
([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item A).

## [0.87.2] - 2026-06-24

HTML/CSS renderer: 3-D `border-style`s `inset`/`outset`/`groove`/`ridge` now shade
the sides for depth instead of rendering flat `solid`. HTML→PDF path only — no SDK
signature change ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item C).

## [0.87.1] - 2026-06-24

HTML/CSS renderer: `aspect-ratio` now derives a block's height from its width
(`width / ratio`) when no definite `height` is set. HTML→PDF path only — no SDK
signature change ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item A).

## [0.87.0] - 2026-06-24

HTML/CSS renderer: colour **alpha** (`rgba()`/`hsla()`/`#rgba`/`#rrggbbaa`) is now
applied — folded into the opacity of the text/background/border/cell it paints,
composing with `opacity` — instead of being dropped. Also fixes function colours
with internal spaces (`rgba(0, 0, 0, .5)`) in the `background`/`border` shorthands.
HTML→PDF path only — no SDK signature change
([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item C).

## [0.86.1] - 2026-06-24

HTML/CSS renderer: `grid-template-rows` now resolves `%` and `fr` rows (against
the grid's definite `height`), not just fixed `pt`. HTML→PDF path only — no SDK
signature change ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item A).

## [0.86.0] - 2026-06-24

HTML/CSS renderer: `overflow: hidden`/`clip` now emit a **real PDF clip** (text,
images, backgrounds, gradients straddling an edge are pixel-clipped; nested boxes
intersect). `height` is now a **definite** height (content overflows + is clipped)
instead of a `min-height` alias; text runs carry their width so overflowing text
is clipped too. HTML→PDF path only — no SDK signature change
([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item A).

## [0.85.4] - 2026-06-24

HTML/CSS renderer: `flex-direction: row-reverse` / `column-reverse` now run the
main axis from the far end (were collapsed to the forward axis) in the HTML→PDF
path ([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item A). No API change.

## [0.85.3] - 2026-06-24

HTML/CSS renderer: `justify-content: space-evenly` now uses `n + 1` equal gaps
(was aliased to `space-around`) in the HTML→PDF path
([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item A). No API change.

## [0.85.2] - 2026-06-24

HTML/CSS renderer: `currentColor` now resolves to the element's cascaded `color`
(borders, background, and the `border` shorthand) in the HTML→PDF path
([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item C). No API change.

## [0.85.1] - 2026-06-24

HTML/CSS renderer: absolute & relative CSS length units — `cm`, `mm`, `in`, `pc`,
`q`, `ex`, `ch` — now resolve in the HTML→PDF path
([#1](https://github.com/qrcommunication/gigapdf-lib/issues/1) item E). No API change.

## [0.85.0] - 2026-06-24

Standalone tagged-PDF / PDF-UA authoring. Resolves
[#18](https://github.com/qrcommunication/gigapdf-lib/issues/18).

### Added

- **`doc.toTagged({ pdfUa? })`** — author a tagged (accessible) PDF: a
  `/StructTreeRoot` logical-structure tree with marked content, `/MarkInfo`,
  `/Lang`, `/RoleMap` and `/Alt` on figures, **without** forcing PDF/A. `pdfUa`
  adds the PDF/UA-1 identifier (ISO 14289).

## [0.84.0] - 2026-06-24

Public-key (certificate) encryption + password management. Resolves
[#17](https://github.com/qrcommunication/gigapdf-lib/issues/17).

### Added

- **`doc.encryptForRecipients(certificates, opts?)`** — encrypt to one or more
  X.509 recipients (`/Filter /Adobe.PubSec`); only a recipient private key opens
  it. `certificates` are DER certs; `opts = { flags?, permissions?, aes256?,
  encryptMetadata?, seed?, rngSeed? }` (`seed`/`rngSeed` default to Web Crypto).
- **`engine.openWithPrivateKey(pdf, certificate, privateKey)`** — open a
  public-key-encrypted PDF with a DER cert + PKCS#1 RSA key (`null` if not a
  recipient).
- **`doc.changePasswords(newPassword, fileId, opts?)`** — re-encrypt an opened
  document with new passwords (`opts` adds `encryptMetadata?`).
- **`doc.removeEncryption()`** — strip encryption → plaintext PDF.

## [0.83.0] - 2026-06-24

Press-ready colour authoring. Resolves
[#11](https://github.com/qrcommunication/gigapdf-lib/issues/11) (CMYK / spot /
ICC OutputIntent / overprint).

### Added

- **`Color`** union type — `{ space: "rgb", rgb }` · `{ space: "cmyk", c,m,y,k }`
  · `{ space: "gray", gray }` · `{ space: "separation", name, tint, cmyk }` ·
  `{ space: "icc", components, profile }`. CMYK/gray/tint components are `0`…`1`.
- **`addFilledRectangle(page, rect, color, opacity?)`** and
  **`addFilledPolygon(page, points, color, opacity?)`** — fill shapes in any
  colour space.
- **`addTextColor(page, x, y, size, text, font, color, opts?)`** — base-14 text
  in any colour space (`opts = { opacity?, rotation?, underline?, strikethrough? }`).
- **`setOverprint(page, fill, stroke, mode?)`** — prepress overprint (`/op`,
  `/OP`, `/OPM`).
- **`addOutputIntent(profile, condition)`** — embed an ICC profile as a document
  OutputIntent (`/S /GTS_PDFX`), decoupled from PDF/A.

## [0.82.0] - 2026-06-24

Gradient authoring. Resolves
[#12](https://github.com/qrcommunication/gigapdf-lib/issues/12) (gradients;
tiling patterns / blend modes deferred).

### Added

- **`addGradient(page, spec)`.** Paint a **linear** or **radial** gradient over a
  rectangle. `spec = { kind: "linear" | "radial", coords, stops, rect, extend?,
  opacity? }` — `coords` is `[x0,y0,x1,y1]` (linear) or `[x0,y0,r0,x1,y1,r1]`
  (radial); `stops` is ≥ 2 `{ offset, rgb }`. Rendered as a shading pattern (ISO
  32000-1 §8.7.4/§8.7.3); the stops compile to a PDF interpolation function.
- New exported types **`GradientSpec`** and **`GradientStop`**.

## [0.81.0] - 2026-06-24

Compact output — object streams + cross-reference stream. Resolves
[#10](https://github.com/qrcommunication/gigapdf-lib/issues/10) (linearization
excepted).

### Added

- **`saveOptimized(opts?)`.** Serialize with PDF 1.5+ **object streams**
  (`/ObjStm`) + a **cross-reference stream** (`/XRef`) — the most compact output.
  `opts = { objectStreams?, xrefStreams? }` (both default `true`; `objectStreams`
  implies `xrefStreams`). Streams are Flate-compressed first, like
  `saveCompressed`.

Both modes are validated with `qpdf --check`. Linearization (Fast Web View) is
not performed.

## [0.80.0] - 2026-06-24

Signature verification + DocMDP certification. Resolves
[#16](https://github.com/qrcommunication/gigapdf-lib/issues/16).

### Added

- **`signatures()`.** List every signature (`/Sig` field) with `{ fieldName,
  signerName, reason, location, date, subFilter, byteRange }`.
- **`verifySignatures(pdfBytes)`.** Cryptographically verify each signature
  against the **original bytes** — `{ byteRangeOk, digestOk, signatureOk,
  coversWholeDocument, signerCommonName, certCount, algorithm }`. `digestOk` is
  content integrity (ByteRange SHA-256 vs CMS `messageDigest`); `signatureOk` is
  the RSA SignerInfo signature. RSA + SHA-256 only.
- **`certify(fields, random, docmdpLevel, keyBits?)`.** Certify the document
  (DocMDP) — writes `/Perms /DocMDP` + a `/Reference` transform; `docmdpLevel` is
  `1` (no changes), `2` (form-fill + sign) or `3` (also annotate).
- New exported types **`SignatureInfo`** and **`SignatureReport`**.

## [0.79.0] - 2026-06-24

Interactive-form completeness: signature fields, field JavaScript, calculation
order, field deletion and appearance regeneration. Resolves
[#15](https://github.com/qrcommunication/gigapdf-lib/issues/15).

### Added

- **`addSignatureField(page, name, rect, opts?)`.** A visible signature field
  (`/FT /Sig`) the signing pipeline can target; sets the AcroForm `/SigFlags`.
- **`setFieldScript(name, trigger, js)`.** Field-level JavaScript on a field's
  `/AA` — `trigger ∈ "keystroke" | "format" | "validate" | "calculate"` (input
  masks, formatting, validation, computed totals).
- **`setCalculationOrder(names)`.** The AcroForm `/CO` recalculation order.
- **`removeField(name)`.** Delete a field (from `/Fields`, `/CO` and page annots).
- **`regenerateFieldAppearance(name)`.** Rebuild a field's appearance after a
  programmatic value change (text / choice / checkbox).

## [0.78.0] - 2026-06-24

Full action & destination navigation model. Resolves
[#14](https://github.com/qrcommunication/gigapdf-lib/issues/14).

### Added

- **`addLink(page, rect, action)`.** A link over `rect` carrying any `Action` —
  `goto` (with every fit mode: `xyz`/`fit`/`fitH`/`fitV`/`fitR`/`fitB`/`fitBH`/
  `fitBV`/`named`), `gotoR` (remote file), `uri`, `named` viewer navigation,
  `launch`, `javascript`, `submitForm`, `resetForm`.
- **`setOpenAction(action)`.** Set the document `/OpenAction` performed on open.
- **`removeLink(page, linkIndex)`.** Delete the *n*-th `/Link` annotation on a
  page (other annotations untouched).
- **`setBookmarks(bookmarks)`.** Replace the outline with `Bookmark[]`
  (`{title, level, action?}`) — bookmarks can carry any action (a `goto` becomes
  a `/Dest`, anything else an `/A`).
- New exported types: **`Action`**, **`Destination`**, **`Bookmark`**.

## [0.77.0] - 2026-06-24

Geometric annotation subtypes + appearance regeneration. Resolves
[#13](https://github.com/qrcommunication/gigapdf-lib/issues/13).

### Added

- **`addCircleAnnotation` / `addPolygonAnnotation` / `addPolylineAnnotation` /
  `addCaretAnnotation`.** The missing geometric annotation subtypes (`/Circle`,
  `/Polygon`, `/PolyLine`, `/Caret`), with border width and interior colour, each
  rendered via a generated `/AP` appearance stream.
- **`regenerateAppearance(page, index)`.** Rebuild an existing annotation's baked
  appearance after editing its colour/border/geometry (Square, Circle, Line,
  Polygon, PolyLine, Highlight, Underline, StrikeOut, Ink, Caret); `false` for
  subtypes that can't be reconstructed (FreeText/Stamp/Text/Link).

A text watermark already exists as `addWatermark`, so no new watermark method was
added.

## [0.76.0] - 2026-06-24

General document metadata (XMP + typed Info, kept in sync). Resolves
[#7](https://github.com/qrcommunication/gigapdf-lib/issues/7).

### Added

- **`setInfo(fields)` / `getXmp()` / `setXmp(xmp)`.** `setInfo` writes the typed
  document-information fields (`{ title?, author?, subject?, keywords?, creator?,
  producer?, creationDate?, modDate? }`) to **both** the `/Info` dictionary and a
  synced XMP `/Metadata` packet — a partial update (omitted fields are left
  unchanged), curing the classic Info-vs-XMP "two sources of truth" drift.
  `getXmp`/`setXmp` read and replace the raw XMP packet (bytes or string). New
  `InfoFields` type. The single-key `setMetadata(key, value)` is unchanged (it
  touches only `/Info`).

## [0.75.0] - 2026-06-24

Writable file attachments + Factur-X / ZUGFeRD `/AF`. Resolves
[#9](https://github.com/qrcommunication/gigapdf-lib/issues/9).

### Added

- **`addAttachment` / `addAssociatedFile` / `removeAttachment` /
  `addFileAttachmentAnnot`.** Embed, replace and remove document-level file
  attachments (`/Names /EmbeddedFiles`), anchor a visible `FileAttachment`
  annotation, and link **associated files** (`/AF`, PDF/A-3) — the mechanism
  hybrid e-invoices (Factur-X / ZUGFeRD / Order-X) use to carry their invoice XML
  (`addAssociatedFile(name, bytes, "alternative", …)`). Attachment bytes are stored
  FlateDecode-compressed; re-using a name replaces it. New `AfRelationship`,
  `AttachmentOptions` and `FileAttachmentIcon` types. The read side
  (`attachments()`) is unchanged.

## [0.74.0] - 2026-06-24

Page labels (`/PageLabels`). Resolves
[#8](https://github.com/qrcommunication/gigapdf-lib/issues/8).

### Added

- **`getPageLabels()` / `setPageLabels(ranges)` / `pageLabel(page)`.** Read,
  author and resolve page-numbering labels (ISO 32000-1 §12.4.2) — front matter
  in roman numerals, an appendix as `A-1, A-2`, etc. `getPageLabels` returns the
  ranges sorted by `startPage` (1-based); `setPageLabels` replaces them (an empty
  array clears all labels); `pageLabel` returns the viewer-visible string for a
  page (e.g. `"iv"`, `"A-3"`), falling back to the decimal page number outside any
  range. Labels survive a save→reopen round-trip. New `PageLabelRange` /
  `PageLabelStyle` types.

## [0.73.0] - 2026-06-24

Print-production release: full read/write access to all five ISO 32000-1 page
boundary boxes. Resolves
[#6](https://github.com/qrcommunication/gigapdf-lib/issues/6).

### Added

- **`getPageBoxes(page)` / `setPageBox(page, kind, box)`.** Read and write the
  five page boundary boxes (`media`/`crop`/`bleed`/`trim`/`art`, ISO 32000-1
  §14.11.2). `getPageBoxes` returns each box as `[x0, y0, x1, y1]` (points) with
  inheritance and the per-box default chain applied (CropBox→MediaBox;
  Bleed/Trim/Art→CropBox), plus a `declared` map flagging which boxes are
  explicitly present vs inherited/defaulted. `setPageBox` takes a box as
  `{ x, y, w, h }` (origin + size), normalises it, and preserves the page's other
  boxes — so a `/TrimBox`/`/BleedBox` survives a save→reopen round-trip. This is
  the prerequisite for PDF/X export and commercial-print pipelines (imposition,
  bleed, finished-size trimming).
- **Types `PageBoxes`, `PageBoxKind`, and the `PAGE_BOX_KINDS` constant.**

## [0.72.0] - 2026-06-24

Fidelity release focused on text extraction and AcroForm rendering on dense
government forms (CERFA). The public API is additive — existing behaviour is
unchanged except where noted as a fix.

### Added

- **`FormField` now surfaces its text-formatting metadata.** Each field exposes
  `comb` (the `/Ff` comb flag for fixed-pitch character cells), `quadding` (the
  `/Q` justification: 0 left, 1 centre, 2 right), and the default-appearance font
  and size parsed from the field's `/DA` string as `daFont` / `daSize`. This lets
  a host reproduce a field's intended layout (combed cells, alignment, font
  metrics) without re-parsing the appearance stream.

### Fixed

- **Spurious in-word spaces during text extraction.** A new gap-aware
  `runs_join` helper now drives all four reconstruction paths (lines,
  paragraphs, lists, tables): a word split across several font runs no longer
  emits a phantom space at each run boundary (e.g. `ENFANT S` is reassembled as
  `ENFANTS`). Spacing is decided from the real inter-run gap, not the mere fact
  that the text changed font.
- **Form-field appearances double-rendered behind the editable text.** A
  `widget_appearances` flag makes `renderPageNoText` / `renderPageExcluding`
  omit the `/AP` appearance streams of AcroForm widgets, so a filled field's
  baked-in value no longer shows through underneath the live, editable overlay.
- **Borderless prose misdetected as a table.** A `line_has_gutter` guard now
  requires a real inter-cell gutter before promoting a borderless block to a
  table: a two-run-per-line prose notice is kept as prose, while genuine tables
  (wide column gutters) are still recognised.

## [0.71.1] - 2026-06-23

Documentation-only patch. No code changes — the WASM blob is byte-for-byte
identical to 0.71.0.

### Documentation

- Complete overhaul of the SDK documentation for 0.71: API reference (signature
  matrix for B / B-T / LTV signing, full ~263-method surface, removal of the
  phantom OCR methods `doc.ocr` / `ocrText` / `extractText`), USAGE guide (the
  four signing-signature levels + the host-fetch two-phase model + an SSRF note),
  COOKBOOK (added `signTimestamped` / `signLtv` recipes and an image-watermark
  recipe), plus the README and `sdk/README` (npm). No behavioural change — the
  WASM is identical to 0.71.0.

## [0.71.0] - 2026-06-23

Long-term validation release: PAdES-LTV builds on the B-T timestamped signatures
from 0.70 by embedding the validation material (certificate chain + revocation
responses) so a signature keeps verifying long after its certificates expire or
are revoked. The public API is additive — existing behaviour is unchanged.

### Added

- **PAdES-LTV (B-LT / B-LTA).** New SDK `GigaPdfDoc.signLtv()` (async) produces a
  long-term-validation signature: it first builds a B-T signature
  (`signTimestamped`), then embeds a Document Security Store (`/DSS` with
  `/Certs`, `/OCSPs`, `/CRLs`, and per-signature `/VRI`) carrying the revocation
  material for the certificate chain (B-LT). With `archiveTimestamp` it also adds
  a `/DocTimeStamp` document timestamp (`ETSI.RFC3161` subfilter) over the whole
  updated file for B-LTA, refreshing the long-term trust anchor. The engine
  computes *which* OCSP/CRL endpoints to query from the certificates' AIA / CRL-DP
  extensions; the host fetches them (the WASM core has no network stack, same
  pure-data two-phase model as the TSA). OCSP requests follow RFC 6960; CRLs are
  parsed as `CertificateList`. The exported `defaultOcspPost` and `defaultCrlGet`
  perform the round trips via `fetch`, and the `revocationFetch` / `crlFetch`
  hooks let the host add auth/proxy/retries **and apply its own SSRF allow-list**.

### Fixed

- **B-T `id-aa-timeStampToken` now carries the bare `TimeStampToken`.**
  `signFinishTimestamped` / `signTimestamped` previously embedded the TSA's raw
  `TimeStampResp` (`SEQUENCE { PKIStatusInfo, TimeStampToken }`) verbatim in the
  `id-aa-timeStampToken` unsigned attribute. The engine now unwraps the response
  to the bare `TimeStampToken` (a CMS `ContentInfo`) before embedding it — as
  required by RFC 3161 §3.3.2 / ETSI EN 319 122 — matching the B-LTA
  document-timestamp path. Both a raw `TimeStampResp` and an already-unwrapped
  token are accepted (the `PKIStatusInfo` gate is still enforced).

## [0.70.0] - 2026-06-23

Fidelity + standards release: advanced (PAdES-B-T) timestamped signatures,
richer shading and JPEG decoding at the rasteriser, complex-script text shaping
for Indic writing systems, CFF flex curves, and RTF image import. The public API
is additive — existing behaviour is unchanged.

### Added

- **PAdES-B-T trusted timestamps (RFC 3161).** New SDK
  `GigaPdfDoc.signTimestamped()` (async) embeds an RFC 3161 timestamp token in
  the SignerInfo for an *advanced*-level PAdES-B-T signature — `ETSI.CAdES.detached`
  subfilter, `signing-certificate-v2` (ESS) signed attribute, and the
  `id-aa-timeStampToken` unsigned attribute. Uses the engine's pure-data
  two-phase TSA flow (core emits the `TimeStampReq`, host POSTs it, core embeds
  the returned token) since the WASM core has no network stack; `tsaFetch` lets
  the host add auth/proxy/retries **and apply its own SSRF allow-list**, and the
  exported `defaultTsaPost` POSTs `application/timestamp-query` via `fetch`
  (e.g. FreeTSA). Signs with an imported PKCS#12 or a freshly generated
  self-signed identity.
- **Mesh shadings at the rasteriser.** Free-form (type 4), lattice (type 5),
  Coons (type 6) and tensor (type 7) shadings are now rendered as Gouraud
  triangles (pure, zero-dep decoder; Coons/tensor patches tessellated per
  ISO 32000-1 §8.7.4.5.7), with per-vertex colour resolved through
  `Separation`/`DeviceN`/`ICCBased`/`CMYK`/`Gray`. Axial (2) and radial (3)
  shadings are unchanged.
- **Arithmetic-coded JPEG decoding.** SOF9 (sequential) and SOF10 (progressive)
  JPEGs now decode via a hand-rolled ISO/IEC 10918-1 Annex MQ arithmetic decoder
  with the F.1.4 DC/AC context models and `DAC` conditioning. Baseline/Huffman
  paths are unchanged; lossless (SOF3/SOF11) and 12-bit Huffman (SOF1) remain
  gracefully unsupported.
- **Indic complex-script shaping.** A syllabic reordering machine for the
  Brahmi-derived scripts (Devanagari, Bengali, Gurmukhi, Gujarati, Oriya, Tamil,
  Telugu, Kannada, Malayalam) — reph and pre-base matra reordering — plus the
  missing OpenType lookups: GSUB 2 (multiple), GSUB 3 (alternate), GSUB 8
  (reverse chaining single) and GPOS 3 (cursive attachment). Latin and the
  existing contextual paths are unchanged.
- **CFF/Type2 flex operators.** The Type2 charstring interpreter now implements
  the four flex operators (`flex`, `flex1`, `hflex`, `hflex1`, Adobe TN #5177),
  each emitting two cubic curves — CFF glyphs using flex no longer drop or
  mis-render contour segments.
- **RTF image import.** RTF import parses the `\pict` group, extracting
  `\pngblip`/`\jpegblip` payloads as `<img src="data:image/…;base64,…">`
  (display size recovered from `\picwgoal`/`\pichgoal`), reusing the HTML
  engine's image-embed pipeline. DIB/BMP, WMF/EMF and binary `\bin` payloads are
  skipped (documented limits), guarded by a PNG/JPEG magic-byte check.

## [0.69.0] - 2026-06-23

Image-watermark release: stamp a raster image across any range of pages, with
the same ergonomics as the existing text watermark. The text watermark is
unchanged.

### Added

- **Image watermark.** Stamp a raster image over pages —
  `addImageWatermark` (SDK) / `add_image_watermark` (core) /
  `gp_add_image_watermark` (FFI). Accepts **PNG / JPEG / WebP / GIF / AVIF**
  source images and supports per-watermark **opacity**, **anchoring**
  (center + four corners) with offsets, **rotation** (about the image center),
  **scaling** to a target size (aspect-follow), and an optional **tiling** grid.
  The image XObject is embedded **once** and referenced on each target page,
  reusing the existing image-embed/raster-transcode pipeline. The text
  watermark and `add_image` behavior are unchanged.

## [0.68.0] - 2026-06-23

Format-reach + import/render fidelity release: the unified model now exports
**Markdown / CSV / EPUB** end to end, Office/ODF import preserves far more
structure, the HTML→PDF renderer gains the remaining common CSS, and several
image-codec and rendering bugs are fixed.

### Added

- **Markdown / CSV / EPUB model export.** The unified editable model can now be
  raised to **Markdown** (`modelToMd`), **CSV** (RFC 4180, `modelToCsv`) and
  **EPUB 3** (`modelToEpub`), alongside the existing
  `modelTo{Docx,Xlsx,Pptx,Odt,Ods,Odp,Pdf,Html,Rtf}` targets (ABI
  `gp_model_to_{md,csv,epub}`).
- **Complete Markdown modelling.** `CodeBlock`, `Blockquote` and
  `HorizontalRule` are first-class in the model — full Markdown round-trip
  (headings, runs, links, images, nested lists, GFM tables, code blocks,
  block-quotes, horizontal rules, footnotes, front-matter) rendered and exported
  consistently across formats.
- **Office / ODF import fidelity.** DOCX/XLSX/PPTX and **ODF (`.odt`/`.ods`/
  `.odp`)** import now preserves **images, hyperlinks, strikethrough,
  highlighting, spreadsheet formulas, grouped shapes, charts, SmartArt text and
  master/layout (theme) inheritance**.
- **HTML / CSS → PDF — remaining common CSS.** **Radial** and **conic**
  gradients, **`font-weight` 100–900**, **`box-shadow`** (blur), **elliptical
  `border-radius`**, dashed/dotted borders, **`linear-gradient`** and
  **`position: sticky`**.
- **OpenType text shaping.** GPOS mark positioning, GSUB contextual, script
  selection and Arabic joining (complex scripts only; Latin unchanged).
- **Image codecs.** SVG `<text>` rendering and GIF multi-frame decoding.
- **Run highlight.** Character-level `background` is painted and emitted across
  HTML, PDF and Office output.
- **`setTextRunStyle`.** Run-level style bake exposed in the SDK.
- **Mermaid flowchart renderer** in the HTML engine (`graph TD/LR`, node shapes,
  typed edges + arrowheads, Sugiyama layout → PDF vectors).

### Fixed

- **AVIF multi-tile decode — corrupt images > 9.4 MP.** Multi-tile AVIFs were
  decoded as a single tile, garbling pixels. The AV1 spec forces multi-tile
  above ~9.4 MP, so essentially every modern phone/camera AVIF was silently
  corrupted. Each tile is now decoded independently; single-tile and existing
  fixtures are byte-for-byte unchanged (validated bit-exact vs `dav1d`).
- **WebP lossless (VP8L)** — lossless transforms + meta-Huffman now decode real
  `cwebp`/libwebp lossless images correctly.

### Changed

- **Non-Device colorspaces** — Pattern fills and `Separation`/`ICCBased` colours
  in content streams are unified through the raster colour resolver (consistent
  with the rasterizer) instead of a device-default fallback.
- **Docs honesty** — README corrected to **near-zero-dependency** (hand-written
  PDF/render/conversion core; **RustCrypto** for crypto/signatures; **Boa** for
  JS — the earlier from-scratch JS engine is gone), **1198 tests** (was 284), and
  `.wasm` **~5.6 MB** (was ~540 KB, before Boa was bundled).

## [0.67.0] - 2026-06-23

### Added

- **Structured-editing ModelOps + permissions API exposed in the SDK.** New
  `applyModelOps` variants: paragraph formatting (`setParagraphStyle` — align/indent/
  spacing/line-height), lists (`setListLevel`/`setListMarker`/`setListOrdered`),
  absolute block placement (`setBlockFrame`/`setBlockRotation`), and table styling
  (`setCellShading`/`setRowHeight`/`setColWidth`/`setTableBorder`). Table structural
  edits (`insertTableRow`/`deleteTableRow`/`insertTableColumn`/`deleteTableColumn`/
  `setCellSpan` + sheet row/column ops) and `GigaPdfDoc` permission helpers
  (`permissionsToP`/`decodePermissions`/`getPermissions` + `saveEncrypted({ flags })`)
  are now callable from JS.

### Changed

- 8 PDF permission flags are functional: `/P` is computed from named flags per
  ISO 32000-1 Table 22 (previously a cosmetic integer).

## [0.66.0] - 2026-06-23

### Added

- **HTML/CSS rendering — LibreOffice-level fidelity.** `htmlRender` gains real **CSS
  grid** (`fr`/`minmax`/`repeat`/`span`/`auto-rows`) and **complete flexbox**
  (basis/grow/shrink/wrap/justify/align), **multi-column** (`column-count`/`columns`/
  `column-gap`), **pragmatic RTL/bidi** (`direction`/`dir`, RTL block/inline/run
  layout), table fidelity (colspan/rowspan, LibreOffice-level), text styling
  (super/sub, underline, strike), `@media`, font shorthand and further CSS-2 coverage.
- **Document reconstruction (`structuredText`) — waves R1–R10.** Typed + populated
  `pageBlocks` bodies, merged-cell spans, strikethrough, hyperlinks, paragraph
  spacing, super/subscript, document outline + figure captions, list nesting +
  continuation lines, multi-column reading order, multiple tables per page
  (connected-component split), borderless right/decimal-aligned columns, true
  decimal-tab alignment.
- **PDF permissions — 8 functional flags.** `getPermissions` + correct `/P` encoding
  of the 8 standard permission bits (print, modify, copy, annotate, fill-forms,
  extract, assemble, high-res print).
- **Model structural edits.** Table & sheet structural-edit ModelOps.

### OCR (native `gigapdf-ocr-rten` crate — host-side, not bundled in the npm package)

- Pivoted the OCR engine to **PaddleOCR PP-OCR on RTen** (pure-Rust ONNX, no C++/
  Tesseract): 13 printed languages incl. our own **Hebrew** model, with automatic
  per-line **script selection**.
- **Handwriting** recognizer (`latin_hw`) — our own CRNN trained on real handwriting
  (IAM/RIMES/NorHand/…; standard `nn.LSTM` → dynamic-width ONNX), **opt-in** via
  `recognize_page_handwriting` / `recognize_page_with(img, "latin_hw")`.
- Full OCR documentation refresh (architecture, training data, SDK, cookbook).

## [0.65.0] - 2026-06-22

### Added

- **Office→PDF phase-2 fonts** — `officeToPdfWith(office, fonts)` (ABI
  `gp_office_to_pdf_with_fonts`, core `office_to_pdf_with_fonts`) completes the
  two-phase font flow opened by `officeNeededFonts`: hand back the host-fetched
  faces for the families a container **references but doesn't embed** (e.g.
  Carlito for a Calibri reference) and styled runs lay out + paint with the right
  metrics instead of drifting onto the bundled fallback. The supplied faces are
  merged with whatever the document embeds itself — **embedded faces win on
  conflict** — so an empty `fonts` array yields exactly `officeToPdf`'s output
  (no regression). `fonts` uses the same packed blob as `htmlRender`.

## [0.64.0] - 2026-06-22

Office↔PDF fidelity program — import all formats → PDF and export PDF → all
formats much closer to 1:1, including complex layouts (boxes/encadrés).

### Added

- **Office→PDF preserves absolute layout** — presentation/box geometry is no
  longer reflowed into a flat stack. PPTX/ODP shapes, images and tables carrying
  an explicit `a:xfrm` / `draw:frame` are emitted at their exact coordinates
  (EMU/ODF units → pt), with slide backgrounds and `a:schemeClr` theme colours
  resolved. DOCX floating/anchored drawings (`wp:anchor`) and text boxes
  (`w:txbxContent`) become absolutely-positioned frames (the “encadrés”), and
  explicit page breaks (`w:br type=page`, `w:pageBreakBefore`, section breaks)
  are honoured.
- **XLSX/ODS render with cell styling** — fonts (bold/italic/underline/size/
  colour/family), borders, alignment and row heights are read from each cell's
  style and applied at render (theme colours resolved); ODS cells were previously
  unstyled. Merges, column widths and number formats unchanged.
- **PDF→Office export preserves absolute layout** — text boxes, images and vector
  rectangles/paths (fill/stroke/dash) are exported at their exact coordinates for
  PPTX/ODP/DOCX/ODT, so an exported deck/doc opened in PowerPoint/Word/Impress/
  Writer looks like the source PDF, encadrés included.
- **Office→PDF embeds the document's own fonts** — a self-embedding DOCX/PPTX/
  XLSX (`word|ppt|xl/fonts/*.odttf`, de-obfuscated per ECMA-376 §17.8.1) or ODT/
  ODS/ODP (`Fonts/*`, TTF/OTF) renders with its **own** typefaces (exact glyphs
  and metrics, no reflow drift) instead of the bundled Liberation fallback.
- **`officeNeededFonts(office)` / `gp_office_needed_fonts`** — phase-1 for
  `officeToPdf`: returns the fonts a container **references but doesn't embed**
  (`HtmlFontRequest[]`), so the host can fetch metric clones (Carlito↔Calibri,
  Arimo↔Arial, …) into its font cache for correct line-breaking. `null` for an
  unrecognized archive, `[]` when nothing is needed.
- **Stateful RTF rendering** — `rtfToPdf` now uses a real RTF parser with a `{}`
  group state stack: character styling (`\b \i \ul \strike \cf \fs \f` via
  font/colour tables), paragraph alignment/indents (`\qc\qr\qj\li\fi`), tables
  (`\trowd\cell\row`) and correct CP1252 (`\'80`→€, smart quotes, dashes) instead
  of the previous text-only extraction.

## [0.63.0] - 2026-06-22

### Changed

- **Added base-14 text references the standard font instead of embedding a
  substitute** — `embed_font` now detects base-14 families (Helvetica/Arial,
  Times, Courier, Symbol, ZapfDingbats — including Bold/Italic styles, via the new
  `base14_postscript_name`) and registers a nude `/Type1` base-14 font (no
  `FontFile`, WinAnsi encoding) rather than subsetting and embedding a Liberation
  substitute. This mirrors the principle the form `/AP` regeneration already
  applies. Adding text in a base-14 font now writes ~1 KB instead of ~57 KB per
  font (≈50× smaller saved PDFs) while rendering identically (the rasteriser draws
  base-14 natively). Custom (non-base-14) families are unchanged — still
  subset + embedded. HTML rendering excludes base-14 from host font fetches.
  Opt-in by family name: pass a base-14 PostScript/family name
  (`'Helvetica'`, `'Times-Roman'`, `'Courier'`, …) to `embedFont` to reference
  rather than embed.

## [0.62.0] - 2026-06-22

### Added

- **Markdown importer** — `mdToModel(md)` parses CommonMark-ish Markdown
  (pure Rust, zero deps) into the unified editable model: ATX headings,
  paragraphs, ordered/unordered nested lists, inline bold/italic/code/links with
  backslash escapes, fenced code blocks, block quotes, thematic breaks, and GFM
  pipe tables. ABI `gp_model_from_md`.
- **CSV importer** — `csvToModel(csv)` parses RFC 4180 CSV (quoted fields with
  embedded delimiters/newlines, `""` escape, CRLF/CR/LF, UTF-8 BOM strip,
  delimiter auto-detection among `,` `;` tab `|`) into a single table block
  (header row bold + shaded). ABI `gp_model_from_csv`. Returns `null` for empty
  input.

## [0.61.0] - 2026-06-22

### Fixed

- **Form layouts no longer mis-detected as giant tables.** The layout
  reconstruction (`recon/tables`, surfaced by `pageBlocks`/`toModel`) clustered
  every ruling line into grid edges and claimed every text line inside the grid
  bounding box, so a form's field-separator rules synthesised a giant table that
  swallowed the title and intro prose into cells. A geometric sanity gate now
  rejects a table candidate when `n_cols > 14`, `n_rows × n_cols > 160`, or the
  cell fill ratio `< 0.28` — the text flows back to the heading/paragraph
  pipeline. Real data tables (regular grid, well-filled) are preserved; dense
  ruled forms (16–47 columns, 7–24% fill) become standalone headings/paragraphs
  in reading order. No change to genuine `table` blocks.

## [0.60.0] - 2026-06-22

### Added

- **`pageBlocks(page)`** — per-page layout blocks (paragraphs, headings, tables,
  lists, columns) in reading order, each run carrying its `source_index` for
  lossless editing. Surfaces the existing `recon/` reconstruction pipeline
  (until now whole-document only, via `toModel()`) one page at a time, for
  continuous / lazily-virtualised editors. Routes through form XObject text so
  cerfa / invoice template text is reconstructed into blocks too.
- **Base-14 standard fonts in the rasterizer** — `renderPage` now draws the
  standard 14 fonts (Helvetica / Arial / Times / Courier / Symbol /
  ZapfDingbats) via a bundled metric-compatible face, **in memory only**
  (nothing is written to the PDF). Authoritative Symbol + ZapfDingbats
  `code → Unicode` tables (e.g. ZapfDingbats `0x34` → U+2714 ✔, not the digit).

### Changed

- **Form-field appearances reference the field's `/DA` standard font** (e.g.
  `/Helv`) instead of injecting a bundled font resource into the PDF. Filling a
  text field adds **no** font to the document; Adobe draws the standard font
  natively. A `/DA` font missing from `/DR` is registered as a bare base-14
  `/Type1` dict (no `FontFile`), exactly what a clean AcroForm carries.

### Fixed

- Embedded **Type1 / CFF subsets with a base-14 BaseFont** (e.g. `Times-Bold`)
  now render — they were drawn as `.notdef` (invisible) because the base-14
  substitution was applied even when the font embeds its own program. The
  substitute is now used only when no program is embedded.
- Glyph advances honour the PDF **`/Widths`** (`/W` + `/DW`, or `/Widths` +
  `/FirstChar`) authoritatively per ISO 32000-1 §9.2.4; the embedded-font
  advance is the fallback. Fixes collapsed / overlapping words on subset fonts
  whose charstring advances are degenerate.

## [0.59.0] - 2026-06-21

### Added — Universal font decoding (text extraction matches Adobe, zero OCR)

A per-font `code → Unicode` resolver, built once at font setup via the full
ISO 32000 §9.10 priority chain, now covers the real-world font matrix that made
`textElements()`/`structuredText()` emit `�` or dingbats before:

- **Standard Macintosh Glyph Ordering** — `/gNN` glyph names (in `/Differences`
  and via CID GID) resolve through the 258 standard Mac glyph names
  (`g49`→`N`, `g106`→`agrave`→`à`). This is how Adobe reads subset fonts that
  **MuPDF and poppler drop** to `�`.
- **Type1C (CFF) simple fonts** without `/ToUnicode` — fall back on the embedded
  **CFF charset** (`code → gid → SID → name → Unicode`), recovering accents
  (`à/è/ê/ç`) that were `�`.
- **AGL ligature names** `f_l`→`fl`, `f_i`→`fi`, `f_f_i`→`ffi` (recursive).
- **`/ToUnicode` CMap** (bfchar/bfrange, UTF-16BE, multi-char), **CID cmap/post**,
  **named/embedded CMaps**, base encodings (WinAnsi/MacRoman/Standard), digit
  names. Unmapped codes emit **nothing** (like Adobe), never an invented letter
  or a control char.

### Added — Coloured text extraction via named colour spaces

`textElements()` now resolves `/Separation`, `/ICCBased`, `/Indexed`, `/DeviceN`
fill colours (running the tint transform) for text, reusing the rasteriser's
`NamedColorResolver`. Text painted via `cs`/`sc`/`scn` is no longer reported as
black `[0,0,0]`.

### Added — Right-to-left (Hebrew/Arabic) logical order

RTL runs extracted in visual order are reordered to logical Unicode order
(`direction:"rtl"`), guarded against double-reversal via Hebrew final-form
position. `"רצואה / לארשי תנידמ"` → `"מדינת ישראל / האוצר"`.

### Fixed — AcroForm text-field appearance (`/AP`)

`setTextField()`/`setChoice()` now regenerate the widget's `/AP /N` appearance
stream (iterating `/Kids` widgets), honouring `/DA` (font size + colour) and
`/Q` quadding, so filled values render natively and in Adobe.

### Fixed — Split-word run joining

Adjacent runs on a line are joined unless separated by a real horizontal gap
(`"N om et adresse"` → `"Nom et adresse"`, `"ENFANT S"` → `"ENFANTS"`).

## [0.58.3] - 2026-06-21

### Fixed

- **Text extraction (`textElements()`, `structuredText()`) recovers far more
  characters from subset fonts with broken/partial `/ToUnicode`.** Type0
  (Identity-H) subsets whose `/ToUnicode` is affine but incomplete, and simple
  fonts using `/MacRomanEncoding` or `/Differences`, were decoded as raw WinAnsi
  — yielding U+FFFD (`�`) for characters that are perfectly *rendered* (the glyph
  is drawn; only the code→Unicode map is missing). Extraction now follows the
  ISO 32000 §9.10 priority: `/ToUnicode` → embedded `cmap`/`post` (`cid_to_gid`)
  → an auto-calibrated affine inference for partial `/ToUnicode` subsets →
  `/Encoding` base (WinAnsi/MacRoman/Standard) + `/Differences` resolved through
  the Adobe Glyph List. On a real 76-font form this cut U+FFFD from **243 to 25**
  per page (the 25 residual are codes that *no* source in the file maps — not
  recoverable by any reader). **Page rendering was already correct and is
  unchanged** — this only affects the extracted/editable text layer.

## [0.58.2] - 2026-06-21

### Fixed

- **Named colour spaces (`/Separation`, `/ICCBased`, `/Indexed`, `/DeviceN`) are
  now resolved when extracting vector paths (`elements()`, `vectorPaths()`).**
  Previously the content-layer vector extractor carried its own simplistic
  colour-space model (Device Gray/RGB/CMYK only): any *named* colour space set
  via `cs`/`CS` fell back to `Unknown` and `sc`/`scn` operands were guessed by
  arity, so a 1-component Separation tint was misread as grey — a blue spot/ICC
  fill rendered **black/grey**, and unresolvable fills were **dropped entirely**.
  Vector extraction now reuses the rasteriser's full colour pipeline
  (`raster/colorspace.rs` tint-transform via the PDF function evaluator,
  ICCBased by `/N`, Indexed palette lookup), resolving named spaces against the
  page `/Resources/ColorSpace`. Separation `/Black` tint `1.0` → true black,
  spot/ICC blues → their real RGB. The rasteriser path was already correct and
  is unchanged.

## [0.58.1] - 2026-06-21

### Fixed

- **`reorderElement` now preserves the element's effective graphics state
  (fill/stroke colour, line width, dash, font) so reordered shapes/text keep
  their appearance.** Previously the moved op range was re-wrapped in a *bare*
  `q … Q`, dropping the graphics state set *before* the element (fill colour via
  `rg`/`g`/`k` or `cs`+`scn`, stroke colour via `RG`/`G`/`K` or `CS`+`SCN`, line
  width `w`, dash `d`, caps/joins `J`/`j`/`M`, the active `/ExtGState` `gs`, and —
  for text — the font `Tf`). A red shape brought to the front would render black,
  etc. `reorderElement` now runs a last-write-wins scan over the operators
  preceding the element (honouring the `q`/`Q` save/restore stack) and re-emits
  the actually-set state operators inside the new `q … Q`, before the moved run,
  so the element renders identically at its new position; the trailing `Q` still
  restores, so neighbours are unaffected. Images (no colour state) are unchanged.

## [0.58.0] - 2026-06-21

### Added

- **`setElementOpacity(page, index, fillAlpha)` — constant opacity on *any*
  element.** Sets a single transparency value on a text, image **or** shape
  element in place by registering a page `/ExtGState` (`/ca` = `/CA` =
  `fillAlpha`, clamped to `0..=1`, auto-named `GpGs<n>`) and wrapping the
  element's op range in `q /<gs> gs … Q`, so the alpha applies to that run only
  and following content is unaffected. This is the way to set an **image**'s
  opacity in place; shapes may use either this or `setPathStyle`'s `fillAlpha` /
  `strokeAlpha` (same underlying `/ExtGState` mechanism — the difference is that
  `setElementOpacity` uses one value for both `/ca` and `/CA`, while `setPathStyle`
  can set fill and stroke alpha independently). New ABI
  `gp_set_element_opacity(handle, page, index, fill_alpha)` and core
  `Document::set_element_opacity` / `content::set_element_opacity`. Returns
  `false` for a missing page/index.
- **`reorderElement(page, index, toFront)` — native PDF stacking order.** Changes
  an element's paint (z) order by splicing its op range to the **end** of the
  content stream (`toFront = true` → painted last, on top) or to the **start**
  (`toFront = false` → painted first, behind everything). The moved range is
  re-wrapped in `q … Q` so it neither inherits nor leaks graphics state; works for
  text, image and shape elements. **The element's unified index changes after the
  splice — re-read `pageElements`.** New ABI `gp_reorder_element(handle, page,
  index, to_front)` and core `Document::reorder_element` /
  `content::reorder_element`. Returns `false` for a missing page/index.
- **`renderPageExcluding(page, indices, scale?)` — rasterise a page minus given
  elements.** Rasterises a page to PNG while **omitting** the listed top-level
  unified element `indices` (from `pageElements`) — each excluded element paints
  nothing (fills, strokes, shadings, images and text alike) while all
  non-excluded content renders normally. Generalises `renderPageNoText` (which
  suppresses *all* text); an empty `indices` renders the full page and unknown
  indices are ignored. Built for live-overlay editing — paint a background
  without the element currently being edited, then overlay an editable version on
  top. Native rasteriser, no third-party image library. New ABI
  `gp_render_page_excluding(handle, page, indices_ptr, indices_len, scale,
  out_len)` and core `Document::render_page_excluding`, alongside the unchanged
  `renderPage` / `renderPageNoText`.

### Changed

- **`setPathStyle` opacity is now real.** `fillAlpha` / `strokeAlpha` (`0..=1`)
  are now **fully applied** (previously accepted for API symmetry but a no-op):
  the engine registers an `/ExtGState` carrying `/ca` / `/CA` on the page and
  injects a `/<gs> gs` into the path's `q … Q` wrap, so the alpha applies to that
  path run only. The earlier "opacity not applied — needs an `/ExtGState`"
  limitation no longer holds. For non-path elements (e.g. images) use
  `setElementOpacity`.

## [0.57.0] - 2026-06-21

### Added

- **`transformElement(page, index, m)` — full affine transform of an element in
  place.** Generalises `moveElement` (a translate-only `[1,0,0,1,dx,dy]` matrix)
  to a complete PDF affine matrix `m = [a, b, c, d, e, f]` — scale, rotate, shear
  and translate — so an element can be moved **and** resized **and** rotated in a
  single call. Non-destructive: the element is wrapped in `q  a b c d e f cm  …
  Q`, so its internal coordinates are never rewritten and it behaves identically
  for text, images and shapes. New ABI `gp_transform_element(handle, page, index,
  a, b, c, d, e, f)` and core `Document::transform_element` /
  `content::transform_element`, alongside the existing `moveElement` /
  `gp_move_element` (kept). Returns `false` for a missing page/element.
- **`setPathStyle(page, index, style)` — in-place vector restyle.** Re-styles a
  **path** element (returns `false` for a non-path index) without touching its
  geometry: the path's op range is wrapped in `q … Q` and, for each provided
  field, an override operator is injected before the paint op — `fill`→`r g b rg`,
  `stroke`→`r g b RG`, `strokeWidth`→`w`, `dash`→`[…] 0 d`; omitted fields keep
  the inherited graphics state. `style = { fill?, stroke?, strokeWidth?,
  fillAlpha?, strokeAlpha?, dash? }`; colours are RGB `[r,g,b]` in `0..=1` and
  `dash` is the PDF dash array (`[]` = solid). New ABI
  `gp_set_path_style_json(handle, page, index, json_ptr, json_len)` and core
  `content::set_path_style` + `PathStyle` / `Document::set_path_style`. **Note:**
  `fillAlpha` / `strokeAlpha` are accepted for API symmetry but are **not**
  applied — PDF opacity requires a named `/ExtGState` resource, which a pure
  content-stream edit cannot create; use the resource-level shape APIs (whose
  `opacity` argument allocates the `/ExtGState`) when real transparency is needed.

## [0.56.0] - 2026-06-21

### Added

- **`renderPageNoText(page, scale?)` — text-free page raster.** Rasterise a page to
  PNG **without** its page-content text (glyphs from `Tj`/`'`/`"`/`TJ` are suppressed)
  while every non-text element — vectors, gradients/shadings, images and patterns —
  plus annotation/widget appearances are rendered in full. Built for editors that
  overlay real, editable text on top of a text-free background. Native rasteriser, no
  third-party image library. New ABI `gp_render_page_no_text(handle, page, scale,
  out_len)` and core `Document::render_page_no_text`, alongside the existing
  `renderPage` / `gp_render_page`.

## [0.55.1] - 2026-06-21

### Fixed

- **`imageToPdf` now embeds every PNG variant — no more empty buffer.** The pure-Rust
  PNG decoder only handled 8-bit, non-interlaced images, so any PNG with a 16-bit
  depth (common from screenshots and image editors), a sub-byte depth (1/2/4-bit
  greyscale and palette), or Adam7 interlacing was rejected — `imageToPdf` returned
  an **empty array** for those inputs. The decoder now supports the full PNG matrix:
  colour types 0/2/3/4/6 at bit depths 1, 2, 4, 8 and 16, both non-interlaced and
  interlaced, plus `tRNS` colour-key transparency for greyscale and truecolour
  images. Transparency (PNG alpha and transcoded GIF/WebP/AVIF alpha) is preserved
  via a `/DeviceGray` soft mask (`/SMask`), never flattened.

### Added

- **`imageToPdf(image)` — raster image → one-page PDF.** PNG, JPEG, GIF, WebP and
  AVIF are accepted (format auto-detected); the image is centred and scaled to fit
  on an A4 portrait page and embedded as a real `/Image` XObject. PNG/JPEG embed
  directly (JPEG verbatim via `/DCTDecode`); **GIF/WebP/AVIF are transcoded to PNG**
  first (native `gif`/`webp`/`avif` decode → PNG encode), since the embedder only
  writes PNG/JPEG XObjects. Returns an empty array for unrecognized bytes. Pure
  Rust/WASM — no third-party image library.
- **AVIF dimension probe.** The image-header reader now recognizes AVIF/HEIF-still
  containers and reads the canvas size from the `meta → iprp → ipco → ispe` box (a
  cheap header parse, no AV1 decode; falls back to a full decode for unusual box
  orderings), so `image_to_model` lowers an AVIF to a full-page image document too.
- **`mergePdfs(pdfs)` — concatenate several PDFs into one.** Appends each input's
  pages in order onto the first (empty list → empty bytes; single PDF → returned
  unchanged). Built on the existing `appendPages`.

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
