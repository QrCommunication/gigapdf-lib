# API reference

> Calling from TypeScript/JavaScript? Use the high-level SDK and its complete
> per-method reference in **[SDK.md](SDK.md)**. This file documents the two
> lower-level surfaces the SDK is built on.

Two surfaces expose the same engine:

- **Rust** — `gigapdf_core::Document` (+ free functions in `gigapdf_core::convert`).
- **WebAssembly** — flat `extern "C"` `gp_*` exports in `gigapdf_wasm`.

Conventions for the WASM ABI (see [USAGE.md](USAGE.md)): a *handle* is the opaque
pointer from `gp_open*`; *buffer-returning* functions take a trailing
`out_len: *mut usize`, return a data pointer (or null on error), and the host
frees both; string/byte arguments are passed as `(ptr, len)`; `rgb` is packed
`0xRRGGBB`; mutating ops return `0` on success, negative on error.

## Lifecycle

| Rust | WASM | Notes |
|------|------|-------|
| `Document::open(&[u8]) -> Result<Document>` | `gp_open(ptr,len) -> handle` | 0/Err on failure |
| `Document::open_with_password(&[u8],&[u8])` | `gp_open_encrypted(ptr,len,pw,pwlen)` | decrypts |
| `doc.save() -> Vec<u8>` | `gp_save(handle,outlen)` | renumbering serializer |
| `doc.save_compressed()` | `gp_save_compressed(handle,outlen)` | Flate uncompressed streams |
| `doc.save_encrypted(pw,owner,id0,key,algo,perms)` | `gp_save_encrypted(handle,pw,pwlen,owner,ownerlen,id,idlen,key,keylen,algo,perms,outlen)` | algo 0=RC4-128, 1=AES-128, 2=AES-256; `key`=secret host randomness (AES-256) |
| — | `gp_close(handle)` | free the document |
| — | `gp_alloc(len)` / `gp_free(ptr,len)` | linear-memory management |
| `doc.page_count() -> usize` | `gp_page_count(handle)` | |

## Content editing

| Rust | WASM |
|------|------|
| `page_text_runs(page) -> Vec<TextRun>` | `gp_text_runs_json(handle,page,outlen)` |
| `page_elements(page) -> Vec<ContentElement>` | `gp_elements_json(handle,page,outlen)` |
| `element_at(page,x,y) -> Option<usize>` | `gp_element_at(handle,page,x,y)` |
| `replace_text_run(page,i,&str)` (font-aware: re-encodes Type0/Identity-H runs through the font's char→glyph map) | `gp_replace_text(handle,page,i,ptr,len)` |
| `remove_text_run(page,i)` / `remove_element(page,i)` | `gp_remove_element(handle,page,i)` |
| `move_element(page,i,dx,dy)` | `gp_move_element(handle,page,i,dx,dy)` |
| `duplicate_element(page,i)` | `gp_duplicate_element(handle,page,i)` |
| `add_rectangle(page,x,y,w,h,stroke,fill,lw,opacity)` | `gp_add_rectangle(...)` |
| `add_line(page,x1,y1,x2,y2,stroke,lw,opacity)` | `gp_draw_line(...)` |
| `add_ellipse(page,cx,cy,rx,ry,stroke,fill,lw,opacity)` | `gp_add_ellipse(...)` |
| `add_polygon(page,&pts,close,stroke,fill,lw,opacity)` | `gp_add_polygon(...)` |
| `add_path(page,svg,ox,oy,stroke,fill,lw,opacity)` (SVG path, Y-flipped) | `gp_add_path(...)` |
| `add_image(page,&data,x,y,w,h,opacity)` (PNG/JPEG, alpha) | `gp_add_image(...)` |
| `add_svg(page,src,x,y,w,h)` (full SVG → **native vector**, fits viewBox to the box) | `gp_add_svg(...)` · SDK `addSvg` |

## Fonts & real text

| Rust | WASM |
|------|------|
| `add_text_standard(page,x,y,size,text,font_name,rgb,opacity,rot)` | `gp_add_text_standard(handle,page,x,y,size,ptr,len,fontptr,fontlen,rgb,opacity,rot)` |
| `embed_font(family,&bytes) -> u32` (glyf TrueType **or** OpenType-CFF, auto-detected; `embed_truetype_font` is a kept alias) | `gp_embed_font(handle,famptr,famlen,ttfptr,ttflen) -> u32` |
| `add_text(page,x,y,size,text,font_obj,rgb)` | `gp_add_text(handle,page,x,y,size,ptr,len,font_obj,rgb)` |
| `embedded_fonts() -> Vec<EmbeddedFontInfo>` | `gp_embedded_fonts_json(handle,outlen)` |
| `extract_font_program(name) -> Option<(Vec<u8>,fmt)>` | `gp_extract_font(handle,nameptr,namelen,outlen)` |
| `needed_fonts() -> Vec<String>` | `gp_needed_fonts(handle,outlen)` (JSON) |
| `font::catalog::lookup(name)` / `CATALOG` | `gp_font_catalog_json(outlen)` |
| `font::google::css_url(family,weight,italic)` | `gp_font_request_url(famptr,famlen,weight,italic,outlen)` |
| `font::google::parse_css_font_url(css)` | `gp_parse_css_font_url(cssptr,csslen,outlen)` |

Three complementary ways to draw real, selectable text — no host font files needed:

1. **Base-14 standard fonts** — `add_text_standard` with a PostScript name
   (`Helvetica`/`Times`/`Courier` × 4 styles, `Symbol`, `ZapfDingbats`). No
   embedding; every viewer ships them. Several different standard fonts can
   coexist on one page.
2. **Any family via embedding** — `embed_font` accepts **any** outline program
   and auto-detects the flavour: a glyf `.ttf` → Type0 / CIDFontType2 +
   `FontFile2`; an OpenType-CFF `.otf` (`OTTO`) → Type0 / CIDFontType0 +
   `FontFile3` `/Subtype /OpenType`. Both are Identity-H with full `/W` widths
   and a `/ToUnicode` CMap; `add_text` then writes text in it. Feed it a Google
   Font the host fetched (`font::google::css_url` → download → embed) or any
   `.ttf`/`.otf`. (`embed_truetype_font` remains as an alias.)
3. **The document's own embedded fonts** — `embedded_fonts` lists the faces a PDF
   already carries (`{base_font, format}`); `extract_font_program` pulls a font's
   raw bytes out. `truetype` (glyf) and full OpenType `cff` (`OTTO`) re-embed
   directly with `embed_font`; bare `cff` (Type1C) and `type1` are read-only.
   `add_text`/`replace_text_run` resolve the char→glyph map from `FontFile2`
   **or** `FontFile3`, so you can re-bake edited text in the exact original face.

## Annotations & forms

| Rust | WASM |
|------|------|
| `add_highlight/underline/strike_out(page,rect,rgb)` | `gp_add_highlight/_underline/_strike_out` |
| `add_free_text(page,rect,text,size,rgb)` | `gp_add_free_text(...)` |
| `add_square_annotation/add_line_annotation` | `gp_add_square/gp_add_line` |
| `add_ink(page,paths,rgb,lw)` / `add_stamp` | `gp_add_ink / gp_add_stamp` |
| `page_annotations(page)` | `gp_annotations_json(handle,page,outlen)` |
| `remove_annotation(page,i)` | `gp_remove_annotation(handle,page,i)` |
| `flatten_annotations(page) -> usize` | `gp_flatten_annotations(handle,page)` |
| `form_fields() -> Vec<FormField>` | `gp_fields_json(handle,outlen)` |
| `set_text_field/set_checkbox/set_radio/set_choice_field` | `gp_set_text_field / _checkbox / _radio / _choice` |
| `add_text_field(page,name,rect,value,max_len,multiline,password,&style)` | `gp_add_text_field(handle,page,name*,rect…,value*,max_len,multiline,password,style…)` |
| `add_checkbox(page,name,rect,checked,export,&style)` | `gp_add_checkbox(handle,page,name*,rect…,checked,export*,style…)` |
| `add_radio_group(page,name,&[(export,rect)],selected,&style)` | `gp_add_radio_group(handle,page,name*,exports*,rects*,selected*,style…)` |
| `add_combo_box(page,name,rect,&options,selected,editable,&style)` | `gp_add_combo_box(handle,page,name*,rect…,options*,selected*,editable,style…)` |
| `add_list_box(page,name,rect,&options,selected,multi,&style)` | `gp_add_list_box(handle,page,name*,rect…,options*,selected*,multi,style…)` |

`FieldStyle { font_size, color, border, background, border_width }` controls the
new field's appearance. In the WASM ABI it is passed as the 7 trailing scalars
`style… = font_size, color_rgb, border_rgb, has_border, bg_rgb, has_bg,
border_width`; `exports`/`options` are newline-separated, `rects` is a
comma-separated flat list of `4 × N` numbers (one rect per radio option). Every
created widget gets a real `/AP` appearance stream and the form is flagged
`NeedAppearances` so values display immediately and survive later edits.

## Pages, links, outline, metadata

| Rust | WASM |
|------|------|
| `rotate_page(page,deg)` / `delete_page(page)` / `move_page(from,to)` | `gp_rotate_page / gp_delete_page / gp_move_page` |
| `extract_pages(&[u32]) -> Vec<u8>` | `gp_extract_pages(handle,ptr,count,outlen)` |
| `append_pages_from(&[u8])` | `gp_append_pages(handle,ptr,len)` |
| `add_uri_link(page,rect,uri)` / `add_goto_link(page,rect,target)` | `gp_add_uri_link / gp_add_goto_link` |
| `add_named_dest(name,target)` / `named_dests() -> Vec<(String,u32)>` | `gp_add_named_dest(handle,nameptr,namelen,target) / gp_named_dests_json(handle,outlen)` |
| `add_goto_link_named(page,rect,name)` (jumps to a `/Dest /name`; split-safe) | `gp_add_goto_link_named(handle,page,x0,y0,x1,y1,nameptr,namelen)` |
| `page_links(page)` | `gp_links_json(handle,page,outlen)` |
| `set_outline(&[(title,page,level)])` / `outline_items()` | `gp_set_outline(handle,ptr,len) / gp_outline_json` |
| `get_metadata(key)` / `set_metadata(key,val)` | `gp_get_metadata / gp_set_metadata` |

## Security

| Rust | WASM |
|------|------|
| `redact_region(page,x,y,w,h,cover:Option<[f64;3]>) -> usize` | `gp_redact_region(handle,page,x,y,w,h,cover_rgb,has_cover)` |
| `sign(&Signer,name,reason,date) -> Result<Vec<u8>>` | `gp_sign(handle,fieldsptr,fieldslen,randptr,randlen,key_bits,outlen)` |
| `sign_p12(&Pkcs12Identity,name,reason,date,location,contact)` | `gp_sign_p12(handle,p12*,pass*,fields*,outlen)` |
| `sign::pkcs12::parse(pfx,password) -> Pkcs12Identity` | (via `gp_sign_p12`) |
| `save_encrypted(...)` | `gp_save_encrypted(...)` |

`Signer` is built from host-supplied randomness; `sign` produces a self-signed
`adbe.pkcs7.detached` CMS signature with a `/ByteRange`-patched PDF. `sign_p12`
signs with a **user-supplied identity** imported natively from a PKCS#12
(`.p12`/`.pfx`) — PBES2 (PBKDF2 + AES) and PBES1 (3DES, RC2-40) bags, integrity
MAC verified — with **no third-party crypto** (all in `crate::crypto`).

## Render

| Rust | WASM |
|------|------|
| `render_page(page,scale) -> Vec<u8>` (PNG) | `gp_render_page(handle,page,scale,outlen)` |
| `raster::encode_png(w,h,&rgba) -> Vec<u8>` | `gp_rgba_to_png(w,h,ptr,len,outlen)` · SDK `rgbaToPng` (native RGBA→PNG, no `canvas`) |

## Text intelligence & OCR

| Rust | WASM | Notes |
|------|------|-------|
| `structured_text(page) -> Vec<TextLine>` | `gp_structured_text_json(handle,page,outlen)` | reading-order lines + bounds |
| `search(query,case_insensitive) -> Vec<SearchMatch>` | `gp_search_json(handle,ptr,len,ci,outlen)` | match lines + highlight boxes |
| `ocr_page(page,scale) -> Vec<OcrWord>` | `gp_ocr_json(handle,page,scale,outlen)` | scanned pages → words + boxes (PDF space) |
| `ocr_page_text(page,scale) -> String` | `gp_ocr_text(handle,page,scale,outlen)` | scanned page → plain text |

OCR uses the built-in recognizer (no Tesseract): Otsu → connected components →
line/word segmentation → an MLP trained offline on EMNIST (handwriting) + synthetic
font glyphs (printed + accented Latin). Use `scale ≥ 2.0` for small text. For pages
that already carry a text layer, `structured_text` / `search` are exact and faster.

## Conversions

### PDF → X (forward)

| Rust (`Document`) | WASM | Output |
|------|------|--------|
| `to_text() -> String` | `gp_to_text(handle,outlen)` | UTF-8 |
| `to_html() -> String` | `gp_to_html(handle,outlen)` | positioned HTML + inline images |
| `to_docx() -> Vec<u8>` | `gp_to_docx(handle,outlen)` | editable Word |
| `to_pptx() -> Vec<u8>` | `gp_to_pptx(handle,outlen)` | one slide/page |
| `to_odp() -> Vec<u8>` | `gp_to_odp(handle,outlen)` | OpenDocument Presentation |
| `to_odt() -> Vec<u8>` | `gp_to_odt(handle,outlen)` | OpenDocument Text |
| `to_xlsx() -> Vec<u8>` | `gp_to_xlsx(handle,outlen)` | tables → cells, prose → text |
| `to_ods() -> Vec<u8>` | `gp_to_ods(handle,outlen)` | OpenDocument Spreadsheet |
| `convert::office::to_xlsx_named(grids,&names)` / `to_ods_named` (pure; host-built `Vec<Vec<Vec<String>>>` grid + sheet names) | `gp_grids_to_xlsx(grids_json,glen,names_json,nlen,outlen)` / `gp_grids_to_ods(…)` · SDK `gridsToXlsx`/`gridsToOds` | emit `.xlsx`/`.ods` from a caller's own table grid (`string[][][]` JSON + `string[]` names) — no Document needed |
| `convert::office::xlsx_to_grids(&bytes) -> Vec<(String,Vec<Vec<String>>)>` (inverse; inline + shared strings) | `gp_xlsx_to_grids(ptr,len,outlen)` (JSON `[{name,rows}]`) · SDK `xlsxToGrids` | read an `.xlsx` back into per-sheet name + rows grids |
| `to_rtf() -> Vec<u8>` | `gp_to_rtf(handle,outlen)` | RTF |
| `to_pdfa() -> Vec<u8>` | `gp_to_pdfa(handle,outlen)` | PDF/A-2b metadata |

### X → PDF (reverse, stateless)

| Rust (`convert::reverse`) | WASM |
|------|------|
| `txt_to_pdf(&str)` | `gp_txt_to_pdf(ptr,len,outlen)` |
| `html_to_pdf(&str)` | `gp_html_to_pdf(ptr,len,outlen)` |
| `rtf_to_pdf(&str)` | `gp_rtf_to_pdf(ptr,len,outlen)` |
| `office_to_pdf(&[u8]) -> Option<Vec<u8>>` | `gp_office_to_pdf(ptr,len,outlen)` (auto-detect docx/odt/odp/pptx/xlsx/ods) |
| `docx_to_pdf / odt_to_pdf / odp_to_pdf / pptx_to_pdf / xlsx_to_pdf / ods_to_pdf` | via `gp_office_to_pdf` |

### Building blocks (Rust)

- `convert::build::PdfBuilder` — from-scratch PDF (pages, positioned text in
  standard-14 fonts, rectangles).
- `convert::zip::{ZipWriter, read_zip}` — ZIP container read/write.
- `convert::table::reconstruct(&[PlacedText])` — heuristic row/column grid.
- `convert::style::parse_base_font(&str)` — recover family/weight/style.
- `filters::deflate::{deflate, flate_encode}` — DEFLATE/zlib encoder.

## HTML / CSS → PDF (with JavaScript)

A native renderer (no headless browser). Text is set in **host-downloaded
Google fonts**, so the host fetches fonts in two phases.

| Rust (`html`) | ABI / SDK | Notes |
|------|------|------|
| `needed_fonts(html) -> Vec<FontRequest>` | `gp_html_needed_fonts` · `htmlNeededFonts` | phase 1: fonts to download (after running `<script>`s) |
| `needed_fonts_with(html, header, footer)` | `gp_html_needed_fonts_ex` · `htmlNeededFontsWith` | phase 1 incl. the header/footer fonts |
| `render(html, &[ProvidedFont], page_w, page_h, margin) -> Vec<u8>` | `gp_html_render` · `htmlRender` | phase 2: HTML+CSS → PDF (uniform margin) |
| `render_with(html, &[ProvidedFont], &RenderOptions) -> Vec<u8>` | `gp_html_render_opts` · `htmlRenderWith` | phase 2 with size, per-side margins, header/footer, numbering |
| `page_size(name) -> Option<(f64,f64)>` | `gp_page_size` · `pageSize` | resolve `"A4"`/`"a3-landscape"`/`"letter"`… → points |

- **Page setup** (`render_with` / `RenderOptions`): named or explicit size,
  per-side margins, and a **running header/footer** painted in the page margins
  with `{{page}}` / `{{pages}}` substitution and `start_page_number`. See
  [`HTML-CSS.md` §1](HTML-CSS.md#1-page-setup).
- **Layout**: block / inline / table / **flex** (`flex-direction`,
  `justify-content`, `flex-grow`) / **grid** (`grid-template-columns`), selector
  cascade (`tag`/`.class`/`#id`/`*`, descendant), pagination.
- **Page breaks**: CSS `page-break-before|after: always`, `break-before|after:
  page`, or a `<pagebreak>` element / `class="page-break"` — forces the next
  content onto a new page.
- **Exhaustive reference**: every supported HTML element, CSS property, length
  unit, colour and selector is listed in [`HTML-CSS.md`](HTML-CSS.md).
- **JavaScript** (`js` module): inline `<script>`s execute **before layout** via
  a zero-dependency engine; `js::run_inline_scripts(html) -> String` does it
  standalone (the renderer calls it automatically). `js::Interp::eval_source(src)
  -> Result<Value, String>` evaluates a snippet.
  - Language: classes + `super`, closures, destructuring, spread, optional
    chaining, template literals, `RegExp`, `Map`/`Set`, `Symbol`, `eval`/
    `Function`. `function*`/`async` bodies compile to a **suspendable bytecode
    VM**: lazy/infinite generators, bidirectional `.next(v)`, `yield*`,
    spec-ordered `async`/`await`, and full control flow across a suspension
    (`try/catch/finally`, `for…of`/`for…in`, `switch`, labels, destructuring,
    spread).
  - Built-ins: `Object`/`Array`/`String`/`Number`/`Boolean`/`Math`/`JSON`/
    `console`/`Map`/`Set`/`RegExp` (+ a backtracking regex engine)/`Error`,
    `parseInt`/`parseFloat`/`setTimeout`/`queueMicrotask`.
  - DOM: `document.getElementById`/`getElementsByTagName`/`querySelector(All)`
    (combinators `>`/`+`/`~`, attribute selectors), `createElement`/
    `createTextNode`, and on elements `textContent`/`innerHTML`/`getAttribute`/
    `setAttribute`/`appendChild`/`removeChild`/`classList`/`style`/`children`.

## Data types

- `ContentElement { index, kind: Text|Image|Path, label, bounds, font, color }`
- `TextRun { index, operator, text, op_position }`
- `FormField { name, field_type, value, flags, options, max_len }`
- `Link { kind: uri|page, uri, page, rect }`, `OutlineItem { title, page, level }`
- `convert::{ConvPage, PlacedText, PlacedImage, PlacedShape, TextStyle, Generic}`

JSON-returning WASM functions serialize these structures directly.
