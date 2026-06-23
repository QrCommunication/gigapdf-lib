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
| **DOCX** | Rich | text & runs, bold/italic/underline/strike, font/size/colour/highlight, headings & named styles, alignment/indent/spacing, lists (marker + ordinal + nesting), tables (cells + shading), images (PNG/JPEG/WebP), external hyperlinks | **headers/footers**, footnotes/endnotes, comments, track-changes, embedded OLE; multi-row vMerge approximated; internal anchors → page 0 |
| **ODT** | Rich | text & runs, char styling, headings, lists, tables (cells), images, hyperlinks | **headers/footers**, paragraph align/indent/spacing, numbered lists become bullets, table spans/borders/widths |
| **XLSX** | Rich | cell values & types, **formulas** (kept as text), merged cells, multiple sheets, number formats, cell fills | per-cell **font/size/colour** (hardcoded default), column widths |
| **ODS** | Good | cell values, formulas (text), multiple sheets | merges, number formats, fills, column widths |
| **PPTX** | Good | slides, text boxes, shapes (geometry + rotation + groups), runs (bold/italic/colour), images, charts→table of cached data, SmartArt→bullet list | underline/strike/highlight, paragraph align/indent, lists-as-lists, run hyperlinks, **animations/transitions**, **speaker notes**, non-text autoshapes |
| **ODP** | Good | slides, text boxes, shapes (pos + groups), runs (full char styling), images | shape rotation, charts/SmartArt, animations, speaker notes, paragraph props |
| **DOC / XLS / PPT** (legacy OLE2) | **Text only** | flat plain text (largest stream, UTF‑16/ASCII) | **everything else** — styling, tables, sheets, slides, images, structure. A real binary reader is needed (tracked) |
| **Markdown** | Good | ATX headings, bold/italic/code, links, ordered/unordered nested lists, GFM tables, fenced code, blockquotes, HR | strikethrough `~~`, images `![]()`, task-lists, reference/footnote links, setext headings, inline HTML (pass through as text) |
| **CSV** | Full | quoting/escaping (RFC 4180), embedded delimiters/newlines, BOM, delimiter auto-detect, ragged rows padded | type inference (all cells are text), multi-sheet (CSV has none) |
| **RTF → PDF** | Rich | full char/para formatting, fonts, colours, tables, PNG/JPEG pictures | hyperlinks, WMF/EMF/BMP pictures, nested tables |
| **RTF → model** | **Text only** | plain paragraphs (uses the text parser, not the rich one — tracked) | all styling/tables/images/links |
| **HTML** | — | see [HTML-CSS.md](HTML-CSS.md) for the full HTML/CSS feature surface | — |

---

## 2. Export — editable model → file

| Target | Status | What survives | Main drops |
|--------|--------|---------------|------------|
| **DOCX** | Richest | paragraphs & runs, bold/italic/underline/strike, font/size/colour/highlight, super/sub, headings→styles, alignment/indent/spacing/line-height, lists (nesting), tables (spans, borders, widths, shading), images, inline images, external links | image format hard-coded **PNG** (JPEG/GIF → corrupt), internal page links, multi-section page setup |
| **ODT** | Rich | as DOCX for text/lists/images/links | table **borders & row height**, inline images, list nesting, super/sub, block shapes, image format (PNG only) |
| **PPTX / ODP** | Good | slides, text boxes, shapes, images, runs (bold/italic/colour), alignment | paragraph spacing/indent/line-height (PPTX), lists & tables flattened to paragraphs, run highlight, external links, super/sub, **speaker notes**, image format (PNG only) |
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
grouping, alignment, super/sub · ruled tables (with col/row spans) · images (with
lifted figure captions) · vector shapes · underline/strike (from drawn rules) ·
external + internal hyperlinks · outline/bookmarks · page geometry · tagged-PDF
`/StructTreeRoot` (consumed).

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
