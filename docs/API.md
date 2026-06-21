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
| `page_text_elements(page) -> Vec<TextElementInfo>` (rich per-run text: bounds + family/bold/italic + size + colour + rotation + direction) | `gp_text_elements_json(handle,page,outlen)` |
| `document_language() -> DocumentLanguage` (dominant direction + script + ISO-639-1) | `gp_document_language(handle,outlen)` → `{direction,script,lang?}` · SDK `documentLanguage` |
| `page_image_elements(page) -> Vec<ImageElementInfo>` (placement bounds + format + embeddable bytes + pixel dims + rotation + opacity) | `gp_image_elements_json(handle,page,outlen)` → `[{…,format,pixelWidth,pixelHeight,dataBase64,rotation,opacity}]` |
| `page_vector_paths(page) -> Vec<VectorPath>` (painted paths: segments + bounds + fill/stroke RGB + line width + alpha + dash; **the returned RGB resolves the page's colour spaces — Device & CIE families *and* **named** `/Separation` · `/ICCBased` · `/Indexed` · `/DeviceN` (resolved against `/Resources/ColorSpace`, applying the `/Separation` tint transform, `/ICCBased` `/N`, and the `/Indexed` palette) — so spot/ICC fills come back as their true RGB instead of a grey approximation; **v0.58.2**) | `gp_vector_paths_json(handle,page,outlen)` → `[{…,segments,fill,stroke,strokeWidth,fillAlpha,strokeAlpha,dash}]` |
| `element_at(page,x,y) -> Option<usize>` | `gp_element_at(handle,page,x,y)` |
| `replace_text_run(page,i,&str)` (font-aware: re-encodes Type0/Identity-H runs through the font's char→glyph map) | `gp_replace_text(handle,page,i,ptr,len)` |
| `remove_text_run(page,i)` / `remove_element(page,i)` | `gp_remove_element(handle,page,i)` |
| `move_element(page,i,dx,dy)` | `gp_move_element(handle,page,i,dx,dy)` |
| `transform_element(page,i,[a,b,c,d,e,f])` (affine generalisation of `move_element`; wraps the element in `q a b c d e f cm … Q` — move/resize/rotate, non-destructive, same for text/image/path) | `gp_transform_element(handle,page,i,a,b,c,d,e,f)` · SDK `transformElement(page,i,m)` |
| `reorder_element(page,i,to_front)` (native z-order: splices the element's op range to the end (`to_front=true` → on top) or start (behind), re-wrapped in `q … Q` with the element's effective graphics state — fill/stroke colour, line width, dash, font — re-emitted inside it so it keeps its appearance; text/image/path. The unified index changes after the splice — re-read elements) | `gp_reorder_element(handle,page,i,to_front)` · SDK `reorderElement(page,i,toFront)` |
| `set_path_style(page,i,&PathStyle)` (path elements only; wraps the op range in `q … Q` and injects override ops before the paint: `fill`→`rg`, `stroke`→`RG`, `strokeWidth`→`w`, `dash`→`d`. `fillAlpha`/`strokeAlpha` **applied** via a page `/ExtGState` `/ca`/`/CA` + `/<gs> gs` in the wrap) | `gp_set_path_style_json(handle,page,i,json_ptr,json_len)` · SDK `setPathStyle(page,i,style)`; `PathStyle = {fill?,stroke?:[r,g,b] 0..=1, strokeWidth?, fillAlpha?, strokeAlpha?, dash?:number[]}` |
| `set_element_opacity(page,i,fill_alpha)` (constant opacity on **any** element — text/image/shape; registers `/ExtGState` `/ca`=`/CA`=`fill_alpha` (`0..=1`, auto-named `GpGs<n>`) + `/<gs> gs` in a `q … Q` wrap. The image-alpha path; shapes may instead use `set_path_style` for independent fill/stroke alpha) | `gp_set_element_opacity(handle,page,i,fill_alpha)` · SDK `setElementOpacity(page,i,fillAlpha)` |
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
| `add_text_standard_styled(…,rot,underline,strike)` (base-14 + decoration rules) | `gp_add_text_standard_styled(handle,page,x,y,size,ptr,len,fontptr,fontlen,rgb,opacity,rot,underline,strike)` · SDK `addStandardText(…,opts)` |
| `embed_font(family,&bytes) -> u32` (glyf TrueType **or** OpenType-CFF, auto-detected; `embed_truetype_font` is a kept alias) | `gp_embed_font(handle,famptr,famlen,ttfptr,ttflen) -> u32` |
| `add_text(page,x,y,size,text,font_obj,rgb)` | `gp_add_text(handle,page,x,y,size,ptr,len,font_obj,rgb)` |
| `add_text_styled(…,rot,underline,strike)` (embedded font + decoration rules) | `gp_add_text_styled(handle,page,x,y,size,ptr,len,font_obj,rgb,opacity,rot,underline,strike)` · SDK `addText(…,opts)` |
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
| `page_annotations(page)` (rich: author/subject/dates/colour/opacity/quadPoints/inkList/stamp name/link target) | `gp_annotations_json(handle,page,outlen)` |
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
| `resize_page(page,w,h)` / `add_page(w,h,after)` / `copy_page(page)` / `page_info(page)` | `gp_resize_page / gp_add_page / gp_copy_page / gp_page_info_json` |
| `extract_pages(&[u32]) -> Vec<u8>` | `gp_extract_pages(handle,ptr,count,outlen)` |
| `append_pages_from(&[u8])` | `gp_append_pages(handle,ptr,len)` |
| `page_margins(page)` / `set_page_margins(page,t,r,b,l)` | `gp_page_margins(handle,page,outlen) / gp_set_page_margins(handle,page,t,r,b,l)` |
| `set_header(spec)` / `set_footer(spec)` (JSON `HeaderFooterSpec`, `{{page}}`/`{{pages}}` tokens) | `gp_set_header(handle,ptr,len) / gp_set_footer(handle,ptr,len)` |
| `remove_headers()` / `remove_footers()` / `header_footer()` (reader) | `gp_remove_headers / gp_remove_footers / gp_header_footer(handle,outlen)` |
| `add_uri_link(page,rect,uri)` / `add_goto_link(page,rect,target)` | `gp_add_uri_link / gp_add_goto_link` |
| `add_named_dest(name,target)` / `named_dests() -> Vec<(String,u32)>` | `gp_add_named_dest(handle,nameptr,namelen,target) / gp_named_dests_json(handle,outlen)` |
| `add_goto_link_named(page,rect,name)` (jumps to a `/Dest /name`; split-safe) | `gp_add_goto_link_named(handle,page,x0,y0,x1,y1,nameptr,namelen)` |
| `page_links(page)` | `gp_links_json(handle,page,outlen)` |
| `set_outline(&[(title,page,level)])` / `outline_items()` | `gp_set_outline(handle,ptr,len) / gp_outline_json` |
| `get_metadata(key)` / `set_metadata(key,val)` | `gp_get_metadata / gp_set_metadata` |
| `attachments() -> Vec<Attachment>` (embedded files from `/Names /EmbeddedFiles`) | `gp_attachments_json(handle,outlen)` → `[{name,filename,mime,description,creationDate,modDate,dataBase64}]` |

## Security

| Rust | WASM |
|------|------|
| `redact_region(page,x,y,w,h,cover:Option<[f64;3]>) -> usize` (text only; image left intact) | `gp_redact_region(handle,page,x,y,w,h,cover_rgb,has_cover)` · SDK `redact` |
| `redact_pii(page,&[rect], …)` *(v0.52.4)* — **irreversible**: remove text **+ erase image pixels** (safe on scans/OCR) under an opaque mark | (ABI added in v0.52.4) · SDK `redactPii(page, rects)` |
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
| `render_page_no_text(page,scale) -> Vec<u8>` (PNG, page-content text suppressed) | `gp_render_page_no_text(handle,page,scale,outlen)` · SDK `renderPageNoText` (text-free background for editor overlays; vectors/gradients/images/annotations still rendered) |
| `render_page_excluding(page,&indices,scale) -> Vec<u8>` (PNG, omits the given top-level unified element indices — generalises `render_page_no_text`; non-excluded content still renders; exclusion is top-level only) | `gp_render_page_excluding(handle,page,indices_ptr,indices_len,scale,outlen)` · SDK `renderPageExcluding` (background minus specific elements for live-overlay editing; empty list = full page, unknown indices ignored) |
| `raster::encode_png(w,h,&rgba) -> Vec<u8>` | `gp_rgba_to_png(w,h,ptr,len,outlen)` · SDK `rgbaToPng` (native RGBA→PNG, no `canvas`) |
| `raster::resize_rgba(&rgba,sw,sh,dw,dh) -> Vec<u8>` (alpha-correct, separable) | `gp_resize_rgba(ptr,len,sw,sh,dw,dh,outlen)` · SDK `resizeRgba` (no `sharp`) |
| `raster::jpeg::encode_jpeg(w,h,&rgba,quality) -> Vec<u8>` (baseline 4:4:4) | `gp_encode_jpeg(w,h,ptr,len,quality,outlen)` · SDK `encodeJpeg` |
| `raster::jpeg::decode_jpeg(&bytes) -> Option<(w,h,rgba)>` / `raster::decode_png` | `gp_decode_jpeg` / `gp_decode_png(ptr,len,outlen)` → `[w:u32][h:u32][rgba]` · SDK `decodeJpeg`/`decodePng` |
| `raster::webp::encode_webp(w,h,&rgba) -> Vec<u8>` (lossless VP8L) | `gp_encode_webp(w,h,ptr,len,outlen)` · SDK `encodeWebp` |
| `raster::webp::decode_webp(&bytes) -> Option<(w,h,rgba)>` (lossless **VP8L** + lossy **VP8** keyframe; not `VP8X`/animation) | `gp_decode_webp(ptr,len,outlen)` · SDK `decodeWebp` |
| `raster::gif::decode_gif(&bytes) -> Option<(w,h,rgba)>` (first frame) | `gp_decode_gif(ptr,len,outlen)` · SDK `decodeGif` |
| `raster::avif::decode_avif(&bytes) -> Option<(w,h,rgba)>` (AV1 intra still — see matrix) | `gp_decode_avif(ptr,len,outlen)` · SDK `decodeAvif` |

All decoders return a framed `[w:u32 LE][h:u32 LE][rgba]` buffer (8-byte header
the SDK unpacks into `DecodedImage`), `null`/empty on a malformed or unsupported
stream. Every codec is pure-Rust→WASM with **no third-party image library**
(no `sharp`, no `canvas`, no `libwebp`/`libaom`).

### AVIF (AV1 intra) — capability matrix

The AVIF decoder is a from-scratch AV1 intra decoder validated **bit-exact vs
dav1d** on minted fixtures. Supported:

| Area | Status |
|------|--------|
| Container | ISOBMFF still image (`ftyp`/`meta`/`mdat`, primary item) |
| Sequence header | `reduced_still_picture_header` **and** full streaming header (timing/decoder-model/operating-points, frame-id, order-hint feature flags) |
| Frame header | KEY-frame preamble + `disable_frame_end_update_cdf`, quant/segmentation-off/delta-q, tiles |
| Transforms | lossy (DCT/ADST/identity/flip) + lossless (4×4 WHT) |
| Intra prediction | DC, Paeth, Smooth(/V/H), directional Z1/Z2/Z3, CfL, filter-intra |
| Palette | screen-content **palette** mode (§5.11.46-50): Y + chroma, colour cache/delta coding, wave-front index map, skip + residual paths |
| In-loop filters | deblocking (§7.14) + CDEF (§7.15) including multi-strength `cdef_bits > 0` |
| Chroma | 4:2:0 / 4:2:2 / 4:4:4, 8-bit |

Not yet covered (returns wrong pixels or is absent — tracked, see CHANGELOG):
animated AVIF, film grain, loop restoration (§7.17), the fully bit-exact
directional top-right/bottom-left intra edge (real-neighbour gather is in, a
residual Z1/Z3 edge-filter gap remains), and the lossless WHT path at `q ≤ 20`.

## Text intelligence & OCR

| Rust | WASM | Notes |
|------|------|-------|
| `structured_text(page) -> Vec<TextLine>` | `gp_structured_text_json(handle,page,outlen)` | reading-order lines + bounds |
| `search(query,case_insensitive) -> Vec<SearchMatch>` | `gp_search_json(handle,ptr,len,ci,outlen)` | match lines + highlight boxes |
| `ocr_page(page,scale) -> Vec<OcrWord>` | `gp_ocr_json(handle,page,scale,outlen)` | scanned pages → words + boxes (PDF space) |
| `ocr_page_text(page,scale) -> String` | `gp_ocr_text(handle,page,scale,outlen)` | scanned page → plain text |
| `ocr::load_model(&[u8]) -> bool` / `ocr::clear_models()` | `gp_ocr_load_model(ptr,len) / gp_ocr_clear_models()` | host-load a `.gpocr` line model (CRNN+CTC) / reset to mono-glyph · SDK `loadOcrModel` / `clearOcrModels` (+ Node `loadBundledOcrModel(s)` / `loadAllBundledOcrModels`) |

OCR uses the built-in recognizer (no Tesseract): Otsu → connected components →
line/word segmentation → a compact CNN trained offline on EMNIST (handwriting) +
synthetic font glyphs (printed + accented Latin). Use `scale ≥ 2.0` for small text.
For pages that already carry a text layer, `structured_text` / `search` are exact and
faster. A second **line-level CRNN+CTC** recognizer (opt-in via the `ocr-*` Cargo
features — `ocr-alpha` = Latin-extended + Cyrillic + Greek, **trained**;
`ocr-cjk`/`ocr-arabic`/`ocr-deva`/… infra-ready) removes per-glyph segmentation and is
competitive with Tesseract on clean multi-script print. `ocr()` routes to it when a model
is embedded and falls back to the mono-glyph CNN otherwise — same ABI, no signature
change. See [`OCR_ARCHITECTURE.md`](./OCR_ARCHITECTURE.md),
[`OCR_TRAINING_DATA.md`](./OCR_TRAINING_DATA.md) and
[`OCR_TRAINING_LOG.md`](./OCR_TRAINING_LOG.md).

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
| `image_to_pdf(&[u8]) -> Option<Vec<u8>>` | `gp_image_to_pdf(ptr,len,outlen)` (auto-detect **PNG/JPEG/GIF/WebP/AVIF**; one A4 page, image centred & shrink-to-fit, never upscaled; GIF/WebP/AVIF transcoded to PNG before embed; PNG covers every color-type 0/2/3/4/6, bit-depths 1/2/4/8/16, Adam7 interlacing, transparency via `/SMask`. `null`/empty if the format is unrecognized) |

### Unified editable model (lower / edit / raise)

A format-neutral document tree (`model::Document`, JSON-serialized). Lower any
format into it, edit with `ModelOp`s, raise to any format — see
[SDK.md § The unified editable model](SDK.md#the-unified-editable-model).

| Rust (`model`) | WASM | SDK |
|------|------|-----|
| `Document::from_pdf(&doc) -> model::Document` | `gp_model_from_pdf(handle,outlen)` | `doc.toModel()` |
| `model::from_office(&[u8]) -> Option<Document>` | `gp_model_from_office(ptr,len,outlen)` | `officeToModel` |
| `model::from_html(&str) -> Document` | `gp_model_from_html(ptr,len,outlen)` | `htmlToModel` |
| `model.apply_ops(&[ModelOp]) -> Document` | `gp_model_apply_ops(modelptr,modellen,opsptr,opslen,outlen)` | `applyModelOps` |
| `model.to_{docx,xlsx,pptx,odt,ods,odp,pdf}() -> Vec<u8>` | `gp_model_to_{docx,xlsx,pptx,odt,ods,odp,pdf}(ptr,len,outlen)` | `modelTo{Docx,…}` |
| `model.to_{html,rtf}() -> String` | `gp_model_to_{html,rtf}(ptr,len,outlen)` | `modelToHtml` / `modelToRtf` |

All model functions take/return the model's JSON envelope as a string. A
`ModelOp` addresses a block by `[section, page, index]` (zero-based); ops run in
order and out-of-range addresses are no-ops.

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
| `needed_resources(html, header, footer) -> Vec<ResourceNeed>` | `gp_html_needed_resources` · `htmlNeededResources` | phase 1 (unified): fonts **and** external `<img src>` images to fetch |
| `render(html, &[ProvidedFont], page_w, page_h, margin) -> Vec<u8>` | `gp_html_render` · `htmlRender` | phase 2: HTML+CSS → PDF (uniform margin) |
| `render_with(html, &[ProvidedFont], &RenderOptions) -> Vec<u8>` | `gp_html_render_opts` · `htmlRenderWith` | phase 2 with size, per-side margins, header/footer, numbering |
| `page_size(name) -> Option<(f64,f64)>` | `gp_page_size` · `pageSize` | resolve `"A4"`/`"a3-landscape"`/`"letter"`… → points |

- **Page setup** (`render_with` / `RenderOptions`): named or explicit size,
  per-side margins, and a **running header/footer** painted in the page margins
  with `{{page}}` / `{{pages}}` substitution and `start_page_number`. See
  [`HTML-CSS.md` §1](HTML-CSS.md#1-page-setup).
- **External images** (`RenderOptions.resources` / `needed_resources`): the
  engine is **zero-network**, so list every external resource with
  `needed_resources` (fonts + `http(s)` `<img>` URLs), have the host fetch each,
  and pass image bytes back via `RenderOptions.resources` (a `url → bytes` map).
  `data:` image URIs are inlined automatically and need no entry — this is the
  native replacement for a headless browser's autonomous resource loading.
- **Layout**: block / inline / table / **flex** (`flex-direction`,
  `justify-content`, `flex-grow`) / **grid** (`grid-template-columns`), selector
  cascade (`tag`/`.class`/`#id`/`*`, descendant), pagination.
- **Page breaks**: CSS `page-break-before|after: always`, `break-before|after:
  page`, or a `<pagebreak>` element / `class="page-break"` — forces the next
  content onto a new page.
- **Exhaustive reference**: every supported HTML element, CSS property, length
  unit, colour and selector is listed in [`HTML-CSS.md`](HTML-CSS.md).
- **JavaScript** (`js` module): inline `<script>`s execute **before layout** on
  the embedded **Boa** engine; `js::run_inline_scripts(html) -> String` does it
  standalone (the renderer calls it automatically), and `js::eval(src) -> String`
  evaluates a snippet.
  - Language: Boa is a full ES2021+ engine, so classes + `super`, closures,
    destructuring, spread, optional chaining, template literals, `RegExp`,
    `Map`/`Set`, `Symbol`, lazy/infinite generators (`yield*`, bidirectional
    `.next(v)`) and spec-ordered `async`/`await` all work, alongside the usual
    `Object`/`Array`/`String`/`Number`/`Math`/`JSON`/`console` built-ins.
  - DOM: a JavaScript polyfill (built over `crate::html::dom`) provides
    `document.getElementById`/`getElementsByTagName`/`querySelector(All)`
    (combinators `>`/`+`/`~`, attribute selectors), `createElement`/
    `createTextNode`, and on elements `textContent`/`innerHTML`/`getAttribute`/
    `setAttribute`/`appendChild`/`removeChild`/`classList`/`style`/`children`.

## Data types

- `ContentElement { index, kind: Text|Image|Path, label, bounds, font, color }`
- `TextRun { index, operator, text, op_position }`
- `TextElementInfo { index, text, bounds, font_family, bold, italic, size, color, rotation, direction }`
- `FormField { name, field_type, value, flags, options, max_len }`
- `Link { kind: uri|page, uri, page, rect }`, `OutlineItem { title, page, level }`
- `HeaderFooterSpec { text, align, font_size, color, page_range, show_on_first_page, band_height }`
- `model::{Document, Section, Page, Block, Inline, CharStyle, CellValue, ModelOp, BlockAddr, StylePatch}`
- `convert::{ConvPage, PlacedText, PlacedImage, PlacedShape, TextStyle, Generic}`

JSON-returning WASM functions serialize these structures directly.
