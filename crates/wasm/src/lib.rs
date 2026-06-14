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
    Annotation, ContentElement, Document, ElementKind, FieldKind, FormField, Link, LinkTarget,
    OcrWord, OutlineItem, SearchMatch, TextLine, TextRun,
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

/// Serialize the document encrypted (RC4 128-bit) with the given password and
/// file id. Buffer-returning (host frees); null on error.
#[no_mangle]
pub extern "C" fn gp_save_encrypted(
    handle: *const Document,
    pw_ptr: *const u8,
    pw_len: usize,
    id_ptr: *const u8,
    id_len: usize,
    permissions: i32,
    out_len: *mut usize,
) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => {
            let password = unsafe { str_arg(pw_ptr, pw_len) };
            let id = unsafe {
                if id_ptr.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(id_ptr, id_len)
                }
            };
            let pdf = doc.save_encrypted(password.as_bytes(), id, permissions);
            unsafe { bytes_into_host(pdf, out_len) }
        }
        None => std::ptr::null_mut(),
    }
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
    let signer = match gigapdf_core::sign::Signer::generate(parts[0], parts[3], parts[4], bits, rand)
    {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    match doc.sign(&signer, parts[0], parts[1], parts[2]) {
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
) -> i32 {
    let stroke = (has_stroke != 0).then(|| unpack_rgb(stroke_rgb));
    let fill = (has_fill != 0).then(|| unpack_rgb(fill_rgb));
    edit(handle, |doc| {
        doc.add_rectangle(page, x, y, width, height, stroke, fill, line_width)
    })
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
pub extern "C" fn gp_txt_to_pdf(text_ptr: *const u8, text_len: usize, out_len: *mut usize) -> *mut u8 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    unsafe { bytes_into_host(gigapdf_core::convert::reverse::txt_to_pdf(text), out_len) }
}

/// HTML → PDF (text-faithful). Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_html_to_pdf(html_ptr: *const u8, html_len: usize, out_len: *mut usize) -> *mut u8 {
    let html = unsafe { str_arg(html_ptr, html_len) };
    unsafe { bytes_into_host(gigapdf_core::convert::reverse::html_to_pdf(html), out_len) }
}

/// RTF → PDF. Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_rtf_to_pdf(rtf_ptr: *const u8, rtf_len: usize, out_len: *mut usize) -> *mut u8 {
    let rtf = unsafe { str_arg(rtf_ptr, rtf_len) };
    unsafe { bytes_into_host(gigapdf_core::convert::reverse::rtf_to_pdf(rtf), out_len) }
}

/// Office (DOCX/ODT/PPTX/XLSX/ODS) → PDF, auto-detected. Null if unrecognized.
#[no_mangle]
pub extern "C" fn gp_office_to_pdf(bytes_ptr: *const u8, bytes_len: usize, out_len: *mut usize) -> *mut u8 {
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
// HTTP download and hands TTF bytes back to gp_embed_font, which bakes them in.

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

/// Embed a downloaded TrueType program (`family` + raw `.ttf` bytes) as a Type0
/// font. Returns the font's object number (pass to `gp_add_text`), or 0 on error.
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
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    edit(handle, |doc| {
        doc.add_text(page, x, y, size, text, font_obj, unpack_rgb(rgb))
    })
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
        let level = parts.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);
        let page = parts.next().and_then(|s| s.parse::<u32>().ok());
        let page = page.filter(|p| *p > 0);
        let title = parts.next().unwrap_or("").to_string();
        items.push((title, page, level));
    }
    edit(handle, |doc| doc.set_outline(&items))
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
        out.push_str(&format!(",\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}", b.x, b.y, b.width, b.height));
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
        out.push_str(&format!(",\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}", b.x, b.y, b.width, b.height));
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
