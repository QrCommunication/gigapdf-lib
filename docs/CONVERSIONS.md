# Conversions — fidelity reference

gigapdf-lib converts between PDF, the Office/OpenDocument family, HTML, RTF,
Markdown, CSV, plain text and EPUB **with no third-party office/conversion
library** — every reader and writer is hand-written in Rust. This page is the
**honest, code-grounded** account of what each conversion preserves and what it
drops, so you know what to expect. The live list of gaps being closed is tracked
in the issues linked at the bottom.

## How a conversion flows

Everything routes through one **unified editable model** (`Giga*` block types —
headings, paragraphs/runs, lists, tables, sheets, slides, shapes, images, links):

```
import:   file ──►  model        (officeToModel, htmlToModel, csv/md/rtf)
export:   model ──► file         (modelToDocx / …Xlsx / …Pptx / …Odt / …Html / …Md / …Csv / …Epub / …Rtf)
PDF → X:  PDF ──► model ──► file  (pdf.toDocx() etc. — reconstruct, then export)
```

> **Key consequence:** the quality of every `pdf.toDocx()/toHtml()/toMarkdown()/…`
> is capped by the **PDF → model reconstruction** step (§3). Conversions
> *starting from the model* (an imported Office file, generated content) are
> higher-fidelity than conversions *starting from an arbitrary third-party PDF*.

---

## 1. Import — file → editable model

| Source | Status | What survives | Main drops |
|--------|--------|---------------|------------|
| **DOCX** | Rich | text & runs, bold/italic/underline/strike, font/size/colour/highlight, headings & named styles, paragraph alignment/indent/spacing/line-height, **hard page breaks** (`w:br w:type="page"`, `w:pPr/w:pageBreakBefore`, an intermediate `w:pPr/w:sectPr` → the body splits into several model pages), lists (**per-level marker/format** resolved through the full `numbering.xml` chain `w:num → w:abstractNumId → w:lvl@w:numFmt`, incl. `w:lvlOverride/w:lvl/w:numFmt` + nesting), tables (cells, **column widths, table & cell borders, cell shading, row height, grid/row spans**), images (PNG/JPEG/WebP), external hyperlinks, **document outline / TOC tree** (`Document.outline`) — built from heading paragraphs (`Heading1`..`Heading9`/`Title` styles **and/or** `w:pPr/w:outlineLvl`) nested by level (skipped levels tolerated), each entry targeting the page it lands on; **user bookmarks** (`w:bookmarkStart@w:name`) folded in as navigable anchors nested under their section (Word‑internal `_Toc…`/`_GoBack`/`_Ref…`/`_Hlk…` dropped); **full document metadata** (`docProps/core.xml` + `app.xml`, see *Document metadata* below) | **headers/footers**, footnotes/endnotes, comments, track-changes, embedded OLE; multi-row vMerge approximated; internal **hyperlink** anchors (`w:anchor`) still resolve their *link target* to page 0 (the outline now records bookmark pages, but the `Inline::Link` jump isn't matched to them); **paragraph borders/shading (`w:pBdr`/`w:pPr/w:shd`)**, **tab stops (`w:tabs`)**, **cell vertical alignment (`w:vAlign`)** and **per-cell borders** (no model slot — table border is single & table-wide); **list numbering ordinals are positional** — a level's `w:start` / `w:lvlOverride/w:startOverride` (restart-at-N) and custom `w:lvlText` template (e.g. `%1)`, legal `%1.%2`) are **not** lowered (the model derives ordinals by position and renders ordered markers with a fixed `.` suffix — no start/template slot) |
| **ODT** | Rich | text & runs, char styling, headings, lists, tables (cells), images, hyperlinks, **document outline / TOC tree** (`Document.outline`) — built from `text:h@text:outline-level` (1..10) nested by level (skipped levels tolerated), plus **bookmarks** (`text:bookmark`/`text:bookmark-start@text:name`) as anchors nested under their section (`_`‑prefixed names dropped); the whole ODT is one model page so every entry targets page 0; **full document metadata** (`meta.xml`) | **headers/footers**, paragraph align/indent/spacing, numbered lists become bullets, table spans/borders/widths |
| **XLSX** | Rich | cell values & types, **formulas** (kept as text), merged cells, multiple sheets, number formats, cell fills, **document metadata** (`docProps/core.xml`) | per-cell **font/size/colour** (hardcoded default), column widths |
| **ODS** | Good | cell values, formulas (text), multiple sheets, **document metadata** (`meta.xml`) | merges, number formats, fills, column widths |
| **PPTX** | Good | slides, text boxes, shapes (geometry + rotation + groups), runs (bold/italic/colour), images, charts→table of cached data, SmartArt→bullet list, **document metadata** (`docProps/core.xml`) | underline/strike/highlight, paragraph align/indent, lists-as-lists, run hyperlinks, **animations/transitions**, **speaker notes**, non-text autoshapes |
| **ODP** | Good | slides, text boxes, shapes (pos + groups), runs (full char styling), images, **document metadata** (`meta.xml`) | shape rotation, charts/SmartArt, animations, speaker notes, paragraph props |
| **DOC / XLS / PPT** (legacy OLE2) | **Text only** | flat plain text (largest stream, UTF‑16/ASCII) | **everything else** — styling, tables, sheets, slides, images, structure. A real binary reader is needed (tracked) |
| **Markdown** | Good | ATX headings, bold/italic/code, links, ordered/unordered nested lists, GFM tables, fenced code, blockquotes, HR | strikethrough `~~`, images `![]()`, task-lists, reference/footnote links, setext headings, inline HTML (pass through as text) |
| **CSV** | Full | quoting/escaping (RFC 4180), embedded delimiters/newlines, BOM, delimiter auto-detect, ragged rows padded | type inference (all cells are text), multi-sheet (CSV has none) |
| **RTF → PDF** | Rich | full char/para formatting, fonts, colours, tables, PNG/JPEG pictures | hyperlinks, WMF/EMF/BMP pictures, nested tables |
| **RTF → model** | **Text only** | plain paragraphs (uses the text parser, not the rich one — tracked) | all styling/tables/images/links |
| **HTML** | — | see [HTML-CSS.md](HTML-CSS.md) for the full HTML/CSS feature surface | — |

### Document metadata

Office import reads the container's metadata part **in full** into the model's
`DocMeta`, closing the round-trip with the exporter (which already *writes* these
parts from `DocMeta`). For OOXML both `docProps/core.xml` and `docProps/app.xml`
are read; for ODF, `meta.xml`. The mapping:

| `DocMeta` field | OOXML | ODF (`meta.xml`) |
|-----------------|-------|------------------|
| `title`            | `dc:title`     | `dc:title` |
| `author`           | `dc:creator`   | `dc:creator` |
| `subject`          | `dc:subject`   | `dc:subject` |
| `keywords`         | `cp:keywords` (one string, split on `,`/`;`) | each `meta:keyword` element |
| `lang`             | `dc:language`  | `dc:language` |
| `description`      | `dc:description` | `dc:description` |
| `created`          | `dcterms:created`  | `meta:creation-date` |
| `modified`         | `dcterms:modified` | `dc:date` |
| `last_modified_by` | `cp:lastModifiedBy` | — |
| `revision`         | `cp:revision`  | — |
| `application`      | `app.xml` `<Application>` | — |
| `company`          | `app.xml` `<Company>` | — |
| `generator`        | — | `meta:generator` |
| `editing_cycles`   | — | `meta:editing-cycles` |

Dates are stored as their raw ISO-8601 / W3CDTF source text (no date type is
introduced); `revision` and `editing_cycles` are likewise kept verbatim as
strings. A missing or empty metadata part yields a default (empty) `DocMeta` —
never an error. All of this then flows through to any re-export and to the JSON
model (`officeToModel` / `gp_model_from_office`).

---

## 2. Export — editable model → file

| Target | Status | What survives | Main drops |
|--------|--------|---------------|------------|
| **DOCX** | Richest | paragraphs & runs, bold/italic/underline/strike, font/size/colour/highlight, super/sub, headings→styles, **named style table → `word/styles.xml` (one `w:style w:type="paragraph"` per `NamedStyle`: `w:name`, `w:pPr`/`w:rPr` from the style, `w:basedOn` from `based_on`) + paragraph `w:pStyle` references (`Paragraph.style_ref`)**, alignment/indent/spacing/line-height, lists (nesting), tables (spans, borders, widths, shading), images, inline images, external links | image format hard-coded **PNG** (JPEG/GIF → corrupt), internal page links, multi-section page setup; a `NamedStyle` reusing a built-in id (`Normal`/`Heading1‑6`) is not re-emitted (the built-in keeps its defaults) |
| **ODT** | Rich | as DOCX for text/lists/images/links; **named style table → `office:styles` in `styles.xml` (one `style:style style:family="paragraph"` per `NamedStyle`: `style:name`/`style:display-name`, `style:parent-style-name` from `based_on`, para/text properties) + paragraph `text:style-name` references (`Paragraph.style_ref`; a paragraph with direct overrides gets an automatic style inheriting the named one via `style:parent-style-name`)** | table **borders & row height**, inline images, list nesting, super/sub, block shapes, image format (PNG only) |
| **PPTX / ODP** | Good | slides, text boxes, shapes, images, runs (bold/italic/colour/**highlight** — `a:highlight` for PPTX, `fo:background-color` for ODP), alignment; **placeholder semantic roles round-trip** — a placeholder’s `PlaceholderRole` becomes `<p:ph type="title\|subTitle\|body">` (PPTX) or `presentation:class="title\|subtitle\|outline"` + `presentation:placeholder="true"` (ODP, ISO 26300 §9.6.1); unmapped roles keep their original ODF class token, and free (non-placeholder) shapes carry none; **slide tables round-trip as real tables** — a `Table` block becomes a DrawingML `p:graphicFrame`/`a:tbl` (PPTX) or a `draw:frame`/`table:table` (ODP) with the right rows/cols/cells, column widths, cell spans (`gridSpan`/`rowSpan` · `number-columns/rows-spanned`) and cell shading, not a paragraph flatten; PPTX emits a complete OPC layout chain — every slide references a `slideLayout` → `slideMaster` → `theme` (opens without a PowerPoint *repair* prompt) | paragraph spacing/indent/line-height (PPTX), lists flattened to paragraphs, external links, super/sub, **speaker notes**, image format (PNG only) |
| **XLSX / ODS** | Good | cell values & types, number formats, merged ranges, column widths, multiple sheets, bold/italic | **cell formulas** (only the cached value is written), underline/strike, in-cell images |
| **HTML** | Full (semantic) | clean `<h1-6>/<p>/<ul>/<ol>/<table>` with colspan/rowspan + shading, styled `<span>` runs, `<a>`, `<img>` data-URI, `<pre><code>`, `<blockquote>`, sheets/slides | vector `Shape` → a 1em bordered box (geometry lost) |
| **Markdown** | Full (GFM) | headings, bold/italic/strike/code/underline/super-sub, links, images, nested+ordered lists, GFM tables, blockquotes, HR, code fences, YAML front-matter | run colour (no portable MD form), shapes |
| **EPUB** | Full (EPUB 3) | valid OCF, per-block XHTML (same fidelity as HTML export), embedded images, metadata, nav + NCX | **flat TOC** (one chapter per `Section`, depth 1), non-unique identifier, inline-only CSS |
| **RTF** | Partial | char styling (bold/italic/underline/strike/size/colour/highlight), paragraph alignment, blockquote indent, HR | tables → tab-separated lines, lists → `\bullet` (no ordering/nesting), images, hyperlinks |
| **Plain text** | Partial | reading-order text, form-feed between pages | runs from the **PDF layer**, not the model tree — tables aren't aligned/TSV, list markers lost |
| **CSV** | Full | multi-sheet (concatenated), RFC 4180 quoting, CRLF | non-standard `#`-comment separators between sheets |
| **PDF/A** | Partial (b-level) | PDF/A-2b identification: XMP packet, sRGB OutputIntent + embedded ICC, deterministic `/ID` | **does not enforce** font embedding or strip forbidden constructs → a strict validator may reject; metadata hardcoded |

---

## 3. PDF → editable model (the basis of every PDF → X)

A PDF has no document structure — gigapdf-lib **reconstructs** it. This is
genuinely structure-aware and strong on the engine's own output and clean
single-column / ruled-table PDFs:

**Recovered well (FULL):** text runs with font family/size/colour · paragraph
grouping, alignment, super/sub · **run-level rotated / vertical in-page text**
(the baseline angle from the text/CTM matrix is carried onto the reconstructed
block's rotation — `90°/180°/270°` snap to the exact cardinal, any other angle is
preserved free-form, and upright text stays unrotated) · ruled tables (with
col/row spans) · images — both `Do` XObjects **and inline images**
(`BI`/`ID`/`EI`, ISO 32000-1 §8.9.7) — with lifted figure captions · vector
shapes · underline/strike (from drawn rules) · external + internal hyperlinks ·
outline/bookmarks · page geometry · tagged-PDF `/StructTreeRoot` (consumed) ·
**optional-content (OCG/OCMD layer) visibility** (see below).

**Optional content (layers, ISO 32000-1 §8.11):** when rendering an existing
PDF, content on a layer that is **OFF in the default configuration**
(`/OCProperties /D`, honouring `/BaseState` + the `/ON`/`/OFF` overrides) is
**not** rasterized. A `/OC … BDC … EMC` marked sequence whose group (an OCG, or
an `/OCMD` resolved through its `/P` policy — `AnyOn`/`AllOn`/`AnyOff`/`AllOff`)
is hidden has its drawing operators skipped, with nested `BDC`/`EMC` tracked on a
visibility stack (an inner ON layer stays hidden under an OFF ancestor); a `Do`
XObject carrying a hidden `/OC` is likewise omitted. A PDF without
`/OCProperties` renders everything, unchanged.

**Inline images** are decoded through the *same* pipeline as image XObjects: the
abbreviated dictionary keys (`/W`, `/H`, `/BPC`, `/CS`, `/F`, `/IM`, `/D`, `/DP`,
`/I`) are expanded to their long names, the `ID`/`EI` boundary is found by the
exact sample length when unfiltered (so a literal `EI` inside the pixel bytes
never truncates them) and by a whitespace-delimited `EI` scan otherwise, and the
samples run through the engine's filters — `/AHx` (ASCIIHex), `/A85` (ASCII85),
`/LZW`, `/Fl` (Flate), `/RL` (RunLength), `/DCT` (baseline JPEG) — and colour
spaces `/G`/`/RGB`/`/CMYK`/`/I` (plus Indexed). `/IM true` image masks paint the
current fill colour through the stencil. **Not yet decoded:** `/CCF`
(CCITTFaxDecode) — no engine decoder exists, so such inline images are skipped
(the same limitation applies to CCITT XObjects).

**Limits on arbitrary third-party PDFs (tracked in [#5](../../issues/5)):**

- **Running headers/footers are not stripped** → page numbers / running titles
  leak into the body on every page.
- **Heading levels** use fixed size buckets → can be non-monotonic / skip levels.
- **Tables**: no header-row (`<th>`) concept; borderless merged cells forced 1×1;
  very sparse / very wide (>14 cols) / very long (>160 cells) / rotated tables are
  dropped.
- **Lists**: any `1.` / `a)` token is accepted (no ordinal-sequence check) →
  numeric sentences can become phantom lists.
- **Bold/italic** detected only from the `/BaseFont` *name* (no FontDescriptor
  flags, no faux-bold).
- **Columns**: whitespace-gutter detection only — a single wide line can collapse
  two columns into one and scramble reading order.

---

## Tracking — the full gap roadmap

Every limitation above is itemised, with file references and priorities, in:

| Issue | Area |
|-------|------|
| [#2](../../issues/2) | Office **export** (model → DOCX/PPTX/XLSX/ODT/ODS/ODP) |
| [#3](../../issues/3) | Office **import** (DOCX/DOC/XLSX/XLS/PPTX/PPT/ODT/ODS/ODP → model) |
| [#4](../../issues/4) | Markdown / CSV / RTF / HTML / Text / EPUB / PDF-A |
| [#5](../../issues/5) | PDF → editable-model **reconstruction** (caps all PDF → X) |
| [#1](../../issues/1) | HTML/CSS → PDF renderer |

For the per-method SDK signatures see [SDK.md](SDK.md); for runnable recipes see
[COOKBOOK.md](COOKBOOK.md).
