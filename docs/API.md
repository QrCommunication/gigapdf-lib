# API reference

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
| `doc.save_encrypted(pw,id0,perms)` | `gp_save_encrypted(handle,pw,pwlen,id,idlen,perms,outlen)` | RC4 128 |
| — | `gp_close(handle)` | free the document |
| — | `gp_alloc(len)` / `gp_free(ptr,len)` | linear-memory management |
| `doc.page_count() -> usize` | `gp_page_count(handle)` | |

## Content editing

| Rust | WASM |
|------|------|
| `page_text_runs(page) -> Vec<TextRun>` | `gp_text_runs_json(handle,page,outlen)` |
| `page_elements(page) -> Vec<ContentElement>` | `gp_elements_json(handle,page,outlen)` |
| `element_at(page,x,y) -> Option<usize>` | `gp_element_at(handle,page,x,y)` |
| `replace_text_run(page,i,&str)` | `gp_replace_text(handle,page,i,ptr,len)` |
| `remove_text_run(page,i)` / `remove_element(page,i)` | `gp_remove_element(handle,page,i)` |
| `move_element(page,i,dx,dy)` | `gp_move_element(handle,page,i,dx,dy)` |
| `duplicate_element(page,i)` | `gp_duplicate_element(handle,page,i)` |
| `add_rectangle(page,x,y,w,h,stroke,fill,lw)` | `gp_add_rectangle(...)` |
| `add_line(page,x1,y1,x2,y2,rgb,lw)` | `gp_add_line(...)` |

## Fonts & real text

| Rust | WASM |
|------|------|
| `embed_truetype_font(family,&ttf) -> u32` | `gp_embed_font(handle,famptr,famlen,ttfptr,ttflen) -> u32` |
| `add_text(page,x,y,size,text,font_obj,rgb)` | `gp_add_text(handle,page,x,y,size,ptr,len,font_obj,rgb)` |
| `needed_fonts() -> Vec<String>` | `gp_needed_fonts(handle,outlen)` (JSON) |
| `font::catalog::lookup(name)` / `CATALOG` | `gp_font_catalog_json(outlen)` |
| `font::google::css_url(family,weight,italic)` | `gp_font_request_url(famptr,famlen,weight,italic,outlen)` |
| `font::google::parse_css_font_url(css)` | `gp_parse_css_font_url(cssptr,csslen,outlen)` |

`embed_truetype_font` builds a Type0 / CIDFontType2 font (Identity-H, full widths,
`ToUnicode`) from a glyf-based `.ttf`; `add_text` writes real, selectable
content-stream text in that font.

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

## Pages, links, outline, metadata

| Rust | WASM |
|------|------|
| `rotate_page(page,deg)` / `delete_page(page)` / `move_page(from,to)` | `gp_rotate_page / gp_delete_page / gp_move_page` |
| `extract_pages(&[u32]) -> Vec<u8>` | `gp_extract_pages(handle,ptr,count,outlen)` |
| `append_pages_from(&[u8])` | `gp_append_pages(handle,ptr,len)` |
| `add_uri_link(page,rect,uri)` / `add_goto_link(page,rect,target)` | `gp_add_uri_link / gp_add_goto_link` |
| `page_links(page)` | `gp_links_json(handle,page,outlen)` |
| `set_outline(&[(title,page,level)])` / `outline_items()` | `gp_set_outline(handle,ptr,len) / gp_outline_json` |
| `get_metadata(key)` / `set_metadata(key,val)` | `gp_get_metadata / gp_set_metadata` |

## Security

| Rust | WASM |
|------|------|
| `redact_region(page,x,y,w,h,cover:Option<[f64;3]>) -> usize` | `gp_redact_region(handle,page,x,y,w,h,cover_rgb,has_cover)` |
| `sign(&Signer,name,reason,date) -> Result<Vec<u8>>` | `gp_sign(handle,fieldsptr,fieldslen,randptr,randlen,key_bits,outlen)` |
| `save_encrypted(...)` | `gp_save_encrypted(...)` |

`Signer` is built from host-supplied randomness; `sign` produces an
`adbe.pkcs7.detached` CMS signature with a `/ByteRange`-patched PDF.

## Render

| Rust | WASM |
|------|------|
| `render_page(page,scale) -> Vec<u8>` (PNG) | `gp_render_page(handle,page,scale,outlen)` |

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
| `to_odt() -> Vec<u8>` | `gp_to_odt(handle,outlen)` | OpenDocument Text |
| `to_xlsx() -> Vec<u8>` | `gp_to_xlsx(handle,outlen)` | tables → cells, prose → text |
| `to_ods() -> Vec<u8>` | `gp_to_ods(handle,outlen)` | OpenDocument Spreadsheet |
| `to_rtf() -> Vec<u8>` | `gp_to_rtf(handle,outlen)` | RTF |
| `to_pdfa() -> Vec<u8>` | `gp_to_pdfa(handle,outlen)` | PDF/A-2b metadata |

### X → PDF (reverse, stateless)

| Rust (`convert::reverse`) | WASM |
|------|------|
| `txt_to_pdf(&str)` | `gp_txt_to_pdf(ptr,len,outlen)` |
| `html_to_pdf(&str)` | `gp_html_to_pdf(ptr,len,outlen)` |
| `rtf_to_pdf(&str)` | `gp_rtf_to_pdf(ptr,len,outlen)` |
| `office_to_pdf(&[u8]) -> Option<Vec<u8>>` | `gp_office_to_pdf(ptr,len,outlen)` (auto-detect docx/odt/pptx/xlsx/ods) |
| `docx_to_pdf / odt_to_pdf / pptx_to_pdf / xlsx_to_pdf / ods_to_pdf` | via `gp_office_to_pdf` |

### Building blocks (Rust)

- `convert::build::PdfBuilder` — from-scratch PDF (pages, positioned text in
  standard-14 fonts, rectangles).
- `convert::zip::{ZipWriter, read_zip}` — ZIP container read/write.
- `convert::table::reconstruct(&[PlacedText])` — heuristic row/column grid.
- `convert::style::parse_base_font(&str)` — recover family/weight/style.
- `filters::deflate::{deflate, flate_encode}` — DEFLATE/zlib encoder.

## Data types

- `ContentElement { index, kind: Text|Image|Path, label, bounds, font, color }`
- `TextRun { index, operator, text, op_position }`
- `FormField { name, field_type, value, flags, options, max_len }`
- `Link { kind: uri|page, uri, page, rect }`, `OutlineItem { title, page, level }`
- `convert::{ConvPage, PlacedText, PlacedImage, PlacedShape, TextStyle, Generic}`

JSON-returning WASM functions serialize these structures directly.
