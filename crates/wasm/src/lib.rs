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

// Host entropy backend for getrandom (RSA blinding + Boa Math.random) on wasm.
#[cfg(target_arch = "wasm32")]
mod rng;

use gigapdf_core::{
    Action, AfRelationship, Annotation, Bookmark, CollectionConfig, Color, ContentElement,
    Document, ElementKind, EmbeddedFontInfo, FieldKind, FormField, GradientKind, GradientSpec,
    GradientStop, HeaderFooterSpec, InfoFields, Layer, Link, LinkTarget, Margins, OutlineItem,
    PageBox, PageLabelRange, PageLabelStyle, PageTransition, Permissions, SearchMatch,
    TextLayerRun, TextLine, TextRun, TransitionDimension, TransitionDirection, TransitionMotion,
    TransitionStyle, ViewerPreferences,
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

/// Re-encrypt an already-opened (decrypted) document with NEW passwords, also
/// controlling `/EncryptMetadata`. `algorithm`: `0` RC4, `1` AES-128, `2`
/// AES-256 (`key` = host randomness for AES-256). Buffer-returning (host frees).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_change_passwords(
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
    encrypt_metadata: i32,
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
            let pdf = doc.change_passwords(
                password.as_bytes(),
                owner.as_bytes(),
                id,
                key,
                algorithm,
                permissions,
                encrypt_metadata != 0,
            );
            unsafe { bytes_into_host(pdf, out_len) }
        }
        None => std::ptr::null_mut(),
    }
}

/// Strip encryption from an already-opened (decrypted) document, returning a
/// plaintext PDF. Buffer-returning (host frees).
#[no_mangle]
pub extern "C" fn gp_remove_encryption(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.remove_encryption(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Encrypt the document to X.509 recipients (public-key / `/Adobe.PubSec`).
/// `certs` is the recipient DER certificates concatenated; `lens` (a `u32`
/// array of `lens_count` entries) gives each one's byte length. `aes256` picks
/// AESV3; `seed` (≥ 20 bytes) and `rng` (≥ 32 bytes) are host randomness.
/// Buffer-returning (host frees); null on bad args.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_encrypt_for_recipients(
    handle: *const Document,
    certs_ptr: *const u8,
    certs_len: usize,
    lens_ptr: *const u32,
    lens_count: usize,
    permissions: i32,
    aes256: i32,
    encrypt_metadata: i32,
    seed_ptr: *const u8,
    seed_len: usize,
    rng_ptr: *const u8,
    rng_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let Some(doc) = (unsafe { handle.as_ref() }) else {
        return std::ptr::null_mut();
    };
    if certs_ptr.is_null() || lens_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let blob = unsafe { std::slice::from_raw_parts(certs_ptr, certs_len) };
    let lens = unsafe { std::slice::from_raw_parts(lens_ptr, lens_count) };
    let mut certs: Vec<Vec<u8>> = Vec::with_capacity(lens_count);
    let mut offset = 0usize;
    for &len in lens {
        let len = len as usize;
        if offset + len > blob.len() {
            return std::ptr::null_mut();
        }
        certs.push(blob[offset..offset + len].to_vec());
        offset += len;
    }
    let seed = unsafe { std::slice::from_raw_parts(seed_ptr, seed_len) };
    let rng = unsafe { std::slice::from_raw_parts(rng_ptr, rng_len) };
    match doc.encrypt_for_recipients(
        &certs,
        permissions,
        aes256 != 0,
        encrypt_metadata != 0,
        seed,
        rng,
    ) {
        Ok(pdf) => unsafe { bytes_into_host(pdf, out_len) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// Open a public-key (certificate) encrypted PDF with a recipient's DER `cert`
/// and PKCS#1 RSA private `key`. Returns a document handle, or null if the key
/// is not a recipient.
#[no_mangle]
pub extern "C" fn gp_open_with_private_key(
    ptr: *const u8,
    len: usize,
    cert_ptr: *const u8,
    cert_len: usize,
    key_ptr: *const u8,
    key_len: usize,
) -> *mut Document {
    if ptr.is_null() || cert_ptr.is_null() || key_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let cert = unsafe { std::slice::from_raw_parts(cert_ptr, cert_len) };
    let key = unsafe { std::slice::from_raw_parts(key_ptr, key_len) };
    match Document::open_with_private_key(bytes, cert, key) {
        Ok(doc) => Box::into_raw(Box::new(doc)),
        Err(_) => std::ptr::null_mut(),
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

/// Pack eight access-permission flags into the signed 32-bit `/P` value for the
/// `/Encrypt` dictionary (ISO 32000-1 Table 22). Each argument is a boolean
/// (`0` = denied, non-zero = granted) in spec-bit order: print (3), modify (4),
/// copy (5), annotate (6), fill forms (9), accessibility (10), assemble (11),
/// high-resolution print (12). Reserved bits are set per the spec. Pass this
/// result as the `permissions` argument of [`gp_save_encrypted`].
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_permissions_to_p(
    print: i32,
    modify: i32,
    copy: i32,
    annotate: i32,
    fill_forms: i32,
    accessibility: i32,
    assemble: i32,
    print_high_res: i32,
) -> i32 {
    Permissions {
        print: print != 0,
        modify: modify != 0,
        copy: copy != 0,
        annotate: annotate != 0,
        fill_forms: fill_forms != 0,
        accessibility: accessibility != 0,
        assemble: assemble != 0,
        print_high_res: print_high_res != 0,
    }
    .to_p()
}

/// Decode a `/P` value into its eight access-permission flags. Returns a JSON
/// buffer `{"print":bool,"modify":bool,"copy":bool,"annotate":bool,
/// "fillForms":bool,"accessibility":bool,"assemble":bool,"printHighRes":bool}`.
/// Combine with [`gp_encryption_info`]'s `permissions` to read a document's
/// permissions. Buffer-returning (host frees).
#[no_mangle]
pub extern "C" fn gp_permissions_from_p(p: i32, out_len: *mut usize) -> *mut u8 {
    let perms = Permissions::from_p(p);
    let json = format!(
        "{{\"print\":{},\"modify\":{},\"copy\":{},\"annotate\":{},\"fillForms\":{},\"accessibility\":{},\"assemble\":{},\"printHighRes\":{}}}",
        perms.print,
        perms.modify,
        perms.copy,
        perms.annotate,
        perms.fill_forms,
        perms.accessibility,
        perms.assemble,
        perms.print_high_res
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

/// **Certify** the document (DocMDP). Like `gp_sign` plus `docmdp_p` (`1`/`2`/`3`
/// — the permitted-changes level): writes the `/Reference` DocMDP transform and
/// the catalog `/Perms /DocMDP`. Buffer-returning; null on error.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_sign_certify(
    handle: *mut Document,
    fields_ptr: *const u8,
    fields_len: usize,
    rand_ptr: *const u8,
    rand_len: usize,
    bits: usize,
    docmdp_p: u32,
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
    match doc.sign_certify(&signer, parts[0], parts[1], parts[2], docmdp_p as u8) {
        Ok(pdf) => unsafe { bytes_into_host(pdf, out_len) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// Every signature on the document as a JSON array
/// `[{fieldName,signerName,reason,location,date,subFilter,byteRange:[a,b,c,d]}]`
/// (string fields `null` when absent). Reads the parsed model; for cryptographic
/// validity call `gp_verify_signatures`. Host frees the buffer.
#[no_mangle]
pub extern "C" fn gp_signatures_json(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            // `json_escape` emits the surrounding quotes itself.
            let opt = |v: Option<&str>, out: &mut String| match v {
                Some(s) => json_escape(s, out),
                None => out.push_str("null"),
            };
            let mut s = String::from("[");
            for (i, sig) in doc.signatures().iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str("{\"fieldName\":");
                opt(Some(&sig.field_name), &mut s);
                s.push_str(",\"signerName\":");
                opt(sig.signer_name.as_deref(), &mut s);
                s.push_str(",\"reason\":");
                opt(sig.reason.as_deref(), &mut s);
                s.push_str(",\"location\":");
                opt(sig.location.as_deref(), &mut s);
                s.push_str(",\"date\":");
                opt(sig.date.as_deref(), &mut s);
                s.push_str(",\"subFilter\":");
                opt(sig.sub_filter.as_deref(), &mut s);
                let [a, b, c, d] = sig.byte_range;
                s.push_str(&format!(",\"byteRange\":[{a},{b},{c},{d}]}}"));
            }
            s.push(']');
            s
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Cryptographically verify every signature against `pdf` (the bytes the document
/// was opened from). JSON array
/// `[{fieldName,byteRangeOk,digestOk,signatureOk,coversWholeDocument,signerCommonName,certCount,algorithm}]`.
/// Host frees the buffer.
#[no_mangle]
pub extern "C" fn gp_verify_signatures(
    handle: *const Document,
    pdf_ptr: *const u8,
    pdf_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let pdf = unsafe {
                if pdf_ptr.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(pdf_ptr, pdf_len)
                }
            };
            let mut s = String::from("[");
            for (i, r) in doc.verify_signatures(pdf).iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str("{\"fieldName\":");
                json_escape(&r.field_name, &mut s);
                s.push_str(&format!(
                    ",\"byteRangeOk\":{},\"digestOk\":{},\"signatureOk\":{},\"coversWholeDocument\":{},",
                    r.byte_range_ok, r.digest_ok, r.signature_ok, r.covers_whole_document
                ));
                s.push_str("\"signerCommonName\":");
                match r.signer_common_name.as_deref() {
                    Some(cn) => json_escape(cn, &mut s),
                    None => s.push_str("null"),
                }
                s.push_str(&format!(",\"certCount\":{},\"algorithm\":", r.cert_count));
                json_escape(&r.algorithm, &mut s);
                s.push('}');
            }
            s.push(']');
            s
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
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

/// Phase 1 of a PAdES-B-T (RFC 3161 timestamped) signature: build the signature
/// and return the `TimeStampReq` DER the host must POST to a TSA. The partial
/// signature is stashed on the document until [`gp_sign_finish_tsa`].
///
/// `fields` is seven tab-separated UTF-8 values:
/// `name\treason\tdate\tlocation\tcontactInfo\tnotBefore\tnotAfter` (`date` a PDF
/// date string `D:YYYYMMDDHHMMSSZ`; `notBefore`/`notAfter` UTCTime
/// `YYMMDDHHMMSSZ`, used only by the self-signed path). When `p12_len > 0` the
/// signing identity is imported from the PKCS#12 `(p12, password)` (a CA-issued /
/// eIDAS certificate); otherwise a fresh self-signed digital ID is generated from
/// `(rand, bits)`. `nonce` is optional host entropy echoed by the TSA (empty →
/// none). Buffer-returning (host frees); null on error.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_sign_prepare_tsa(
    handle: *mut Document,
    fields_ptr: *const u8,
    fields_len: usize,
    rand_ptr: *const u8,
    rand_len: usize,
    bits: usize,
    p12_ptr: *const u8,
    p12_len: usize,
    password_ptr: *const u8,
    password_len: usize,
    nonce_ptr: *const u8,
    nonce_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return std::ptr::null_mut(),
    };
    let fields = unsafe { str_arg(fields_ptr, fields_len) };
    let parts: Vec<&str> = fields.split('\t').collect();
    if parts.len() < 3 {
        return std::ptr::null_mut();
    }
    let name = parts[0];
    let reason = parts[1];
    let date = parts[2];
    let location = parts.get(3).copied().unwrap_or("");
    let contact = parts.get(4).copied().unwrap_or("");
    let not_before = parts.get(5).copied().unwrap_or("");
    let not_after = parts.get(6).copied().unwrap_or("");

    let nonce_bytes = unsafe { opt_slice(nonce_ptr, nonce_len) };
    let nonce = if nonce_bytes.is_empty() {
        None
    } else {
        Some(nonce_bytes)
    };

    // Resolve the signing identity: imported PKCS#12, or a fresh self-signed ID.
    let (key, cert_der) = if p12_len > 0 {
        let p12 = unsafe { opt_slice(p12_ptr, p12_len) };
        let password = unsafe { str_arg(password_ptr, password_len) };
        let identity = match gigapdf_core::sign::pkcs12::parse(p12, password) {
            Ok(id) => id,
            Err(_) => return std::ptr::null_mut(),
        };
        let cert = match identity.certificates.first() {
            Some(c) => c.clone(),
            None => return std::ptr::null_mut(),
        };
        (identity.key, cert)
    } else {
        if not_before.is_empty() || not_after.is_empty() {
            return std::ptr::null_mut();
        }
        let rand = unsafe { opt_slice(rand_ptr, rand_len) };
        let signer =
            match gigapdf_core::sign::Signer::generate(name, not_before, not_after, bits, rand) {
                Some(s) => s,
                None => return std::ptr::null_mut(),
            };
        (signer.key().clone(), signer.certificate().to_vec())
    };

    match doc.sign_prepare_timestamped(
        &key, &cert_der, name, reason, date, location, contact, nonce,
    ) {
        Ok(req) => unsafe { bytes_into_host(req, out_len) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// Phase 2 of a PAdES-B-T signature: embed the RFC 3161 timestamp from the TSA
/// reply `token` as the SignerInfo's unsigned attribute and finalize the signed
/// PDF. `token` may be the raw `TimeStampResp` returned by the TSA (the usual
/// case — its `PKIStatusInfo` is checked and the bare `TimeStampToken` is
/// extracted) or an already-unwrapped `TimeStampToken` `ContentInfo`; either way
/// the bare token is what lands in `id-aa-timeStampToken`. Requires a prior
/// [`gp_sign_prepare_tsa`] on the same handle. Buffer-returning (host frees);
/// null on error (no pending signature, reply not granted / no token, or the CMS
/// overflows `/Contents`).
#[no_mangle]
pub extern "C" fn gp_sign_finish_tsa(
    handle: *mut Document,
    token_ptr: *const u8,
    token_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return std::ptr::null_mut(),
    };
    let token = unsafe { opt_slice(token_ptr, token_len) };
    match doc.sign_finish_timestamped(token) {
        Ok(pdf) => unsafe { bytes_into_host(pdf, out_len) },
        Err(_) => std::ptr::null_mut(),
    }
}

// ─── PAdES-LTV (B-LT / B-LTA) ────────────────────────────────────────────────

/// Append a lowercase-hex encoding of `bytes` to `out` (for binary fields carried
/// inside the LTV targets JSON — OCSP requests, certificate DER).
fn push_hex(bytes: &[u8], out: &mut String) {
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
}

/// Split a length-framed buffer (`[u32 LE count]([u32 LE len][len bytes])*`) into
/// its component blobs. Returns an empty vector on any framing inconsistency.
fn read_framed(buf: &[u8]) -> Vec<Vec<u8>> {
    if buf.len() < 4 {
        return Vec::new();
    }
    let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let mut out = Vec::with_capacity(count);
    let mut pos = 4;
    for _ in 0..count {
        if pos + 4 > buf.len() {
            return Vec::new();
        }
        let len = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4;
        if pos + len > buf.len() {
            return Vec::new();
        }
        out.push(buf[pos..pos + len].to_vec());
        pos += len;
    }
    out
}

/// **LTV phase 1.** Compute the validation-material fetch plan for an
/// already-signed PDF `(pdf_ptr, pdf_len)` and return it as a JSON string the host
/// uses to drive the OCSP/CRL fetches. Each chain certificate yields its hex DER
/// and the revocation sources discovered from its extensions:
///
/// ```json
/// [{"certHex":"30..","sources":[
///     {"kind":"ocsp","url":"http://ocsp...","requestHex":"30.."},
///     {"kind":"crl","url":"http://crl..."}]}]
/// ```
///
/// `nonce` (optional host entropy) is threaded into each OCSP request. Buffer-
/// returning (host frees); never null (an empty `[]` if no signature is found).
#[no_mangle]
pub extern "C" fn gp_ltv_targets(
    pdf_ptr: *const u8,
    pdf_len: usize,
    nonce_ptr: *const u8,
    nonce_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    use gigapdf_core::sign::ltv::RevocationSource;
    let pdf = unsafe { opt_slice(pdf_ptr, pdf_len) };
    let nonce_bytes = unsafe { opt_slice(nonce_ptr, nonce_len) };
    let nonce = if nonce_bytes.is_empty() {
        None
    } else {
        Some(nonce_bytes)
    };
    let plans = Document::ltv_fetch_plan(pdf, nonce);

    let mut s = String::from("[");
    for (i, plan) in plans.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"certHex\":\"");
        push_hex(&plan.cert_der, &mut s);
        s.push_str("\",\"sources\":[");
        for (j, source) in plan.sources.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            match source {
                RevocationSource::Ocsp { url, request } => {
                    s.push_str("{\"kind\":\"ocsp\",\"url\":");
                    json_escape(url, &mut s);
                    s.push_str(",\"requestHex\":\"");
                    push_hex(request, &mut s);
                    s.push_str("\"}");
                }
                RevocationSource::Crl { url } => {
                    s.push_str("{\"kind\":\"crl\",\"url\":");
                    json_escape(url, &mut s);
                    s.push('}');
                }
            }
        }
        s.push_str("]}");
    }
    s.push(']');
    unsafe { bytes_into_host(s.into_bytes(), out_len) }
}

/// **LTV phase 2 (B-LT).** Add a `/DSS` to the signed PDF `(pdf_ptr, pdf_len)` as
/// an incremental update, embedding the host-fetched validation material. Each of
/// `certs`/`ocsps`/`crls` is a length-framed buffer
/// (`[u32 count]([u32 len][bytes])*`). Returns the upgraded PDF (PAdES-B-LT).
/// Buffer-returning (host frees); null on error.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_apply_dss(
    pdf_ptr: *const u8,
    pdf_len: usize,
    certs_ptr: *const u8,
    certs_len: usize,
    ocsps_ptr: *const u8,
    ocsps_len: usize,
    crls_ptr: *const u8,
    crls_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let pdf = unsafe { opt_slice(pdf_ptr, pdf_len) };
    let certs = read_framed(unsafe { opt_slice(certs_ptr, certs_len) });
    let ocsps = read_framed(unsafe { opt_slice(ocsps_ptr, ocsps_len) });
    let crls = read_framed(unsafe { opt_slice(crls_ptr, crls_len) });
    match Document::apply_dss(pdf, &certs, &ocsps, &crls) {
        Ok(out) => unsafe { bytes_into_host(out, out_len) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// **B-LTA phase 1.** Append an `ETSI.RFC3161` document-timestamp shell to
/// `(pdf_ptr, pdf_len)` and return the RFC 3161 `TimeStampReq` (DER) the host must
/// POST to a TSA. The partial update is stashed on `handle` until
/// [`gp_doc_timestamp_finish`]. `nonce` is optional. Buffer-returning (host
/// frees); null on error.
#[no_mangle]
pub extern "C" fn gp_doc_timestamp_prepare(
    handle: *mut Document,
    pdf_ptr: *const u8,
    pdf_len: usize,
    nonce_ptr: *const u8,
    nonce_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return std::ptr::null_mut(),
    };
    let pdf = unsafe { opt_slice(pdf_ptr, pdf_len) };
    let nonce_bytes = unsafe { opt_slice(nonce_ptr, nonce_len) };
    let nonce = if nonce_bytes.is_empty() {
        None
    } else {
        Some(nonce_bytes)
    };
    match doc.prepare_doc_timestamp(pdf, nonce) {
        Ok(req) => unsafe { bytes_into_host(req, out_len) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// **B-LTA phase 2.** Embed the RFC 3161 `token` the host fetched into the pending
/// document timestamp on `handle`, finalizing the PAdES-B-LTA PDF. Requires a
/// prior [`gp_doc_timestamp_prepare`] on the same handle. Buffer-returning (host
/// frees); null on error.
#[no_mangle]
pub extern "C" fn gp_doc_timestamp_finish(
    handle: *mut Document,
    token_ptr: *const u8,
    token_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return std::ptr::null_mut(),
    };
    let token = unsafe { opt_slice(token_ptr, token_len) };
    match doc.finish_doc_timestamp(token) {
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

/// Author a **tagged (accessible) PDF** — `/StructTreeRoot` + marked content,
/// `/MarkInfo`, `/Lang`, `/RoleMap`, `/Alt` on figures — without forcing PDF/A.
/// `pdf_ua` (non-zero) also stamps the PDF/UA-1 identifier (ISO 14289) in XMP.
/// Buffer-returning (host frees); null on error.
#[no_mangle]
pub extern "C" fn gp_to_tagged(
    handle: *const Document,
    pdf_ua: i32,
    out_len: *mut usize,
) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe { bytes_into_host(doc.to_tagged(pdf_ua != 0), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Attach author-supplied alternate text (`/Alt`, ISO 32000-1 §14.9.3) to the
/// figure at the **document-global** `index` (0-based, page-then-content order),
/// so it appears on the figure's structure element when a level-A / PDF-UA
/// export is produced (instead of the generic placeholder). `alt` is UTF-8.
/// Returns `0` on success, `-1` null handle, `-3` on error (e.g. empty `alt`).
#[no_mangle]
pub extern "C" fn gp_set_figure_alt(
    handle: *mut Document,
    index: usize,
    alt_ptr: *const u8,
    alt_len: usize,
) -> i32 {
    let alt = unsafe { str_arg(alt_ptr, alt_len) };
    edit(handle, |doc| doc.set_figure_alt(index, alt))
}

/// The number of taggable figures the engine reconstructs across the document
/// (the valid range `0..N` for [`gp_set_figure_alt`]'s `index`). `-1` on a null
/// handle.
#[no_mangle]
pub extern "C" fn gp_figure_count(handle: *const Document) -> i32 {
    match unsafe { handle.as_ref() } {
        Some(doc) => doc.figure_count() as i32,
        None => -1,
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

/// **Layout blocks** of a single page as a JSON array of `model::Block`s
/// (`[<Block>,…]`, the same per-block shape `gp_model_from_pdf` emits): the
/// structural reconstruction (paragraphs / headings / lists / tables / shapes /
/// images) in reading order, each block keeping a top-down `frame` and every
/// text run its `source_index`. The per-page counterpart of `gp_model_from_pdf`,
/// for a continuous/lazy editor requesting one page at a time. Host frees the
/// buffer; null handle → `[]`.
#[no_mangle]
pub extern "C" fn gp_page_blocks_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let blocks = doc.page_blocks(page);
            let mut out = String::from("[");
            for (i, b) in blocks.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&gigapdf_core::model::json::block_to_json(b));
            }
            out.push(']');
            out
        }
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

// OCR moved host-side: PaddleOCR PP-OCR models run via RTen in the `gigapdf-ocr-rten` crate
// (state-of-the-art, multilingual). The lean pure-std WASM core no longer ships an OCR engine;
// the host exposes OCR as an endpoint. The legacy CRNN `gp_ocr_*` exports were removed.

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
/// `[{index,text,x,y,width,height,fontFamily,bold,italic,fontSize,color:[r,g,b],
/// rotation,direction}]`. Bounds are in user space (origin bottom-left); `index`
/// is the text-run index accepted by `gp_replace_text`; `direction` is
/// `"ltr"`|`"rtl"`|`"neutral"` for the run's strong characters. Host frees the
/// returned buffer.
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
                    ",\"bold\":{},\"italic\":{},\"fontSize\":{},\"color\":[{},{},{}],\"rotation\":{},\"direction\":\"{}\"}}",
                    e.bold,
                    e.italic,
                    fnum(e.font_size),
                    fnum(e.color[0]),
                    fnum(e.color[1]),
                    fnum(e.color[2]),
                    fnum(e.rotation_deg),
                    gigapdf_core::text::direction_str(e.direction)
                ));
            }
            s.push(']');
            s
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// The document's aggregate language signal as JSON
/// `{"direction":"ltr"|"rtl"|"neutral","script":"arabic"|"hebrew"|"latin"|
/// "greek"|"cyrillic"|"cjk"|"other","lang":<ISO-639-1>|null}`, computed over
/// every page's decoded text runs. Host frees the returned buffer; a null
/// handle yields the neutral/other default.
#[no_mangle]
pub extern "C" fn gp_document_language(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let dl = doc.document_language();
            let mut s = String::new();
            s.push_str(&format!(
                "{{\"direction\":\"{}\",\"script\":\"{}\",\"lang\":",
                gigapdf_core::text::direction_str(dl.direction),
                gigapdf_core::text::script_str(dl.script)
            ));
            match dl.lang {
                Some(code) => json_escape(&code, &mut s),
                None => s.push_str("null"),
            }
            s.push('}');
            s
        }
        None => "{\"direction\":\"neutral\",\"script\":\"other\",\"lang\":null}".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Every image element on a page as JSON, for a host editor:
/// `[{index,x,y,width,height,format,pixelWidth,pixelHeight,dataBase64,rotation,
/// opacity}]`. Bounds in user space (origin bottom-left); `format` is
/// `jpeg`/`png`/`jp2`/`unknown`; `dataBase64` is the embeddable encoded bytes
/// (empty when `unknown`); `rotation` is the placement angle in degrees and
/// `opacity` the `/ExtGState` fill alpha (`1` = opaque). Host frees the buffer.
#[no_mangle]
pub extern "C" fn gp_image_elements_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let fnum = |v: f64| if v.is_finite() { v } else { 0.0 };
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let mut s = String::from("[");
            for (i, e) in doc.page_image_elements(page).iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!(
                    "{{\"index\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"format\":",
                    e.index,
                    fnum(e.x),
                    fnum(e.y),
                    fnum(e.width),
                    fnum(e.height)
                ));
                json_escape(&e.format, &mut s);
                s.push_str(&format!(
                    ",\"pixelWidth\":{},\"pixelHeight\":{},\"dataBase64\":",
                    e.pixel_width, e.pixel_height
                ));
                json_escape(&gigapdf_core::convert::base64(&e.data), &mut s);
                s.push_str(&format!(
                    ",\"rotation\":{},\"opacity\":{}}}",
                    fnum(e.rotation),
                    fnum(e.opacity)
                ));
            }
            s.push(']');
            s
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Every painted vector path on a page as JSON, for a host editor's shape layer:
/// `[{index,hasBounds,x0,y0,x1,y1,segments,fill,stroke,strokeWidth,fillAlpha,
/// strokeAlpha,dash}]`. Bounds + segment points are in user space (origin
/// bottom-left); each segment is `{op:"M"|"L"|"C"|"Z",pts:[…]}`; `fill`/`stroke`
/// are `[r,g,b]` in `0..=1` or `null`. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_vector_paths_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    use gigapdf_core::content::vector::PathSeg;
    let fnum = |v: f64| if v.is_finite() { v } else { 0.0 };
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let paths = doc.page_vector_paths(page).unwrap_or_default();
            let mut s = String::from("[");
            for (i, p) in paths.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                let (hb, b) = match p.bounds {
                    Some(b) => (true, [b.x, b.y, b.x + b.width, b.y + b.height]),
                    None => (false, [0.0; 4]),
                };
                s.push_str(&format!(
                    "{{\"index\":{},\"hasBounds\":{},\"x0\":{},\"y0\":{},\"x1\":{},\"y1\":{},\"segments\":[",
                    p.index, hb, fnum(b[0]), fnum(b[1]), fnum(b[2]), fnum(b[3])
                ));
                for (j, seg) in p.segments.iter().enumerate() {
                    if j > 0 {
                        s.push(',');
                    }
                    let (op, pts): (&str, Vec<f64>) = match *seg {
                        PathSeg::Move(x, y) => ("M", vec![x, y]),
                        PathSeg::Line(x, y) => ("L", vec![x, y]),
                        PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => {
                            ("C", vec![x1, y1, x2, y2, x3, y3])
                        }
                        PathSeg::Close => ("Z", Vec::new()),
                    };
                    s.push_str(&format!("{{\"op\":\"{op}\",\"pts\":"));
                    s.push_str(&num_array_json(&pts));
                    s.push('}');
                }
                s.push_str("],\"fill\":");
                match p.fill {
                    Some(c) => s.push_str(&num_array_json(&c)),
                    None => s.push_str("null"),
                }
                s.push_str(",\"stroke\":");
                match p.stroke {
                    Some(c) => s.push_str(&num_array_json(&c)),
                    None => s.push_str("null"),
                }
                s.push_str(&format!(
                    ",\"strokeWidth\":{},\"fillAlpha\":{},\"strokeAlpha\":{},\"dash\":",
                    fnum(p.stroke_width),
                    fnum(p.fill_alpha),
                    fnum(p.stroke_alpha)
                ));
                s.push_str(&num_array_json(&p.dash));
                s.push('}');
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

/// Apply the affine transform `[a, b, c, d, e, f]` to element `index` on `page`
/// (wraps it in `q … cm … Q`). Generalises [`gp_move_element`] to
/// scale/rotate/shear. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_transform_element(
    handle: *mut Document,
    page: u32,
    index: usize,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
) -> i32 {
    edit(handle, |doc| {
        doc.transform_element(page, index, [a, b, c, d, e, f])
    })
}

/// Re-style the **path** element `index` on `page` in place. `json_ptr`/`json_len`
/// is a small JSON object with optional keys: `fill`/`stroke` (`[r,g,b]` in
/// `0..=1`), `strokeWidth` (number), `dash` (number array), and `fillAlpha`/
/// `strokeAlpha` (numbers, accepted but not emitted — opacity needs an
/// `/ExtGState`). 0 on success; negative on error (incl. non-path element).
#[no_mangle]
pub extern "C" fn gp_set_path_style_json(
    handle: *mut Document,
    page: u32,
    index: usize,
    json_ptr: *const u8,
    json_len: usize,
) -> i32 {
    let json = unsafe { str_arg(json_ptr, json_len) };
    let style = parse_path_style_json(json);
    edit(handle, |doc| doc.set_path_style(page, index, &style))
}

/// Re-style **sub-ranges** of text run `index` on `page` in place. `json_ptr`/
/// `json_len` is a JSON **array** of span objects, each with integer `start`/`end`
/// (UTF-16 offsets into the run's decoded text) and optional style keys: `color`
/// (`[r,g,b]` in `0..=1`), `sizePt` (number), `bold`/`italic`/`underline`/`strike`
/// (booleans). Each span sets the style of its `[start, end)` slice; the run is
/// split so the rest keeps its original style and positioning is preserved. The
/// by-character-run companion of [`gp_set_path_style_json`]. 0 on success; negative
/// on error (incl. an index that does not resolve to a top-level text run → the SDK
/// surfaces `false`).
#[no_mangle]
pub extern "C" fn gp_set_text_run_style_json(
    handle: *mut Document,
    page: u32,
    index: usize,
    json_ptr: *const u8,
    json_len: usize,
) -> i32 {
    let json = unsafe { str_arg(json_ptr, json_len) };
    let spans = parse_text_spans_json(json);
    edit(handle, |doc| doc.set_text_run_style(page, index, &spans))
}

/// Parse a JSON array of `{start,end,color,sizePt,bold,italic,underline,strike}`
/// span objects into `(start, end, TextStylePatch)` tuples. Std-only, tailored to
/// this fixed shape (no third-party JSON): each top-level `{ … }` object is sliced
/// out by brace matching and scanned with the existing key helpers. Objects
/// missing `start`/`end` are skipped.
fn parse_text_spans_json(json: &str) -> Vec<(usize, usize, gigapdf_core::content::TextStylePatch)> {
    let mut out = Vec::new();
    let bytes = json.as_bytes();
    let mut i = 0;
    // Walk top-level object boundaries inside the array, honouring nested braces
    // (a `color` array has none, but be robust) and skipping string contents.
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let start_obj = i;
        let mut depth = 0i32;
        let mut in_str = false;
        let mut j = i;
        while j < bytes.len() {
            let c = bytes[j];
            if in_str {
                if c == b'\\' {
                    j += 2;
                    continue;
                }
                if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            j += 1;
        }
        let obj = &json[start_obj..=j.min(bytes.len() - 1)];
        if let (Some(s), Some(e)) = (json_number(obj, "start"), json_number(obj, "end")) {
            let patch = gigapdf_core::content::TextStylePatch {
                color: json_rgb(obj, "color"),
                size_pt: json_number(obj, "sizePt"),
                bold: json_bool(obj, "bold"),
                italic: json_bool(obj, "italic"),
                underline: json_bool(obj, "underline"),
                strike: json_bool(obj, "strike"),
                font_swap: None,
            };
            out.push((s.max(0.0) as usize, e.max(0.0) as usize, patch));
        }
        i = j + 1;
    }
    out
}

/// Read a JSON boolean value for `key` (`true`/`false`); `None` if absent.
fn json_bool(json: &str, key: &str) -> Option<bool> {
    let start = json_value_start(json, key)?;
    let rest = json[start..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// Set a constant opacity (`fill_alpha`, `0..=1`) on element `index` on `page`
/// — text, image **or** shape — by registering an `/ExtGState` (`/ca` = `/CA` =
/// `fill_alpha`) and wrapping the element's op range in `q /<gs> gs … Q`. This is
/// how an **image**'s alpha is set in place. 0 on success; negative on error.
#[no_mangle]
pub extern "C" fn gp_set_element_opacity(
    handle: *mut Document,
    page: u32,
    index: usize,
    fill_alpha: f64,
) -> i32 {
    edit(handle, |doc| doc.set_element_opacity(page, index, fill_alpha))
}

/// Change the paint order (z-order) of element `index` on `page`: `to_front != 0`
/// brings it on top (painted last), otherwise it goes behind (painted first). The
/// element's index changes after the move — the caller should re-read the element
/// list. 0 on success; negative on error (incl. out-of-range index).
#[no_mangle]
pub extern "C" fn gp_reorder_element(
    handle: *mut Document,
    page: u32,
    index: usize,
    to_front: i32,
) -> i32 {
    edit(handle, |doc| doc.reorder_element(page, index, to_front != 0))
}

/// Parse a small `{fill,stroke,strokeWidth,dash,fillAlpha,strokeAlpha}` JSON
/// object into a [`PathStyle`]. Std-only, tailored to this fixed shape (no
/// third-party JSON dependency): unknown keys are ignored, missing keys stay
/// `None`. RGB arrays must hold exactly three numbers to be applied.
fn parse_path_style_json(json: &str) -> gigapdf_core::content::PathStyle {
    gigapdf_core::content::PathStyle {
        fill: json_rgb(json, "fill"),
        stroke: json_rgb(json, "stroke"),
        stroke_width: json_number(json, "strokeWidth"),
        fill_alpha: json_number(json, "fillAlpha"),
        stroke_alpha: json_number(json, "strokeAlpha"),
        dash: json_number_array(json, "dash"),
    }
}

/// Find the byte position just after `"key"` and its `:` in `json`, or `None`.
fn json_value_start(json: &str, key: &str) -> Option<usize> {
    let needle = format!("\"{key}\"");
    let key_at = json.find(&needle)?;
    let after_key = &json[key_at + needle.len()..];
    let colon = after_key.find(':')?;
    Some(key_at + needle.len() + colon + 1)
}

/// Read a JSON number value for `key` (e.g. `"strokeWidth": 3.5`).
fn json_number(json: &str, key: &str) -> Option<f64> {
    let start = json_value_start(json, key)?;
    let rest = json[start..].trim_start();
    let end = rest
        .find(|c: char| !matches!(c, '0'..='9' | '.' | '-' | '+' | 'e' | 'E'))
        .unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

/// Read a JSON array of numbers for `key` (e.g. `"dash": [4, 2]`).
fn json_number_array(json: &str, key: &str) -> Option<Vec<f64>> {
    let start = json_value_start(json, key)?;
    let rest = json[start..].trim_start();
    let open = rest.find('[')?;
    let close = rest[open..].find(']')? + open;
    let inner = &rest[open + 1..close];
    let nums: Vec<f64> = inner
        .split(',')
        .filter_map(|s| {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                t.parse().ok()
            }
        })
        .collect();
    Some(nums)
}

/// Read a 3-number RGB array for `key`; `None` unless exactly three numbers.
fn json_rgb(json: &str, key: &str) -> Option<[f64; 3]> {
    let nums = json_number_array(json, key)?;
    if nums.len() == 3 {
        Some([nums[0], nums[1], nums[2]])
    } else {
        None
    }
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

/// True **PII redaction** of one or more regions on `page`. `rects_ptr`/`rects_len`
/// is a flat `[x0,y0,w0,h0, x1,y1,w1,h1, …]` f64 array (page user space); for each
/// region the overlapping text/vectors are deleted from the stream, the covered
/// sub-rectangle of any underlying image XObject is overwritten with opaque black
/// (and the image re-encoded), overlapping annotations + field values are stripped,
/// and — when `has_cover != 0` — a `cover_rgb` (packed `0xRRGGBB`) box is painted
/// as the visible mark. Returns the number of content elements removed, or a
/// negative error code.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_redact_pii(
    handle: *mut Document,
    page: u32,
    rects_ptr: *const f64,
    rects_len: usize,
    cover_rgb: u32,
    has_cover: i32,
) -> i32 {
    let flat: &[f64] = if rects_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(rects_ptr, rects_len) }
    };
    let rects: Vec<(f64, f64, f64, f64)> = flat
        .chunks_exact(4)
        .map(|c| (c[0], c[1], c[2], c[3]))
        .collect();
    let cover = (has_cover != 0).then(|| unpack_rgb(cover_rgb));
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.redact_pii_with(page, &rects, cover) {
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

/// Paint a gradient over `[rx,ry,rw,rh]` on `page`. `kind` is `0` linear (coords
/// `[x0,y0,x1,y1]`) or `1` radial (coords `[x0,y0,r0,x1,y1,r1]`). `offsets` (f64)
/// and `colors` (packed `0xRRGGBB`) are parallel arrays of `stops_count` colour
/// stops. `extend_start`/`extend_end` flag `/Extend`. `0` on success, `-2` bad
/// args, `-1` null handle.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_gradient(
    handle: *mut Document,
    page: u32,
    kind: i32,
    coords_ptr: *const f64,
    coords_count: usize,
    offsets_ptr: *const f64,
    colors_ptr: *const u32,
    stops_count: usize,
    rx: f64,
    ry: f64,
    rw: f64,
    rh: f64,
    extend_start: i32,
    extend_end: i32,
    opacity: f64,
) -> i32 {
    let coords: &[f64] = if coords_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(coords_ptr, coords_count) }
    };
    let offsets: &[f64] = if offsets_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(offsets_ptr, stops_count) }
    };
    let colors = unsafe {
        if colors_ptr.is_null() {
            &[][..]
        } else {
            std::slice::from_raw_parts(colors_ptr, stops_count)
        }
    };
    if offsets.len() != colors.len() || offsets.len() < 2 {
        return -2;
    }
    let kind = match kind {
        1 if coords.len() >= 6 => GradientKind::Radial {
            x0: coords[0],
            y0: coords[1],
            r0: coords[2],
            x1: coords[3],
            y1: coords[4],
            r1: coords[5],
        },
        0 if coords.len() >= 4 => GradientKind::Linear {
            x0: coords[0],
            y0: coords[1],
            x1: coords[2],
            y1: coords[3],
        },
        _ => return -2,
    };
    let stops: Vec<GradientStop> = offsets
        .iter()
        .zip(colors.iter())
        .map(|(&offset, &rgb)| GradientStop {
            offset,
            color: unpack_rgb(rgb),
        })
        .collect();
    let spec = GradientSpec {
        kind,
        stops,
        rect: [rx, ry, rw, rh],
        extend: (extend_start != 0, extend_end != 0),
        opacity,
    };
    edit(handle, |doc| doc.add_gradient(page, &spec))
}

/// Decode a colour passed across the ABI. `kind`: `0` RGB (`comps=[r,g,b]`), `1`
/// CMYK (`[c,m,y,k]`), `2` gray (`[v]`), `3` Separation (`comps=[tint,c,m,y,k]`,
/// `name` = spot name), `4` ICCBased (`comps` = components, `profile` = ICC bytes).
fn decode_color(kind: i32, comps: &[f64], name: &str, profile: &[u8]) -> Option<Color> {
    Some(match kind {
        0 if comps.len() >= 3 => Color::Rgb([comps[0], comps[1], comps[2]]),
        1 if comps.len() >= 4 => Color::Cmyk([comps[0], comps[1], comps[2], comps[3]]),
        2 if !comps.is_empty() => Color::Gray(comps[0]),
        3 if comps.len() >= 5 => Color::Separation {
            name: name.to_string(),
            tint: comps[0],
            cmyk: [comps[1], comps[2], comps[3], comps[4]],
        },
        4 if !comps.is_empty() && !profile.is_empty() => Color::IccBased {
            components: comps.to_vec(),
            profile: profile.to_vec(),
        },
        _ => return None,
    })
}

/// Read a colour's three ABI buffers (`comps` f64 array, `name` UTF-8, `profile`
/// bytes) into a [`Color`]. Any pointer may be null when unused for that `kind`.
unsafe fn color_arg(
    kind: i32,
    comps_ptr: *const f64,
    comps_count: usize,
    name_ptr: *const u8,
    name_len: usize,
    profile_ptr: *const u8,
    profile_len: usize,
) -> Option<Color> {
    let comps: &[f64] = if comps_ptr.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(comps_ptr, comps_count)
    };
    let name = if name_ptr.is_null() {
        ""
    } else {
        str_arg(name_ptr, name_len)
    };
    let profile: &[u8] = if profile_ptr.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(profile_ptr, profile_len)
    };
    decode_color(kind, comps, name, profile)
}

/// Fill a rectangle `[x,y,w,h]` with a colour in any space (see [`color_arg`] for
/// the colour encoding). `0` success, `-2` bad colour args, `-1` null handle.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_filled_rectangle(
    handle: *mut Document,
    page: u32,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    kind: i32,
    comps_ptr: *const f64,
    comps_count: usize,
    name_ptr: *const u8,
    name_len: usize,
    profile_ptr: *const u8,
    profile_len: usize,
    opacity: f64,
) -> i32 {
    let color = match unsafe {
        color_arg(kind, comps_ptr, comps_count, name_ptr, name_len, profile_ptr, profile_len)
    } {
        Some(c) => c,
        None => return -2,
    };
    edit(handle, |doc| doc.add_filled_rectangle(page, [x, y, w, h], &color, opacity))
}

/// Fill a polygon through flat `[x0,y0,…]` `points` with a colour in any space.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_filled_polygon(
    handle: *mut Document,
    page: u32,
    points_ptr: *const f64,
    points_count: usize,
    kind: i32,
    comps_ptr: *const f64,
    comps_count: usize,
    name_ptr: *const u8,
    name_len: usize,
    profile_ptr: *const u8,
    profile_len: usize,
    opacity: f64,
) -> i32 {
    let points: &[f64] = if points_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(points_ptr, points_count) }
    };
    let color = match unsafe {
        color_arg(kind, comps_ptr, comps_count, name_ptr, name_len, profile_ptr, profile_len)
    } {
        Some(c) => c,
        None => return -2,
    };
    edit(handle, |doc| doc.add_filled_polygon(page, points, &color, opacity))
}

/// Draw a base-14 text run in any colour space. `font` is the base-14 name.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_text_color(
    handle: *mut Document,
    page: u32,
    x: f64,
    y: f64,
    size: f64,
    text_ptr: *const u8,
    text_len: usize,
    font_ptr: *const u8,
    font_len: usize,
    kind: i32,
    comps_ptr: *const f64,
    comps_count: usize,
    name_ptr: *const u8,
    name_len: usize,
    profile_ptr: *const u8,
    profile_len: usize,
    opacity: f64,
    rotation_deg: f64,
    underline: i32,
    strikethrough: i32,
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    let font = unsafe { str_arg(font_ptr, font_len) };
    let color = match unsafe {
        color_arg(kind, comps_ptr, comps_count, name_ptr, name_len, profile_ptr, profile_len)
    } {
        Some(c) => c,
        None => return -2,
    };
    edit(handle, |doc| {
        doc.add_text_color(
            page,
            x,
            y,
            size,
            text,
            font,
            &color,
            opacity,
            rotation_deg,
            underline != 0,
            strikethrough != 0,
        )
    })
}

/// Turn overprint on/off for subsequent content on `page` (`/op`, `/OP`, `/OPM`).
#[no_mangle]
pub extern "C" fn gp_set_overprint(
    handle: *mut Document,
    page: u32,
    fill: i32,
    stroke: i32,
    mode: u32,
) -> i32 {
    edit(handle, |doc| {
        doc.set_overprint(page, fill != 0, stroke != 0, mode as u8)
    })
}

/// Add a document-level OutputIntent embedding the ICC `profile` (`/N` read from
/// the profile), with `condition` as the output-condition identifier.
#[no_mangle]
pub extern "C" fn gp_add_output_intent(
    handle: *mut Document,
    profile_ptr: *const u8,
    profile_len: usize,
    condition_ptr: *const u8,
    condition_len: usize,
) -> i32 {
    let profile: &[u8] = if profile_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(profile_ptr, profile_len) }
    };
    let condition = unsafe { str_arg(condition_ptr, condition_len) };
    edit(handle, |doc| doc.add_output_intent(profile, condition))
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

/// Replace the pixels of the existing image element `index` on `page` **in place**
/// with a fresh raster (PNG or JPEG bytes at `data_ptr`, `data_len`). `index` is
/// the unified element index from [`gp_image_elements_json`]; the image's object
/// number, every `/Do` reference and its placement matrix are preserved — only the
/// stream bytes and image dictionary change. 0 on success; `-3` when `index` isn't
/// a top-level image or the bytes aren't decodable PNG/JPEG.
#[no_mangle]
pub extern "C" fn gp_replace_image(
    handle: *mut Document,
    page: u32,
    index: usize,
    data_ptr: *const u8,
    data_len: usize,
) -> i32 {
    if data_ptr.is_null() {
        return -2;
    }
    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
    edit(handle, |doc| doc.replace_image(page, index, data))
}

/// Stamp an **image watermark** (PNG / JPEG / WebP / GIF / AVIF bytes at
/// `data_ptr`, `data_len`) across pages. The image is embedded once and
/// referenced on every target page.
///
/// `pages_ptr`/`pages_count` is a `u32` array of 1-based page numbers; pass a
/// null pointer or `0` count to stamp **every** page. `anchor` is a tag:
/// `0`=Center, `1`=TopLeft, `2`=TopRight, `3`=BottomLeft, `4`=BottomRight.
/// `width`/`height` are target points; pass `<= 0` to use the source pixel size
/// (`height <= 0` keeps the aspect ratio). `tile != 0` repeats the image in a
/// grid (then `offset_x`/`offset_y` are the tile gaps). Returns `0` on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_image_watermark(
    handle: *mut Document,
    data_ptr: *const u8,
    data_len: usize,
    pages_ptr: *const u32,
    pages_count: usize,
    anchor: u32,
    offset_x: f64,
    offset_y: f64,
    width: f64,
    height: f64,
    rotation_deg: f64,
    opacity: f64,
    tile: u32,
) -> i32 {
    if data_ptr.is_null() {
        return -2;
    }
    let data = unsafe { std::slice::from_raw_parts(data_ptr, data_len) };
    let pages = if pages_ptr.is_null() || pages_count == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(pages_ptr, pages_count) }.to_vec()
    };
    let anchor = match anchor {
        1 => gigapdf_core::WatermarkAnchor::TopLeft,
        2 => gigapdf_core::WatermarkAnchor::TopRight,
        3 => gigapdf_core::WatermarkAnchor::BottomLeft,
        4 => gigapdf_core::WatermarkAnchor::BottomRight,
        _ => gigapdf_core::WatermarkAnchor::Center,
    };
    let opts = gigapdf_core::ImageWatermarkOptions {
        pages,
        anchor,
        offset_x,
        offset_y,
        width: (width > 0.0).then_some(width),
        height: (height > 0.0).then_some(height),
        rotation_deg,
        opacity,
        tile: tile != 0,
    };
    edit(handle, |doc| doc.add_image_watermark(data, &opts).map(|_| ()))
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

/// A byte slice from a `(ptr, len)` pair, or empty when the pointer is null.
unsafe fn opt_slice<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(ptr, len)
    }
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

/// Create a **visible signature field** (`/FT /Sig`) over `[x0,y0,x1,y1]` and set
/// the AcroForm `/SigFlags`. Style params mirror the other field creators. `0` on
/// success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_signature_field(
    handle: *mut Document,
    page: u32,
    name_ptr: *const u8,
    name_len: usize,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    font_size: f64,
    color_rgb: u32,
    border_rgb: u32,
    has_border: i32,
    bg_rgb: u32,
    has_bg: i32,
    border_width: f64,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
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
        doc.add_signature_field(page, name, [x0, y0, x1, y1], &style)
    })
}

/// Attach field-level JavaScript to a field's `/AA`. `trigger` is one of
/// `keystroke`/`format`/`validate`/`calculate`. Returns `1` if set, `0` if no
/// field has that name, `-1` null handle, `-2` unknown trigger.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_set_field_script(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
    trigger_ptr: *const u8,
    trigger_len: usize,
    js_ptr: *const u8,
    js_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let trigger = unsafe { str_arg(trigger_ptr, trigger_len) };
    let js = unsafe { str_arg(js_ptr, js_len) };
    let Some(trigger) = gigapdf_core::form::FieldTrigger::from_name(trigger) else {
        return -2;
    };
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.set_field_action(name, trigger, js) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(_) => -3,
        },
        None => -1,
    }
}

/// Set the AcroForm calculation order (`/CO`). `names` is newline-separated field
/// names (unknown ones skipped). `0` on success.
#[no_mangle]
pub extern "C" fn gp_set_calculation_order(
    handle: *mut Document,
    names_ptr: *const u8,
    names_len: usize,
) -> i32 {
    let text = unsafe { str_arg(names_ptr, names_len) };
    let names: Vec<&str> = text.split('\n').filter(|s| !s.is_empty()).collect();
    edit(handle, |doc| doc.set_calculation_order(&names))
}

/// Delete a form field by name. Returns `1` if removed, `0` if not found, `-1`
/// null handle.
#[no_mangle]
pub extern "C" fn gp_remove_field(handle: *mut Document, name_ptr: *const u8, name_len: usize) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.remove_field(name) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(_) => -3,
        },
        None => -1,
    }
}

/// Rebuild a field's `/AP` appearance from its current value/style. Returns `1`
/// if regenerated, `0` if unknown/unsupported kind, `-1` null handle.
#[no_mangle]
pub extern "C" fn gp_regenerate_field_appearance(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.regenerate_field_appearance(name) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(_) => -3,
        },
        None => -1,
    }
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

/// Append a **subset** of another PDF's pages (`other_ptr`/`other_len`) to this
/// one — the `count` 1-based page numbers in the `u32` array at `pages_ptr`, in
/// order. Out-of-range numbers are skipped; an empty selection is an error.
/// 0 on success.
#[no_mangle]
pub extern "C" fn gp_append_pages_subset(
    handle: *mut Document,
    other_ptr: *const u8,
    other_len: usize,
    pages_ptr: *const u32,
    count: usize,
) -> i32 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return -1,
    };
    if other_ptr.is_null() || pages_ptr.is_null() {
        return -2;
    }
    let bytes = unsafe { std::slice::from_raw_parts(other_ptr, other_len) };
    let pages = unsafe { std::slice::from_raw_parts(pages_ptr, count) };
    match doc.append_pages_from_subset(bytes, pages) {
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

/// Resize a page's `/MediaBox` to `width`×`height` points (box geometry only —
/// the content is not scaled; use [`gp_scale_page_content`] for that). 0 on
/// success.
#[no_mangle]
pub extern "C" fn gp_resize_page(handle: *mut Document, page: u32, width: f64, height: f64) -> i32 {
    edit(handle, |doc| doc.resize_page(page, width, height))
}

/// Scale a page's **content** uniformly by `factor` about the page origin (ISO
/// 32000-1 §8.3.4): the content stream is wrapped in `q <factor 0 0 factor 0 0>
/// cm … Q`, the `/MediaBox`/`/CropBox` (+ any declared production boxes) are
/// scaled, and every annotation `/Rect` is scaled so appearances stay aligned
/// (§12.5.5). `factor` must be finite and positive. 0 on success.
#[no_mangle]
pub extern "C" fn gp_scale_page_content(handle: *mut Document, page: u32, factor: f64) -> i32 {
    edit(handle, |doc| doc.scale_page_content(page, factor))
}

/// Anisotropic [`gp_scale_page_content`]: scale a page's content by `sx`
/// horizontally and `sy` vertically about the origin, scaling the boxes and
/// annotation `/Rect`s by the same factors. Both must be finite and positive.
/// 0 on success.
#[no_mangle]
pub extern "C" fn gp_scale_page_content_xy(
    handle: *mut Document,
    page: u32,
    sx: f64,
    sy: f64,
) -> i32 {
    edit(handle, |doc| doc.scale_page_content_xy(page, sx, sy))
}

/// Scale a page's content to **fit within** `width`×`height` points (shrink- or
/// grow-to-fit), preserving aspect ratio. Returns the uniform factor applied
/// (positive), or a negative value on error (bad page number, non-positive
/// target, or zero-area page).
#[no_mangle]
pub extern "C" fn gp_scale_page_to(
    handle: *mut Document,
    page: u32,
    width: f64,
    height: f64,
) -> f64 {
    match unsafe { handle.as_mut() } {
        Some(doc) => doc.scale_page_to(page, width, height).unwrap_or(-1.0),
        None => -1.0,
    }
}

/// Set a page's `/UserUnit` (ISO 32000-1 §14.11.2) — large-format authoring: one
/// default user-space unit becomes `unit`⁄72 inch. `1.0` (the default) removes
/// the key. `unit` must be finite and positive. 0 on success.
#[no_mangle]
pub extern "C" fn gp_set_user_unit(handle: *mut Document, page: u32, unit: f64) -> i32 {
    edit(handle, |doc| doc.set_user_unit(page, unit))
}

/// A page's `/UserUnit` (default `1.0` when absent), or a negative value on error
/// (bad page number).
#[no_mangle]
pub extern "C" fn gp_page_user_unit(handle: *mut Document, page: u32) -> f64 {
    match unsafe { handle.as_ref() } {
        Some(doc) => doc.page_user_unit(page).unwrap_or(-1.0),
        None => -1.0,
    }
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

/// N-up imposition: place the **content** of `source` (1-based) scaled and
/// translated onto `target` (1-based) as a Form XObject (ISO 32000-1 §8.10).
/// `(x, y)` is where the visible page's lower-left lands; `(scale_x, scale_y)`
/// scale the page as displayed (the source `/MediaBox` origin and `/Rotate` are
/// absorbed into the placement matrix). Composable — call repeatedly to build a
/// 2-up/4-up sheet. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_place_page(
    handle: *mut Document,
    target: u32,
    source: u32,
    x: f64,
    y: f64,
    scale_x: f64,
    scale_y: f64,
) -> i32 {
    edit(handle, |doc| {
        doc.place_page(target, source, x, y, scale_x, scale_y)
    })
}

/// Place `source` (1-based) onto `target` (1-based) using an explicit
/// content-stream matrix `[a b c d e f]` — the low-level primitive behind
/// [`gp_place_page`] for full control of the affine (no origin/rotation
/// normalization: identity draws the source 1:1 at the target origin). 0 on
/// success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_place_page_matrix(
    handle: *mut Document,
    target: u32,
    source: u32,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
) -> i32 {
    edit(handle, |doc| {
        doc.place_page_matrix(target, source, [a, b, c, d, e, f])
            .map(|_| ())
    })
}

/// Impose **all** pages `cols × rows` per sheet onto freshly added sheets
/// (2-up/4-up/booklet thumbnails/contact sheets). Each source is scaled to fit
/// its cell (aspect preserved) and centred; `sheet_w`/`sheet_h`/`margin`/`gutter`
/// are points. The originals are dropped, leaving only the imposed sheets.
/// Returns the number of sheets produced, or a negative error code.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_n_up(
    handle: *mut Document,
    cols: u32,
    rows: u32,
    sheet_w: f64,
    sheet_h: f64,
    margin: f64,
    gutter: f64,
) -> i32 {
    match unsafe { handle.as_mut() } {
        Some(doc) => {
            let opts = gigapdf_core::NupOptions {
                sheet_width: sheet_w,
                sheet_height: sheet_h,
                margin,
                gutter,
            };
            match doc.n_up(cols, rows, &opts) {
                Ok(sheets) => sheets as i32,
                Err(_) => -3,
            }
        }
        None => -1,
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
    let fallback = "{\"width\":0,\"height\":0,\"rotation\":0,\"mediaBox\":[0,0,0,0]}".to_string();
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => match doc.page_info(page) {
            Ok((w, h, r)) => {
                let mb = doc.page_media_box(page).unwrap_or([0.0, 0.0, w, h]);
                format!(
                    "{{\"width\":{w},\"height\":{h},\"rotation\":{r},\"mediaBox\":[{},{},{},{}]}}",
                    mb[0], mb[1], mb[2], mb[3]
                )
            }
            Err(_) => fallback,
        },
        None => fallback,
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// A page's margins as JSON `{"top":…,"right":…,"bottom":…,"left":…}` (points):
/// the gap between `/CropBox` and `/MediaBox` when a CropBox exists, else
/// estimated from the content bounding box. Buffer-returning (host frees).
#[no_mangle]
pub extern "C" fn gp_page_margins(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let fallback = "{\"top\":0,\"right\":0,\"bottom\":0,\"left\":0}".to_string();
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => match doc.page_margins(page) {
            Ok(m) => format!(
                "{{\"top\":{},\"right\":{},\"bottom\":{},\"left\":{}}}",
                m.top, m.right, m.bottom, m.left
            ),
            Err(_) => fallback,
        },
        None => fallback,
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Set a page's margins (points) by insetting its `/CropBox` from the
/// `/MediaBox`. Returns `0` on success, `<0` on error.
#[no_mangle]
pub extern "C" fn gp_set_page_margins(
    handle: *mut Document,
    page: u32,
    top: f64,
    right: f64,
    bottom: f64,
    left: f64,
) -> i32 {
    let m = Margins {
        top,
        right,
        bottom,
        left,
    };
    edit(handle, |doc| doc.set_page_margins(page, m))
}

/// A page's five boundary boxes (ISO 32000-1 §14.11.2) as JSON. Each box is the
/// effective rectangle `[x0,y0,x1,y1]` in points (inheritance + the per-box
/// default chain applied), and `declared` flags which boxes are explicitly
/// present on the page (vs inherited/defaulted):
/// `{"media":[…],"crop":[…],"bleed":[…],"trim":[…],"art":[…],
///   "declared":{"media":bool,"crop":bool,"bleed":bool,"trim":bool,"art":bool}}`.
/// Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_page_boxes_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let fallback = "{\"media\":[0,0,0,0],\"crop\":[0,0,0,0],\"bleed\":[0,0,0,0],\
        \"trim\":[0,0,0,0],\"art\":[0,0,0,0],\"declared\":{\"media\":false,\"crop\":false,\
        \"bleed\":false,\"trim\":false,\"art\":false}}"
        .to_string();
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => match doc.page_boxes(page) {
            Ok(b) => {
                let r = |v: [f64; 4]| format!("[{},{},{},{}]", v[0], v[1], v[2], v[3]);
                let d = b.declared;
                format!(
                    "{{\"media\":{},\"crop\":{},\"bleed\":{},\"trim\":{},\"art\":{},\
                     \"declared\":{{\"media\":{},\"crop\":{},\"bleed\":{},\"trim\":{},\"art\":{}}}}}",
                    r(b.media),
                    r(b.crop),
                    r(b.bleed),
                    r(b.trim),
                    r(b.art),
                    d.media,
                    d.crop,
                    d.bleed,
                    d.trim,
                    d.art,
                )
            }
            Err(_) => fallback,
        },
        None => fallback,
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Set one of a page's boundary boxes to `[x0,y0,x1,y1]` (points). `kind` is
/// `0`=media `1`=crop `2`=bleed `3`=trim `4`=art. The rectangle is normalised
/// (so reversed corners are accepted) and sibling boxes are preserved. Returns
/// `0` on success, `-1` null handle, `-3` invalid kind / degenerate box / bad page.
#[no_mangle]
pub extern "C" fn gp_set_page_box(
    handle: *mut Document,
    page: u32,
    kind: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
) -> i32 {
    edit(handle, |doc| {
        let kind = match kind {
            0 => PageBox::Media,
            1 => PageBox::Crop,
            2 => PageBox::Bleed,
            3 => PageBox::Trim,
            4 => PageBox::Art,
            other => {
                return Err(gigapdf_core::EngineError::InvalidArgument(format!(
                    "unknown page-box kind {other} (expected 0..=4)"
                )))
            }
        };
        doc.set_page_box(page, kind, [x0, y0, x1, y1])
    })
}

// ─── page transitions (/Trans + /Dur, ISO 32000-1 §12.4.4) ───────────────────

/// Map the `style` index used across the C ABI to a [`TransitionStyle`]
/// (matches the SDK `PAGE_TRANSITION_STYLES` order).
fn transition_style_from_index(style: u32) -> Option<TransitionStyle> {
    Some(match style {
        0 => TransitionStyle::Split,
        1 => TransitionStyle::Blinds,
        2 => TransitionStyle::Box,
        3 => TransitionStyle::Wipe,
        4 => TransitionStyle::Dissolve,
        5 => TransitionStyle::Glitter,
        6 => TransitionStyle::Fly,
        7 => TransitionStyle::Push,
        8 => TransitionStyle::Cover,
        9 => TransitionStyle::Uncover,
        10 => TransitionStyle::Fade,
        11 => TransitionStyle::Replace,
        _ => return None,
    })
}

/// The SDK style token for a [`TransitionStyle`] (matches `PAGE_TRANSITION_STYLES`).
fn transition_style_str(style: TransitionStyle) -> &'static str {
    match style {
        TransitionStyle::Split => "split",
        TransitionStyle::Blinds => "blinds",
        TransitionStyle::Box => "box",
        TransitionStyle::Wipe => "wipe",
        TransitionStyle::Dissolve => "dissolve",
        TransitionStyle::Glitter => "glitter",
        TransitionStyle::Fly => "fly",
        TransitionStyle::Push => "push",
        TransitionStyle::Cover => "cover",
        TransitionStyle::Uncover => "uncover",
        TransitionStyle::Fade => "fade",
        TransitionStyle::Replace => "replace",
    }
}

/// Author a presentation transition + auto-advance on `page` (1-based),
/// ISO 32000-1 §12.4.4. Scalar params encode the optional `/Trans` sub-keys and
/// the page's `/Dur`; only keys that apply to the chosen `style` are written.
///
/// - `style`: `0`=split `1`=blinds `2`=box `3`=wipe `4`=dissolve `5`=glitter
///   `6`=fly `7`=push `8`=cover `9`=uncover `10`=fade `11`=replace.
/// - `duration`: `/D` effect seconds; pass **NaN** to omit.
/// - `dimension`: `/Dm` — `-1` omit, `0`=H, `1`=V (Split/Blinds).
/// - `motion`: `/M` — `-1` omit, `0`=I, `1`=O (Split/Box).
/// - `direction`: `/Di` — `-2` omit, `-1`=`/None`, else degrees (0/90/180/270/315).
/// - `scale`: `/SS` Fly scale; pass **NaN** to omit.
/// - `fly_b`: `/B` — `-1` omit, `0`=false, `1`=true (Fly).
/// - `display_duration`: `/Dur` auto-advance seconds; pass **NaN** to omit/remove.
///
/// Returns `0` on success, `-1` null handle, `-2` unknown style, `-3` bad
/// value / bad page.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_set_page_transition(
    handle: *mut Document,
    page: u32,
    style: u32,
    duration: f64,
    dimension: i32,
    motion: i32,
    direction: i32,
    scale: f64,
    fly_b: i32,
    display_duration: f64,
) -> i32 {
    let Some(style) = transition_style_from_index(style) else {
        return -2;
    };
    let trans = PageTransition {
        style,
        duration: if duration.is_nan() {
            None
        } else {
            Some(duration)
        },
        dimension: match dimension {
            0 => Some(TransitionDimension::Horizontal),
            1 => Some(TransitionDimension::Vertical),
            _ => None,
        },
        motion: match motion {
            0 => Some(TransitionMotion::Inward),
            1 => Some(TransitionMotion::Outward),
            _ => None,
        },
        direction: match direction {
            -2 => None,
            -1 => Some(TransitionDirection::None),
            deg => Some(
                TransitionDirection::from_degrees(deg as i64).unwrap_or(TransitionDirection::None),
            ),
        },
        scale: if scale.is_nan() { None } else { Some(scale) },
        fly_area_opaque: match fly_b {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        display_duration: if display_duration.is_nan() {
            None
        } else {
            Some(display_duration)
        },
    };
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return -1,
    };
    match doc.set_page_transition(page, &trans) {
        Ok(()) => 0,
        Err(_) => -3,
    }
}

/// Remove any presentation transition (`/Trans` + `/Dur`) from `page` (1-based).
/// Returns `0` on success, `<0` on error.
#[no_mangle]
pub extern "C" fn gp_clear_page_transition(handle: *mut Document, page: u32) -> i32 {
    edit(handle, |doc| doc.clear_page_transition(page))
}

/// The presentation transition on `page` (1-based) as JSON, or `null` when the
/// page declares no `/Trans`. Shape:
/// `{"style":"wipe","duration":0.5,"dimension":null,"motion":null,
/// "direction":90,"scale":null,"flyAreaOpaque":null,"displayDuration":5}`
/// (omitted optional keys are `null`; `direction` is a number, `"none"`, or
/// `null`). Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_page_transition_json(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => match doc.page_transition(page) {
            Ok(Some(t)) => {
                let num = |v: Option<f64>| v.map_or("null".to_string(), |n| format!("{n}"));
                let dimension = match t.dimension {
                    Some(TransitionDimension::Horizontal) => "\"horizontal\"",
                    Some(TransitionDimension::Vertical) => "\"vertical\"",
                    None => "null",
                };
                let motion = match t.motion {
                    Some(TransitionMotion::Inward) => "\"inward\"",
                    Some(TransitionMotion::Outward) => "\"outward\"",
                    None => "null",
                };
                let direction = match t.direction {
                    Some(TransitionDirection::None) => "\"none\"".to_string(),
                    Some(d) => d
                        .degrees()
                        .map_or("null".to_string(), |deg| deg.to_string()),
                    None => "null".to_string(),
                };
                let fly = match t.fly_area_opaque {
                    Some(b) => b.to_string(),
                    None => "null".to_string(),
                };
                format!(
                    "{{\"style\":\"{}\",\"duration\":{},\"dimension\":{},\"motion\":{},\
                     \"direction\":{},\"scale\":{},\"flyAreaOpaque\":{},\"displayDuration\":{}}}",
                    transition_style_str(t.style),
                    num(t.duration),
                    dimension,
                    motion,
                    direction,
                    num(t.scale),
                    fly,
                    num(t.display_duration),
                )
            }
            _ => "null".to_string(),
        },
        None => "null".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// The friendly JSON style token for a page-label style (matches the SDK enum).
fn page_label_style_str(style: PageLabelStyle) -> &'static str {
    match style {
        PageLabelStyle::Decimal => "decimal",
        PageLabelStyle::RomanLower => "romanLower",
        PageLabelStyle::RomanUpper => "romanUpper",
        PageLabelStyle::AlphaLower => "alphaLower",
        PageLabelStyle::AlphaUpper => "alphaUpper",
        PageLabelStyle::None => "none",
    }
}

/// Parse a single-letter `/S` style token from the line-delimited set format
/// (`D`/`r`/`R`/`a`/`A`; anything else, e.g. `-`, means prefix-only).
fn page_label_style_from_token(tok: &str) -> PageLabelStyle {
    match tok {
        "D" => PageLabelStyle::Decimal,
        "r" => PageLabelStyle::RomanLower,
        "R" => PageLabelStyle::RomanUpper,
        "a" => PageLabelStyle::AlphaLower,
        "A" => PageLabelStyle::AlphaUpper,
        _ => PageLabelStyle::None,
    }
}

/// All page-label ranges (ISO 32000-1 §12.4.2) as JSON
/// `[{"startPage":n,"style":"decimal","prefix":"…","startNumber":k}]` (`startPage`
/// 1-based). Empty array when the document declares no `/PageLabels`. The `style`
/// is one of `decimal`/`romanLower`/`romanUpper`/`alphaLower`/`alphaUpper`/`none`.
/// Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_page_labels_json(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let mut s = String::from("[");
            for (i, r) in doc.page_labels().iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!(
                    "{{\"startPage\":{},\"style\":\"{}\",\"prefix\":",
                    r.start_page,
                    page_label_style_str(r.style)
                ));
                json_escape(&r.prefix, &mut s);
                s.push_str(&format!(",\"startNumber\":{}}}", r.start_number));
            }
            s.push(']');
            s
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Replace the document's page labels from a newline-delimited spec — one range
/// per line as `startPage<TAB>style<TAB>startNumber<TAB>prefix` (style one of
/// `D r R a A`, or `-`/empty for prefix-only; `startPage` 1-based). An **empty**
/// buffer clears all page labels. Returns `0` on success, `<0` on error.
#[no_mangle]
pub extern "C" fn gp_set_page_labels(
    handle: *mut Document,
    text_ptr: *const u8,
    text_len: usize,
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    let mut ranges: Vec<PageLabelRange> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, '\t');
        let start_page = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);
        let style = page_label_style_from_token(parts.next().unwrap_or("-"));
        let start_number = parts
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(1);
        let prefix = parts.next().unwrap_or("").to_string();
        ranges.push(PageLabelRange {
            start_page,
            style,
            prefix,
            start_number,
        });
    }
    edit(handle, |doc| doc.set_page_labels(&ranges))
}

/// The viewer-visible label string for the 1-based `page` (e.g. `iv`, `A-3`),
/// resolving the applicable `/PageLabels` range; the decimal page number when no
/// range applies. Host frees the returned buffer.
#[no_mangle]
pub extern "C" fn gp_page_label(
    handle: *const Document,
    page: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let label = match unsafe { handle.as_ref() } {
        Some(doc) => doc.page_label(page),
        None => String::new(),
    };
    unsafe { bytes_into_host(label.into_bytes(), out_len) }
}

/// Bake a running **header** from a JSON spec (`text`, `align`, `fontSize`,
/// `color`, `pageRange`, `showOnFirstPage`, `bandHeight`) onto every in-range
/// page. Idempotent (re-baking replaces the prior header). Returns `0` on
/// success, `-1` null handle, `-2` malformed JSON, `-3` bake error.
#[no_mangle]
pub extern "C" fn gp_set_header(handle: *mut Document, json_ptr: *const u8, json_len: usize) -> i32 {
    set_header_footer(handle, json_ptr, json_len, true)
}

/// Bake a running **footer** from a JSON spec onto every in-range page. The
/// footer twin of [`gp_set_header`].
#[no_mangle]
pub extern "C" fn gp_set_footer(handle: *mut Document, json_ptr: *const u8, json_len: usize) -> i32 {
    set_header_footer(handle, json_ptr, json_len, false)
}

/// Shared body for [`gp_set_header`]/[`gp_set_footer`]: parse the spec JSON and
/// bake it as a header (`header == true`) or footer.
fn set_header_footer(
    handle: *mut Document,
    json_ptr: *const u8,
    json_len: usize,
    header: bool,
) -> i32 {
    let doc = match unsafe { handle.as_mut() } {
        Some(doc) => doc,
        None => return -1,
    };
    let json = unsafe { str_arg(json_ptr, json_len) };
    let spec = match HeaderFooterSpec::from_json(json) {
        Some(spec) => spec,
        None => return -2,
    };
    let result = if header {
        doc.set_header(&spec)
    } else {
        doc.set_footer(&spec)
    };
    match result {
        Ok(()) => 0,
        Err(_) => -3,
    }
}

/// Remove every previously-baked running header from all pages. Returns `0` on
/// success, `<0` on error.
#[no_mangle]
pub extern "C" fn gp_remove_headers(handle: *mut Document) -> i32 {
    edit(handle, |doc| doc.remove_headers())
}

/// Remove every previously-baked running footer from all pages.
#[no_mangle]
pub extern "C" fn gp_remove_footers(handle: *mut Document) -> i32 {
    edit(handle, |doc| doc.remove_footers())
}

/// Detect the running header/footer already baked into this PDF, as JSON
/// `{"header":<spec|null>,"footer":<spec|null>}` — the reader counterpart of
/// [`gp_set_header`]/[`gp_set_footer`]. Each present side is a spec object
/// (same keys as the bake JSON: `text`, `align`, `fontSize`, …) with its
/// recovered `text`; an absent side is `null`. Buffer-returning (host frees).
#[no_mangle]
pub extern "C" fn gp_header_footer(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => doc.header_footer().to_json(),
        None => "{\"header\":null,\"footer\":null}".to_string(),
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

/// Rasterize a page to a PNG at `scale` device pixels per PDF point **without**
/// the page content stream's text (glyphs from `Tj`/`'`/`"`/`TJ` are suppressed);
/// gradients, shadings, images, vectors and patterns are preserved. Form-field
/// **widget** appearances are omitted (the editor re-shows their values as an
/// editable overlay, so baking them would double every field); other annotation
/// appearances are still painted. Lets the editor lay real, editable text over a
/// text-free raster background. Buffer-returning (host frees); null on error.
#[no_mangle]
pub extern "C" fn gp_render_page_no_text(
    handle: *const Document,
    page: u32,
    scale: f64,
    out_len: *mut usize,
) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => match doc.render_page_no_text(page, scale) {
            Ok(png) => unsafe { bytes_into_host(png, out_len) },
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

/// Rasterize `page` to a PNG at `scale` while **omitting** the top-level element
/// indices in `indices_ptr`/`indices_len` (a `u32` array). Each excluded element
/// paints nothing (fills/strokes/shadings/images/text); everything else renders
/// normally. Like `gp_render_page_no_text`, form-field **widget** appearances are
/// omitted (the editor re-shows them as an editable overlay); other annotation
/// appearances are painted. Lets the host paint a background without specific
/// elements and overlay live editable versions. Buffer-returning (host frees);
/// null on error.
#[no_mangle]
pub extern "C" fn gp_render_page_excluding(
    handle: *const Document,
    page: u32,
    indices_ptr: *const u32,
    indices_len: usize,
    scale: f64,
    out_len: *mut usize,
) -> *mut u8 {
    let indices: Vec<usize> = if indices_ptr.is_null() || indices_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(indices_ptr, indices_len) }
            .iter()
            .map(|&v| v as usize)
            .collect()
    };
    match unsafe { handle.as_ref() } {
        Some(doc) => match doc.render_page_excluding(page, &indices, scale) {
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

/// Decode a still **AVIF** (AV1 intra) to `[w: u32 LE][h: u32 LE][rgba]` with the
/// engine's native AV1 decoder — no third-party library. Empty for an
/// unsupported/malformed stream. Host frees the result.
#[no_mangle]
pub extern "C" fn gp_decode_avif(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    let out = if ptr.is_null() {
        Vec::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        frame_image(gigapdf_core::raster::avif::decode_avif(bytes))
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

/// Map a `pdf_version` selector to a [`gigapdf_core::serialize::PdfVersion`]:
/// `0` = PDF 1.7 (default), `1` = PDF 2.0; any other value falls back to 1.7.
fn pdf_version_from(code: i32) -> gigapdf_core::serialize::PdfVersion {
    match code {
        1 => gigapdf_core::serialize::PdfVersion::V2_0,
        _ => gigapdf_core::serialize::PdfVersion::V1_7,
    }
}

/// Serialize with PDF 1.5+ object streams + a cross-reference stream (ISO 32000-1
/// §7.5.7/§7.5.8) — the most compact output. `object_streams != 0` packs
/// non-stream objects into `/ObjStm`s (implies a cross-reference stream);
/// `xref_streams != 0` alone writes a `/XRef` stream with classic object bodies.
/// Both `0` ⇒ `gp_save_compressed`. `pdf_version` selects the header banner
/// (`0` = 1.7, `1` = 2.0). Linearization is not performed.
#[no_mangle]
pub extern "C" fn gp_save_optimized(
    handle: *const Document,
    object_streams: i32,
    xref_streams: i32,
    pdf_version: i32,
    out_len: *mut usize,
) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe {
            bytes_into_host(
                doc.save_optimized_with_version(
                    object_streams != 0,
                    xref_streams != 0,
                    pdf_version_from(pdf_version),
                ),
                out_len,
            )
        },
        None => std::ptr::null_mut(),
    }
}

/// Serialize as a **linearized** ("Fast Web View") PDF per ISO 32000-1 Annex F:
/// the first page and the objects needed to render it — plus a `/Linearized`
/// parameter dictionary and a primary hint stream — are written at the front of
/// the file so a web viewer can display page 1 before the rest downloads. Streams
/// are Flate-compressed and embedded fonts subset, like `gp_save_compressed`.
/// `pdf_version` selects the header banner (`0` = 1.7, `1` = 2.0). Falls back to
/// the plain writer if the document cannot be linearized.
#[no_mangle]
pub extern "C" fn gp_to_linearized(
    handle: *const Document,
    pdf_version: i32,
    out_len: *mut usize,
) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe {
            bytes_into_host(
                doc.to_linearized_with_version(pdf_version_from(pdf_version)),
                out_len,
            )
        },
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

/// Map a level tag (`"pdfa-1b"`, `"pdfa-2b"`, `"pdfa-2u"`, `"pdfa-3b"`; bare
/// `"1b"`/`"2b"`/`"2u"`/`"3b"` also accepted) to a [`PdfaLevel`]. Empty or
/// unrecognized → the default `Pdfa2b`, so an absent argument is back-compatible.
fn parse_pdfa_level(tag: &str) -> gigapdf_core::convert::pdfa::PdfaLevel {
    use gigapdf_core::convert::pdfa::PdfaLevel;
    match tag.trim().trim_start_matches("pdfa-") {
        "1b" => PdfaLevel::Pdfa1b,
        "1a" => PdfaLevel::Pdfa1a,
        "2u" => PdfaLevel::Pdfa2u,
        "2a" => PdfaLevel::Pdfa2a,
        "3b" => PdfaLevel::Pdfa3b,
        _ => PdfaLevel::Pdfa2b,
    }
}

/// Re-serialize with PDF/A archival metadata (XMP + sRGB OutputIntent + ID) at
/// the level named by `(level_ptr, level_len)`. See [`parse_pdfa_level`] for the
/// accepted tags; an empty argument defaults to PDF/A-2b.
#[no_mangle]
pub extern "C" fn gp_to_pdfa(
    handle: *const Document,
    level_ptr: *const u8,
    level_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => {
            let level = parse_pdfa_level(unsafe { str_arg(level_ptr, level_len) });
            unsafe { bytes_into_host(doc.to_pdfa_level(level), out_len) }
        }
        None => std::ptr::null_mut(),
    }
}

// ─── unified editable model: produce / edit / export ─────────────────────────
//
// The format-neutral `model::Document` is carried across the FFI boundary as its
// stable JSON envelope (UTF-8 bytes). The host parses it, edits it (directly or
// via `gp_model_apply_ops`), then exports it to any target format. Every export
// fn parses the JSON model, runs the matching `*_from_model` converter, and
// returns the bytes; bad JSON / null input → null pointer + `out_len = 0`.

/// Reconstruct the unified editable model from an open PDF handle. Returns the
/// model as JSON (the `model::Document` envelope). Null on null handle.
#[no_mangle]
pub extern "C" fn gp_model_from_pdf(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    match unsafe { handle.as_ref() } {
        Some(doc) => unsafe {
            bytes_into_host(doc.reconstruct_model().to_json().into_bytes(), out_len)
        },
        None => std::ptr::null_mut(),
    }
}

/// Lower an Office document (DOCX/XLSX/PPTX/ODT/ODS/ODP, auto-detected) at
/// `(ptr, len)` into the unified model, returned as JSON. Null on unrecognized
/// or empty input.
#[no_mangle]
pub extern "C" fn gp_model_from_office(
    ptr: *const u8,
    len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    if ptr.is_null() || len == 0 {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    match gigapdf_core::convert::office_to_model(bytes) {
        Some(model) => unsafe { bytes_into_host(model.to_json().into_bytes(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Lower an HTML string at `(ptr, len)` into the unified model, returned as JSON.
#[no_mangle]
pub extern "C" fn gp_model_from_html(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    let html = unsafe { str_arg(ptr, len) };
    let model = gigapdf_core::convert::html_to_model(html);
    unsafe { bytes_into_host(model.to_json().into_bytes(), out_len) }
}

/// Lower a Markdown string at `(ptr, len)` into the unified model, returned as
/// JSON (CommonMark-ish: headings, lists, tables, code, emphasis, links).
#[no_mangle]
pub extern "C" fn gp_model_from_md(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    let md = unsafe { str_arg(ptr, len) };
    let model = gigapdf_core::convert::md_to_model(md);
    unsafe { bytes_into_host(model.to_json().into_bytes(), out_len) }
}

/// Lower a CSV buffer at `(ptr, len)` (RFC 4180, auto-detected delimiter) into
/// the unified model — a single editable table — returned as JSON. Null on input
/// with no parseable fields.
#[no_mangle]
pub extern "C" fn gp_model_from_csv(ptr: *const u8, len: usize, out_len: *mut usize) -> *mut u8 {
    if ptr.is_null() || len == 0 {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    match gigapdf_core::convert::csv_to_model(bytes) {
        Some(model) => unsafe { bytes_into_host(model.to_json().into_bytes(), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Apply a batch of edit ops to a model. `(model_ptr, model_len)` is the model
/// JSON, `(ops_ptr, ops_len)` is a JSON array of ops (see `model::edit`).
/// Returns the edited model as JSON. Null when the model JSON is malformed.
/// (Unparseable individual ops and out-of-range addresses are silently skipped.)
///
/// This entry point is op-agnostic: every `ModelOp` variant — run/block/text
/// edits, `setCellText`/`setSheetCell`, and the structural table/sheet ops
/// (`insertTableRow`/`deleteTableRow`/`insertTableColumn`/`deleteTableColumn`/
/// `setCellSpan`, `insertSheetRow`/`deleteSheetRow`/`insertSheetColumn`/
/// `deleteSheetColumn`) — is dispatched here via `parse_ops`, so no per-op FFI
/// function is needed.
#[no_mangle]
pub extern "C" fn gp_model_apply_ops(
    model_ptr: *const u8,
    model_len: usize,
    ops_ptr: *const u8,
    ops_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let model_json = unsafe { str_arg(model_ptr, model_len) };
    let mut model = match gigapdf_core::model::Document::from_json(model_json) {
        Some(m) => m,
        None => return std::ptr::null_mut(),
    };
    let ops_json = unsafe { str_arg(ops_ptr, ops_len) };
    let ops = gigapdf_core::model::parse_ops(ops_json);
    gigapdf_core::model::apply_ops(&mut model, &ops);
    unsafe { bytes_into_host(model.to_json().into_bytes(), out_len) }
}

/// Shared body for the model exporters: parse the model JSON, run `convert` on
/// it, return the bytes. Null pointer + `out_len = 0` on malformed model JSON.
fn model_export(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
    convert: impl FnOnce(&gigapdf_core::model::Document) -> Vec<u8>,
) -> *mut u8 {
    let json = unsafe { str_arg(model_ptr, model_len) };
    match gigapdf_core::model::Document::from_json(json) {
        Some(model) => unsafe { bytes_into_host(convert(&model), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Export a model (JSON) to an editable Word document (`.docx`).
#[no_mangle]
pub extern "C" fn gp_model_to_docx(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::export_model::docx_from_model(m)
    })
}

/// Export a model (JSON) to an Excel workbook (`.xlsx`).
#[no_mangle]
pub extern "C" fn gp_model_to_xlsx(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::export_model::xlsx_from_model(m)
    })
}

/// Export a model (JSON) to a PowerPoint presentation (`.pptx`).
#[no_mangle]
pub extern "C" fn gp_model_to_pptx(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::export_model::pptx_from_model(m)
    })
}

/// Export a model (JSON) to an OpenDocument Text (`.odt`).
#[no_mangle]
pub extern "C" fn gp_model_to_odt(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::export_model::odt_from_model(m)
    })
}

/// Export a model (JSON) to an OpenDocument Spreadsheet (`.ods`).
#[no_mangle]
pub extern "C" fn gp_model_to_ods(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::export_model::ods_from_model(m)
    })
}

/// Export a model (JSON) to an OpenDocument Presentation (`.odp`).
#[no_mangle]
pub extern "C" fn gp_model_to_odp(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::export_model::odp_from_model(m)
    })
}

/// Export a model (JSON) to standalone HTML (returned as UTF-8 string bytes).
#[no_mangle]
pub extern "C" fn gp_model_to_html(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::web::html_from_model(m).into_bytes()
    })
}

/// Export a model (JSON) to RTF (returned as UTF-8 string bytes).
#[no_mangle]
pub extern "C" fn gp_model_to_rtf(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::reverse::rtf_from_model(m)
    })
}

/// Export a model (JSON) to Markdown (returned as UTF-8 string bytes).
#[no_mangle]
pub extern "C" fn gp_model_to_md(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::export_model::markdown_from_model(m).into_bytes()
    })
}

/// Export a model (JSON) to CSV (returned as UTF-8 string bytes).
#[no_mangle]
pub extern "C" fn gp_model_to_csv(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::export_model::csv_from_model(m).into_bytes()
    })
}

/// Export a model (JSON) to an EPUB e-book (`.epub`).
#[no_mangle]
pub extern "C" fn gp_model_to_epub(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::export_model::epub_from_model(m)
    })
}

/// Export a model (JSON) back to a PDF.
#[no_mangle]
pub extern "C" fn gp_model_to_pdf(
    model_ptr: *const u8,
    model_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    model_export(model_ptr, model_len, out_len, |m| {
        gigapdf_core::convert::project::pdf_from_model(m)
    })
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
    let out = gigapdf_core::js::eval(src);
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
    resources_ptr: *const u8,
    resources_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    let html = unsafe { str_arg(html_ptr, html_len) };
    let blob: &[u8] = if fonts_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(fonts_ptr, fonts_len) }
    };
    let fonts = parse_font_blob(blob);
    let res_blob: &[u8] = if resources_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(resources_ptr, resources_len) }
    };
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
        resources: parse_resources_blob(res_blob),
    };
    let pdf = gigapdf_core::html::render_with(html, &fonts, &opts);
    unsafe { bytes_into_host(pdf, out_len) }
}

/// Decode the packed resources blob (host-fetched external URLs) passed to
/// [`gp_html_render_opts`]: little-endian `u32 count`, then per entry
/// `u32 url_len, url utf8, u32 data_len, data bytes`.
fn parse_resources_blob(b: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn rd_u32(b: &[u8], i: &mut usize) -> Option<u32> {
        let v = b.get(*i..*i + 4)?;
        *i += 4;
        Some(u32::from_le_bytes(v.try_into().ok()?))
    }
    let mut out = std::collections::BTreeMap::new();
    let mut i = 0;
    let Some(count) = rd_u32(b, &mut i) else {
        return out;
    };
    for _ in 0..count {
        let Some(ul) = rd_u32(b, &mut i) else { break };
        let Some(url) = b.get(i..i + ul as usize) else {
            break;
        };
        i += ul as usize;
        let Some(dl) = rd_u32(b, &mut i) else { break };
        let Some(data) = b.get(i..i + dl as usize) else {
            break;
        };
        i += dl as usize;
        out.insert(String::from_utf8_lossy(url).into_owned(), data.to_vec());
    }
    out
}

/// HTML rendering engine — phase 1 (unified): every external resource the
/// document needs as JSON. Fonts: `{kind:"font",family,weight,italic,url}`;
/// images: `{kind:"image",url}`. One discovery call for the host's fetch loop.
/// Buffer-returning.
#[no_mangle]
pub extern "C" fn gp_html_needed_resources(
    html_ptr: *const u8,
    html_len: usize,
    header_ptr: *const u8,
    header_len: usize,
    footer_ptr: *const u8,
    footer_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    use gigapdf_core::html::ResourceNeed;
    let html = unsafe { str_arg(html_ptr, html_len) };
    let header = unsafe { opt_str_arg(header_ptr, header_len) };
    let footer = unsafe { opt_str_arg(footer_ptr, footer_len) };
    let needs = gigapdf_core::html::needed_resources(html, header, footer);
    let mut s = String::from("[");
    for (i, n) in needs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match n {
            ResourceNeed::Font(f) => {
                s.push_str(&format!(
                    "{{\"kind\":\"font\",\"weight\":{},\"italic\":{},\"family\":",
                    f.weight, f.italic
                ));
                json_escape(&f.family, &mut s);
                s.push_str(",\"url\":");
                json_escape(&f.url, &mut s);
                s.push('}');
            }
            ResourceNeed::Image(url) => {
                s.push_str("{\"kind\":\"image\",\"url\":");
                json_escape(url, &mut s);
                s.push('}');
            }
        }
    }
    s.push(']');
    unsafe { bytes_into_host(s.into_bytes(), out_len) }
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

/// Office (DOCX/ODT/PPTX/XLSX/ODS/ODP) phase-1 fonts: the families the container
/// **references but doesn't embed**, as a JSON array of `{family, weight, italic,
/// url}` — the set the host should fetch (Google Fonts) before [`gp_office_to_pdf`]
/// so styled runs lay out with the right metrics. Faces the container embeds
/// itself are de-obfuscated and used directly, so they're excluded here. Returns
/// `[]` JSON for a recognized archive that needs nothing, and null for an
/// unrecognized one. Host frees the buffer.
#[no_mangle]
pub extern "C" fn gp_office_needed_fonts(
    bytes_ptr: *const u8,
    bytes_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    if bytes_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
    match gigapdf_core::convert::reverse::office_needed_fonts(bytes) {
        Some(reqs) => unsafe { bytes_into_host(font_reqs_json(&reqs), out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Office (DOCX/ODT/PPTX/XLSX/ODS/ODP) → PDF, auto-detected, phase 2 of the
/// two-phase font flow: `fonts` carries the host-fetched faces for the families
/// [`gp_office_needed_fonts`] reported (e.g. Carlito for a Calibri reference) so
/// styled runs lay out with the right metrics. `fonts` is the SAME packed blob as
/// [`gp_html_render`] (little-endian `u32 count`, then per font `u32 family_len,
/// family utf8, u16 weight, u8 italic, u32 ttf_len, ttf bytes`); a null/empty blob
/// behaves like [`gp_office_to_pdf`]. Faces the container embeds itself win on
/// conflict. Null if the bytes are unrecognized. Host frees the buffer.
#[no_mangle]
pub extern "C" fn gp_office_to_pdf_with_fonts(
    bytes_ptr: *const u8,
    bytes_len: usize,
    fonts_ptr: *const u8,
    fonts_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    if bytes_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
    let blob: &[u8] = if fonts_ptr.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(fonts_ptr, fonts_len) }
    };
    let fonts = parse_font_blob(blob);
    match gigapdf_core::convert::reverse::office_to_pdf_with_fonts(bytes, &fonts) {
        Some(pdf) => unsafe { bytes_into_host(pdf, out_len) },
        None => std::ptr::null_mut(),
    }
}

/// Image (PNG/JPEG/GIF/WebP/AVIF) → one-page PDF, format auto-detected (the
/// image centred and fit on an A4 page). Null if the bytes are unrecognized.
#[no_mangle]
pub extern "C" fn gp_image_to_pdf(
    bytes_ptr: *const u8,
    bytes_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    if bytes_ptr.is_null() {
        return std::ptr::null_mut();
    }
    let bytes = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
    match gigapdf_core::convert::reverse::image_to_pdf(bytes) {
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

/// Like `gp_add_text` but also bakes text decorations: pass `underline` and/or
/// `strikethrough` non-zero to draw the corresponding rule (filled in the text
/// colour, spanning the run). 0/0 is identical to `gp_add_text`. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_text_styled(
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
    underline: i32,
    strikethrough: i32,
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    edit(handle, |doc| {
        doc.add_text_styled(
            page,
            x,
            y,
            size,
            text,
            font_obj,
            unpack_rgb(rgb),
            opacity,
            rotation_deg,
            underline != 0,
            strikethrough != 0,
        )
    })
}

/// Like `gp_add_text_standard` but also bakes text decorations: pass `underline`
/// and/or `strikethrough` non-zero to draw the corresponding rule (filled in the
/// text colour). 0/0 is identical to `gp_add_text_standard`. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_text_standard_styled(
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
    underline: i32,
    strikethrough: i32,
) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    let font = unsafe { str_arg(font_ptr, font_len) };
    edit(handle, |doc| {
        doc.add_text_standard_styled(
            page,
            x,
            y,
            size,
            text,
            font,
            unpack_rgb(rgb),
            opacity,
            rotation_deg,
            underline != 0,
            strikethrough != 0,
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
/// `end_arrow != 0` draws an open arrowhead at the `(x2,y2)` end.
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
    end_arrow: u32,
) -> i32 {
    edit(handle, |doc| {
        doc.add_line_annotation(page, x1, y1, x2, y2, unpack_rgb(rgb), line_width, end_arrow != 0)
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

/// Add a Circle (ellipse) annotation inscribed in `[x0,y0,x1,y1]`. `stroke_rgb`
/// (border, `/C`) and `fill_rgb` (interior, `/IC`) are packed `0xRRGGBB`, each
/// gated by its `has_*` flag. 0 on success.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_circle_annot(
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
        doc.add_circle_annotation(page, [x0, y0, x1, y1], stroke, fill, line_width)
    })
}

/// Add a Polygon annotation through `coords` (flat `f64` `x,y` pairs;
/// `coord_count` = twice the vertex count). `stroke_rgb`/`fill_rgb` packed
/// `0xRRGGBB`, gated by `has_*`. 0 on success, `-2` bad input.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_polygon_annot(
    handle: *mut Document,
    page: u32,
    coords_ptr: *const f64,
    coord_count: usize,
    stroke_rgb: u32,
    has_stroke: i32,
    fill_rgb: u32,
    has_fill: i32,
    line_width: f64,
) -> i32 {
    if coords_ptr.is_null() || coord_count < 4 {
        return -2;
    }
    let coords = unsafe { std::slice::from_raw_parts(coords_ptr, coord_count) };
    let verts: Vec<(f64, f64)> = coords.chunks_exact(2).map(|c| (c[0], c[1])).collect();
    let stroke = (has_stroke != 0).then(|| unpack_rgb(stroke_rgb));
    let fill = (has_fill != 0).then(|| unpack_rgb(fill_rgb));
    edit(handle, |doc| {
        doc.add_polygon_annotation(page, &verts, stroke, fill, line_width)
    })
}

/// Add a PolyLine annotation through `coords` (flat `f64` `x,y` pairs). `rgb`
/// packed `0xRRGGBB`. 0 on success, `-2` bad input.
#[no_mangle]
pub extern "C" fn gp_add_polyline_annot(
    handle: *mut Document,
    page: u32,
    coords_ptr: *const f64,
    coord_count: usize,
    rgb: u32,
    line_width: f64,
) -> i32 {
    if coords_ptr.is_null() || coord_count < 4 {
        return -2;
    }
    let coords = unsafe { std::slice::from_raw_parts(coords_ptr, coord_count) };
    let verts: Vec<(f64, f64)> = coords.chunks_exact(2).map(|c| (c[0], c[1])).collect();
    edit(handle, |doc| {
        doc.add_polyline_annotation(page, &verts, unpack_rgb(rgb), line_width)
    })
}

/// Add a Caret annotation (a small upward wedge) in `[x0,y0,x1,y1]`. `rgb` packed
/// `0xRRGGBB`. 0 on success.
#[no_mangle]
pub extern "C" fn gp_add_caret_annot(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    rgb: u32,
) -> i32 {
    edit(handle, |doc| {
        doc.add_caret_annotation(page, [x0, y0, x1, y1], unpack_rgb(rgb))
    })
}

/// Regenerate the appearance (`/AP /N`) of the 0-based `index` annotation on
/// `page` from its stored geometry. 0 on success, `-1` null handle, `-3` bad
/// index or a subtype whose appearance can't be reconstructed.
#[no_mangle]
pub extern "C" fn gp_regenerate_appearance(handle: *mut Document, page: u32, index: usize) -> i32 {
    edit(handle, |doc| doc.regenerate_appearance(page, index))
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

/// Inline a page's form XObjects (`/Subtype /Form` invoked via `Do`) into its
/// content stream, de-sharing each placement so the former form text becomes
/// editable page runs. Returns the number of form XObjects inlined, or a
/// negative error code. Distinct from [`gp_flatten_form`] (AcroForm fields).
#[no_mangle]
pub extern "C" fn gp_flatten_form_xobjects(handle: *mut Document, page: u32) -> i32 {
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.flatten_form_xobjects(page) {
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

/// The document's XMP metadata packet (catalog `/Metadata`, decoded) as raw
/// bytes; an **empty** buffer when the document has no XMP. Host frees.
#[no_mangle]
pub extern "C" fn gp_get_xmp(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let xmp = match unsafe { handle.as_ref() } {
        Some(doc) => doc.xmp().unwrap_or_default(),
        None => Vec::new(),
    };
    unsafe { bytes_into_host(xmp, out_len) }
}

/// Replace (or create) the document's XMP metadata stream (catalog `/Metadata`,
/// stored uncompressed). Returns `0` on success, `-1` null handle, `-3` on error.
#[no_mangle]
pub extern "C" fn gp_set_xmp(handle: *mut Document, ptr: *const u8, len: usize) -> i32 {
    let bytes = unsafe { opt_slice(ptr, len) };
    edit(handle, |doc| doc.set_xmp(bytes))
}

/// Set the standard document-information fields from a JSON object
/// (`{title?,author?,subject?,keywords?,creator?,producer?,creationDate?,modDate?}`),
/// writing **both** the `/Info` dictionary and a synced XMP `/Metadata` stream.
/// Absent keys are left unchanged (a partial update). Returns `0`/`-1`/`-3`.
#[no_mangle]
pub extern "C" fn gp_set_info_json(handle: *mut Document, ptr: *const u8, len: usize) -> i32 {
    let json = unsafe { str_arg(ptr, len) };
    let fields = InfoFields::from_json(json);
    edit(handle, |doc| doc.set_info(&fields))
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

/// Embed `bytes` as a document-level file attachment named `name`
/// (`/Names /EmbeddedFiles`). `mime` and `desc` are optional (empty = omitted);
/// re-using a `name` replaces that attachment. Returns `0` on success, `-1` null
/// handle, `-3` on error (e.g. empty name).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_attachment(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
    bytes_ptr: *const u8,
    bytes_len: usize,
    mime_ptr: *const u8,
    mime_len: usize,
    desc_ptr: *const u8,
    desc_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let bytes = unsafe { opt_slice(bytes_ptr, bytes_len) };
    let mime = unsafe { opt_str_arg(mime_ptr, mime_len) };
    let desc = unsafe { opt_str_arg(desc_ptr, desc_len) };
    edit(handle, |doc| doc.add_attachment(name, bytes, mime, desc))
}

/// Embed `bytes` as an **associated file** (`/AF`, PDF/A-3 — Factur-X/ZUGFeRD).
/// Like [`gp_add_attachment`] plus `relationship`: `0`=source `1`=data
/// `2`=alternative `3`=supplement `4`=unspecified. Returns `0`/`-1`/`-3`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_associated_file(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
    bytes_ptr: *const u8,
    bytes_len: usize,
    mime_ptr: *const u8,
    mime_len: usize,
    desc_ptr: *const u8,
    desc_len: usize,
    relationship: u32,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let bytes = unsafe { opt_slice(bytes_ptr, bytes_len) };
    let mime = unsafe { opt_str_arg(mime_ptr, mime_len) };
    let desc = unsafe { opt_str_arg(desc_ptr, desc_len) };
    edit(handle, |doc| {
        let rel = match relationship {
            0 => AfRelationship::Source,
            1 => AfRelationship::Data,
            2 => AfRelationship::Alternative,
            3 => AfRelationship::Supplement,
            _ => AfRelationship::Unspecified,
        };
        doc.add_associated_file(name, bytes, mime, desc, rel)
    })
}

/// Remove the attachment named `name`. Returns `1` if one was removed, `0` if no
/// attachment had that name, `-1` null handle, `-3` on error.
#[no_mangle]
pub extern "C" fn gp_remove_attachment(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.remove_attachment(name) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(_) => -3,
        },
        None => -1,
    }
}

/// Mark the document as an embedded-file **portfolio** / collection by writing
/// the catalog `/Collection` (ISO 32000-1 §7.11.6) from a JSON config. The files
/// must already be embedded (`gp_add_attachment`). `json` is a
/// [`CollectionConfig`] object:
/// `{view, schema:[{key,name?,subtype?,order?,visible?}], sort:{field,ascending?}|null,
///   defaultFile?|null, items:[{file,values:{key:val,…}}]}` —
/// `view` ∈ `details`/`tile`/`hidden`; field `subtype` ∈
/// `text`/`date`/`number`/`filename`/`description`/`size`/`creationDate`/`modDate`.
/// Per-file `values` populate each file's `/CI`. Returns `0` on success, `-1`
/// null handle, `-2` on malformed JSON, `-3` on a write error.
#[no_mangle]
pub extern "C" fn gp_set_collection_json(
    handle: *mut Document,
    json_ptr: *const u8,
    json_len: usize,
) -> i32 {
    let json = unsafe { str_arg(json_ptr, json_len) };
    let Some(cfg) = CollectionConfig::from_json(json) else {
        return -2;
    };
    edit(handle, |doc| doc.set_collection(&cfg))
}

/// The document's portfolio configuration as a JSON [`CollectionConfig`] object
/// (the same shape [`gp_set_collection_json`] accepts), or the JSON literal
/// `null` when the document is not a portfolio (no `/Collection`). Host frees the
/// returned buffer.
#[no_mangle]
pub extern "C" fn gp_collection_json(handle: *const Document, out_len: *mut usize) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => match doc.collection() {
            Some(cfg) => cfg.to_json(),
            None => "null".to_string(),
        },
        None => "null".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Install a **document-level JavaScript** under the catalog `/Names /JavaScript`
/// name tree (ISO 32000-1 §7.7.3.4). `name` keys a `<< /S /JavaScript /JS … >>`
/// action; viewers run document-level scripts in name (lexical) order on open.
/// Re-using a `name` replaces that script; long sources are stored as a
/// FlateDecode stream. Returns `0` on success, `-1` null handle, `-3` on error
/// (e.g. empty name).
#[no_mangle]
pub extern "C" fn gp_add_document_javascript(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
    script_ptr: *const u8,
    script_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let script = unsafe { str_arg(script_ptr, script_len) };
    edit(handle, |doc| doc.add_document_javascript(name, script))
}

/// Remove the document-level JavaScript named `name` from `/Names /JavaScript`.
/// Returns `1` if one was removed, `0` if none had that name, `-1` null handle,
/// `-3` on error.
#[no_mangle]
pub extern "C" fn gp_remove_document_javascript(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.remove_document_javascript(name) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(_) => -3,
        },
        None => -1,
    }
}

/// Every document-level JavaScript as a JSON array `[{name,script}]`, in name
/// (lexical) order — the read side of `gp_add_document_javascript`. Host frees
/// the returned buffer.
#[no_mangle]
pub extern "C" fn gp_document_javascripts_json(
    handle: *const Document,
    out_len: *mut usize,
) -> *mut u8 {
    let json = match unsafe { handle.as_ref() } {
        Some(doc) => {
            let mut s = String::from("[");
            for (i, (name, script)) in doc.document_javascripts().iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str("{\"name\":");
                json_escape(name, &mut s);
                s.push_str(",\"script\":");
                json_escape(script, &mut s);
                s.push('}');
            }
            s.push(']');
            s
        }
        None => "[]".to_string(),
    };
    unsafe { bytes_into_host(json.into_bytes(), out_len) }
}

/// Add a page-anchored **FileAttachment** annotation over `[x0,y0,x1,y1]` on the
/// 1-based `page`, pointing at the already-embedded attachment `name`. `icon` is
/// optional (`PushPin` default; `Paperclip`/`Graph`/`Tag`). Returns `0`/`-1`/`-3`
/// (`-3` if no such attachment).
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_file_attachment_annot(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    name_ptr: *const u8,
    name_len: usize,
    icon_ptr: *const u8,
    icon_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let icon = unsafe { opt_str_arg(icon_ptr, icon_len) };
    edit(handle, |doc| {
        doc.add_file_attachment_annot(page, [x0, y0, x1, y1], name, icon)
    })
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

/// Add a `/Link` annotation over `[x0,y0,x1,y1]` carrying the action described by
/// the JSON `action` (see `Action::from_json` — `{type, dest?, uri?, file?, …}`).
/// Returns `0` on success, `-1` null handle, `-2` malformed action, `-3` error.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_add_link(
    handle: *mut Document,
    page: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    action_ptr: *const u8,
    action_len: usize,
) -> i32 {
    let json = unsafe { str_arg(action_ptr, action_len) };
    let Some(action) = Action::from_json(json) else {
        return -2;
    };
    edit(handle, |doc| doc.add_link(page, [x0, y0, x1, y1], &action))
}

/// Set the document `/OpenAction` from a JSON action (see `gp_add_link`).
/// `0` success, `-1` null handle, `-2` malformed action, `-3` error.
#[no_mangle]
pub extern "C" fn gp_set_open_action(
    handle: *mut Document,
    action_ptr: *const u8,
    action_len: usize,
) -> i32 {
    let json = unsafe { str_arg(action_ptr, action_len) };
    let Some(action) = Action::from_json(json) else {
        return -2;
    };
    edit(handle, |doc| doc.set_open_action(&action))
}

/// Remove the `link_index`-th `/Link` annotation on `page` (links counted in
/// `/Annots` order). Returns `1` if one was removed, `0` if none, `-1` null handle.
#[no_mangle]
pub extern "C" fn gp_remove_link(handle: *mut Document, page: u32, link_index: usize) -> i32 {
    match unsafe { handle.as_mut() } {
        Some(doc) => match doc.remove_link(page, link_index) {
            Ok(true) => 1,
            Ok(false) => 0,
            Err(_) => -3,
        },
        None => -1,
    }
}

/// Replace the outline with bookmarks that may carry actions. `text` is one
/// bookmark per line, `level<TAB>title<TAB>actionJson` (the action field may be
/// empty for a plain heading; a `GoTo` action becomes `/Dest`). An empty buffer
/// clears the outline. `0` on success, `<0` on error.
#[no_mangle]
pub extern "C" fn gp_set_bookmarks(handle: *mut Document, text_ptr: *const u8, text_len: usize) -> i32 {
    let text = unsafe { str_arg(text_ptr, text_len) };
    let mut items: Vec<Bookmark> = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let level = parts
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let title = parts.next().unwrap_or("").to_string();
        let action = parts
            .next()
            .filter(|s| !s.is_empty())
            .and_then(Action::from_json);
        items.push(Bookmark {
            title,
            level,
            action,
        });
    }
    edit(handle, |doc| doc.set_bookmarks(&items))
}

// ─── viewer preferences / page layout / page mode (catalog UX hints) ──────────

/// Map a tri-state flag to `Option<bool>`: `<0` = leave unchanged (`None`),
/// `0` = `Some(false)`, `>0` = `Some(true)`.
fn tri_bool(flag: i32) -> Option<bool> {
    match flag {
        n if n < 0 => None,
        0 => Some(false),
        _ => Some(true),
    }
}

/// Set the catalog `/ViewerPreferences`. Each boolean flag is tri-state:
/// `<0` leaves the key unchanged, `0` clears it to false, `>0` sets it true.
/// `direction` is `"L2R"`/`"R2L"` (empty buffer = leave unchanged). 0 on
/// success, non-zero on an invalid direction.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn gp_set_viewer_preferences(
    handle: *mut Document,
    hide_toolbar: i32,
    hide_menubar: i32,
    hide_window_ui: i32,
    fit_window: i32,
    center_window: i32,
    display_doc_title: i32,
    direction_ptr: *const u8,
    direction_len: usize,
) -> i32 {
    let direction = unsafe { str_arg(direction_ptr, direction_len) };
    let prefs = ViewerPreferences {
        hide_toolbar: tri_bool(hide_toolbar),
        hide_menubar: tri_bool(hide_menubar),
        hide_window_ui: tri_bool(hide_window_ui),
        fit_window: tri_bool(fit_window),
        center_window: tri_bool(center_window),
        display_doc_title: tri_bool(display_doc_title),
        direction: (!direction.is_empty()).then(|| direction.to_string()),
    };
    edit(handle, |doc| doc.set_viewer_preferences(&prefs))
}

/// Set the catalog `/PageLayout` name (e.g. `TwoColumnLeft`). An empty buffer
/// removes the key. 0 on success, non-zero on an unknown name.
#[no_mangle]
pub extern "C" fn gp_set_page_layout(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let layout = (!name.is_empty()).then_some(name);
    edit(handle, |doc| doc.set_page_layout(layout))
}

/// Set the catalog `/PageMode` name (e.g. `UseOutlines`). An empty buffer
/// removes the key. 0 on success, non-zero on an unknown name.
#[no_mangle]
pub extern "C" fn gp_set_page_mode(
    handle: *mut Document,
    name_ptr: *const u8,
    name_len: usize,
) -> i32 {
    let name = unsafe { str_arg(name_ptr, name_len) };
    let mode = (!name.is_empty()).then_some(name);
    edit(handle, |doc| doc.set_page_mode(mode))
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

/// Begin an optional-content marked-content sequence on `page` for the layer
/// `ocg` (its object number): registers the OCG under the page's
/// `/Resources /Properties` and appends `/OC /OCn BDC` to the content stream.
/// Subsequent drawing (text/rect/image) on that page is gated on the layer
/// until [`gp_end_optional_content`] appends `EMC`. Returns the chosen `OCn`
/// property name as a host buffer (free it); empty on error. Calls nest.
#[no_mangle]
pub extern "C" fn gp_begin_optional_content(
    handle: *mut Document,
    page: u32,
    ocg: u32,
    out_len: *mut usize,
) -> *mut u8 {
    let name = match unsafe { handle.as_mut() } {
        Some(doc) => doc.begin_optional_content(page, ocg).unwrap_or_default(),
        None => Vec::new(),
    };
    unsafe { bytes_into_host(name, out_len) }
}

/// End the innermost optional-content marked-content sequence on `page` (`EMC`).
/// Pairs one-for-one with [`gp_begin_optional_content`]. 0 on success.
#[no_mangle]
pub extern "C" fn gp_end_optional_content(handle: *mut Document, page: u32) -> i32 {
    edit(handle, |doc| doc.end_optional_content(page))
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
            ",\"kind\":\"{}\",\"flags\":{},\"readOnly\":{},\"required\":{},\"multiline\":{},\"fillable\":{},\"comb\":{},\"quadding\":{},\"daSize\":{}",
            field_kind_str(field.kind()),
            field.flags,
            field.is_read_only(),
            field.is_required(),
            field.is_multiline(),
            field.is_fillable(),
            field.comb,
            field.quadding,
            field.da_size,
        ));
        if let Some(font) = &field.da_font {
            out.push_str(",\"daFont\":");
            json_escape(font, &mut out);
        }
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

/// A JSON array literal of finite numbers (NaN/∞ → 0).
fn num_array_json(values: &[f64]) -> String {
    let mut s = String::from("[");
    for (i, &v) in values.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{}", if v.is_finite() { v } else { 0.0 }));
    }
    s.push(']');
    s
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
        out.push_str(",\"author\":");
        json_escape(&a.author, &mut out);
        out.push_str(",\"subject\":");
        json_escape(&a.subject, &mut out);
        out.push_str(",\"created\":");
        json_escape(&a.created, &mut out);
        out.push_str(",\"modified\":");
        json_escape(&a.modified, &mut out);
        out.push_str(",\"name\":");
        json_escape(&a.name, &mut out);
        out.push_str(&format!(",\"opacity\":{}", if a.opacity.is_finite() { a.opacity } else { 1.0 }));
        out.push_str(",\"color\":");
        out.push_str(&num_array_json(&a.color));
        out.push_str(",\"quadPoints\":");
        out.push_str(&num_array_json(&a.quad_points));
        out.push_str(",\"inkList\":[");
        for (j, path) in a.ink_list.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&num_array_json(path));
        }
        out.push(']');
        out.push_str(",\"linkUri\":");
        json_escape(&a.link_uri, &mut out);
        out.push_str(&format!(",\"linkPage\":{}}}", a.link_page));
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
        out.push_str(&format!(
            ",\"bold\":{},\"italic\":{},\"color\":[{},{},{}]",
            item.bold, item.italic, item.color[0], item.color[1], item.color[2]
        ));
        if !item.dest_kind.is_empty() {
            out.push_str(",\"destKind\":");
            json_escape(&item.dest_kind, &mut out);
        }
        if let Some(x) = item.dest_x {
            out.push_str(&format!(",\"x\":{x}"));
        }
        if let Some(y) = item.dest_y {
            out.push_str(&format!(",\"y\":{y}"));
        }
        if let Some(z) = item.dest_zoom {
            out.push_str(&format!(",\"zoom\":{z}"));
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
