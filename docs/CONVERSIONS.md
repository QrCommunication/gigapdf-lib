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
| **DOCX** | Rich | text & runs, bold/italic/underline/strike, font/size/colour/highlight, **superscript/subscript** (`w:vertAlign@val` `superscript`/`subscript` → `CharStyle.vertical_align`), headings, **named style table → `Document.styles`** (`word/styles.xml`: each `w:style w:type="paragraph"` → a `NamedStyle` keyed by `w:styleId`, with its own `w:pPr`/`w:rPr` lowered — align/spacing/indent/line-height + font/size/bold/italic/underline/colour — and `w:basedOn` kept as `based_on`, *not* flattened; `character`/`table`/`numbering` styles skipped) so each paragraph's `style_ref` (set from `w:pStyle`) resolves, paragraph alignment/indent/spacing/line-height, **hard page breaks** (`w:br w:type="page"`, `w:pPr/w:pageBreakBefore`, an intermediate `w:pPr/w:sectPr` → the body splits into several model pages), lists (**per-level marker/format** resolved through the full `numbering.xml` chain `w:num → w:abstractNumId → w:lvl@w:numFmt`, incl. `w:lvlOverride/w:lvl/w:numFmt` + nesting), tables (cells, **column widths, table & cell borders, cell shading, cell vertical alignment** (`w:tcPr/w:vAlign@w:val` `top`/`center`/`bottom` → `Cell.vertical_align`), **row height, grid/row spans**), images (PNG/JPEG/WebP), **drawing geometry & image alt text** (`w:drawing`: an inline `wp:inline` stays an `Inline::Image`, a floating `wp:anchor` is lifted to a positioned sibling `Block` whose `frame` carries the `wp:extent` **size** (EMU→pt) and, when a `wp:posOffset` is given, the absolute **position** — top-left `posOffset` flipped about the page height into the model's lower-left `Rect`; `wp:docPr@descr` then `@title` → `ImageRef.alt` for both inline and floating drawings), external hyperlinks, **document outline / TOC tree** (`Document.outline`) — built from heading paragraphs (`Heading1`..`Heading9`/`Title` styles **and/or** `w:pPr/w:outlineLvl`) nested by level (skipped levels tolerated), each entry targeting the page it lands on; **user bookmarks** (`w:bookmarkStart@w:name`) folded in as navigable anchors nested under their section (Word‑internal `_Toc…`/`_GoBack`/`_Ref…`/`_Hlk…` dropped); **full document metadata** (`docProps/core.xml` + `app.xml`, see *Document metadata* below), **running header & footer** (the body section's default `w:sectPr/w:headerReference`+`w:footerReference` `@w:type="default"` → the referenced `word/headerN.xml`/`footerN.xml` part, lowered through the **same body walker** so its paragraphs/tables/run-styling/inline-images keep their real formatting, into `Section.header`/`Section.footer`; header/footer images intern into the same `Document.resources`), **footnotes & endnotes** (`word/footnotes.xml`/`word/endnotes.xml`: each `w:footnoteReference`/`w:endnoteReference w:id` in a run lowers the matching note **inline at the reference point** — a superscript citation marker (the note's 1-based display ordinal) followed by the note body text — by re-walking the note body's `w:p`/`w:r` through the **same body walker** so its styling survives, then flattening to text; the synthetic `separator`/`continuationSeparator` placeholders are skipped; mirrors the ODT `text:note` lowering), **symbol runs** (`w:sym@w:font@w:char` → the symbol's Unicode glyph as a real text run: the common symbol fonts are mapped — `Symbol` (Greek + math/arrows), the `Wingdings`/`Wingdings 2`/`Wingdings 3` family and `Webdings` for their frequent glyphs (`Wingdings F0FC` → `✓`, `F0FB` → `✗`, bullets/arrows, …); an unmapped font/char falls back to the raw code point with Word's `F0xx → U+00xx` Private-Use folding, so the character is never dropped), **text boxes** (`w:txbxContent` from a DrawingML `wps:txbx`/`a:txBody` **or** legacy VML `v:textbox`, inside `w:drawing`/`w:pict` → a `BlockKind::TextBox` whose blocks are the box's paragraphs/tables lowered through the **same body walker** — run styling, lists, nested tables kept — lifted to a sibling block carrying the drawing's `wp:extent` size + `wp:posOffset` position as its `frame` when anchored/sized), **field codes** (`w:fldSimple@w:instr` and complex `w:fldChar` ranges `begin`/`separate`/`end` with `w:instrText`: a `HYPERLINK "url"` field wraps its cached **result** runs in an `Inline::Link` (a `\l` bookmark switch → page 0); `REF`/`PAGEREF`/`TOC`/`SEQ`/`STYLEREF`/`PAGE`/`NUMPAGES`/`DATE`/`TIME` and any unrecognised instruction emit the cached **result** text — the raw field code is never surfaced) | **Office Math** — `m:oMath` (OMML) equations are still **dropped** (no math-layout node in the model yet; the remaining open part of [#37](../../issues/37)); **per-page-type header/footer variants** — only the **default** running header/footer is lowered; the `first`-page and `even`-page references (`@w:type="first"`/`"even"`) collapse onto the default because [`Section`] holds a single `header`/`footer` slot (no per-page-type variant in the model); the inlined footnote/endnote **body is flattened to plain text** (per-run bold/italic *inside* the note isn't kept on the inlined run, same as ODT) and end- vs foot-notes aren't distinguished (one inline-note shape in the model); comments, track-changes, embedded OLE; multi-row vMerge approximated; internal **hyperlink** anchors (`w:anchor`) still resolve their *link target* to page 0 (the outline now records bookmark pages, but the `Inline::Link` jump isn't matched to them); **paragraph borders/shading (`w:pBdr`/`w:pPr/w:shd`)**, **tab stops (`w:tabs`)** and **per-cell borders** (no model slot — table border is single & table-wide); **floating-drawing wrap & z-order** — the wrap mode (`wp:wrapSquare`/`wrapTight`/`wrapTopAndBottom`/`wrapNone`/`wrapThrough`), the z-order flag (`@behindDoc`) and the `@relativeFrom` anchor reference have **no model slot** (`Block.frame` is one absolute `Rect` with no wrap/z-order/anchor-reference field), so the offset is treated as page-absolute; a `wp:align`-only anchor (no `wp:posOffset`) keeps its **size** but defaults its position to `0` (no absolute coordinate at this layer); an **inline** drawing has no size slot on `Inline::Image`, so its `wp:extent` size is not lowered (only the alt text is); **list numbering ordinals are positional** — a level's `w:start` / `w:lvlOverride/w:startOverride` (restart-at-N) and custom `w:lvlText` template (e.g. `%1)`, legal `%1.%2`) are **not** lowered (the model derives ordinals by position and renders ordered markers with a fixed `.` suffix — no start/template slot); a named style's **`w:name`** (human display name) has no model slot — the `StyleId` key carries the machine `w:styleId` |
| **ODT** | Rich | text & runs, char styling, **superscript/subscript** (`style:text-position` first token `super`/`sub`/signed `%` → `CharStyle.vertical_align`), headings, paragraph alignment/indent/spacing/line-height, lists (**ordered vs unordered + per-level number format** resolved from the list's `text:style-name` → `text:list-style`: a `text:list-level-style-number` level is ordered with its `style:num-format` `1`/`a`/`A`/`i`/`I` → `ListMarker` Decimal/LowerAlpha/UpperAlpha/LowerRoman/UpperRoman, a `text:list-level-style-bullet`/`-image` (or an unrecognised/absent style) stays an unordered bullet; resolved **per nesting level**, so a nested level can differ), tables (cells, **column & row cell spans** (`table:number-columns-spanned`/`number-rows-spanned` → `Cell.col_span`/`row_span`, with the trailing `table:covered-table-cell` fillers dropped — same merge model as DOCX/XLSX), **cell shading, cell vertical alignment** (`style:table-cell-properties@style:vertical-align` `top`/`middle`/`bottom` → `Cell.vertical_align`)), images, hyperlinks, **named style table → `Document.styles`** (`styles.xml` `office:styles`: each `style:style style:family="paragraph"` → a `NamedStyle` keyed by `style:name`, with its `style:paragraph-properties` + `style:text-properties` lowered and `style:parent-style-name` kept as `based_on`, *not* flattened; `text`/`table`/`graphic` families skipped) so each paragraph's `style_ref` (set from `text:style-name`) resolves, **document outline / TOC tree** (`Document.outline`) — built from `text:h@text:outline-level` (1..10) nested by level (skipped levels tolerated), plus **bookmarks** (`text:bookmark`/`text:bookmark-start@text:name`) as anchors nested under their section (`_`‑prefixed names dropped); the whole ODT is one model page so every entry targets page 0; **full document metadata** (`meta.xml`), **running header & footer** (the master page's `style:header`/`style:footer` in `styles.xml` `office:master-styles`, lowered through the **same body walker** so its paragraphs/tables/run-styling/inline-images keep their real formatting, into `Section.header`/`Section.footer`; the first master page's wins) | the ODF **left/first running variants** (`style:header-left`/`style:footer-left`/`style:header-first`/…) are not collapsed in (only the primary `style:header`/`style:footer` is lowered — the model has a single `header`/`footer` slot); list **numbering ordinals are positional** (an ODF `text:start-value` / per-level start and the `style:num-format` *prefix*/*suffix* templates have no model slot — the model derives ordinals by position and renders ordered markers with a fixed `.` suffix); table **borders** (no model slot — table border is single & table-wide); a named style's **`style:display-name`** (human label) has no model slot — the `StyleId` key carries `style:name` |
| **XLSX** | Rich | cell values & types, **formulas** (kept as text), merged cells, multiple sheets, number formats, cell fills, **per-cell character styling** (the cell's `xf@fontId` → the `<fonts>` record → `SheetCell.style` (`CharStyle`): family (`name`), size (`sz`, points), **bold** (`b`), **italic** (`i`), **underline** (`u`), and **colour** (`color`, resolved from `rgb` / `indexed` palette / `theme`+`tint`); gated by `applyFont` — absent/`1` applies the font, an explicit `0` keeps the default style), incl. **superscript/subscript** (font `vertAlign val="superscript"`/`"subscript"` → `CharStyle.vertical_align`), **cell vertical alignment** (`xf/alignment@vertical` `top`/`center`/`bottom` → `SheetCell.vertical_align`; absent ⇒ the OOXML default, bottom), **document metadata** (`docProps/core.xml`) | column widths |
| **ODS** | Good | cell values, formulas (text), multiple sheets, per-cell **superscript/subscript** (`style:text-position` → `CharStyle.vertical_align`), **cell vertical alignment** (`style:table-cell-properties@style:vertical-align` `top`/`middle`/`bottom` → `SheetCell.vertical_align`), **document metadata** (`meta.xml`) | merges, number formats, fills, column widths |
| **PPTX** | Good | slides, text boxes, shapes (geometry + rotation + groups), runs (bold/italic/colour), **superscript/subscript** (`a:rPr@baseline` per-mille: `>0` super, `<0` sub → `CharStyle.vertical_align`), images, charts→table of cached data, **slide-table cell vertical alignment** (`a:tc/a:tcPr@anchor` `t`/`ctr`/`b` → `Cell.vertical_align`), SmartArt→bullet list, **document metadata** (`docProps/core.xml`) | underline/strike/highlight, paragraph align/indent, lists-as-lists, run hyperlinks, **animations/transitions**, **speaker notes**, non-text autoshapes |
| **ODP** | Good | slides, text boxes, shapes (pos + groups), runs (full char styling), images, **document metadata** (`meta.xml`) | shape rotation, charts/SmartArt, animations, speaker notes, paragraph props |
| **ODG** | Good | OpenDocument **Graphics** (mimetype `…opendocument.graphics`); each `draw:page` of shapes is lowered through the **same slide/drawing path as ODP** — one model slide per drawing page, positioned `draw:frame`s → shapes (geometry from `svg:x/y/width/height`), text boxes → placeholders, images, page/master fill → slide background | same drops as ODP (shape rotation beyond the ODP set, charts, animations, layered connector/curve geometry) |
| **DOC / XLS / PPT** (legacy OLE2) | **Text only** | flat plain text (largest stream, UTF‑16/ASCII) | **everything else** — styling, tables, sheets, slides, images, structure. A real binary reader is needed (tracked) |
| **Markdown** | Good | ATX **and setext** headings, bold/italic/**strikethrough `~~`**/code, **inline `[t](url)` + reference `[t][ref]`/collapsed `[t][]`/shortcut `[t]` links** (resolved against `[ref]: url "title"` defs), **footnote refs `[^id]` → the `[^id]: …` body**, **inline images `![alt](url "title")`** (`Inline::Image` keyed by URL hash + alt, mirroring the HTML importer — local/`data:`/external all keep the reference), ordered/unordered nested lists, **GFM task-lists `- [ ]`/`- [x]`** (leading `☐`/`☑` glyph), GFM tables, fenced code, blockquotes, HR, **inline HTML phrasing tags** (`<b>`/`<strong>`, `<i>`/`<em>`, `<code>`, `<u>`, `<s>`/`<del>`, `<a href>`, `<br>`) **+ character references** (`&amp;`, `&#233;`…) | task-list state is a glyph (no boolean checkbox slot on `ListItem`); footnotes resolve inline (no separate footnote section/backref); inline HTML limited to the common phrasing tags (unknown tags drop, text kept); image bytes are not fetched/interned (URL reference only, as in HTML) |
| **CSV** | Full | quoting/escaping (RFC 4180), embedded delimiters/newlines, BOM, delimiter auto-detect, ragged rows padded | type inference (all cells are text), multi-sheet (CSV has none) |
| **RTF → PDF** | Rich | full char/para formatting, fonts, colours, tables, PNG/JPEG pictures, `\field` hyperlinks (`HYPERLINK "url"` → `<a href>`) | WMF/EMF/BMP pictures, nested tables |
| **RTF → model** | Rich | run-level char styling (bold/italic/underline/strike, `\cf` colour, `\fs` size, `\f` font family + serif/sans/mono generic, super/sub), paragraph alignment/indents, tables (`\trowd`/`\cell`/`\row` → `BlockKind::Table`), PNG/JPEG `\pict` images (**bytes interned** into `Document.resources.images`), `\field` hyperlinks (`HYPERLINK "url"` → `Inline::Link`) — routed through the **same rich parser** as RTF→PDF (no text-only fallback) | WMF/EMF/BMP & `\bin` pictures (no decoder), nested tables, list ordering/nesting (lowered as plain paragraphs) |
| **HTML** | — | see [HTML-CSS.md](HTML-CSS.md) for the full HTML/CSS feature surface | — |

> **Flat (single-file) ODF** — `.fodt` / `.fods` / `.fodp` / `.fodg` are also
> importable. These are one **uncompressed** `<office:document>` XML (inline
> `office:meta` + `office:styles` + `office:automatic-styles` + `office:body`)
> rather than a ZIP of parts. They are detected by the XML root element (and the
> `office:mimetype` attribute, falling back to the `office:body` child:
> `office:text` / `office:spreadsheet` / `office:presentation` / `office:drawing`)
> and routed through the **same** ODT/ODS/ODP/ODG lowering — fidelity is identical
> to the zipped form. Likewise `.odg` (zipped graphics) reuses the ODP path. No new
> API: the existing `office_to_model` / `office_to_pdf` / `office_needed_fonts`
> entry points accept these inputs unchanged.

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
| **DOCX** | Richest | paragraphs & runs, bold/italic/underline/strike, font/size/colour/highlight, super/sub, headings→styles, **named style table → `word/styles.xml` (one `w:style w:type="paragraph"` per `NamedStyle`: `w:name`, `w:pPr`/`w:rPr` from the style, `w:basedOn` from `based_on`) + paragraph `w:pStyle` references (`Paragraph.style_ref`)**, alignment/indent/spacing/line-height, lists (nesting), tables (spans, borders, widths, shading, **cell vertical alignment** `Cell.vertical_align` → `w:tcPr/w:vAlign`), images **in their real format** (`ImageResource.format` → the media part extension, `[Content_Types]` `Default` and `r:embed` relationship target all follow the blob — `jpeg`/`png`/`gif`/`bmp`/`tiff`/`webp`, unknown ⇒ `png`), inline images, external links | internal page links, multi-section page setup; a `NamedStyle` reusing a built-in id (`Normal`/`Heading1‑6`) is not re-emitted (the built-in keeps its defaults) |
| **ODT** | Rich | as DOCX for text/lists/links and **images in their real format** (`ImageResource.format` → the `Pictures/imgN.<ext>` name, `manifest.xml` media-type and `draw:image xlink:href` all follow the blob — `jpeg`/`png`/`gif`/`bmp`/`tiff`/`webp`, unknown ⇒ `png`); table **cell shading & cell vertical alignment** (`Cell.vertical_align` → `style:table-cell-properties@style:vertical-align`); **named style table → `office:styles` in `styles.xml` (one `style:style style:family="paragraph"` per `NamedStyle`: `style:name`/`style:display-name`, `style:parent-style-name` from `based_on`, para/text properties) + paragraph `text:style-name` references (`Paragraph.style_ref`; a paragraph with direct overrides gets an automatic style inheriting the named one via `style:parent-style-name`)** | table **borders & row height**, inline images, list nesting, super/sub, block shapes |
| **PPTX / ODP** | Good | slides, text boxes, shapes, images, runs (bold/italic/colour/**highlight** — `a:highlight` for PPTX, `fo:background-color` for ODP), alignment; **placeholder semantic roles round-trip** — a placeholder’s `PlaceholderRole` becomes `<p:ph type="title\|subTitle\|body">` (PPTX) or `presentation:class="title\|subtitle\|outline"` + `presentation:placeholder="true"` (ODP, ISO 26300 §9.6.1); unmapped roles keep their original ODF class token, and free (non-placeholder) shapes carry none; **slide tables round-trip as real tables** — a `Table` block becomes a DrawingML `p:graphicFrame`/`a:tbl` (PPTX) or a `draw:frame`/`table:table` (ODP) with the right rows/cols/cells, column widths, cell spans (`gridSpan`/`rowSpan` · `number-columns/rows-spanned`), cell shading and **cell vertical alignment** (`Cell.vertical_align` → `a:tc/a:tcPr@anchor` `t`/`ctr`/`b` for PPTX · `style:table-cell-properties@style:vertical-align` for ODP), not a paragraph flatten; PPTX emits a complete OPC layout chain — every slide references a `slideLayout` → `slideMaster` → `theme` (opens without a PowerPoint *repair* prompt); **speaker notes round-trip** — `Slide.notes` becomes a `ppt/notesSlides/notesSlideN.xml` (`p:notes`, body placeholder) with its `[Content_Types]` override and a bidirectional slide↔notesSlide relationship (PPTX), or a `presentation:notes` aside (`presentation:class="notes"`) inside the `draw:page` (ODP); **images in their real format** — `ImageResource.format` drives the media part extension + `[Content_Types]`/`r:embed` (PPTX) and the `Pictures/` name + `manifest.xml` media-type + `draw:image xlink:href` (ODP) (`jpeg`/`png`/`gif`/`bmp`/`tiff`/`webp`, unknown ⇒ `png`) | paragraph spacing/indent/line-height (PPTX), lists flattened to paragraphs, external links, super/sub |
| **XLSX / ODS** | Good | cell values & types, **cell formulas** (`SheetCell.formula` → XLSX `<f>…</f>` alongside the cached `<v>`, `t="str"` for a text result · ODS `table:formula="of:=…"` keeping the cached `office:value`; leading `=` stripped, round-trips), number formats, merged ranges, column widths, multiple sheets, bold/italic, **cell vertical alignment** (`SheetCell.vertical_align` → XLSX `xf/alignment@vertical` `top`/`center`/`bottom` · ODS `style:table-cell-properties@style:vertical-align` `top`/`middle`/`bottom`) | underline/strike, in-cell images |
| **HTML** | Full (semantic) | clean `<h1-6>/<p>/<ul>/<ol>/<table>` with colspan/rowspan + shading, styled `<span>` runs, `<a>`, `<img>` data-URI, `<pre><code>`, `<blockquote>`, sheets/slides; **vector `Shape` → self-contained inline `<svg>`** — geometry preserved (`viewBox` + `width`/`height` in pt from the path bounds, segments as `<path d>` with Y flipped to top-left origin), `fill` (`none` when unfilled), `stroke`/`stroke-width`/`stroke-dasharray` from the paint (a point-less/empty shape still falls back to a 1em box) | — |
| **Markdown** | Full (GFM) | headings, bold/italic/strike/code/underline/super-sub, links, images, nested+ordered lists, GFM tables, blockquotes, HR, code fences, YAML front-matter | run colour (no portable MD form), shapes |
| **EPUB** | Full (EPUB 3) | valid OCF, per-block XHTML (same fidelity as HTML export), embedded images, metadata; **vector `Shape` → self-contained inline `<svg>`** (same geometry→SVG mapping as the HTML export — `viewBox` + pt `width`/`height` from the path bounds, segments as `<path d>` with Y flipped, `fill`/`stroke`/`stroke-width`/`stroke-dasharray` from the paint; a point-less shape still falls back to a 1em box); **nested TOC** — the nav `<ol>` and NCX `navPoint`s nest the in-document heading hierarchy (H1→H2→H3…) under each chapter, with stable per-heading anchor ids (`text-N.xhtml#secN-hK`) emitted on the headings so the links resolve, and `dtb:depth` reflecting the deepest level; **unique, deterministic identifier** — `dc:identifier`/`unique-identifier` and the NCX `dtb:uid` agree on a `urn:gigapdf:<hex>` content hash (FNV-1a over the document's text + structure; no clock/RNG), so two different documents never collide while the same document is stable | inline-only CSS |
| **RTF** | Rich | char styling (bold/italic/underline/strike/size/colour/highlight), paragraph alignment, blockquote indent, HR; **tables → real `\trowd … \cellxN … \cell … \row` grids** (cell right-edges from `Table.col_widths`, pt→twips); **lists → ordered/unordered markers with nesting** (`\pard\liN\fi-360` per depth, `\bullet` for unordered, a running number `1.`/`a.`/`i.` for ordered — honours `List.ordered`/`List.marker`, cycling decimal→lower-alpha→lower-roman by depth when the marker isn't pinned, restarting per-level counters); **images → `{\pict …}`** (`\pngblip`/`\jpegblip` detected from the interned `Document.resources` bytes, `\picwgoal`/`\pichgoal` from the pixel size at 96 dpi, hex payload); **hyperlinks → `{\field{\*\fldinst HYPERLINK "url"}{\fldrslt …}}`** (the form the RTF importer reads back, round-tripping `Inline::Link`) | vector `Shape` geometry; image formats RTF can't carry (GIF/WebP/AVIF → skipped); an internal page link is emitted as a `#pageN` field target (no native RTF page jump) |
| **Plain text** | Good (model-tree) | **structure-preserving when a real model exists** — a document carrying a `/StructTreeRoot` (an authored Tagged PDF, or one produced by the Office importers / `to_tagged_pdf`) is rendered from the reconstructed model tree in reading order: headings/paragraphs become clean lines (blank line between top-level blocks); **lists keep their marker** (`- ` unordered · `1.`/`a.`/`i.` ordered per `List.marker`, re-numbered per nesting level) with **per-depth indentation** (2 spaces/level); **tables render as aligned columns** (each column padded to its widest cell, cells joined by a ` \| ` gutter, one row per line, header row included — *aligned* chosen over TSV for plain-text legibility); blockquotes are `> `-prefixed, code blocks verbatim, images/shapes a short `[image]`/`[shape]` placeholder. A **pure-PDF** document with no structure tree keeps the original **PDF-layer** path: one text run per line, form-feed (`\x0C`) between pages | table `col_span`/`row_span` not expanded into repeated cells (a spanned cell holds its single slot); per-run styling inside a cell/list-lead is flattened to text; the untagged PDF-layer fallback is still flat (no alignment / list markers — it has no model to walk) |
| **CSV** | Full | multi-sheet (concatenated), RFC 4180 quoting, CRLF | non-standard `#`-comment separators between sheets |
| **PDF/A** | Partial (b-level) | PDF/A-2b identification: XMP packet, sRGB OutputIntent + embedded ICC, deterministic `/ID` | **does not enforce** font embedding or strip forbidden constructs → a strict validator may reject; metadata hardcoded |

---

## 3. PDF → editable model (the basis of every PDF → X)

A PDF has no document structure — gigapdf-lib **reconstructs** it. This is
genuinely structure-aware and strong on the engine's own output and clean
single-column / ruled-table PDFs:

**Recovered well (FULL):** text runs with font family/size/colour · paragraph
grouping, alignment, super/sub · **multi-column reading order** (2- and 3-column
pages read column-by-column, not interleaved; robust to full-width
headings/figures bridging the gutter — see the column note below) · **run-level
rotated / vertical in-page text**
(the baseline angle from the text/CTM matrix is carried onto the reconstructed
block's rotation — `90°/180°/270°` snap to the exact cardinal, any other angle is
preserved free-form, and upright text stays unrotated) · ruled **and** borderless
tables (with col/row spans, including borderless merged cells inferred from text
geometry; large/sparse/long tables kept via a structural test; rotated tables
projected onto their reading axes) · images — both `Do` XObjects **and inline images**
(`BI`/`ID`/`EI`, ISO 32000-1 §8.9.7) — with lifted figure captions · vector
shapes · underline/strike (from drawn rules) · external + internal hyperlinks ·
outline/bookmarks · page geometry · **running headers/footers** (stripped from the
body flow and lifted to `Section.header`/`Section.footer`, see below) · tagged-PDF
`/StructTreeRoot` (consumed) · **optional-content (OCG/OCMD layer) visibility**
(see below).

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
`/LZW`, `/Fl` (Flate), `/RL` (RunLength), `/DCT` (baseline JPEG), `/CCF`
(CCITTFax, see below) — and colour spaces `/G`/`/RGB`/`/CMYK`/`/I` (plus Indexed).
`/IM true` image masks paint the current fill colour through the stencil. **Not
yet decoded:** `/JPX` (JPEG 2000).

**Bilevel scanned-document images (ISO 32000-1 §7.4.6 / §7.4.7):** the two fax /
bilevel filters used by N&B scans are now decoded from scratch (pure `std`, zero
deps) and flow through the normal 1-bpp image path — both for **rasterizing** and
**image extraction** (an XObject or inline image, an `/ImageMask` stencil, an
explicit `/Mask`, or a soft `/SMask`):

- **`/CCITTFaxDecode`** — Group 3 1-D (`/K 0`), Group 3 2-D (`/K > 0`, per-line
  1-D/2-D tag bit) and Group 4 (`/K < 0`, pure 2-D). The full modified-Huffman
  white/black run-length tables (terminating + make-up + the shared >1728 make-up
  codes), the 2-D modes (Pass, Horizontal, Vertical `V0`/`VR1‑3`/`VL1‑3`) via the
  `a0`/`a1`/`b1`/`b2` changing-element algorithm, and `/Columns`, `/Rows`,
  `/BlackIs1`, `/EncodedByteAlign`, `/EndOfLine`, `/EndOfBlock` (RTC/EOFB) are
  honoured.
- **`/JBIG2Decode`** — the embedded-in-PDF profile, with **full ITU-T T.88 segment
  coverage**: the segment-header parser, the MQ arithmetic decoder (Annex E `Qe`
  table + INITDEC/DECODE/RENORMD/BYTEIN), the integer arithmetic decoders
  (`IADH`/`IADW`/`IAEX`/`IADT`/`IAFS`/`IADS`/`IAIT`/`IARI`/`IARDX`/`IARDY` + the
  `IAID` symbol-id coder), and every region/dictionary segment type:
  - **generic region** (§6.2) — GB templates 0-3 with `TPGDON` typical prediction,
    plus an MMR mode that reuses the CCITT G4 core;
  - **generic refinement region** (§6.3) — GR templates 0 & 1 with `TPGRON`
    typical prediction, refining the page area in place;
  - **symbol dictionary** (§6.5) — arithmetic *and* Huffman coding, plain generic
    symbols *and* refinement/aggregate (`REFAGG`) symbols (single-symbol refinement
    and the aggregate text-region case) — including the Huffman + `REFAGG`
    combination (`SDHUFF=1` *and* `SDREFAGG=1`: the symbol-ID as a fixed
    `SBSYMCODELEN` code, the refinement deltas via the standard Annex B tables, the
    refinement bitmap arithmetic-coded);
  - **text region** (§6.4) — arithmetic *and* Huffman coding (the run-code-built
    symbol-ID table + the standard Annex B tables), with per-symbol refinement
    (`SBREFINE`/`IARI`), reference-corner and transposition handling;
  - **pattern dictionary** (§6.7) + **halftone region** (§6.6) — the collective
    pattern bitmap, the grayscale image decoded as Gray-coded generic-region
    bitplanes (§C.5) in **both** the arithmetic and the MMR mode (`HMMR=1`, all
    `HBPP` bitplanes recovered from the one bit-continuous G4 stream — not just the
    first plane), and grid placement with the combination operator;
  - the standard Huffman tables **B.1–B.15** and **custom table segments**
    (§7.4.13, run-code-built) — all composited onto the page bitmap with the
    segment combination operator (OR/AND/XOR/XNOR/REPLACE).

  A `/JBIG2Globals` stream in `/DecodeParms` (the shared symbol dictionary of a
  scanned document) is **fully resolved** — whether embedded inline *or* supplied
  as an *indirect reference*. The image-decode call site (which holds the object
  resolver) follows the reference, decodes that globals stream through its own
  filter chain, and feeds the resulting segments into the JBIG2 decoder; both the
  single-dictionary and array `/DecodeParms` forms are handled. A
  genuinely-unknown segment type is skipped (its region left blank) rather than
  aborting the page.

**Vertical writing mode (CJK, ISO 32000-1 §9.4.4 / §9.7.4.3):** a composite
(Type0) font whose `/Encoding` CMap selects vertical writing — a predefined `-V`
name (`Identity-V`, `UniJIS-UCS2-V`, …) **or** an embedded CMap stream declaring
`/WMode 1` — is now **laid out vertically**, both for text extraction (element
positions / hit-testing) and for rasterizing. The pen advances **top-to-bottom**
by each glyph's vertical displacement `w1y` (from the descendant CIDFont's `/W2`,
else `/DW2`, default −1000 ‰) instead of rightward by `w0`, and every glyph is
offset by its **position vector** `v` (`/W2` per-CID `[vx vy]`, else the `/DW2`
default `[w0/2, 880]` ‰) so it centres on the vertical baseline; `TJ` numeric
adjustments move along the vertical axis. Horizontal (`Identity-H` / `-H`) runs
are unchanged.

**Running headers & footers:** page furniture a PDF repeats on every page — a
running title in the top margin, a page number / rule in the bottom margin — is
**stripped from the body flow** so it no longer leaks into the prose (and thus
into every `toDocx()`/`toMarkdown()`/… export) once per page. After the per-page
blocks are reconstructed, each section's **top band** and **bottom band** (the
outer 12 % of the page) are scanned: a block whose normalized signature recurs
across a **strict majority** of the section's pages is treated as furniture,
removed from every page, and a single representative copy is lifted onto
`Section.header` / `Section.footer` (structure preserved, not just deleted). Page
numbers are folded to a common signature (`Page 1`/`Page 2` cluster) while
distinct numbered headings (`Chapter 1 Title`) stay separate, so the detector is
conservative: a single-page document, or one whose margins carry no repeated
content, is left untouched (header/footer `None`, body unchanged) — real
first-page content is never stripped. The lift runs before the heading-outline
fallback, so furniture never pollutes the table of contents either.

**Heading levels (clustered, stable):** a paragraph that is short and visually
prominent (font above the body size, or bold-and-short) is promoted to a heading,
and its **level** (`h1`..`h6`) now comes from **clustering the distinct
heading-candidate font sizes actually present** on the page — not fixed global
size ratios. The distinct sizes are sorted descending and grouped within a small
relative tolerance (sizes within ~6 % collapse to one level); the largest cluster
maps to `h1`, the next to `h2`, and so on, **monotonically with no skipped
levels** (a 24/18/14-over-11 pt document yields `h1/h2/h3`, never `h1/h3/h4`). A
heading only ~1.15× the body is detected as a heading (not missed, not forced to
`h6`); a bold run-in subhead at body size lands on the **deepest present** level
rather than always `h6`; a document with a single heading size yields one
consistent level for all. Body prose never enters the hierarchy, so ordinary text
is never promoted.

**Limits on arbitrary third-party PDFs (tracked in [#5](../../issues/5)):**

- **Tables**: detection now recovers **merged (spanning) cells in borderless
  tables** (a run whose box reaches across otherwise-empty columns/rows becomes a
  `col_span`/`row_span` cell, no phantom blank cell left behind — alongside the
  ruled path's missing-rule span inference), keeps **large / sparse / long tables**
  (the old flat caps of 14 cols / 160 cells / 28 % fill are replaced by a
  structural test — a wide or sparse grid is kept when most of its rows
  consistently span several columns, the signature of a real table; field-fence
  *forms* and running prose still fall back to paragraphs), and detects **rotated
  tables** (a table on a 90/180/270° page or region is projected onto its logical
  reading axes, so its rows/columns are found along the rotated direction and the
  cells emit in logical order with the table block oriented to match). Still
  missing: a **header-row (`<th>`/`thead`) concept** — no cell is marked as a
  header, so exports get `<td>` throughout.
- **Lists**: list detection is **ordinal-validated** — a run of ordered markers
  is only taken as a list when it forms a coherent sequence (consecutive/monotonic
  in one `1.`/`a)`/`i.` format, small gaps tolerated, starting at a plausible first
  ordinal), so numeric sentences, citations (`12. Smith et al.`), prices (`$5.99`)
  and stray section numbers fall back to prose instead of becoming phantom lists;
  a lone ordered marker is prose (a single bullet stays a one-item list), mixed
  formats don't merge, and nested ordinal sub-runs are validated on their own.
  Unordered bullets remain lists with no ordinal requirement.
- **Bold/italic** detected from the `/BaseFont` *name* **and** the font's
  `/FontDescriptor` (ISO 32000-1 Table 121), so the style survives when the name
  omits the tokens (subset-prefixed / renamed fonts): `/Flags` ForceBold (bit 19)
  ⇒ bold and Italic (bit 7) ⇒ italic, `/FontWeight ≥ 600` ⇒ bold, `/ItalicAngle
  ≠ 0` ⇒ italic, and — only as a conservative last resort when name/ForceBold/
  `/FontWeight` are all silent — `/StemV ≥ 120` ⇒ bold. Bold/italic are only ever
  *added*, never cleared, so name detection stays authoritative. The descriptor is
  read for both simple fonts and Type0 fonts (via the descendant CIDFont).
  Content-stream **faux-bold** (render-mode 2 `Tr` fill+stroke / double-stroke) is
  still not detected at this layer.
- **Columns**: multi-column detection is **robust to full-width lines**. Gutters
  are projected from a *robust majority* of the lines, not a unanimous vote: a body
  line far wider than the typical column line (and covering a real share of the
  measure) — a cross-column heading, pull-quote, wide figure caption or stray run —
  is set aside before the whitespace projection, so a *single* gutter-spanning line
  no longer welds two columns into one and scrambles the reading order. Such
  spanning lines (and explicit full-width banners) are folded back in **at their
  Y** as region breaks: a heading above two columns reads first, then the left
  column top→bottom, then the right; a mid-page spanning figure splits the column
  flow around it — reading order is `[full-width pre] [col1] [col2] [full-width
  post] …`, never interleaved. A genuinely sparse column (ordinary-width lines)
  survives the split, and a true single-column page is never falsely split by
  coincidental whitespace. Generalises to 2 and 3 columns.
- **Lines**: runs are grouped into baseline lines on a **width-weighted centroid**,
  not the first run that happened to be sorted. The band tracks the line's dominant
  body text, so a line that *opens* with a superscript / small-cap / footnote-marker
  run is no longer anchored on that outlier — the following body run still falls in
  the band instead of starting a spurious new line. A **second overlap-merge pass**
  then rejoins any fragment that still split off (an inline superscript/subscript, a
  formula run, a mixed-size glyph) to the line it belongs to, judged on the runs'
  real vertical extents (top/bottom from font size), not a centre point. The merge
  is conservative: it fires only when a fragment is **small/partial** relative to the
  line it joins *and* their extents overlap past a threshold, so two adjacent
  full-height body lines never fuse. Horizontal reading order (left→right) is
  preserved within the merged line. Net effect: superscripts and mixed font sizes
  are no longer mis-split or mis-merged.

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
