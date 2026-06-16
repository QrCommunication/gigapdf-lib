# SDK Reference — `@qrcommunication/gigapdf-lib`

Complete reference for the TypeScript SDK. Exact signatures and defaults ship in
the bundled `.d.ts` (your IDE surfaces them inline); this document explains what
every method *does*, its parameters, return value, and the gotchas.

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
| **Errors** | the engine never throws across the boundary; failures are `false`/`null`/`[]`. (`signP12` is the one exception — it throws a generic error.) |

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
| `officeToPdf(office)` | `Uint8Array` | Convert an Office/ODF file (docx/xlsx/pptx/doc/xls/ppt/odt/ods/odp) to PDF; format auto-detected by magic bytes. |
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
| `htmlNeededFonts(html)` / `htmlNeededFontsWith(html, header?, footer?)` | `HtmlFontRequest[]` | Which Google Fonts the HTML needs (fetch them, then pass to `htmlRender`). |
| `evalJs(src)` | `string` | Run JavaScript in the native ES2021 interpreter; returns the result stringified. |
| `runInlineScripts(html)` | `string` | Execute the `<script>`s in an HTML string against a native DOM and return the mutated HTML. |
| `pageSize(name)` | `{ w, h } \| null` | Look up a named page size (`"A4"`, `"Letter"`, …) in points. |

---

## `GigaPdfDoc`

### Lifecycle

| Method | Returns | Description |
|--------|---------|-------------|
| <a id="close"></a>`close()` | `void` | Free the wasm document handle. **Call once.** Using the doc after is undefined; closing twice corrupts the shared heap. |
| `pageCount()` | `number` | Number of pages. |
| `save()` | `Uint8Array` | Serialize to PDF bytes (plain, uncompressed object structure — easiest to grep/debug). |
| `saveCompressed()` | `Uint8Array` | Serialize packing objects into Flate object streams (smaller output). |
| `pageInfo(page)` | `PageInfo` | `{ width, height, rotation }` — MediaBox size (unrotated) and the `/Rotate` flag. |

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
| `appendPages(otherPdf)` | `boolean` | Append every page of another PDF. Powers *merge*. |

### Reading text & content elements

| Method | Returns | Description |
|--------|---------|-------------|
| `textRuns(page)` | `TextRunInfo[]` | Raw content-stream text runs (operator + text), in draw order. |
| `structuredText(page)` | `TextLine[]` | Lines with bounding boxes (`x,y,w,h` + text) — for selection / extraction. |
| `elements(page)` | `Element[]` | All content elements (text/image/path) with kind + bounds — the editor scene graph. |
| `elementAt(page, x, y)` | `number` | Hit-test: index of the element under a point, or `-1`. |
| `search(query, caseInsensitive?)` | `SearchHit[]` | Full-text search with per-hit bounding boxes. |

### Editing existing content

| Method | Returns | Description |
|--------|---------|-------------|
| `replaceText(page, index, text)` | `boolean` | Replace the text of run `index` in place. **Font-aware**: a run in an embedded Type0/Identity-H face (TrueType *or* OpenType-CFF) is re-encoded through that font's char→glyph map; base-14/simple fonts use WinAnsi — so it works with **any** font. |
| `removeElement(page, index)` | `boolean` | Delete a content element. |
| `moveElement(page, index, dx, dy)` | `boolean` | Translate an element by `(dx, dy)` points. |
| `duplicateElement(page, index)` | `boolean` | Clone an element. |

### Drawing new content

| Method | Returns | Description |
|--------|---------|-------------|
| `addText(page, x, y, size, text, fontObj, rgb?, opacity?, rotationDeg?)` | `boolean` | Draw selectable text in **any embedded** font (`fontObj` from `embedFont`/`extractFont`) — glyf TrueType or OpenType-CFF, each character encoded through the font's char→glyph map (Identity-H). |
| `addStandardText(page, x, y, size, text, fontName, rgb?, opacity?, rotationDeg?)` | `boolean` | Draw selectable text in a **built-in base-14** font (no embedding). See [Fonts](#fonts). |
| `addWatermark(page, x, y, size, text, rgb?, opacity?, rotationDeg?)` | `boolean` | Standard-Helvetica watermark (thin wrapper over `addStandardText`). |
| `addTextLayer(page, runs)` | `number` | Stamp an invisible (render-mode 3) text layer — e.g. a searchable OCR layer; one content append. Returns runs written. |
| `addImage(page, data, x, y, w, h, opacity?)` | `boolean` | Embed a PNG/JPEG as an image XObject in the box `(x,y,w,h)`. |
| `addRectangle(page, x, y, w, h, stroke?, fill?, lineWidth?)` | `boolean` | Vector rectangle. `stroke`/`fill` are `0xRRGGBB` or `null`. |
| `addEllipse(page, cx, cy, rx, ry, stroke?, fill?, lineWidth?, opacity?)` | `boolean` | Vector ellipse (Bézier). |
| `addPolygon(page, points, close, stroke?, fill?)` | `boolean` | Polyline/polygon from a flat `[x0,y0,x1,y1,…]` list. |
| `addPath(page, svgPath, x, y, stroke?, fill?, lineWidth?)` | `boolean` | Draw an SVG `<path d="…">` at `(x,y)`. |
| `drawLine(page, x1, y1, x2, y2, rgb?, lineWidth?)` | `boolean` | Straight line. |
| `addSvg(page, svg, x, y, w, h)` | `boolean` | Render SVG markup as **native vector paths** fitting its `viewBox` into `(x,y,w,h)`. |
| `redact(page, x, y, w, h, coverRgb?, hasCover?)` | `number` | True redaction: physically delete content intersecting the region; optional opaque cover. Returns ops removed. |

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
| `annotations(page)` | `AnnotationInfo[]` | List markup annotations (subtype + rect). |
| `addHighlight / addUnderline / addStrikeOut(page, x0, y0, x1, y1, rgb?)` | `boolean` | Text-markup annotations over a quad. |
| `addSquare(page, x0, y0, x1, y1, stroke?, fill?)` | `boolean` | Rectangle annotation. |
| `addLineAnnotation(page, x1, y1, x2, y2, rgb?, lineWidth?)` | `boolean` | Line annotation. |
| `addFreeText(page, x0, y0, x1, y1, text, …)` | `boolean` | Free-text (typewriter) annotation. |
| `addTextNote(page, x, y, rgb, meta?)` | `boolean` | Sticky note; `meta = { contents, author, id, date }`. |
| `addInk(page, points, rgb?, lineWidth?)` | `boolean` | Freehand ink path from a flat point list. |
| `addStamp(page, x0, y0, x1, y1, label, rgb?)` | `boolean` | Rubber-stamp annotation. |
| `addMarkupAnnotation(…)` | `boolean` | Generic markup with shared reviewer metadata. |
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
| `flattenForm()` | `number` | Bake all fields into static page content. |

Every created widget gets a real `/AP` appearance stream and the form is flagged
`NeedAppearances`. `FieldStyle = { fontSize, color, border, background, borderWidth }`.

### Links, layers, outline, metadata

| Method | Returns | Description |
|--------|---------|-------------|
| `links(page)` | `LinkInfo[]` | Hyperlinks with `{ x0,y0,x1,y1, kind: "uri"\|"page"\|"unknown", uri?, page? }`. |
| `addUriLink(page, x0, y0, x1, y1, uri)` | `boolean` | External URL link over a rect. |
| `addGotoLink(page, x0, y0, x1, y1, targetPage)` | `boolean` | Internal "jump to page" link (explicit page reference). |
| `addNamedDest(name, targetPage)` | `boolean` | Register a named destination `name` → page (a `/Fit` view) in the catalog `/Dests`. Resolves through the catalog, so it survives split/extract while its page is kept. |
| `namedDests()` | `NamedDest[]` | The catalog's named destinations as `{ name, page }` pairs. |
| `addGotoLinkNamed(page, x0, y0, x1, y1, name)` | `boolean` | Internal link that jumps to a **named** destination (`/Dest /name`) — the retargetable, split-safe alternative to `addGotoLink`. |
| `layers()` | `LayerInfo[]` | Optional-content groups (calques) `{ id, name, visible, locked }`. |
| `addLayer(name)` | `number` | Create a layer; returns its id (`0` on error). |
| `setLayerVisibility(id, visible)` / `setLayerLocked(id, locked)` | `boolean` | Toggle a layer. |
| `removeLayer(id)` | `boolean` | Delete a layer. |
| `outline()` | `OutlineEntry[]` | The bookmark tree (`{ title, page, level }`). |
| `setOutline(entries)` | `boolean` | Replace the bookmark tree. |
| `getMetadata(key)` / `setMetadata(key, value)` | `string` / `boolean` | Read/write an Info-dictionary entry (`Title`, `Author`, …). |

### Conversions (PDF → X)

Each returns the target file as bytes (or a string for `toText`/`toHtml`). These
produce **real editable elements** (positioned text boxes, re-embedded images,
reconstructed tables for spreadsheets) — not a rasterised image.

| Method | Output |
|--------|--------|
| `toText()` | plain text (`string`) |
| `toHtml()` | HTML (`string`) |
| `toDocx()` / `toOdt()` | Word / OpenDocument Text |
| `toPptx()` / `toOdp()` | PowerPoint / OpenDocument Presentation |
| `toXlsx()` / `toOds()` | Excel / OpenDocument Spreadsheet |
| `toRtf()` | Rich Text Format |
| `toPdfA()` | PDF/A-2b archival PDF |

### Render

| Method | Returns | Description |
|--------|---------|-------------|
| `renderPage(page, scale?)` | `Uint8Array` | Rasterise a page to PNG at `scale` (1 = 72 dpi). Native rasteriser (glyphs, images, vectors, SVG, colour emoji). |
| `rgbaToPng(rgba, width, height)` | `Uint8Array` | *(engine-level)* Encode raw RGBA pixels (`width*height*4`, row-major, non-premultiplied) to PNG with the native encoder — no `canvas`/image library. Empty on a length mismatch. |
| `resizeRgba(rgba, sw, sh, dw, dh)` | `Uint8Array` | *(engine-level)* Resample raw RGBA `sw`×`sh` → `dw`×`dh` with the native alpha-correct resampler (triangle kernel, footprint scaled for down/up) — no `sharp`/image library. Empty on a bad input. |

### OCR & text intelligence

| Method | Returns | Description |
|--------|---------|-------------|
| `ocr(page, scale?)` | `OcrWord[]` | Recognise words (with boxes) on a scanned page — native CNN, no external engine. |
| `ocrText(page, scale?)` | `string` | OCR'd plain text. |

To make a scanned PDF searchable: `ocr(page)` → map words to placements →
`addTextLayer(page, runs)` (invisible, selectable).

### Security

| Method | Returns | Description |
|--------|---------|-------------|
| `saveEncrypted(password, fileId, opts?)` | `Uint8Array` | Encrypt (RC4-128 / AES-128 / AES-256); `opts = { ownerPassword, permissions, keySeed }`. The host supplies the file id and key seed (the wasm has no RNG). |
| `sign(fields, random, keyBits?)` | `Uint8Array` | Self-signed `adbe.pkcs7.detached` signature (an ephemeral digital ID); `fields = "name\treason\tdate\tnotBefore\tnotAfter"`, `random` ≥ 256 host bytes. |
| `signP12(p12, password, opts?)` | `Uint8Array` | Sign with a **user PKCS#12** identity (CA/eIDAS cert + RSA key), imported natively. `opts = { name, reason, date, location, contactInfo }`. **Throws** a generic error on a bad password/file/cipher (anti-enumeration). |

`signP12` imports PBES2 (PBKDF2 + AES) and PBES1 (3DES, RC2-40) bags and verifies
the integrity MAC — entirely in-engine, no node-forge / @signpdf / pdf-lib.

---

## Types

All result/option shapes are exported interfaces — import them for typed code:

```ts
import type {
  FontInfo, EmbeddedFont, PageInfo, TextLine, TextRunInfo, Element,
  SearchHit, OcrWord, AnnotationInfo, FieldInfo, FieldStyle, LinkInfo,
  LayerInfo, OutlineEntry, NamedDest, XlsxSheet, HtmlFontRequest, HtmlFont,
  SignP12Options,
} from "@qrcommunication/gigapdf-lib";
```

See also: [USAGE.md](USAGE.md) (cookbook), [API.md](API.md) (Rust + WASM ABI),
[HTML-CSS.md](HTML-CSS.md) (HTML→PDF), [INSTALL.md](INSTALL.md).
