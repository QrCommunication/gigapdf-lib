# Usage — host integration

The engine ships as a single `gigapdf_wasm.wasm` with a flat `extern "C"` ABI
and no `wasm-bindgen`. It imports one host function — `env.gp_host_random`
(entropy for RSA signatures and `Math.random`) — and exports the `gp_*` symbols.
This guide shows how a JavaScript host (browser or Node) drives it. For the
complete symbol list see [API.md](API.md).

> **Prefer the high-level SDK?** Most hosts never touch the raw ABI below. The
> `GigaPdfEngine` / `GigaPdfDoc` classes wrap all of it — start with the
> **[Cookbook](COOKBOOK.md)** (redaction, styled text, headers/footers,
> conversions, OCR, forms, annotations, signing, encryption, the editable
> model), the [SDK recipes](../sdk/README.md#recipes), and the per-method
> [`SDK.md`](SDK.md) reference. The guide below is for hosts driving the
> `extern "C"` module directly.

## 1. Load the module

```js
import { readFileSync } from "node:fs"; // browser: fetch + arrayBuffer
let ex; // exports; the import closure reads ex.memory lazily (called post-init)
const { instance } = await WebAssembly.instantiate(
  readFileSync("gigapdf_wasm.wasm"),
  {
    env: {
      // The engine's only host import: entropy for RSA signatures + Math.random.
      gp_host_random(ptr, len) {
        const view = new Uint8Array(ex.memory.buffer, ptr, len);
        for (let off = 0; off < len; off += 65536) {
          crypto.getRandomValues(view.subarray(off, Math.min(off + 65536, len)));
        }
      },
    },
  },
);
ex = instance.exports;
```

## 2. The memory ABI

All data crosses the boundary through the wasm **linear memory**. Three rules:

1. **Input bytes**: allocate with `gp_alloc(len)`, copy into memory, pass `(ptr, len)`.
2. **Output bytes**: "buffer-returning" functions take an `out_len` pointer; they
   return a data pointer and write the length to `*out_len`. **The host owns the
   result** and must `gp_free(dataPtr, len)`.
3. Re-create the `Uint8Array`/`DataView` after any call that may have grown memory.

```js
const u8 = () => new Uint8Array(ex.memory.buffer);
const dv = () => new DataView(ex.memory.buffer);

function toWasm(bytes) {                 // host → wasm
  const ptr = ex.gp_alloc(bytes.length);
  u8().set(bytes, ptr);
  return ptr;
}

function callBuffer(call) {              // wasm → host (buffer-returning)
  const lenPtr = ex.gp_alloc(4);         // usize is 32-bit on wasm32
  const dataPtr = call(lenPtr);
  if (dataPtr === 0) { ex.gp_free(lenPtr, 4); return null; }
  const len = dv().getUint32(lenPtr, true);
  const out = u8().slice(dataPtr, dataPtr + len); // copy out
  ex.gp_free(dataPtr, len);
  ex.gp_free(lenPtr, 4);
  return out;
}

const enc = new TextEncoder(), dec = new TextDecoder();
function strArg(s) { const b = enc.encode(s); return { ptr: toWasm(b), len: b.length }; }
function freeArg(a) { ex.gp_free(a.ptr, a.len); }
```

## 3. Open / save a document

```js
const inPtr = toWasm(pdfBytes);
const handle = ex.gp_open(inPtr, pdfBytes.length); // 0 on failure
ex.gp_free(inPtr, pdfBytes.length);

// ... operate on `handle` ...

const saved = callBuffer((lp) => ex.gp_save(handle, lp)); // Uint8Array
ex.gp_close(handle);                                       // free the document
```

Encrypted input: `ex.gp_open_encrypted(ptr, len, pwPtr, pwLen)`.

## 4. Editing

```js
// Replace text run #0 on page 1
const t = strArg("New text");
ex.gp_replace_text(handle, 1, 0, t.ptr, t.len); // returns 0 on success
freeArg(t);

// Inspect elements as JSON (text/image/shape with bounds)
const els = JSON.parse(dec.decode(callBuffer((lp) => ex.gp_elements_json(handle, 1, lp))));

// Add a rectangle (rgb packed 0xRRGGBB; -1 = none)
ex.gp_add_rectangle(handle, 1, 72, 72, 200, 100, 0x808080, -1, 1.0);

// True redaction: delete everything intersecting the region (no opaque cover)
ex.gp_redact_region(handle, 1, 100, 100, 200, 20, 0, /*has_cover=*/0);
```

> **`redactPii` (SDK, v0.52.4)** — for genuinely sensitive data, the SDK's
> `redactPii(page, rects)` goes further than `gp_redact_region`: it also
> **erases the pixels of any image** in the zone (so a scanned/OCR'd page can't
> be recovered) and stamps an opaque mark. `gp_redact_region` removes text
> operators only and leaves images intact.

### Styled text (underline / strikethrough)

`gp_add_text_styled` / `gp_add_text_standard_styled` add the rotation and two
decoration flags after the colour/opacity (`…, rgb, opacity, rotation_deg,
underline, strikethrough`). The rules are filled in the text colour and follow
the rotation. The SDK exposes these as the optional `opts = { underline,
strikethrough }` argument on `addText` / `addStandardText`.

```js
// Underlined standard-Helvetica text at 0° (underline=1, strikethrough=0).
const t = strArg("Confidential"), fn = strArg("Helvetica");
ex.gp_add_text_standard_styled(handle, 1, 72, 700, 12, t.ptr, t.len, fn.ptr, fn.len,
  0xcc0000, 1.0, 0, /*underline=*/1, /*strikethrough=*/0);
freeArg(t); freeArg(fn);
```

## 4b. Build an interactive form (AcroForm, no `pdf-lib`)

Coordinates are PDF user space `[x0, y0, x1, y1]` (origin bottom-left). The style
is 7 trailing scalars: `font_size, color_rgb, border_rgb, has_border, bg_rgb,
has_bg, border_width` (here: auto size, black text, black border, no background).

```js
const style = [0, 0x000000, 0x000000, 1, 0x000000, 0, 1];

// Text field (max 60 chars). max_len < 0 = unlimited; multiline / password = 0|1.
{
  const n = strArg("fullname"), v = strArg("");
  ex.gp_add_text_field(handle, 1, n.ptr, n.len, 50, 700, 300, 720, v.ptr, v.len, 60, 0, 0, ...style);
  freeArg(n); freeArg(v);
}
// Checkbox, initially checked, on-state name "Yes".
{
  const n = strArg("subscribe"), e = strArg("Yes");
  ex.gp_add_checkbox(handle, 1, n.ptr, n.len, 50, 670, 64, 684, 1, e.ptr, e.len, ...style);
  freeArg(n); freeArg(e);
}
// Radio group: exports newline-separated, rects a comma-separated flat 4×N list.
{
  const n = strArg("plan"), ex2 = strArg("Basic\nPro");
  const r = strArg("50,640,64,654,80,640,94,654"), sel = strArg("Pro");
  ex.gp_add_radio_group(handle, 1, n.ptr, n.len, ex2.ptr, ex2.len, r.ptr, r.len, sel.ptr, sel.len, ...style);
  freeArg(n); freeArg(ex2); freeArg(r); freeArg(sel);
}
// Combo box (drop-down). Options newline-separated; last flag = editable.
{
  const n = strArg("country"), o = strArg("FR\nUS\nDE"), sel = strArg("FR");
  ex.gp_add_combo_box(handle, 1, n.ptr, n.len, 50, 610, 200, 626, o.ptr, o.len, sel.ptr, sel.len, 0, ...style);
  freeArg(n); freeArg(o); freeArg(sel);
}
// List box. Empty `selected` = none; last flag = multi-select.
{
  const n = strArg("langs"), o = strArg("en\nfr\nde"), sel = strArg("");
  ex.gp_add_list_box(handle, 1, n.ptr, n.len, 50, 540, 200, 600, o.ptr, o.len, sel.ptr, sel.len, 1, ...style);
  freeArg(n); freeArg(o); freeArg(sel);
}

// Read them straight back (name, kind, value, options).
const fields = JSON.parse(dec.decode(callBuffer((lp) => ex.gp_fields_json(handle, lp))));
```

## 5. Render a page to PNG

```js
const png = callBuffer((lp) => ex.gp_render_page(handle, 1, 2.0, lp)); // 2× scale
```

## 6. Convert PDF → anything

```js
const docx = callBuffer((lp) => ex.gp_to_docx(handle, lp)); // real editable Word
const xlsx = callBuffer((lp) => ex.gp_to_xlsx(handle, lp)); // tables → cells
const html = dec.decode(callBuffer((lp) => ex.gp_to_html(handle, lp)));
const txt  = dec.decode(callBuffer((lp) => ex.gp_to_text(handle, lp)));
// also: gp_to_pptx, gp_to_odp, gp_to_odt, gp_to_ods, gp_to_rtf
```

## 7. Convert anything → PDF

```js
const docxBytes = /* a .docx */;
const dPtr = toWasm(docxBytes);
const pdf = callBuffer((lp) => ex.gp_office_to_pdf(dPtr, docxBytes.length, lp)); // null if not Office
ex.gp_free(dPtr, docxBytes.length);

const html = strArg("<p>Hello</p>");
const pdf2 = callBuffer((lp) => ex.gp_html_to_pdf(html.ptr, html.len, lp));
freeArg(html);

const imgBytes = /* PNG/JPEG/GIF/WebP/AVIF */;
const iPtr = toWasm(imgBytes);
const pdf3 = callBuffer((lp) => ex.gp_image_to_pdf(iPtr, imgBytes.length, lp)); // null if not an image — one A4 page, centred & shrink-to-fit
ex.gp_free(iPtr, imgBytes.length);
// also: gp_txt_to_pdf, gp_rtf_to_pdf
```

## 7b. HTML + CSS → PDF, with JavaScript and page breaks

The HTML renderer is a native engine (no browser). It runs the document's inline
`<script>`s **before layout**, so script-driven content is rendered. Use the SDK
helpers (`GigaPdfEngine.htmlNeededFonts` → fetch fonts → `htmlRender`), or via
the raw ABI two-phase flow. JavaScript runs automatically — no extra call.

```html
<!-- A report whose rows are built by JavaScript, split across pages. -->
<body style="font-family: Roboto">
  <h1>Invoice</h1>
  <table id="rows"></table>
  <script>
    const items = [['Widget', 9.99], ['Gadget', 19.5], ['Gizmo', 4.25]];
    const t = document.getElementById('rows');
    for (const [name, price] of items) {
      const tr = document.createElement('tr');
      tr.innerHTML = '<td>' + name + '</td><td>' + price.toFixed(2) + '</td>';
      t.appendChild(tr);
    }
  </script>

  <!-- Force a new page before the terms section: -->
  <div style="page-break-before: always"></div>
  <h2>Terms</h2>
  <p>Net 30.</p>

  <!-- …or drop a <pagebreak> tag anywhere to break to the next page. -->
</body>
```

Page breaks: any of `style="page-break-before: always"`,
`page-break-after: always`, `break-before: page`, a `<pagebreak></pagebreak>`
element, or `class="page-break"` starts the next content on a fresh page.

### Page size, margins, header/footer, numbering

`htmlRender(html, fonts, pageW = 612, pageH = 792, margin = 36)` is the simple
path (explicit size + one uniform margin). For named paper sizes, per-side
margins and a **running header/footer with automatic page numbers**, use
`htmlRenderWith` — and `htmlNeededFontsWith` so the header/footer fonts are
fetched too:

```ts
const header = `<div style="text-align:center;color:#888">Acme Inc.</div>`;
const footer = `<div style="text-align:right">Page {{page}} / {{pages}}</div>`;

const fonts = await fetchFonts(giga.htmlNeededFontsWith(html, header, footer));
const pdf = giga.htmlRenderWith(html, fonts, {
  pageSize: "A4",                                   // a0…a6, b4/b5, letter, legal, tabloid, executive (+ "-landscape")
  margin: { top: 72, bottom: 72, left: 54, right: 54 }, // or a single number
  header,
  footer,                                           // {{page}} / {{pages}} substituted per page
  headerOffset: 24,                                 // pt from the top/bottom edge (default 18)
  startPageNumber: 1,
});
```

`giga.pageSize("a4-landscape")` resolves a name to `{ w, h }` points if you need
the dimensions directly. See [`docs/HTML-CSS.md` §1](HTML-CSS.md#1-page-setup)
for the full size table and the header/footer rules.

The JS engine supports classes/`super`, closures, destructuring, `RegExp`,
`Map`/`Set`, `Symbol`, `eval`/`Function`, and — through a **suspendable bytecode
VM** — lazy/infinite generators and spec-ordered `async`/`await` (with full
`try/catch/finally`, `switch`, labels and spread across a suspension), plus DOM
APIs (`querySelector(All)` with `>`/`+`/`~`/`[attr]`, `textContent`/`innerHTML`/
`setAttribute`/`classList`/`style`, …).

For the **complete list of supported HTML elements, CSS properties, units,
colours and selectors**, see [`docs/HTML-CSS.md`](HTML-CSS.md).

## 8. Fonts: catalog, download (host), embed

The wasm sandbox has no network — the engine tells you **what** to fetch and
**parses** what you fetched; **your host performs the HTTP request**.

```js
// 1. Browse the catalog (1951 families).
const catalog = JSON.parse(dec.decode(callBuffer((lp) => ex.gp_font_catalog_json(lp))));

// 2. Which fonts does the document reference but not embed?
const needed = JSON.parse(dec.decode(callBuffer((lp) => ex.gp_needed_fonts(handle, lp))));

// 3. Ask the engine for the Google Fonts CSS URL, then YOU fetch it.
const fam = strArg("Roboto");
const cssUrl = dec.decode(callBuffer((lp) => ex.gp_font_request_url(fam.ptr, fam.len, 400, 0, lp)));
freeArg(fam);
//    Fetch with a legacy User-Agent so Google returns TTF (not WOFF2):
const css = await (await fetch(cssUrl, { headers: { "User-Agent": "Mozilla/5.0 (Windows NT 10.0)" } })).text();

// 4. Extract the trusted gstatic URL (anti-SSRF) and fetch the TTF.
const c = strArg(css);
const ttfUrl = dec.decode(callBuffer((lp) => ex.gp_parse_css_font_url(c.ptr, c.len, lp)));
freeArg(c);
const ttf = new Uint8Array(await (await fetch(ttfUrl)).arrayBuffer());

// 5. Embed it, then add selectable text in that font. gp_embed_font accepts any
//    outline file — a glyf .ttf OR an OpenType-CFF .otf (OTTO), auto-detected.
const f = strArg("Roboto");
const ttfPtr = toWasm(ttf);
const fontObj = ex.gp_embed_font(handle, f.ptr, f.len, ttfPtr, ttf.length); // > 0 on success
ex.gp_free(ttfPtr, ttf.length); freeArg(f);

const txt2 = strArg("Crisp embedded text — café");
ex.gp_add_text(handle, 1, 72, 700, 18, txt2.ptr, txt2.len, fontObj, 0x000000);
freeArg(txt2);

// 6. No download needed for the 14 standard fonts — draw straight away.
const tb = strArg("Times-Bold"), heading = strArg("Heading in Times Bold");
ex.gp_add_text_standard(handle, 1, 72, 660, 18, heading.ptr, heading.len, tb.ptr, tb.len, 0x000000, 1, 0);
freeArg(tb); freeArg(heading);

// 7. Reuse a face the PDF already embeds: list → extract → re-embed → draw.
const embedded = JSON.parse(dec.decode(callBuffer((lp) => ex.gp_embedded_fonts_json(handle, lp))));
//    embedded = [{ baseFont, format: "truetype" | "cff" | "type1" }]
const nm = strArg(embedded.find((f) => f.format === "truetype").baseFont);
const prog = callBuffer((lp) => ex.gp_extract_font(handle, nm.ptr, nm.len, lp)); freeArg(nm);
//    prog[0] = format tag (1 truetype / 2 cff / 3 type1); prog.slice(1) = the font bytes,
//    feed back into gp_embed_font to draw new text in the document's own face
//    (glyf truetype and full OpenType cff re-embed directly).

// 8. Edit text in place — font-aware. A run in an embedded Type0/Identity-H face
//    (TrueType or OpenType-CFF) is re-encoded through that font's char→glyph map.
const repl = strArg("Réécrit dans la même police");
ex.gp_replace_text(handle, 1, 0, repl.ptr, repl.len); // run #0 on page 1
freeArg(repl);
```

> SDK wrappers for the above: `doc.addStandardText(page, x, y, size, text, fontName)`,
> `doc.embeddedFonts()`, `doc.extractFont(name)`, `doc.embedFont(family, font)`
> (any `.ttf`/`.otf`), `doc.addText(page, x, y, size, text, fontObj)`,
> `doc.replaceText(page, index, text)` (font-aware).

## 9. Security: encrypt, sign, PDF/A

```js
// Encrypt with AES-256 (algo 2). The host supplies the file id AND a secret
// 32-byte file key (the engine has no RNG); algo 0=RC4-128, 1=AES-128.
const pw = strArg("s3cret"), owner = strArg("owner-pw"), id = strArg("16-byte-file-id!");
const key = new Uint8Array(32); crypto.getRandomValues(key);
const kPtr = toWasm(key);
const enc = callBuffer((lp) =>
  ex.gp_save_encrypted(handle, pw.ptr, pw.len, owner.ptr, owner.len, id.ptr, id.len, kPtr, key.length, 2, -44, lp));
freeArg(pw); freeArg(owner); freeArg(id); ex.gp_free(kPtr, key.length);

// Self-signed digital signature — host supplies random bytes for key generation
const fields = strArg("Signer\tReason\tD:20260614120000Z\t260614000000Z\t360614000000Z");
const rand = crypto.getRandomValues(new Uint8Array(256));
const rPtr = toWasm(rand);
const signed = callBuffer((lp) => ex.gp_sign(handle, fields.ptr, fields.len, rPtr, rand.length, 512, lp));
ex.gp_free(rPtr, rand.length); freeArg(fields);

// PDF/A-2b archival metadata
const pdfa = callBuffer((lp) => ex.gp_to_pdfa(handle, lp));
```

## 9b. Running headers & footers

Bake a running header/footer onto an existing PDF. The spec is JSON (the SDK's
`HeaderFooterSpec`); `{{page}}` / `{{pages}}` are substituted per page.
`gp_header_footer` reads back what's baked (the reader side).

```js
const hdr = strArg(JSON.stringify({ text: "Acme Inc.", align: "center", fontSize: 10 }));
ex.gp_set_header(handle, hdr.ptr, hdr.len); freeArg(hdr);

const ftr = strArg(JSON.stringify({ text: "Page {{page}} / {{pages}}", align: "right" }));
ex.gp_set_footer(handle, ftr.ptr, ftr.len); freeArg(ftr);

// Read them back: { header: {...}|null, footer: {...}|null }
const hf = JSON.parse(dec.decode(callBuffer((lp) => ex.gp_header_footer(handle, lp))));

// Remove: ex.gp_remove_headers(handle) / ex.gp_remove_footers(handle)
// Margins: ex.gp_page_margins(handle, page, lp) / ex.gp_set_page_margins(handle, page, t, r, b, l)
```

## 9c. The unified editable model

The model is a format-neutral JSON tree (the SDK's `GigaDocument`). Lower any
format into it, edit it with ops, and raise it to any format — all through JSON
strings across the ABI.

```js
// Lower this PDF → model JSON.
const model = dec.decode(callBuffer((lp) => ex.gp_model_from_pdf(handle, lp)));
//   also: gp_model_from_office(ptr,len,lp) · gp_model_from_html(ptr,len,lp)

// Edit: apply an ops array (JSON). Addresses are [section, page, blockIndex].
const ops = JSON.stringify([
  { op: "setRunText", addr: [0, 0, 0], run: 0, text: "Revised title" },
]);
const m = strArg(model), o = strArg(ops);
const edited = dec.decode(callBuffer((lp) => ex.gp_model_apply_ops(m.ptr, m.len, o.ptr, o.len, lp)));
freeArg(m); freeArg(o);

// Raise: model JSON → any target.
const e = strArg(edited);
const docx = callBuffer((lp) => ex.gp_model_to_docx(e.ptr, e.len, lp));
//   also: gp_model_to_{xlsx,pptx,odt,ods,odp,pdf}  → bytes
//         gp_model_to_{html,rtf}                    → string
freeArg(e);
```

## 10. Always free what you allocate

Every `gp_alloc` / buffer-returning pointer must be `gp_free`d, and every
`gp_open*` handle must be `gp_close`d. The helpers above do this for you.

## 11. Document viewer (browser)

`@qrcommunication/gigapdf-lib/viewer` is a **zero-dependency document viewer**
built on the engine — no pdf.js, no external libs. It opens **PDF, Office
(docx/xlsx/pptx + legacy & ODF) and HTML** (non-PDF inputs are converted to PDF
in-engine), renders pages with `renderPage`, **detects each page's orientation**
and adapts, and provides navigation, zoom, a thumbnail rail and a **fullscreen
presentation mode**.

```ts
import { GigaPdfEngine } from "@qrcommunication/gigapdf-lib";
import { GigaPdfViewer } from "@qrcommunication/gigapdf-lib/viewer";

const giga = await GigaPdfEngine.load(wasmUrl);
const viewer = new GigaPdfViewer(giga, document.getElementById("app")!);

await viewer.open({ kind: "auto", bytes });   // pdf / office / html auto-detected
//   or: { kind: "office", bytes } · { kind: "html", html, fonts? } · { kind: "pdf", bytes }

viewer.goTo(3);
viewer.present();                              // fullscreen slideshow (←/→, Space, Esc, F)
viewer.orientation(3);                         // "portrait" | "landscape"
viewer.destroy();                              // close + detach listeners
```

**Zoom & fit.** The toolbar carries `−` / `+`, a live `%` readout and a preset
drop-down (Fit width · Fit page · 50–400 %). Programmatically:

```ts
viewer.fitWidth();        // fit the page width to the viewport …
viewer.fitPage();         // … or the whole page (width *and* height)
viewer.actualSize();      // 100 %
viewer.setZoomPercent(150);
viewer.setZoom(1.25);     // multiplier
viewer.zoom;              // current multiplier (getter)
```

A chosen **fit mode is sticky**: it re-applies when the window resizes or when you
move to a page of a different orientation. `Ctrl`/`⌘` + mouse-wheel zooms.

Keyboard: `←`/`→` `PageUp`/`PageDown` `Space` navigate, `Home`/`End` jump, `+`/`-`
zoom, `0` actual size, `F` toggle presentation, `Esc` exit. The viewer is
browser-only (DOM); the engine itself runs anywhere.

### Editing canvas

`@qrcommunication/gigapdf-lib/editor` extends the viewer with an interactive
**editing canvas** (`GigaPdfEditor`): an SVG overlay per page with tools —
select / text / rectangle / ellipse / line / freehand ink / image / highlight /
redaction — plus select·move·delete and a tool palette. Edits are drawn live in
page coordinates and **baked into the real PDF** through the engine
(`addRectangle`/`addEllipse`/`addPolygon`/`addText`/`addImage`/`redact`), then the
page re-renders.

```ts
import { GigaPdfEditor } from "@qrcommunication/gigapdf-lib/editor";

// A TTF is required for the text tool (the engine has no bundled fonts).
const ed = new GigaPdfEditor(giga, host, { defaultFont: { family: "Roboto", ttf } });
await ed.open({ kind: "auto", bytes });

ed.setTool("rect");
ed.setStyle({ color: 0xcc0000, fill: null, lineWidth: 2 });
// …user draws on the page…
ed.applyEdits();          // bake pending edits into the PDF + re-render
const pdf = ed.save();    // the edited PDF bytes
```

The editor inherits **all** the viewer's zoom / fit / presentation controls.

**Rulers & margins.** Every page shows graduated **rulers** (top + left, in mm)
and four **margin guides** that you drag *live* from handles in the ruler bands —
or type exact values in the palette's `T R B L` mm fields. The guides scale with
the zoom and stay a constant on-screen size.

```ts
ed.setMargins({ top: 25, bottom: 25, left: 20, right: 20 }); // mm by default
ed.setMargins({ left: 56.7 }, "pt");                          // …or points
ed.getMargins();          // { top, right, bottom, left } in mm
ed.showRulers(false);     // hide the rulers + guides
```

Geometry is kept in page **points** (zoom-invariant) and flipped to PDF's
bottom-left origin on apply. The `Apply` / `Delete` buttons, a colour picker and
the margin controls ship in the palette; `setTool`/`setStyle`/`applyEdits`/`save`/
`removeSelected`/`clearEdits`/`setMargins`/`getMargins`/`showRulers` are the
programmatic API.
