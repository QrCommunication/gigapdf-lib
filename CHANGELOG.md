# Changelog

All notable changes to **gigapdf-lib** (the Rust engine + the
`@qrcommunication/gigapdf-lib` TypeScript SDK) are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/) and the project adheres
to [Semantic Versioning](https://semver.org/).

The per-release SDK detail also lives in [`sdk/CHANGELOG.md`](sdk/CHANGELOG.md).

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
