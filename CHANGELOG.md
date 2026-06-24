# Changelog

All notable changes to **gigapdf-lib** (the Rust engine + the
`@qrcommunication/gigapdf-lib` TypeScript SDK) are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/) and the project adheres
to [Semantic Versioning](https://semver.org/).

The per-release SDK detail also lives in [`sdk/CHANGELOG.md`](sdk/CHANGELOG.md).

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
