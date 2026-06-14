# Usage — host integration

The engine ships as a single `gigapdf_wasm.wasm` with a flat, dependency-free
`extern "C"` ABI. This guide shows how a JavaScript host (browser or Node) drives
it. For the complete symbol list see [API.md](API.md).

## 1. Load the module

```js
import { readFileSync } from "node:fs"; // browser: fetch + arrayBuffer
const { instance } = await WebAssembly.instantiate(
  readFileSync("gigapdf_wasm.wasm"),
  {}, // no imports required — the engine is self-contained
);
const ex = instance.exports;
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
// also: gp_to_pptx, gp_to_odt, gp_to_ods, gp_to_rtf
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
// also: gp_txt_to_pdf, gp_rtf_to_pdf
```

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

// 5. Embed it, then add selectable text in that font.
const f = strArg("Roboto");
const ttfPtr = toWasm(ttf);
const fontObj = ex.gp_embed_font(handle, f.ptr, f.len, ttfPtr, ttf.length); // > 0 on success
ex.gp_free(ttfPtr, ttf.length); freeArg(f);

const txt2 = strArg("Crisp embedded text — café");
ex.gp_add_text(handle, 1, 72, 700, 18, txt2.ptr, txt2.len, fontObj, 0x000000);
freeArg(txt2);
```

## 9. Security: encrypt, sign, PDF/A

```js
// Encrypt (host supplies the file-id randomness)
const pw = strArg("s3cret"), id = strArg("16-byte-file-id!");
const enc = callBuffer((lp) => ex.gp_save_encrypted(handle, pw.ptr, pw.len, id.ptr, id.len, -44, lp));
freeArg(pw); freeArg(id);

// Self-signed digital signature — host supplies random bytes for key generation
const fields = strArg("Signer\tReason\tD:20260614120000Z\t260614000000Z\t360614000000Z");
const rand = crypto.getRandomValues(new Uint8Array(256));
const rPtr = toWasm(rand);
const signed = callBuffer((lp) => ex.gp_sign(handle, fields.ptr, fields.len, rPtr, rand.length, 512, lp));
ex.gp_free(rPtr, rand.length); freeArg(fields);

// PDF/A-2b archival metadata
const pdfa = callBuffer((lp) => ex.gp_to_pdfa(handle, lp));
```

## 10. Always free what you allocate

Every `gp_alloc` / buffer-returning pointer must be `gp_free`d, and every
`gp_open*` handle must be `gp_close`d. The helpers above do this for you.
