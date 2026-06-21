# @qrcommunication/gigapdf-lib

TypeScript SDK for **gigapdf-lib** — a zero-dependency Rust→WASM PDF engine.
Read, edit, render, OCR, search, sign, encrypt, fill forms, annotate, and convert
PDFs ↔ Office/HTML/RTF. The engine `.wasm` is self-contained: **no third-party
runtime dependencies**.

## Install

```bash
pnpm add @qrcommunication/gigapdf-lib
# or: npm i @qrcommunication/gigapdf-lib
```

## Quick start

```ts
import { GigaPdfEngine } from "@qrcommunication/gigapdf-lib";

// Node: loads the bundled gigapdf.wasm from the package.
const giga = await GigaPdfEngine.loadDefault();

// Browser / explicit control: pass a URL, Response, or bytes.
// const giga = await GigaPdfEngine.load("/gigapdf.wasm");

const doc = giga.open(pdfBytes); // Uint8Array

// Read
const lines = doc.structuredText(1); // [{ text, x, y, w, h }]
const hits = doc.search("invoice"); // [{ page, text, x, y, w, h }]
const words = doc.ocr(1, 2); // OCR a scanned page at 2× scale

// Edit (operates on the real content stream — no cosmetic overlay)
doc.replaceText(1, 0, "New value");
doc.redact(1, 72, 700, 180, 14); // physically remove content in a region
doc.addHighlight(1, 72, 690, 252, 704, 0xffff00);

// Convert
const docx = doc.toDocx(); // also: toPptx/toOdp/toOdt/toXlsx/toOds/toHtml/toText/toRtf/toPdfA
const png = doc.renderPage(1, 2); // rasterize a page

// Save
const out = doc.save(); // or doc.saveCompressed()
doc.close();
```

### Office / HTML / RTF → PDF

```ts
const fromDocx = giga.officeToPdf(officeBytes); // docx/xlsx/pptx/odt/ods/odp auto-detected
const fromHtml = giga.htmlToPdf("<h1>Hello</h1>");
const fromRtf = giga.rtfToPdf(rtfString);
```

### Build an interactive form (no `pdf-lib`)

```ts
// Coordinates are PDF user space (origin bottom-left): [x0, y0, x1, y1].
doc.addTextField(1, "fullname", [50, 700, 300, 720], "", { maxLen: 60 });
doc.addCheckbox(1, "subscribe", [50, 670, 64, 684], true, { export: "Yes" });
doc.addRadioGroup(1, "plan", [
  { export: "Basic", rect: [50, 640, 64, 654] },
  { export: "Pro", rect: [80, 640, 94, 654] },
], { selected: "Pro" });
doc.addComboBox(1, "country", [50, 610, 200, 626], ["FR", "US", "DE"], { selected: "FR" });
doc.addListBox(1, "langs", [50, 540, 200, 600], ["en", "fr", "de"], { multi: true });

// Optional per-field styling.
doc.addTextField(1, "vat", [50, 510, 200, 528], "", {
  style: { fontSize: 11, color: 0x102030, border: 0x888888, background: 0xf5f5f5 },
});

const filled = doc.fields(); // read them straight back: kind + value + options
```

### Fonts — three sources, no host font files required

```ts
// 1. Base-14 standard fonts — no embedding, no network.
doc.addStandardText(1, 72, 720, 24, "Heading", "Helvetica-Bold", 0x111111);
doc.addStandardText(1, 72, 690, 12, "body in Times", "Times-Roman");

// 2. Any family / Google Font — the host fetches, the engine embeds. embedFont
//    accepts any outline file: glyf .ttf OR OpenType-CFF .otf (auto-detected).
const url = giga.fontRequestUrl("Roboto", 400); // Google Fonts CSS2 URL
const css = await (await fetch(url, { headers: { "User-Agent": "Mozilla/4.0" } })).text();
const ttf = new Uint8Array(await (await fetch(giga.parseCssFontUrl(css))).arrayBuffer());
const fontObj = doc.embedFont("Roboto", ttf);
doc.addText(1, 72, 660, 18, "Selectable embedded text", fontObj, 0x111111);
// Font-aware editing: replace the run's text — re-encoded through Roboto's cmap.
doc.replaceText(1, doc.textRuns(1).length - 1, "Edited in the same font");

// 3. Reuse a face the PDF already embeds: list → extract → re-embed → draw.
const face = doc.embeddedFonts().find((f) => f.format === "truetype");
if (face) {
  const prog = doc.extractFont(face.baseFont)!;        // { format, bytes }
  const reused = doc.embedFont("Reused", prog.bytes);
  doc.addText(1, 72, 630, 14, "drawn in the document's own font", reused);
}
```

## Recipes

Task-oriented snippets using the high-level classes. Each assumes
`const giga = await GigaPdfEngine.loadDefault()` (Node) or
`GigaPdfEngine.load(url)` (browser), `Uint8Array` in/out, and a final `close()`.

### Merge several PDFs

```ts
const merged = giga.mergePdfs([first, second, third]); // one PDF, pages in order
```

Or, for finer control, append page-by-page yourself:

```ts
const doc = giga.open(first);                 // the base document
for (const extra of [second, third]) doc.appendPages(extra); // append every page
const merged = doc.saveCompressed();
doc.close();
```

### Image → PDF

```ts
const pdf = giga.imageToPdf(imageBytes); // PNG/JPEG/GIF/WebP/AVIF → one A4 page
// (image centred, shrink-to-fit, never upscaled; empty array if not an image)
```

### Split — extract a page range into a new PDF

```ts
const doc = giga.open(pdfBytes);
const partA = doc.extractPages([1, 2, 3]);    // a NEW, self-contained PDF…
const partB = doc.extractPages([4, 5, 6]);    // …links / fields / dests to dropped pages pruned
doc.close();
```

### Encrypt (AES-256) and re-open

```ts
// `fileId` is the document /ID (any stable string). AES-256 auto-generates the
// 32-byte key via Web Crypto; pass `opts.keySeed` to supply your own.
const locked = doc.saveEncrypted("user-pw", "doc-001", {
  ownerPassword: "owner-pw",
  algorithm: "aes256",           // "rc4" | "aes128" | "aes256" (default)
  // permissions: -44,           // PDF permission bitmask (optional)
});
doc.close();

const reopened = giga.openEncrypted(locked, "user-pw"); // null on a wrong password
reopened?.close();
// Inspect without opening:
const info = giga.encryptionInfo(locked); // { encrypted, permissions, version, revision }
```

### Digital signature

```ts
// (a) Self-signed — an ephemeral digital ID. `random` ≥ 256 host-entropy bytes.
//     fields = "name\treason\tdate\tnotBefore\tnotAfter" (PDF date strings).
const fields = "Jane Doe\tApproved\tD:20260618120000Z\t260618000000Z\t360618000000Z";
const random = crypto.getRandomValues(new Uint8Array(256));
const signed = doc.sign(fields, random);

// (b) PKCS#12 — your CA / eIDAS certificate + RSA key (.p12/.pfx), imported
//     natively. Throws on a wrong password / malformed file.
const signed2 = doc.signP12(p12Bytes, "p12-password", {
  name: "Jane Doe",
  reason: "I approve this document",
  date: "D:20260618120000Z",
  location: "Paris",
});
doc.close();
```

### Annotate, then flatten

```ts
doc.addHighlight(1, 72, 690, 252, 704, 0xffff00);
doc.addTextNote(1, 300, 700, 0xff0000, { contents: "Check this clause", author: "Reviewer" });
doc.addSquare(1, 60, 680, 264, 712, 0xff0000, null);   // stroke red, no fill
const all = doc.annotations(1);                         // read back, with full metadata
doc.flattenAnnotations(1);                              // bake into page content (non-interactive)
const out = doc.save();
doc.close();
```

### HTML + CSS → PDF with auto-fetched Google Fonts

```ts
import type { HtmlFontRequest, HtmlFont } from "@qrcommunication/gigapdf-lib";

// Phase 1: ask the engine which fonts the HTML needs; the HOST fetches them
// (the wasm has no network). Phase 2: render with those fonts embedded.
async function fetchFonts(reqs: HtmlFontRequest[]): Promise<HtmlFont[]> {
  return Promise.all(reqs.map(async (r) => {
    const css = await (await fetch(r.url, {
      headers: { "User-Agent": "Mozilla/5.0 (Windows NT 10.0)" }, // → TTF, not WOFF2
    })).text();
    const ttf = new Uint8Array(await (await fetch(giga.parseCssFontUrl(css))).arrayBuffer());
    return { family: r.family, weight: r.weight, italic: r.italic, ttf };
  }));
}

const html = `<body style="font-family: Roboto"><h1>Invoice</h1><p>Net 30.</p></body>`;
const fonts = await fetchFonts(giga.htmlNeededFonts(html));
const pdf = giga.htmlRender(html, fonts, 595, 842, 36);  // A4 in points, 36pt margin
// Named sizes, per-side margins and a header/footer with {{page}}/{{pages}} tokens:
// giga.htmlRenderWith(html, fonts, { pageSize: "A4", header, footer });
```

### Make a scanned PDF searchable (OCR → invisible text layer)

```ts
const doc = giga.open(scannedPdf);
const scale = 2;                                  // rasterise at 2× = 144 dpi for OCR
for (let page = 1; page <= doc.pageCount(); page++) {
  const { height } = doc.pageInfo(page);
  const words = doc.ocr(page, scale);             // OcrWord[] in raster pixels (top-left)
  // Map each word box back to PDF user space (bottom-left, Y up) and stamp an
  // invisible (render-mode 3) text layer — selectable & searchable, pixel-aligned.
  doc.addTextLayer(page, words.map((w) => ({
    x: w.x / scale,
    y: height - (w.y + w.h) / scale,
    size: w.h / scale,
    text: w.text,
  })));
}
const searchable = doc.save();
doc.close();
```

### Metadata & bookmarks (outline)

```ts
doc.setMetadata("Title", "Quarterly report");
doc.setMetadata("Author", "Finance");
doc.setOutline([
  { title: "Summary", level: 0, page: 1 },
  { title: "Details", level: 0, page: 2 },
  { title: "Appendix", level: 1, page: 5 },
]);
const out = doc.save();
doc.close();
```

## Next.js (`output: "standalone"`)

`loadDefault()` reads `gigapdf.wasm` from the package directory. In standalone
output, add it to the route's `outputFileTracingIncludes` so it is copied into
the bundle:

```ts
// next.config.ts
outputFileTracingIncludes: {
  "/api/pdf/*": ["./node_modules/@qrcommunication/gigapdf-lib/gigapdf.wasm"],
}
```

Or call `GigaPdfEngine.load(bytes)` with bytes you read yourself.

## API surface

> **Full, per-method reference:** [`docs/SDK.md`](https://github.com/QrCommunication/gigapdf-lib/blob/main/docs/SDK.md)
> documents every method (parameters, return, notes) grouped by domain. Exact
> signatures and defaults also ship in the bundled `.d.ts`.

- **`GigaPdfEngine`** — `load`/`loadDefault`, `open`/`openEncrypted`,
  `txtToPdf`/`htmlToPdf`/`rtfToPdf`/`officeToPdf`/`imageToPdf` (PNG/JPEG/GIF/WebP/AVIF → A4 page),
  `mergePdfs` (concatenate many PDFs), `fontCatalog`/`fontRequestUrl`/`parseCssFontUrl`.
- **`GigaPdfDoc`** — text intelligence (`textRuns`, `structuredText`, `search`,
  `ocr`, `ocrText`, `elements`, `elementAt`), editing (`replaceText`,
  `removeElement`, `moveElement`, `transformElement` (full affine — move + resize
  + rotate in place), `reorderElement` (native z-order — bring to front / send to
  back), `setPathStyle` (in-place vector restyle: fill/stroke/width/dash + **real
  opacity**), `setElementOpacity` (constant opacity on any element — text/image/shape),
  `duplicateElement`, `redact`), vector drawing
  (`addRectangle`, `drawLine`, `addEllipse`, `addPolygon`, `addPath` (SVG path),
  `addImage` (PNG/JPEG, alpha + opacity)), pages (`rotatePage`, `deletePage`,
  `movePage`, `appendPages`, `extractPages`, `resizePage`, `addPage`, `copyPage`,
  `pageInfo`),
  `renderPage` (and `renderPageNoText` — a text-free background for editors that
  overlay real editable text; vectors/gradients/images/annotations still rendered —
  plus `renderPageExcluding` — a background omitting specific elements for
  live-overlay editing),
  fonts (base-14 `addStandardText`, embed **any** TrueType/OpenType
  via `embedFont`/`addText`, font-aware editing `replaceText`,
  the document's own faces `embeddedFonts`/`extractFont`, `neededFonts`),
  conversions (`toText/Html/Docx/Pptx/Odp/Odt/Xlsx/Ods/Rtf/PdfA`, plus
  engine-level `gridsToXlsx`/`gridsToOds` to emit Office output from a
  host-built table grid), security
  (`saveEncrypted`, self-signed `sign`, **PKCS#12** `signP12`), metadata
  (`getMetadata`, `setMetadata`), embedded files (`attachments` — extract every
  `/EmbeddedFiles` entry with its decoded bytes), annotations (`addSquare`,
  `addHighlight`, `addLineAnnotation`, `addFreeText`, `addUnderline`,
  `addStrikeOut`, `addInk`, `addStamp`, `annotations`, `removeAnnotation`,
  `flattenAnnotations`), links (`links`, `addUriLink`, `addGotoLink`, named
  destinations `addNamedDest`/`namedDests`/`addGotoLinkNamed`), outline
  (`outline`, `setOutline`), forms — read/fill (`fields`, `setTextField`,
  `setCheckbox`, `setRadio`, `setChoice`) **and create**
  (`addTextField`, `addCheckbox`, `addRadioGroup`, `addComboBox`, `addListBox`,
  each with an optional `FieldStyle`), and optional-content layers (`layers`,
  `addLayer`, `setLayerVisibility`, `setLayerLocked`, `removeLayer`).
- **HTML rendering engine** (on `GigaPdfEngine`) — `htmlNeededFonts(html)`
  returns the Google fonts to download (phase 1); `htmlRender(html, fonts,
  pageW?, pageH?, margin?)` renders HTML + CSS to PDF with those fonts embedded
  (phase 2). **No headless browser.** Block / inline / table / **flex**
  (`flex-direction`, `justify-content`, `flex-grow`) / **grid**
  (`grid-template-columns`) layout, selector cascade, pagination, and forced page
  breaks via CSS `page-break-before|after: always` / `break-*: page` or a
  `<pagebreak>` tag. **See the exhaustive list of supported HTML elements, CSS
  properties, units, colours and selectors in
  [`docs/HTML-CSS.md`](https://github.com/QrCommunication/gigapdf-lib/blob/main/docs/HTML-CSS.md).**
- **JavaScript** — a document's inline `<script>`s run **before layout** through
  a built-in zero-dependency JS engine (no Chromium/Playwright), so script-driven
  content is rendered. It covers classes + `super`, closures, destructuring,
  `RegExp`, `Map`/`Set`, `Symbol`, `eval`/`Function`, and DOM bindings
  (`document.getElementById`, `querySelector(All)` with `>`/`+`/`~`/`[attr]`,
  `textContent`, `innerHTML`, `createElement`/`appendChild`, `classList`,
  `style`). `function*`/`async` bodies compile to a **suspendable bytecode VM**,
  so generators are **truly lazy** (infinite generators, bidirectional
  `.next(v)`, `yield*`) and `await` **yields** with spec microtask ordering. This
  happens automatically inside `htmlRender`/`htmlNeededFonts` — no extra call
  needed.

Every method is fully typed. Always `close()` a document when done to free the
WASM handle.

## License

**PolyForm Noncommercial License 1.0.0** © Rony Licha / QR Communication.
Free for noncommercial use; a commercial license is required otherwise. See
[`LICENSE`](LICENSE).
