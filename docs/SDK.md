# SDK Reference — `@qrcommunication/gigapdf-lib`

Complete reference for the TypeScript SDK. Exact signatures and defaults ship in
the bundled `.d.ts` (your IDE surfaces them inline); this document explains what
every method *does*, its parameters, return value, and the gotchas.

> Looking for ready-made, copy-pasteable snippets? See the
> **[Cookbook](COOKBOOK.md)** — redaction, styled text, headers/footers,
> conversions, OCR, forms, annotations, signing, encryption and the editable
> model, each as a short worked recipe.

Two classes:

- **`GigaPdfEngine`** — the loaded WebAssembly module. Stateless factory:
  create/parse documents and run font/HTML/JS helpers. Load it once and share it.
- **`GigaPdfDoc`** — one open document. All editing/reading/export lives here.
  You **own** it: call [`close()`](#close) exactly once when done.

```ts
import { GigaPdfEngine } from "@qrcommunication/gigapdf-lib";

const giga = await GigaPdfEngine.loadDefault();      // reads the bundled .wasm
const doc = giga.open(pdfBytes);                     // → GigaPdfDoc
try {
  doc.addStandardText(1, 72, 720, 24, "Hello", "Helvetica-Bold");
  const out = doc.saveCompressed();
} finally {
  doc.close();                                       // free the wasm handle
}
```

### Conventions

| Topic | Rule |
|-------|------|
| **Pages** | 1-based (`page = 1` is the first page). |
| **Coordinates** | PDF user space, points (1/72"), origin **bottom-left**, Y up. |
| **Colours** | packed `0xRRGGBB` integers (e.g. `0xff0000` red). |
| **Bytes** | `Uint8Array` in and out. PDFs, fonts, images, exports are all bytes. |
| **Booleans** | edit methods return `true` on success, `false` on a bad page/arg. |
| **Memory** | every `Uint8Array` you pass is copied into/out of wasm and freed for you. |
| **Errors** | the engine never throws across the boundary; failures are `false`/`null`/`[]`. The signing methods are the exception — `signP12` / `signTimestamped` / `signLtv` throw a generic `Error` on failure, and `saveEncrypted` throws if AES-256 is requested without Web Crypto or a `keySeed`. |

---

## `GigaPdfEngine`

### Loading

| Method | Returns | Description |
|--------|---------|-------------|
| `GigaPdfEngine.loadDefault()` | `Promise<GigaPdfEngine>` | Instantiate from the `.wasm` bundled in the package. The usual entry point. |
| `GigaPdfEngine.load(wasm)` | `Promise<GigaPdfEngine>` | Instantiate from caller-supplied `.wasm` bytes (custom hosting/CDN). |
| `.raw` | `Exports` | The raw `extern "C"` exports, for ABIs not yet wrapped. Escape hatch. |

### Creating / opening documents

| Method | Returns | Description |
|--------|---------|-------------|
| `open(pdf)` | `GigaPdfDoc` | Parse a PDF (handles xref tables **and** xref streams / object streams). |
| `openEncrypted(pdf, password)` | `GigaPdfDoc \| null` | Open a password-protected PDF; `null` if the password is wrong. |
| `encryptionInfo(pdf)` | `{ encrypted, permissions, version, revision }` | Inspect a PDF's `/Encrypt` dictionary without opening it. |
| `txtToPdf(text)` | `Uint8Array` | Render plain text to a fresh single-/multi-page PDF (standard Helvetica, word-wrapped). |
| `htmlToPdf(html)` / `htmlRender(html, fonts, w, h, margin)` | `Uint8Array` | Render HTML+CSS to PDF with the **native** engine (no browser). See [HTML-CSS.md](HTML-CSS.md). |
| `rtfToPdf(rtf)` | `Uint8Array` | Render RTF to PDF. |
| `officeToPdf(office)` | `Uint8Array` | Convert an Office/ODF file (docx/xlsx/pptx/doc/xls/ppt/odt/ods/odp) to PDF; format auto-detected by magic bytes. Faces the container embeds itself are de-obfuscated and used; families it only *references* fall back to a bundled face (use the two-phase variant below for an exact match). |
| `officeNeededFonts(office)` | `HtmlFontRequest[] \| null` | **Phase 1** for an exact Office render: the Google/system fonts the container **references but doesn't embed** (e.g. Calibri). Download each `url` → TTF and pass them to `officeToPdfWith`. `null` if not a recognized Office container; `[]` if none are needed. |
| `officeToPdfWith(office, fonts?)` | `Uint8Array` | **Phase 2**: render an Office container to PDF with the host-fetched `fonts` embedded, so referenced-but-not-embedded families lay out with the right metrics (e.g. Carlito for Calibri). The container's own embedded faces win on conflict, so `fonts = []` yields exactly `officeToPdf`'s output. |
| `imageToPdf(image)` | `Uint8Array` | Wrap a raster image in a single A4-page PDF (centred, shrink-to-fit, never upscaled). Format auto-detected: **PNG, JPEG, GIF, WebP, AVIF** (GIF/WebP/AVIF transcoded to PNG before embed; PNG keeps every color-type & bit-depth, Adam7 interlacing and transparency via `/SMask`). Empty `Uint8Array` for an unrecognized format. |
| `mergePdfs(parts)` | `Uint8Array` | Concatenate a list of sources into one (sequential `appendPages` under the hood). Each entry is either raw `Uint8Array` bytes (every page) or a `MergePart` `{ pdf, pages? }` selecting 1-based page numbers — so the two forms can be mixed (e.g. `[whole, { pdf: b, pages: [1, 3] }]`). `0` inputs → empty; a single whole PDF (no `pages`) → returned unchanged; otherwise → merged. |
| `gridsToXlsx(grids, sheetNames?)` / `gridsToOds(grids, sheetNames?)` | `Uint8Array` | Write a host-built grid (`pages[rows][cells]`, `string[][][]`) to an `.xlsx`/`.ods` workbook — one sheet per page — with the native writer. Supply your own table reconstruction and emit Office output with **no third-party library**. `sheetNames` (index-aligned) overrides the default `Page <n>` titles. |
| `xlsxToGrids(xlsx)` | `XlsxSheet[]` | Read an `.xlsx` back into `{ name, rows: string[][] }` sheets (the inverse of `gridsToXlsx`/`toXlsx`). Decodes inline strings, shared strings (`sharedStrings.xml`) and plain values. `[]` for non-xlsx input. |

### Fonts (engine-level helpers)

| Method | Returns | Description |
|--------|---------|-------------|
| `fontCatalog()` | `FontInfo[]` | The bundled catalog (~1951 families) with `{ family, category, google, weights }`. |
| `fontRequestUrl(family, weight?, italic?)` | `string` | The Google Fonts CSS URL to fetch for a family/weight (the host performs the HTTP request — the wasm has no network). |
| `parseCssFontUrl(css)` | `string` | Extract the trusted `gstatic` TTF URL from fetched Google Fonts CSS (host-pinned, anti-SSRF). |
| `helveticaWidth(size, text)` | `number` | Width of `text` in standard Helvetica at `size` pt (AFM metrics) — for laying out without embedding. |

### HTML / JavaScript engine

| Method | Returns | Description |
|--------|---------|-------------|
| `htmlNeededFonts(html)` / `htmlNeededFontsWith(html, header?, footer?)` | `HtmlFontRequest[]` | Which Google Fonts the HTML needs (fetch them, then pass to `htmlRender` / `htmlRenderWith`). The `*With` form also scans the running header/footer. |
| `htmlNeededResources(html, header?, footer?)` | `HtmlResourceNeed[]` | Unified phase 1: the fonts **and** external `<img>` URLs the document needs, in one list. Fetch each, pass fonts to `htmlRenderWith` and image bytes via `HtmlRenderOptions.resources` (the engine is zero-network); `data:` URIs need no entry. |
| `htmlRenderWith(html, fonts?, options?)` | `Uint8Array` | Phase 2 with full page control: `options = { pageSize?, pageWidth?, pageHeight?, margin?, header?, footer?, headerOffset?, footerOffset?, startPageNumber?, resources? }`. The header/footer are HTML painted in the page margins with `{{page}}`/`{{pages}}` tokens. |
| `evalJs(src)` | `string` | Run JavaScript in the native ES2021+ engine (Boa); returns the result stringified. |
| `runInlineScripts(html)` | `string` | Execute the `<script>`s in an HTML string against a native DOM and return the mutated HTML (the render paths do this automatically). |
| `pageSize(name)` | `{ w, h } \| null` | Look up a named page size (`"A4"`, `"a3-landscape"`, `"letter"`, …) in points; `null` if unknown. |

The unified-model lowering helpers (`officeToModel`, `htmlToModel`,
`applyModelOps`, `modelTo*`) also live on `GigaPdfEngine` — see
[The unified editable model](#the-unified-editable-model).

---

## `GigaPdfDoc`

### Lifecycle

| Method | Returns | Description |
|--------|---------|-------------|
| <a id="close"></a>`close()` | `void` | Free the wasm document handle. **Call once.** Using the doc after is undefined; closing twice corrupts the shared heap. |
| `pageCount()` | `number` | Number of pages. |
| `save()` | `Uint8Array` | Serialize to PDF bytes (plain, uncompressed streams + classic xref table — easiest to grep/debug). |
| `saveCompressed()` | `Uint8Array` | Serialize with every uncompressed stream Flate-compressed (still a classic xref table). |
| `saveOptimized(opts?)` | `Uint8Array` | Serialize with PDF 1.5+ **object streams** (`/ObjStm`) + a **cross-reference stream** (`/XRef`) — the most compact output (ISO 32000-1 §7.5.7/§7.5.8). `opts = { objectStreams?, xrefStreams? }` (both default `true`; `objectStreams` implies `xrefStreams`). Streams are Flate-compressed first. Linearization (Fast Web View) is not performed. |
| `pageInfo(page)` | `PageInfo` | `{ width, height, rotation, mediaBox }` — MediaBox size (unrotated), the `/Rotate` flag, and the raw `/MediaBox` `[x0,y0,x1,y1]` (preserves the box origin). |

### Pages

| Method | Returns | Description |
|--------|---------|-------------|
| `addPage(width, height, after?)` | `number` | Insert a blank page (points) after the 1-based `after` page (`0` prepends); returns its object number. |
| `deletePage(page)` | `boolean` | Remove a page. |
| `copyPage(page)` | `number` | Duplicate a page in place. |
| `movePage(from, to)` | `boolean` | Reorder a page. |
| `rotatePage(page, degrees)` | `boolean` | Add `degrees` (90/180/270) to the page's `/Rotate`. |
| `resizePage(page, width, height)` | `boolean` | Set the page MediaBox to `width`×`height` points. |
| `extractPages(pages)` | `Uint8Array` | A new **self-contained** PDF with just `pages` (1-based) — cross-page links/AcroForm fields/named dests/outline entries to dropped pages are pruned. Powers *split*. |
| `appendPages(otherPdf, pages?)` | `boolean` | Append pages of another PDF. With no `pages`, appends **every** page (powers *merge*). With `pages` — 1-based page numbers, in the order given — appends only that **selection** (ISO 32000-1 §7.7.3), each page keeping its content, resources, annotations and box geometry. Out-of-range numbers are skipped; an empty/all-out-of-range selection returns `false`. |

### Page boxes (Media / Crop / Bleed / Trim / Art)

The five boundary boxes of ISO 32000-1 §14.11.2. `getPageBoxes` resolves the full
**inheritance + default chain** (Crop→Media, Bleed/Trim/Art→Crop, and `/MediaBox`
/`/CropBox` inherited from an ancestor `/Pages` node), so every box always reads as
a concrete rectangle. `setPageBox` writes one box and **preserves the others** —
nothing is silently dropped on save. Setting `trim`/`bleed` is the prerequisite for
PDF/X and commercial-print pipelines (imposition, bleed, finished-size trimming).

| Method | Returns | Description |
|--------|---------|-------------|
| `getPageBoxes(page)` | [`PageBoxes`](#pageboxes) | All five boxes as `[x0,y0,x1,y1]` (points), defaults/inheritance applied, plus `declared` flags marking which are explicitly on the page (vs inherited/defaulted). |
| `setPageBox(page, kind, box)` | `boolean` | Set one box. `kind` is `"media" \| "crop" \| "bleed" \| "trim" \| "art"`; `box` is `{ x, y, w, h }` (origin + size, points), written as `[x, y, x+w, y+h]` and normalised. `false` on unknown kind / degenerate box / bad page. |

<a id="pageboxes"></a>`PageBoxes = { media, crop, bleed, trim, art: [x0,y0,x1,y1], declared: { media, crop, bleed, trim, art: boolean } }`.
The box constants live in `PAGE_BOX_KINDS` (`["media","crop","bleed","trim","art"]`).

### Page labels (`/PageLabels`)

Documents can number pages with schemes other than `1, 2, 3…` — front matter in
lowercase roman (`i, ii, iii`), the body in decimal, appendices as `A-1, A-2`, etc.
(ISO 32000-1 §12.4.2). Viewers show these in the page navigator, so dropping them on
edit is a visible regression. `getPageLabels`/`setPageLabels` read and author the
ranges; `pageLabel(n)` resolves the actual displayed string for a page.

| Method | Returns | Description |
|--------|---------|-------------|
| `getPageLabels()` | [`PageLabelRange`](#pagelabelrange)`[]` | Every label range, sorted by `startPage` (1-based). Empty when the document has no `/PageLabels`. |
| `setPageLabels(ranges)` | `boolean` | Replace all page labels. An **empty array clears** them. Ranges are sorted by `startPage` and collapsed to one entry per page (last wins). |
| `pageLabel(page)` | `string` | The viewer-visible label for the 1-based `page` (e.g. `"iv"`, `"A-3"`); the decimal page number when no range applies. |

<a id="pagelabelrange"></a>`PageLabelRange = { startPage, style, prefix, startNumber }`, where
`style` is `"decimal" | "romanLower" | "romanUpper" | "alphaLower" | "alphaUpper" | "none"`
(`none` = the prefix alone, no number), `prefix` is prepended to every page in the
range, and `startNumber` (≥ 1, default 1) is the value the range's first page gets.

### Margins & running header/footer

Page margins and a baked running header/footer on an **existing** PDF (for an
HTML→PDF header/footer instead, use `htmlRenderWith` — see [HTML / JavaScript](#html--javascript-engine)).

| Method | Returns | Description |
|--------|---------|-------------|
| `pageMargins(page)` | `PageMargins` | A page's `{ top, right, bottom, left }` margins (points): the `/CropBox`↔`/MediaBox` gap when a CropBox exists, else estimated from the content box. |
| `setPageMargins(page, m)` | `boolean` | Set a page's margins by insetting its `/CropBox` from the `/MediaBox` — a real, visible change. |
| `setHeader(spec)` | `boolean` | Bake a running **header** onto every in-range page (idempotent — re-baking replaces the prior one). `spec` is a [`HeaderFooterSpec`](#headerfooterspec). |
| `setFooter(spec)` | `boolean` | Bake a running **footer** (same spec). |
| `removeHeaders()` / `removeFooters()` | `boolean` | Remove every previously-baked running header / footer from all pages. |
| `headerFooter()` | `{ header, footer }` | **Reader** counterpart: detect the header/footer already baked into the PDF. Each side is a `HeaderFooterSpec` (with its recovered, per-page-substituted `text`) or `null`. Lets a Word-like editor reflect existing state. |

<a id="headerfooterspec"></a>`HeaderFooterSpec = { text, align?, fontSize?, color?, pageRange?, showOnFirstPage?, bandHeight? }`.
`text` may contain `{{page}}` (1-based page number) and `{{pages}}` (total page
count), substituted per page. Text is drawn in standard Helvetica inside the top
(header) / bottom (footer) margin band — no font embedding required. Defaults:
`align: "left"`, `fontSize: 10`, `color: [0,0,0]`, every page, `bandHeight: 36`.

### Reading text & content elements

| Method | Returns | Description |
|--------|---------|-------------|
| `textRuns(page)` | `TextRunInfo[]` | Raw content-stream text runs (operator + text), in draw order. |
| `structuredText(page)` | `TextLine[]` | Lines with bounding boxes (`x,y,w,h` + text) — for selection / extraction. |
| `pageBlocks(page)` | `GigaBlock[]` | The **layout blocks** of one page — its structural reconstruction (paragraphs, headings, lists, tables, shapes, images) in reading order, each `GigaBlock` keeping a top-down `frame` and every text run its `source_index` back to the editable content-stream operator. The **per-page** counterpart of `toModel()` (which reconstructs the whole document at once): a continuous / lazily-virtualized editor calls this one page at a time. Out-of-range page → `[]`. |
| `elements(page)` | `Element[]` | All content elements (text/image/path) with kind + bounds — the editor scene graph. |
| `textElements(page)` | `TextElementInfo[]` | **Rich** per-run text for an editor: text + bounds (user space) + resolved `fontFamily`/`bold`/`italic` + `fontSize` + RGB `color` + `rotation`. `index` is the text-run index for `replaceText` — extract, render and edit from one model. |
| `imageElements(page)` | `ImageElementInfo[]` | Image placements for an editor: `{ index, x, y, width, height, format, pixelWidth, pixelHeight, data, rotation, opacity }`. Bounds user space; `format` `jpeg`/`png`/`jp2`/`unknown`; `data` is the embeddable encoded bytes (JPEG/JP2 passthrough, Flate/raw RGB·Gray re-encoded to PNG); `rotation` (deg) and `opacity` (`/ca`) come from the placement CTM + `/ExtGState`. The native replacement for a reader's image extraction. |
| `vectorPaths(page)` | `VectorPathInfo[]` | Every painted path for a shape layer: `{ segments (M/L/C/Z), bounds, fill, stroke, strokeWidth, fillAlpha, strokeAlpha, dash }`. Geometry in user space; `fill`/`stroke` are RGB `0..=1` or `null`; clip-only paths are omitted. The read-side counterpart of the SVG→PDF drawing helpers. |
| `elementAt(page, x, y)` | `number` | Hit-test: index of the element under a point, or `-1`. |
| `search(query, caseInsensitive?)` | `SearchHit[]` | Full-text search with per-hit bounding boxes. |

### Editing existing content

| Method | Returns | Description |
|--------|---------|-------------|
| `replaceText(page, index, text)` | `boolean` | Replace the text of run `index` in place. **Font-aware**: a run in an embedded Type0/Identity-H face (TrueType *or* OpenType-CFF) is re-encoded through that font's char→glyph map; base-14/simple fonts use WinAnsi — so it works with **any** font. |
| `removeElement(page, index)` | `boolean` | Delete a content element. |
| `moveElement(page, index, dx, dy)` | `boolean` | Translate an element by `(dx, dy)` points. |
| `transformElement(page, index, m)` | `boolean` | Apply a full affine transform to an element in place. `m = [a, b, c, d, e, f]` is a PDF matrix (scale / rotate / shear / translate); it **generalises** `moveElement` (whose matrix is the pure translate `[1,0,0,1,dx,dy]`) to move + resize + rotate in one call. Non-destructive: the element is wrapped in `q  a b c d e f cm  …  Q`, so its internal coordinates are never rewritten — it works identically for text, images and shapes. `false` if the page/element doesn't exist. |
| `reorderElement(page, index, toFront)` | `boolean` | Change the paint (z) order of an element. `toFront = true` moves its op range to the **end** of the content stream (painted last → on top); `false` moves it to the **start** (painted first → behind everything). The moved range is re-wrapped in `q … Q` with the element's effective graphics state (fill/stroke colour, line width, dash and, for text, font) re-emitted inside it, so it renders identically at its new position and does not leak state onto neighbours; works for text, images and shapes. **The element's index changes after the move — re-read `pageElements`.** `false` if the page/element doesn't exist. |
| `setPathStyle(page, index, style)` | `boolean` | Re-style a **path** element in place (returns `false` for a non-path index). `style = { fill?, stroke?, strokeWidth?, fillAlpha?, strokeAlpha?, dash? }`; colours are RGB `[r,g,b]` in `0..=1`, `dash` is the PDF dash array (`[]` = solid). The path's op range is wrapped in `q … Q` and, for each provided field, an override operator is injected before the paint op: `fill`→`r g b rg`, `stroke`→`r g b RG`, `strokeWidth`→`w`, `dash`→`[…] 0 d`; omitted fields keep the inherited state. **`fillAlpha`/`strokeAlpha` (`0..=1`) are applied** — an `/ExtGState` carrying `/ca`/`/CA` is registered on the page and a `/<gs> gs` is injected into the path's `q … Q` wrap, so the alpha applies to that path run only. (For non-path elements such as images, use `setElementOpacity`.) |
| `setTextRunStyle(page, index, spans)` | `boolean` | Re-style **sub-ranges** of a **text** run in place — the by-character companion of `setPathStyle`. Each span sets the style of the `[start, end)` UTF-16 slice of the run's *decoded* text; `spans = [{ start, end, color?, sizePt?, bold?, italic?, underline?, strike? }]` (`color` is `[r,g,b]` in `0..=1`). The run is split so the rest keeps its style, and **positioning is preserved** — the original glyph codes (incl. `TJ` kerning) are sliced and re-emitted, never re-encoded, each styled slice wrapped in `q … Q`. `bold` is faux-bold (fill+stroke) when no bold variant is wired; `italic` is a no-op without an italic variant. Spans may be in any order and are clamped. `false` if `index` isn't a top-level text run. |
| `setElementOpacity(page, index, fillAlpha)` | `boolean` | Set one constant opacity (`fillAlpha`, clamped `0..=1`) on **any** element — text, image **or** shape — by registering an `/ExtGState` (`/ca` = `/CA` = `fillAlpha`, auto-named `GpGs<n>`) on the page and wrapping the element's op range in `q /<gs> gs … Q`. This is how you set an **image**'s alpha in place; shapes may also use `setPathStyle`'s `fillAlpha`/`strokeAlpha` (same `/ExtGState` mechanism, but those let you set fill and stroke alpha independently, whereas this uses one value for both). `false` if the page/index doesn't exist. |
| `duplicateElement(page, index)` | `boolean` | Clone an element. |
| `replaceImage(page, index, data)` | `boolean` | Swap the pixels of an **existing image XObject in place** (ISO 32000-1 §8.9) — replace a logo or photo while every reference to it stays intact. `index` is the **unified element index** of an image, exactly the `index` reported by `imageElements` (and accepted by `removeElement`/`transformElement`); `data` is a fresh **PNG or JPEG**. Unlike delete-then-re-add, the image keeps its object number, **every `/Do` placement, and its position / scale / rotation / clip matrix** — only the stream bytes and the image dictionary (`/Width`, `/Height`, `/ColorSpace`, `/BitsPerComponent`, `/Filter`) are rewritten. The new raster is re-encoded through the same path as `addImage` (PNG alpha → a fresh `/SMask`; JPEG → `/DCTDecode` passthrough) and need not match the old pixel size — it is drawn into the same box (transform the element first to re-fit it). `false` if `page`/`index` isn't a top-level image, or the bytes aren't a decodable PNG/JPEG. |

### Drawing new content

| Method | Returns | Description |
|--------|---------|-------------|
| `addText(page, x, y, size, text, fontObj, rgb?, opacity?, rotationDeg?, opts?)` | `boolean` | Draw selectable text in **any embedded** font (`fontObj` from `embedFont`/`extractFont`) — glyf TrueType or OpenType-CFF, each character encoded through the font's char→glyph map (Identity-H). `rotationDeg` rotates CCW about `(x,y)`. `opts = { underline?, strikethrough? }` bakes filled decoration rules (in the text colour, spanning the real glyph advance, following the rotation). |
| `addStandardText(page, x, y, size, text, fontName, rgb?, opacity?, rotationDeg?, opts?)` | `boolean` | Draw selectable text in a **built-in base-14** font (no embedding). Same `opts = { underline?, strikethrough? }` decorations as `addText`. See [Fonts](#fonts). |
| `addWatermark(page, x, y, size, text, rgb?, opacity?, rotationDeg?)` | `boolean` | Standard-Helvetica watermark (thin wrapper over `addStandardText`). |
| `addTextLayer(page, runs)` | `number` | Stamp an invisible (render-mode 3) text layer — e.g. a searchable OCR layer; one content append. Each run is `{ x, y, size, text, rotation? }`. Returns runs written. |
| `addImage(page, data, x, y, w, h, opacity?)` | `boolean` | Embed a PNG/JPEG as an image XObject in the box `(x,y,w,h)`. |
| `addImageWatermark(data, opts?)` | `boolean` | Stamp an **image watermark** across pages from raw bytes — decodes **PNG/JPEG/WebP/GIF/AVIF**, embeds it **once** and references it on every target page. `opts = { pages?, anchor?, offsetX?, offsetY?, width?, height?, rotationDeg?, opacity?, tile? }`: `pages` is 1-based (omit/`[]` = every page); `anchor` is `'center'` (default) or a corner; `offsetX`/`offsetY` nudge it (in `tile` mode they are the gaps between tiles); `width`/`height` set the size in points (height keeps aspect when omitted); `rotationDeg` rotates about the centre; `opacity` (0–1, default 0.25). `false` if the image can't be decoded. |
| `addRectangle(page, x, y, w, h, stroke?, fill?, lineWidth?, opacity?)` | `boolean` | Vector rectangle. `stroke`/`fill` are `0xRRGGBB` or `null`. |
| `addEllipse(page, cx, cy, rx, ry, stroke?, fill?, lineWidth?, opacity?)` | `boolean` | Vector ellipse (Bézier). |
| `addPolygon(page, points, close, stroke?, fill?, lineWidth?, opacity?)` | `boolean` | Polyline/polygon from a flat `[x0,y0,x1,y1,…]` list. |
| `addGradient(page, spec)` | `boolean` | Paint a **linear** or **radial** gradient over `spec.rect`. `spec = { kind: "linear"\|"radial", coords, stops, rect, extend?, opacity? }` — `coords` is `[x0,y0,x1,y1]` (linear) or `[x0,y0,r0,x1,y1,r1]` (radial); `stops` is ≥ 2 `{ offset (0…1), rgb }`. Rendered as a shading pattern (ISO 32000-1 §8.7.4/§8.7.3). `false` for < 2 stops. |
| `addFilledRectangle(page, rect, color, opacity?)` | `boolean` | Fill a rectangle (`{x,y,w,h}`) with `color` in **any** colour space (see `Color` — CMYK, spot `Separation`, gray, ICC). Press-ready counterpart of `addRectangle`. |
| `addFilledPolygon(page, points, color, opacity?)` | `boolean` | Fill a polygon (flat `[x0,y0,…]`, ≥ 3 vertices, auto-closed) with `color` in any colour space. `false` for < 3 vertices. |
| `addTextColor(page, x, y, size, text, font, color, opts?)` | `boolean` | Draw a base-14 text run in any colour space. `opts = { opacity?, rotation?, underline?, strikethrough? }`. |
| `setOverprint(page, fill, stroke, mode?)` | `boolean` | Turn overprint on/off for subsequent content (`/op`, `/OP`, `/OPM`; `mode` `0`=independent colorants, `1`=non-erasing, default `1`). Prepress trapping. |
| `addOutputIntent(profile, condition)` | `boolean` | Add a document **OutputIntent** embedding the ICC `profile` (`Uint8Array`; `/N` read from it). `condition` = output-condition id (e.g. `"Coated FOGRA39"`). Decoupled from PDF/A. |
| `addPath(page, svgPath, ox, oy, stroke?, fill?, lineWidth?, opacity?)` | `boolean` | Draw an SVG `<path d="…">` anchored at `(ox,oy)` (Y-flipped, `pdf-lib` convention). |
| `drawLine(page, x1, y1, x2, y2, rgb?, lineWidth?, opacity?)` | `boolean` | Straight line. |
| `addSvg(page, svg, x, y, w, h)` | `boolean` | Render SVG markup as **native vector paths** fitting its `viewBox` into `(x,y,w,h)`. |
| `redact(page, x, y, w, h, coverRgb?, hasCover?)` | `number` | True redaction: physically delete content intersecting the region; optional opaque cover. **Leaves images intact** — for scans/OCR use `redactPii`. Returns ops removed. |
| `redactPii(page, rects, opts?)` *(v0.52.4)* | — | **Irreversible** redaction of one or more `{ x, y, width, height }` rects (opts `{ cover?, coverRgb? }`): removes the text operators, **overwrites the pixels of any image** in the zone (safe on scanned/OCR'd pages), and draws an opaque black box. Not recoverable by copy-paste/extraction. See the [security note](COOKBOOK.md#note-redact-vs-redactpii). |

### Fonts

Three ways to draw real, selectable text — **no host font files required**:

1. **Base-14 standard fonts** — `addStandardText(page, x, y, size, text, fontName)`.
   `fontName` is a PostScript name: `Helvetica`, `Helvetica-Bold`,
   `Helvetica-Oblique`, `Helvetica-BoldOblique`, `Times-Roman`, `Times-Bold`,
   `Times-Italic`, `Times-BoldItalic`, `Courier`, `Courier-Bold`,
   `Courier-Oblique`, `Courier-BoldOblique`, `Symbol`, `ZapfDingbats`. WinAnsi
   encoding (Symbol/ZapfDingbats use their built-in encoding). No embedding —
   every viewer ships these. Several different standard fonts can coexist on one page.
2. **Any family via embedding** — `embedFont(family, font) → fontObj`, then
   `addText(…, fontObj)`. Accepts **any outline font file** — the flavour is
   auto-detected: a glyf **TrueType** (`.ttf`) becomes a Type0/CIDFontType2 +
   `FontFile2`; an **OpenType-CFF** (`.otf`/`OTTO`) becomes a Type0/CIDFontType0
   + `FontFile3` `/Subtype /OpenType`. Either way it's Identity-H with a full
   `/W` width array and a `/ToUnicode` CMap. Feed it a Google Font the host
   fetched (`fontRequestUrl` → fetch → `parseCssFontUrl` → fetch the program →
   `embedFont`) or any `.ttf`/`.otf`.
3. **The document's own embedded fonts** — `embeddedFonts()` lists `{ baseFont,
   format }`; `extractFont(name)` pulls a font's raw bytes out. `truetype` (glyf)
   and full OpenType `cff` (`OTTO`) re-embed directly via `embedFont`; bare `cff`
   (Type1C) and `type1` are read-only. Lets you re-bake edited text in the
   **exact original face** — `addText` and `replaceText` resolve its char→glyph
   map from `FontFile2` or `FontFile3`.

| Method | Returns | Description |
|--------|---------|-------------|
| `embedFont(family, font)` | `number` | Embed **any** outline program — glyf TrueType (`.ttf`) or OpenType-CFF (`.otf`), auto-detected; returns the font handle for `addText` (`0` on failure). |
| `addText(…)` / `addStandardText(…)` | `boolean` | See [Drawing](#drawing-new-content). |
| `embeddedFonts()` | `EmbeddedFont[]` | List the fonts the PDF embeds (`{ baseFont, format: "truetype"\|"cff"\|"type1" }`). |
| `extractFont(name)` | `{ format, bytes } \| null` | Pull an embedded font's program out by (fuzzy) `/BaseFont` name. |
| `neededFonts()` | `string[]` | Fonts the PDF references but does **not** embed (fetch + embed to fix tofu). |

### Annotations

| Method | Returns | Description |
|--------|---------|-------------|
| `annotations(page)` | `AnnotationInfo[]` | List markup annotations **with full metadata**: subtype + rect + `author`/`subject`/`created`/`modified`/`name` + `opacity` + `color` (RGB) + `quadPoints` (text markup) + `inkList` (freehand) + link target (`linkUri`/`linkPage`). |
| `addHighlight / addUnderline / addStrikeOut(page, x0, y0, x1, y1, rgb?)` | `boolean` | Text-markup annotations over a quad. |
| `addSquare(page, x0, y0, x1, y1, stroke?, fill?)` | `boolean` | Rectangle annotation. |
| `addLineAnnotation(page, x1, y1, x2, y2, rgb?, lineWidth?, endArrow?)` | `boolean` | Line annotation. `endArrow` (default `false`) draws an open arrowhead at the `(x2,y2)` end (`/LE [/None /OpenArrow]`). |
| `addFreeText(page, x0, y0, x1, y1, text, …)` | `boolean` | Free-text (typewriter) annotation. |
| `addTextNote(page, rect, rgb, meta?, icon?, open?)` | `boolean` | Sticky note at `rect = [x0,y0,x1,y1]`; `meta = { contents, author, id, date }`, `icon` (e.g. `"Note"`, `"Comment"`), `open` initial popup state. |
| `addInk(page, points, rgb?, lineWidth?)` | `boolean` | Freehand ink path from a flat point list. |
| `addStamp(page, x0, y0, x1, y1, label, rgb?)` | `boolean` | Rubber-stamp annotation. |
| `addCircleAnnotation(page, x0, y0, x1, y1, stroke?, fill?, lineWidth?)` | `boolean` | Ellipse (`/Circle`) inscribed in the rectangle; `stroke`/`fill` are `0xRRGGBB` or `null`. |
| `addPolygonAnnotation(page, points, stroke?, fill?, lineWidth?)` | `boolean` | Closed `/Polygon` through a flat `[x0,y0,x1,y1,…]` vertex list. |
| `addPolylineAnnotation(page, points, rgb?, lineWidth?)` | `boolean` | Open `/PolyLine` through a flat vertex list. |
| `addCaretAnnotation(page, x0, y0, x1, y1, rgb?)` | `boolean` | `/Caret` insertion mark (a small upward wedge). |
| `addMarkupAnnotation(…)` | `boolean` | Generic markup with shared reviewer metadata. |
| `regenerateAppearance(page, index)` | `boolean` | Rebuild the 0-based annotation's `/AP` appearance from its geometry after editing its colour/border/geometry. `false` for subtypes that can't be reconstructed (FreeText/Stamp/Text/Link). |
| `removeAnnotation(page, index)` | `boolean` | Delete an annotation. |
| `flattenAnnotations(page)` | `number` | Bake annotation appearances into page content (non-interactive). |

### Interactive forms (AcroForm)

| Method | Returns | Description |
|--------|---------|-------------|
| `fields()` | `FieldInfo[]` | Every terminal field with kind, value, flags. |
| `setTextField(name, value)` | `boolean` | Fill a text field. |
| `setCheckbox(name, checked)` | `boolean` | Check/uncheck. |
| `setRadio(name, value)` | `boolean` | Select a radio option by export value. |
| `setChoice(name, values)` | `boolean` | Select dropdown/listbox option(s). |
| `addTextField(page, name, bounds, value, opts?)` | `boolean` | Create a text field; `opts = { maxLen, multiline, password, style }`. |
| `addCheckbox(page, name, bounds, checked, opts?)` | `boolean` | Create a checkbox; `opts = { export, style }`. |
| `addRadioGroup(page, name, options, opts?)` | `boolean` | Create a radio group; `options = [{ export, rect }]`. |
| `addComboBox(page, name, options, opts?)` | `boolean` | Create a dropdown; `opts = { selected, editable, style }`. |
| `addListBox(page, name, options, opts?)` | `boolean` | Create a list box; `opts = { selected, multi, style }`. |
| `addSignatureField(page, name, rect, opts?)` | `boolean` | Create a **visible signature field** (`/FT /Sig`) the signing pipeline can target; sets the AcroForm `/SigFlags`. `opts = { style }`. |
| `setFieldScript(name, trigger, js)` | `boolean` | Attach field-level JavaScript to a field's `/AA` — `trigger ∈ "keystroke" \| "format" \| "validate" \| "calculate"`. `false` if no field has that name. |
| `setCalculationOrder(names)` | `boolean` | Set the AcroForm `/CO` — the order in which `calculate` scripts recompute (unknown names skipped). |
| `removeField(name)` | `boolean` | Delete a field (from `/Fields`, `/CO` and every page's annotations). `false` if not found. |
| `regenerateFieldAppearance(name)` | `boolean` | Rebuild a field's `/AP` from its current value/style (text/choice/checkbox) — call after changing a value. `false` for unknown names or kinds that can't be rebuilt alone (e.g. a radio parent). |
| `flattenForm()` | `number` | Bake every **AcroForm** field widget across all pages into page content and drop `/AcroForm` — the document is no longer fillable (`fields()` then returns `[]`). Returns the number of widgets baked. |
| `flattenFormXObjects(page)` | `number` | Inline & **de-share** a page's form XObjects (`/Subtype /Form` invoked via `Do`) into its content stream, so their text/graphics become **ordinary editable page content** with real text-run indices — the enabler for editing invoice/template text in place via `replaceText`/`moveElement`/`removeElement` instead of a redact+add overlay. Image XObjects are untouched. Distinct from `flattenForm` (which flattens **interactive AcroForm fields**). Returns the number of form XObjects inlined. |

Every created widget gets a real `/AP` appearance stream and the form is flagged
`NeedAppearances`. `FieldStyle = { fontSize, color, border, background, borderWidth }`.

### Links, layers, outline, metadata

| Method | Returns | Description |
|--------|---------|-------------|
| `links(page)` | `LinkInfo[]` | Hyperlinks with `{ x0,y0,x1,y1, kind: "uri"\|"page"\|"unknown", uri?, page? }`. |
| `addUriLink(page, x0, y0, x1, y1, uri)` | `boolean` | External URL link over a rect. |
| `addGotoLink(page, x0, y0, x1, y1, targetPage)` | `boolean` | Internal "jump to page" link (explicit page reference). |
| `addLink(page, rect, action)` | `boolean` | Link over `rect` (`{x,y,w,h}`) carrying any `Action` — the full model: `goto` (every fit mode), `gotoR`, `uri`, `named` navigation, `launch`, `javascript`, `submitForm`, `resetForm`. `false` if the action is malformed. |
| `setOpenAction(action)` | `boolean` | Set the document `/OpenAction` (performed on open) from an `Action`. |
| `removeLink(page, linkIndex)` | `boolean` | Remove the `linkIndex`-th `/Link` annotation on the page (links counted in order, ignoring other annotations). |
| `addNamedDest(name, targetPage)` | `boolean` | Register a named destination `name` → page (a `/Fit` view) in the catalog `/Dests`. Resolves through the catalog, so it survives split/extract while its page is kept. |
| `namedDests()` | `NamedDest[]` | The catalog's named destinations as `{ name, page }` pairs — from both the inline `/Dests` dictionary **and** the PDF 1.2+ `/Names /Dests` name tree (parity with a reader's `getDestinations()`). |
| `addGotoLinkNamed(page, x0, y0, x1, y1, name)` | `boolean` | Internal link that jumps to a **named** destination (`/Dest /name`) — the retargetable, split-safe alternative to `addGotoLink`. |
| `layers()` | `LayerInfo[]` | Optional-content groups (calques) `{ id, name, visible, locked }`. |
| `addLayer(name)` | `number` | Create a layer; returns its id (`0` on error). |
| `setLayerVisibility(id, visible)` / `setLayerLocked(id, locked)` | `boolean` | Toggle a layer. |
| `removeLayer(id)` | `boolean` | Delete a layer. |
| `outline()` | `OutlineEntry[]` | The flattened bookmark list: `{ title, level, page?, bold?, italic?, color?, destKind?, x?, y?, zoom? }` — nesting depth, destination page + `/XYZ` position/zoom, and `/F`+`/C` label style. Rebuild the tree from `level`. |
| `setOutline(entries)` | `boolean` | Replace the bookmark tree (`{level, page?, title}` per entry — a `/Fit` page jump). |
| `setBookmarks(bookmarks)` | `boolean` | Replace the bookmark tree with `Bookmark[]` (`{title, level, action?}`) — bookmarks can carry **any** `Action` (a `goto` becomes a `/Dest`, anything else an `/A`). Empty array clears the outline. |
| `getMetadata(key)` / `setMetadata(key, value)` | `string` / `boolean` | Read/write a **single** Info-dictionary entry (`Title`, `Author`, …) — touches only `/Info`. |
| `setInfo(fields)` | `boolean` | Set the typed [`InfoFields`](#infofields) (`{ title?, author?, subject?, keywords?, creator?, producer?, creationDate?, modDate? }`), writing **both** `/Info` and a synced XMP `/Metadata` packet. **Partial update** — omitted fields are left unchanged. The cure for the "two sources of truth" drift between Info and XMP. |
| `getXmp()` / `setXmp(xmp)` | `Uint8Array \| null` / `boolean` | Read / replace the catalog `/Metadata` XMP packet (raw bytes; `setXmp` also accepts a UTF-8 string). `getXmp` is `null` when the document has no XMP. |
| `setViewerPreferences(prefs)` | `boolean` | Write the catalog `/ViewerPreferences` (ISO 32000-1 §12.2) from a `ViewerPreferences` object — `hideToolbar?`, `hideMenubar?`, `hideWindowUI?`, `fitWindow?`, `centerWindow?`, `displayDocTitle?` (omitted = leave unchanged) and `direction?` (`"L2R"`/`"R2L"`). An emptied dictionary is removed. `false` on an invalid `direction`. |
| `setPageLayout(layout)` | `boolean` | Set the catalog `/PageLayout` (how pages are arranged): a `PageLayout` (`"SinglePage"` \| `"OneColumn"` \| `"TwoColumnLeft"` \| `"TwoColumnRight"` \| `"TwoPageLeft"` \| `"TwoPageRight"`), or `null` to remove the key. `false` on an unknown name. |
| `setPageMode(mode)` | `boolean` | Set the catalog `/PageMode` (which panel opens): a `PageMode` (`"UseNone"` \| `"UseOutlines"` \| `"UseThumbs"` \| `"FullScreen"` \| `"UseOC"` \| `"UseAttachments"`), or `null` to remove the key. `false` on an unknown name. |
| `attachments()` | `Attachment[]` | Extract every embedded file from the `/Names /EmbeddedFiles` name tree: `{ name, filename, mime, description, creationDate, modDate, data }` where `data` is the decoded bytes. The native replacement for a reader's `getAttachments()`. |
| `addAttachment(name, bytes, opts?)` | `boolean` | Embed a document-level file (`/Names /EmbeddedFiles`). `opts` is `{ mime?, description? }`; re-using a `name` **replaces** it. Bytes are stored FlateDecode-compressed. |
| `addAssociatedFile(name, bytes, relationship, opts?)` | `boolean` | Embed an **associated file** (`/AF`, PDF/A-3) — the Factur-X / ZUGFeRD / Order-X invoice-XML mechanism. `relationship` is `"source" \| "data" \| "alternative" \| "supplement" \| "unspecified"` (invoices use `"alternative"`); the filespec gets `/AFRelationship` and is linked from the catalog `/AF`. |
| `removeAttachment(name)` | `boolean` | Remove an attachment (and its `/AF` link). `true` if one was removed, `false` if none had that name. |
| `addFileAttachmentAnnot(page, rect, name, icon?)` | `boolean` | Anchor a visible **FileAttachment** annotation (ISO 32000-1 §12.5.6.15) over `rect` (`{ x, y, w, h }`) pointing at the already-embedded `name`. `icon` ∈ `"PushPin"` (default) / `"Paperclip"` / `"Graph"` / `"Tag"`. |
| `addDocumentJavascript(name, script)` | `boolean` | Install a **document-level JavaScript** in the catalog `/Names /JavaScript` name tree (ISO 32000-1 §7.7.3.4 + §12.6.4.16): a named `<< /S /JavaScript /JS … >>` action. Viewers run document-level scripts in **name (lexical) order** on open — where form calculation/validation helper libraries live. Re-using a `name` **replaces** it; long sources are stored as a FlateDecode stream; sibling `/Names` subtrees (`/EmbeddedFiles`, `/Dests`, …) are preserved. `false` on an empty name. |
| `documentJavascripts()` | `DocumentJavascript[]` | The document-level scripts as `{ name, script }` in name (lexical) order (decodes both a literal `/JS` string and a `/JS` stream). |
| `removeDocumentJavascript(name)` | `boolean` | Remove a document-level JavaScript from `/Names /JavaScript`. `true` if one was removed, `false` if none had that name. |

<a id="infofields"></a>`InfoFields = { title?, author?, subject?, keywords?, creator?, producer?, creationDate?, modDate? }`
— the standard document-information fields. `setInfo` maps them to both `/Info`
(`/Title`, `/Author`, …) and XMP (`dc:title`, `dc:creator`, `dc:description`,
`pdf:Keywords`, `xmp:CreatorTool`, `pdf:Producer`, `xmp:CreateDate`/`ModifyDate`).
Dates are PDF date strings (`"D:YYYYMMDDHHmmSS+HH'mm'"`), converted to ISO 8601 in
the XMP.

### Conversions (PDF → X)

Each returns the target file as bytes (or a string for `toText`/`toHtml`). These
produce **real editable elements** (positioned text boxes, re-embedded images,
reconstructed tables for spreadsheets) — not a rasterised image.

| Method | Output |
|--------|--------|
| `toText()` | plain text (`string`) |
| `toHtml()` | HTML (`string`) |
| `toModel()` | the unified editable [`GigaDocument`](#the-unified-editable-model) model |
| `toDocx()` / `toOdt()` | Word / OpenDocument Text |
| `toPptx()` / `toOdp()` | PowerPoint / OpenDocument Presentation |
| `toXlsx()` / `toOds()` | Excel / OpenDocument Spreadsheet |
| `toRtf()` | Rich Text Format |
| `toPdfA()` | PDF/A-2b archival PDF |
| `toTagged({ pdfUa? })` | tagged (accessible) PDF — `/StructTreeRoot` + marked content + `/MarkInfo`/`/Lang`/`/RoleMap`/`/Alt`, without PDF/A; `pdfUa` adds the PDF/UA-1 identifier |

### The unified editable model

A **format-neutral document tree** ([`GigaDocument`](#types): sections → pages →
blocks → runs) every format lowers into and is rebuilt from. Lower any source
into it, edit it with structured ops, then raise it to any target — the substrate
for a universal editor that edits every format the same way. See the
[round-trip recipe](COOKBOOK.md#round-trip-the-unified-editable-model).

| Method | Class | Returns | Description |
|--------|-------|---------|-------------|
| `doc.toModel()` | `GigaPdfDoc` | `GigaDocument` | Lower this PDF into the unified model. |
| `officeToModel(office)` | `GigaPdfEngine` | `GigaDocument \| null` | Lower an Office/ODF file (auto-detected); `null` if not a recognised container. |
| `htmlToModel(html)` | `GigaPdfEngine` | `GigaDocument` | Lower an HTML string into the model. |
| `mdToModel(md)` | `GigaPdfEngine` | `GigaDocument` | Lower a Markdown string (CommonMark-ish: headings, lists, GFM tables, fenced code, emphasis/links) into the model. |
| `csvToModel(csv)` | `GigaPdfEngine` | `GigaDocument \| null` | Lower a CSV file (UTF-8 bytes, RFC 4180, auto `,`/`;`/tab/`\|` delimiter) into the model as a single editable table; `null` if no parseable fields. |
| `applyModelOps(model, ops)` | `GigaPdfEngine` | `GigaDocument` | Apply a batch of [`ModelOp`](#types) edits (run in order; out-of-range addresses are no-ops, so a partial batch never throws). |
| `modelToDocx / modelToXlsx / modelToPptx / modelToOdt / modelToOds / modelToOdp / modelToPdf / modelToEpub(model)` | `GigaPdfEngine` | `Uint8Array` | Raise the model to each binary target (`modelToEpub` emits an `.epub` e-book). |
| `modelToHtml / modelToRtf / modelToMarkdown / modelToCsv(model)` | `GigaPdfEngine` | `string` | Raise the model to HTML / RTF / Markdown / CSV text. |

A `ModelOp` addresses a block by `[section, page, index]` (zero-based). The full
op set:

- **Runs:** `setRunText`, `restyleRun`, `insertRun`, `deleteRun`.
- **Blocks:** `insertBlock`, `deleteBlock`, `moveBlock`, `setBlockText`,
  `restyleBlock`, `setBlockFrame`, `setBlockRotation`.
- **Paragraph:** `setParagraphStyle` (a `GigaParaPatch`: `align`, `indent_left`,
  `indent_right`, `first_line`, `space_before`, `space_after`, `line_height`).
- **Lists:** `setListLevel`, `setListMarker`, `setListOrdered`.
- **Tables:** `setCellText`, `insertTableRow`, `deleteTableRow`,
  `insertTableColumn`, `deleteTableColumn`, `setCellSpan`, `setCellShading`,
  `setRowHeight`, `setColWidth`, `setTableBorder` (structural edits keep
  `col_widths` + spans coherent).
- **Spreadsheets:** `setSheetCell`, `insertSheetRow`, `deleteSheetRow`,
  `insertSheetColumn`, `deleteSheetColumn` (shift cells and re-map merge ranges).

A run's character style (`GigaCharStyle`) carries `bold`, `italic`, `underline`,
`strike`, `color`, `size_pt`, and `valign` (`"baseline" | "super" | "sub"` —
sub/superscript), so decorations and offset baselines survive a round-trip.

### Render

| Method | Returns | Description |
|--------|---------|-------------|
| `renderPage(page, scale?)` | `Uint8Array` | Rasterise a page to PNG at `scale` (1 = 72 dpi). Native rasteriser (glyphs, images, vectors, SVG, colour emoji). |
| `renderPageNoText(page, scale?)` | `Uint8Array` | Rasterise a page to PNG **without** its page-content text — for editors that overlay real editable text on a text-free background; vectors/gradients/images/annotations are still rendered. |
| `renderPageExcluding(page, indices, scale?)` | `Uint8Array` | Rasterise a page to PNG while **omitting** the given top-level unified element `indices` (from `pageElements`). Each excluded element paints nothing (fills, strokes, shadings, images and text alike) while everything else renders normally — **generalises** `renderPageNoText` (which suppresses *all* text). Built for live-overlay editing: paint a background without the element being edited, then overlay an editable version on top. An empty `indices` renders the full page; unknown indices are ignored. |
| `rgbaToPng(rgba, width, height)` | `Uint8Array` | *(engine-level)* Encode raw RGBA pixels (`width*height*4`, row-major, non-premultiplied) to PNG with the native encoder — no `canvas`/image library. Empty on a length mismatch. |
| `resizeRgba(rgba, sw, sh, dw, dh)` | `Uint8Array` | *(engine-level)* Resample raw RGBA `sw`×`sh` → `dw`×`dh` with the native alpha-correct resampler (triangle kernel, footprint scaled for down/up) — no `sharp`/image library. Empty on a bad input. |
| `encodeJpeg(rgba, width, height, quality?)` | `Uint8Array` | *(engine-level)* Encode RGBA → baseline JPEG (native codec, 4:4:4, `quality` 1–100, default 82) — no image library. Alpha composited on white. |
| `encodeWebp(rgba, width, height)` | `Uint8Array` | *(engine-level)* Encode RGBA → **lossless** WebP (VP8L, native codec) — no `libwebp`. Alpha preserved exactly. Empty on a length mismatch. |
| `decodeJpeg(bytes)` / `decodePng(bytes)` | `DecodedImage \| null` | *(engine-level)* Decode a baseline JPEG / PNG to `{ width, height, rgba }`. `null` on a malformed/unsupported stream. |
| `decodeWebp(bytes)` | `DecodedImage \| null` | *(engine-level)* Decode a WebP — lossless **VP8L** *and* lossy **VP8** keyframes both supported. Extended/animated (`VP8X`) returns `null`. |
| `decodeGif(bytes)` | `DecodedImage \| null` | *(engine-level)* Decode the **first frame** of a GIF (LZW, interlace, transparency) to RGBA. `null` if unsupported. |
| `decodeAvif(bytes)` | `DecodedImage \| null` | *(engine-level)* Decode an AVIF still — pure-Rust AV1 intra decoder (lossy + lossless transforms, deblock §7.14, CDEF §7.15, palette §5.11.46-50, reduced + full headers), bit-exact vs dav1d. `null` for animated / film-grain / loop-restoration streams. |

### OCR & text intelligence

**OCR is not in this WASM SDK** — it's a separate **native** crate, **`gigapdf-ocr-rten`**, because
it runs **PaddleOCR PP-OCR** models through **RTen** (a pure-Rust ONNX runtime, no C++/Tesseract)
whose weights are far heavier than the lean ~540 KB WASM core. Run it host-side (a service/binary)
and expose it as an endpoint; this WASM SDK provides the **text-layer** side (`addTextLayer`) so a
host can stamp recognized words back onto the PDF to make a scan searchable. For PDFs that already
carry text, prefer the SDK's `toText` / `structuredText` / `search` (exact, no OCR needed).

**Engine:** shared **DBNet** detector + per-language **SVTR/CRNN + CTC** recognizers, with automatic
per-line **script selection** (each line routed to the highest-confidence printed recognizer — no
separate classifier). **13 printed languages**: Arabic (RTL), **Hebrew** (RTL, our own trained
model), Simplified/Traditional Chinese, Japanese, Korean, Cyrillic, Devanagari, Tamil, Telugu,
Kannada, English, Latin (FR/DE/ES/…). Plus **opt-in Latin/Cyrillic/Greek handwriting** (`latin_hw`,
our own trained CRNN — real IAM/RIMES/… handwriting, dynamic-width).

Rust API (`gigapdf_ocr_rten`):

| Method | Returns | Description |
|--------|---------|-------------|
| `OcrEngine::load_models_dir(dir)` | `OcrEngine` | Load the shared `det.rten` + every recognizer present in `dir` (per `REC_MODELS`). |
| `OcrEngine::load(det, rec, dict)` | `OcrEngine` | Detector + a single recognizer (convenience). |
| `recognize_page(&img)` | `Vec<Line>` | Detect + recognize, **auto script selection** (printed). `Line { bbox, text, confidence, model }`. |
| `recognize_line_auto(&line)` | `(text, conf, model)` | One cropped line, auto-selected recognizer. |
| `ocr_pdf_page(&doc, page, scale)` | `Vec<OcrWord>` | OCR a **PDF page** (rasterized via `gigapdf-core`); boxes in **PDF user space** (bottom-left). `scale ≥ 2`. |
| `ocr_pdf_page_text(&doc, page, scale)` | `String` | Same, plain text (reading order). |
| `recognize_page_handwriting(&img)` | `Vec<Line>` | **Handwriting** (`latin_hw`) — bypasses auto selection. |
| `recognize_page_with(&img, name)` / `recognize_line_with(&line, name)` | `Vec<Line>` / `Option<(text,conf)>` | Force a specific recognizer by name (`HANDWRITING_MODEL` = `"latin_hw"`). |
| `has_handwriting()` / `rec_count()` | `bool` / `usize` | Introspection. |

`OcrWord { text, x, y, width, height, confidence, model }` is the replacement for the old
`Document::ocr_page` — map straight onto `addTextLayer` to make a scan searchable. Handwriting is
**opt-in** (a HW model is overconfident on printed input, so it's excluded from auto selection):
call `recognize_page_handwriting` / `..._with(img, HANDWRITING_MODEL)` when the input is handwritten.

Models are fetched/converted at deploy (`tools/fetch_models.sh`); see
[`OCR_ARCHITECTURE.md`](./OCR_ARCHITECTURE.md) and [`crates/ocr-rten/README.md`](../crates/ocr-rten/README.md).

### Security

#### Encryption & permissions

| Method | Class | Returns | Description |
|--------|-------|---------|-------------|
| `saveEncrypted(password, fileId, opts?)` | `GigaPdfDoc` | `Uint8Array` | Encrypt with the PDF Standard Security Handler — **default AES-256 (R6)**, or `"rc4"` (RC4-128) / `"aes128"`. `opts = { ownerPassword?, algorithm?, flags?, permissions?, keySeed? }`. `flags` (a `Partial<PdfPermissions>`) is the readable way to set the eight access permissions; `permissions` is the raw signed-32-bit `/P` (overridden by `flags`). For AES-256 a secret 32-byte key is taken from `keySeed` or generated with Web Crypto (RC4/AES-128 derive it from the password). |
| `changePasswords(newPassword, fileId, opts?)` | `GigaPdfDoc` | `Uint8Array` | Re-encrypt an already-opened document with new passwords (opening with the old password authorises it). Same `opts` as `saveEncrypted` plus `encryptMetadata?` (when `false`, the metadata stream stays in the clear). |
| `removeEncryption()` | `GigaPdfDoc` | `Uint8Array` | Strip encryption from an opened document → a plaintext PDF. |
| `encryptForRecipients(certificates, opts?)` | `GigaPdfDoc` | `Uint8Array` | **Public-key (certificate) encryption** (`/Filter /Adobe.PubSec`, ISO 32000-1 §7.6.5): only a holder of a recipient private key can open it. `certificates` are DER X.509 certs (`Uint8Array[]`). `opts = { flags?, permissions?, aes256?, encryptMetadata?, seed?, rngSeed? }` — `seed`/`rngSeed` default to Web Crypto randomness. Open with `openWithPrivateKey`. |
| `openWithPrivateKey(pdf, certificate, privateKey)` | `GigaPdfEngine` | `GigaPdfDoc \| null` | Open a public-key-encrypted PDF with a recipient DER `certificate` + PKCS#1 RSA `privateKey`; `null` if the key is not a recipient. |
| `permissionsToP(flags?)` | `GigaPdfEngine` | `number` | Pack eight `PdfPermissions` flags (omitted = granted) into a signed 32-bit `/P` value (ISO 32000-1 Table 22). Feed to `saveEncrypted` via `opts.permissions`. |
| `decodePermissions(p)` | `GigaPdfEngine` | `PdfPermissions` | Decode a `/P` bitmask into the eight named booleans (`true` = granted). |
| `getPermissions(pdf)` | `GigaPdfEngine` | `PdfPermissions` | Read a PDF's access permissions **without decrypting** it (an unencrypted document grants everything). |

`PdfPermissions = { print, modify, copy, annotate, fillForms, accessibility,
assemble, printHighRes }` (all `boolean`).

#### Digital signatures

Four levels, escalating in long-term assurance. All produce a CMS signature in a
`/Sig` field over a `/ByteRange`-patched PDF, **entirely in-engine** (no
node-forge / @signpdf / pdf-lib). `sign`/`signP12` are synchronous; the
timestamped/LTV methods are **`async`** because they need network round trips.

| Method | Returns | Level | Description |
|--------|---------|-------|-------------|
| `sign(fields, random, keyBits?)` | `Uint8Array` | **B** (self-signed) | Self-signed `adbe.pkcs7.detached` signature (an ephemeral digital ID). `fields = "name\treason\tdate\tnotBefore\tnotAfter"`, `random` ≥ 256 host bytes, `keyBits` RSA modulus (default 2048). |
| `signP12(p12, password, opts?)` | `Uint8Array` | **B** (PKCS#12) | Sign with a **user PKCS#12** identity (CA/eIDAS cert + RSA key), imported natively. `opts = { name?, reason?, date?, location?, contactInfo? }` (`SignP12Options`). **Throws** a generic error on a bad password/file/cipher (anti-enumeration). |
| `signTimestamped(opts)` | `Promise<Uint8Array>` | **B-T** (PAdES) | B signature **+ an RFC 3161 trusted timestamp** embedded in the SignerInfo (`ETSI.CAdES.detached`, `signing-certificate-v2`, `id-aa-timeStampToken`). `opts` is `SignTsaOptions`. |
| `signLtv(opts)` | `Promise<Uint8Array>` | **B-LT / B-LTA** (PAdES-LTV) | B-T **+ a `/DSS`** (Document Security Store) carrying the certificate chain and OCSP/CRL revocation material, so the signature validates long after the certs expire/revoke. With `opts.archiveTimestamp` a `/DocTimeStamp` over the whole file is added (**B-LTA**, renewable archival). `opts` is `SignLtvOptions`. |
| `certify(fields, random, docmdpLevel, keyBits?)` | `Uint8Array` | **Certify** (DocMDP) | Like `sign` but **certifies** the document: writes `/Perms /DocMDP` and a `/Reference` transform declaring which later changes are allowed — `docmdpLevel` is `1` (no changes), `2` (form-fill + sign) or `3` (also annotate). |
| `signatures()` | `SignatureInfo[]` | — | List every signature (`/Sig` field) with `{ fieldName, signerName, reason, location, date, subFilter, byteRange }`. Reads the parsed model; for validity call `verifySignatures`. |
| `verifySignatures(pdfBytes)` | `SignatureReport[]` | — | **Verify** each signature against the **original bytes** (`pdfBytes` = what you opened): `{ fieldName, byteRangeOk, digestOk, signatureOk, coversWholeDocument, signerCommonName, certCount, algorithm }`. `digestOk` = content integrity (ByteRange SHA-256 vs CMS `messageDigest`); `signatureOk` = the RSA SignerInfo signature; `coversWholeDocument` = nothing appended after. RSA + SHA-256 only. |

`signP12` imports PBES2 (PBKDF2 + AES) and PBES1 (3DES, RC2-40) bags and verifies
the integrity MAC.

##### `SignTsaOptions` (B-T) — extends `SignP12Options`

```ts
interface SignTsaOptions extends SignP12Options {
  tsaUrl: string;                         // TSA endpoint, e.g. "https://freetsa.org/tsr"
  tsaFetch?: (req: Uint8Array, url: string) => Promise<Uint8Array>;
  p12?: Uint8Array;                       // PKCS#12 identity; omit → self-signed path
  password?: string;                      // PKCS#12 passphrase
  random?: Uint8Array;                    // self-signed path: ≥ 256 random bytes
  keyBits?: number;                       // self-signed path: RSA bits (default 2048)
  notBefore?: string;                     // self-signed cert notBefore (UTCTime YYMMDDHHMMSSZ)
  notAfter?: string;                      // self-signed cert notAfter
  nonce?: Uint8Array;                     // optional 8–16 bytes echoed by the TSA
}
```

The signing identity is **`p12` + `password`** when supplied, otherwise a
freshly-generated self-signed digital ID (`random` + `notBefore`/`notAfter`).

##### `SignLtvOptions` (B-LT / B-LTA) — extends `SignTsaOptions`

```ts
interface SignLtvOptions extends SignTsaOptions {
  archiveTimestamp?: boolean;             // add the B-LTA /DocTimeStamp (2nd TSA round trip). default false (B-LT)
  revocationFetch?: (req: Uint8Array, url: string) => Promise<Uint8Array>; // OCSP override
  crlFetch?: (url: string) => Promise<Uint8Array>;                         // CRL override
}
```

##### Host-fetch model (2 phases)

The WASM core has no network. The signing methods run a two-phase flow with the
HTTP **in between**, handled by the SDK:

1. **B-T** — the engine builds the signature and returns the DER `TimeStampReq`;
   the SDK POSTs it to `tsaUrl` and embeds the returned `TimeStampResp`.
2. **LTV** — after the B-T signature, the engine reports which OCSP/CRL URLs to
   fetch (taken **from the certificates' AIA / CRL-DP extensions**); the SDK
   fetches each (unreachable responders are skipped — the `/DSS` is built from
   whatever resolves) and stores the material. A self-signed identity (no
   AIA/CRL-DP) simply yields a `/DSS/Certs`-only store.

The default HTTP helpers are exported and overridable:

| Function | Default behaviour |
|----------|-------------------|
| `defaultTsaPost(url, req)` | POST `application/timestamp-query` → raw `TimeStampResp`. |
| `defaultOcspPost(req, url)` | POST `application/ocsp-request` → raw `OCSPResponse`. |
| `defaultCrlGet(url)` | GET the URL → raw `CertificateList` (CRL). |

> **SSRF note.** OCSP/CRL/TSA URLs are **host-supplied** — for LTV they come from
> the certificate extensions, so a malicious certificate could point them at an
> internal host. The default helpers do **no allow-listing** (they set
> `redirect: "error"` only). A service exposing signing to untrusted input MUST
> validate these URLs: pass `tsaFetch` / `revocationFetch` / `crlFetch` to inject
> an allow-list, auth headers or a proxy. The engine only computes *which* URLs
> to fetch; the host decides whether to.

```ts
// B-T with a PKCS#12 identity and FreeTSA, host-controlled fetch (allow-list):
const signed = await doc.signTimestamped({
  p12, password, name: "Jane Doe", reason: "Approved",
  date: "D:20260616120000Z",
  tsaUrl: "https://freetsa.org/tsr",
  tsaFetch: async (req, url) => {           // host SSRF gate + auth
    assertAllowed(url);
    const r = await fetch(url, { method: "POST",
      headers: { "Content-Type": "application/timestamp-query" }, body: req });
    return new Uint8Array(await r.arrayBuffer());
  },
});

// B-LTA: B-T + DSS (OCSP/CRL) + a document timestamp over the whole file.
const ltv = await doc.signLtv({
  p12, password, tsaUrl: "https://freetsa.org/tsr", archiveTimestamp: true,
});
```

---

## Types

All result/option shapes are exported interfaces — import them for typed code:

```ts
import type {
  FontInfo, EmbeddedFont, PageInfo, PageMargins, HeaderFooterSpec, HeaderFooterAlign,
  TextLine, TextRunInfo, Element, TextElementInfo, DocumentLanguage,
  ImageElementInfo, VectorPathInfo, PathSegment, PdfPermissions,
  SearchHit, AnnotationInfo, FieldInfo, FieldKind, FieldStyle, RadioOption,
  LinkInfo, LayerInfo, OutlineEntry, ViewerPreferences, PageLayout, PageMode,
  NamedDest, Action, Destination, Bookmark,
  SignatureInfo, SignatureReport, GradientSpec, GradientStop, Color, Attachment, XlsxSheet, DecodedImage,
  MergePart,
  HtmlFontRequest, HtmlFont, HtmlResource, HtmlResourceNeed, HtmlRenderOptions,
  HtmlMargins, SignP12Options, SignTsaOptions, SignLtvOptions,
  // unified editable model:
  GigaDocument, GigaSection, GigaPage, GigaBlock, GigaBlockKind, GigaInline,
  GigaCharStyle, GigaParagraphStyle, GigaGeneric, GigaBlockAddr, GigaStylePatch,
  GigaParaPatch, GigaCellValue, GigaOutlineNode, ModelOp,
} from "@qrcommunication/gigapdf-lib";
```

The three signing HTTP helpers are also exported (as runtime functions, for use
as `SignTsaOptions.tsaFetch` / `SignLtvOptions.revocationFetch` / `crlFetch`
overrides or directly):

```ts
import { defaultTsaPost, defaultOcspPost, defaultCrlGet } from "@qrcommunication/gigapdf-lib";
```

See also: [COOKBOOK.md](COOKBOOK.md) (task-oriented recipes), [USAGE.md](USAGE.md)
(raw buffer ABI), [API.md](API.md) (Rust + WASM ABI), [HTML-CSS.md](HTML-CSS.md)
(HTML→PDF), [INSTALL.md](INSTALL.md).
