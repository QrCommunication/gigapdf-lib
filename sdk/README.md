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
const docx = doc.toDocx(); // also: toPptx/toOdt/toXlsx/toOds/toHtml/toText/toRtf/toPdfA
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

### Fonts (host performs the network fetch)

```ts
const url = giga.fontRequestUrl("Roboto", 400); // Google Fonts CSS2 URL
const css = await (await fetch(url, { headers: { "User-Agent": "Mozilla/4.0" } })).text();
const ttfUrl = giga.parseCssFontUrl(css); // trusted gstatic URL
const ttf = new Uint8Array(await (await fetch(ttfUrl)).arrayBuffer());
const fontObj = doc.embedFont("Roboto", ttf);
doc.addText(1, 72, 720, 18, "Selectable text", fontObj, 0x111111);
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

- **`GigaPdfEngine`** — `load`/`loadDefault`, `open`/`openEncrypted`,
  `txtToPdf`/`htmlToPdf`/`rtfToPdf`/`officeToPdf`, `fontCatalog`/`fontRequestUrl`/`parseCssFontUrl`.
- **`GigaPdfDoc`** — text intelligence (`textRuns`, `structuredText`, `search`,
  `ocr`, `ocrText`, `elements`, `elementAt`), editing (`replaceText`,
  `removeElement`, `moveElement`, `duplicateElement`, `addRectangle`, `redact`),
  pages (`rotatePage`, `deletePage`, `movePage`, `appendPages`, `extractPages`),
  `renderPage`, fonts (`embedFont`, `addText`, `neededFonts`), conversions
  (`toText/Html/Docx/Pptx/Odt/Xlsx/Ods/Rtf/PdfA`), security (`saveEncrypted`,
  `sign`), metadata (`getMetadata`, `setMetadata`), annotations (`addSquare`,
  `addHighlight`, `addLineAnnotation`, `addFreeText`, `addUnderline`,
  `addStrikeOut`, `addInk`, `addStamp`, `annotations`, `removeAnnotation`,
  `flattenAnnotations`), links (`links`, `addUriLink`, `addGotoLink`), outline
  (`outline`, `setOutline`), and forms (`fields`, `setTextField`, `setCheckbox`,
  `setRadio`, `setChoice`).

Every method is fully typed. Always `close()` a document when done to free the
WASM handle.

## License

**PolyForm Noncommercial License 1.0.0** © Rony Licha / QR Communication.
Free for noncommercial use; a commercial license is required otherwise. See
[`LICENSE`](LICENSE).
