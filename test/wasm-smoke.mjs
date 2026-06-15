// End-to-end smoke test of the WASM engine, driven from Node with no deps.
// Loads the .wasm, opens a real PDF, edits text, adds a frame, saves, and
// verifies the output is a valid PDF that re-opens with the edit applied.
//
// Run:  node test/wasm-smoke.mjs
import { readFileSync } from "node:fs";

const WASM =
  "target/wasm32-unknown-unknown/release/gigapdf_wasm.wasm";

const { instance } = await WebAssembly.instantiate(readFileSync(WASM), {});
const ex = instance.exports;

const u8 = () => new Uint8Array(ex.memory.buffer);
const dv = () => new DataView(ex.memory.buffer);

function toWasm(bytes) {
  const ptr = ex.gp_alloc(bytes.length);
  u8().set(bytes, ptr);
  return ptr;
}
function readBufferReturning(call) {
  const lenPtr = ex.gp_alloc(4); // usize is 32-bit on wasm32
  const dataPtr = call(lenPtr);
  const len = dv().getUint32(lenPtr, true);
  const out = u8().slice(dataPtr, dataPtr + len);
  ex.gp_free(dataPtr, len);
  ex.gp_free(lenPtr, 4);
  return out;
}
const dec = new TextDecoder();
const enc = new TextEncoder();

let failures = 0;
const check = (cond, msg) => {
  console.log(`${cond ? "ok  " : "FAIL"}  ${msg}`);
  if (!cond) failures++;
};

// 1. open a real PDF
const pdf = readFileSync("fixtures/simple-text.pdf");
const inPtr = toWasm(pdf);
const handle = ex.gp_open(inPtr, pdf.length);
ex.gp_free(inPtr, pdf.length);
check(handle !== 0, "gp_open returns a handle");
check(ex.gp_page_count(handle) >= 1, "page_count >= 1");

// 2. read text runs as JSON
const runsJson = dec.decode(
  readBufferReturning((lp) => ex.gp_text_runs_json(handle, 1, lp)),
);
const runs = JSON.parse(runsJson);
check(Array.isArray(runs) && runs.length > 0, `text runs parsed (${runs.length})`);

// 3. edit text run 0
const newText = enc.encode("Edited via WASM");
const tPtr = toWasm(newText);
const rc = ex.gp_replace_text(handle, 1, 0, tPtr, newText.length);
ex.gp_free(tPtr, newText.length);
check(rc === 0, "gp_replace_text returns 0");

// 4. add a frame rectangle
const rcRect = ex.gp_add_rectangle(
  handle, 1, 50, 50, 200, 100, 0x000000, 1, 0, 0, 1.5,
);
check(rcRect === 0, "gp_add_rectangle returns 0");

// 5. save and verify the output is a PDF
const saved = readBufferReturning((lp) => ex.gp_save(handle, lp));
check(saved.length > 0, `gp_save produced ${saved.length} bytes`);
check(dec.decode(saved.slice(0, 5)) === "%PDF-", "output starts with %PDF-");
ex.gp_close(handle);

// 6. re-open the saved bytes and confirm the edit survived
const rPtr = toWasm(saved);
const handle2 = ex.gp_open(rPtr, saved.length);
ex.gp_free(rPtr, saved.length);
check(handle2 !== 0, "saved PDF re-opens");
const runs2 = JSON.parse(
  dec.decode(readBufferReturning((lp) => ex.gp_text_runs_json(handle2, 1, lp))),
);
check(
  runs2.some((r) => r.text.includes("Edited via WASM")),
  "edited text present after round-trip",
);
ex.gp_close(handle2);

// 7. interactive forms: list + fill every field type through the WASM ABI
const form = readFileSync("fixtures/with-forms.pdf");
const fPtr = toWasm(form);
const fh = ex.gp_open(fPtr, form.length);
ex.gp_free(fPtr, form.length);
check(fh !== 0, "with-forms.pdf opens");

// helper: pass one or two string args to a setter, return its int code
const argStr = (s) => {
  const bytes = enc.encode(s);
  return { ptr: toWasm(bytes), len: bytes.length };
};
const freeArg = (a) => ex.gp_free(a.ptr, a.len);

const fields = JSON.parse(
  dec.decode(readBufferReturning((lp) => ex.gp_fields_json(fh, lp))),
);
check(fields.length === 5, `gp_fields_json lists 5 fields (${fields.length})`);
const kinds = Object.fromEntries(fields.map((f) => [f.name, f.kind]));
check(kinds.name === "text", "name classified as text");
check(kinds.agree === "checkbox", "agree classified as checkbox");
check(kinds.gender === "radio", "gender classified as radio");
check(kinds.country === "combo", "country classified as combo");

// text
{
  const n = argStr("name"), v = argStr("Jane Smith");
  check(ex.gp_set_text_field(fh, n.ptr, n.len, v.ptr, v.len) === 0, "gp_set_text_field name");
  freeArg(n); freeArg(v);
}
// checkbox
{
  const n = argStr("agree");
  check(ex.gp_set_checkbox(fh, n.ptr, n.len, 1) === 0, "gp_set_checkbox agree");
  freeArg(n);
}
// radio (use its first declared export option)
{
  const opt = fields.find((f) => f.name === "gender").options[0];
  const n = argStr("gender"), v = argStr(opt);
  check(ex.gp_set_radio(fh, n.ptr, n.len, v.ptr, v.len) === 0, `gp_set_radio gender=${opt}`);
  freeArg(n); freeArg(v);
}
// choice
{
  const n = argStr("country"), v = argStr("Germany");
  check(ex.gp_set_choice(fh, n.ptr, n.len, v.ptr, v.len) === 0, "gp_set_choice country=Germany");
  freeArg(n); freeArg(v);
}

const savedForm = readBufferReturning((lp) => ex.gp_save(fh, lp));
check(savedForm.length > 0, `form save produced ${savedForm.length} bytes`);
ex.gp_close(fh);

const sfPtr = toWasm(savedForm);
const fh2 = ex.gp_open(sfPtr, savedForm.length);
ex.gp_free(sfPtr, savedForm.length);
const filled = JSON.parse(
  dec.decode(readBufferReturning((lp) => ex.gp_fields_json(fh2, lp))),
);
const valueOf = (n) => filled.find((f) => f.name === n)?.value;
check(valueOf("name") === "Jane Smith", "text value survived round-trip");
check(valueOf("agree") === "Yes", "checkbox value survived round-trip");
check(valueOf("country") === "Germany", "choice value survived round-trip");
ex.gp_close(fh2);

// 7b. interactive forms CREATION: build every field type via the WASM ABI,
// save, reopen, and confirm each field reads back with the right kind/value.
{
  const host = argStr("blank form host page");
  const blank = readBufferReturning((lp) => ex.gp_txt_to_pdf(host.ptr, host.len, lp));
  freeArg(host);
  const bPtr = toWasm(blank);
  const ch = ex.gp_open(bPtr, blank.length);
  ex.gp_free(bPtr, blank.length);
  check(ch !== 0, "blank host PDF opens for field creation");

  // packed style: fontSize 0 (auto), black text, black border (has=1),
  // no background (has=0), border width 1.
  const style = [0, 0x000000, 0x000000, 1, 0x000000, 0, 1];

  {
    const n = argStr("fullname"), v = argStr("Jane");
    check(
      ex.gp_add_text_field(ch, 1, n.ptr, n.len, 50, 700, 300, 720, v.ptr, v.len, 40, 0, 0, ...style) === 0,
      "gp_add_text_field fullname",
    );
    freeArg(n); freeArg(v);
  }
  {
    const n = argStr("subscribe"), e = argStr("Yes");
    check(
      ex.gp_add_checkbox(ch, 1, n.ptr, n.len, 50, 670, 64, 684, 1, e.ptr, e.len, ...style) === 0,
      "gp_add_checkbox subscribe",
    );
    freeArg(n); freeArg(e);
  }
  {
    const n = argStr("plan"), exps = argStr("Basic\nPro");
    const rects = argStr("50,640,64,654,80,640,94,654"), sel = argStr("Pro");
    check(
      ex.gp_add_radio_group(ch, 1, n.ptr, n.len, exps.ptr, exps.len, rects.ptr, rects.len, sel.ptr, sel.len, ...style) === 0,
      "gp_add_radio_group plan",
    );
    freeArg(n); freeArg(exps); freeArg(rects); freeArg(sel);
  }
  {
    const n = argStr("country"), o = argStr("FR\nUS"), sel = argStr("FR");
    check(
      ex.gp_add_combo_box(ch, 1, n.ptr, n.len, 50, 610, 200, 626, o.ptr, o.len, sel.ptr, sel.len, 0, ...style) === 0,
      "gp_add_combo_box country",
    );
    freeArg(n); freeArg(o); freeArg(sel);
  }
  {
    const n = argStr("langs"), o = argStr("en\nfr"), sel = argStr("");
    check(
      ex.gp_add_list_box(ch, 1, n.ptr, n.len, 50, 560, 200, 600, o.ptr, o.len, sel.ptr, sel.len, 1, ...style) === 0,
      "gp_add_list_box langs",
    );
    freeArg(n); freeArg(o); freeArg(sel);
  }

  const made = readBufferReturning((lp) => ex.gp_save(ch, lp));
  ex.gp_close(ch);
  const mPtr2 = toWasm(made);
  const ch2 = ex.gp_open(mPtr2, made.length);
  ex.gp_free(mPtr2, made.length);
  const created = JSON.parse(dec.decode(readBufferReturning((lp) => ex.gp_fields_json(ch2, lp))));
  check(created.length === 5, `created 5 fields via ABI (${created.length})`);
  const kindOf = Object.fromEntries(created.map((f) => [f.name, f.kind]));
  check(kindOf.fullname === "text", "created text field");
  check(kindOf.subscribe === "checkbox", "created checkbox");
  check(kindOf.plan === "radio", "created radio group");
  check(kindOf.country === "combo", "created combo box");
  check(kindOf.langs === "list", "created list box");
  const valOf = (n) => created.find((f) => f.name === n)?.value;
  check(valOf("fullname") === "Jane", "created text field value");
  check(valOf("plan") === "Pro", "created radio selection");
  ex.gp_close(ch2);
}

// 8. page ops + annotations + metadata through the WASM ABI
const multi = readFileSync("fixtures/multi-page.pdf");
const mPtr = toWasm(multi);
const mh = ex.gp_open(mPtr, multi.length);
ex.gp_free(mPtr, multi.length);
const pageCount = ex.gp_page_count(mh);
check(pageCount >= 2, `multi-page.pdf has >= 2 pages (${pageCount})`);

check(ex.gp_rotate_page(mh, 1, 90) === 0, "gp_rotate_page 90deg");

// annotations: add highlight + free text, then list them
check(
  ex.gp_add_highlight(mh, 1, 72, 700, 320, 720, 0xffff00) === 0,
  "gp_add_highlight",
);
{
  const t = argStr("Reviewed via WASM");
  check(
    ex.gp_add_free_text(mh, 1, 72, 600, 320, 660, t.ptr, t.len, 12, 0x0000ff) === 0,
    "gp_add_free_text",
  );
  freeArg(t);
}
// markup + ink + stamp
check(ex.gp_add_underline(mh, 1, 72, 580, 320, 592, 0xff0000) === 0, "gp_add_underline");
check(ex.gp_add_strike_out(mh, 1, 72, 560, 320, 572, 0xff0000) === 0, "gp_add_strike_out");
{
  const s = argStr("DRAFT");
  check(ex.gp_add_stamp(mh, 1, 400, 700, 520, 740, s.ptr, s.len, 0xff0000) === 0, "gp_add_stamp");
  freeArg(s);
}
{
  // one freehand polyline as a flat f64 array of x,y pairs
  const pts = [100, 100, 130, 140, 160, 110];
  const cptr = ex.gp_alloc(pts.length * 8);
  for (let i = 0; i < pts.length; i++) dv().setFloat64(cptr + i * 8, pts[i], true);
  check(ex.gp_add_ink(mh, 1, cptr, pts.length, 0x0000ff, 2) === 0, "gp_add_ink");
  ex.gp_free(cptr, pts.length * 8);
}

const annots = JSON.parse(
  dec.decode(readBufferReturning((lp) => ex.gp_annotations_json(mh, 1, lp))),
);
check(annots.length >= 6, `gp_annotations_json lists >= 6 (${annots.length})`);
check(
  annots.some((a) => a.contents.includes("Reviewed via WASM")),
  "free-text annotation content present",
);
check(
  ["Underline", "StrikeOut", "Ink", "Stamp"].every((s) =>
    annots.some((a) => a.subtype === s),
  ),
  "underline/strikeout/ink/stamp all listed",
);

// flatten: bake every annotation into the page content, markup goes away
const baked = ex.gp_flatten_annotations(mh, 1);
check(baked >= 6, `gp_flatten_annotations baked >= 6 (${baked})`);
const afterFlatten = JSON.parse(
  dec.decode(readBufferReturning((lp) => ex.gp_annotations_json(mh, 1, lp))),
);
check(afterFlatten.length === 0, "no annotations remain after flatten");

// metadata round-trip
{
  const k = argStr("Title"), v = argStr("GigaPDF Engine");
  check(ex.gp_set_metadata(mh, k.ptr, k.len, v.ptr, v.len) === 0, "gp_set_metadata Title");
  const title = dec.decode(
    readBufferReturning((lp) => ex.gp_get_metadata(mh, k.ptr, k.len, lp)),
  );
  check(title === "GigaPDF Engine", "gp_get_metadata reads it back");
  freeArg(k); freeArg(v);
}

// extract page 1 into a new standalone PDF
const pagesPtr = ex.gp_alloc(4);
dv().setUint32(pagesPtr, 1, true);
const extracted = readBufferReturning((lp) => ex.gp_extract_pages(mh, pagesPtr, 1, lp));
ex.gp_free(pagesPtr, 4);
check(extracted.length > 0 && dec.decode(extracted.slice(0, 5)) === "%PDF-", "gp_extract_pages → PDF");
const exPtr = toWasm(extracted);
const exh = ex.gp_open(exPtr, extracted.length);
ex.gp_free(exPtr, extracted.length);
check(ex.gp_page_count(exh) === 1, "extracted PDF has exactly 1 page");
ex.gp_close(exh);

// render a page to PNG via the built-in zero-dep rasterizer
const png = readBufferReturning((lp) => ex.gp_render_page(mh, 1, 1.0, lp));
const pngSig = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
check(
  png.length > 8 && pngSig.every((b, i) => png[i] === b),
  `gp_render_page → valid PNG (${png.length} bytes)`,
);

// encrypt with AES-256 (R6) + owner password, then reopen with each password
{
  const pw = argStr("s3cret");
  const owner = argStr("owner-secret");
  const id = argStr("file-id-bytes-01");
  const fek = new Uint8Array(32).fill(0x5a); // stands in for secret host randomness
  const fekPtr = toWasm(fek);
  const enc = readBufferReturning((lp) =>
    ex.gp_save_encrypted(mh, pw.ptr, pw.len, owner.ptr, owner.len, id.ptr, id.len, fekPtr, fek.length, 2, -44, lp),
  );
  check(enc.length > 0 && dec.decode(enc.slice(0, 5)) === "%PDF-", "gp_save_encrypted AES-256 → PDF");
  const ePtr = toWasm(enc);
  const okHandle = ex.gp_open_encrypted(ePtr, enc.length, pw.ptr, pw.len);
  check(okHandle !== 0 && ex.gp_page_count(okHandle) === pageCount, "AES-256 reopened with user password");
  if (okHandle !== 0) ex.gp_close(okHandle);
  // The owner password opens it too (Algorithm 2.A).
  const ownerHandle = ex.gp_open_encrypted(ePtr, enc.length, owner.ptr, owner.len);
  check(ownerHandle !== 0, "AES-256 reopened with owner password");
  if (ownerHandle !== 0) ex.gp_close(ownerHandle);
  // Wrong (empty) password is rejected → null handle.
  const badHandle = ex.gp_open_encrypted(ePtr, enc.length, 0, 0);
  check(badHandle === 0, "wrong password rejected");
  if (badHandle !== 0) ex.gp_close(badHandle);
  // encryption_info reads /P /V /R WITHOUT the password.
  const infoBuf = readBufferReturning((lp) => ex.gp_encryption_info(ePtr, enc.length, lp));
  const info = JSON.parse(dec.decode(infoBuf));
  check(
    info.encrypted === true && info.version === 5 && info.revision === 6 && info.permissions === -44,
    `gp_encryption_info (no password) → ${JSON.stringify(info)}`,
  );
  ex.gp_free(ePtr, enc.length);
  ex.gp_free(fekPtr, fek.length);
  freeArg(pw); freeArg(owner); freeArg(id);
}

// digitally sign with a freshly generated self-signed digital ID
{
  const fields = argStr("Tester\tApproval\tD:20260614120000Z\t260614000000Z\t360614000000Z");
  const rand = new Uint8Array(256).map((_, i) => (i * 53 + 7) & 0xff);
  const rPtr = ex.gp_alloc(rand.length);
  u8().set(rand, rPtr);
  const signed = readBufferReturning((lp) =>
    ex.gp_sign(mh, fields.ptr, fields.len, rPtr, rand.length, 512, lp),
  );
  check(signed.length > 0 && dec.decode(signed.slice(0, 5)) === "%PDF-", "gp_sign → PDF");
  const text = dec.decode(signed);
  check(text.includes("adbe.pkcs7.detached"), "detached signature embedded");
  check(!text.includes("9999999999"), "ByteRange patched");
  ex.gp_free(rPtr, rand.length);
  freeArg(fields);
}

// 9. hyperlinks + outline (table of contents) through the WASM ABI
const lPtr = toWasm(multi);
const lh = ex.gp_open(lPtr, multi.length);
ex.gp_free(lPtr, multi.length);
{
  const uri = argStr("https://giga-pdf.com");
  check(
    ex.gp_add_uri_link(lh, 1, 72, 700, 300, 720, uri.ptr, uri.len) === 0,
    "gp_add_uri_link",
  );
  freeArg(uri);
}
check(ex.gp_add_goto_link(lh, 1, 72, 650, 300, 670, 2) === 0, "gp_add_goto_link → page 2");

// outline: one bookmark per line "level\tpage\ttitle"
{
  const toc = ["0\t1\tChapter 1", "1\t1\tSection 1.1", "1\t2\tSection 1.2", "0\t3\tChapter 2"].join("\n");
  const t = argStr(toc);
  check(ex.gp_set_outline(lh, t.ptr, t.len) === 0, "gp_set_outline (4 bookmarks)");
  freeArg(t);
}

const savedLinks = readBufferReturning((lp) => ex.gp_save(lh, lp));
ex.gp_close(lh);
const slPtr = toWasm(savedLinks);
const lh2 = ex.gp_open(slPtr, savedLinks.length);
ex.gp_free(slPtr, savedLinks.length);

const links = JSON.parse(
  dec.decode(readBufferReturning((lp) => ex.gp_links_json(lh2, 1, lp))),
);
check(links.length === 2, `gp_links_json lists 2 links (${links.length})`);
check(
  links.some((l) => l.kind === "uri" && l.uri === "https://giga-pdf.com"),
  "URI link survived round-trip",
);
check(
  links.some((l) => l.kind === "page" && l.page === 2),
  "internal link resolves to page 2",
);

const outline = JSON.parse(
  dec.decode(readBufferReturning((lp) => ex.gp_outline_json(lh2, lp))),
);
check(outline.length === 4, `gp_outline_json lists 4 items (${outline.length})`);
check(
  outline[1].title === "Section 1.1" && outline[1].level === 1,
  "nested bookmark level preserved",
);
check(outline[3].page === 3, "bookmark destination page preserved");
ex.gp_close(lh2);

// 10. conversions & compression
const isZip = (b) => b.length > 4 && b[0] === 0x50 && b[1] === 0x4b && b[2] === 3 && b[3] === 4;

// fresh simple-text handle (the original `handle` was closed at line 72)
const convPtr = toWasm(pdf);
const convH = ex.gp_open(convPtr, pdf.length);
ex.gp_free(convPtr, pdf.length);
check(convH !== 0, "reopened simple-text for conversions");

const txt = dec.decode(readBufferReturning((lp) => ex.gp_to_text(convH, lp)));
check(txt.length > 0 && /\S/.test(txt), `gp_to_text → non-empty text (${txt.trim().length} chars)`);

const html = dec.decode(readBufferReturning((lp) => ex.gp_to_html(convH, lp)));
check(html.startsWith("<!DOCTYPE html") && html.includes("<span"), "gp_to_html → positioned HTML");

// office formats are ZIP containers of editable XML (not page images)
const odt = readBufferReturning((lp) => ex.gp_to_odt(mh, lp));
check(isZip(odt) && dec.decode(odt.slice(30, 38)) === "mimetype", `gp_to_odt → ODT zip (${odt.length} b)`);

const docx = readBufferReturning((lp) => ex.gp_to_docx(mh, lp));
check(isZip(docx), `gp_to_docx → DOCX zip (${docx.length} b)`);

const pptx = readBufferReturning((lp) => ex.gp_to_pptx(mh, lp));
check(isZip(pptx), `gp_to_pptx → PPTX zip (${pptx.length} b)`);

const xlsx = readBufferReturning((lp) => ex.gp_to_xlsx(convH, lp));
check(isZip(xlsx), `gp_to_xlsx → XLSX zip (${xlsx.length} b)`);

const ods = readBufferReturning((lp) => ex.gp_to_ods(convH, lp));
check(isZip(ods) && dec.decode(ods.slice(30, 38)) === "mimetype", `gp_to_ods → ODS zip (${ods.length} b)`);
ex.gp_close(convH);

// compression round-trips to a valid, re-openable PDF
const comp = readBufferReturning((lp) => ex.gp_save_compressed(mh, lp));
check(dec.decode(comp.slice(0, 5)) === "%PDF-", `gp_save_compressed → PDF (${comp.length} b)`);
const cPtr = toWasm(comp);
const ch = ex.gp_open(cPtr, comp.length);
ex.gp_free(cPtr, comp.length);
check(ch !== 0 && ex.gp_page_count(ch) === pageCount, "compressed PDF re-opens with same pages");
if (ch !== 0) ex.gp_close(ch);

// 11. font subsystem: catalog + Google Fonts URL + embedding + add_text
const catalog = JSON.parse(dec.decode(readBufferReturning((lp) => ex.gp_font_catalog_json(lp))));
check(catalog.length >= 1000, `gp_font_catalog_json → ${catalog.length} families`);
check(catalog.some((f) => f.family === "Roboto" && f.google), "catalog has Roboto (google)");

{
  const fam = argStr("Open Sans");
  const url = dec.decode(readBufferReturning((lp) => ex.gp_font_request_url(fam.ptr, fam.len, 700, 1, lp)));
  check(
    url === "https://fonts.googleapis.com/css2?family=Open+Sans:ital,wght@1,700&display=swap",
    "gp_font_request_url → CSS2 URL",
  );
  freeArg(fam);

  const css = argStr("src:url(https://fonts.gstatic.com/s/x/y.ttf) format('truetype')");
  const fontUrl = dec.decode(readBufferReturning((lp) => ex.gp_parse_css_font_url(css.ptr, css.len, lp)));
  check(fontUrl === "https://fonts.gstatic.com/s/x/y.ttf", "gp_parse_css_font_url → gstatic url");
  freeArg(css);
}

const needed = JSON.parse(dec.decode(readBufferReturning((lp) => ex.gp_needed_fonts(mh, lp))));
check(Array.isArray(needed), `gp_needed_fonts → array (${needed.length} non-embedded)`);

// Embed a real TTF (host-downloaded equivalent) and add selectable text with it.
const ttf = readFileSync("/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf");
{
  const fam = argStr("Liberation Sans");
  const ttfPtr = toWasm(ttf);
  const fontObj = ex.gp_embed_font(mh, fam.ptr, fam.len, ttfPtr, ttf.length);
  ex.gp_free(ttfPtr, ttf.length);
  freeArg(fam);
  check(fontObj > 0, `gp_embed_font → object #${fontObj}`);

  const txt = argStr("Embedded Aé 123");
  const rc = ex.gp_add_text(mh, 1, 72, 700, 18, txt.ptr, txt.len, fontObj, 0x0040c0);
  freeArg(txt);
  check(rc === 0, "gp_add_text with embedded font → 0");

  // OCR end-to-end: add large clean text in the embedded font (so the rasterizer
  // produces real glyphs), then recognize it. Accuracy is ~0.7 so assert the
  // pipeline locates words + produces text, not an exact string match.
  const big = argStr("HELLO");
  ex.gp_add_text(mh, 1, 72, 600, 46, big.ptr, big.len, fontObj, 0x000000);
  freeArg(big);
  const ocr = JSON.parse(dec.decode(readBufferReturning((lp) => ex.gp_ocr_json(mh, 1, 3, lp))));
  const ocrText = ocr.map((w) => w.text).join(" ");
  check(ocr.length > 0 && /\S/.test(ocrText), `gp_ocr_json reads rendered text ("${ocrText.slice(0, 24)}")`);

  const out = readBufferReturning((lp) => ex.gp_save(mh, lp));
  const outStr = dec.decode(out);
  check(outStr.includes("CIDFontType2") && outStr.includes("Identity-H"), "embedded Type0 font in output");
  check(outStr.includes("FontFile2"), "font program embedded (FontFile2)");
  // Re-open + render the page with the embedded font (must not crash).
  const oPtr = toWasm(out);
  const oh = ex.gp_open(oPtr, out.length);
  ex.gp_free(oPtr, out.length);
  check(oh !== 0, "PDF with embedded font re-opens");
  if (oh !== 0) {
    const png2 = readBufferReturning((lp) => ex.gp_render_page(oh, 1, 1.0, lp));
    check(png2.length > 8 && png2[0] === 0x89 && png2[1] === 0x50, `renders with embedded font (${png2.length} b PNG)`);
    ex.gp_close(oh);
  }
}

// 12. reverse conversions: <format> → PDF (+ to_rtf forward)
const rtf = readBufferReturning((lp) => ex.gp_to_rtf(mh, lp));
check(dec.decode(rtf.slice(0, 6)) === "{\\rtf1", `gp_to_rtf → RTF (${rtf.length} b)`);

const reopenPdf = (bytes, label) => {
  check(dec.decode(bytes.slice(0, 5)) === "%PDF-", `${label} → PDF (${bytes.length} b)`);
  const pp = toWasm(bytes);
  const hh = ex.gp_open(pp, bytes.length);
  ex.gp_free(pp, bytes.length);
  check(hh !== 0 && ex.gp_page_count(hh) >= 1, `${label} re-opens (${hh ? ex.gp_page_count(hh) : 0} pages)`);
  let text = "";
  if (hh !== 0) {
    text = dec.decode(readBufferReturning((lp) => ex.gp_to_text(hh, lp)));
    ex.gp_close(hh);
  }
  return text;
};

{
  const t = argStr("First paragraph here.\nSecond paragraph.\nThird line of text.");
  const pdf = readBufferReturning((lp) => ex.gp_txt_to_pdf(t.ptr, t.len, lp));
  freeArg(t);
  const text = reopenPdf(pdf, "gp_txt_to_pdf");
  check(text.includes("Second paragraph"), "txt→PDF preserves text");
}
{
  const rtfPtr = toWasm(rtf);
  const pdf = readBufferReturning((lp) => ex.gp_rtf_to_pdf(rtfPtr, rtf.length, lp));
  ex.gp_free(rtfPtr, rtf.length);
  reopenPdf(pdf, "gp_rtf_to_pdf");
}
{
  const docxPtr = toWasm(docx);
  const pdf = readBufferReturning((lp) => ex.gp_office_to_pdf(docxPtr, docx.length, lp));
  ex.gp_free(docxPtr, docx.length);
  const text = reopenPdf(pdf, "gp_office_to_pdf(docx)");
  check(text.length > 0, "docx→PDF carries text");
}
{
  const h = argStr("<html><body><p>Hello world</p><p>Second &amp; line</p></body></html>");
  const pdf = readBufferReturning((lp) => ex.gp_html_to_pdf(h.ptr, h.len, lp));
  freeArg(h);
  const text = reopenPdf(pdf, "gp_html_to_pdf");
  check(text.includes("Second & line"), "html→PDF preserves escaped text");
}

// 12c. page-size presets + full page control (per-side margins, running footer)
{
  const nm = argStr("A4");
  const outPtr = ex.gp_alloc(16);
  const ok = ex.gp_page_size(nm.ptr, nm.len, outPtr, outPtr + 8);
  freeArg(nm);
  const pw = dv().getFloat64(outPtr, true);
  const ph = dv().getFloat64(outPtr + 8, true);
  ex.gp_free(outPtr, 16);
  check(
    ok === 1 && Math.abs(pw - 595.28) < 0.1 && Math.abs(ph - 841.89) < 0.1,
    `gp_page_size("A4") → ${pw.toFixed(1)}×${ph.toFixed(1)}pt`
  );

  const body = argStr("<div>" + "<p>line</p>".repeat(120) + "</div>");
  const footer = argStr('<div style="text-align:center">Page {{page}} / {{pages}}</div>');
  const pdf = readBufferReturning((lp) =>
    ex.gp_html_render_opts(
      body.ptr,
      body.len,
      0,
      0, // no fonts
      pw,
      ph,
      48,
      36,
      48,
      36, // margins t/r/b/l
      0,
      0, // no header
      footer.ptr,
      footer.len,
      18,
      18, // header/footer offsets
      1, // start page number
      lp
    )
  );
  freeArg(body);
  freeArg(footer);
  check(
    dec.decode(pdf.subarray(0, 5)) === "%PDF-" && pdf.length > 400,
    `gp_html_render_opts → A4 PDF w/ footer (${pdf.length} b)`
  );
}

// 12d. SVG → native vector paths on a page
{
  const svg = argStr(
    '<svg viewBox="0 0 10 10"><rect width="10" height="10" fill="#0088cc"/>' +
      '<circle cx="5" cy="5" r="3" fill="none" stroke="red" stroke-width="1"/></svg>'
  );
  const rc = ex.gp_add_svg(mh, 1, svg.ptr, svg.len, 50, 50, 100, 100);
  freeArg(svg);
  check(rc === 0, `gp_add_svg → native vector on page 1 (rc=${rc})`);
}

// 13. PDF/A archival metadata
{
  const pdfa = readBufferReturning((lp) => ex.gp_to_pdfa(mh, lp));
  const s = dec.decode(pdfa);
  check(s.startsWith("%PDF-"), `gp_to_pdfa → PDF (${pdfa.length} b)`);
  check(s.includes("pdfaid:part>2") && s.includes("GTS_PDFA1"), "PDF/A-2b XMP + OutputIntent present");
  check(s.includes("/OutputIntents") && s.includes("/DestOutputProfile"), "sRGB ICC OutputIntent wired");
  const pp = toWasm(pdfa);
  const hh = ex.gp_open(pp, pdfa.length);
  ex.gp_free(pp, pdfa.length);
  check(hh !== 0 && ex.gp_page_count(hh) === pageCount, "PDF/A re-opens with same pages");
  if (hh !== 0) ex.gp_close(hh);
}

// 14. structured text + full-text search
{
  const lines = JSON.parse(dec.decode(readBufferReturning((lp) => ex.gp_structured_text_json(mh, 1, lp))));
  check(Array.isArray(lines) && lines.length > 0, `gp_structured_text_json → ${lines.length} lines w/ bounds`);
  const word = dec.decode(readBufferReturning((lp) => ex.gp_to_text(mh, lp))).trim().split(/\s+/)[0] || "";
  if (word) {
    const q = argStr(word);
    const hits = JSON.parse(dec.decode(readBufferReturning((lp) => ex.gp_search_json(mh, q.ptr, q.len, 1, lp))));
    freeArg(q);
    check(hits.length > 0 && hits[0].page >= 1 && typeof hits[0].x === "number", `gp_search_json finds "${word}" (${hits.length} hits)`);
  }
}

ex.gp_close(mh);

console.log(failures === 0 ? "\nALL GREEN" : `\n${failures} FAILURE(S)`);
process.exit(failures === 0 ? 0 : 1);
