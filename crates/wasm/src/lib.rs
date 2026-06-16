//! WebAssembly bindings for gigapdf-core — a plain `extern "C"` ABI, no
//! `wasm-bindgen`, so the browser build pulls in zero third-party crates.
//!
//! Memory model: the host (JS) allocates input buffers with [`gp_alloc`], copies
//! bytes in, and calls an entry point. Functions that return a buffer (save,
//! JSON queries) allocate it, write its length through an out-pointer, and
//! return the data pointer; the host copies it out and calls [`gp_free`].
//!
//! A [`Document`] is held behind an opaque pointer obtained from [`gp_open`] and
//! released with [`gp_close`].
//!
//! Every export is the FFI boundary: the host upholds the pointer/length
//! contract documented above, so the raw-pointer dereference lint is allowed
//! crate-wide rather than marking each `#[no_mangle]` export `unsafe` (which
//! would not change the wasm ABI seen by the JS host).

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use gigapdf_core::{
    Annotation, ContentElement, Document, ElementKind, EmbeddedFontInfo, FieldKind, FormField,
    Layer, Link, LinkTarget, OcrWord, OutlineItem, SearchMatch, TextLayerRun, TextLine, TextRun,
};

// ─── raw memory management ───────────────────────────────────────────────────

/// Allocate `len` zeroed bytes in the wasm linear memory; returns the pointer.
#[no_mangle]
pub extern "C" fn gp_alloc(len: usize) -> *mut u8 {
    let mut buffer = vec![0u8; len];
    let ptr = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    ptr
}

/// Free a buffer previously returned by [`gp_alloc`] / a buffer-returning call.
///
/// # Safety
/// `ptr`/`len` must come from this module's allocator.
#[no_mangle]
pub extern "C" fn gp_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    unsafe {
        drop(Vec::from_raw_parts(ptr, len, len));
    }
}

unsafe fn bytes_into_host(buffer: Vec<u8>, out_len: *mut usize) -> *mut u8 {
    let len = buffer.len();
    let mut boxed = buffer.into_boxed_slice();
    let ptr = boxed.as_mut_ptr();
    std::mem::forget(boxed);
    if !out_len.is_null() {
        *out_len = len;
    }
    ptr
}

// ─── document lifecycle ──────────────────────────────────────────────────────

/// Open a PDF from a buffer. Returns an opaque document handle, or null on error.
#[no_mangle]
pub extern "C" fn gp_open(ptr: *const u8, len: usize) -> *mut Document {
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    match Document::open(bytes) {
        Ok(doc) => Box::into_raw(Box::new(doc)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Open a (possibly encrypted) PDF, decrypting with the UTF-8 password at
/// `(pw_ptr, pw_len)`. Returns an opaque handle, or null on error / wrong
/// password.
#[no_mangle]
pub extern "C" fn gp_open_encrypted(
    ptr: *const u8,
    len: usize,
    pw_ptr: *const u8,
    pw_len: usize,
) -> *mut Document {
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let password = unsafe {
        if pw_ptr.is_null() {
            &[][..]
        } else {
            std::slice::from_raw_parts(pw_ptr, pw_len)
        }
    };
    match Document::open_with_password(bytes, password) {
        Ok(doc) => Box::into_raw(Box::new(doc)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Serialize the document encrypted with the Standard Security Handler.
/// `algorithm`: `0` = RC4-128 (R3), `1` = AES-128 (R4), `2` = AES-256 (R6).
/// `owner` is the owner password (empty → owner = user). `key` is **secret
/// host randomness** (≥32 bytes) used only by AES-256 (the engine has no RNG).
/// Buffer-returning (host frees); null on error.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_save_encrypted(
    handle: *const Document,
    pw_ptr: *const u8,
    pw_len: usize,
    owner_ptr: *const u8,
    owner_len: usize,
    id_ptr: *const u8,
    id_len: usize,
    key_ptr: *const u8,
    key_len: usize,
    algorithm: i32,
    permissions: i32,
    out_len: *mut usize,
) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => {
            let password = unsafe { str_arg(pw_ptr, pw_len) };
            let owner = unsafe { str_arg(owner_ptr, owner_len) };
            let id = unsafe {
                if id_ptr.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(id_ptr, id_len)
                }
            };
            let key = unsafe {
                if key_ptr.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(key_ptr, key_len)
                }
            };
            let pdf = doc.save_encrypted(
                password.as_bytes(),
                owner.as_bytes(),
                id,
                key,
                algorithm,
                permissions,
            );
            unsafe { bytes_into_host(pdf, out_len) }
        }
        None => std::ptr::null_mut(),
    }
}

/// Inspect a PDF's encryption WITHOUT decrypting it (no password needed).
/// Returns a JSON buffer `{"encrypted":bool,"permissions":int,"version":int,
/// "revision":int}`. Buffer-returning (host frees); null on a null input.
#[no_mangle]
pub extern "C" fn gp_encryption_info(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let info = Document::encryption_info(bytes);
    let json = format!(
        "{{\"encrypted\":{},\"permissions\":{},\"version\":{},\"revision\":{}}}",
        info.encrypted, info.permissions, info.version, info.revision
    );
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Digitally sign the document with a freshly generated, self-signed digital
/// ID (`adbe.pkcs7.detached`). `fields` is five tab-separated UTF-8 values:
/// `name\treason\tdate\tnotBefore\tnotAfter` (the two dates are UTCTime,
/// `YYMMDDHHMMSSZ`). `rand` is host entropy for key generation; `bits` is the
/// RSA modulus size. Buffer-returning (host frees); null on error.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_sign(
    handle: *mut Document,
    fields_ptr: *const u8,
    fields_len: usize,
    rand_ptr: *const u8,
    rand_len: usize,
    bits: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return std::ptr::null_mut(),
    };
    let fields = unsafe { str_arg(fields_ptr, fields_len) };
    let parts: Vec<&str> = fields.split('\t').collect();
    if parts.len() < 5 {
        return std::ptr::null_mut();
    }
    let rand = unsafe {
        if rand_ptr.is_null() {
            &[][..]
        } else {
            std::slice::from_raw_parts(rand_ptr, rand_len)
        }
    };
    let signer =
        match gigapdf_core::sign::Signer::generate(parts[0], parts[3], parts[4], bits, rand) {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
    match doc.sign(&signer, parts[0], parts[1], parts[2]) {
        Ok(pdf) => unsafe { bytes_into_host(pdf, out_len) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// Digitally sign the document with an identity imported from a PKCS#12
/// (`.p12`/`.pfx`) file — a CA-issued / eIDAS certificate and its RSA key.
/// `p12` is the raw file; `password` its passphrase (UTF-8); `fields` is five
/// tab-separated values: `name\treason\tdate\tlocation\tcontactInfo` (`date` a
/// PDF date string, `D:YYYYMMDDHHMMSSZ`; the last two are optional `/Location`
/// and `/ContactInfo`). Buffer-returning (host frees); null on error (wrong
/// password, malformed file, unsupported cipher, or no usable certificate).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_sign_p12(
    handle: *mut Document,
    p12_ptr: *const u8,
    p12_len: usize,
    password_ptr: *const u8,
    password_len: usize,
    fields_ptr: *const u8,
    fields_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return std::ptr::null_mut(),
    };
    let p12 = unsafe {
        if p12_ptr.is_null() {
            &[][..]
        } else {
            std::slice::from_raw_parts(p12_ptr, p12_len)
        }
    };
    let password = unsafe { str_arg(password_ptr, password_len) };
    let fields = unsafe { str_arg(fields_ptr, fields_len) };
    let parts: Vec<&str> = fields.split('\t').collect();
    if parts.len() < 3 {
        return std::ptr::null_mut();
    }
    let location = parts.get(3).copied().unwrap_or("");
    let contact = parts.get(4).copied().unwrap_or("");
    let identity = match gigapdf_core::sign::pkcs12::parse(p12, password) {
        Ok(id) => id,
        Err(_) => return std::ptr::null_mut(),
    };
    match doc.sign_p12(&identity, parts[0], parts[1], parts[2], location, contact) {
        Ok(pdf) => unsafe { bytes_into_host(pdf, out_len) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// Release a document handle.
#[no_mangle]
pub extern "C" fn gp_close(handle: *mut Document) {
    if !handle.is_null() {
        unsafe {
            drop(Box::from_raw(handle));
        }
    }
}

/// Number of pages, or 0 if the handle is null.
#[no_mangle]
pub extern "C" fn gp_page_count(handle: *const Document) -> u32 {
    match unsafe { handle.as_ref() } {
        Some(doc) => doc.page_count() as u32,
        None => 0,
    }
}

/// Serialize the (edited) document. Writes the length through `out_len` and
/// returns the data pointer (host must `gp_free` it). Null on error.
#[no_mangle]
pub extern "C" fn gp_save(handle: *mut Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_mut() } {
        Some(doc) => unsafe { bytes_into_host(doc.save(), out_len) },
        None => std::ptr::null_mut(),
    }
}

// ─── content queries (JSON) ──────────────────────────────────────────────────

/// Text runs of a page as a JSON array. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_text_runs_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => doc
            .page_text_runs(page)
            .map(|runs| text_runs_json(&runs))
            .unwrap_or_else(|_| "[]".to_string()),
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Reading-order text lines of a page (structured text) as JSON `[{text,x,y,w,h}]`.
#[no_mangle]
pub extern "C" fn gp_structured_text_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => text_lines_json(&doc.structured_text(page)),
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Full-text search → JSON `[{page,text,x,y,w,h}]`. `case_insensitive` != 0 folds case.
#[no_mangle]
pub extern "C" fn gp_search_json(
    handle: *const Document,
    query_ptr: *const u8,
    query_len: usize,
    case_insensitive: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let query = unsafe { str_arg(query_ptr, query_len) };
            search_json(&doc.search(query, case_insensitive != 0))
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// OCR a page with the built-in recognizer → JSON `[{text,x,y,w,h}]` (PDF user
/// space). `scale` ≥ 2.0 recommended for small text.
#[no_mangle]
pub extern "C" fn gp_ocr_json(
    handle: *const Document,
    page: u32,
    scale: f64,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => ocr_words_json(&doc.ocr_page(page, scale)),
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// OCR a page → plain recognized text (UTF-8).
#[no_mangle]
pub extern "C" fn gp_ocr_text(
    handle: *const Document,
    page: u32,
    scale: f64,
    out_len: *mut usize,
) -> *mut u8 {
    let text = match unsafe { handle.as_ref() } {
        Some(doc) => doc.ocr_page_text(page, scale),
        None => String::new(),
    };
    unsafe { bytes_into_host(text.into_bytes(), out_len) }
}

/// Elements (text/image/shape) of a page as a JSON array. Host frees the buffer.
#[no_mangle]
pub extern "C" fn gp_elements_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => doc
            .page_elements(page)
            .map(|els| elements_json(&els))
            .unwrap_or_else(|_| "[]".to_string()),
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Every text element on a page as JSON, enriched for a host editor:
/// `[{index,text,x,y,width,height,fontFamily,bold,italic,fontSize,color:[r,g,b],rotation}]`.
/// Bounds are in user space (origin bottom-left); `index` is the text-run index
/// accepted by `gp_replace_text`. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_text_elements_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let fnum = |v: f64| if v.is_finite() { v } else { 0.0 };
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let mut s = String::from("[");
            for (i, e) in doc.page_text_elements(page).iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!("{{\"index\":{},\"text\":", e.index));
                json_escape(&e.text, &mut s);
                s.push_str(&format!(
                    ",\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"fontFamily\":",
                    fnum(e.x),
                    fnum(e.y),
                    fnum(e.width),
                    fnum(e.height)
                ));
                json_escape(&e.font_family, &mut s);
                s.push_str(&format!(
                    ",\"bold\":{},\"italic\":{},\"fontSize\":{},\"color\":[{},{},{}],\"rotation\":{}}}",
                    e.bold,
                    e.italic,
                    fnum(e.font_size),
                    fnum(e.color[0]),
                    fnum(e.color[1]),
                    fnum(e.color[2]),
                    fnum(e.rotation_deg)
                ));
            }
            s.push(']');
            s
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Index of the element at page point `(x, y)`, or -1 if none.
#[no_mangle]
pub extern "C" fn gp_element_at(handle: *const Document, page: u32, x: f64, y: f64) -> i32 {
    match unsafe { handle.as_ref() } {
        Some(doc) => match doc.element_at(page, x, y) {
            Ok(Some(index)) => index as i32,
            _ => -1,
        },
        None => -1,
    }
}

// ─── editing ─────────────────────────────────────────────────────────────────

/// Replace text run `index` on `page` with the UTF-8 text at `text_ptr`.
/// Font-aware: a Type0/Identity-H run (embedded TrueType or OpenType-CFF) is
/// re-encoded through the font's char→glyph map; simple fonts use WinAnsi.
/// Returns 0 on success, negative on error.
#[no_mangle]
pub extern "C" fn gp_replace_text(
    handle: *mut Document,
    page: u32,
    index: usize,
    text_ptr: *const u8,
    text_len: usize,
) -> i32 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return -1,
    };
    let bytes = unsafe { std::slice::from_raw_parts(text_ptr, text_len) };
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return -2,
    };
    match doc.replace_text_run(page, index, text) {
        Ok(()) => 0,
        Err(_) => -3,
    }
}

/// Remove element `index` on `page`. 0 on success.
#[no_mangle]
pub extern "C" fn gp_remove_element(handle: *mut Document, page: u32, index: usize) -> i32 {
    edit(handle, |doc| doc.remove_element(page, index))
}

/// Duplicate element `index` on `page`. 0 on success.
#[no_mangle]
pub extern "C" fn gp_duplicate_element(handle: *mut Document, page: u32, index: usize) -> i32 {
    edit(handle, |doc| doc.duplicate_element(page, index))
}

/// Move element `index` on `page` by `(dx, dy)`. 0 on success.
#[no_mangle]
pub extern "C" fn gp_move_element(
    handle: *mut Document,
    page: u32,
    index: usize,
    dx: f64,
    dy: f64,
) -> i32 {
    edit(handle, |doc| doc.move_element(page, index, dx, dy))
}

/// Redact a rectangular region: permanently remove overlapping content from the
/// page stream (the background behind it is preserved). When `has_cover != 0` a
/// `cover_rgb` (packed `0xRRGGBB`) box is drawn to visibly mark the area;
/// otherwise nothing is drawn. Returns the number of elements removed, or a
/// negative error code.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_redact_region(
    handle: *mut Document,
    page: u32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    cover_rgb: u32,
    has_cover: i32,
) -> i32 {
    let cover = (has_cover != 0).then(|| unpack_rgb(cover_rgb));
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.redact_region(page, x, y, width, height, cover) {
            Ok(count) => count as i32,
            Err(_) => -3,
        },
        None => -1,
    }
}

/// Draw a rectangle. `stroke_rgb`/`fill_rgb` are packed `0xRRGGBB`; the `has_*`
/// flags select which to apply. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_rectangle(
    handle: *mut Document,
    page: u32,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    stroke_rgb: u32,
    has_stroke: i32,
    fill_rgb: u32,
    has_fill: i32,
    line_width: f64,
    opacity: f64,
) -> i32 {
    let stroke = (has_stroke != 0).then(|| unpack_rgb(stroke_rgb));
    let fill = (has_fill != 0).then(|| unpack_rgb(fill_rgb));
    edit(handle, |doc| {
        doc.add_rectangle(page, x, y, width, height, stroke, fill, line_width, opacity)
    })
}

/// Draw a straight line on a page's content. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_draw_line(
    handle: *mut Document,
    page: u32,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    stroke_rgb: u32,
    line_width: f64,
    opacity: f64,
) -> i32 {
    let stroke = unpack_rgb(stroke_rgb);
    edit(handle, |doc| {
        doc.add_line(page, x1, y1, x2, y2, stroke, line_width, opacity)
    })
}

/// Draw an ellipse (circle when `rx == ry`) centred at `(cx, cy)`. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_ellipse(
    handle: *mut Document,
    page: u32,
    cx: f64,
    cy: f64,
    rx: f64,
    ry: f64,
    stroke_rgb: u32,
    has_stroke: i32,
    fill_rgb: u32,
    has_fill: i32,
    line_width: f64,
    opacity: f64,
) -> i32 {
    let stroke = (has_stroke != 0).then(|| unpack_rgb(stroke_rgb));
    let fill = (has_fill != 0).then(|| unpack_rgb(fill_rgb));
    edit(handle, |doc| {
        doc.add_ellipse(page, cx, cy, rx, ry, stroke, fill, line_width, opacity)
    })
}

/// Draw a polyline/polygon through flat `[x0,y0,x1,y1,…]` points (`points_ptr`,
/// `points_len` f64 values). `close != 0` joins back to the start. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_polygon(
    handle: *mut Document,
    page: u32,
    points_ptr: *const f64,
    points_len: usize,
    close: i32,
    stroke_rgb: u32,
    has_stroke: i32,
    fill_rgb: u32,
    has_fill: i32,
    line_width: f64,
    opacity: f64,
) -> i32 {
    let points: &[f64] = if points_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(points_ptr, points_len) }
    };
    let stroke = (has_stroke != 0).then(|| unpack_rgb(stroke_rgb));
    let fill = (has_fill != 0).then(|| unpack_rgb(fill_rgb));
    edit(handle, |doc| {
        doc.add_polygon(page, points, close != 0, stroke, fill, line_width, opacity)
    })
}

/// Draw an SVG path (`path_ptr`, `path_len` UTF-8) anchored at `(ox, oy)` with
/// the Y axis flipped (like `pdf-lib drawSvgPath`). 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_path(
    handle: *mut Document,
    page: u32,
    path_ptr: *const u8,
    path_len: usize,
    ox: f64,
    oy: f64,
    stroke_rgb: u32,
    has_stroke: i32,
    fill_rgb: u32,
    has_fill: i32,
    line_width: f64,
    opacity: f64,
) -> i32 {
    let svg = unsafe { str_arg(path_ptr, path_len) };
    let stroke = (has_stroke != 0).then(|| unpack_rgb(stroke_rgb));
    let fill = (has_fill != 0).then(|| unpack_rgb(fill_rgb));
    edit(handle, |doc| {
        doc.add_path(page, svg, ox, oy, stroke, fill, line_width, opacity)
    })
}

/// Embed a raster image (PNG or JPEG bytes at `data_ptr`, `data_len`) on a page
/// at `(x, y)` sized `(width, height)`, with `opacity` in `0..=1`. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_image(
    handle: *mut Document,
    page: u32,
    data_ptr: *const u8,
    data_len: usize,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    opacity: f64,
) -> i32 {
    if data_ptr.is_null() {
        return -2;
    }
    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
    edit(handle, |doc| {
        doc.add_image(page, data, x, y, width, height, opacity)
    })
}

/// Draw SVG markup (`src_ptr`, `src_len` UTF-8) on a page, fitting its viewBox
/// into the box `(x, y, width, height)` as **native vector paths** (not
/// rasterized). 0 on success; non-zero if the SVG can't be parsed.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_svg(
    handle: *mut Document,
    page: u32,
    src_ptr: *const u8,
    src_len: usize,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> i32 {
    let src = unsafe { str_arg(src_ptr, src_len) };
    edit(handle, |doc| doc.add_svg(page, src, x, y, width, height))
}

fn edit<F>(handle: *mut Document, op: F) -> i32
where
    F: FnOnce(&mut Document) -> gigapdf_core::Result<()>,
{
    match unsafe { handle.as_mut() } {
        Some(doc) => match op(doc) {
            Ok(()) => 0,
            Err(_) => -3,
        },
        None => -1,
    }
}

// ─── interactive forms ───────────────────────────────────────────────────────

/// Read a UTF-8 string argument from `(ptr, len)`; empty string on null/invalid.
unsafe fn str_arg<'a>(ptr: *const u8, len: usize) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len)).unwrap_or("")
}

/// All form fields as a JSON array. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_fields_json(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => doc
            .form_fields()
            .map(|fields| fields_json(&fields))
            .unwrap_or_else(|_| "[]".to_string()),
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Fill a text field by name with the UTF-8 value. 0 on success.
#[no_mangle]
pub extern "C" fn gp_set_text_field(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
    value_ptr: *const u8,
    value_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let value = unsafe { str_arg(value_ptr, value_len) };
    edit(handle, |doc| doc.set_text_field(name, value))
}

/// Check (`checked != 0`) or uncheck a checkbox by name. 0 on success.
#[no_mangle]
pub extern "C" fn gp_set_checkbox(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
    checked: i32,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    edit(handle, |doc| doc.set_checkbox(name, checked != 0))
}

/// Select a radio group's option by export value. 0 on success.
#[no_mangle]
pub extern "C" fn gp_set_radio(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
    value_ptr: *const u8,
    value_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let value = unsafe { str_arg(value_ptr, value_len) };
    edit(handle, |doc| doc.set_radio(name, value))
}

/// Set a choice field's selection. `values` is newline-separated (one line per
/// selected option, allowing multi-select list boxes). 0 on success.
#[no_mangle]
pub extern "C" fn gp_set_choice(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
    values_ptr: *const u8,
    values_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let joined = unsafe { str_arg(values_ptr, values_len) };
    let values: Vec<&str> = if joined.is_empty() {
        Vec::new()
    } else {
        joined.split('\n').collect()
    };
    edit(handle, |doc| doc.set_choice_field(name, &values))
}

// ─── form field *creation* ────────────────────────────────────────────────────

/// Build a [`FieldStyle`](gigapdf_core::form::FieldStyle) from packed args.
/// Colours are `0xRRGGBB`; `has_border`/`has_bg` toggle the optional colours.
fn make_field_style(
    font_size: f64,
    color_rgb: u32,
    border_rgb: u32,
    has_border: i32,
    bg_rgb: u32,
    has_bg: i32,
    border_width: f64,
) -> gigapdf_core::form::FieldStyle {
    gigapdf_core::form::FieldStyle {
        font_size,
        color: unpack_rgb(color_rgb),
        border: (has_border != 0).then(|| unpack_rgb(border_rgb)),
        background: (has_bg != 0).then(|| unpack_rgb(bg_rgb)),
        border_width,
    }
}

/// Create a text field on `page` covering `[x0,y0,x1,y1]`. `max_len < 0` means
/// no limit. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_text_field(
    handle: *mut Document,
    page: u32,
    name_ptr: *const u8,
    name_len: usize,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    value_ptr: *const u8,
    value_len: usize,
    max_len: i32,
    multiline: i32,
    password: i32,
    font_size: f64,
    color_rgb: u32,
    border_rgb: u32,
    has_border: i32,
    bg_rgb: u32,
    has_bg: i32,
    border_width: f64,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let value = unsafe { str_arg(value_ptr, value_len) };
    let style = make_field_style(
        font_size,
        color_rgb,
        border_rgb,
        has_border,
        bg_rgb,
        has_bg,
        border_width,
    );
    let ml = (max_len >= 0).then_some(max_len as u32);
    edit(handle, |doc| {
        doc.add_text_field(
            page,
            name,
            [x0, y0, x1, y1],
            value,
            ml,
            multiline != 0,
            password != 0,
            &style,
        )
    })
}

/// Create a checkbox on `page`. `export` is the on-state name (empty → `On`).
/// 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_checkbox(
    handle: *mut Document,
    page: u32,
    name_ptr: *const u8,
    name_len: usize,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    checked: i32,
    export_ptr: *const u8,
    export_len: usize,
    font_size: f64,
    color_rgb: u32,
    border_rgb: u32,
    has_border: i32,
    bg_rgb: u32,
    has_bg: i32,
    border_width: f64,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let export = unsafe { str_arg(export_ptr, export_len) };
    let style = make_field_style(
        font_size,
        color_rgb,
        border_rgb,
        has_border,
        bg_rgb,
        has_bg,
        border_width,
    );
    edit(handle, |doc| {
        doc.add_checkbox(page, name, [x0, y0, x1, y1], checked != 0, export, &style)
    })
}

/// Create a radio-button group. `exports` is newline-separated export names;
/// `rects` is a comma-separated flat list of `4 × N` numbers (one rect per
/// option, in the same order). `selected` (empty → none) is the chosen export.
/// 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_radio_group(
    handle: *mut Document,
    page: u32,
    name_ptr: *const u8,
    name_len: usize,
    exports_ptr: *const u8,
    exports_len: usize,
    rects_ptr: *const u8,
    rects_len: usize,
    selected_ptr: *const u8,
    selected_len: usize,
    font_size: f64,
    color_rgb: u32,
    border_rgb: u32,
    has_border: i32,
    bg_rgb: u32,
    has_bg: i32,
    border_width: f64,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let exports = unsafe { str_arg(exports_ptr, exports_len) };
    let rects = unsafe { str_arg(rects_ptr, rects_len) };
    let selected = unsafe { str_arg(selected_ptr, selected_len) };
    let nums: Vec<f64> = rects
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let mut options: Vec<(String, [f64; 4])> = Vec::new();
    for (i, ex) in exports.split('\n').filter(|s| !s.is_empty()).enumerate() {
        let b = i * 4;
        if b + 4 <= nums.len() {
            options.push((
                ex.to_string(),
                [nums[b], nums[b + 1], nums[b + 2], nums[b + 3]],
            ));
        }
    }
    let sel = (!selected.is_empty()).then_some(selected);
    let style = make_field_style(
        font_size,
        color_rgb,
        border_rgb,
        has_border,
        bg_rgb,
        has_bg,
        border_width,
    );
    edit(handle, |doc| {
        doc.add_radio_group(page, name, &options, sel, &style)
    })
}

/// Create a drop-down combo box. `options` is newline-separated; `selected`
/// (empty → none) is the initial value; `editable != 0` allows free text.
/// 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_combo_box(
    handle: *mut Document,
    page: u32,
    name_ptr: *const u8,
    name_len: usize,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    options_ptr: *const u8,
    options_len: usize,
    selected_ptr: *const u8,
    selected_len: usize,
    editable: i32,
    font_size: f64,
    color_rgb: u32,
    border_rgb: u32,
    has_border: i32,
    bg_rgb: u32,
    has_bg: i32,
    border_width: f64,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let opts = unsafe { str_arg(options_ptr, options_len) };
    let selected = unsafe { str_arg(selected_ptr, selected_len) };
    let options: Vec<String> = if opts.is_empty() {
        Vec::new()
    } else {
        opts.split('\n').map(str::to_string).collect()
    };
    let sel = (!selected.is_empty()).then_some(selected);
    let style = make_field_style(
        font_size,
        color_rgb,
        border_rgb,
        has_border,
        bg_rgb,
        has_bg,
        border_width,
    );
    edit(handle, |doc| {
        doc.add_combo_box(
            page,
            name,
            [x0, y0, x1, y1],
            &options,
            sel,
            editable != 0,
            &style,
        )
    })
}

/// Create a scrolling list box. `options` is newline-separated; `selected`
/// (empty → none) is the initial value; `multi != 0` allows multi-select.
/// 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_list_box(
    handle: *mut Document,
    page: u32,
    name_ptr: *const u8,
    name_len: usize,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    options_ptr: *const u8,
    options_len: usize,
    selected_ptr: *const u8,
    selected_len: usize,
    multi: i32,
    font_size: f64,
    color_rgb: u32,
    border_rgb: u32,
    has_border: i32,
    bg_rgb: u32,
    has_bg: i32,
    border_width: f64,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let opts = unsafe { str_arg(options_ptr, options_len) };
    let selected = unsafe { str_arg(selected_ptr, selected_len) };
    let options: Vec<String> = if opts.is_empty() {
        Vec::new()
    } else {
        opts.split('\n').map(str::to_string).collect()
    };
    let sel = (!selected.is_empty()).then_some(selected);
    let style = make_field_style(
        font_size,
        color_rgb,
        border_rgb,
        has_border,
        bg_rgb,
        has_bg,
        border_width,
    );
    edit(handle, |doc| {
        doc.add_list_box(
            page,
            name,
            [x0, y0, x1, y1],
            &options,
            sel,
            multi != 0,
            &style,
        )
    })
}

fn unpack_rgb(packed: u32) -> [f64; 3] {
    [
        ((packed >> 16) & 0xFF) as f64 / 255.0,
        ((packed >> 8) & 0xFF) as f64 / 255.0,
        (packed & 0xFF) as f64 / 255.0,
    ]
}

// ─── page operations ─────────────────────────────────────────────────────────

/// Rotate a page; `degrees` is normalized to 0/90/180/270. 0 on success.
#[no_mangle]
pub extern "C" fn gp_rotate_page(handle: *mut Document, page: u32, degrees: i32) -> i32 {
    edit(handle, |doc| doc.rotate_page(page, degrees))
}

/// Delete a page (1-based). 0 on success.
#[no_mangle]
pub extern "C" fn gp_delete_page(handle: *mut Document, page: u32) -> i32 {
    edit(handle, |doc| doc.delete_page(page))
}

/// Move the page at `from` to position `to` (both 1-based). 0 on success.
#[no_mangle]
pub extern "C" fn gp_move_page(handle: *mut Document, from: u32, to: u32) -> i32 {
    edit(handle, |doc| doc.move_page(from, to))
}

/// Append every page of another PDF (`other_ptr`/`other_len`) to this one.
/// 0 on success.
#[no_mangle]
pub extern "C" fn gp_append_pages(
    handle: *mut Document,
    other_ptr: *const u8,
    other_len: usize,
) -> i32 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return -1,
    };
    if other_ptr.is_null() {
        return -2;
    }
    let bytes = unsafe { std::slice::from_raw_parts(other_ptr, other_len) };
    match doc.append_pages_from(bytes) {
        Ok(()) => 0,
        Err(_) => -3,
    }
}

/// Add an invisible (render mode 3) Helvetica OCR text layer to `page` from a
/// packed run buffer. Each run is `x,y,size,rotation` (4 × f64 little-endian),
/// then a `u32` little-endian text length and that many UTF-8 bytes. Returns
/// the number of runs written (≥ 0) on success, negative on error.
#[no_mangle]
pub extern "C" fn gp_add_text_layer(
    handle: *mut Document,
    page: u32,
    data_ptr: *const u8,
    data_len: usize,
) -> i32 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return -1,
    };
    if data_ptr.is_null() {
        return -2;
    }
    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
    let mut runs: Vec<TextLayerRun> = Vec::new();
    let mut i = 0usize;
    while i + 36 <= data.len() {
        let x = f64::from_le_bytes(data[i..i + 8].try_into().unwrap());
        let y = f64::from_le_bytes(data[i + 8..i + 16].try_into().unwrap());
        let size = f64::from_le_bytes(data[i + 16..i + 24].try_into().unwrap());
        let rotation = f64::from_le_bytes(data[i + 24..i + 32].try_into().unwrap());
        let tlen = u32::from_le_bytes(data[i + 32..i + 36].try_into().unwrap()) as usize;
        i += 36;
        if i + tlen > data.len() {
            break;
        }
        let text = String::from_utf8_lossy(&data[i..i + tlen]).into_owned();
        i += tlen;
        runs.push(TextLayerRun {
            x,
            y,
            size,
            text,
            rotation_deg: rotation,
        });
    }
    match doc.add_text_layer(page, &runs) {
        Ok(written) => written as i32,
        Err(_) => -3,
    }
}

/// Resize a page's `/MediaBox` to `width`×`height` points. 0 on success.
#[no_mangle]
pub extern "C" fn gp_resize_page(handle: *mut Document, page: u32, width: f64, height: f64) -> i32 {
    edit(handle, |doc| doc.resize_page(page, width, height))
}

/// Insert a blank `width`×`height` page after the 1-based `after` page
/// (`after == 0` prepends). Returns the new page's object number, 0 on error.
#[no_mangle]
pub extern "C" fn gp_add_page(handle: *mut Document, width: f64, height: f64, after: u32) -> u32 {
    match unsafe { handle.as_mut() } {
        Some(doc) => doc.add_page(width, height, after).unwrap_or(0),
        None => 0,
    }
}

/// Duplicate the 1-based `page`, inserting the copy right after it. Returns the
/// new page's object number, 0 on error.
#[no_mangle]
pub extern "C" fn gp_copy_page(handle: *mut Document, page: u32) -> u32 {
    match unsafe { handle.as_mut() } {
        Some(doc) => doc.copy_page(page).unwrap_or(0),
        None => 0,
    }
}

/// A page's geometry as JSON `{"width":w,"height":h,"rotation":r}` (points,
/// `/Rotate` normalized). Host frees the buffer.
#[no_mangle]
pub extern "C" fn gp_page_info_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => match doc.page_info(page) {
            Ok((w, h, r)) => format!("{{\"width\":{w},\"height\":{h},\"rotation\":{r}}}"),
            Err(_) => "{\"width\":0,\"height\":0,\"rotation\":0}".to_string(),
        },
        None => "{\"width\":0,\"height\":0,\"rotation\":0}".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Rasterize a page to a PNG at `scale` device pixels per PDF point, using the
/// built-in zero-dependency renderer. Buffer-returning (host frees); null on
/// error.
#[no_mangle]
pub extern "C" fn gp_render_page(
    handle: *const Document,
    page: u32,
    scale: f64,
    out_len: *mut usize,
) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => match doc.render_page(page, scale) {
            Ok(png) => unsafe { bytes_into_host(png, out_len) },
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

/// Encode raw RGBA pixels (`width*height*4` bytes, row-major, non-premultiplied)
/// to a PNG with the engine's native encoder — no third-party image library.
/// Returns `null` if the buffer length doesn't match `width*height*4`. Host frees
/// the result.
#[no_mangle]
pub extern "C" fn gp_rgba_to_png(
    width: u32,
    height: u32,
    rgba_ptr: *const u8,
    rgba_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let expected = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if rgba_ptr.is_null() || rgba_len != expected || expected == 0 {
        return std::ptr::null_mut();
    }
    let rgba = unsafe { std::slice::from_raw_parts(rgba_ptr, rgba_len) };
    let png = gigapdf_core::raster::encode_png(width, height, rgba);
    unsafe { bytes_into_host(png, out_len) }
}

/// Resample raw RGBA pixels (`sw`×`sh`, `sw*sh*4` bytes) to `dw`×`dh` with the
/// engine's native alpha-correct resampler — no third-party image library.
/// Returns the resized RGBA (`dw*dh*4`), or a 0-length buffer on a bad input.
/// Host frees the result.
#[no_mangle]
pub extern "C" fn gp_resize_rgba(
    src_ptr: *const u8,
    src_len: usize,
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let out = if src_ptr.is_null() {
        Vec::new()
    } else {
        let src = unsafe { std::slice::from_raw_parts(src_ptr, src_len) };
        gigapdf_core::raster::resize_rgba(src, sw, sh, dw, dh)
    };
    unsafe { bytes_into_host(out, out_len) }
}

/// Encode raw RGBA pixels to a baseline JPEG at `quality` (1..=100) with the
/// engine's native encoder — no third-party image library. Alpha is composited
/// onto white. 0-length buffer on a bad input. Host frees the result.
#[no_mangle]
pub extern "C" fn gp_encode_jpeg(
    width: u32,
    height: u32,
    rgba_ptr: *const u8,
    rgba_len: usize,
    quality: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let out = if rgba_ptr.is_null() {
        Vec::new()
    } else {
        let rgba = unsafe { std::slice::from_raw_parts(rgba_ptr, rgba_len) };
        gigapdf_core::raster::jpeg::encode_jpeg(width, height, rgba, quality)
    };
    unsafe { bytes_into_host(out, out_len) }
}

/// Frame a decoded image as `[width: u32 LE][height: u32 LE][rgba…]`, the wire
/// format the SDK's `decodeJpeg`/`decodePng` unpack. Empty on a decode failure.
fn frame_image(decoded: Option<(u32, u32, Vec<u8>)>) -> Vec<u8> {
    match decoded {
        Some((w, h, rgba)) => {
            let mut out = Vec::with_capacity(8 + rgba.len());
            out.extend_from_slice(&w.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&rgba);
            out
        }
        None => Vec::new(),
    }
}

/// Decode a baseline JPEG to `[w: u32 LE][h: u32 LE][rgba]` (native decoder, no
/// third-party library). Empty on an unsupported/malformed stream. Host frees.
#[no_mangle]
pub extern "C" fn gp_decode_jpeg(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    let out = if ptr.is_null() {
        Vec::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        frame_image(gigapdf_core::raster::jpeg::decode_jpeg(bytes))
    };
    unsafe { bytes_into_host(out, out_len) }
}

/// Decode a PNG to `[w: u32 LE][h: u32 LE][rgba]` (native decoder). Empty on a
/// malformed/unsupported stream. Host frees the result.
#[no_mangle]
pub extern "C" fn gp_decode_png(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    let out = if ptr.is_null() {
        Vec::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        frame_image(
            gigapdf_core::raster::decode_png(bytes).map(|d| (d.width, d.height, d.rgba)),
        )
    };
    unsafe { bytes_into_host(out, out_len) }
}

/// Decode a GIF (first frame) to `[w: u32 LE][h: u32 LE][rgba]` (native decoder).
/// Empty on a malformed stream. Host frees the result.
#[no_mangle]
pub extern "C" fn gp_decode_gif(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    let out = if ptr.is_null() {
        Vec::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        frame_image(gigapdf_core::raster::gif::decode_gif(bytes))
    };
    unsafe { bytes_into_host(out, out_len) }
}

/// Encode raw RGBA pixels to a **lossless** WebP (VP8L) with the engine's native
/// encoder — no third-party image library. 0-length on a bad input. Host frees.
#[no_mangle]
pub extern "C" fn gp_encode_webp(
    width: u32,
    height: u32,
    rgba_ptr: *const u8,
    rgba_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let out = if rgba_ptr.is_null() {
        Vec::new()
    } else {
        let rgba = unsafe { std::slice::from_raw_parts(rgba_ptr, rgba_len) };
        gigapdf_core::raster::webp::encode_webp(width, height, rgba)
    };
    unsafe { bytes_into_host(out, out_len) }
}

/// Decode a **lossless** (VP8L) WebP to `[w: u32 LE][h: u32 LE][rgba]`. Empty for
/// lossy (VP8) / extended WebP or a malformed stream. Host frees the result.
#[no_mangle]
pub extern "C" fn gp_decode_webp(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    let out = if ptr.is_null() {
        Vec::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        frame_image(gigapdf_core::raster::webp::decode_webp(bytes))
    };
    unsafe { bytes_into_host(out, out_len) }
}

// ─── conversions & compression ───────────────────────────────────────────────
//
// All buffer-returning (host frees the result). Office exporters reconstruct
// real editable content (positioned text, re-embedded images), not page images.

/// Re-serialize the PDF with every uncompressed stream Flate-compressed.
#[no_mangle]
pub extern "C" fn gp_save_compressed(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.save_compressed(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Extract the document's text (UTF-8, form-feed between pages).
#[no_mangle]
pub extern "C" fn gp_to_text(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_text().into_bytes(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Convert to standalone HTML with positioned text.
#[no_mangle]
pub extern "C" fn gp_to_html(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_html().into_bytes(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Convert to an editable OpenDocument Text (`.odt`).
#[no_mangle]
pub extern "C" fn gp_to_odt(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_odt(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Convert to an editable Word document (`.docx`).
#[no_mangle]
pub extern "C" fn gp_to_docx(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_docx(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Convert to an editable PowerPoint presentation (`.pptx`).
#[no_mangle]
pub extern "C" fn gp_to_pptx(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_pptx(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Convert to an editable OpenDocument Presentation (`.odp`).
#[no_mangle]
pub extern "C" fn gp_to_odp(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_odp(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Convert to an Excel workbook (`.xlsx`): tables → cells, prose → text rows.
#[no_mangle]
pub extern "C" fn gp_to_xlsx(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_xlsx(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Convert to an OpenDocument Spreadsheet (`.ods`).
#[no_mangle]
pub extern "C" fn gp_to_ods(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_ods(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Write a host-provided grid (JSON `string[][][]` = pages → rows → cells) to an
/// `.xlsx` workbook — one sheet per page — reusing the engine's spreadsheet
/// writer. `names` is an optional JSON `string[]` of per-sheet titles (pass
/// `names_len = 0` to default each sheet to `Page <n>`). Lets a host supply its
/// own table reconstruction *and* sheet names yet emit XLSX with no third-party
/// library. Empty/malformed grids JSON yields a single blank sheet.
#[no_mangle]
pub extern "C" fn gp_grids_to_xlsx(
    grids_ptr: *const u8,
    grids_len: usize,
    names_ptr: *const u8,
    names_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let grids = parse_grids(grids_ptr, grids_len);
    let names = parse_sheet_names(names_ptr, names_len);
    let bytes = gigapdf_core::convert::office::to_xlsx_named(&grids, &names);
    unsafe { bytes_into_host(bytes, out_len) }
}

/// Write a host-provided grid (JSON `string[][][]`) with optional sheet `names`
/// (JSON `string[]`, `names_len = 0` for defaults) to an OpenDocument
/// Spreadsheet (`.ods`) — the `.ods` counterpart of `gp_grids_to_xlsx`.
#[no_mangle]
pub extern "C" fn gp_grids_to_ods(
    grids_ptr: *const u8,
    grids_len: usize,
    names_ptr: *const u8,
    names_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let grids = parse_grids(grids_ptr, grids_len);
    let names = parse_sheet_names(names_ptr, names_len);
    let bytes = gigapdf_core::convert::office::to_ods_named(&grids, &names);
    unsafe { bytes_into_host(bytes, out_len) }
}

/// Decode a `string[][][]` grids JSON argument (empty/malformed → empty grid).
fn parse_grids(ptr: *const u8, len: usize) -> Vec<Vec<Vec<String>>> {
    if ptr.is_null() || len == 0 {
        return Vec::new();
    }
    let json = unsafe { str_arg(ptr, len) };
    gigapdf_core::convert::grids::from_json(json).unwrap_or_default()
}

/// Decode an optional `string[]` sheet-names JSON argument (`len == 0` → none).
fn parse_sheet_names(ptr: *const u8, len: usize) -> Vec<String> {
    if ptr.is_null() || len == 0 {
        return Vec::new();
    }
    let json = unsafe { str_arg(ptr, len) };
    gigapdf_core::convert::grids::strings_from_json(json).unwrap_or_default()
}

/// Read an `.xlsx` workbook back into per-sheet grids — the inverse of
/// `gp_grids_to_xlsx` / `gp_to_xlsx`. Returns JSON `[{name, rows: string[][]}]`
/// in sheet order (inline strings, shared strings and plain values all handled).
/// Non-xlsx / unreadable input → `[]`. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_xlsx_to_grids(
    bytes_ptr: *const u8,
    bytes_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let sheets = if bytes_ptr.is_null() || bytes_len == 0 {
        Vec::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
        gigapdf_core::convert::office::xlsx_to_grids(bytes)
    };
    let mut s = String::from("[");
    for (i, (name, rows)) in sheets.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"name\":");
        json_escape(name, &mut s);
        s.push_str(",\"rows\":[");
        for (r, row) in rows.iter().enumerate() {
            if r > 0 {
                s.push(',');
            }
            s.push('[');
            for (c, cell) in row.iter().enumerate() {
                if c > 0 {
                    s.push(',');
                }
                json_escape(cell, &mut s);
            }
            s.push(']');
        }
        s.push_str("]}");
    }
    s.push(']');
    unsafe { bytes_into_host(s.into_bytes(), out_len) }
}

/// Convert the document's text to RTF.
#[no_mangle]
pub extern "C" fn gp_to_rtf(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_rtf(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Re-serialize with PDF/A-2b archival metadata (XMP + sRGB OutputIntent + ID).
#[no_mangle]
pub extern "C" fn gp_to_pdfa(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_pdfa(), out_len) },
        None => std::ptr::null_mut(),
    }
}

// ─── reverse conversions: <format> → PDF (stateless byte transforms) ──────────

/// Plain text → PDF. Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_txt_to_pdf(
    text_ptr: *const u8,
    text_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    unsafe { bytes_into_host(gigapdf_core::convert::reverse::txt_to_pdf(text), out_len) }
}

/// HTML → PDF (text-faithful, fast path). Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_html_to_pdf(
    html_ptr: *const u8,
    html_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let html = unsafe { str_arg(html_ptr, html_len) };
    unsafe { bytes_into_host(gigapdf_core::convert::reverse::html_to_pdf(html), out_len) }
}

/// Evaluate a JavaScript snippet with the built-in engine and return the result
/// value as a string (or `Uncaught …` / `SyntaxError: …`). Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_js_eval(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    let src = unsafe { str_arg(ptr, len) };
    let out = match gigapdf_core::js::parse(src) {
        Ok(prog) => {
            let mut it = gigapdf_core::js::Interp::new();
            match it.run(&prog) {
                Ok(v) => it.to_string_v(&v).unwrap_or_default(),
                Err(gigapdf_core::js::Abrupt::Throw(e)) => {
                    format!("Uncaught {}", it.to_string_v(&e).unwrap_or_default())
                }
                Err(_) => String::from("Uncaught error"),
            }
        }
        Err(e) => format!("SyntaxError: {e}"),
    };
    unsafe { bytes_into_host(out.into_bytes(), out_len) }
}

/// Run a document's inline `<script>`s and return the resulting HTML (the
/// renderer does this automatically; exposed for standalone use). Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_run_inline_scripts(
    ptr: *const u8,
    len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let html = unsafe { str_arg(ptr, len) };
    let out = gigapdf_core::js::run_inline_scripts(html);
    unsafe { bytes_into_host(out.into_bytes(), out_len) }
}

/// Serialise font requests to the `[{family,weight,italic,url}]` JSON the SDK
/// expects. Shared by the plain and `_ex` needed-fonts entry points.
fn font_reqs_json(reqs: &[gigapdf_core::html::FontRequest]) -> Vec<u8> {
    let mut json = String::from("[");
    for (i, r) in reqs.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str("{\"family\":");
        json_escape(&r.family, &mut json);
        json.push_str(&format!(
            ",\"weight\":{},\"italic\":{},\"url\":",
            r.weight, r.italic
        ));
        json_escape(&r.url, &mut json);
        json.push('}');
    }
    json.push(']');
    json.into_bytes()
}

/// Read an optional string arg: `None` when the pointer is null or the length is
/// zero (so the SDK can pass "no header/footer" as an empty span).
unsafe fn opt_str_arg<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if ptr.is_null() || len == 0 {
        None
    } else {
        Some(str_arg(ptr, len))
    }
}

/// HTML rendering engine — phase 1: the Google fonts a document needs. Returns a
/// JSON array of `{family, weight, italic, url}`. Host frees the buffer.
#[no_mangle]
pub extern "C" fn gp_html_needed_fonts(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    let html = unsafe { str_arg(ptr, len) };
    let reqs = gigapdf_core::html::needed_fonts(html);
    unsafe { bytes_into_host(font_reqs_json(&reqs), out_len) }
}

/// Like [`gp_html_needed_fonts`] but also scans the running header/footer HTML
/// (empty span = absent), so their fonts are requested too.
#[no_mangle]
pub extern "C" fn gp_html_needed_fonts_ex(
    html_ptr: *const u8,
    html_len: usize,
    header_ptr: *const u8,
    header_len: usize,
    footer_ptr: *const u8,
    footer_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let html = unsafe { str_arg(html_ptr, html_len) };
    let header = unsafe { opt_str_arg(header_ptr, header_len) };
    let footer = unsafe { opt_str_arg(footer_ptr, footer_len) };
    let reqs = gigapdf_core::html::needed_fonts_with(html, header, footer);
    unsafe { bytes_into_host(font_reqs_json(&reqs), out_len) }
}

/// Resolve a named paper size (`"A4"`, `"a3-landscape"`, `"letter"`, …) to
/// points. Writes `*out_w`/`*out_h` and returns 1 on success, 0 if unknown.
#[no_mangle]
pub extern "C" fn gp_page_size(
    name_ptr: *const u8,
    name_len: usize,
    out_w: *mut f64,
    out_h: *mut f64,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    match gigapdf_core::html::page_size(name) {
        Some((w, h)) => {
            unsafe {
                *out_w = w;
                *out_h = h;
            }
            1
        }
        None => 0,
    }
}

/// HTML rendering engine — phase 2: render HTML+CSS to PDF with embedded Google
/// fonts. `fonts` is a packed blob (little-endian): `u32 count`, then per font
/// `u32 family_len, family utf8, u16 weight, u8 italic, u32 ttf_len, ttf bytes`.
/// Buffer-returning.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_html_render(
    html_ptr: *const u8,
    html_len: usize,
    fonts_ptr: *const u8,
    fonts_len: usize,
    page_w: f64,
    page_h: f64,
    margin: f64,
    out_len: *mut usize,
) -> *mut u8 {
    let html = unsafe { str_arg(html_ptr, html_len) };
    let blob: &[u8] = if fonts_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(fonts_ptr, fonts_len) }
    };
    let fonts = parse_font_blob(blob);
    let pdf = gigapdf_core::html::render(html, &fonts, page_w, page_h, margin);
    unsafe { bytes_into_host(pdf, out_len) }
}

/// HTML rendering engine — phase 2 with full page control: per-side margins and
/// a running header/footer (empty span = absent) carrying `{{page}}`/`{{pages}}`.
/// `fonts` is the same packed blob as [`gp_html_render`]. Buffer-returning.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_html_render_opts(
    html_ptr: *const u8,
    html_len: usize,
    fonts_ptr: *const u8,
    fonts_len: usize,
    page_w: f64,
    page_h: f64,
    margin_top: f64,
    margin_right: f64,
    margin_bottom: f64,
    margin_left: f64,
    header_ptr: *const u8,
    header_len: usize,
    footer_ptr: *const u8,
    footer_len: usize,
    header_offset: f64,
    footer_offset: f64,
    start_page_number: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let html = unsafe { str_arg(html_ptr, html_len) };
    let blob: &[u8] = if fonts_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(fonts_ptr, fonts_len) }
    };
    let fonts = parse_font_blob(blob);
    let header = unsafe { opt_str_arg(header_ptr, header_len) };
    let footer = unsafe { opt_str_arg(footer_ptr, footer_len) };
    let opts = gigapdf_core::html::RenderOptions {
        page_w,
        page_h,
        margins: gigapdf_core::html::Margins {
            top: margin_top,
            right: margin_right,
            bottom: margin_bottom,
            left: margin_left,
        },
        header: header.map(str::to_string),
        footer: footer.map(str::to_string),
        header_offset,
        footer_offset,
        start_page_number: start_page_number.max(1),
    };
    let pdf = gigapdf_core::html::render_with(html, &fonts, &opts);
    unsafe { bytes_into_host(pdf, out_len) }
}

/// Decode the packed font blob passed to [`gp_html_render`].
fn parse_font_blob(b: &[u8]) -> Vec<gigapdf_core::html::ProvidedFont> {
    fn rd_u32(b: &[u8], i: &mut usize) -> Option<u32> {
        let v = b.get(*i..*i + 4)?;
        *i += 4;
        Some(u32::from_le_bytes(v.try_into().ok()?))
    }
    let mut out = Vec::new();
    let mut i = 0;
    let Some(count) = rd_u32(b, &mut i) else {
        return out;
    };
    for _ in 0..count {
        let Some(fl) = rd_u32(b, &mut i) else { break };
        let Some(fam) = b.get(i..i + fl as usize) else {
            break;
        };
        i += fl as usize;
        let Some(wb) = b.get(i..i + 2) else { break };
        i += 2;
        let weight = u16::from_le_bytes([wb[0], wb[1]]);
        let Some(&italic) = b.get(i) else { break };
        i += 1;
        let Some(tl) = rd_u32(b, &mut i) else { break };
        let Some(ttf) = b.get(i..i + tl as usize) else {
            break;
        };
        i += tl as usize;
        out.push(gigapdf_core::html::ProvidedFont {
            family: String::from_utf8_lossy(fam).into_owned(),
            weight,
            italic: italic != 0,
            ttf: ttf.to_vec(),
        });
    }
    out
}

/// RTF → PDF. Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_rtf_to_pdf(
    rtf_ptr: *const u8,
    rtf_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let rtf = unsafe { str_arg(rtf_ptr, rtf_len) };
    unsafe { bytes_into_host(gigapdf_core::convert::reverse::rtf_to_pdf(rtf), out_len) }
}

/// Office (DOCX/ODT/PPTX/XLSX/ODS) → PDF, auto-detected. Null if unrecognized.
#[no_mangle]
pub extern "C" fn gp_office_to_pdf(
    bytes_ptr: *const u8,
    bytes_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    if bytes_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
    match gigapdf_core::convert::reverse::office_to_pdf(bytes) {
        Some(pdf) => unsafe { bytes_into_host(pdf, out_len) },
        None => std::ptr::null_mut(),
    }
}

// ─── fonts: catalog, Google Fonts download (host port), embedding ────────────
//
// The WASM sandbox has no network. The engine ships the catalog, computes the
// Google Fonts URL, and parses the CSS the host fetched; the HOST performs the
// HTTP download and hands the font bytes back to gp_embed_font, which bakes them
// in — glyf TrueType (.ttf) or OpenType-CFF (.otf), flavour auto-detected.

/// The font catalog as a JSON array of `{family, category, google, weights}`.
#[no_mangle]
pub extern "C" fn gp_font_catalog_json(out_len: *mut usize) -> *mut u8 {
    let mut s = String::from("[");
    for (i, f) in gigapdf_core::font::catalog::CATALOG.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let family = f.family.replace('\\', "\\\\").replace('"', "\\\"");
        s.push_str(&format!(
            "{{\"family\":\"{}\",\"category\":\"{}\",\"google\":{},\"weights\":[",
            family,
            f.category.as_str(),
            f.google
        ));
        for (j, w) in f.weights.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str(&w.to_string());
        }
        s.push_str("]}");
    }
    s.push(']');
    unsafe { bytes_into_host(s.into_bytes(), out_len) }
}

/// Build the Google Fonts CSS2 URL for a family/weight/italic. The host fetches
/// it (with a legacy User-Agent for TTF). Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_font_request_url(
    family_ptr: *const u8,
    family_len: usize,
    weight: u32,
    italic: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let family = unsafe { str_arg(family_ptr, family_len) };
    let url = gigapdf_core::font::google::css_url(family, weight as u16, italic != 0);
    unsafe { bytes_into_host(url.into_bytes(), out_len) }
}

/// Extract the trusted `fonts.gstatic.com` font URL from a Google Fonts CSS2
/// response. Empty buffer if none/untrusted. Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_parse_css_font_url(
    css_ptr: *const u8,
    css_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let css = unsafe { str_arg(css_ptr, css_len) };
    let url = gigapdf_core::font::google::parse_css_font_url(css).unwrap_or_default();
    unsafe { bytes_into_host(url.into_bytes(), out_len) }
}

/// JSON array of `/BaseFont` names the document references but does not embed.
#[no_mangle]
pub extern "C" fn gp_needed_fonts(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let names: Vec<String> = doc
                .needed_fonts()
                .into_iter()
                .map(|n| format!("\"{}\"", n.replace('\\', "\\\\").replace('"', "\\\"")))
                .collect();
            format!("[{}]", names.join(","))
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Embed a downloaded outline font (`family` + raw bytes) as a Type0 font —
/// glyf **TrueType** (`.ttf`) or **OpenType-CFF** (`.otf`/`OTTO`), auto-detected.
/// Returns the font's object number (pass to `gp_add_text`), or 0 on error.
#[no_mangle]
pub extern "C" fn gp_embed_font(
    handle: *mut Document,
    family_ptr: *const u8,
    family_len: usize,
    ttf_ptr: *const u8,
    ttf_len: usize,
) -> u32 {
    let Some(doc) = (unsafe { handle.as_mut() }) else {
        return 0;
    };
    if ttf_ptr.is_null() {
        return 0;
    }
    let family = unsafe { str_arg(family_ptr, family_len) };
    let ttf = unsafe { std::slice::from_raw_parts(ttf_ptr, ttf_len) };
    doc.embed_truetype_font(family, ttf).unwrap_or(0)
}

/// Add real, selectable text in an embedded font (from `gp_embed_font`).
/// `rgb` packed `0xRRGGBB`. Returns 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_text(
    handle: *mut Document,
    page: u32,
    x: f64,
    y: f64,
    size: f64,
    text_ptr: *const u8,
    text_len: usize,
    font_obj: u32,
    rgb: u32,
    opacity: f64,
    rotation_deg: f64,
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    edit(handle, |doc| {
        doc.add_text(
            page,
            x,
            y,
            size,
            text,
            font_obj,
            unpack_rgb(rgb),
            opacity,
            rotation_deg,
        )
    })
}

/// Draw `text` in a built-in **base-14 standard font** — `font` is the
/// PostScript name (e.g. `Helvetica`, `Times-Bold`, `Courier-Oblique`,
/// `Symbol`). Like `gp_add_text` but needs no embedded-font handle. 0 on
/// success, non-zero on error (unknown font name / bad page).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_text_standard(
    handle: *mut Document,
    page: u32,
    x: f64,
    y: f64,
    size: f64,
    text_ptr: *const u8,
    text_len: usize,
    font_ptr: *const u8,
    font_len: usize,
    rgb: u32,
    opacity: f64,
    rotation_deg: f64,
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    let font = unsafe { str_arg(font_ptr, font_len) };
    edit(handle, |doc| {
        doc.add_text_standard(
            page,
            x,
            y,
            size,
            text,
            font,
            unpack_rgb(rgb),
            opacity,
            rotation_deg,
        )
    })
}

/// The document's embedded fonts as a JSON array
/// `[{"baseFont":…,"format":"truetype"|"cff"|"type1"}]`. Host frees the buffer.
#[no_mangle]
pub extern "C" fn gp_embedded_fonts_json(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let doc = match unsafe { handle.as_ref() } {
        Some(doc) => doc,
        None => return std::ptr::null_mut(),
    };
    let json = embedded_fonts_json(&doc.embedded_fonts());
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Extract an embedded font program by (fuzzy) `/BaseFont` name. Returns a buffer
/// whose **first byte** is the format tag (1 = truetype, 2 = cff, 3 = type1)
/// followed by the raw decoded font bytes. Null (empty) when no embedded match —
/// lets a host re-embed the document's own font when re-baking edited text.
#[no_mangle]
pub extern "C" fn gp_extract_font(
    handle: *const Document,
    name_ptr: *const u8,
    name_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let doc = match unsafe { handle.as_ref() } {
        Some(doc) => doc,
        None => return std::ptr::null_mut(),
    };
    let name = unsafe { str_arg(name_ptr, name_len) };
    match doc.extract_font_program(name) {
        Some((bytes, format)) => {
            let tag: u8 = match format {
                "truetype" => 1,
                "cff" => 2,
                _ => 3,
            };
            let mut out = Vec::with_capacity(bytes.len() + 1);
            out.push(tag);
            out.extend_from_slice(&bytes);
            unsafe { bytes_into_host(out, out_len) }
        }
        None => std::ptr::null_mut(),
    }
}

/// Add a text-markup annotation (Highlight / Underline / StrikeOut / Squiggly)
/// over `quads` (flat `[x0,y0,x1,y1, …]` in PDF coords). `meta` packs five
/// `\x1f`-separated strings: subtype, contents, author, id, date. `rgb` packed
/// `0xRRGGBB`, `opacity` 0–1. Returns 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_markup_annotation(
    handle: *mut Document,
    page: u32,
    meta_ptr: *const u8,
    meta_len: usize,
    quads_ptr: *const f64,
    quads_len: usize,
    rgb: u32,
    opacity: f64,
) -> i32 {
    let meta = unsafe { str_arg(meta_ptr, meta_len) };
    let mut parts = meta.split('\u{1f}');
    let subtype = parts.next().unwrap_or("");
    let contents = parts.next().unwrap_or("");
    let author = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("");
    let date = parts.next().unwrap_or("");
    let flat: &[f64] = if quads_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(quads_ptr, quads_len) }
    };
    let quads: Vec<[f64; 4]> = flat
        .chunks_exact(4)
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect();
    edit(handle, |doc| {
        doc.add_markup_annotation(
            page,
            subtype,
            &quads,
            unpack_rgb(rgb),
            opacity,
            contents,
            author,
            id,
            date,
        )
    })
}

/// Add a sticky-note (`/Text`) annotation. `rect` = `[x0,y0,x1,y1]`. `meta` packs
/// four `\x1f`-separated strings: contents, author, id, date. `icon` is the
/// `/Name` (e.g. "Note"). `open` non-zero opens the popup. `rgb` packed
/// `0xRRGGBB`. Returns 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_text_note(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    meta_ptr: *const u8,
    meta_len: usize,
    icon_ptr: *const u8,
    icon_len: usize,
    open: i32,
    rgb: u32,
) -> i32 {
    let meta = unsafe { str_arg(meta_ptr, meta_len) };
    let icon = unsafe { str_arg(icon_ptr, icon_len) };
    let mut parts = meta.split('\u{1f}');
    let contents = parts.next().unwrap_or("");
    let author = parts.next().unwrap_or("");
    let id = parts.next().unwrap_or("");
    let date = parts.next().unwrap_or("");
    edit(handle, |doc| {
        doc.add_text_note(
            page,
            [x0, y0, x1, y1],
            contents,
            author,
            id,
            date,
            open != 0,
            icon,
            unpack_rgb(rgb),
        )
    })
}

/// Stamp a standard-Helvetica watermark (no font embed): `text` at `(x, y)`,
/// rotated `rotation_deg`° CCW, `rgb` packed `0xRRGGBB`, `opacity` 0–1.
/// Returns 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_watermark(
    handle: *mut Document,
    page: u32,
    x: f64,
    y: f64,
    size: f64,
    text_ptr: *const u8,
    text_len: usize,
    rgb: u32,
    opacity: f64,
    rotation_deg: f64,
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    edit(handle, |doc| {
        doc.add_watermark(
            page,
            x,
            y,
            size,
            text,
            unpack_rgb(rgb),
            opacity,
            rotation_deg,
        )
    })
}

/// Width of `text` set in standard Helvetica at `size` points (AFM metrics) —
/// lets a host place watermark/header text without embedding a font.
#[no_mangle]
pub extern "C" fn gp_helvetica_width(text_ptr: *const u8, text_len: usize, size: f64) -> f64 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    Document::helvetica_width(text, size)
}

/// Extract `count` pages (1-based numbers in the `u32` array at `pages_ptr`)
/// into a NEW standalone PDF. Buffer-returning (host frees); null on error.
#[no_mangle]
pub extern "C" fn gp_extract_pages(
    handle: *const Document,
    pages_ptr: *const u32,
    count: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let doc = match unsafe { handle.as_ref() } {
        Some(doc) => doc,
        None => return std::ptr::null_mut(),
    };
    if pages_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let pages = unsafe { std::slice::from_raw_parts(pages_ptr, count) }.to_vec();
    match doc.extract_pages(&pages) {
        Ok(bytes) => unsafe { bytes_into_host(bytes, out_len) },
        Err(_) => std::ptr::null_mut(),
    }
}

// ─── annotations ─────────────────────────────────────────────────────────────

/// Page annotations as a JSON array. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_annotations_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => doc
            .page_annotations(page)
            .map(|a| annotations_json(&a))
            .unwrap_or_else(|_| "[]".to_string()),
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Remove annotation `index` on `page`. 0 on success.
#[no_mangle]
pub extern "C" fn gp_remove_annotation(handle: *mut Document, page: u32, index: usize) -> i32 {
    edit(handle, |doc| doc.remove_annotation(page, index))
}

/// Add a Square annotation. `stroke_rgb`/`fill_rgb` are packed `0xRRGGBB`,
/// gated by the `has_*` flags. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_square(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    stroke_rgb: u32,
    has_stroke: i32,
    fill_rgb: u32,
    has_fill: i32,
    line_width: f64,
) -> i32 {
    let stroke = (has_stroke != 0).then(|| unpack_rgb(stroke_rgb));
    let fill = (has_fill != 0).then(|| unpack_rgb(fill_rgb));
    edit(handle, |doc| {
        doc.add_square_annotation(page, [x0, y0, x1, y1], stroke, fill, line_width)
    })
}

/// Add a Highlight annotation over a rectangle. `rgb` is packed `0xRRGGBB`.
#[no_mangle]
pub extern "C" fn gp_add_highlight(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    rgb: u32,
) -> i32 {
    edit(handle, |doc| {
        doc.add_highlight(page, [x0, y0, x1, y1], unpack_rgb(rgb))
    })
}

/// Add a Line annotation from `(x1,y1)` to `(x2,y2)`. `rgb` packed `0xRRGGBB`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_line(
    handle: *mut Document,
    page: u32,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    rgb: u32,
    line_width: f64,
) -> i32 {
    edit(handle, |doc| {
        doc.add_line_annotation(page, x1, y1, x2, y2, unpack_rgb(rgb), line_width)
    })
}

/// Add a FreeText annotation (a text box). `rgb` is packed `0xRRGGBB`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_free_text(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    text_ptr: *const u8,
    text_len: usize,
    font_size: f64,
    rgb: u32,
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    edit(handle, |doc| {
        doc.add_free_text(page, [x0, y0, x1, y1], text, font_size, unpack_rgb(rgb))
    })
}

/// Add an Underline annotation under a text rectangle. `rgb` packed `0xRRGGBB`.
#[no_mangle]
pub extern "C" fn gp_add_underline(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    rgb: u32,
) -> i32 {
    edit(handle, |doc| {
        doc.add_underline(page, [x0, y0, x1, y1], unpack_rgb(rgb))
    })
}

/// Add a StrikeOut annotation through a text rectangle. `rgb` packed `0xRRGGBB`.
#[no_mangle]
pub extern "C" fn gp_add_strike_out(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    rgb: u32,
) -> i32 {
    edit(handle, |doc| {
        doc.add_strike_out(page, [x0, y0, x1, y1], unpack_rgb(rgb))
    })
}

/// Add an Ink (freehand) annotation from one polyline. `coords` is a flat
/// `f64` array of `x, y` pairs (`coord_count` is the number of `f64`s, i.e.
/// twice the point count). `rgb` packed `0xRRGGBB`. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_ink(
    handle: *mut Document,
    page: u32,
    coords_ptr: *const f64,
    coord_count: usize,
    rgb: u32,
    line_width: f64,
) -> i32 {
    if coords_ptr.is_null() || coord_count < 2 {
        return -2;
    }
    let coords = unsafe { std::slice::from_raw_parts(coords_ptr, coord_count) };
    let path: Vec<(f64, f64)> = coords.chunks_exact(2).map(|c| (c[0], c[1])).collect();
    edit(handle, |doc| {
        doc.add_ink(page, &[path], unpack_rgb(rgb), line_width)
    })
}

/// Add a rubber-stamp annotation (a labelled, bordered box). `rgb` packed
/// `0xRRGGBB`. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_stamp(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    label_ptr: *const u8,
    label_len: usize,
    rgb: u32,
) -> i32 {
    let label = unsafe { str_arg(label_ptr, label_len) };
    edit(handle, |doc| {
        doc.add_stamp(page, [x0, y0, x1, y1], label, unpack_rgb(rgb))
    })
}

/// Flatten a page's annotations into its content (bake appearances, drop
/// markup). Returns the number baked, or a negative error code.
#[no_mangle]
pub extern "C" fn gp_flatten_annotations(handle: *mut Document, page: u32) -> i32 {
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.flatten_annotations(page) {
            Ok(count) => count as i32,
            Err(_) => -3,
        },
        None => -1,
    }
}

/// Flatten the whole interactive form: bake every field widget across all pages
/// and drop `/AcroForm`. Returns the number of widgets baked, or a negative
/// error code.
#[no_mangle]
pub extern "C" fn gp_flatten_form(handle: *mut Document) -> i32 {
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.flatten_form() {
            Ok(count) => count as i32,
            Err(_) => -3,
        },
        None => -1,
    }
}

// ─── metadata ────────────────────────────────────────────────────────────────

/// Set a document info-dictionary entry (e.g. "Title", "Author"). 0 on success.
#[no_mangle]
pub extern "C" fn gp_set_metadata(
    handle: *mut Document,
    key_ptr: *const u8,
    key_len: usize,
    value_ptr: *const u8,
    value_len: usize,
) -> i32 {
    let key = unsafe { str_arg(key_ptr, key_len) };
    let value = unsafe { str_arg(value_ptr, value_len) };
    edit(handle, |doc| doc.set_metadata(key, value))
}

/// Read a document info-dictionary entry as UTF-8 (empty if absent). Host frees.
#[no_mangle]
pub extern "C" fn gp_get_metadata(
    handle: *const Document,
    key_ptr: *const u8,
    key_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let value = match unsafe { handle.as_ref() } {
        Some(doc) => doc
            .get_metadata(unsafe { str_arg(key_ptr, key_len) })
            .unwrap_or_default(),
        None => String::new(),
    };
    unsafe { bytes_into_host(value.into_bytes(), out_len) }
}

// ─── hyperlinks ──────────────────────────────────────────────────────────────

/// Page hyperlinks as a JSON array. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_links_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => doc
            .page_links(page)
            .map(|links| links_json(&links))
            .unwrap_or_else(|_| "[]".to_string()),
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Add an external URI hyperlink over a rectangle. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_uri_link(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    uri_ptr: *const u8,
    uri_len: usize,
) -> i32 {
    let uri = unsafe { str_arg(uri_ptr, uri_len) };
    edit(handle, |doc| doc.add_uri_link(page, [x0, y0, x1, y1], uri))
}

/// Add an internal hyperlink over a rectangle that jumps to `target_page`.
/// 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_goto_link(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    target_page: u32,
) -> i32 {
    edit(handle, |doc| {
        doc.add_goto_link(page, [x0, y0, x1, y1], target_page)
    })
}

/// Register a named destination `name` → `target_page` (a `/Fit` view) in the
/// catalog's `/Dests`. 0 on success.
#[no_mangle]
pub extern "C" fn gp_add_named_dest(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
    target_page: u32,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    edit(handle, |doc| doc.add_named_dest(name, target_page))
}

/// The catalog's named destinations as a JSON array `[{name,page}]`. Host frees
/// the returned buffer.
#[no_mangle]
pub extern "C" fn gp_named_dests_json(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let mut s = String::from("[");
            for (i, (name, page)) in doc.named_dests().iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let n = name.replace('\\', "\\\\").replace('"', "\\\"");
                s.push_str(&format!("{{\"name\":\"{n}\",\"page\":{page}}}"));
            }
            s.push(']');
            s
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Every embedded file attachment as a JSON array
/// `[{name,filename,mime,description,creationDate,modDate,dataBase64}]`. The
/// optional string fields are JSON `null` when absent; `dataBase64` is the
/// decoded file bytes, standard Base64. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_attachments_json(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let opt = |v: Option<&str>, out: &mut String| match v {
                Some(s) => json_escape(s, out),
                None => out.push_str("null"),
            };
            let mut s = String::from("[");
            for (i, att) in doc.attachments().iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str("{\"name\":");
                json_escape(&att.name, &mut s);
                s.push_str(",\"filename\":");
                json_escape(&att.filename, &mut s);
                s.push_str(",\"mime\":");
                opt(att.mime.as_deref(), &mut s);
                s.push_str(",\"description\":");
                opt(att.description.as_deref(), &mut s);
                s.push_str(",\"creationDate\":");
                opt(att.creation_date.as_deref(), &mut s);
                s.push_str(",\"modDate\":");
                opt(att.mod_date.as_deref(), &mut s);
                s.push_str(",\"dataBase64\":");
                json_escape(&gigapdf_core::convert::base64(&att.data), &mut s);
                s.push('}');
            }
            s.push(']');
            s
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Add an internal hyperlink over a rectangle that jumps to the named
/// destination `name` (define it with `gp_add_named_dest`). 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_goto_link_named(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    name_ptr: *const u8,
    name_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    edit(handle, |doc| {
        doc.add_goto_link_named(page, [x0, y0, x1, y1], name)
    })
}

// ─── outline (bookmarks / table of contents) ─────────────────────────────────

/// The document outline as a JSON array. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_outline_json(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => outline_json(&doc.outline_items()),
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Replace the document outline. `text` is one bookmark per line, each line
/// `level<TAB>page<TAB>title` (page `0` means no destination). An empty buffer
/// clears the outline. 0 on success.
#[no_mangle]
pub extern "C" fn gp_set_outline(
    handle: *mut Document,
    text_ptr: *const u8,
    text_len: usize,
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    let mut items: Vec<(String, Option<u32>, usize)> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let level = parts
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let page = parts.next().and_then(|s| s.parse::<u32>().ok());
        let page = page.filter(|p| *p > 0);
        let title = parts.next().unwrap_or("").to_string();
        items.push((title, page, level));
    }
    edit(handle, |doc| doc.set_outline(&items))
}

// ─── optional content (layers / OCG) ─────────────────────────────────────────

/// The document's optional-content layers as JSON
/// `[{id,name,visible,locked,order}]`. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_layers_json(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => layers_json(&doc.layers()),
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Create a new optional-content layer (visible, unlocked). Returns the layer's
/// object number (pass to the visibility/lock/remove calls), or 0 on error.
#[no_mangle]
pub extern "C" fn gp_add_layer(handle: *mut Document, name_ptr: *const u8, name_len: usize) -> u32 {
    let Some(doc) = (unsafe { handle.as_mut() }) else {
        return 0;
    };
    let name = unsafe { str_arg(name_ptr, name_len) };
    doc.add_layer(name).unwrap_or(0)
}

/// Show (`visible != 0`) or hide a layer by id. 0 on success.
#[no_mangle]
pub extern "C" fn gp_set_layer_visibility(
    handle: *mut Document,
    layer_id: u32,
    visible: i32,
) -> i32 {
    edit(handle, |doc| {
        doc.set_layer_visibility(layer_id, visible != 0)
    })
}

/// Lock (`locked != 0`) or unlock a layer by id. 0 on success.
#[no_mangle]
pub extern "C" fn gp_set_layer_locked(handle: *mut Document, layer_id: u32, locked: i32) -> i32 {
    edit(handle, |doc| doc.set_layer_locked(layer_id, locked != 0))
}

/// Remove a layer from the optional-content configuration. 0 on success.
#[no_mangle]
pub extern "C" fn gp_remove_layer(handle: *mut Document, layer_id: u32) -> i32 {
    edit(handle, |doc| doc.remove_layer(layer_id))
}

// ─── minimal JSON (zero-dep) ─────────────────────────────────────────────────

fn json_escape(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn text_runs_json(runs: &[TextRun]) -> String {
    let mut out = String::from("[");
    for (i, run) in runs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"index\":{},\"operator\":", run.index));
        json_escape(&String::from_utf8_lossy(&run.operator), &mut out);
        out.push_str(",\"text\":");
        json_escape(&run.text, &mut out);
        out.push('}');
    }
    out.push(']');
    out
}

fn text_lines_json(lines: &[TextLine]) -> String {
    let mut out = String::from("[");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"text\":");
        json_escape(&line.text, &mut out);
        let b = line.bounds;
        out.push_str(&format!(
            ",\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
            b.x, b.y, b.width, b.height
        ));
    }
    out.push(']');
    out
}

fn ocr_words_json(words: &[OcrWord]) -> String {
    let mut out = String::from("[");
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"text\":");
        json_escape(&word.text, &mut out);
        out.push_str(&format!(
            ",\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
            word.x, word.y, word.width, word.height
        ));
    }
    out.push(']');
    out
}

fn search_json(matches: &[SearchMatch]) -> String {
    let mut out = String::from("[");
    for (i, m) in matches.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"page\":{},\"text\":", m.page));
        json_escape(&m.text, &mut out);
        let b = m.bounds;
        out.push_str(&format!(
            ",\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}",
            b.x, b.y, b.width, b.height
        ));
    }
    out.push(']');
    out
}

fn elements_json(elements: &[ContentElement]) -> String {
    let mut out = String::from("[");
    for (i, element) in elements.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let kind = match element.kind {
            ElementKind::Text => "text",
            ElementKind::Image => "image",
            ElementKind::Path => "shape",
        };
        out.push_str(&format!(
            "{{\"index\":{},\"kind\":\"{kind}\",\"label\":",
            element.index
        ));
        json_escape(&element.label, &mut out);
        if let Some(b) = element.bounds {
            out.push_str(&format!(
                ",\"x\":{},\"y\":{},\"w\":{},\"h\":{}",
                b.x, b.y, b.width, b.height
            ));
        }
        out.push('}');
    }
    out.push(']');
    out
}

fn field_kind_str(kind: FieldKind) -> &'static str {
    match kind {
        FieldKind::Text => "text",
        FieldKind::Checkbox => "checkbox",
        FieldKind::Radio => "radio",
        FieldKind::PushButton => "pushbutton",
        FieldKind::ComboBox => "combo",
        FieldKind::ListBox => "list",
        FieldKind::Signature => "signature",
        FieldKind::Unknown => "unknown",
    }
}

fn fields_json(fields: &[FormField]) -> String {
    let mut out = String::from("[");
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        json_escape(&field.name, &mut out);
        out.push_str(",\"type\":");
        json_escape(&field.field_type, &mut out);
        out.push_str(&format!(
            ",\"kind\":\"{}\",\"flags\":{},\"readOnly\":{},\"required\":{},\"multiline\":{},\"fillable\":{}",
            field_kind_str(field.kind()),
            field.flags,
            field.is_read_only(),
            field.is_required(),
            field.is_multiline(),
            field.is_fillable(),
        ));
        if let Some(max) = field.max_len {
            out.push_str(&format!(",\"maxLen\":{max}"));
        }
        if let Some(page) = field.page {
            out.push_str(&format!(",\"page\":{page}"));
        }
        if let Some(b) = field.bounds {
            out.push_str(&format!(
                ",\"bounds\":[{},{},{},{}]",
                b[0], b[1], b[2], b[3]
            ));
        }
        out.push_str(",\"value\":");
        json_escape(&field.value, &mut out);
        out.push_str(",\"options\":[");
        for (j, option) in field.options.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            json_escape(option, &mut out);
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}

fn annotations_json(annots: &[Annotation]) -> String {
    let mut out = String::from("[");
    for (i, a) in annots.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"index\":{},\"subtype\":", a.index));
        json_escape(&a.subtype, &mut out);
        out.push_str(&format!(
            ",\"x0\":{},\"y0\":{},\"x1\":{},\"y1\":{},\"contents\":",
            a.rect[0], a.rect[1], a.rect[2], a.rect[3]
        ));
        json_escape(&a.contents, &mut out);
        out.push('}');
    }
    out.push(']');
    out
}

fn embedded_fonts_json(fonts: &[EmbeddedFontInfo]) -> String {
    let mut out = String::from("[");
    for (i, f) in fonts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"baseFont\":");
        json_escape(&f.base_font, &mut out);
        out.push_str(",\"format\":");
        json_escape(&f.format, &mut out);
        out.push('}');
    }
    out.push(']');
    out
}

fn links_json(links: &[Link]) -> String {
    let mut out = String::from("[");
    for (i, l) in links.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"index\":{},\"x0\":{},\"y0\":{},\"x1\":{},\"y1\":{},",
            l.index, l.rect[0], l.rect[1], l.rect[2], l.rect[3]
        ));
        match &l.target {
            LinkTarget::Uri(uri) => {
                out.push_str("\"kind\":\"uri\",\"uri\":");
                json_escape(uri, &mut out);
            }
            LinkTarget::Page(page) => {
                out.push_str(&format!("\"kind\":\"page\",\"page\":{page}"));
            }
            LinkTarget::Unknown => out.push_str("\"kind\":\"unknown\""),
        }
        out.push('}');
    }
    out.push(']');
    out
}

fn outline_json(items: &[OutlineItem]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"level\":{},\"title\":", item.level));
        json_escape(&item.title, &mut out);
        if let Some(page) = item.page {
            out.push_str(&format!(",\"page\":{page}"));
        }
        out.push('}');
    }
    out.push(']');
    out
}

fn layers_json(layers: &[Layer]) -> String {
    let mut out = String::from("[");
    for (i, layer) in layers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("{{\"id\":{},\"name\":", layer.id));
        json_escape(&layer.name, &mut out);
        out.push_str(&format!(
            ",\"visible\":{},\"locked\":{},\"order\":{}}}",
            layer.visible, layer.locked, layer.order
        ));
    }
    out.push(']');
    out
}
