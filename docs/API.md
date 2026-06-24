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
| `doc.save() -> Vec<u8>` | `gp_save(handle,outlen)` | renumbering serializer (classic xref table) |
| `doc.save_compressed()` | `gp_save_compressed(handle,outlen)` | Flate uncompressed streams (classic xref table) |
| `doc.save_optimized(object_streams,xref_streams)` | `gp_save_optimized(handle,object_streams,xref_streams,outlen)` | PDF 1.5+ **object streams** (`/ObjStm`) + **cross-reference stream** (`/XRef`), ISO 32000-1 §7.5.7/§7.5.8 — the most compact output; `object_streams` implies `xref_streams`. Linearization (Fast Web View, Annex F) is **not** done |
| `doc.save_encrypted(pw,owner,id0,key,algo,perms)` | `gp_save_encrypted(handle,pw,pwlen,owner,ownerlen,id,idlen,key,keylen,algo,perms,outlen)` | algo 0=RC4-128, 1=AES-128, 2=AES-256; `key`=secret host randomness (AES-256) |
| `doc.save_encrypted_ex(pw,owner,id0,key,algo,perms,encrypt_metadata)` | (via `gp_change_passwords`) | as `save_encrypted` but also controls `/EncryptMetadata` (ISO 32000-1 §7.6.3.2) |
| `doc.change_passwords(user,owner,id0,key,algo,perms,encrypt_metadata)` | `gp_change_passwords(handle,user,userlen,owner,ownerlen,id,idlen,key,keylen,algo,perms,encrypt_metadata,outlen)` | re-encrypt an opened doc with new passwords (discards the prior `/Encrypt`) |
| `doc.remove_encryption() -> Vec<u8>` | `gp_remove_encryption(handle,outlen)` | strip encryption → plaintext PDF |
| `doc.encrypt_for_recipients(&[cert_der],perms,aes256,encrypt_metadata,seed20,rng_seed)` | `gp_encrypt_for_recipients(handle,certs,certslen,lens,lenscount,perms,aes256,encrypt_metadata,seed,seedlen,rng,rnglen,outlen)` | **public-key** (certificate) encryption, `/Filter /Adobe.PubSec` — CMS-enveloped seed per X.509 recipient (ISO 32000-1 §7.6.5); `seed20`≥20B + `rng_seed`≥32B host randomness |
| `Document::open_with_private_key(&[u8],cert_der,key_der)` | `gp_open_with_private_key(ptr,len,cert,certlen,key,keylen)` | open a public-key-encrypted PDF with a recipient DER cert + PKCS#1 RSA key |
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
| `set_text_run_style(page,i,&[Span])` (per-character-run restyle of text element `i`: each span sets the style of the `[start,end)` UTF-16 slice — bold/italic/underline/strike/colour/sizePt; the run is split so the rest keeps its style and the **original glyph codes/`TJ` kerning are sliced & re-emitted, not re-encoded**, so positioning is preserved; each styled slice wrapped in `q … Q`) | `gp_set_text_run_style_json(handle,page,i,json_ptr,json_len)` · SDK `setTextRunStyle(page,i,spans)`; `Span = {start,end,color?:[r,g,b], sizePt?, bold?, italic?, underline?, strike?}` |
| `set_element_opacity(page,i,fill_alpha)` (constant opacity on **any** element — text/image/shape; registers `/ExtGState` `/ca`=`/CA`=`fill_alpha` (`0..=1`, auto-named `GpGs<n>`) + `/<gs> gs` in a `q … Q` wrap. The image-alpha path; shapes may instead use `set_path_style` for independent fill/stroke alpha) | `gp_set_element_opacity(handle,page,i,fill_alpha)` · SDK `setElementOpacity(page,i,fillAlpha)` |
| `duplicate_element(page,i)` | `gp_duplicate_element(handle,page,i)` |
| `add_rectangle(page,x,y,w,h,stroke,fill,lw,opacity)` | `gp_add_rectangle(...)` |
| `add_line(page,x1,y1,x2,y2,stroke,lw,opacity)` | `gp_draw_line(...)` |
| `add_ellipse(page,cx,cy,rx,ry,stroke,fill,lw,opacity)` | `gp_add_ellipse(...)` |
| `add_polygon(page,&pts,close,stroke,fill,lw,opacity)` | `gp_add_polygon(...)` |
| `add_gradient(page,&GradientSpec)` (linear/radial shading — type 2/3 — painted as a `PatternType 2` shading pattern over a rect; colour stops → a type-2/type-3 PDF function, ISO 32000-1 §8.7.4/§8.7.3) | `gp_add_gradient(handle,page,kind,coords*,coordcount,offsets*,colors*,stopcount,rx,ry,rw,rh,extstart,extend,opacity)` |
| `add_filled_rectangle(page,[x,y,w,h],&Color,opacity)` (rect fill in **any** colour space — DeviceCMYK/Gray/spot `Separation`/`ICCBased`, ISO 32000-1 §8.6) | `gp_add_filled_rectangle(handle,page,x,y,w,h,kind,comps*,compcount,name*,namelen,profile*,proflen,opacity)` |
| `add_filled_polygon(page,&points,&Color,opacity)` (≥ 3-vertex polygon fill, any colour space) | `gp_add_filled_polygon(handle,page,points*,pointcount,kind,comps*,compcount,name*,namelen,profile*,proflen,opacity)` |
| `add_text_color(page,x,y,size,text,font,&Color,opacity,rot,underline,strike)` (base-14 text in any colour space) | `gp_add_text_color(handle,page,x,y,size,text*,textlen,font*,fontlen,kind,comps*,compcount,name*,namelen,profile*,proflen,opacity,rot,underline,strike)` |
| `set_overprint(page,fill,stroke,mode)` (overprint `/ExtGState` `/op`·`/OP`·`/OPM` for subsequent content, ISO 32000-1 §8.6.7) | `gp_set_overprint(handle,page,fill,stroke,mode)` |
| `add_output_intent(&profile,condition)` (document `OutputIntent` embedding an ICC profile, `/S /GTS_PDFX`, ISO 32000-1 §8.6.3 — decoupled from PDF/A) | `gp_add_output_intent(handle,profile*,proflen,condition*,condlen)` |
| `add_path(page,svg,ox,oy,stroke,fill,lw,opacity)` (SVG path, Y-flipped) | `gp_add_path(...)` |
| `add_image(page,&data,x,y,w,h,opacity)` (PNG/JPEG, alpha) | `gp_add_image(...)` |
| `add_image_watermark(&data,&pages,anchor,dx,dy,w,h,rot,opacity,tile)` (decode PNG/JPEG/WebP/GIF/AVIF once, reference on every target page; anchor/offset/size/rotate/opacity, optional tiling) | `gp_add_image_watermark(handle,data*,pages*,anchor,dx,dy,w,h,rot,opacity,tile)` · SDK `addImageWatermark(data,opts)` |
| `add_svg(page,src,x,y,w,h)` (full SVG → **native vector**, fits viewBox to the box) | `gp_add_svg(...)` · SDK `addSvg` |
| `flatten_form_xobjects(page) -> usize` (inline & **de-share** page form XObjects so their text becomes ordinary editable runs — distinct from `flatten_form`, which flattens AcroForm fields) | `gp_flatten_form_xobjects(handle,page)` · SDK `flattenFormXObjects` |

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
| `add_circle_annotation(page,rect,stroke?,fill?,lw)` / `add_caret_annotation(page,rect,rgb)` | `gp_add_circle_annot / gp_add_caret_annot` |
| `add_polygon_annotation(page,verts,stroke?,fill?,lw)` / `add_polyline_annotation(page,verts,rgb,lw)` (closed / open path through `(x,y)` vertices) | `gp_add_polygon_annot / gp_add_polyline_annot` (flat `f64` coords) |
| `add_ink(page,paths,rgb,lw)` / `add_stamp` | `gp_add_ink / gp_add_stamp` |
| `regenerate_appearance(page,index)` (rebuild an existing annotation's `/AP /N` from its geometry — Square/Circle/Line/Polygon/PolyLine/Highlight/Underline/StrikeOut/Ink/Caret) | `gp_regenerate_appearance(handle,page,index)` |
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
| `add_signature_field(page,name,rect,&style)` (visible `/FT /Sig` widget for the signing pipeline; sets `/SigFlags`) | `gp_add_signature_field(handle,page,name*,rect…,style…)` |
| `set_field_action(name,FieldTrigger,js)` (field-level JS in `/AA`: `Keystroke`=K / `Format`=F / `Validate`=V / `Calculate`=C) | `gp_set_field_script(handle,name*,trigger*,js*)` (`1`/`0`/`-2`) |
| `set_calculation_order(&[name])` (AcroForm `/CO`) / `remove_field(name) -> bool` / `regenerate_field_appearance(name) -> bool` (rebuild `/AP` for text/choice/checkbox) | `gp_set_calculation_order(handle,names*)` / `gp_remove_field(handle,name*)` / `gp_regenerate_field_appearance(handle,name*)` |

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
| `page_boxes(page) -> PageBoxes` / `set_page_box(page,kind,[x0,y0,x1,y1])` (all five ISO 32000-1 boxes: Media/Crop/Bleed/Trim/Art; inheritance + per-box defaults applied on read; siblings preserved on write) | `gp_page_boxes_json(handle,page,outlen) / gp_set_page_box(handle,page,kind,x0,y0,x1,y1)` (`kind` 0=media 1=crop 2=bleed 3=trim 4=art) |
| `page_labels() -> Vec<PageLabelRange>` / `set_page_labels(&[PageLabelRange])` (`/PageLabels` number tree, ISO 32000-1 §12.4.2; empty slice clears) and `page_label(page) -> String` (resolved viewer label, e.g. `iv`, `A-3`) | `gp_page_labels_json(handle,outlen)` → `[{startPage,style,prefix,startNumber}]` / `gp_set_page_labels(handle,ptr,len)` (lines `startPage⇥style⇥startNumber⇥prefix`, style `D r R a A` or `-`) / `gp_page_label(handle,page,outlen)` |
| `set_header(spec)` / `set_footer(spec)` (JSON `HeaderFooterSpec`, `{{page}}`/`{{pages}}` tokens) | `gp_set_header(handle,ptr,len) / gp_set_footer(handle,ptr,len)` |
| `remove_headers()` / `remove_footers()` / `header_footer()` (reader) | `gp_remove_headers / gp_remove_footers / gp_header_footer(handle,outlen)` |
| `add_uri_link(page,rect,uri)` / `add_goto_link(page,rect,target)` | `gp_add_uri_link / gp_add_goto_link` |
| `add_link(page,rect,&Action)` (any action: GoTo with every fit mode, GoToR, URI, Named, Launch, JavaScript, SubmitForm, ResetForm — `Action::from_json`) | `gp_add_link(handle,page,x0,y0,x1,y1,actionptr,actionlen)` (JSON action; `-2` malformed) |
| `set_open_action(&Action)` (document `/OpenAction`) / `remove_link(page,index) -> bool` | `gp_set_open_action(handle,ptr,len)` / `gp_remove_link(handle,page,index)` (`1`/`0`/`-1`) |
| `add_named_dest(name,target)` / `named_dests() -> Vec<(String,u32)>` | `gp_add_named_dest(handle,nameptr,namelen,target) / gp_named_dests_json(handle,outlen)` |
| `add_goto_link_named(page,rect,name)` (jumps to a `/Dest /name`; split-safe) | `gp_add_goto_link_named(handle,page,x0,y0,x1,y1,nameptr,namelen)` |
| `page_links(page)` | `gp_links_json(handle,page,outlen)` |
| `set_outline(&[(title,page,level)])` / `set_bookmarks(&[Bookmark])` (bookmarks carrying any `Action`) / `outline_items()` | `gp_set_outline(handle,ptr,len)` / `gp_set_bookmarks(handle,ptr,len)` (lines `level⇥title⇥actionJson`) / `gp_outline_json` |
| `get_metadata(key)` / `set_metadata(key,val)` | `gp_get_metadata / gp_set_metadata` |
| `xmp() -> Option<Vec<u8>>` / `set_xmp(&[u8])` (catalog `/Metadata` XMP packet) and `info_fields() -> InfoFields` / `set_info(&InfoFields)` (typed Info-dict fields; `set_info` writes **both** `/Info` and a synced XMP packet, partial-merge) | `gp_get_xmp(handle,outlen)` / `gp_set_xmp(handle,ptr,len)` / `gp_set_info_json(handle,ptr,len)` (`{title?,author?,subject?,keywords?,creator?,producer?,creationDate?,modDate?}`) |
| `attachments() -> Vec<Attachment>` (embedded files from `/Names /EmbeddedFiles`) | `gp_attachments_json(handle,outlen)` → `[{name,filename,mime,description,creationDate,modDate,dataBase64}]` |
| `add_attachment(name,bytes,mime?,desc?)` / `add_associated_file(name,bytes,mime?,desc?,rel)` (`/AF` PDF/A-3 — Factur-X/ZUGFeRD) / `remove_attachment(name) -> bool` / `add_file_attachment_annot(page,rect,name,icon?)` | `gp_add_attachment(handle,nameptr,namelen,bytesptr,byteslen,mimeptr,mimelen,descptr,desclen)` / `gp_add_associated_file(…,rel)` (`rel` 0=source 1=data 2=alternative 3=supplement 4=unspecified) / `gp_remove_attachment(handle,nameptr,namelen)` (1=removed 0=absent) / `gp_add_file_attachment_annot(handle,page,x0,y0,x1,y1,nameptr,namelen,iconptr,iconlen)` |

## Security

| Rust | WASM |
|------|------|
| `redact_region(page,x,y,w,h,cover:Option<[f64;3]>) -> usize` (text only; image left intact) | `gp_redact_region(handle,page,x,y,w,h,cover_rgb,has_cover)` · SDK `redact` |
| `redact_pii(page,&[rect], …)` *(v0.52.4)* — **irreversible**: remove text **+ erase image pixels** (safe on scans/OCR) under an opaque mark | `gp_redact_pii(handle,page,rects_ptr,rects_count,cover_rgb,has_cover)` · SDK `redactPii(page, rects)` |
| `save_encrypted(...)` (default **AES-256 R6**) | `gp_save_encrypted(...)` |
| `permissions_to_p(8 flags) -> i32` / `permissions_from_p(p) -> 8 flags` (ISO 32000-1 Table 22) | `gp_permissions_to_p(…)` / `gp_permissions_from_p(p,outlen)` · SDK `permissionsToP`/`decodePermissions`/`getPermissions` |

### Digital signatures

Four signature levels, increasing in long-term assurance. All produce a CMS
(`SignedData`) embedded in a `/Sig` field, with a `/ByteRange`-patched PDF and
**no third-party crypto** (everything in `crate::crypto`/`crate::sign`).

| Level | Rust | WASM | SDK | Network |
|-------|------|------|-----|---------|
| **B (self-signed)** — ephemeral digital ID, `adbe.pkcs7.detached` | `sign(&Signer,name,reason,date)` | `gp_sign(handle,fields*,rand*,key_bits,outlen)` | `sign(fields, random, keyBits?)` | none |
| **B (PKCS#12)** — user CA/eIDAS identity, `adbe.pkcs7.detached` | `sign_p12(&Pkcs12Identity, …)` | `gp_sign_p12(handle,p12*,pass*,fields*,outlen)` | `signP12(p12, password, opts?)` | none |
| **B-T (PAdES)** — RFC 3161 trusted timestamp in the SignerInfo (`ETSI.CAdES.detached`, `signing-certificate-v2`, `id-aa-timeStampToken`) | `sign_prepare_tsa(…)` → host POST → `sign_finish_tsa(token)` | `gp_sign_prepare_tsa(…)` / `gp_sign_finish_tsa(handle,token*,outlen)` | `signTimestamped(opts)` *(async)* | 1× TSA |
| **B-LT / B-LTA (PAdES-LTV)** — B-T + `/DSS` (`/Certs`+`/OCSPs`+`/CRLs`+`/VRI`); B-LTA adds a `/DocTimeStamp` over the whole file | `ltv_targets(pdf,nonce)` → host OCSP/CRL fetch → `apply_dss(pdf,certs,ocsps,crls)`; archive: `doc_timestamp_prepare` → host POST → `doc_timestamp_finish` | `gp_ltv_targets(pdf*,nonce*,outlen)` / `gp_apply_dss(pdf*,certs*,ocsps*,crls*,outlen)` / `gp_doc_timestamp_prepare(handle,pdf*,nonce*,outlen)` / `gp_doc_timestamp_finish(handle,token*,outlen)` | `signLtv(opts)` *(async)* | 1× TSA + 1 OCSP/CRL per cert (+ 1× TSA if archive) |
| **Certify (DocMDP)** — a certifying signature + `/Perms /DocMDP` and a `/Reference` transform; `docmdp_p` = 1 (no changes) / 2 (fill+sign) / 3 (also annotate) | `sign_certify(&Signer,name,reason,date,docmdp_p)` | `gp_sign_certify(handle,fields*,rand*,key_bits,docmdp_p,outlen)` | `certify(fields, random, docmdpLevel, keyBits?)` | none |

**Verification** (ISO 32000-1 §12.8.1) — the inverse of the signing stack:

| Rust | WASM | SDK |
|------|------|-----|
| `signatures() -> Vec<SignatureInfo>` (list `/Sig` fields: name/reason/location/date/subFilter/byteRange) | `gp_signatures_json(handle,outlen)` | `signatures()` |
| `verify_signatures(&pdf_bytes) -> Vec<SignatureReport>` (per signature: ByteRange digest, CMS `messageDigest`, RSA SignerInfo signature, whole-file coverage, signer CN) | `gp_verify_signatures(handle,pdf*,outlen)` | `verifySignatures(pdfBytes)` |

Verification recomputes the SHA-256 over the `/ByteRange` and checks it against the
embedded CMS `messageDigest` (`digestOk`), then validates the SignerInfo RSA
signature under the signer certificate's key (`signatureOk`); `coversWholeDocument`
flags whether anything was appended after the signature. **RSA + SHA-256** (what
this engine produces) is verified; other algorithms are reported `unsupported`.
Verification needs the **original file bytes** (the `Document` doesn't retain
them) — pass the same bytes you opened. Live OCSP/CRL revocation, full
chain-to-trusted-root and ECDSA are out of scope.

- `Signer` is built from host-supplied randomness; the self-signed `sign`
  produces a self-signed `adbe.pkcs7.detached` CMS signature.
- `sign_p12` signs with a **user-supplied identity** imported natively from a
  PKCS#12 (`.p12`/`.pfx`) — PBES2 (PBKDF2 + AES) and PBES1 (3DES, RC2-40) bags,
  integrity MAC verified.
- **Host-fetch model (2 phases).** Timestamping/LTV require HTTP the WASM core
  can't perform, so the engine emits the request bytes and the host POSTs them:
  `gp_sign_prepare_tsa` returns the DER `TimeStampReq` → host POSTs it to the TSA
  (`application/timestamp-query`) → `gp_sign_finish_tsa` embeds the
  `TimeStampResp`. LTV adds `gp_ltv_targets` (which OCSP/CRL URLs to fetch, taken
  **from the certificates' AIA / CRL-DP**) → host fetches → `gp_apply_dss`. The
  SDK's `signTimestamped`/`signLtv` orchestrate this with the global `fetch`.
- **SSRF note.** OCSP/CRL/TSA URLs are **host-supplied** (from the certificate
  extensions for LTV); the engine performs no allow-listing. A host that exposes
  signing to untrusted input MUST validate these URLs — pass
  `tsaFetch`/`revocationFetch`/`crlFetch` to inject an allow-list, auth or proxy.

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
| `page_blocks(page) -> Vec<Block>` | `gp_page_blocks_json(handle,page,outlen)` | **per-page** structural reconstruction (paragraphs/headings/lists/tables/shapes/images) in reading order; each text run keeps its `source_index` back to the editable operator. The streaming counterpart of `from_pdf`/`toModel` for a virtualized editor · SDK `pageBlocks` |
| `search(query,case_insensitive) -> Vec<SearchMatch>` | `gp_search_json(handle,ptr,len,ci,outlen)` | match lines + highlight boxes |
| _(OCR removed from core/WASM)_ | — | OCR is host-side: **`gigapdf-ocr-rten`** crate (PaddleOCR PP-OCR on pure-Rust RTen, 13 langs + auto script selection). See [`crates/ocr-rten/README.md`](../crates/ocr-rten/README.md) |

OCR is **not** in the pure-`std` core/WASM. It runs host-side in the **`gigapdf-ocr-rten`**
crate — PaddleOCR PP-OCR (DBNet detect + SVTR/CRNN recognize) on the pure-Rust **RTen**
runtime (no C++, no Tesseract), 13 languages incl. Hebrew + automatic per-line script
selection. API: `OcrEngine::ocr_pdf_page(&Document, page, scale) -> Vec<OcrWord>` (boxes in
PDF user space) / `recognize_page(&img)`. For pages that already carry a text layer,
`structured_text` / `search` are exact and faster. See
[`OCR_ARCHITECTURE.md`](./OCR_ARCHITECTURE.md) and
[`../crates/ocr-rten/README.md`](../crates/ocr-rten/README.md).

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
| `to_tagged(pdf_ua) -> Vec<u8>` | `gp_to_tagged(handle,pdf_ua,outlen)` | **tagged (accessible) PDF** — `/StructTreeRoot` + marked content + `/MarkInfo` + `/Lang` + `/RoleMap` + `/Alt` on figures, **without** PDF/A (ISO 32000-1 §14.7/§14.8). `pdf_ua` stamps the PDF/UA-1 identifier (ISO 14289) |

### X → PDF (reverse, stateless)

| Rust (`convert::reverse`) | WASM |
|------|------|
| `txt_to_pdf(&str)` | `gp_txt_to_pdf(ptr,len,outlen)` |
| `html_to_pdf(&str)` | `gp_html_to_pdf(ptr,len,outlen)` |
| `rtf_to_pdf(&str)` | `gp_rtf_to_pdf(ptr,len,outlen)` |
| `office_to_pdf(&[u8]) -> Option<Vec<u8>>` | `gp_office_to_pdf(ptr,len,outlen)` (auto-detect docx/odt/odp/pptx/xlsx/ods) |
| `docx_to_pdf / odt_to_pdf / odp_to_pdf / pptx_to_pdf / xlsx_to_pdf / ods_to_pdf` | via `gp_office_to_pdf` |
| `office_needed_fonts(&[u8]) -> Option<Vec<FontRequest>>` (phase 1: families the container **references but doesn't embed** — host fetches each `url`→TTF) | `gp_office_needed_fonts(ptr,len,outlen)` · SDK `officeNeededFonts` |
| `office_to_pdf_with_fonts(&[u8],&[ProvidedFont]) -> Vec<u8>` (phase 2: render with the host-fetched fonts embedded; the container's own embedded faces win on conflict) | `gp_office_to_pdf_with_fonts(office*,fonts*,outlen)` · SDK `officeToPdfWith` |
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
| `model::from_md(&str) -> Document` (CommonMark-ish: headings, lists, GFM tables, fenced code) | `gp_model_from_md(ptr,len,outlen)` | `mdToModel` |
| `model::from_csv(&[u8]) -> Option<Document>` (RFC 4180; auto `,`/`;`/tab/`|` delimiter → one editable table) | `gp_model_from_csv(ptr,len,outlen)` | `csvToModel` |
| `model.apply_ops(&[ModelOp]) -> Document` | `gp_model_apply_ops(modelptr,modellen,opsptr,opslen,outlen)` | `applyModelOps` |
| `model.to_{docx,xlsx,pptx,odt,ods,odp,pdf,epub}() -> Vec<u8>` | `gp_model_to_{docx,xlsx,pptx,odt,ods,odp,pdf,epub}(ptr,len,outlen)` | `modelTo{Docx,…,Epub}` |
| `model.to_{html,rtf,md,csv}() -> String` | `gp_model_to_{html,rtf,md,csv}(ptr,len,outlen)` | `modelToHtml` / `modelToRtf` / `modelToMarkdown` / `modelToCsv` |

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
- `FormField { name, field_type, value, flags, options, max_len }`, `FieldKind` (enum `Text|Checkbox|Radio|PushButton|ComboBox|ListBox|Signature|Unknown`), and `FieldTrigger` (enum `Keystroke|Format|Validate|Calculate` — the `/AA` JavaScript event; `FieldTrigger::from_name` parses the SDK's lowercase name)
- `SignatureInfo { field_name, signer_name, reason, location, date, sub_filter, byte_range: [i64;4] }` (listing) and `SignatureReport { field_name, byte_range_ok, digest_ok, signature_ok, covers_whole_document, signer_common_name, cert_count, algorithm }` (verification verdict, ISO 32000-1 §12.8.1)
- `GradientSpec { kind: GradientKind, stops: Vec<GradientStop>, rect: [f64;4], extend: (bool,bool), opacity }` with `GradientKind` (enum `Linear { x0,y0,x1,y1 }` | `Radial { x0,y0,r0,x1,y1,r1 }`) and `GradientStop { offset (0..1), color: [f64;3] }` — authored gradients (ISO 32000-1 §8.7.4 shadings)
- `Color` (enum) — a fill/stroke colour in any space: `Rgb([f64;3])` · `Cmyk([f64;4])` · `Gray(f64)` · `Separation { name, tint, cmyk: [f64;4] }` (spot ink with its `DeviceCMYK` tint transform) · `IccBased { components: Vec<f64>, profile: Vec<u8> }`. Components are `0.0..=1.0`. Across the ABI a colour is `(kind, comps[], name, profile)` — `kind` `0` rgb / `1` cmyk / `2` gray / `3` separation (`comps=[tint,c,m,y,k]`) / `4` icc
- `Link { kind: uri|page, uri, page, rect }`, `OutlineItem { title, page, level }`, `Bookmark { title, level, action: Option<Action> }`
- `Action` (ISO 32000-1 §12.6) and `Destination` (§12.3.2) — the navigation model. `Action::from_json` accepts a tagged object: `{"type":"goto","dest":<Destination>}`, `{"type":"gotoR","file":"…","dest":<Destination>}`, `{"type":"uri","uri":"…"}`, `{"type":"named","action":"nextPage|prevPage|firstPage|lastPage"}`, `{"type":"launch","file":"…"}`, `{"type":"javascript","js":"…"}`, `{"type":"submitForm","url":"…"}`, `{"type":"resetForm"}`. A `Destination` is `{"fit":"xyz","page":N,"left"?,"top"?,"zoom"?}` or `fit` ∈ `fit|fitH|fitV|fitR|fitB|fitBH|fitBV` (with `top`/`left`/`rect` as the mode needs), or `{"fit":"named","name":"…"}`. `page` is 1-based; `GoToR` encodes it as a 0-based integer for the remote file
- `HeaderFooterSpec { text, align, font_size, color, page_range, show_on_first_page, band_height }`
- `PageBox` (enum `Media|Crop|Bleed|Trim|Art`) and `PageBoxes { media, crop, bleed, trim, art: [f64;4], declared: PageBoxesDeclared { media, crop, bleed, trim, art: bool } }` — every rectangle is the **effective** box (ISO 32000-1 §14.11.2 inheritance + the per-box default chain applied: Crop→Media, Bleed/Trim/Art→Crop), reported verbatim; `declared` flags which boxes are explicitly on the page dictionary vs inherited/defaulted
- `PageLabelStyle` (enum `Decimal|RomanLower|RomanUpper|AlphaLower|AlphaUpper|None`) and `PageLabelRange { start_page (1-based), style, prefix: String, start_number }` — one entry per `/PageLabels` range (ISO 32000-1 §12.4.2). `page_label(n)` formats the displayed string (roman/letter sequences, prefix, `St` offset), falling back to the decimal page number outside any range
- `Attachment { name, filename, mime, description, creation_date, mod_date, data }` (read) and `AfRelationship` (enum `Source|Data|Alternative|Supplement|Unspecified`, the filespec `/AFRelationship` for `/AF` associated files) — write via `add_attachment`/`add_associated_file`/`remove_attachment`/`add_file_attachment_annot`
- `InfoFields { title, author, subject, keywords, creator, producer, creation_date, mod_date: Option<String> }` — the standard `/Info` fields shared with the XMP packet (`dc:`/`xmp:`/`pdf:`); `set_info` regenerates XMP from them (PDF dates → ISO 8601), `InfoFields::from_json` parses the SDK object
- `model::{Document, Section, Page, Block, Inline, CharStyle, CellValue, ModelOp, BlockAddr, StylePatch}`
- `convert::{ConvPage, PlacedText, PlacedImage, PlacedShape, TextStyle, Generic}`

JSON-returning WASM functions serialize these structures directly.
