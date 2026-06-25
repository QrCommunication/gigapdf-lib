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
- [Gradient fills (linear & radial)](#gradient-fills-linear--radial) — *v0.82.0*
- [Press-ready colour: CMYK, spot inks, overprint & output intent](#press-ready-colour-cmyk-spot-inks-overprint--output-intent) — *v0.83.0*
- [Read & write running headers and footers](#headers-and-footers)
- [Set print boxes (TrimBox / BleedBox) for prepress](#print-boxes) — *v0.73.0*
- [Number pages with labels (roman front matter, prefixes)](#page-labels) — *v0.74.0*
- [Kiosk slideshow: page transitions & auto-advance (`/Trans` + `/Dur`)](#page-transitions) — *v0.95.0*
- [Embed file attachments (+ Factur-X / ZUGFeRD `/AF`)](#attachments) — *v0.75.0*
- [Bundle invoices as a PDF portfolio (`/Collection`)](#collection-portfolio) — *v0.95.0*
- [Set document metadata (Info + XMP, kept in sync)](#metadata) — *v0.76.0*
- [Convert PDF ↔ Office / HTML / RTF](#convert-pdf--office--html--rtf)
- [Image → PDF (single & batch)](#image--pdf)
- [Stamp an image watermark](#stamp-an-image-watermark) — *v0.69.0*
- [A toggleable "Watermark" layer (optional content / OCG)](#a-toggleable-watermark-layer-optional-content--ocg)
- [Merge multiple PDFs](#merge-multiple-pdfs)
- [2-up booklet & contact sheet (N-up imposition)](#2-up-booklet--contact-sheet-n-up-imposition)
- [Scale page content & large-format pages (UserUnit)](#scale-page-content--large-format-pages-userunit)
- [Compact output (object streams + cross-reference stream)](#compact-output-object-streams--cross-reference-stream) — *v0.81.0*
- [OCR a scanned page + full-text search](#ocr-a-scanned-page--full-text-search)
- [Fill and create form fields](#fill-and-create-form-fields)
- [Annotate (highlight, note, ink, stamp)](#annotate)
- [Navigation: links, bookmarks & open-action](#navigation-links-bookmarks--open-action) — *full action model: v0.78.0*
- [Sign a PDF (B · B-T · LTV · certify · verify)](#sign-a-pdf-b--b-t--ltv) — *B-T: v0.70.0 · LTV: v0.71.0 · verify + DocMDP: v0.80.0*
- [Author a tagged (accessible) PDF](#author-a-tagged-accessible-pdf) — *v0.85.0*
- [Encrypt with AES-256](#encrypt-with-aes-256)
- [Change a password, drop encryption, or encrypt to a certificate](#change-a-password-drop-encryption-or-encrypt-to-a-certificate) — *v0.84.0*
- [Move, resize, restyle, fade & reorder existing elements in place](#move-resize--restyle-existing-elements-in-place) — *opacity & z-order: v0.58.0*
- [Render a page without a specific element (live-overlay editing)](#render-a-page-without-a-specific-element-live-overlay-editing) — *v0.58.0*
- [Round-trip the unified editable model](#round-trip-the-unified-editable-model)
- [Set how the document opens (ViewerPreferences · PageLayout · PageMode)](#set-how-the-document-opens-viewerpreferences--pagelayout--pagemode)

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

// Redact a name and an account number on page 1 (rects in PDF user space,
// origin bottom-left).
doc.redactPii(1, [
  { x: 72, y: 690, width: 180, height: 14 },   // the customer name
  { x: 72, y: 660, width: 220, height: 14 },   // the IBAN
]);
// opts (optional): { cover?: boolean (default true), coverRgb?: number }.
// `cover: false` erases the content/pixels with no visible mark.

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

## Gradient fills (linear & radial)

`addGradient` paints a multi-stop linear or radial gradient over a rectangle — a
shading (type 2/3) rendered through a `PatternType 2` shading pattern, with the
colour stops compiled to a PDF interpolation function. RGB; `rgb` is `0xRRGGBB`.

```ts
const doc = giga.open(pdfBytes);

// Linear: a left-to-right red → green → blue band.
doc.addGradient(1, {
  kind: "linear",
  coords: [72, 700, 522, 700], // [x0, y0, x1, y1] — the gradient axis
  stops: [
    { offset: 0,   rgb: 0xff0000 },
    { offset: 0.5, rgb: 0x00cc00 },
    { offset: 1,   rgb: 0x0000ff },
  ],
  rect: { x: 72, y: 680, w: 450, h: 40 },
});

// Radial: a soft spotlight (inner circle r0=0 → outer circle r1=120).
doc.addGradient(1, {
  kind: "radial",
  coords: [300, 400, 0, 300, 400, 120], // [x0,y0,r0, x1,y1,r1]
  stops: [
    { offset: 0, rgb: 0xffffff },
    { offset: 1, rgb: 0x1133aa },
  ],
  rect: { x: 180, y: 280, w: 240, h: 240 },
  extend: [true, true],
  opacity: 0.9,
});

const out = doc.save();
doc.close();
```

> Tiling patterns, blend-mode authoring (`/BM`) and transparency groups are not
> covered here — only gradient fills.

---

## Press-ready colour: CMYK, spot inks, overprint & output intent

Authoring fills are no longer limited to DeviceRGB. `addFilledRectangle`,
`addFilledPolygon` and `addTextColor` accept a `Color` in any colour space —
`DeviceCMYK`, `DeviceGray`, a spot `Separation` ink, or an embedded ICC profile
(`ICCBased`). `setOverprint` enables prepress trapping, and `addOutputIntent`
attaches the ICC characterisation of the target press.

```ts
const doc = giga.open(pdfBytes);

// A process-CMYK colour block (c, m, y, k are 0…1).
doc.addFilledRectangle(
  1,
  { x: 72, y: 700, w: 200, h: 40 },
  { space: "cmyk", c: 0.1, m: 0.85, y: 0.9, k: 0 }
);

// A spot ink — its tint transform interpolates toward `cmyk` at full tint.
doc.addFilledRectangle(
  1,
  { x: 300, y: 700, w: 200, h: 40 },
  { space: "separation", name: "PANTONE 285 C", tint: 1, cmyk: [0.9, 0.5, 0, 0] }
);

// Black text set to overprint (so it traps over the colour beneath it).
doc.setOverprint(1, /* fill */ true, /* stroke */ false, /* mode */ 1);
doc.addTextColor(1, 72, 660, 18, "Overprinting black", "Helvetica", {
  space: "cmyk", c: 0, m: 0, y: 0, k: 1,
});

// A grey polygon and an ICC-tagged fill.
doc.addFilledPolygon(1, [72, 500, 272, 500, 172, 600], { space: "gray", gray: 0.5 });
doc.addFilledRectangle(1, { x: 72, y: 440, w: 80, h: 40 }, {
  space: "icc", components: [0.2, 0.4, 0.6], profile: iccProfileBytes,
});

// Tell the press which condition the document is prepared for.
doc.addOutputIntent(iccProfileBytes, "Coated FOGRA39");

const out = doc.save();
doc.close();
```

> `rgb` colours are packed `0xRRGGBB`; all other components are `0…1`. Overprint
> only affects content drawn **after** the `setOverprint` call. ICC profile bytes
> are a `Uint8Array` (e.g. read from a `.icc` file).

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

<a id="print-boxes"></a>

## Set print boxes (TrimBox / BleedBox) for prepress

> **Available in v0.73.0.**

A press-ready PDF carries more than a `MediaBox`. The five boxes of ISO 32000-1
§14.11.2 tell the RIP where the **finished page** is (`TrimBox`), how far artwork
**bleeds** past the trim (`BleedBox`), the **visible** area (`CropBox`) and the
**meaningful art** (`ArtBox`). `getPageBoxes` reads all five — already resolving
inheritance and the per-box default chain — and `setPageBox` writes one at a time
without disturbing the others.

Here we take an A4 page (595.28 × 841.89 pt) and add a standard **3 mm bleed**
(8.504 pt) plus a `TrimBox` at the finished A4 size, growing the `MediaBox` so the
bleed has somewhere to live:

```ts
const doc = giga.open(pdfBytes);

const mm = (v: number) => (v * 72) / 25.4; // millimetres → points
const bleed = mm(3); // 3 mm ≈ 8.504 pt
const a4 = { w: 595.28, h: 841.89 };

// Grow the sheet so the bleed is inside the media box, then place the boxes.
doc.setPageBox(1, "media", { x: 0, y: 0, w: a4.w + 2 * bleed, h: a4.h + 2 * bleed });
doc.setPageBox(1, "bleed", { x: 0, y: 0, w: a4.w + 2 * bleed, h: a4.h + 2 * bleed });
doc.setPageBox(1, "trim", { x: bleed, y: bleed, w: a4.w, h: a4.h });

const boxes = doc.getPageBoxes(1);
//   → boxes.trim      = [8.50, 8.50, 603.78, 850.39]
//     boxes.bleed     = [0, 0, 612.29, 858.90]
//     boxes.declared  = { media: true, crop: false, bleed: true, trim: true, art: false }
//   (crop/art were never set → they default to the media box on read)

const out = doc.save(); // TrimBox/BleedBox survive the round-trip
doc.close();
```

**Reading boxes back** — every field of `getPageBoxes` is always a concrete
`[x0, y0, x1, y1]` rectangle: a box the page does not declare is resolved through
the default chain (`CropBox`→`MediaBox`; `BleedBox`/`TrimBox`/`ArtBox`→`CropBox`)
and inheritance (`MediaBox`/`CropBox` may come from an ancestor `/Pages` node). Use
the `declared` flags to tell a *real* `TrimBox` from one defaulted to the crop box.

> `setPageBox` rejects a degenerate rectangle (zero or negative area) and returns
> `false`; reversed corners are accepted (the box is normalised so `x0 < x1`,
> `y0 < y1`). Boxes are written verbatim — they are **not** clamped to their
> intersection with the media box, so what you set is what later tools read.

---

<a id="page-labels"></a>

## Number pages with labels (roman front matter, prefixes)

> **Available in v0.74.0.**

Page labels (`/PageLabels`, ISO 32000-1 §12.4.2) let a document number its pages with
schemes other than `1, 2, 3…` — lowercase roman for front matter, decimal for the
body, a prefixed scheme like `A-1, A-2` for an appendix. Viewers show these in the
page navigator, and they are dropped on a naïve edit, so re-authoring them after a
merge/insert is essential for books, reports and legal documents.

Here we label a report: cover + TOC in lowercase roman (`i, ii, iii`), the body from
page 3 in decimal restarting at 1, and an appendix from page 20 as `A-1, A-2, …`:

```ts
const doc = giga.open(pdfBytes);

doc.setPageLabels([
  { startPage: 1, style: "romanLower", prefix: "", startNumber: 1 }, // i, ii
  { startPage: 3, style: "decimal", prefix: "", startNumber: 1 }, // 1, 2, 3…
  { startPage: 20, style: "decimal", prefix: "A-", startNumber: 1 }, // A-1, A-2…
]);

// Resolve the viewer-visible string for any page:
doc.pageLabel(1); //  "i"
doc.pageLabel(2); //  "ii"
doc.pageLabel(3); //  "1"
doc.pageLabel(20); // "A-1"
doc.pageLabel(21); // "A-2"

// Read the ranges back (sorted by startPage):
const labels = doc.getPageLabels();
//   → [ { startPage: 1,  style: "romanLower", prefix: "",   startNumber: 1 },
//       { startPage: 3,  style: "decimal",    prefix: "",   startNumber: 1 },
//       { startPage: 20, style: "decimal",    prefix: "A-", startNumber: 1 } ]

const out = doc.save(); // labels survive the round-trip
doc.close();
```

The `style` is one of `decimal`, `romanLower`, `romanUpper`, `alphaLower`
(`a…z, aa…zz, aaa…`), `alphaUpper`, or `none` (the `prefix` alone, with no number).
A range runs until the next one begins; `pageLabel(n)` falls back to the decimal page
number for any page before the first range, or when the document has no labels at all.

> Pass an **empty array** to `setPageLabels([])` to strip all page labels (the page
> navigator reverts to `1, 2, 3…`). Setting labels replaces the whole `/PageLabels`
> tree, so include every range you want each time — it is not a merge.

---

<a id="page-transitions"></a>

## Kiosk slideshow: page transitions & auto-advance (`/Trans` + `/Dur`)

> **Available in v0.95.0.**

When a PDF is opened **full-screen** (a presentation, a trade-show kiosk), each
page can carry a visual *transition* (`/Trans`) that plays as the viewer arrives,
and a *display duration* (`/Dur`) that makes the viewer **auto-advance** to the
next page after that many seconds — ISO 32000-1 §12.4.4. The engine produces
`.pptx`/`.odp`, but this authors the PDF-native effect so a plain PDF viewer in
presentation mode runs the slideshow unattended.

Here we turn every page of a deck into a self-running slideshow: a **wipe** from
left to right, half-a-second long, auto-advancing after **5 seconds**:

```ts
const doc = giga.open(deckBytes);

for (let p = 1; p <= doc.pageCount(); p++) {
  doc.setPageTransition(p, {
    style: "wipe",
    duration: 0.5, // /D — the wipe lasts 0.5 s
    direction: 0, // /Di — sweep left → right (0°)
    displayDuration: 5, // /Dur — auto-advance after 5 s on screen
  });
}
// Each page now has /Trans << /S /Wipe /D 0.5 /Di 0 >> and /Dur 5.

const out = doc.save(); // open full-screen → it plays itself
doc.close();
```

Only the sub-keys that apply to the chosen `style` are written, so you cannot
emit a meaningless combination. A few common variants:

```ts
// Vertical split, opening outward from the centre (no /Di — Split ignores it):
doc.setPageTransition(1, { style: "split", dimension: "vertical", motion: "outward" });

// "Fly" the new page straight toward the viewer, 1.5× scale, opaque box:
doc.setPageTransition(2, { style: "fly", direction: "none", scale: 1.5, flyAreaOpaque: true });

// Simple cross-fade, 0.8 s:
doc.setPageTransition(3, { style: "fade", duration: 0.8 });

// Title page that lingers 8 s with no visible effect (just auto-advance):
doc.setPageTransition(4, { style: "replace", displayDuration: 8 });
```

Read a transition back, or remove it entirely:

```ts
doc.getPageTransition(1);
//   → { style: "wipe", duration: 0.5, direction: 0, displayDuration: 5,
//       dimension: null, motion: null, scale: null, flyAreaOpaque: null }

doc.clearPageTransition(1); // drops both /Trans and /Dur from the page
doc.getPageTransition(1); // → null
```

`style` is one of `split`, `blinds`, `box`, `wipe`, `dissolve`, `glitter`,
`fly`, `push`, `cover`, `uncover`, `fade`, `replace`. `dimension` (`split`/
`blinds`) is `horizontal`/`vertical`; `motion` (`split`/`box`) is `inward`/
`outward`; `direction` (`wipe`/`glitter`/`fly`/`cover`/`uncover`/`push`) is one
of `0 90 180 270 315` degrees or `"none"`; `scale`/`flyAreaOpaque` apply to
`fly`. `displayDuration` is the page's auto-advance time and is independent of
the visual effect — set it alone (with `style: "replace"`) for a timed slideshow
with no animation.

> Re-calling `setPageTransition` replaces the page's transition **in full**; omit
> `displayDuration` to remove the `/Dur` auto-advance while keeping (or changing)
> the visual effect.

---

<a id="attachments"></a>

## Embed file attachments (+ Factur-X / ZUGFeRD `/AF`)

> **Available in v0.75.0.** The read side (`attachments()`) shipped earlier.

A PDF can carry **embedded files** in its `/Names /EmbeddedFiles` name tree (ISO
32000-1 §7.11) — the "carry the source inside the PDF" pattern, and the backbone of
hybrid **e-invoices**: Factur-X / ZUGFeRD / Order-X embed a structured XML invoice
inside a human-readable PDF/A-3, linking it through the catalog `/AF` (associated
files) array with an `/AFRelationship` of `Alternative`.

```ts
const doc = giga.open(pdfBytes);
const enc = new TextEncoder();

// A plain attachment (e.g. the source spreadsheet), replaceable by name.
doc.addAttachment("source.csv", enc.encode("a,b\n1,2\n"), {
  mime: "text/csv",
  description: "Source data",
});

// A Factur-X invoice payload as an ASSOCIATED file (PDF/A-3 /AF).
const xml = enc.encode('<?xml version="1.0"?><rsm:CrossIndustryInvoice .../>');
doc.addAssociatedFile("factur-x.xml", xml, "alternative", { mime: "text/xml" });

// Optionally anchor a visible paperclip on page 1 that opens the CSV.
doc.addFileAttachmentAnnot(1, { x: 36, y: 760, w: 16, h: 16 }, "source.csv", "Paperclip");

const out = doc.save();
doc.close();
```

Read them back (this part already existed) with `attachments()`:

```ts
const files = giga.open(out).attachments();
//   → [ { name: "factur-x.xml", mime: "text/xml", data: Uint8Array, … },
//       { name: "source.csv",   mime: "text/csv", data: Uint8Array, … } ]
```

Re-using a `name` in `addAttachment` **replaces** that attachment; `removeAttachment(name)`
drops it (and its `/AF` link), returning `false` if nothing matched. Attachment bytes
are stored FlateDecode-compressed.

> For a **fully conformant** Factur-X / ZUGFeRD file you still need the surrounding
> PDF/A-3 conformance (output intent, XMP with the Factur-X extension schema). This
> recipe provides the embedding + `/AF` linkage — the part the engine owns — so the
> XML travels with the document and is discoverable via `/AF` and the name tree.

---

<a id="collection-portfolio"></a>

## Bundle invoices as a PDF portfolio (`/Collection`)

> **Available in v0.95.0.** Builds on the embedded-files plumbing above.

A **PDF portfolio** (a.k.a. a *collection* or *package*, ISO 32000-1 §7.11.6 §12.3.5)
presents a document's embedded files as a navigable, sortable list — a cover sheet
plus an attachments panel with **columns**. It's the right container when you want to
ship many files as one PDF (a month of invoices, a bid package, a case file) and have
the viewer show them in a table with a description, a date, an amount, etc.

The files themselves still live in `/Names /EmbeddedFiles` (so embed them first with
`addAttachment`); `setCollection` only adds the **presentation layer** — the catalog
`/Collection` dictionary with its `/View`, column `/Schema`, `/Sort` and the
initially-selected file, plus a per-file `/CI` carrying each file's column values.

```ts
const enc = new TextEncoder();

// 1. Start from a cover page and embed the invoices.
const doc = giga.open(giga.txtToPdf("Acme Corp — January 2026 invoices"));
const invoices = [
  { name: "INV-001.pdf", customer: "Globex",   total: "1240.00", date: "D:20260103" },
  { name: "INV-002.pdf", customer: "Initech",  total: "890.50",  date: "D:20260118" },
  { name: "INV-003.pdf", customer: "Umbrella",  total: "3120.00", date: "D:20260129" },
];
for (const inv of invoices) {
  // (here the bytes would be each rendered invoice PDF)
  doc.addAttachment(inv.name, enc.encode(`%PDF invoice ${inv.name}`), {
    mime: "application/pdf",
    description: `Invoice for ${inv.customer}`,
  });
}

// 2. Turn it into a portfolio: a details view with four columns, sorted by date.
doc.setCollection({
  view: "details", // a multi-column list ("tile" = a grid of file tiles)
  schema: [
    { key: "customer", subtype: "text",     name: "Customer", order: 0 },
    { key: "total",    subtype: "number",   name: "Total (€)", order: 1 },
    { key: "issued",   subtype: "date",     name: "Issued",   order: 2 },
    { key: "file",     subtype: "filename", name: "File",     order: 3 }, // viewer-derived
  ],
  sort: { field: "issued", ascending: true },
  defaultFile: "INV-001.pdf", // selected when the portfolio opens
  items: invoices.map((inv) => ({
    file: inv.name,
    values: { customer: inv.customer, total: inv.total, issued: inv.date },
  })),
});

const out = doc.save();
doc.close();
```

Opened in a portfolio-aware viewer (Acrobat, PDF.js portfolios), `out` shows the cover
page plus the three invoices as rows under **Customer · Total (€) · Issued · File**,
sorted by issue date, with `INV-001.pdf` selected.

Read the configuration back — handy to reflect existing document state in an editor:

```ts
const coll = giga.open(out).collection();
//   → { view: "details",
//       schema: [ { key: "customer", subtype: "text", name: "Customer", order: 0 }, … ],
//       sort: { field: "issued", ascending: true },
//       defaultFile: "INV-001.pdf",
//       items: [ { file: "INV-001.pdf", values: { customer: "Globex", total: "1240", issued: "D:20260103" } }, … ] }
if (coll === null) {
  // not a portfolio (no /Collection)
}
```

Notes:

- **Column kinds.** `"text"` / `"date"` / `"number"` are **user** columns — their value
  comes from each file's `items[].values` (written as the filespec `/CI`). The rest —
  `"filename"`, `"description"`, `"size"`, `"creationDate"`, `"modDate"` — are
  **synthetic**: the viewer derives them from the file itself, so they need no value.
- Re-calling `setCollection` **replaces** the whole `/Collection` (and rewrites each
  `/CI`). An **empty/omitted `schema`** still produces a valid `/Collection` — useful to
  just flag the document as a portfolio and let the viewer show its default columns.
- A number value is stored as a numeric `/CI` entry; a date value is a PDF date string
  (`D:YYYYMMDD…`), the same format `creationDate`/`modDate` use elsewhere.

---

<a id="metadata"></a>

## Set document metadata (Info + XMP, kept in sync)

> **Available in v0.76.0.**

A PDF stores document metadata in **two** places: the legacy `/Info` dictionary
(`/Title`, `/Author`, …) and the catalog `/Metadata` **XMP** packet (RDF/XML, the
form modern readers, search indexers and DAM systems consult — ISO 32000-2
deprecates `/Info` in favour of it). Keeping them consistent is the classic
"two sources of truth" trap. `setInfo` writes **both** from one typed object:

```ts
const doc = giga.open(pdfBytes);

doc.setInfo({
  title: "Annual Report 2026",
  author: "Ada Lovelace",
  subject: "Financial results",
  keywords: "finance, annual, 2026",
  creator: "GigaPDF",
  creationDate: "D:20260624153000+02'00'", // PDF date string
});

// setInfo is a PARTIAL update — this changes only the title, author is preserved:
doc.setInfo({ title: "Annual Report 2026 (final)" });

const out = doc.save();
doc.close();
```

The XMP packet is regenerated to match, mapping each field to its standard
namespace (`dc:title`, `dc:creator`, `dc:description`, `pdf:Keywords`,
`xmp:CreatorTool`, `pdf:Producer`, `xmp:CreateDate` / `xmp:ModifyDate`), with PDF
dates converted to ISO 8601.

Read metadata back, or take full control of the raw XMP:

```ts
const reopened = giga.open(out);
reopened.getMetadata("Title");           // "Annual Report 2026 (final)"  (from /Info)
const xmp = reopened.getXmp();            // Uint8Array of the RDF/XML packet, or null

// Replace the whole XMP packet with your own (e.g. a custom schema):
reopened.setXmp(`<?xpacket begin="﻿"?>…your RDF…<?xpacket end="w"?>`);
```

> `setMetadata(key, value)` still exists for a **single** `/Info` entry, but it
> does **not** touch the XMP — prefer `setInfo` so the two never drift. `setXmp`
> writes the `/Metadata` stream verbatim (uncompressed), overriding whatever
> `setInfo` generated.

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

## Image → PDF

Wrap a raster image in a one-page PDF. The format is auto-detected — **PNG,
JPEG, GIF, WebP, AVIF** — and the image is placed on an A4 page, centred and
shrunk to fit (never upscaled). PNG keeps every color-type and bit-depth, Adam7
interlacing and transparency (via `/SMask`); GIF/WebP/AVIF are transcoded to PNG
before embedding. An unrecognized format returns an empty `Uint8Array`.

```ts
const pdf = giga.imageToPdf(imageBytes); // one A4 page
if (pdf.length === 0) throw new Error("not a recognized image");
```

Batch — turn a folder of images into a single multi-page PDF by wrapping each
one and merging the results:

```ts
const pages = images.map((bytes) => giga.imageToPdf(bytes)).filter((p) => p.length > 0);
const album = giga.mergePdfs(pages); // one PDF, one image per page
```

---

## Stamp an image watermark

> **Available in v0.69.0.** For a *text* watermark, draw rotated, faded text with
> [`addStandardText`](#styled-text) (`opacity` + `rotationDeg`) on each page.

`addImageWatermark(data, opts?)` stamps a raster image across pages — e.g. a logo
or a "DRAFT" badge. The source is auto-detected (**PNG / JPEG / WebP / GIF /
AVIF**), embedded **once** and referenced on every target page, so a 50-page
watermark adds one image XObject, not fifty. It returns `false` if the bytes
aren't a decodable image.

```ts
const doc = giga.open(pdfBytes);

// Centred, faded logo on every page (defaults: anchor "center", opacity 0.25).
doc.addImageWatermark(logoPng, { width: 200 }); // height keeps aspect when omitted

// …or a tiled, rotated badge on pages 1–3 only.
doc.addImageWatermark(draftPng, {
  pages: [1, 2, 3],        // 1-based; omit or [] = every page
  anchor: "center",        // or a corner: "top-left" | "top-right" | "bottom-left" | "bottom-right"
  width: 120,              // points; height follows aspect
  rotationDeg: 45,         // rotate about the image centre
  opacity: 0.15,
  tile: true,              // repeat across the page…
  offsetX: 40,             // …with these gaps between tiles (in non-tile mode: anchor nudge)
  offsetY: 40,
});

const stamped = doc.save();
doc.close();
```

---

## A toggleable "Watermark" layer (optional content / OCG)

A plain watermark is baked into the page — a reader can't switch it off. An
**optional-content group** (OCG, a PDF *layer*; ISO 32000-1 §8.11) wraps the
drawn content in a marked-content sequence the viewer lists in its Layers panel,
so it can be shown or hidden (and ships hidden or visible by default).

`addLayer(name)` creates the OCG and returns its id. `beginOptionalContent(page,
id)` then assigns whatever you draw next to that layer — it registers the OCG
under the page's `/Resources /Properties` and emits `/OC /OCn BDC`; every drawing
call until `endOptionalContent(page)` (`EMC`) belongs to the layer. The calls
**nest**, so layers can be stacked.

```ts
const doc = giga.open(pdfBytes);

// 1. Create the layer (visible & unlocked by default).
const layer = doc.addLayer("Watermark");

// 2. Draw a faded "DRAFT" badge on page 1 *inside* the layer.
//    addStandardText(page, x, y, size, text, fontName, rgb=0xRRGGBB, opacity, rotationDeg)
doc.beginOptionalContent(1, layer);              // → "OC0" (its /Properties name)
doc.addStandardText(1, 160, 380, 64, "DRAFT", "Helvetica-Bold", 0xd81a1a, 0.18, 45);
doc.endOptionalContent(1);                        // EMC — close the sequence

// 3. (optional) Ship it hidden — the reader toggles it on from the Layers panel.
doc.setLayerVisibility(layer, false);

const out = doc.save();
doc.close();
```

The marked content is balanced (each `beginOptionalContent` has its
`endOptionalContent`) and survives a save/reopen: `layers()` still lists the OCG
and the page content keeps its `/OC … BDC … EMC` span. To gate several pieces on
one layer, call `beginOptionalContent` with the **same** id again — it reuses the
page's existing property entry. To nest, open a second layer before closing the
first:

```ts
const base = doc.addLayer("Annotations");
const detail = doc.addLayer("Annotations · detail");

doc.beginOptionalContent(1, base);                // outer
//   addRectangle(page, x, y, w, h, stroke=0xRRGGBB|null, fill=0xRRGGBB|null, lineWidth, opacity)
doc.addRectangle(1, 40, 40, 200, 120, 0x0000cc, null, 1, 1);
doc.beginOptionalContent(1, detail);              //   inner
doc.addStandardText(1, 48, 150, 10, "see note", "Helvetica", 0x000000, 1, 0);
doc.endOptionalContent(1);                        //   close detail
doc.endOptionalContent(1);                        // close base
```

---

## Merge multiple PDFs

`mergePdfs` concatenates a list of PDFs into one, in order:

```ts
const merged = giga.mergePdfs([first, second, third]);
// 0 inputs → empty; 1 → returned unchanged; N → pages appended sequentially
```

### Merge with page-range selection

Each input can also be a `MergePart` `{ pdf, pages? }` whose `pages` picks
**1-based** page numbers (in the order you give) — so you can take only some
pages of a source, reorder them, or repeat one, all in a single call (ISO
32000-1 §7.7.3). Whole-PDF and selected inputs mix freely; each selected page
keeps its content, resources, annotations and box geometry:

```ts
const merged = giga.mergePdfs([
  cover,                         // every page of the cover
  { pdf: report, pages: [3, 5, 6, 7] }, // only pages 3 and 5–7 of the report
  { pdf: appendix, pages: [2] }, // just page 2 of the appendix
]);
// Out-of-range page numbers are skipped; a part whose selection is entirely
// out of range contributes nothing.
```

For finer control (e.g. interleaving with edits) append page-by-page on an open
document instead. `appendPages` takes the same optional `pages` selection — omit
it to append every page:

```ts
const doc = giga.open(first);
doc.appendPages(second);             // all of `second`
doc.appendPages(third, [1, 4, 5]);   // only pages 1, 4 and 5 of `third`
const merged = doc.saveCompressed();
doc.close();
```

---

## 2-up booklet & contact sheet (N-up imposition)

Imposition draws the **content** of one page, scaled and translated, onto another
page (ISO 32000-1 §8.10, Form XObjects). The source page becomes a reusable Form
XObject — its content stream **and** `/Resources` (so its fonts and images come
along) — and is drawn on the target with a placement matrix. Each placement is a
single `q <a 0 0 d e f> cm /Fmn Do Q` appended to the target, so it never disturbs
what is already there.

The low-level primitive is `placePage(target, source, x, y, scaleX, scaleY)`:
`(x, y)` is where the source page's **visible lower-left corner** lands and
`(scaleX, scaleY)` scale it **as a reader sees it** — the source's `/MediaBox`
origin and `/Rotate` are baked into the matrix for you. It is *composable*: call
it as many times as you like (different sources, different cells) to build a
sheet. Source and target may be the same page.

**One call for the common cases** — `nUp(cols, rows, opts?)` imposes **every**
page of the document, `cols × rows` per sheet, onto freshly added sheets. Pages
go left-to-right, top-to-bottom; each is scaled to fit its cell (aspect
preserved) and centred; the originals are dropped, leaving only the imposed
sheets:

```ts
// 2-up: two source pages side by side per A4-landscape sheet.
const doc = giga.open(report);
doc.nUp(2, 1, { sheetWidth: 841.89, sheetHeight: 595.28 }); // A4 landscape
const twoUp = doc.saveCompressed();
doc.close();

// 4-up contact sheet (2×2) on A4 portrait with a wider gutter.
const sheet = giga.open(report);
const sheets = sheet.nUp(2, 2, { gutter: 24, margin: 28 }); // → number of sheets
const contact = sheet.saveCompressed();
sheet.close();
```

**Hand-rolled 2-up booklet** — when you need full control of placement (custom
sheet size, booklet page order, crop marks…), build it from `placePage`. Here we
fold an N-page document into a booklet: a wide sheet with two source pages per
side, scaled to half width.

```ts
const src = giga.open(report);
const n = src.pageCount();
const { width: pw, height: ph } = src.pageInfo(1); // assume uniform page size

const out = giga.open(report);            // start from the same pages…
// Append blank landscape sheets, two source pages each.
const sheetW = pw * 2;
const sheetH = ph;
const sheetCount = Math.ceil(n / 2);
const sheetPages: number[] = [];
let after = n;                            // append after the originals
for (let i = 0; i < sheetCount; i++) {
  out.addPage(sheetW, sheetH, after);
  after += 1;
  sheetPages.push(after);
}
// Place page 2k onto the left half, 2k+1 onto the right half.
for (let p = 1; p <= n; p++) {
  const sheet = sheetPages[Math.floor((p - 1) / 2)];
  const leftHalf = (p - 1) % 2 === 0;
  const x = leftHalf ? 0 : pw;
  out.placePage(sheet, p, x, 0, 1, 1);    // 1:1 — each fills its half exactly
}
// Drop the originals, keep only the imposed sheets.
for (let p = n; p >= 1; p--) out.deletePage(p);
const booklet = out.saveCompressed();
src.close();
out.close();
```

For an explicit affine (shear, a non-right-angle rotation, a mirror), use
`placePageMatrix(target, source, [a, b, c, d, e, f])` — it applies your matrix as
the `cm` directly, with **no** origin/rotation normalisation, so an identity
matrix draws the source 1:1 at the target origin.

---

## Scale page content & large-format pages (UserUnit)

`resizePage` changes the page box **only** — the drawn marks keep their
coordinates, so they overflow or float in the new box. To resize a page *with*
its content, scale the content for real.

`scalePageContent(page, factor)` does a true zoom of the whole page (ISO 32000-1
§8.3.4): it wraps the existing content in `q <factor 0 0 factor 0 0> cm … Q`,
scales `/MediaBox` + `/CropBox` (and any declared Bleed/Trim/Art) about the
origin, **and** scales every annotation `/Rect` so widget and stamp appearances —
fit from their `/BBox` into the `/Rect` per §12.5.5 — follow automatically.

```ts
// Halve a page: content, box and annotations all shrink to 50 %.
const doc = giga.open(pdfBytes);
doc.scalePageContent(1, 0.5);            // 612×792 → 306×396, content scaled to match
const half = doc.saveCompressed();
doc.close();
```

Shrink-or-grow **to fit** a target box (aspect preserved) with
`scalePageTo(page, width, height)`, which returns the uniform factor it applied:

```ts
// Fit an oversized page onto US-Letter (612×792), keeping its proportions.
const fit = giga.open(pdfBytes);
const factor = fit.scalePageTo(1, 612, 792); // e.g. 0.5 when the page was 1224×1584
const lettered = fit.saveCompressed();
fit.close();
```

`scalePageContentXY(page, sx, sy)` is the anisotropic variant (stretch a page to
a non-proportional target — `sx ≠ sy`).

**Large format without huge coordinates** — PDF user space tops out around 200
inches. For a poster or CAD sheet, leave the coordinates as they are and raise the
page's `/UserUnit` (ISO 32000-1 §14.11.2): one default unit becomes `unit`⁄72 inch,
so the medium itself is enlarged. This is *separate* from `scalePageContent` — it
rescales the coordinate system, it does not rewrite the content.

```ts
// A 24×36 in poster: author at 1728×2592 pt and double the unit (1 unit = 1⁄36″).
const poster = giga.open(pdfBytes);
poster.setUserUnit(1, 2.0);              // physical size doubles; coordinates unchanged
poster.pageUserUnit(1);                  // → 2.0 (1.0 when absent)
const big = poster.saveCompressed();
poster.close();
```

Pass `1.0` to `setUserUnit` to remove the key (the default — keeps the file
minimal).

---

## Compact output (object streams + cross-reference stream)

Three serializers, in increasing compactness. `saveOptimized` packs every
non-stream object into compressed `/ObjStm`s and writes a `/XRef` cross-reference
stream (PDF 1.5+) — the smallest output, accepted by every modern reader.

```ts
const doc = giga.open(pdfBytes);

const plain   = doc.save();            // classic, uncompressed — easiest to debug
const flated  = doc.saveCompressed();  // streams Flate-compressed, classic xref table
const compact = doc.saveOptimized();   // object streams + xref stream (smallest)

// Fine-grained: an xref stream but classic object bodies (no /ObjStm).
const xrefOnly = doc.saveOptimized({ objectStreams: false });

doc.close();
```

> Note: linearization ("Fast Web View", ISO 32000-1 Annex F) is a separate
> byte-layout optimization — produced by `toLinearized()` (next section), not by
> the compact serializers above.

---

## Fast Web View for web delivery (linearization)

When a PDF is served over HTTP and the viewer supports **byte-range requests**, a
**linearized** ("Fast Web View") file lets the reader render **page 1 before the
whole file has downloaded**. Linearization (ISO 32000-1 **Annex F**) re-orders the
file so the first page — and only the objects needed to draw it — plus a
`/Linearized` parameter dictionary and a *hint stream* (a compact index of where
each page's objects live) sit at the **front**; the remaining pages and shared
resources follow, and a final cross-reference table closes the file.

```ts
import { GigaPDF } from "@gigapdf/sdk";

const giga = await GigaPDF.create();
const doc = giga.open(pdfBytes);

// Produce a linearized ("Fast Web View") PDF. Streams are Flate-compressed and
// embedded fonts subset, exactly like saveCompressed().
const webReady = doc.toLinearized();       // (saveLinearized() is an alias)

// Serve `webReady` with `Accept-Ranges: bytes` so the browser/viewer can fetch
// the first-page region first and start rendering immediately.
doc.close();
```

What you get, in file order:

1. `%PDF-1.7` header;
2. the `/Linearized` parameter dictionary (`/L` file length, `/H` hint-stream
   `[offset length]`, `/O` first-page object number, `/E` end of the first page,
   `/N` page count, `/T` main-xref offset) — the **first object**, so a reader can
   read it from the very start;
3. the **first-page cross-reference section**, whose trailer's `/Prev` chains to
   the main xref;
4. the document catalog, then the **primary hint stream**;
5. the **first page** and its private objects (its content, page-only resources);
6. the remaining pages, then the **shared** objects (page tree, cross-page fonts);
7. the **main cross-reference table** + final trailer.

If the document has no page tree, `toLinearized()` returns the plain `save()`
output instead of failing.

> Verified with **qpdf**: `qpdf --check out.pdf` reports the file *linearized*
> with no warnings or errors (qpdf is strict about the hint tables and every byte
> offset).

---

## OCR a scanned page + full-text search

For pages that **already carry a text layer**, `structuredText` / `search` are
exact and fast. For **scanned, image-only** pages, OCR them and stamp an
invisible (render-mode 3) text layer so the result becomes selectable and
searchable.

OCR runs **host-side** in the **`gigapdf-ocr-rten`** crate (PaddleOCR + RTen — pure-Rust, no
Tesseract; 13 printed languages incl. Hebrew + **automatic per-line script selection**, plus opt-in
handwriting). `ocr_pdf_page` rasterizes a page, recognizes it, and returns words already in **PDF
user space** so they drop straight onto `addTextLayer`.

### Native: OCR every page → searchable PDF (auto script selection)

```rust
use gigapdf_core::Document;
use gigapdf_ocr_rten::OcrEngine;

let pdf = std::fs::read("scan.pdf")?;
let doc = Document::open(&pdf)?;
let eng = OcrEngine::load_models_dir("models")?; // shared DBNet det + every recognizer present

for page in 1..=doc.page_count() as u32 {
    // Detect lines, recognize each with the best-matching printed recognizer (KR→ko, RU→cyrillic…).
    let words = eng.ocr_pdf_page(&doc, page, 2.0)?; // scale ≥ 2 for small text
    for w in &words {
        // w: OcrWord { text, x, y, width, height, confidence, model } — PDF user space, bottom-left.
        println!("p{page} [{:.2}|{}] {}", w.confidence, w.model, w.text);
    }
}
```

### Handwriting (opt-in — explicit recognizer)

A handwriting model is overconfident on printed text, so it's **excluded from auto selection**.
Call it explicitly when the input is known to be handwritten (Latin/Cyrillic/Greek):

```rust
use gigapdf_ocr_rten::{OcrEngine, HANDWRITING_MODEL};

let eng = OcrEngine::load_models_dir("models")?;
if eng.has_handwriting() {
    // Either the convenience method…
    let lines = eng.recognize_page_handwriting(&page_rgb)?;
    // …or force any recognizer by name:
    let lines = eng.recognize_page_with(&page_rgb, HANDWRITING_MODEL)?; // "latin_hw"
    for l in &lines { println!("[{:.2}] {}", l.confidence, l.text); }
}
```

### Stamp the text layer (this WASM SDK) → searchable

The native engine returns boxes in PDF user space; map them onto `addTextLayer` (render-mode 3,
invisible) so the scan becomes selectable + searchable:

```ts
// `words` come from your OCR service (the native engine above).
const doc = giga.open(scannedPdf);
doc.addTextLayer(page, words.map((w) => ({ x: w.x, y: w.y, size: w.height, text: w.text })));
const searchable = doc.save();
doc.close();
```

> Models (det + recognizers + Hebrew + handwriting) are fetched/converted at deploy time
> (`crates/ocr-rten/tools/fetch_models.sh`) and are **not** bundled in the package. Full design:
> [OCR_ARCHITECTURE.md](OCR_ARCHITECTURE.md).

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

// A visible signature field for the signing pipeline (sets /SigFlags).
doc.addSignatureField(1, "approval", [380, 60, 560, 120]);

// Make it an *interactive* form: a computed total + input validation, with an
// explicit calculation order.
doc.addTextField(1, "qty", [50, 480, 150, 498], "1");
doc.addTextField(1, "price", [50, 450, 150, 468], "10");
doc.addTextField(1, "total", [50, 420, 150, 438], "");
doc.setFieldScript("qty", "keystroke", "if (event.willCommit) AFNumber_Keystroke(0,0,0,0,'',true);");
doc.setFieldScript(
  "total",
  "calculate",
  "event.value = Number(this.getField('qty').value) * Number(this.getField('price').value);"
);
doc.setFieldScript("total", "format", "AFNumber_Format(2,0,0,0,'€ ',true);");
doc.setCalculationOrder(["total"]); // recompute order when fields change

// Edit a value programmatically, then rebuild its appearance.
doc.setTextField("price", "12");
doc.regenerateFieldAppearance("price");

// Drop a field entirely (from /Fields, /CO and the page annotations).
doc.removeField("skills");

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
doc.addCircleAnnotation(1, 300, 600, 380, 660, 0x0066cc, 0xcce5ff, 2); // stroked + filled ellipse
doc.addPolygonAnnotation(1, [100, 480, 160, 480, 130, 540], 0x008000, null, 1.5); // closed triangle
doc.addPolylineAnnotation(1, [200, 480, 240, 510, 280, 480], 0x884400, 1.5); // open path
doc.addCaretAnnotation(1, 420, 690, 432, 704, 0x333333); // insertion mark
doc.addInk(1, [80, 600, 120, 620, 160, 590], 0x0000ff, 2); // freehand polyline
doc.addStamp(1, 60, 540, 180, 568, "APPROVED", 0xc00000);

// Edit an annotation's colour, then rebuild its baked /AP so every viewer shows it:
doc.regenerateAppearance(1, 0); // 0-based index in the page's annotation list

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

## Navigation: links, bookmarks & open-action

The full ISO 32000-1 action & destination model — links and bookmarks accept
**any** `Action` (`goto` with every fit mode, `gotoR`, `uri`, `named`
navigation, `launch`, `javascript`, `submitForm`, `resetForm`), and the document
can carry an `/OpenAction` performed when it opens.

```ts
const doc = giga.open(pdfBytes);

// A clickable rect that jumps to page 4, scrolled so y=720 sits at the top, 150% zoom.
doc.addLink(1, { x: 72, y: 700, w: 180, h: 16 }, {
  type: "goto",
  dest: { fit: "xyz", page: 4, top: 720, zoom: 1.5 },
});

// An external URL, and a "fit the whole page" jump to a sibling document.
doc.addLink(1, { x: 72, y: 676, w: 180, h: 16 }, { type: "uri", uri: "https://giga-pdf.com" });
doc.addLink(1, { x: 72, y: 652, w: 180, h: 16 }, {
  type: "gotoR",
  file: "appendix.pdf",
  dest: { fit: "fit", page: 1 },
});

// Open the file on its last page (a Named viewer action).
doc.setOpenAction({ type: "named", action: "lastPage" });

// Bookmarks that carry actions: a destination (→ /Dest) and a URL (→ /A).
doc.setBookmarks([
  { title: "Cover", level: 0, action: { type: "goto", dest: { fit: "fit", page: 1 } } },
  { title: "Chapter 1", level: 0, action: { type: "goto", dest: { fit: "fitH", page: 2, top: 780 } } },
  { title: "  Section 1.1", level: 1, action: { type: "goto", dest: { fit: "xyz", page: 3 } } },
  { title: "Project site", level: 0, action: { type: "uri", uri: "https://giga-pdf.com" } },
]);

// Remove the first link on page 1 (links counted in order, other annotations ignored).
doc.removeLink(1, 0);

const out = doc.save();
doc.close();
```

### Document-level JavaScript (`/Names /JavaScript`)

Beyond per-link / open-action scripts, a document can install **document-level
JavaScript** in the catalog `/Names /JavaScript` name tree (ISO 32000-1
§7.7.3.4 + §12.6.4.16). A conforming viewer runs every entry **in name (lexical)
order** when the file opens — so this is where form-calculation and validation
helper libraries belong, and where a one-shot "on open" action lives. `/JS` is
stored as a literal string (or a FlateDecode stream past ~2 KB), and adding a
script preserves any sibling `/Names` subtrees (e.g. `/EmbeddedFiles`).

```ts
const doc = giga.open(pdfBytes);

// Runs first (name "00_init" sorts before "10_greet"): set a text field's value.
doc.addDocumentJavascript("00_init", `this.getField("orderRef").value = "INV-2026-0042";`);

// Runs next: greet the reader on open.
doc.addDocumentJavascript("10_greet", `app.alert("Welcome — review the form before signing.");`);

// Read them back, in the order viewers execute them.
for (const js of doc.documentJavascripts()) {
  console.log(js.name, "→", js.script);
}
// 00_init → this.getField("orderRef").value = "INV-2026-0042";
// 10_greet → app.alert("Welcome — review the form before signing.");

// Re-using a name replaces it; removeDocumentJavascript drops one entry.
doc.removeDocumentJavascript("10_greet");

const out = doc.save();
doc.close();
```

---

## Sign a PDF (B · B-T · LTV)

Four PAdES levels, escalating in long-term assurance — all produce a CMS
signature in a `/Sig` field over a `/ByteRange`-patched PDF, **entirely in-engine**
(no node-forge / @signpdf / pdf-lib). `sign` / `signP12` are synchronous;
`signTimestamped` / `signLtv` are **`async`** because they need network round
trips, performed by the SDK through a **host-fetch two-phase model** (the WASM core
has no network: the engine emits a request blob, the SDK does the HTTP, the engine
embeds the response). All three of `signP12` / `signTimestamped` / `signLtv`
**throw** a single generic `Error` on failure (anti-enumeration).

| Level | Method | What it adds |
|-------|--------|--------------|
| **B** (self-signed) | `sign(fields, random, keyBits?)` | ephemeral digital ID, `adbe.pkcs7.detached` |
| **B** (PKCS#12) | `signP12(p12, password, opts?)` | a real CA / eIDAS identity from a `.p12`/`.pfx` |
| **B-T** | `signTimestamped(opts)` *(async)* | + an RFC 3161 **trusted timestamp** in the SignerInfo |
| **B-LT / B-LTA** | `signLtv(opts)` *(async)* | B-T + a `/DSS` (cert chain + OCSP/CRL); B-LTA adds an archival `/DocTimeStamp` |

### Level B — PKCS#12 or self-signed

```ts
const doc = giga.open(pdfBytes);

// (a) PKCS#12: a CA-issued / eIDAS cert + RSA key, imported natively.
const signed = doc.signP12(p12Bytes, "p12-password", {
  name: "Jane Doe",
  reason: "I approve this document",
  date: "D:20260619120000Z",   // /M — a PDF date string (the engine has no clock)
  location: "Paris",
  contactInfo: "jane@example.com",
});

// (b) …or an ephemeral, self-signed digital ID (no certificate file).
//     fields = "name\treason\tdate\tnotBefore\tnotAfter"; ≥ 256 host-entropy bytes.
const fields = "Jane Doe\tApproved\tD:20260619120000Z\t260619000000Z\t360619000000Z";
const ephemeral = doc.sign(fields, crypto.getRandomValues(new Uint8Array(256)));

doc.close();
```

### Certify (DocMDP) — lock down later changes

```ts
const doc = giga.open(pdfBytes);
const fields = "Approver\tI certify this document\tD:20260624000000Z\t260101000000Z\t360101000000Z";
// docmdpLevel: 1 = no changes · 2 = form-fill + sign · 3 = also annotate
const certified = doc.certify(fields, crypto.getRandomValues(new Uint8Array(256)), 2);
doc.close();
```

### Verify signatures

Verification runs against the **original bytes** you opened (the document doesn't
retain them). It recomputes the `/ByteRange` digest, checks the CMS
`messageDigest`, and validates the RSA SignerInfo signature.

```ts
const bytes = signedPdfBytes;           // the exact bytes you opened
const doc = giga.open(bytes);

for (const s of doc.signatures()) {
  console.log(s.fieldName, s.signerName, s.subFilter);
}

for (const r of doc.verifySignatures(bytes)) {
  const valid = r.byteRangeOk && r.digestOk && r.signatureOk;
  console.log(
    `${r.fieldName}: ${valid ? "VALID" : "INVALID"} ` +
    `(${r.algorithm}, CN=${r.signerCommonName ?? "?"}, ` +
    `covers whole file: ${r.coversWholeDocument})`
  );
}

doc.close();
```

### Level B-T — trusted timestamp (PAdES-B-T)

`signTimestamped` adds an **RFC 3161 timestamp** from a TSA (here FreeTSA), proving
the signature existed at a verifiable time. By default the SDK POSTs the
`TimeStampReq` via the exported `defaultTsaPost`. The signing identity is the
`p12` + `password` when supplied, otherwise the self-signed path (`random` +
`notBefore` / `notAfter`).

```ts
import { defaultTsaPost } from "@qrcommunication/gigapdf-lib";

const doc = giga.open(pdfBytes);

const signed = await doc.signTimestamped({
  p12: p12Bytes,
  password: "p12-password",
  name: "Jane Doe",
  reason: "Approved",
  date: "D:20260619120000Z",
  tsaUrl: "https://freetsa.org/tsr",
  // Default fetch (no allow-list) — fine for a trusted, hard-coded TSA URL:
  tsaFetch: defaultTsaPost,
});
doc.close();
// `signed` is the PAdES-B-T PDF bytes.
```

> **SSRF — host-controlled fetch.** The TSA URL here is yours, so `defaultTsaPost`
> is safe. If a URL ever comes from untrusted input, pass your own `tsaFetch` that
> validates it first:
>
> ```ts
> tsaFetch: async (req, url) => {
>   assertAllowed(url);                       // your allow-list / proxy
>   const r = await fetch(url, { method: "POST",
>     headers: { "Content-Type": "application/timestamp-query" }, body: req });
>   return new Uint8Array(await r.arrayBuffer());
> },
> ```

### Level B-LT / B-LTA — long-term validation (PAdES-LTV)

`signLtv` produces a B-T signature **and then embeds the validation material** — a
`/DSS` (Document Security Store) carrying the certificate chain plus OCSP/CRL
revocation responses — so the signature keeps verifying long after its
certificates expire or are revoked. The engine computes *which* OCSP/CRL endpoints
to query **from the certificates' AIA / CRL-DP extensions**; the SDK fetches them
(unreachable responders are skipped). With `archiveTimestamp: true` it also adds a
`/DocTimeStamp` over the whole updated file (**B-LTA**, the renewable archival
anchor — a second TSA round trip).

```ts
import { defaultTsaPost, defaultOcspPost, defaultCrlGet } from "@qrcommunication/gigapdf-lib";

const doc = giga.open(pdfBytes);

const ltv = await doc.signLtv({
  p12: p12Bytes,
  password: "p12-password",
  name: "Jane Doe",
  reason: "Approved",
  date: "D:20260619120000Z",
  tsaUrl: "https://freetsa.org/tsr",
  archiveTimestamp: true,           // false → B-LT; true → B-LTA (extra /DocTimeStamp)
  // Default HTTP for TSA + OCSP + CRL (no allow-list — trusted endpoints only):
  tsaFetch: defaultTsaPost,
  revocationFetch: defaultOcspPost, // OCSP: POST application/ocsp-request
  crlFetch: defaultCrlGet,          // CRL: GET the distribution point
});
doc.close();
// `ltv` is the PAdES-B-LTA PDF bytes — long-term verifiable.
```

> **SSRF (NON-NEGOTIABLE for LTV).** Unlike the TSA URL, the **OCSP/CRL URLs are
> read from the signing certificate**, so a malicious certificate could point them
> at an internal host. The engine only computes *which* URLs to fetch — the host
> decides *whether* to. A service that signs untrusted input MUST replace the
> default `revocationFetch` / `crlFetch` with validating fetchers:
>
> ```ts
> revocationFetch: async (req, url) => { assertPublicHttps(url); /* …POST… */ },
> crlFetch:        async (url)      => { assertPublicHttps(url); /* …GET…  */ },
> ```
>
> A self-signed identity (no AIA / CRL-DP) simply yields a `/DSS/Certs`-only store.

---

## Author a tagged (accessible) PDF

`toTagged` emits a logical-structure tree (`/StructTreeRoot`) the same way PDF/A
level A does — headings, paragraphs, tables, lists and figures become
`/StructElem`s with marked content (`/MCID`) — but **without** forcing the file
into PDF/A. The catalog gains `/MarkInfo /Marked true`, a `/Lang`, a `/RoleMap`
and `/Alt` on every figure. Pass `{ pdfUa: true }` to also declare **PDF/UA-1**
(ISO 14289) in XMP.

```ts
const doc = giga.open(pdfBytes);

// A plain tagged PDF (accessible structure, not PDF/A).
const tagged = doc.toTagged();

// …or one that also carries the PDF/UA-1 conformance identifier.
const ua = doc.toTagged({ pdfUa: true });

doc.close();
```

The structure is derived from the engine's reconstruction (the same model behind
`toDocx`/`toHtml`), so documents produced by the HTML→PDF and Office→PDF
pipelines tag well. If a document has no reconstructable structure, the plain
(untagged) PDF is returned unchanged.

> Figures without author-supplied alternate text receive a non-empty `/Alt`
> placeholder so the output stays structurally PDF/UA-valid. **Meaningful** alt
> text — what the figure actually conveys — requires author input: set it with
> `setFigureAlt` (next section). For full archival **and** accessibility,
> `toPdfA("pdfa-2a")` produces a PDF/A-2a (level A) file with the same tree (and
> the same author `/Alt`).

### Meaningful alternate text per figure (`/Alt`)

Real accessibility (WCAG, PDF/UA, ISO 32000-1 §14.9.3) needs each figure to carry
**alternate text** describing the image — only the author knows what a figure
conveys. `setFigureAlt(index, alt)` attaches that text to the figure's `/Figure`
structure element so it lands in the tagged output instead of the placeholder.

`index` is the **document-global figure index**: figures are numbered `0, 1, 2, …`
in page order, then in page-content order within each page (the Nth image of the
whole document is figure `N`). `figureCount()` returns how many figures the engine
reconstructs (the valid range `0..figureCount()`).

```ts
const doc = giga.open(pdfBytes);

console.log(doc.figureCount()); // e.g. 2

// Describe each figure for assistive technology.
doc.setFigureAlt(0, "Bar chart: quarterly revenue grew 18% YoY");
doc.setFigureAlt(1, "Photo of the signed contract, page 3");

// The text lands on each `/Figure` `/Alt` in any accessible export:
const tagged = doc.toTagged({ pdfUa: true });   // tagged + PDF/UA-1
const archival = doc.toPdfA("pdfa-2a");          // PDF/A-2a (level A)

doc.close();
```

Figures you don't label keep the generic placeholder, so a partly-labelled
document still validates (no figure is ever left without an `/Alt`). `setFigureAlt`
returns `false` if `alt` is empty — a `/Alt` must be non-empty to be useful, and
the placeholder is the "no alt" state.

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

## Change a password, drop encryption, or encrypt to a certificate

Beyond setting a password at save time, you can **re-key** or **decrypt** an
already-open document, or encrypt to **X.509 recipients** so no shared password
is needed at all (ISO 32000-1 §7.6).

```ts
// Rotate the passwords on an existing protected file.
const locked = giga.openEncrypted(encryptedBytes, "old-user")!;
const rekeyed = locked.changePasswords("new-user", "doc-001", {
  ownerPassword: "new-owner",
  // encryptMetadata: false,   // optionally leave the XMP metadata in the clear
});
// The old password no longer opens `rekeyed`; "new-user" does.

// Remove protection entirely → a plaintext PDF.
const plaintext = locked.removeEncryption();
locked.close();
```

**Public-key (certificate) encryption** — encrypt to one or more recipients;
only a holder of a recipient private key can open it:

```ts
const doc = giga.open(pdfBytes);
// `cert` is a DER X.509 certificate (e.g. from a colleague's .cer file).
const sealed = doc.encryptForRecipients([cert], { aes256: true });
doc.close();

// The recipient opens it with their certificate + PKCS#1 RSA private key.
const opened = giga.openWithPrivateKey(sealed, cert, privateKeyDer);
// `giga.open(sealed)` / `openEncrypted` cannot open it — there is no password.
opened?.close();
```

> `seed`/`rngSeed` default to Web Crypto randomness; pass them explicitly in a
> non-browser host without `globalThis.crypto`. Multiple recipients: pass several
> certificates — `encryptForRecipients([alice, bob], …)`.

---

## Move, resize & restyle existing elements in place

Two in-place editors operate on the existing content stream — they wrap the
target element's ops in `q … Q` and inject only the override operators, so the
edit is **non-destructive** (internal coordinates are never rewritten) and the
rest of the page is untouched.

### Move + resize an image with `transformElement`

`transformElement(page, index, m)` applies a full affine PDF matrix
`m = [a, b, c, d, e, f]` (scale / rotate / shear / translate) to an element. It
**generalises** `moveElement` — whose matrix is the pure translate
`[1,0,0,1,dx,dy]` — to move **and** resize **and** rotate in a single call, and
because it is purely matrix-based it works identically for text, images and
shapes. The engine emits `q  a b c d e f cm  <element ops>  Q`.

```ts
const doc = giga.open(pdfBytes);

// Find the image we want to shrink + reposition (element index on page 1).
const imgs = doc.imageElements(1);
const index = imgs[0].index;

// Scale to 50% (a = d = 0.5), no rotation/shear (b = c = 0), and translate
// +100pt right / +40pt up (e = 100, f = 40). One call = move + resize.
doc.transformElement(1, index, [0.5, 0, 0, 0.5, 100, 40]); // true on success

// Rotate an element 90° CCW about its own origin: [cosθ, sinθ, −sinθ, cosθ, 0, 0].
// doc.transformElement(1, index, [0, 1, -1, 0, 0, 0]);

const out = doc.save();
doc.close();
```

### Swap an image's pixels in place with `replaceImage`

To change a logo or a photo **without** disturbing its placement, replace the
existing image XObject's bytes instead of deleting and re-adding it.
`replaceImage(page, index, data)` (ISO 32000-1 §8.9) re-encodes the new **PNG or
JPEG** through the same path as `addImage` and rewrites the stream **over the same
object number** — so the image keeps its object id, **every `/Do` invocation, and
its position / scale / rotation / clip matrix**. Only the stream bytes and the
image dictionary (`/Width`, `/Height`, `/ColorSpace`, `/BitsPerComponent`,
`/Filter`) change.

A delete-then-`addImage` instead loses the original placement (and any `/SMask` /
clip): the new image lands wherever `addImage` puts it. `replaceImage` is the
in-place swap.

```ts
const doc = giga.open(pdfBytes);

// Identify the image to swap by its unified element index — the same `index`
// reported by imageElements (and used by transformElement / removeElement).
const img = doc.imageElements(1)[0];

// Replace its pixels with a new raster. The image stays exactly where it was —
// same box, same rotation, same clip — only the picture changes.
const ok = doc.replaceImage(1, img.index, newLogoPngOrJpeg); // false if not an image / undecodable

// The new raster need not match the old pixel size: it is stretched into the
// existing box. To re-fit a differently-proportioned image, transform it after:
// doc.transformElement(1, img.index, [/* scale/translate to taste */]);

const out = doc.save();
doc.close();
```

### Restyle a vector path with `setPathStyle`

`setPathStyle(page, index, style)` re-styles a **path** element in place — it
returns `false` for any non-path index. Colours are RGB `[r,g,b]` in `0..=1` and
`dash` is the PDF dash array (`[]` = solid). For each field you set, one override
operator is injected before the path's paint op (`fill`→`r g b rg`,
`stroke`→`r g b RG`, `strokeWidth`→`w`, `dash`→`[…] 0 d`); omitted fields keep
the inherited graphics state.

```ts
const doc = giga.open(pdfBytes);

// Find the path we want to recolour (e.g. the first painted path on page 1).
const paths = doc.vectorPaths(1);
const index = doc.elements(1).findIndex((e) => e.kind === "path");

// Fill red, 2pt black stroke, dashed 4-on / 2-off.
const ok = doc.setPathStyle(1, index, {
  fill: [1, 0, 0],
  stroke: [0, 0, 0],
  strokeWidth: 2,
  dash: [4, 2],
});
// ok === false would mean `index` isn't a path.

const out = doc.save();
doc.close();
```

> **Opacity is applied.** `fillAlpha` / `strokeAlpha` (`0..=1`) take effect — the
> engine registers an `/ExtGState` carrying `/ca` / `/CA` on the page and injects a
> `/<gs> gs` into the path's `q … Q` wrap, so the alpha applies to that path run
> only. For a **non-path** element (e.g. an image), use `setElementOpacity`
> (below) instead.

### Make a shape or image semi-transparent

For **any** element — text, image **or** shape — `setElementOpacity(page, index,
fillAlpha)` sets one constant opacity (`0..=1`) in place: it registers a page
`/ExtGState` (`/ca` = `/CA` = `fillAlpha`) and wraps the element's op range in
`q /<gs> gs … Q`. This is the way to fade an **image** in place.

```ts
const doc = giga.open(pdfBytes);

// Fade the first image on page 1 to 40% opacity.
const img = doc.imageElements(1)[0];
doc.setElementOpacity(1, img.index, 0.4); // true on success

const out = doc.save();
doc.close();
```

For a **shape** you can use either API: `setElementOpacity` (one value for both
fill and stroke) or `setPathStyle` when you need independent fill / stroke alpha:

```ts
// Path element: 30% fill, fully opaque stroke.
doc.setPathStyle(1, pathIndex, { fillAlpha: 0.3, strokeAlpha: 1 });
```

### Bring an element to front / send it to back

`reorderElement(page, index, toFront)` changes the native PDF paint (z) order of
any element. `toFront = true` splices its op range to the **end** of the content
stream (painted last → on top); `false` splices it to the **start** (painted
first → behind everything). The moved range is re-wrapped in `q … Q` with the
element's effective graphics state (fill/stroke colour, line width, dash and, for
text, font) re-emitted inside it, so it renders identically at its new position
and does not leak state onto neighbours.

```ts
const doc = giga.open(pdfBytes);

// Bring the first image on page 1 on top of everything else.
const img = doc.imageElements(1)[0];
doc.reorderElement(1, img.index, true); // true → on top

// …or send element #2 behind everything:
// doc.reorderElement(1, 2, false);

const out = doc.save();
doc.close();
```

> **The index changes after the splice.** Because the element's ops are moved
> within the stream, its unified index is no longer valid — re-read
> `pageElements(page)` (or `imageElements` / `vectorPaths`) before addressing it
> again by index.

---

## Render a page without a specific element (live-overlay editing)

`renderPageExcluding(page, indices, scale?)` rasterises a page to PNG while
**omitting** the given top-level unified element `indices` (from `pageElements`).
Each excluded element paints nothing — fills, strokes, shadings, images and text
alike — while everything else renders normally. It **generalises**
`renderPageNoText` (which suppresses *all* text). The classic use: paint a
background **without** the element the user is currently editing, then overlay an
editable, live version (real Fabric/HTML widget) exactly on top.

```ts
const doc = giga.open(pdfBytes);

// The element under edit on page 1 (e.g. the run the user clicked).
const els = doc.elements(1);
const editing = els.findIndex((e) => e.kind === "text");

// Background = the page minus that element, at 2× (144 dpi). Overlay your
// editable widget at the element's bounds on top of this PNG.
const bg = doc.renderPageExcluding(1, [editing], 2);

// Hide several at once; an empty list renders the full page; unknown indices
// are ignored:
// const clean = doc.renderPageExcluding(1, [editing, 5, 9], 2);

doc.close();
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

## Set how the document opens (ViewerPreferences · PageLayout · PageMode)

These three catalog entries are *reading hints* a conforming viewer honours when
it first opens the file — they don't change a single page, only the UX. Use them
for a presentation (full screen), a magazine (two-up spread), a portfolio
(attachments panel), or a kiosk (chrome-less window).

```ts
const doc = giga.open(pdfBytes);

// /ViewerPreferences (ISO 32000-1 §12.2) — every field is optional; an omitted
// field is left untouched, a boolean sets the key. `direction` is "L2R" | "R2L".
doc.setViewerPreferences({
  hideToolbar: true,     // chrome-less reading
  hideMenubar: true,
  fitWindow: true,       // size the window to the first page
  centerWindow: true,
  displayDocTitle: true, // show the doc title, not the file name, in the title bar
});

// /PageLayout — how pages are arranged on screen.
doc.setPageLayout("TwoPageLeft");   // a magazine-style spread, first page on the left

// /PageMode — which panel (if any) the reader opens with.
doc.setPageMode("UseOutlines");     // open with the bookmarks panel showing

const out = doc.save();
doc.close();
```

Right-to-left scripts (Arabic, Hebrew) set the dominant reading order so spreads
and page navigation flow the correct way:

```ts
doc.setViewerPreferences({ direction: "R2L" });
doc.setPageLayout("TwoPageRight");
```

To clear a hint, pass `null` to `setPageLayout` / `setPageMode`; for
`/ViewerPreferences`, set the relevant flags to `false` (when the dictionary ends
up empty it's removed entirely). Each setter returns `false` (without throwing)
on an unknown name or an invalid `direction`, so a bad value is a no-op:

```ts
doc.setPageLayout(null);                       // remove /PageLayout
doc.setPageMode(null);                         // remove /PageMode
doc.setViewerPreferences({ hideToolbar: false }); // unset chrome-less reading

doc.setPageMode("UseLayers" as never);         // → false (unknown name, no change)
```

> The names are exactly the ISO 32000-1 enumerations:
> `PageLayout` = `SinglePage` · `OneColumn` · `TwoColumnLeft` · `TwoColumnRight`
> · `TwoPageLeft` · `TwoPageRight`; `PageMode` = `UseNone` · `UseOutlines` ·
> `UseThumbs` · `FullScreen` · `UseOC` · `UseAttachments`. They are typed unions
> in the SDK, so your editor autocompletes them.

---

## See also

- [`SDK.md`](SDK.md) — every `GigaPdfEngine` / `GigaPdfDoc` method, grouped by domain.
- [`USAGE.md`](USAGE.md) — the raw `extern "C"` buffer ABI and host integration.
- [`HTML-CSS.md`](HTML-CSS.md) — exhaustive HTML / CSS / JS support in the HTML→PDF engine.
- [`API.md`](API.md) — the Rust ↔ WASM ABI mapping.
- [`INSTALL.md`](INSTALL.md) — install, build-from-source, Next.js standalone wiring.
