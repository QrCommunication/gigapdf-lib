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
  `txtToPdf`/`htmlToPdf`/`rtfToPdf`/`officeToPdf`, `fontCatalog`/`fontRequestUrl`/`parseCssFontUrl`.
- **`GigaPdfDoc`** — text intelligence (`textRuns`, `structuredText`, `search`,
  `ocr`, `ocrText`, `elements`, `elementAt`), editing (`replaceText`,
  `removeElement`, `moveElement`, `duplicateElement`, `redact`), vector drawing
  (`addRectangle`, `drawLine`, `addEllipse`, `addPolygon`, `addPath` (SVG path),
  `addImage` (PNG/JPEG, alpha + opacity)), pages (`rotatePage`, `deletePage`,
  `movePage`, `appendPages`, `extractPages`, `resizePage`, `addPage`, `copyPage`,
  `pageInfo`),
  `renderPage`, fonts (base-14 `addStandardText`, embed **any** TrueType/OpenType
  via `embedFont`/`addText`, font-aware editing `replaceText`,
  the document's own faces `embeddedFonts`/`extractFont`, `neededFonts`),
  conversions (`toText/Html/Docx/Pptx/Odp/Odt/Xlsx/Ods/Rtf/PdfA`, plus
  engine-level `gridsToXlsx`/`gridsToOds` to emit Office output from a
  host-built table grid), security
  (`saveEncrypted`, self-signed `sign`, **PKCS#12** `signP12`), metadata
  (`getMetadata`, `setMetadata`), annotations (`addSquare`,
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
