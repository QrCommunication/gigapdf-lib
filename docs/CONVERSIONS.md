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
| **DOCX** | Rich | text & runs, bold/italic/underline/strike, font/size/colour/highlight, **superscript/subscript** (`w:vertAlign@val` `superscript`/`subscript` → `CharStyle.vertical_align`), headings, **named style table → `Document.styles`** (`word/styles.xml`: each `w:style w:type="paragraph"` → a `NamedStyle` keyed by `w:styleId`, with its own `w:pPr`/`w:rPr` lowered — align/spacing/indent/line-height + font/size/bold/italic/underline/colour — and `w:basedOn` kept as `based_on`, *not* flattened; `character`/`table`/`numbering` styles skipped) so each paragraph's `style_ref` (set from `w:pStyle`) resolves, paragraph alignment/indent/spacing/line-height, **hard page breaks** (`w:br w:type="page"`, `w:pPr/w:pageBreakBefore`, an intermediate `w:pPr/w:sectPr` → the body splits into several model pages), lists (**per-level marker/format** resolved through the full `numbering.xml` chain `w:num → w:abstractNumId → w:lvl@w:numFmt`, incl. `w:lvlOverride/w:lvl/w:numFmt` + nesting), tables (cells, **column widths, table & cell borders, cell shading, cell vertical alignment** (`w:tcPr/w:vAlign@w:val` `top`/`center`/`bottom` → `Cell.vertical_align`), **row height, grid/row spans** (`w:gridSpan` → `Cell.col_span`; `w:vMerge` vertical merges resolve to a **real** `Cell.row_span` — a `w:val="restart"` cell counts the consecutive rows below whose cell in the **same grid column** carries a continuation `w:vMerge`, so `row_span = 1 + continuation rows`, the covered continuation cells are dropped, and a combined `w:gridSpan`+`w:vMerge` keeps both spans; grid-column tracking accounts for `w:gridSpan` widths so a wider cell doesn't mis-align the match), **header rows** (`w:trPr/w:tblHeader` — the repeat-atop-each-page flag — → `Row.is_header`; a `w:val="0"`/`"false"` cancels it)), images (PNG/JPEG/WebP **kept verbatim**; **embedded WMF/EMF metafiles are rasterized to PNG** via the in-house `convert::metafile` GDI decoder so the picture survives into the model/render/export; **GIF/BMP/TIFF/SVG embedded media are preserved with their true format tag** — the resource format is sniffed from the blob's **magic bytes** (filename extension only as a fallback), so a part with a wrong/assumed extension is no longer dropped or mis-tagged; the same interning path serves every Office family — DOCX/PPTX/XLSX/ODT/ODP/ODS), **drawing geometry & image alt text** (`w:drawing`: an inline `wp:inline` stays an `Inline::Image`, a floating `wp:anchor` is lifted to a positioned sibling `Block` whose `frame` carries the `wp:extent` **size** (EMU→pt) and, when a `wp:posOffset` is given, the absolute **position** — top-left `posOffset` flipped about the page height into the model's lower-left `Rect`; `wp:docPr@descr` then `@title` → `ImageRef.alt` for both inline and floating drawings), external hyperlinks, **document outline / TOC tree** (`Document.outline`) — built from heading paragraphs (`Heading1`..`Heading9`/`Title` styles **and/or** `w:pPr/w:outlineLvl`) nested by level (skipped levels tolerated), each entry targeting the page it lands on; **user bookmarks** (`w:bookmarkStart@w:name`) folded in as navigable anchors nested under their section (Word‑internal `_Toc…`/`_GoBack`/`_Ref…`/`_Hlk…` dropped); **full document metadata** (`docProps/core.xml` + `app.xml`, see *Document metadata* below), **running header & footer** (the body section's default `w:sectPr/w:headerReference`+`w:footerReference` `@w:type="default"` → the referenced `word/headerN.xml`/`footerN.xml` part, lowered through the **same body walker** so its paragraphs/tables/run-styling/inline-images keep their real formatting, into `Section.header`/`Section.footer`; header/footer images intern into the same `Document.resources`), **footnotes & endnotes** (`word/footnotes.xml`/`word/endnotes.xml`: each `w:footnoteReference`/`w:endnoteReference w:id` in a run lowers the matching note **inline at the reference point** — a superscript citation marker (the note's 1-based display ordinal) followed by the note body text — by re-walking the note body's `w:p`/`w:r` through the **same body walker** so its styling survives, then flattening to text; the synthetic `separator`/`continuationSeparator` placeholders are skipped; mirrors the ODT `text:note` lowering), **symbol runs** (`w:sym@w:font@w:char` → the symbol's Unicode glyph as a real text run: the common symbol fonts are mapped — `Symbol` (Greek + math/arrows), the `Wingdings`/`Wingdings 2`/`Wingdings 3` family and `Webdings` for their frequent glyphs (`Wingdings F0FC` → `✓`, `F0FB` → `✗`, bullets/arrows, …); an unmapped font/char falls back to the raw code point with Word's `F0xx → U+00xx` Private-Use folding, so the character is never dropped), **text boxes** (`w:txbxContent` from a DrawingML `wps:txbx`/`a:txBody` **or** legacy VML `v:textbox`, inside `w:drawing`/`w:pict` → a `BlockKind::TextBox` whose blocks are the box's paragraphs/tables lowered through the **same body walker** — run styling, lists, nested tables kept — lifted to a sibling block carrying the drawing's `wp:extent` size + `wp:posOffset` position as its `frame` when anchored/sized), **field codes** (`w:fldSimple@w:instr` and complex `w:fldChar` ranges `begin`/`separate`/`end` with `w:instrText`: a `HYPERLINK "url"` field wraps its cached **result** runs in an `Inline::Link` (a `\l` bookmark switch makes it an in-document jump that resolves to the target bookmark's page — see *internal hyperlink anchors* below); `REF`/`PAGEREF`/`TOC`/`SEQ`/`STYLEREF`/`PAGE`/`NUMPAGES`/`DATE`/`TIME` and any unrecognised instruction emit the cached **result** text — the raw field code is never surfaced), **internal hyperlink anchors → their bookmark's page** (`w:hyperlink@w:anchor` and a `HYPERLINK \l "name"` field name a bookmark; a `name → landing-page` map is built across the whole document during the walk and the `Inline::Link` target is resolved to that page in a second pass, so a **forward reference** — link before its `w:bookmarkStart@w:name` — resolves too; an anchor matching **no** bookmark keeps a `#name` placeholder rather than collapsing to a false page 0), **Office Math equations → linear Unicode math** (`m:oMath` / `m:oMathPara` (OMML) — formerly dropped — are now **linearized** to readable Unicode-math **text runs** via `convert::omml::lower_omml`: a math run (`m:r`/`m:t`) keeps its actual Unicode math-italic/Greek/operator codepoints; a fraction `m:f` → `(num)/(den)` (single-token parts left un-parenthesized, e.g. `a/b`); a radical `m:rad` → `√(e)`, `∛`/`∜` for degree 3/4, else `(deg)√(e)`; super/subscripts `m:sSup`/`m:sSub`/`m:sSubSup` → Unicode `xⁿ`/`xₙ` when every char maps (digits, `+ − = ( )`, common letters), else the linear `base^(exp)`/`base_(sub)` fallback; n-aries `m:nary` → `chr_(sub)^(sup) e` honouring `m:naryPr@m:chr` (`∑ ∏ ∫ ∮ ⋃ ⋂`, default `∫`) — e.g. `∑ᵢ₌₁ⁿ k`; delimiters `m:d` → the `m:dPr` begin/sep/end chars (default `()`); functions `m:func` → `fName(e)`; accents `m:acc` → the combining accent char (default combining circumflex); bars `m:bar` → combining over/under-line; limits `m:limLow`/`m:limUpp` → `e_(lim)`/`e^(lim)`; matrices `m:m` → `[r1c1, r1c2; r2c1, r2c2]`; the transparent wrappers `m:groupChr`/`m:box`/`m:eqArr`/`m:borderBox`/`m:phant` and any **unknown** element recurse into their children, so **no sub-expression is ever dropped**; an **inline** `m:oMath` joins the run flow at its position, a display `m:oMathPara` becomes its **own** paragraph block) | **Office Math — true 2D visual layout is deferred** (issue [#37](../../issues/37) closed via the linearization above): stacked fraction bars, the radical vinculum, and positioned super/subscripts & over/under limits would need a full maths typesetting engine and are **out of scope** — the equation **content survives as readable linear Unicode-math text**, not a 2D visual layout; **per-page-type header/footer variants** — only the **default** running header/footer is lowered; the `first`-page and `even`-page references (`@w:type="first"`/`"even"`) collapse onto the default because [`Section`] holds a single `header`/`footer` slot (no per-page-type variant in the model); the inlined footnote/endnote **body is flattened to plain text** (per-run bold/italic *inside* the note isn't kept on the inlined run, same as ODT) and end- vs foot-notes aren't distinguished (one inline-note shape in the model); comments, track-changes, embedded OLE; **paragraph borders/shading (`w:pBdr`/`w:pPr/w:shd`)**, **tab stops (`w:tabs`)** and **per-cell borders** (no model slot — table border is single & table-wide); **floating-drawing wrap & z-order** — the wrap mode (`wp:wrapSquare`/`wrapTight`/`wrapTopAndBottom`/`wrapNone`/`wrapThrough`), the z-order flag (`@behindDoc`) and the `@relativeFrom` anchor reference have **no model slot** (`Block.frame` is one absolute `Rect` with no wrap/z-order/anchor-reference field), so the offset is treated as page-absolute; a `wp:align`-only anchor (no `wp:posOffset`) keeps its **size** but defaults its position to `0` (no absolute coordinate at this layer); an **inline** drawing has no size slot on `Inline::Image`, so its `wp:extent` size is not lowered (only the alt text is); **list numbering ordinals are positional** — a level's `w:start` / `w:lvlOverride/w:startOverride` (restart-at-N) and custom `w:lvlText` template (e.g. `%1)`, legal `%1.%2`) are **not** lowered (the model derives ordinals by position and renders ordered markers with a fixed `.` suffix — no start/template slot); a named style's **`w:name`** (human display name) has no model slot — the `StyleId` key carries the machine `w:styleId` |
| **ODT** | Rich | text & runs, char styling, **superscript/subscript** (`style:text-position` first token `super`/`sub`/signed `%` → `CharStyle.vertical_align`), headings, paragraph alignment/indent/spacing/line-height, lists (**ordered vs unordered + per-level number format** resolved from the list's `text:style-name` → `text:list-style`: a `text:list-level-style-number` level is ordered with its `style:num-format` `1`/`a`/`A`/`i`/`I` → `ListMarker` Decimal/LowerAlpha/UpperAlpha/LowerRoman/UpperRoman, a `text:list-level-style-bullet`/`-image` (or an unrecognised/absent style) stays an unordered bullet; resolved **per nesting level**, so a nested level can differ), tables (cells, **column & row cell spans** (`table:number-columns-spanned`/`number-rows-spanned` → `Cell.col_span`/`row_span`, with the trailing `table:covered-table-cell` fillers dropped — same merge model as DOCX/XLSX), **cell shading, cell vertical alignment** (`style:table-cell-properties@style:vertical-align` `top`/`middle`/`bottom` → `Cell.vertical_align`), **header rows** (the rows inside a `table:table-header-rows` wrapper → `Row.is_header`)), images, hyperlinks, **named style table → `Document.styles`** (`styles.xml` `office:styles`: each `style:style style:family="paragraph"` → a `NamedStyle` keyed by `style:name`, with its `style:paragraph-properties` + `style:text-properties` lowered and `style:parent-style-name` kept as `based_on`, *not* flattened; `text`/`table`/`graphic` families skipped) so each paragraph's `style_ref` (set from `text:style-name`) resolves, **document outline / TOC tree** (`Document.outline`) — built from `text:h@text:outline-level` (1..10) nested by level (skipped levels tolerated), plus **bookmarks** (`text:bookmark`/`text:bookmark-start@text:name`) as anchors nested under their section (`_`‑prefixed names dropped); the whole ODT is one model page so every entry targets page 0; **internal hyperlink anchors → their bookmark's page** (a `text:a xlink:href="#name"` is resolved against that bookmark map in a second pass — a match lands on the bookmark's page (page 0 for single-page ODT), incl. a forward reference; an anchor matching no bookmark — e.g. a `#frame` reference — keeps its `#name` placeholder rather than a false page 0); **full document metadata** (`meta.xml`), **running header & footer** (the master page's `style:header`/`style:footer` in `styles.xml` `office:master-styles`, lowered through the **same body walker** so its paragraphs/tables/run-styling/inline-images keep their real formatting, into `Section.header`/`Section.footer`; the first master page's wins) | the ODF **left/first running variants** (`style:header-left`/`style:footer-left`/`style:header-first`/…) are not collapsed in (only the primary `style:header`/`style:footer` is lowered — the model has a single `header`/`footer` slot); list **numbering ordinals are positional** (an ODF `text:start-value` / per-level start and the `style:num-format` *prefix*/*suffix* templates have no model slot — the model derives ordinals by position and renders ordered markers with a fixed `.` suffix); table **borders** (no model slot — table border is single & table-wide); a named style's **`style:display-name`** (human label) has no model slot — the `StyleId` key carries `style:name` |
| **XLSX** | Rich | cell values & types, **formulas** (kept as text), merged cells, multiple sheets, number formats, cell fills, **per-cell character styling** (the cell's `xf@fontId` → the `<fonts>` record → `SheetCell.style` (`CharStyle`): family (`name`), size (`sz`, points), **bold** (`b`), **italic** (`i`), **underline** (`u`), and **colour** (`color`, resolved from `rgb` / `indexed` palette / `theme`+`tint`); gated by `applyFont` — absent/`1` applies the font, an explicit `0` keeps the default style), incl. **superscript/subscript** (font `vertAlign val="superscript"`/`"subscript"` → `CharStyle.vertical_align`), **cell vertical alignment** (`xf/alignment@vertical` `top`/`center`/`bottom` → `SheetCell.vertical_align`; absent ⇒ the OOXML default, bottom), **column widths** (`<cols><col min=… max=… width=…/>` → `Sheet.col_widths`: each `min..=max` range expanded to per-column widths, Excel character units → points `× 7`, default/absent columns left at the default and trailing defaults trimmed so a sheet with no `<cols>` keeps an empty `col_widths`; round-trips with the XLSX export's `<cols>` emission), **document metadata** (`docProps/core.xml`) | — |
| **ODS** | Rich | cell values & types, formulas (kept as text), **merged cells** (`table:number-columns-spanned`/`number-rows-spanned` on a `table:table-cell` → a `MergeRange`; the trailing `table:covered-table-cell` fillers keep their empty grid slot — advancing the column cursor — but emit no phantom cell, the **same merge model as XLSX**), multiple sheets, **number formats** (`@style:data-style-name` → a `number:number-style`/`percentage-style`/`currency-style` → `0.00`/`#,##0`/`0.00%`/`$#,##0.00`, **and** a `number:date-style`/`time-style` rebuilt token-by-token from its `number:day`/`month`/`year`/`hours`/`minutes`/`seconds`/`am-pm` fields + `number:text` separators → `yyyy-mm-dd`/`hh:mm:ss` → `SheetCell.number_format`, at XLSX `numFmt` parity), **cell fills** (`style:table-cell-properties@fo:background-color` → `SheetCell.fill`), **per-cell character styling** (the cell style's `style:text-properties` → `SheetCell.style` (`CharStyle`): font/size/colour/bold/italic/underline/strike), incl. per-cell **superscript/subscript** (`style:text-position` → `CharStyle.vertical_align`), **cell border** (collapsed `fo:border*` → `SheetCell.border`), **cell alignment** (`fo:text-align` → `SheetCell.align`), **cell vertical alignment** (`style:table-cell-properties@style:vertical-align` `top`/`middle`/`bottom` → `SheetCell.vertical_align`), **text wrap** (`fo:wrap-option=wrap`), **column widths & row heights** (`style:table-column-properties@style:column-width` / `style:table-row-properties@style:row-height`), cell style resolved by precedence (cell own → row default → column `default-cell-style-name`), **document metadata** (`meta.xml`) | a `number:date-style`/`time-style` with no recognised field child stays unformatted (the field vocabulary is best-effort: day/month/year/weekday/hours/minutes/seconds/am-pm) |
| **PPTX** | Good | slides, text boxes, shapes (geometry + rotation + groups), runs (bold/italic/colour, **underline** `a:rPr@u` (any non-`none`), **strike** `a:rPr@strike` (`sngStrike`/`dblStrike`), **highlight** `a:highlight` colour → `CharStyle.background`), **superscript/subscript** (`a:rPr@baseline` per-mille: `>0` super, `<0` sub → `CharStyle.vertical_align`), **paragraph formatting** (`a:pPr`: `@algn` `l`/`ctr`/`r`/`just` → align, `@marL`/`@indent` EMU→pt → left/first-line indent, `a:spcBef`/`a:spcAft` `a:spcPts` → space before/after, `a:lnSpc` `a:spcPct`→multiple / `a:spcPts`→fixed → line-height), images, charts→table of cached data, **slide-table cell vertical alignment** (`a:tc/a:tcPr@anchor` `t`/`ctr`/`b` → `Cell.vertical_align`), SmartArt→bullet list, **bullet/number lists** (`a:pPr` `a:buChar` → unordered `List` with that glyph, `a:buAutoNum@type` → ordered `List` (`arabic*`→Decimal, `roman*`/`alpha*` upper/lower); **consecutive** bulleted/numbered paragraphs of matching ordered-ness fold into one `List` (each item keeps its `@lvl` nesting level), an `a:buNone`/un-bulleted paragraph stays a standalone paragraph), **non-text autoshapes & connectors → vector `Shape` blocks** (a `p:sp`/`p:cxnSp` with no text but a fill/stroke/freeform is lowered to a `BlockKind::Shape` instead of being dropped: `a:prstGeom` presets — `rect`/`roundRect`/… → the box rectangle, `ellipse`/`circle`/… → an inscribed 4-cubic Bézier, `line`/`straightConnector1` → the box diagonal, any other preset → its bounding box; `a:custGeom/a:pathLst` freeforms → the parsed path (`moveTo`/`lnTo`/`cubicBezTo`/`quadBezTo`/`close`, the path-guide units mapped into the shape box and Y-flipped to the model's lower-left origin); `a:solidFill`/first `a:gradFill` stop → the shape `fill`, `a:ln` colour/width + `a:custDash` stops → the stroke, the `a:xfrm` offset/extent (EMU→pt) + rotation → the block `frame`/rotation), **document metadata** (`docProps/core.xml`), **speaker notes** (the slide's `…/relationships/notesSlide` rels target — `ppt/notesSlides/notesSlideN.xml` — is parsed: its notes-body placeholder, the `p:sp` whose `p:ph@type="body"`, has its `a:p`/`a:r`/`a:t` text lowered into `Slide.notes`, one paragraph block per `a:p` (`a:br` → newline); the slide-image/slide-number/header/footer placeholders on the notes slide are ignored, and a missing or empty notes part leaves `Slide.notes` absent — the inverse of the export side, so notes round-trip) | run hyperlinks, **animations/transitions**; a paragraph's bullet **inherited** from its placeholder/layout (no explicit `a:buChar`/`a:buAutoNum`) is not resolved (treated as plain); a relative `a:spcPct` space-before/after has no absolute value without the font size, so only `a:spcPts` spacing is lowered; non-rectangular/non-elliptical preset outlines (`triangle`/`star*`/`arrow*`/callouts, bent/curved connectors) are approximated by their **bounding box** (the model has no preset-shape library — fill/stroke/placement are still kept) |
| **ODP** | Good | slides, text boxes, shapes (pos + groups), runs (full char styling), **paragraph formatting** (a slide frame's `text:p` resolves its `fo:*` align/indent/spacing/line-height from its `text:style-name` → the same `style:paragraph-properties` map as the ODT body path), **bullet/number lists** (a frame `text:list` → a model `List`: ordered-vs-unordered + number format resolved from the list's `text:style-name` → `text:list-style` per nesting level, reusing the shared ODF walker), images, **non-text draw shapes → vector `Shape` blocks** (a text-less `draw:rect`/`draw:ellipse`/`draw:circle`/`draw:line`/`draw:connector`/`draw:custom-shape`/`draw:polyline`/`draw:polygon`/… is lowered to a `BlockKind::Shape` instead of being dropped: rectangles → the box, `draw:ellipse`/`circle` → an inscribed Bézier, `draw:line`/`connector` → its explicit `svg:x1/y1/x2/y2` endpoints, `draw:polyline`/`polygon`/`regular-polygon` → the `svg:viewBox`+`draw:points` list mapped into the box; fill/stroke from the shape's `draw:style-name` graphic style — `draw:fill`/`draw:fill-color` → the shape `fill`, `draw:stroke`/`svg:stroke-color`/`svg:stroke-width` → the stroke; the `svg:x/y/width/height` box → the block `frame`, a `draw:transform` rotation → the block rotation), **document metadata** (`meta.xml`), **speaker notes** (a `draw:page`'s `presentation:notes` aside — its `presentation:class="notes"` `draw:frame`/`draw:text-box` — has its `text:p` paragraphs lowered into `Slide.notes`, one block per paragraph, intercepted before the generic frame branch so the notes don't leak in as a positioned shape; a page with no `presentation:notes` leaves `Slide.notes` absent — the inverse of the export side, so notes round-trip) | shape rotation beyond the ODP set, charts/SmartArt, animations; `draw:custom-shape` `draw:enhanced-geometry` path **mini-language** and `draw:path` are approximated by their **bounding box** (fill/stroke/placement kept); shape stroke **dash** patterns (named `draw:stroke-dash` styles) not lowered |
| **ODG** | Good | OpenDocument **Graphics** (mimetype `…opendocument.graphics`); each `draw:page` of shapes is lowered through the **same slide/drawing path as ODP** — one model slide per drawing page, positioned `draw:frame`s → shapes (geometry from `svg:x/y/width/height`), text boxes → placeholders, images, page/master fill → slide background | same drops as ODP (shape rotation beyond the ODP set, charts, animations, layered connector/curve geometry) |
| **DOC / XLS / PPT** (legacy OLE2) | **Text only** | flat plain text (largest stream, UTF‑16/ASCII) | **everything else** — styling, tables, sheets, slides, images, structure. A real binary reader is needed (tracked) |
| **Markdown** | Good | ATX **and setext** headings, bold/italic/**strikethrough `~~`**/code, **inline `[t](url)` + reference `[t][ref]`/collapsed `[t][]`/shortcut `[t]` links** (resolved against `[ref]: url "title"` defs), **footnote refs `[^id]` → the `[^id]: …` body**, **inline images `![alt](url "title")`** (`Inline::Image` keyed by URL hash + alt, mirroring the HTML importer — local/`data:`/external all keep the reference), ordered/unordered nested lists, **GFM task-lists `- [ ]`/`- [x]`** (leading `☐`/`☑` glyph), GFM tables, fenced code, blockquotes, HR, **inline HTML phrasing tags** (`<b>`/`<strong>`, `<i>`/`<em>`, `<code>`, `<u>`, `<s>`/`<del>`, `<a href>`, `<br>`) **+ character references** (`&amp;`, `&#233;`…) | task-list state is a glyph (no boolean checkbox slot on `ListItem`); footnotes resolve inline (no separate footnote section/backref); inline HTML limited to the common phrasing tags (unknown tags drop, text kept); image bytes are not fetched/interned (URL reference only, as in HTML) |
| **CSV** | Full | quoting/escaping (RFC 4180), embedded delimiters/newlines, BOM, delimiter auto-detect, ragged rows padded; **lowered to a typed `SheetBlock`** (the same typed-cell model as XLSX/ODS, not a text-only layout table) with **per-cell type inference**: a body cell becomes `CellValue::Number` (optionally signed, fraction, decimal exponent `1e5`), `CellValue::Bool` (`true`/`false`, any case incl. `TRUE`/`FALSE`), or a date/datetime (ISO `YYYY-MM-DD`[`THH:MM[:SS]`], space or `T` separator → an Excel-style serial number + a `yyyy-mm-dd`[` hh:mm:ss`] `number_format`); the header row stays verbatim `CellValue::Text`. **Conservative guards** keep ambiguous tokens as text — leading-zero numerics (`01234` ZIP), digit runs > 15 (card/account IDs that would lose `f64` precision), sign/separator-laden tokens (`+1-555-0100` phone), and out-of-range or non-ISO dates (`06/25/2026`, `2026-13-01`, non-leap `2023-02-29`) are never coerced | multi-sheet (CSV has none); percentage/currency (no `%`/`$` parsing — `CellValue` has no such variant and the XLSX path carries these only via number-format metadata absent from raw CSV) |
| **RTF → PDF** | Rich | full char/para formatting, fonts, colours, tables, pictures — **PNG/JPEG** kept verbatim and **WMF/EMF (`\wmetafile`/`\emfblip`) + DIB/BMP (`\dibitmap`/`\wbitmap`)** decoded by the in-house metafile/DIB decoders and **re-encoded to PNG**; both hex and **`\bin<N>` binary** payloads; `\field` hyperlinks (`HYPERLINK "url"` → `<a href>`) | nested tables |
| **RTF → model** | Rich | run-level char styling (bold/italic/underline/strike, `\cf` colour, `\fs` size, `\f` font family + serif/sans/mono generic, super/sub), paragraph alignment/indents, tables (`\trowd`/`\cell`/`\row` → `BlockKind::Table`), `\pict` images (**bytes interned** into `Document.resources.images`) — **PNG/JPEG** verbatim, **WMF/EMF (`\wmetafile`/`\emfblip`)** rasterized by `convert::metafile` and **DIB/BMP (`\dibitmap`/`\wbitmap`)** decoded in-house, both **re-encoded to PNG**; both hex and **`\bin<N>` binary** payloads, `\field` hyperlinks (`HYPERLINK "url"` → `Inline::Link`) — routed through the **same rich parser** as RTF→PDF (no text-only fallback) | nested tables, list ordering/nesting (lowered as plain paragraphs) |
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
| **DOCX** | Richest | paragraphs & runs, bold/italic/underline/strike, font/size/colour/highlight, super/sub, headings→styles, **named style table → `word/styles.xml` (one `w:style w:type="paragraph"` per `NamedStyle`: `w:name`, `w:pPr`/`w:rPr` from the style, `w:basedOn` from `based_on`) + paragraph `w:pStyle` references (`Paragraph.style_ref`)**, alignment/indent/spacing/line-height, lists (nesting), tables (spans, borders, widths, shading, **cell vertical alignment** `Cell.vertical_align` → `w:tcPr/w:vAlign`, **header rows** `Row.is_header` → `w:trPr/w:tblHeader`), images **in their real format** (`ImageResource.format` → the media part extension, `[Content_Types]` `Default` and `r:embed` relationship target all follow the blob — `jpeg`/`png`/`gif`/`bmp`/`tiff`/`webp`, unknown ⇒ `png`), inline images, external links, **internal page links** (`LinkTarget::Page` → a `HYPERLINK \l "pageN"` field whose target is a `w:bookmarkStart`/`End` named `pageN` dropped at each page boundary, 1-based); **multi-section page setup** — each `Section`'s geometry + running header/footer becomes its own `w:sectPr`: a non-final section is terminated by a section-break paragraph carrying its `w:pPr/w:sectPr`, the final section's `w:sectPr` is the direct `w:body` child, and each owning section gets its own `word/headerN.xml`/`footerN.xml` part (+ `[Content_Types]` overrides and `rIdHdrN`/`rIdFtrN` relationships) alongside its own `w:pgSz`/`w:pgMar`; **explicit run colour** (`CharStyle.color = Some(_)` → `w:color`, including a deliberately-black `Some([0,0,0])`; only an unset `None` omits the tag so the default black is inherited) | a `NamedStyle` reusing a built-in id (`Normal`/`Heading1‑6`) is not re-emitted (the built-in keeps its defaults); per-page-type (first/even) header-footer variants and non-page-break "continuous" sections (the model holds one header/footer slot + no section-type flag) |
| **ODT** | Rich | as DOCX for text/lists/links and **images in their real format** (`ImageResource.format` → the `Pictures/imgN.<ext>` name, `manifest.xml` media-type and `draw:image xlink:href` all follow the blob — `jpeg`/`png`/`gif`/`bmp`/`tiff`/`webp`, unknown ⇒ `png`); **run-level inline images** (an `Inline::Image` *inside* a paragraph's runs → a `draw:frame`/`draw:image` anchored `text:anchor-type="as-char"`, interned into `Pictures/` + the manifest like the block-level path); **list nesting** (`ListItem.level` → nested `<text:list>`s: a level-2 item lives in a `text:list` *inside* the level-1 item's `text:list-item`, ISO 26300 §5.3.3, driven by the named list style's per-level definitions); table **cell borders & row height** (the table border → each cell's `style:table-cell-properties@fo:border`; `Row.height` → a `table:table-row` style's `style:row-height`/`min-row-height`), **cell shading & cell vertical alignment** (`Cell.vertical_align` → `style:table-cell-properties@style:vertical-align`), **header rows** (the leading `Row.is_header` rows are wrapped in a `table:table-header-rows` element, the body rows following it); **named style table → `office:styles` in `styles.xml` (one `style:style style:family="paragraph"` per `NamedStyle`: `style:name`/`style:display-name`, `style:parent-style-name` from `based_on`, para/text properties) + paragraph `text:style-name` references (`Paragraph.style_ref`; a paragraph with direct overrides gets an automatic style inheriting the named one via `style:parent-style-name`)**; **super/subscript** (`CharStyle.vertical_align` → `style:text-position="super 58%"`/`"sub 58%"` on the run's text style, on both automatic and named character styles); **block-level shapes** (a `BlockKind::Shape` with a placement `frame` → a `draw:rect` (or `draw:path` for a non-rectangular outline) carrying the frame geometry + a graphic style with `draw:fill`/`draw:stroke`, wrapped in a `text:p` as ODF requires for a body drawing — same geometry/paint mapping as the ODP/PPTX shape export; a frame-less shape has no flow slot and is omitted); **internal page links** (`LinkTarget::Page` → `text:a xlink:href="#pageN"`, resolving to a `text:bookmark` named `pageN` dropped at each page boundary, 1-based); **multi-section page setup** — ODF has no `w:sectPr`, so a page change is driven by `style:master-page-name`: one `style:page-layout` + `style:master-page` (carrying that section's `style:header`/`style:footer`) is minted per distinct section geometry/header/footer (the first stays the canonical `Standard`/`pm1`), and a section that begins a *different* master gets an automatic `style:master-page-name`-bearing paragraph style on its first paragraph that forces the page change; sections sharing a geometry/header/footer collapse onto one master (no spurious break); **explicit run colour** (`CharStyle.color = Some(_)` → `fo:color`, deliberate black included; only `None` omits it) | ODF's master-page mechanism expresses only the page break + new geometry/header/footer — there is no per-section column count, line-numbering restart, or "continuous" (no-page-break) section type, so those OOXML section features cannot map |
| **PPTX / ODP** | Good | slides, text boxes, shapes, images, runs (bold/italic/colour/**highlight** — `a:highlight` for PPTX, `fo:background-color` for ODP), alignment; **placeholder semantic roles round-trip** — a placeholder’s `PlaceholderRole` becomes `<p:ph type="title\|subTitle\|body">` (PPTX) or `presentation:class="title\|subtitle\|outline"` + `presentation:placeholder="true"` (ODP, ISO 26300 §9.6.1); unmapped roles keep their original ODF class token, and free (non-placeholder) shapes carry none; **slide tables round-trip as real tables** — a `Table` block becomes a DrawingML `p:graphicFrame`/`a:tbl` (PPTX) or a `draw:frame`/`table:table` (ODP) with the right rows/cols/cells, column widths, cell spans (`gridSpan`/`rowSpan` · `number-columns/rows-spanned`), cell shading and **cell vertical alignment** (`Cell.vertical_align` → `a:tc/a:tcPr@anchor` `t`/`ctr`/`b` for PPTX · `style:table-cell-properties@style:vertical-align` for ODP), not a paragraph flatten; PPTX emits a complete OPC layout chain — every slide references a `slideLayout` → `slideMaster` → `theme` (opens without a PowerPoint *repair* prompt); **speaker notes round-trip** — `Slide.notes` becomes a `ppt/notesSlides/notesSlideN.xml` (`p:notes`, body placeholder) with its `[Content_Types]` override and a bidirectional slide↔notesSlide relationship (PPTX), or a `presentation:notes` aside (`presentation:class="notes"`) inside the `draw:page` (ODP); **images in their real format** — `ImageResource.format` drives the media part extension + `[Content_Types]`/`r:embed` (PPTX) and the `Pictures/` name + `manifest.xml` media-type + `draw:image xlink:href` (ODP) (`jpeg`/`png`/`gif`/`bmp`/`tiff`/`webp`, unknown ⇒ `png`); **PPTX paragraph spacing/indent/line-height** — `a:pPr@marL`/`@marR`/`@indent` (EMU) for the indents + `a:spcBef`/`a:spcAft` (`a:spcPts`, points×100) and `a:lnSpc` (`a:spcPct` 1/1000th-% for a multiple, `a:spcPts` for a fixed leading) for the spacing/leading, alongside `algn` (ODP keeps these via its shared `fo:*` paragraph properties); **run-level inline images** — an `Inline::Image` inside a run becomes a real slide picture: a standalone `p:pic` hoisted onto the slide's shape tree referencing the same media + relationship (PPTX — DrawingML text runs cannot embed a picture), or an as-char `draw:image` in the paragraph (ODP); **external run hyperlinks** — an `Inline::Link { Url }` emits `a:rPr/a:hlinkClick r:id` + a slide-rels `hyperlink` relationship (`TargetMode="External"`) on the run (PPTX) or wraps the run text in `text:a xlink:href` (ODP); **internal page links** — an `Inline::Link { Page }` to an in-range slide emits `a:hlinkClick r:id` with `action="ppaction://hlinksldjump"` + a slide-rels `slide` relationship targeting `slideN.xml` (PPTX), or `text:a xlink:href="#pageN"` resolving to the target `draw:page draw:name="pageN"` (ODP); **super/subscript** — `CharStyle.vertical_align` → `a:rPr@baseline` `30000`/`-25000` (PPTX) or `style:text-position` (ODP, shared with the ODT export); **non-slide content keeps its structure on a slide** — lists/tables/headings reaching a slide via the page-fallback (a `Document` with no `Slide` blocks → one slide per page) are emitted as **real** structure, not a `collect_paras` plain-paragraph flatten: a `List` → bulleted paragraphs (`a:buChar` for an unordered glyph / `a:buAutoNum@type` for an ordered list, each item carrying its `lvl` + a per-level hanging indent · ODP `text:list`), a `Heading` → a styled (bold, level-sized) paragraph, and a `Table` → a real `p:graphicFrame`/`a:tbl` (PPTX) or `draw:frame`/`table:table` (ODP) hoisted as a sibling shape (a DrawingML text body / `draw:text-box` cannot contain a table); **explicit run colour** (`CharStyle.color = Some(_)` → `a:solidFill`/`a:srgbClr` (PPTX) / `fo:color` (ODP), deliberate black included; only `None` omits it) | a hoisted body table is stacked at a default box (the flowing source carries no slide layout); genuinely unrepresentable body blocks (images/shapes/sheets/code/rules in a placeholder) still degrade to plain text |
| **XLSX / ODS** | Good | cell values & types, **cell formulas** (`SheetCell.formula` → XLSX `<f>…</f>` alongside the cached `<v>`, `t="str"` for a text result · ODS `table:formula="of:=…"` keeping the cached `office:value`; leading `=` stripped, round-trips), number formats, merged ranges, column widths, multiple sheets, bold/italic, **cell underline & strike** (`CharStyle.underline`/`strike` on the per-cell font → XLSX `<u/>`/`<strike/>` in the `<font>` record · ODS `style:text-underline-style="solid"`/`style:text-line-through-style="solid"` on the cell's text style), **cell vertical alignment** (`SheetCell.vertical_align` → XLSX `xf/alignment@vertical` `top`/`center`/`bottom` · ODS `style:table-cell-properties@style:vertical-align` `top`/`middle`/`bottom`) | in-cell images |
| **HTML** | Full (semantic) | clean `<h1-6>/<p>/<ul>/<ol>/<table>` with colspan/rowspan + shading (a table's leading `Row.is_header` rows render as a `<thead>` of `<th>` cells, the body rows as a `<tbody>` of `<td>` cells — a header-less table is a single `<tbody>`), styled `<span>` runs, `<a>`, `<img>` data-URI, `<pre><code>`, `<blockquote>`, sheets/slides; **vector `Shape` → self-contained inline `<svg>`** — geometry preserved (`viewBox` + `width`/`height` in pt from the path bounds, segments as `<path d>` with Y flipped to top-left origin), `fill` (`none` when unfilled), `stroke`/`stroke-width`/`stroke-dasharray` from the paint (a point-less/empty shape still falls back to a 1em box) | — |
| **Markdown** | Full (GFM) | headings, bold/italic/strike/code/underline/super-sub, links, images, nested+ordered lists, GFM tables, blockquotes, HR, code fences, YAML front-matter; **run colour** — an *explicitly-set* run colour (`CharStyle.color = Some(_)`, deliberate black included; an unset `None` stays plain), which CommonMark can't express, is carried by an outer `<span style="color:#RRGGBB">…</span>` wrapping the still-Markdown emphasised body (GFM inline HTML); **vector `Shape` → self-contained inline `<svg>`** (same geometry→SVG mapping as the HTML/EPUB export — `viewBox` + pt `width`/`height` from the path bounds, segments as `<path d>` with Y flipped, `fill`/`stroke`/`stroke-width`/`stroke-dasharray` from the paint; a point-less shape still falls back to a 1em box). Everything representable stays plain ASCII Markdown — only colour + shapes fall to inline HTML/SVG, so the output is **GFM, not CommonMark-strict** | inline-HTML use is GFM-only (colour span + shape `<svg>` won't render in a CommonMark-strict reader, though the text/geometry is preserved) |
| **EPUB** | Full (EPUB 3) | valid OCF, per-block XHTML (same fidelity as HTML export — incl. table **header rows** as `<thead>`/`<th>` vs `<tbody>`/`<td>`), embedded images, metadata; **vector `Shape` → self-contained inline `<svg>`** (same geometry→SVG mapping as the HTML export — `viewBox` + pt `width`/`height` from the path bounds, segments as `<path d>` with Y flipped, `fill`/`stroke`/`stroke-width`/`stroke-dasharray` from the paint; a point-less shape still falls back to a 1em box); **nested TOC** — the nav `<ol>` and NCX `navPoint`s nest the in-document heading hierarchy (H1→H2→H3…) under each chapter, with stable per-heading anchor ids (`text-N.xhtml#secN-hK`) emitted on the headings so the links resolve, and `dtb:depth` reflecting the deepest level; **unique, deterministic identifier** — `dc:identifier`/`unique-identifier` and the NCX `dtb:uid` agree on a `urn:gigapdf:<hex>` content hash (FNV-1a over the document's text + structure; no clock/RNG), so two different documents never collide while the same document is stable | inline-only CSS |
| **RTF** | Rich | char styling (bold/italic/underline/strike/size/colour/highlight), paragraph alignment, blockquote indent, HR; **tables → real `\trowd … \cellxN … \cell … \row` grids** (cell right-edges from `Table.col_widths`, pt→twips); **lists → ordered/unordered markers with nesting** (`\pard\liN\fi-360` per depth, `\bullet` for unordered, a running number `1.`/`a.`/`i.` for ordered — honours `List.ordered`/`List.marker`, cycling decimal→lower-alpha→lower-roman by depth when the marker isn't pinned, restarting per-level counters); **images → `{\pict …}`** (`\pngblip`/`\jpegblip` detected from the interned `Document.resources` bytes, `\picwgoal`/`\pichgoal` from the pixel size at 96 dpi, hex payload); **hyperlinks → `{\field{\*\fldinst HYPERLINK "url"}{\fldrslt …}}`** (the form the RTF importer reads back, round-tripping `Inline::Link`). **`Document::to_rtf` (PDF → RTF) is now model-aware** — it gates on the same `/StructTreeRoot` signal as `Document::to_text`: a tagged / Office-imported / authored document is exported through this rich `rtf_from_model` path (styled runs, `\trowd` tables, list markers, `{\pict}` images, `{\field}` links), *not* the former text-only wrapper. A **pure-PDF** document with no structure tree keeps the plain fallback (its text lines wrapped one `\par` each, no styling) | vector `Shape` geometry; image formats RTF can't carry (GIF/WebP/AVIF → skipped); an internal page link is emitted as a `#pageN` field target (no native RTF page jump); a pure-PDF `Document::to_rtf` stays text-only (no model to walk) |
| **Plain text** | Good (model-tree) | **structure-preserving when a real model exists** — a document carrying a `/StructTreeRoot` (an authored Tagged PDF, or one produced by the Office importers / `to_tagged_pdf`) is rendered from the reconstructed model tree in reading order: headings/paragraphs become clean lines (blank line between top-level blocks); **lists keep their marker** (`- ` unordered · `1.`/`a.`/`i.` ordered per `List.marker`, re-numbered per nesting level) with **per-depth indentation** (2 spaces/level); **tables render as aligned columns** (each column padded to its widest cell, cells joined by a ` \| ` gutter, one row per line, header row included — *aligned* chosen over TSV for plain-text legibility); blockquotes are `> `-prefixed, code blocks verbatim, images/shapes a short `[image]`/`[shape]` placeholder. A **pure-PDF** document with no structure tree keeps the original **PDF-layer** path: one text run per line, form-feed (`\x0C`) between pages | table `col_span`/`row_span` not expanded into repeated cells (a spanned cell holds its single slot); per-run styling inside a cell/list-lead is flattened to text; the untagged PDF-layer fallback is still flat (no alignment / list markers — it has no model to walk) |
| **CSV** | Full | RFC 4180 quoting, CRLF; **standard-friendly multi-sheet separation** — a single sheet/table emits pure RFC 4180 (no preamble or separator); several sheets/tables are each introduced by a *plain* (RFC-4180-quotable) **name row** (`Sheet N`/`Table N` when unnamed) — **not** a `#`-comment row, which a strict parser would mis-read as data — with one **blank line** between consecutive blocks, so a strict split on the blank line yields one well-formed RFC-4180 block per sheet/table | percentage/currency cell formatting (raw values only); the blank-line/name-row multi-sheet convention is itself outside the single-table RFC 4180 grammar (each block, taken alone, is conformant) |
| **PDF/A** | Conformant (1b/2b/3b · 1a/2a tagged) | Full identification (XMP `pdfaid` packet, sRGB OutputIntent + embedded ICC, deterministic `/ID`, level header); **every font embedded** — a face the source only references by name (base-14 / non-embedded) is **substituted with a bundled metric-compatible standard face** (Liberation Sans) embedded as `/FontFile2` + complete `/FontDescriptor` (serif/sans/mono · bold · italic flags matched), preserving the original `/Widths`/`/W` (a base-14 font that declared none gets widths measured from the substitute) so layout is stable, with a `/ToUnicode` kept/synthesised so text stays searchable; **forbidden constructs stripped** — encryption removed (output is plaintext), document-level JavaScript dropped (`/Names /JavaScript`, a JS `/OpenAction`, the catalog `/AA`), and the graphics-state / appearance normalisations (`ExtGState /TR`, incomplete `/CIDSet`, `/AP` alternates → `/N`); **metadata from the document's own `/Info`** (`dc:title`/`dc:creator`/`dc:description`, `pdf:Producer`/`pdf:Keywords`, `xmp:CreatorTool`/`CreateDate`/`ModifyDate`) written to both `/Info` and XMP (they agree), defaulting `Producer`/`Creator` only when absent — validated PASS / 0 failed rules by veraPDF 1.30 (1b + 2b) | a non-embedded **composite** font's substitute uses the bundled glyphs (CID→GID via `/Identity`, widths from `/W` so layout holds) rather than the original face's shapes; a 2u claim still needs source `/ToUnicode` on every glyph (use 2b otherwise); a go-to `/OpenAction` (a destination, not JavaScript) is permitted and kept |

> **Table header rows** — the model carries a per-**row** `Row.is_header` flag,
> wired end-to-end so a table's column-title row survives every hop: **detected**
> in PDF reconstruction (tagged `/THead`/all-`/TH`, or a leading entirely-bold row),
> **imported** from DOCX `w:trPr/w:tblHeader`, HTML `<thead>`/all-`<th>`, ODF
> `table:table-header-rows` and GFM's first row, and **exported** back to all of
> them — DOCX `w:tblHeader`, ODF `table:table-header-rows`, HTML/EPUB `<thead>` +
> `<th>` (vs `<tbody>` + `<td>`), with Markdown's first-row-as-header unchanged.
> The flag round-trips through the JSON model. **Deferred:** per-**cell** column
> headers — a `<th scope>` body cell or a first-column row-header (`<th>` inside the
> body) — are *not* modelled yet; that needs a separate per-cell `Cell.is_header`
> and a wider ripple. This covers the dominant **header-row** case only.

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
projected onto their reading axes; **header-row detection** — a tagged table marks
its `/THead`-grouped rows (or an all-`/TH` row) as `Row.is_header`, and an untagged
reconstructed table marks its **leading** row when it is *entirely bold while the
body is not* — conservative, so a uniform data table flags no header) · images — both `Do` XObjects **and inline images**
(`BI`/`ID`/`EI`, ISO 32000-1 §8.9.7) — with lifted figure captions · vector
shapes · underline/strike (from drawn rules) · external + internal hyperlinks ·
outline/bookmarks · page geometry — including **page-level `/Rotate` (90/180/270)**:
the reconstructed page reflects the **displayed** orientation (its width/height swap
for 90°/270°) and every reconstructed block frame is projected into that upright
reading frame, so a landscape-rotated page reads correctly rather than sideways —
this now covers **both** reconstruction paths: heuristic block frames (top-down) and
**tagged-PDF block frames** (a `/Layout /BBox` hint in raw PDF user space is flipped
top-down before the same cardinal projection), so a document that is *both* tagged
*and* `/Rotate`d no longer leaves its tagged blocks at unrotated positions
(`/Rotate 0` pages — tagged or not — stay byte-identical) · **running headers/footers** (stripped from
the body flow and lifted to `Section.header`/`Section.footer`, see below) · tagged-PDF
`/StructTreeRoot` (consumed; blocks distributed onto their real pages, see below) ·
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
`/LZW`, `/Fl` (Flate), `/RL` (RunLength), `/DCT` (baseline JPEG), `/CCF`
(CCITTFax, see below), `/JPXDecode` (JPEG 2000, see below) — and colour spaces
`/G`/`/RGB`/`/CMYK`/`/I` (plus Indexed). `/IM true` image masks paint the current
fill colour through the stencil.

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

**JPEG 2000 images (`/JPXDecode`, ISO 32000-1 §7.4.9 / ISO/IEC 15444-1):** now
**decoded from scratch** (pure `std`, zero deps) — a complete J2K codestream
decoder feeding the normal image-sample path. The decoder accepts both the raw
codestream (`FF 4F` SOC …) and the **JP2 box** wrapper (`jP  `/`ftyp`/`jp2h`/
`jp2c`), locating the contiguous codestream in either. It implements the full
pipeline:

  - **markers** — `SIZ` (image/tile geometry, per-component bit depth, signedness
    and sub-sampling), `COD`/`COC` (progression order, layers, decomposition
    levels, code-block size, code-block style, 5/3-vs-9/7 transform, precincts),
    `QCD`/`QCC` (no-quantisation, scalar-derived and scalar-expounded), `RGN`
    (region-of-interest max-shift), `SOT`/`SOD`/`EOC` tile-parts, and `POC`/`TLM`/
    `PLM`/`PLT`/`PPM`/`PPT`/`CRG`/`COM`/`SOP`/`EPH` handled or safely skipped;
  - **tier-2 packet decoding** — the subband/precinct/code-block geometry, the
    inclusion and zero-bit-plane **tag-trees**, per-code-block new-pass counts and
    `Lblock` length signalling, layers, and **all five progression orders**
    (LRCP/RLCP/RPCL/PCRL/CPRL), with the bit-stuffed packet headers;
  - **tier-1 EBCOT** — per-code-block bit-plane decoding (significance-propagation,
    magnitude-refinement and cleanup passes with the run-length mode) driven by the
    **same MQ arithmetic decoder** the JBIG2 path uses (ISO/IEC 15444-1 reuses the
    ITU-T T.88 MQ coder), with the 9 zero-coding contexts, the sign-coding and
    magnitude-refinement contexts and the standard context formation;
  - **reconstruction** — scalar dequantisation (reversible and irreversible), the
    **inverse DWT** (5/3 reversible integer lifting and 9/7 irreversible lifting)
    per resolution level, the **inverse multi-component transform** (RCT for 5/3,
    ICT for 9/7), DC level-shift and clamp to the component bit depth.

  The decoded raster is emitted as packed, interleaved samples at the codestream's
  native bit depth, so the image's `/ColorSpace` (or the JP2 `colr` box) drives the
  final colour mapping through the same path as any other image. Multi-tile,
  multi-resolution and multi-component (e.g. RGB-via-RCT) images are supported; ROI
  and unsupported optional features are handled defensively rather than aborting.

  This decoder is wired into the **render and image-extraction pipeline**, not just
  the filter layer: a `/JPXDecode` image XObject or inline image flows through the
  same image-decode path as `/DCTDecode`/`/CCITTFaxDecode`/`/JBIG2Decode` images
  (colour space, `/BitsPerComponent`, `/Decode`, colour-key `/Mask` and `/SMask`
  handling all apply), and a `/JPXDecode` **soft mask** (`/SMask`) — or a JPX-coded
  `/ImageMask` stencil / explicit `/Mask` — decodes through that path too. So JPEG
  2000 images are actually decoded when a page is rasterized or its images are
  extracted, not merely decodable in isolation.

**In-house WMF/EMF metafile decoder (GDI records → RGBA):** Office documents and
RTF embed **vector** clip-art, logos and diagrams as Windows Metafiles (`.wmf`)
and Enhanced Metafiles (`.emf`) — a list of GDI drawing commands, not a pixel
grid — so no image codec can decode them. `convert::metafile` adds a from-scratch
**GDI rasterizer** (zero new deps) that interprets those records onto an in-house
transparent RGBA canvas and returns a tight `MetafileRaster { width, height, rgba }`
at the metafile's natural pixel size (capped to ~4096², transparent where nothing
is painted). It covers both the **WMF** placeable header (`0x9AC6CDD7`, bbox +
units) and the bare `METAHEADER` (16-bit records), and the **EMF** `ENHMETAHEADER`
(`rclBounds`/`rclFrame`, 32-bit records): the GDI **object table** (pens —
`CreatePenIndirect`/`ExtCreatePen` style·width·colour·dash; brushes —
`CreateBrushIndirect`/`CreateSolidBrush`/pattern→solid; fonts —
`CreateFontIndirect[W]`) with `SelectObject`/`DeleteObject` + EMF stock objects;
the drawing records `MoveTo`/`LineTo`, `Polyline`/`Polygon`/`PolyPolygon` (+ EMF
`*16`), `Rectangle`/`RoundRect`/`Ellipse`, `Arc`/`Pie`/`Chord`, `PolyBezier`
(EMF), `SetPixel[V]`, `FillRgn`/`PaintRgn` (region→bbox), and the DIB blits
`BitBlt`/`StretchBlt`/`StretchDIBits`/`SetDIBitsToDevice` (with a from-scratch
DIB/BMP decoder for 1/4/8/24/32-bpp + RLE4/RLE8); the logical→device transform
`SetWindowOrg`/`Ext` + `SetViewportOrg`/`Ext` + `SetMapMode` and the EMF world
transform `SetWorldTransform`/`ModifyWorldTransform`, honouring the current
pen/brush, the fill rule (`SetPolyFillMode`) and the paint state
(`SetBkMode`/`SetBkColor`/`SetTextColor`/`SetROP2`). `ExtTextOut`/`TextOut`
render as a reasonable advance/box strip (text is secondary; no font shaping
here), and genuinely-rare records (palette management, escapes, EMF GDI+ comment
blocks, ROP2 ops other than copy) are skipped safely. Public API:
`decode_wmf`/`decode_emf`/`decode_metafile` (sniffing). **Wired into both the RTF
importer** ([#4](../../issues/4)) — a `{\pict\wmetafile…}`/`{\pict\emfblip…}` is
rasterized through `decode_wmf`/`decode_emf` and a `{\pict\dibitmap…}`/`{\pict\wbitmap…}`
through an in-house packed-DIB decoder, each **re-encoded to PNG** via
`raster::encode_png` before interning (both the hex and the `\bin<N>` binary
payload forms) — **and the Office importers** ([#3](../../issues/3)): every
embedded media part in DOCX/PPTX/XLSX/ODT/ODP/ODS goes through one interning path
that sniffs the blob's magic bytes — a WMF/EMF metafile is decoded here and
re-encoded to PNG (the model/render/export carry no metafile rasterizer), while
GIF/BMP/TIFF/SVG are kept verbatim with their true format tag instead of being
dropped or mis-tagged.

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

**Heading levels (clustered, document-wide, stable):** a paragraph that is short
and visually prominent (font above the body size, or bold-and-short) is promoted
to a heading, and its **level** (`h1`..`h6`) comes from **clustering the distinct
heading-candidate font sizes actually present** — not fixed global size ratios.
The distinct sizes are sorted descending and grouped within a small relative
tolerance (sizes within ~6 % collapse to one level); the largest cluster maps to
`h1`, the next to `h2`, and so on, **monotonically with no skipped levels** (a
24/18/14-over-11 pt document yields `h1/h2/h3`, never `h1/h3/h4`). The clustering
is now **document-wide**: every promoted heading's size across **all pages** is
collected and clustered **once**, then each heading's level is stamped from that
single map — so the same visual size resolves to the **same level on every page**
(a 14 pt heading is `h2` on page 1 and on page 9, never `h1` on a page that
happens to lack a larger title), and the document gets a **coherent nested
outline** rather than a per-page-relative one. The *detection* gate stays
per-page (a heading must be meaningfully larger/bolder than **its own page's**
dominant body size), so only the level assignment is harmonized; a **one-page**
document collects exactly the sizes its single page already clustered, so its
levels are **unchanged**. A heading only ~1.15× the body is detected as a heading
(not missed, not forced to `h6`); a bold run-in subhead at body size lands on the
**deepest present** level rather than always `h6`; a document with a single
heading size yields one consistent level for all. Body prose never enters the
hierarchy, so ordinary text is never promoted.

**Limits on arbitrary third-party PDFs (tracked in [#5](../../issues/5)):**

- **Tagged-PDF path (`/StructTreeRoot`)**: when an authored Tagged PDF is present
  the structure tree is trusted over the geometric heuristics, and the walk now
  **lowers the structure-element attributes** (`/A`, ISO 32000-1 §14.8.5.2)
  instead of dropping them. **Table cell spans** — a `/TD`/`/TH`'s `/ColSpan` /
  `/RowSpan` (`/Table` owner) become the model `Cell.col_span` / `row_span`, and
  the logical column count is the widest row's summed span, so tagged tables come
  out as aligned grids (not the old ragged 0-width ones). **List numbering** — an
  `/L`'s `/ListNumbering` (`/List` owner) drives `List.ordered` and the marker
  format (`Decimal`/`UpperRoman`/`LowerRoman`/`UpperAlpha`/`LowerAlpha` → ordered
  with the matching marker; `Disc`/`Circle`/`Square` → bullets; `None`/absent →
  unordered default), so tagged lists are no longer always unordered. **Geometry**
  — each block's `frame` is set from the element's `/Layout` `/BBox`
  (`[llx lly urx ury]` → a normalized lower-left + width/height rect) when
  present. **Page assignment** — the reconstruction driver now **distributes the
  tagged blocks onto their real model pages**, each placed on the 0-based page
  index resolved from its producing element's `/Pg` (via the paged tag-tree entry
  point): a block declared on page 2 lands on model page 2, not page 1. A block
  whose page can't be resolved (`None` — grouping elements often omit `/Pg`) keeps
  the conservative first-page placement; a page the structure never references
  stays a valid (empty) page, and the heuristic path is not mixed in (so the same
  prose is never double-emitted). Still pending: a **header-row concept** —
  `/THead`/`/TH` is read as a cell but not flagged as a header (`Cell` has no
  `is_header` field yet; a separate task, shared with the heuristic tables item
  below).
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
