# Cookbook — `@qrcommunication/gigapdf-lib`

Task-oriented, runnable recipes for the TypeScript SDK. Each one is a short,
copy-pasteable snippet built on the high-level [`GigaPdfEngine`](SDK.md#gigapdfengine) /
[`GigaPdfDoc`](SDK.md#gigapdfdoc) classes — see [`SDK.md`](SDK.md) for the
per-method reference and [`USAGE.md`](USAGE.md) for the raw `extern "C"` ABI.

Every recipe assumes this preamble:

```ts
import { GigaPdfEngine } from "@qrcommunication/gigapdf-lib";

const giga = await GigaPdfEngine.loadDefault(); // Node: reads the bundled .wasm
// Browser: const giga = await GigaPdfEngine.load("/gigapdf.wasm");
```

Conventions (full table in [`SDK.md` § Conventions](SDK.md#conventions)):

- **Pages** are 1-based.
- **Coordinates** are PDF user space, points (1/72"), origin **bottom-left**, Y up.
- **Colours** are packed `0xRRGGBB` integers.
- **Bytes** are `Uint8Array` in and out.
- Edit methods return `true`/`false`; readers return `[]`/`null` on failure
  (`signP12` is the one method that throws).
- Always `doc.close()` when done to free the wasm handle.

---

## Contents

- [Redact a sensitive zone (PII)](#redact-a-sensitive-zone-pii) — *v0.52.4*
- [Styled text: bold · underline · strikethrough · sub/superscript](#styled-text)
- [Read & write running headers and footers](#headers-and-footers)
- [Convert PDF ↔ Office / HTML / RTF](#convert-pdf--office--html--rtf)
- [OCR a scanned page + full-text search](#ocr-a-scanned-page--full-text-search)
- [Fill and create form fields](#fill-and-create-form-fields)
- [Annotate (highlight, note, ink, stamp)](#annotate)
- [Sign with a PKCS#12 identity](#sign-with-a-pkcs12-identity)
- [Encrypt with AES-256](#encrypt-with-aes-256)
- [Round-trip the unified editable model](#round-trip-the-unified-editable-model)

---

## Redact a sensitive zone (PII)

> **Available in v0.52.4.** Until then, use [`redact()`](#note-redact-vs-redactpii)
> for stream-level content removal.

`redactPii(page, rects, opts?)` performs **true, irreversible redaction** of one
or more rectangles. Unlike a black rectangle painted on top of the content, it:

1. **physically removes the text** operators that fall in the region (nothing to
   copy-paste or extract afterwards);
2. **overwrites the pixels of any image** intersecting the region — this is what
   makes it safe on **scanned / OCR'd documents**, where the sensitive data is
   baked into a raster, not live text; and
3. draws an **opaque black box** over the area as the visible redaction mark.

The result cannot be recovered by selecting, copying, extracting text, or
pulling the image back out.

```ts
const doc = giga.open(pdfBytes);

// Redact a name and an account number on page 1 (rects in PDF user space:
// [x, y, w, h], origin bottom-left).
doc.redactPii(1, [
  { x: 72, y: 690, w: 180, h: 14 },   // the customer name
  { x: 72, y: 660, w: 220, h: 14 },   // the IBAN
]);

const redacted = doc.save();
doc.close();
```

<a id="note-redact-vs-redactpii"></a>

> ### Security note — `redactPii` vs `redact`
>
> | Method | Removes text | Erases image pixels | Visible mark |
> |--------|:---:|:---:|:---:|
> | [`redactPii(page, rects, opts?)`](#redact-a-sensitive-zone-pii) *(v0.52.4)* | ✅ | ✅ (safe on scans/OCR) | opaque black box |
> | [`redact(page, x, y, w, h, coverRgb?, hasCover?)`](SDK.md#editing-existing-content) | ✅ | ❌ (image left intact) | optional cover |
>
> A **black rectangle drawn over** content (e.g. `addRectangle` with a fill) is
> **not** redaction — the data underneath is still in the file. For genuinely
> sensitive data on a page that contains a scan or screenshot, use `redactPii`,
> which is the only method that also destroys the underlying pixels.

---

## Styled text

Both text-drawing methods accept an optional `opts` argument to bake **text
decorations** into the run — `underline` and/or `strikethrough`. The rules are
filled in the text colour, span the run's real glyph advance, and follow the
`rotationDeg` rotation. Omitting `opts` is fully backward-compatible (no
decoration).

```ts
const doc = giga.open(pdfBytes);

// Base-14 font, no embedding needed — bold heading, then an underlined note.
doc.addStandardText(1, 72, 720, 24, "Quarterly report", "Helvetica-Bold");
doc.addStandardText(1, 72, 696, 12, "Confidential", "Helvetica", 0xcc0000, 1, 0, {
  underline: true,
});

// Strike through a superseded line.
doc.addStandardText(1, 72, 672, 12, "Old price: 49.00", "Helvetica", 0x666666, 1, 0, {
  strikethrough: true,
});

const out = doc.save();
doc.close();
```

To draw in an embedded font instead, use `addText(page, x, y, size, text,
fontObj, rgb?, opacity?, rotationDeg?, opts?)` with the `fontObj` from
[`embedFont`](#convert-pdf--office--html--rtf):

```ts
const fontObj = doc.embedFont("Roboto", ttf);
doc.addText(1, 72, 648, 14, "Underlined, in Roboto", fontObj, 0x111111, 1, 0, {
  underline: true,
});
```

### Subscript & superscript

Sub/superscript is expressed through the **unified model's** character style
(`valign: "super" | "sub" | "baseline"`), so it round-trips into Office/HTML/PDF.
Set it on a run with a [`restyleRun`](#round-trip-the-unified-editable-model) op,
or build a run whose `style.valign` is `"super"`/`"sub"`. For example, raising
the model to DOCX/PDF keeps the offset baseline:

```ts
// In a GigaDocument, a run carrying `style.valign = "super"` renders raised
// (e.g. the "2" in "m²", a footnote marker). See the model recipe below for the
// full lower → edit → raise flow.
const model = doc.toModel();
const edited = giga.applyModelOps(model, [
  { op: "restyleRun", addr: [0, 0, 3], run: 1, style: { /* size_pt: 7 */ } },
]);
```

> Decorations (`underline`/`strike`) are also first-class fields on the model's
> `GigaCharStyle`, so the same styling survives a PDF → model → DOCX round-trip,
> not just a freshly-drawn run.

---

## Headers and footers

Bake a **running header/footer** onto every page of an existing PDF, with
`{{page}}` / `{{pages}}` tokens substituted per page. Re-baking replaces the
previous one (idempotent), and a reader counterpart recovers what's already
there — handy for a Word-like editor toggle.

```ts
const doc = giga.open(pdfBytes);

// Write a centred header and a right-aligned page-number footer.
doc.setHeader({ text: "Acme Inc. — Confidential", align: "center", fontSize: 10 });
doc.setFooter({
  text: "Page {{page}} / {{pages}}",
  align: "right",
  color: [0.4, 0.4, 0.4],
});

// Read what's baked in (the reader side):
const { header, footer } = doc.headerFooter();
//   → { header: { text: "Acme Inc. — Confidential", … } | null,
//       footer: { text: "Page 1 / 12", … } | null }

// Remove them again:
// doc.removeHeaders();
// doc.removeFooters();

const out = doc.save();
doc.close();
```

`HeaderFooterSpec` also accepts `pageRange: [first, last]` (omit for every page),
`showOnFirstPage`, and `bandHeight` (the band from the page edge, points). The
text is drawn in standard Helvetica, so no font embedding is required.

> `headerFooter()` returns the faithful, per-page-substituted `text`; `align`,
> `fontSize`, `color`, etc. are best-effort defaults, since the bake records only
> the drawn text.

> Building a PDF *from HTML* instead? `htmlRenderWith` paints a running
> header/footer in the page margins from HTML fragments — see
> [Convert PDF ↔ Office / HTML / RTF](#convert-pdf--office--html--rtf) and
> [`HTML-CSS.md` §1](HTML-CSS.md#1-page-setup).

---

## Convert PDF ↔ Office / HTML / RTF

The conversions produce **real editable objects** (positioned text boxes,
re-embedded images, reconstructed table cells), not a page image.

### PDF → Office / HTML / RTF / text

```ts
const doc = giga.open(pdfBytes);

const docx = doc.toDocx();   // editable Word        (also: toOdt — OpenDocument Text)
const pptx = doc.toPptx();   // one slide per page    (also: toOdp)
const xlsx = doc.toXlsx();   // tables → cells        (also: toOds)
const rtf  = doc.toRtf();    // Rich Text Format (bytes)
const html = doc.toHtml();   // positioned HTML (string)
const text = doc.toText();   // plain text (string)
const pdfa = doc.toPdfA();   // PDF/A-2b archival PDF

doc.close();
```

### Office / HTML / RTF → PDF

`officeToPdf` auto-detects DOCX/XLSX/PPTX, the legacy OLE2 (`.doc`/`.xls`/`.ppt`)
and ODF (`.odt`/`.ods`/`.odp`) by magic bytes:

```ts
const fromOffice = giga.officeToPdf(officeBytes); // any of the formats above
const fromRtf    = giga.rtfToPdf(rtfString);
const fromText   = giga.txtToPdf("Hello\nWorld");
```

### HTML + CSS → PDF (native engine, no headless browser)

The renderer runs a document's inline `<script>`s before layout and needs the
host to fetch Google fonts (the wasm has no network). Phase 1 lists the fonts;
phase 2 renders with them embedded:

```ts
import type { HtmlFontRequest, HtmlFont } from "@qrcommunication/gigapdf-lib";

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
const header = `<div style="text-align:center;color:#888">Acme Inc.</div>`;
const footer = `<div style="text-align:right">Page {{page}} / {{pages}}</div>`;

// `htmlNeededFontsWith` also scans the header/footer fonts.
const fonts = await fetchFonts(giga.htmlNeededFontsWith(html, header, footer));
const pdf = giga.htmlRenderWith(html, fonts, {
  pageSize: "A4",                                          // or pageWidth/pageHeight
  margin: { top: 72, bottom: 72, left: 54, right: 54 },    // or a single number
  header,
  footer,                                                  // {{page}} / {{pages}} per page
});
```

For external `<img src="https://…">` images, list everything to fetch in one
pass with `htmlNeededResources(html, header?, footer?)`, fetch each, and hand the
image bytes back via `HtmlRenderOptions.resources` (the engine never touches the
network). `data:` image URIs need no entry. Full element/property/selector list:
[`HTML-CSS.md`](HTML-CSS.md).

---

## OCR a scanned page + full-text search

For pages that **already carry a text layer**, `structuredText` / `search` are
exact and fast. For **scanned, image-only** pages, OCR them and stamp an
invisible (render-mode 3) text layer so the result becomes selectable and
searchable.

```ts
const doc = giga.open(scannedPdf);

// (Node) load every bundled OCR script so any language is recognised — Latin,
// Cyrillic, Greek, Arabic/Hebrew (RTL), Devanagari, Bengali, Tamil. The script
// detector routes each line to the right model.
await giga.loadAllBundledOcrModels();

const scale = 2; // rasterise at 2× (= 144 dpi) for small text
for (let page = 1; page <= doc.pageCount(); page++) {
  const { height } = doc.pageInfo(page);
  const words = doc.ocr(page, scale); // OcrWord[] — boxes in raster pixels, top-left
  doc.addTextLayer(
    page,
    // Map each word box back to PDF user space (bottom-left, Y up).
    words.map((w) => ({
      x: w.x / scale,
      y: height - (w.y + w.h) / scale,
      size: w.h / scale,
      text: w.text,
    })),
  );
}

// Now the document is searchable, with per-hit boxes.
const hits = doc.search("invoice");          // [{ page, text, x, y, w, h }]
const plain = doc.ocrText(1, scale);         // OCR'd page as a plain string

const searchable = doc.save();
doc.close();
```

> Need only Latin? Skip `loadAllBundledOcrModels()` — the built-in mono-glyph
> classifier recognises printed and handwritten Latin out of the box. Load a
> single script with `giga.loadBundledOcrModel("arabic")` (Node) or
> `giga.loadOcrModel(blobBytes)` (any host) instead.

---

## Fill and create form fields

Read or fill an existing AcroForm, or **build widgets from scratch** (each gets a
real `/AP` appearance stream; the form is flagged `NeedAppearances`). Field rects
are `[x0, y0, x1, y1]` in PDF user space.

```ts
const doc = giga.open(pdfBytes);

// Fill an existing form.
doc.setTextField("fullname", "Jane Doe");
doc.setCheckbox("subscribe", true);
doc.setRadio("plan", "Pro");                 // by export value
doc.setChoice("langs", ["en", "fr"]);        // multi-select list box

// …or create fields where there are none.
doc.addTextField(1, "vat", [50, 700, 250, 718], "", { maxLen: 14 });
doc.addCheckbox(1, "agree", [50, 670, 64, 684], false, { export: "Yes" });
doc.addRadioGroup(1, "tier", [
  { export: "Basic", rect: [50, 640, 64, 654] },
  { export: "Pro",   rect: [80, 640, 94, 654] },
], { selected: "Pro" });
doc.addComboBox(1, "country", [50, 610, 200, 626], ["FR", "US", "DE"], { selected: "FR" });
doc.addListBox(1, "skills", [50, 540, 200, 600], ["a", "b", "c"], { multi: true });

// Optional per-field styling.
doc.addTextField(1, "iban", [50, 510, 250, 528], "", {
  style: { fontSize: 11, color: 0x102030, border: 0x888888, background: 0xf5f5f5 },
});

const fields = doc.fields(); // read them back: name + kind + value + options + bounds
// doc.flattenForm();        // bake every widget into static content (no longer fillable)

const out = doc.save();
doc.close();
```

---

## Annotate

Acrobat-style markup with full reviewer metadata. The engine has no clock, so
pass a PDF date string (e.g. `"D:20260619120000Z"`) where a date is wanted.

```ts
const doc = giga.open(pdfBytes);

doc.addHighlight(1, 72, 690, 252, 704, 0xffff00);
doc.addTextNote(1, [300, 700, 318, 718], 0xff0000, {
  contents: "Check this clause",
  author: "Reviewer",
  date: "D:20260619120000Z",
}, "Comment", false);
doc.addSquare(1, 60, 680, 264, 712, 0xff0000, null); // red stroke, no fill
doc.addInk(1, [80, 600, 120, 620, 160, 590], 0x0000ff, 2); // freehand polyline
doc.addStamp(1, 60, 540, 180, 568, "APPROVED", 0xc00000);

// A wrapped (multi-quad) highlight with metadata:
doc.addMarkupAnnotation(
  1,
  "highlight",
  [[72, 520, 540, 534], [72, 506, 300, 520]], // one [x0,y0,x1,y1] per visual line
  0xffe066,
  0.5,
  { contents: "Important", author: "Reviewer", date: "D:20260619120000Z" },
);

const all = doc.annotations(1); // read back, with author/date/colour/quadPoints/inkList…
// doc.flattenAnnotations(1);   // bake appearances into page content (non-interactive)

const out = doc.save();
doc.close();
```

---

## Sign with a PKCS#12 identity

Sign with a CA-issued / eIDAS certificate and its RSA key from a `.p12`/`.pfx`,
imported natively (no node-forge / @signpdf). `signP12` **throws** a single
generic error on a wrong password, malformed file, unsupported cipher, or missing
certificate.

```ts
const doc = giga.open(pdfBytes);

const signed = doc.signP12(p12Bytes, "p12-password", {
  name: "Jane Doe",
  reason: "I approve this document",
  date: "D:20260619120000Z",   // /M — a PDF date string
  location: "Paris",
  contactInfo: "jane@example.com",
});
doc.close();
// `signed` is the signed PDF bytes.
```

For an **ephemeral, self-signed** digital ID instead (no certificate file), use
`sign(fields, random, keyBits?)` with `fields =
"name\treason\tdate\tnotBefore\tnotAfter"` and ≥ 256 host-entropy bytes:

```ts
const fields = "Jane Doe\tApproved\tD:20260619120000Z\t260619000000Z\t360619000000Z";
const random = crypto.getRandomValues(new Uint8Array(256));
const ephemeral = doc.sign(fields, random);
```

---

## Encrypt with AES-256

`saveEncrypted` defaults to AES-256 (R6). `fileId` is the document `/ID` (any
stable string); the 32-byte file key is auto-generated via Web Crypto unless you
pass `opts.keySeed`.

```ts
const doc = giga.open(pdfBytes);

const locked = doc.saveEncrypted("user-pw", "doc-001", {
  ownerPassword: "owner-pw",
  algorithm: "aes256",   // "rc4" | "aes128" | "aes256" (default)
  // permissions: -44,   // PDF permission bitmask (optional)
});
doc.close();

// Re-open it, or inspect the encryption without a password:
const reopened = giga.openEncrypted(locked, "user-pw"); // null on wrong password
reopened?.close();
const info = giga.encryptionInfo(locked); // { encrypted, permissions, version, revision }
```

---

## Round-trip the unified editable model

The **unified model** ([`GigaDocument`](SDK.md#the-unified-editable-model)) is a
format-neutral tree (sections → pages → blocks → runs). Lower *any* format into
it (`toModel` / `officeToModel` / `htmlToModel`), edit it with structured ops
(`applyModelOps`), then raise it to *any* format (`modelTo{Docx,Xlsx,Pptx,Odt,Ods,Odp,Pdf,Html,Rtf}`).
This is the substrate for editing every format the same way.

```ts
const doc = giga.open(pdfBytes);

// 1. Lower the PDF into the format-neutral model.
const model = doc.toModel();
doc.close();

// 2. Edit it with positional ops. An address is [section, page, blockIndex]
//    (all zero-based); `run` indexes a run inside a paragraph block.
const edited = giga.applyModelOps(model, [
  { op: "setRunText", addr: [0, 0, 0], run: 0, text: "Revised title" },
  { op: "restyleRun", addr: [0, 0, 0], run: 0, style: { bold: true, color: [0.8, 0, 0] } },
  { op: "insertRun",  addr: [0, 0, 2], run: 1, text: " (updated)", style: { italic: true } },
  { op: "setCellText", addr: [0, 0, 5], row: 1, col: 2, text: "42" }, // a table block
]);
// Out-of-range addresses (and unparseable ops) are silently skipped, so a
// partially-valid batch never throws.

// 3. Raise the edited model to whatever you need.
const asDocx = giga.modelToDocx(edited);
const asPdf  = giga.modelToPdf(edited);
const asHtml = giga.modelToHtml(edited); // returns a string
const asXlsx = giga.modelToXlsx(edited);
```

Lower from other sources too:

```ts
const fromOffice = giga.officeToModel(officeBytes); // null if not an Office container
const fromHtml   = giga.htmlToModel("<h1>Hi</h1><p>Body</p>");
// …then applyModelOps + modelTo* exactly as above.
```

The model carries `meta` (title/author/…), `styles`, `outline` and `resources`
opaquely, so a round-trip preserves what your ops don't touch.

---

## See also

- [`SDK.md`](SDK.md) — every `GigaPdfEngine` / `GigaPdfDoc` method, grouped by domain.
- [`USAGE.md`](USAGE.md) — the raw `extern "C"` buffer ABI and host integration.
- [`HTML-CSS.md`](HTML-CSS.md) — exhaustive HTML / CSS / JS support in the HTML→PDF engine.
- [`API.md`](API.md) — the Rust ↔ WASM ABI mapping.
- [`INSTALL.md`](INSTALL.md) — install, build-from-source, Next.js standalone wiring.
