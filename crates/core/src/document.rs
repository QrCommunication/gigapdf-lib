//! High-level PDF document: open a real file, walk the page tree, decode page
//! content. Built on our own lexer/parser/inflate — zero dependencies.
//!
//! Robust open strategy: rather than trusting the cross-reference table (which
//! is frequently broken in real files, and is a compressed stream in PDF 1.5+),
//! we scan the whole file for `n g obj … endobj` definitions and the `trailer`
//! dictionary. Later definitions override earlier ones, which naturally handles
//! incremental updates. The catalog is found via `trailer /Root`, falling back
//! to any `/Type /Catalog` object.

use std::collections::{BTreeMap, BTreeSet};

use crate::annot::{self, Annotation};
use crate::content::{self, ContentElement, TextRun};
use crate::error::{EngineError, Result};
use crate::filters::decode_stream;
use crate::form::{self, FormField};
use crate::lexer::{Lexer, Token};
use crate::link::{Link, LinkTarget};
use crate::object::{Dictionary, Object, ObjectId, Stream, StringKind};
use crate::ocg::Layer;
use crate::outline::OutlineItem;
use crate::parser::Parser;

/// A parsed PDF document held in memory.
#[derive(Debug, Clone)]
pub struct Document {
    objects: BTreeMap<ObjectId, Object>,
    trailer: Dictionary,
    /// GIDs drawn with each embedded Type0 font (keyed by its object number),
    /// accumulated by [`add_text`](Document::add_text) so the save path can
    /// subset each embedded `glyf` table to only the glyphs actually used.
    font_used_gids: BTreeMap<u32, std::collections::BTreeSet<u16>>,
}

/// A full-text search hit: the page, the matching line's text, and its bounding
/// box in PDF user space (origin bottom-left) for highlighting.
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub page: u32,
    pub text: String,
    pub bounds: content::Bounds,
}

// ─── form-XObject inlining (flatten_form_xobjects) ──────────────────────────

/// A form XObject resolved for *inlining* into a page: its decoded content, its
/// own `/Resources` (if any — else it inherits the parent scope), its `/Matrix`
/// (default identity) and its object id (for the cycle guard).
struct ResolvedForm {
    content: Vec<u8>,
    resources: Option<Dictionary>,
    matrix: content::PageMatrix,
    id: Option<ObjectId>,
}

/// An operator whose resource-name operand must resolve in `/Resources`, with
/// the sub-dictionary it lives in and where the name sits among the operands.
struct ResourceOp {
    /// Operator keyword, e.g. `b"Tf"`.
    operator: &'static [u8],
    /// `/Resources` sub-dictionary the name keys into, e.g. `b"Font"`.
    category: &'static [u8],
    /// Where the name operand is: `First` (operand 0) or `Last` (last operand,
    /// and only when it actually *is* a name — `scn`/`SCN` take optional colour
    /// components before an optional pattern name).
    pos: OperandPos,
}

#[derive(Clone, Copy, PartialEq)]
enum OperandPos {
    First,
    Last,
}

/// The content-stream operators that reference a named resource, mapped to the
/// `/Resources` sub-dictionary and the operand position of the name. `ri`
/// (rendering intent) and inline `BDC`/`DP` dictionaries are intentionally
/// absent — they are not resource-name references. `BDC`/`DP` *property* refs
/// (`/OC /P0 BDC`) carry the name as the **last** operand keyed into
/// `/Properties`; a non-name last operand (inline dict) is left alone.
const RESOURCE_OPS: &[ResourceOp] = &[
    ResourceOp { operator: b"Tf", category: b"Font", pos: OperandPos::First },
    ResourceOp { operator: b"Do", category: b"XObject", pos: OperandPos::First },
    ResourceOp { operator: b"gs", category: b"ExtGState", pos: OperandPos::First },
    ResourceOp { operator: b"sh", category: b"Shading", pos: OperandPos::First },
    ResourceOp { operator: b"cs", category: b"ColorSpace", pos: OperandPos::First },
    ResourceOp { operator: b"CS", category: b"ColorSpace", pos: OperandPos::First },
    ResourceOp { operator: b"scn", category: b"Pattern", pos: OperandPos::Last },
    ResourceOp { operator: b"SCN", category: b"Pattern", pos: OperandPos::Last },
    ResourceOp { operator: b"BDC", category: b"Properties", pos: OperandPos::Last },
    ResourceOp { operator: b"DP", category: b"Properties", pos: OperandPos::Last },
];

/// Per-category rename maps applied to a form body's resource-name operands when
/// the form's own resource collided with a different page resource of the same
/// name (so it was copied under a fresh name). An empty map ⇒ identity (every
/// name already resolves in the page, the common case).
struct Renames {
    /// `category → (old form name → new page name)`.
    by_category: BTreeMap<&'static [u8], BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl Renames {
    /// The no-rename map (top-level page ops, or a form whose resources merged
    /// without any collision).
    fn identity() -> Self {
        Self { by_category: BTreeMap::new() }
    }

    fn is_identity(&self) -> bool {
        self.by_category.values().all(BTreeMap::is_empty)
    }

    fn record(&mut self, category: &'static [u8], old: Vec<u8>, new: Vec<u8>) {
        self.by_category.entry(category).or_default().insert(old, new);
    }

    /// Look up the rewritten name for `name` in `category`, if it was renamed.
    fn lookup(&self, category: &'static [u8], name: &[u8]) -> Option<&Vec<u8>> {
        self.by_category.get(category).and_then(|m| m.get(name))
    }

    /// Clone `op`, rewriting its resource-name operand through the rename map.
    /// Ops that don't reference a resource (or whose name wasn't renamed) clone
    /// unchanged.
    fn apply(&self, op: &content::Operation) -> content::Operation {
        if self.is_identity() {
            return op.clone();
        }
        let Some(spec) = RESOURCE_OPS.iter().find(|s| s.operator == op.operator.as_slice()) else {
            return op.clone();
        };
        let idx = match spec.pos {
            OperandPos::First => 0,
            OperandPos::Last => op.operands.len().saturating_sub(1),
        };
        let renamed = match op.operands.get(idx) {
            Some(Object::Name(n)) => self.lookup(spec.category, n).cloned(),
            _ => None,
        };
        let Some(new_name) = renamed else {
            return op.clone();
        };
        let mut operands = op.operands.clone();
        operands[idx] = Object::Name(new_name);
        content::Operation { operator: op.operator.clone(), operands }
    }
}

/// A mutable copy of a page's `/Resources` sub-dictionaries, into which the
/// resources used by inlined forms are merged. Tracks the page's *original*
/// entries (resolved object id when indirect) so a form entry of the same name
/// is reused when it points at the same object, and renamed only on a true
/// collision (a different object).
struct FlattenResources {
    /// `category → (name → value)` working copy.
    cats: BTreeMap<Vec<u8>, Dictionary>,
    /// `category → next collision counter` for fresh `{name}_fx{n}` names.
    counter: BTreeMap<&'static [u8], usize>,
}

impl FlattenResources {
    /// Seed the working copy from the page's resolved `/Resources` (each
    /// sub-dictionary cloned so we can extend it without mutating shared state).
    fn from_dict(doc: &Document, res: &Dictionary) -> Self {
        let mut cats = BTreeMap::new();
        for spec in RESOURCE_OPS {
            if cats.contains_key(spec.category) {
                continue; // ColorSpace/Pattern appear twice in RESOURCE_OPS
            }
            if let Some(sub) = res
                .get(spec.category)
                .map(|o| doc.resolve(o))
                .and_then(Object::as_dict)
            {
                cats.insert(spec.category.to_vec(), sub.clone());
            }
        }
        Self { cats, counter: BTreeMap::new() }
    }

    /// Reassemble the merged `/Resources` dictionary (preserving any keys the
    /// page had that we don't manage by copying them back from the working set).
    fn into_dict(self) -> Dictionary {
        let mut out = Dictionary::new();
        for (category, sub) in self.cats {
            if !sub.is_empty() {
                out.set(category, Object::Dictionary(sub));
            }
        }
        out
    }

    /// The resolved object id behind a resource entry, if it is an indirect
    /// reference (used to tell "same resource, reuse the name" from "different
    /// resource, must rename").
    fn entry_id(value: &Object) -> Option<ObjectId> {
        value.as_reference()
    }

    /// Merge a form's `/Resources` (resolved against `doc`) into the page working
    /// copy and return the [`Renames`] for the form body. For each entry the form
    /// references: absent in the page ⇒ copy under the same name; present and
    /// pointing at the same object ⇒ reuse (no rename); present but a different
    /// object ⇒ copy under a fresh `{name}_fx{n}` name and record the rename.
    fn merge_scope(&mut self, doc: &Document, form_res: &Dictionary) -> Renames {
        let mut renames = Renames::identity();
        for spec in RESOURCE_OPS {
            let Some(form_sub) = form_res
                .get(spec.category)
                .map(|o| doc.resolve(o))
                .and_then(Object::as_dict)
            else {
                continue;
            };
            for (name, value) in &form_sub.0 {
                let page_sub = self.cats.entry(spec.category.to_vec()).or_default();
                match page_sub.get(name) {
                    None => {
                        // Free name: copy verbatim.
                        page_sub.0.insert(name.clone(), value.clone());
                    }
                    Some(existing) => {
                        let same = match (Self::entry_id(existing), Self::entry_id(value)) {
                            (Some(a), Some(b)) => a == b,
                            // Inline (direct) values: compare structurally.
                            _ => existing == value,
                        };
                        if same {
                            continue; // reuse the page name, no rewrite
                        }
                        // Collision with a different object: fresh name.
                        let n = self.counter.entry(spec.category).or_default();
                        let mut new_name;
                        loop {
                            new_name = {
                                let mut nm = name.clone();
                                nm.extend_from_slice(b"_fx");
                                nm.extend_from_slice(n.to_string().as_bytes());
                                nm
                            };
                            *n += 1;
                            if !page_sub.contains(&new_name) {
                                break;
                            }
                        }
                        page_sub.0.insert(new_name.clone(), value.clone());
                        renames.record(spec.category, name.clone(), new_name);
                    }
                }
            }
        }
        renames
    }
}

/// A zero-operand content operation (`q`, `Q`, …).
fn op0(operator: &[u8]) -> content::Operation {
    content::Operation { operator: operator.to_vec(), operands: Vec::new() }
}

/// The appearance matrix that maps an annotation's appearance stream into its
/// `/Rect`, per ISO 32000-1 §12.5.5 ("Algorithm: Appearance streams"):
///
/// 1. transform the four `/BBox` corners by the form's `/Matrix`;
/// 2. take the axis-aligned bounding box (the "transformed appearance box");
/// 3. compute the matrix `A` that scales+translates that box onto `/Rect`;
/// 4. the appearance is rendered with `Matrix · A` (the form `/Matrix` first,
///    then `A`).
///
/// Returns `None` when the transformed box or the `/Rect` is degenerate (zero
/// width or height) — there is then no sensible mapping and the caller skips the
/// annotation. For the engine's own annotations (`/BBox == /Rect`, identity
/// `/Matrix`) this reduces to the identity, leaving the appearance in page space.
fn appearance_matrix(
    bbox: [f64; 4],
    matrix: content::PageMatrix,
    rect: [f64; 4],
) -> Option<content::PageMatrix> {
    let [bx0, by0, bx1, by1] = bbox;
    let corners = [(bx0, by0), (bx1, by0), (bx1, by1), (bx0, by1)];
    let mut tx0 = f64::INFINITY;
    let mut ty0 = f64::INFINITY;
    let mut tx1 = f64::NEG_INFINITY;
    let mut ty1 = f64::NEG_INFINITY;
    for (x, y) in corners {
        let (px, py) = matrix.apply(x, y);
        tx0 = tx0.min(px);
        ty0 = ty0.min(py);
        tx1 = tx1.max(px);
        ty1 = ty1.max(py);
    }
    let (tw, th) = (tx1 - tx0, ty1 - ty0);
    let [rx0, ry0, rx1, ry1] = rect;
    let (rw, rh) = (rx1 - rx0, ry1 - ry0);
    if tw.abs() < 1e-9 || th.abs() < 1e-9 || rw.abs() < 1e-9 || rh.abs() < 1e-9 {
        return None;
    }
    let sx = rw / tw;
    let sy = rh / th;
    // `A`: scale the transformed box by (sx, sy) then translate its origin to the
    // Rect origin — `[sx 0 0 sy  rx0 - sx·tx0  ry0 - sy·ty0]`.
    let a = content::PageMatrix::new(sx, 0.0, 0.0, sy, rx0 - sx * tx0, ry0 - sy * ty0);
    Some(matrix.then(&a))
}

/// Push every indirect reference contained in `object` onto `out`.
fn collect_refs(object: &Object, out: &mut Vec<ObjectId>) {
    match object {
        Object::Reference(id) => out.push(*id),
        Object::Array(items) => items.iter().for_each(|o| collect_refs(o, out)),
        Object::Dictionary(dict) => dict.0.values().for_each(|v| collect_refs(v, out)),
        Object::Stream(stream) => stream.dict.0.values().for_each(|v| collect_refs(v, out)),
        _ => {}
    }
}

/// Build a literal PDF text string object (PDFDocEncoding / UTF-16BE as needed).
fn pdf_text(s: &str) -> Object {
    Object::String(crate::font::encode_pdf_text(s), StringKind::Literal)
}

/// Normalise a PDF font name for fuzzy matching: drop a leading `/`, strip a
/// 6-uppercase-letter subset prefix (`ABCDEF+`), then keep only lowercased
/// alphanumerics. `"HXBDOG+OCRB10PitchBT-Regular"` → `"ocrb10pitchbtregular"`,
/// `"Arial-BoldMT"` → `"arialboldmt"`. Suffix variants are absorbed by the
/// caller's two-direction `contains` match rather than stripped here.
fn normalize_font_name(raw: &str) -> String {
    let mut s = raw.trim_start_matches('/');
    let bytes = s.as_bytes();
    if bytes.len() > 7 && bytes[6] == b'+' && bytes[..6].iter().all(u8::is_ascii_uppercase) {
        s = &s[7..];
    }
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Set the shared reviewer metadata on an annotation dict: popup `/Contents`,
/// `/T` author, `/NM` id, `/M` modification date and the printable `/F` flag.
/// Empty strings are skipped. `/Contents` and `/T` go through `pdf_text` so
/// non-ASCII reviewer text is stored as UTF-16BE.
fn set_annotation_metadata(
    dict: &mut Dictionary,
    contents: &str,
    author: &str,
    id: &str,
    date: &str,
) {
    if !contents.is_empty() {
        dict.set(b"Contents".to_vec(), pdf_text(contents));
    }
    if !author.is_empty() {
        dict.set(b"T".to_vec(), pdf_text(author));
    }
    if !id.is_empty() {
        dict.set(
            b"NM".to_vec(),
            Object::String(id.as_bytes().to_vec(), StringKind::Literal),
        );
    }
    if !date.is_empty() {
        dict.set(
            b"M".to_vec(),
            Object::String(date.as_bytes().to_vec(), StringKind::Literal),
        );
    }
    dict.set(b"F".to_vec(), Object::Integer(4));
}

/// Rewrite an object's indirect references through `map` (for grafting between
/// documents). References absent from the map are kept as-is.
fn remap_object(object: &Object, map: &BTreeMap<ObjectId, ObjectId>) -> Object {
    match object {
        Object::Reference(id) => Object::Reference(map.get(id).copied().unwrap_or(*id)),
        Object::Array(items) => Object::Array(items.iter().map(|o| remap_object(o, map)).collect()),
        Object::Dictionary(dict) => Object::Dictionary(remap_dict(dict, map)),
        Object::Stream(stream) => Object::Stream(Stream {
            dict: remap_dict(&stream.dict, map),
            raw: stream.raw.clone(),
        }),
        other => other.clone(),
    }
}

fn remap_dict(dict: &Dictionary, map: &BTreeMap<ObjectId, ObjectId>) -> Dictionary {
    let mut out = Dictionary::new();
    for (key, value) in &dict.0 {
        out.0.insert(key.clone(), remap_object(value, map));
    }
    out
}

/// Write a `(...)` literal string, escaping the bytes that need it.
fn push_pdf_string(out: &mut Vec<u8>, text: &str) {
    out.push(b'(');
    for &byte in &crate::font::encode_winansi(text) {
        if matches!(byte, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b')');
}

/// Build a field's appearance form (dictionary without `/Length`) and its
/// content stream, sizing the text to the widget rectangle. A `value`
/// containing newlines is laid out as multiple top-aligned lines (multiline
/// text and list boxes); a single line is vertically centred.
fn build_text_field_appearance(rect: [f64; 4], value: &str) -> (Dictionary, Vec<u8>) {
    let w = rect[2] - rect[0];
    let h = rect[3] - rect[1];
    let lines: Vec<&str> = value.split('\n').collect();
    let multiline = lines.len() > 1;

    let size = if multiline {
        (h / (lines.len() as f64 + 0.5)).clamp(6.0, 12.0)
    } else {
        (h * 0.6).clamp(6.0, 14.0)
    };
    let leading = size * 1.15;
    let first_baseline = if multiline {
        h - size
    } else {
        (h - size) / 2.0 + size * 0.2
    };

    let mut content = Vec::new();
    content.extend_from_slice(b"/Tx BMC\nq\nBT\n");
    content.extend_from_slice(format!("/Helv {} Tf 0 g\n", content::num(size)).as_bytes());
    content.extend_from_slice(format!("{} TL\n", content::num(leading)).as_bytes());
    content.extend_from_slice(format!("2 {} Td\n", content::num(first_baseline)).as_bytes());
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            content.extend_from_slice(b"T*\n");
        }
        push_pdf_string(&mut content, line);
        content.extend_from_slice(b" Tj\n");
    }
    content.extend_from_slice(b"ET\nQ\nEMC\n");

    let mut helv = Dictionary::new();
    helv.set(b"Type".to_vec(), annot::name(b"Font"));
    helv.set(b"Subtype".to_vec(), annot::name(b"Type1"));
    helv.set(b"BaseFont".to_vec(), annot::name(b"Helvetica"));
    let mut fonts = Dictionary::new();
    fonts.set(b"Helv".to_vec(), Object::Dictionary(helv));
    let mut resources = Dictionary::new();
    resources.set(b"Font".to_vec(), Object::Dictionary(fonts));

    let mut form = Dictionary::new();
    form.set(b"Type".to_vec(), annot::name(b"XObject"));
    form.set(b"Subtype".to_vec(), annot::name(b"Form"));
    form.set(b"BBox".to_vec(), annot::real_array(&[0.0, 0.0, w, h]));
    form.set(b"Resources".to_vec(), Object::Dictionary(resources));
    (form, content)
}

/// What a PDF's `/Encrypt` dictionary declares, read **without** decrypting
/// (so no password is needed). Returned by [`Document::encryption_info`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncryptionInfo {
    /// Whether the trailer references an `/Encrypt` dictionary.
    pub encrypted: bool,
    /// The `/P` permission bitmask (0 when not encrypted).
    pub permissions: i32,
    /// The handler version `/V` (0 when not encrypted).
    pub version: i32,
    /// The handler revision `/R` (0 when not encrypted).
    pub revision: i32,
}

/// Adobe AFM advance widths (per 1000 em) for the standard **Helvetica** font,
/// for printable ASCII `0x20..=0x7E`. Used by [`Document::helvetica_width`] to
/// position watermark text. Characters outside this range fall back to `556`.
#[rustfmt::skip]
const HELVETICA_AFM: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, // 0x20-0x2F
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556, // 0x30-0x3F
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722, 778, // 0x40-0x4F
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, // 0x50-0x5F
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, // 0x60-0x6F
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584,       // 0x70-0x7E
];

/// One run of an invisible (OCR) text layer for [`Document::add_text_layer`]:
/// baseline-anchored `text` at `(x, y)` in PDF user space, `size` points,
/// rotated `rotation_deg`° counter-clockwise.
#[derive(Debug, Clone)]
pub struct TextLayerRun {
    pub x: f64,
    pub y: f64,
    pub size: f64,
    pub text: String,
    pub rotation_deg: f64,
}

/// One embedded font in a document (from [`Document::embedded_fonts`]): its
/// `/BaseFont` name and embedded program format (`truetype` / `cff` / `type1`).
/// Feed `base_font` to [`Document::extract_font_program`] to pull the bytes out
/// and re-embed it (e.g. to draw new text in the document's own face).
#[derive(Debug, Clone)]
pub struct EmbeddedFontInfo {
    pub base_font: String,
    pub format: String,
}

/// One embedded file attachment (from [`Document::attachments`]). Mirrors what a
/// reader's `getAttachments()` exposes: the name-tree key, the filespec display
/// name (`/UF` or `/F`), the optional embedded-stream MIME (`/Subtype`),
/// description (`/Desc`) and `/Params` dates, plus the decoded file bytes.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// The `/EmbeddedFiles` name-tree key the file was registered under.
    pub name: String,
    /// The filespec display filename (`/UF` preferred, else `/F`, else `name`).
    pub filename: String,
    /// The embedded stream's `/Subtype` (e.g. `application/pdf`), if present.
    pub mime: Option<String>,
    /// The filespec `/Desc` human description, if present.
    pub description: Option<String>,
    /// The `/Params /CreationDate` PDF date string, if present.
    pub creation_date: Option<String>,
    /// The `/Params /ModDate` PDF date string, if present.
    pub mod_date: Option<String>,
    /// The decoded (filters applied) file bytes.
    pub data: Vec<u8>,
}

/// One text element from [`Document::page_text_elements`]: the decoded text plus
/// everything a host editor needs to recreate the run — its bounding box (page
/// user space, origin bottom-left), the resolved `/BaseFont` family and
/// bold/italic flags, the effective point size, the RGB fill colour (`0..=1`)
/// and the baseline rotation. `index` is the **text-run** index accepted by
/// [`Document::replace_text_run`].
#[derive(Debug, Clone)]
pub struct TextElementInfo {
    pub index: usize,
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub font_family: String,
    pub bold: bool,
    pub italic: bool,
    pub font_size: f64,
    pub color: [f64; 3],
    pub rotation_deg: f64,
}

/// One image element from [`Document::page_image_elements`]: its placement box
/// (page user space, origin bottom-left), the embeddable encoded bytes and
/// their format, and the source pixel dimensions. `index` is its position among
/// the page's image elements — the native equivalent of a reader's image
/// extraction, returning bytes a host can display or re-embed.
#[derive(Debug, Clone)]
pub struct ImageElementInfo {
    pub index: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// `"jpeg"` (DCTDecode passthrough), `"png"` (re-encoded from samples),
    /// `"jp2"` (JPXDecode passthrough), or `"unknown"` (colour space/filter not
    /// decoded — `data` is then empty).
    pub format: String,
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Embeddable encoded image bytes (empty when `format == "unknown"`).
    pub data: Vec<u8>,
    /// Rotation in degrees from the placement CTM (`0` = upright).
    pub rotation: f64,
    /// Non-stroking fill alpha in effect (`0.0..=1.0`, `1.0` = fully opaque),
    /// from the active `/ExtGState`'s `/ca`.
    pub opacity: f64,
}

impl Document {
    /// Parse a PDF from raw bytes.
    pub fn open(bytes: &[u8]) -> Result<Self> {
        Self::open_with_password(bytes, b"")
    }

    /// Inspect a PDF's encryption **without decrypting it** — reads the
    /// `/Encrypt` dictionary's `/P`, `/V` and `/R` straight from the structure,
    /// so it works on password-protected files (where [`Document::open`] fails).
    pub fn encryption_info(bytes: &[u8]) -> EncryptionInfo {
        let (objects, mut trailer) = scan(bytes);
        recover_trailer_from_xref(&mut trailer, &objects);
        let not_encrypted = EncryptionInfo {
            encrypted: false,
            permissions: 0,
            version: 0,
            revision: 0,
        };
        let Some(encrypt_ref) = trailer.get(b"Encrypt").and_then(Object::as_reference) else {
            return not_encrypted;
        };
        let Some(dict) = objects.get(&encrypt_ref).and_then(Object::as_dict) else {
            return EncryptionInfo {
                encrypted: true,
                ..not_encrypted
            };
        };
        EncryptionInfo {
            encrypted: true,
            permissions: dict.get(b"P").and_then(Object::as_i64).unwrap_or(0) as i32,
            version: dict.get(b"V").and_then(Object::as_i64).unwrap_or(0) as i32,
            revision: dict.get(b"R").and_then(Object::as_i64).unwrap_or(0) as i32,
        }
    }

    /// Open a (possibly encrypted) PDF, decrypting with `password`.
    pub fn open_with_password(bytes: &[u8], password: &[u8]) -> Result<Self> {
        let (mut objects, mut trailer) = scan(bytes);
        if objects.is_empty() {
            return Err(EngineError::parse(0, "no PDF objects found"));
        }
        // PDF 1.5+: `/Root` lives in the xref-stream dict (no classic trailer),
        // and the catalog/pages are packed inside compressed object streams.
        recover_trailer_from_xref(&mut trailer, &objects);
        // Decrypt top-level objects BEFORE extracting object streams, so the
        // (now-plaintext) ObjStm contents are read directly.
        decrypt_objects(&mut objects, &trailer, password)?;
        extract_object_streams(&mut objects);
        Ok(Self {
            objects,
            trailer,
            font_used_gids: BTreeMap::new(),
        })
    }

    /// Digitally sign the document with an engine-managed signer, producing a
    /// signed PDF (`adbe.pkcs7.detached`). The signer carries a self-signed
    /// certificate (an ephemeral "digital ID", like Adobe's self-signed IDs):
    /// this proves integrity + authorship, not a CA-backed identity (non-eIDAS).
    /// `date` is a PDF date string such as `"D:20260614120000Z"`.
    pub fn sign(
        &mut self,
        signer: &crate::sign::Signer,
        name: &str,
        reason: &str,
        date: &str,
    ) -> Result<Vec<u8>> {
        self.sign_with(name, reason, date, "", "", |signed| {
            signer.detached_cms(signed)
        })
    }

    /// Digitally sign the document with a user-supplied identity imported from a
    /// PKCS#12 (`.p12`/`.pfx`) file — a CA-issued / eIDAS-capable certificate and
    /// its RSA key. Same `adbe.pkcs7.detached` machinery as [`sign`](Self::sign),
    /// but the embedded certificate (and the `SignerInfo` it is referenced by) is
    /// the imported one. `location`/`contact_info` populate the optional `/Location`
    /// and `/ContactInfo` signature fields (empty → omitted). Errors if the
    /// identity has no certificate or its issuer/serial can't be read.
    #[allow(clippy::too_many_arguments)]
    pub fn sign_p12(
        &mut self,
        identity: &crate::sign::pkcs12::Pkcs12Identity,
        name: &str,
        reason: &str,
        date: &str,
        location: &str,
        contact_info: &str,
    ) -> Result<Vec<u8>> {
        let cert = identity
            .certificates
            .first()
            .ok_or_else(|| EngineError::Missing("PKCS#12 identity has no certificate".into()))?
            .clone();
        // Fail fast if we can't reference the cert, so the closure stays infallible.
        crate::sign::issuer_and_serial(&cert).ok_or_else(|| {
            EngineError::Unsupported("certificate issuer/serial unreadable".into())
        })?;
        let key = identity.key.clone();
        self.sign_with(name, reason, date, location, contact_info, move |signed| {
            crate::sign::detached_cms_external(&key, &cert, signed).unwrap_or_default()
        })
    }

    /// Shared `adbe.pkcs7.detached` embedding: builds the signature dictionary
    /// and invisible widget, serializes, patches `/ByteRange`, then fills
    /// `/Contents` with the CMS produced by `build_cms` over the signed bytes.
    fn sign_with(
        &mut self,
        name: &str,
        reason: &str,
        date: &str,
        location: &str,
        contact_info: &str,
        build_cms: impl FnOnce(&[u8]) -> Vec<u8>,
    ) -> Result<Vec<u8>> {
        const CONTENTS_BYTES: usize = 8192; // room for the CMS (hex = 16384 chars)
        let lit = |s: &str| Object::String(crate::font::encode_pdf_text(s), StringKind::Literal);

        // 1. Signature value dictionary with fixed-width placeholders.
        let sig_id = (self.next_object_number(), 0u16);
        let mut sig = Dictionary::new();
        sig.set(b"Type".to_vec(), Object::Name(b"Sig".to_vec()));
        sig.set(b"Filter".to_vec(), Object::Name(b"Adobe.PPKLite".to_vec()));
        sig.set(
            b"SubFilter".to_vec(),
            Object::Name(b"adbe.pkcs7.detached".to_vec()),
        );
        sig.set(b"Name".to_vec(), lit(name));
        sig.set(b"Reason".to_vec(), lit(reason));
        sig.set(b"M".to_vec(), lit(date));
        // /Location and /ContactInfo are optional signature metadata.
        if !location.is_empty() {
            sig.set(b"Location".to_vec(), lit(location));
        }
        if !contact_info.is_empty() {
            sig.set(b"ContactInfo".to_vec(), lit(contact_info));
        }
        // 4 × 10-digit numbers → a fixed-width array we can patch in place.
        sig.set(
            b"ByteRange".to_vec(),
            Object::Array(vec![Object::Integer(9_999_999_999); 4]),
        );
        sig.set(
            b"Contents".to_vec(),
            Object::String(vec![0u8; CONTENTS_BYTES], StringKind::Hex),
        );
        self.objects.insert(sig_id, Object::Dictionary(sig));

        // 2. Signature field = invisible widget on page 1, linked to the value.
        let field_id = (self.next_object_number(), 0u16);
        let mut field = Dictionary::new();
        field.set(b"Type".to_vec(), Object::Name(b"Annot".to_vec()));
        field.set(b"Subtype".to_vec(), Object::Name(b"Widget".to_vec()));
        field.set(b"FT".to_vec(), Object::Name(b"Sig".to_vec()));
        field.set(b"T".to_vec(), lit("Signature1"));
        field.set(b"V".to_vec(), Object::Reference(sig_id));
        field.set(b"Rect".to_vec(), annot::real_array(&[0.0, 0.0, 0.0, 0.0]));
        field.set(b"F".to_vec(), Object::Integer(132)); // Print + Locked
        if let Ok(page_id) = self.page_object_id(1) {
            field.set(b"P".to_vec(), Object::Reference(page_id));
        }
        self.objects.insert(field_id, Object::Dictionary(field));

        if let Ok(page_id) = self.page_object_id(1) {
            if let Some(mut page) = self
                .objects
                .get(&page_id)
                .and_then(Object::as_dict)
                .cloned()
            {
                let mut annots = page
                    .get(b"Annots")
                    .map(|o| self.resolve(o))
                    .and_then(Object::as_array)
                    .map(<[Object]>::to_vec)
                    .unwrap_or_default();
                annots.push(Object::Reference(field_id));
                page.set(b"Annots".to_vec(), Object::Array(annots));
                self.objects.insert(page_id, Object::Dictionary(page));
            }
        }

        // 3. Register the field in the AcroForm and flag the document as signed.
        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        let mut acroform = catalog
            .get(b"AcroForm")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        let mut fields = acroform
            .get(b"Fields")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        fields.push(Object::Reference(field_id));
        acroform.set(b"Fields".to_vec(), Object::Array(fields));
        acroform.set(b"SigFlags".to_vec(), Object::Integer(3));
        catalog.set(b"AcroForm".to_vec(), Object::Dictionary(acroform));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));

        // 4. Serialize, then patch /ByteRange and fill /Contents with the CMS.
        let mut bytes = self.save();
        // The signature's /Contents is the only one written as a hex string
        // (`/Contents <…>`); a page's /Contents is an indirect reference.
        let lt = find_subsequence(&bytes, b"/Contents <")
            .map(|p| p + b"/Contents <".len() - 1) // index of the '<'
            .ok_or_else(|| EngineError::Missing("signature /Contents placeholder".into()))?;
        let gt = bytes[lt..]
            .iter()
            .position(|&b| b == b'>')
            .map(|p| lt + p)
            .ok_or_else(|| EngineError::Missing("signature /Contents end".into()))?;

        let total = bytes.len();
        let byte_range = [0usize, lt, gt + 1, total - (gt + 1)];

        let br = find_subsequence(&bytes, b"/ByteRange [")
            .map(|p| p + b"/ByteRange [".len())
            .ok_or_else(|| EngineError::Missing("signature /ByteRange".into()))?;
        let mut p = br;
        for (i, value) in byte_range.iter().enumerate() {
            bytes[p..p + 10].copy_from_slice(format!("{value:010}").as_bytes());
            p += 10 + usize::from(i < 3); // 10 digits, then a separator space
        }

        // Hash everything except the /Contents hex, build the CMS, hex-fill it.
        let mut signed = Vec::with_capacity(byte_range[1] + byte_range[3]);
        signed.extend_from_slice(&bytes[0..lt]);
        signed.extend_from_slice(&bytes[gt + 1..]);
        let cms = build_cms(&signed);

        let capacity = gt - (lt + 1); // hex digit slots between < and >
        let mut hex = String::with_capacity(capacity);
        for byte in &cms {
            hex.push_str(&format!("{byte:02X}"));
        }
        if hex.len() > capacity {
            return Err(EngineError::Unsupported(
                "signature too large for the reserved /Contents space".into(),
            ));
        }
        while hex.len() < capacity {
            hex.push('0');
        }
        bytes[lt + 1..gt].copy_from_slice(hex.as_bytes());
        Ok(bytes)
    }

    /// Serialize the document encrypted with the Standard Security Handler.
    /// `algorithm`: `0` = RC4-128 (R3), `1` = AES-128 (R4), `2` = AES-256 (R6).
    /// `id0` is the file identifier; `file_key` is **secret host randomness**
    /// used only by AES-256 (the engine has no RNG). `permissions` is `/P`.
    pub fn save_encrypted(
        &self,
        user_password: &[u8],
        owner_password: &[u8],
        id0: &[u8],
        file_key: &[u8],
        algorithm: i32,
        permissions: i32,
    ) -> Vec<u8> {
        use crate::security::Security;
        let (security, encrypt_dict) = match algorithm {
            1 => Security::new_aes_v2(user_password, owner_password, id0, permissions),
            2 => Security::new_aes_v3(user_password, owner_password, file_key, permissions, true),
            _ => Security::new_rc4(user_password, owner_password, id0, permissions),
        };
        crate::serialize::to_pdf_encrypted(
            &self.objects,
            &self.trailer,
            &security,
            &encrypt_dict,
            id0,
        )
    }

    /// Number of objects parsed (diagnostic).
    pub fn object_count(&self) -> usize {
        self.objects.len()
    }

    /// Serialize the (possibly edited) document to a fresh, valid PDF.
    pub fn save(&self) -> Vec<u8> {
        if self.font_used_gids.is_empty() {
            return crate::serialize::to_pdf(&self.objects, &self.trailer);
        }
        let mut objects = self.objects.clone();
        self.subset_embedded_fonts(&mut objects);
        crate::serialize::to_pdf(&objects, &self.trailer)
    }

    /// Fetch an indirect object by id.
    pub fn get(&self, id: ObjectId) -> Option<&Object> {
        self.objects.get(&id)
    }

    /// Follow indirect references until a direct object is reached.
    pub fn resolve<'a>(&'a self, object: &'a Object) -> &'a Object {
        let mut current = object;
        for _ in 0..64 {
            match current {
                Object::Reference(id) => match self.objects.get(id) {
                    Some(next) => current = next,
                    None => return &Object::Null,
                },
                other => return other,
            }
        }
        &Object::Null
    }

    /// The document catalog dictionary.
    fn catalog(&self) -> Result<&Dictionary> {
        if let Some(root) = self.trailer.get(b"Root") {
            if let Some(dict) = self.resolve(root).as_dict() {
                return Ok(dict);
            }
        }
        // Fallback: any /Type /Catalog object.
        for object in self.objects.values() {
            if let Some(dict) = object.as_dict() {
                if dict.get(b"Type").and_then(Object::as_name) == Some(b"Catalog".as_slice()) {
                    return Ok(dict);
                }
            }
        }
        Err(EngineError::Missing("document catalog".into()))
    }

    /// Number of pages in the document (0 if the page tree can't be read).
    pub fn page_count(&self) -> usize {
        self.page_ids().map(|ids| ids.len()).unwrap_or(0)
    }

    /// Object ids of all pages, in reading order.
    pub fn page_ids(&self) -> Result<Vec<ObjectId>> {
        let root = self
            .catalog()?
            .get(b"Pages")
            .ok_or_else(|| EngineError::Missing("catalog /Pages".into()))?
            .clone();
        let mut ids = Vec::new();
        self.collect_pages(&root, &mut ids, 0)?;
        Ok(ids)
    }

    fn collect_pages(&self, node: &Object, out: &mut Vec<ObjectId>, depth: usize) -> Result<()> {
        if depth > 50 {
            return Err(EngineError::Unsupported("page tree too deep".into()));
        }
        let node_id = node.as_reference();
        let dict = match self.resolve(node).as_dict() {
            Some(dict) => dict,
            None => return Ok(()),
        };

        let is_pages_node = dict.get(b"Type").and_then(Object::as_name)
            == Some(b"Pages".as_slice())
            || dict.contains(b"Kids");

        if is_pages_node {
            if let Some(kids) = dict.get(b"Kids") {
                if let Some(items) = self.resolve(kids).as_array() {
                    for kid in items {
                        self.collect_pages(kid, out, depth + 1)?;
                    }
                }
            }
        } else if let Some(id) = node_id {
            out.push(id); // a leaf page
        }
        Ok(())
    }

    /// The page dictionary for a 1-based page number.
    pub fn page_dict(&self, page_no: u32) -> Result<&Dictionary> {
        let ids = self.page_ids()?;
        let id = ids
            .get(page_no.saturating_sub(1) as usize)
            .ok_or(EngineError::PageNotFound(page_no))?;
        self.objects
            .get(id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))
    }

    /// The decoded (filters applied) content stream of a 1-based page.
    ///
    /// Multiple content streams are concatenated with a newline, as a PDF
    /// consumer would.
    pub fn page_content(&self, page_no: u32) -> Result<Vec<u8>> {
        let page = self.page_dict(page_no)?;
        let contents = page
            .get(b"Contents")
            .ok_or_else(|| EngineError::Missing("page /Contents".into()))?
            .clone();
        let mut out = Vec::new();
        self.append_content(&contents, &mut out)?;
        Ok(out)
    }

    // ─── content editing (Word-like) ─────────────────────────────────────────

    /// 1-based page number → its object id.
    fn page_object_id(&self, page_no: u32) -> Result<ObjectId> {
        let ids = self.page_ids()?;
        ids.get(page_no.saturating_sub(1) as usize)
            .copied()
            .ok_or(EngineError::PageNotFound(page_no))
    }

    /// Next free object number (one past the current maximum).
    fn next_object_number(&self) -> u32 {
        self.objects.keys().map(|(n, _)| *n).max().unwrap_or(0) + 1
    }

    /// The text runs on a page (1-based), in reading order. Text is decoded
    /// font-aware (WinAnsi + `/ToUnicode` for CID/Type0 and custom encodings)
    /// so extraction has no tofu.
    pub fn page_text_runs(&self, page_no: u32) -> Result<Vec<TextRun>> {
        let content = self.page_content(page_no)?;
        let fonts = self.page_font_decoders(page_no);
        // Recurse into form XObjects (`Do`) so text drawn via reusable forms
        // (invoice/template content) is extracted, not just the top-level stream.
        let forms = self.page_form_xobjects(page_no);
        content::extract_text_runs_resolved(
            &content,
            &fonts,
            content::PageMatrix::IDENTITY,
            &|name| forms.get(name).cloned(),
            0,
        )
    }

    /// Build per-font text decoders from a page's `/Resources /Font`, reading
    /// each font's `/Subtype` (Type0 ⇒ 2-byte codes) and `/ToUnicode` CMap.
    fn page_font_decoders(&self, page_no: u32) -> content::FontDecoders {
        let Ok(page) = self.page_dict(page_no) else {
            return content::FontDecoders::new();
        };
        match page.get(b"Resources").map(|o| self.resolve(o)).and_then(Object::as_dict) {
            Some(res) => self.font_decoders_for(res),
            None => content::FontDecoders::new(),
        }
    }

    /// Build per-font text decoders from a `/Resources` dictionary's `/Font`
    /// sub-dictionary, reading each font's `/Subtype` (Type0 ⇒ 2-byte codes) and
    /// `/ToUnicode` CMap. Works for a page's resources *and* a form XObject's own
    /// resources, so text drawn inside form XObjects decodes with the right font.
    fn font_decoders_for(&self, resources: &Dictionary) -> content::FontDecoders {
        let mut decoders = content::FontDecoders::new();
        let Some(font_dict) = resources
            .get(b"Font")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
        else {
            return decoders;
        };
        for (name, value) in &font_dict.0 {
            let Some(font) = self.resolve(value).as_dict() else {
                continue;
            };
            let two_byte =
                font.get(b"Subtype").and_then(Object::as_name) == Some(b"Type0".as_slice());
            let to_unicode = font
                .get(b"ToUnicode")
                .map(|o| self.resolve(o))
                .and_then(Object::as_stream)
                .and_then(|stream| decode_stream(stream).ok())
                .map(|bytes| crate::font::cmap::ToUnicode::parse(&bytes))
                .filter(|cmap| !cmap.is_empty());
            let widths = if two_byte {
                self.cid_font_widths(font)
            } else {
                self.simple_font_widths(font)
            };
            // For an Identity-H Type0 font lacking a /ToUnicode, derive a
            // glyph-id → Unicode map from the embedded font's own cmap so text
            // still extracts as real characters (not tofu).
            let cid_to_unicode = if two_byte && to_unicode.is_none() {
                self.cid_font_cmap_unicode(font)
            } else {
                None
            };
            decoders.insert(
                name.clone(),
                crate::font::cmap::TextDecoder {
                    two_byte,
                    to_unicode,
                    widths,
                    cid_to_unicode,
                },
            );
        }
        decoders
    }

    /// Glyph-id → Unicode map from a Type0 font's embedded program (`FontFile2`
    /// glyf or `FontFile3` OpenType/CFF) cmap, for Identity-H fonts that carry no
    /// `/ToUnicode`. `None` when the font isn't embedded or has no usable cmap.
    fn cid_font_cmap_unicode(
        &self,
        font: &Dictionary,
    ) -> Option<std::collections::BTreeMap<u16, String>> {
        let desc = font
            .get(b"DescendantFonts")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)?
            .first()?;
        let cid = self.resolve(desc).as_dict()?;
        // Identity-H only: a non-identity CIDToGIDMap would break the code==gid
        // assumption the cmap reverse-map relies on.
        if let Some(map) = cid.get(b"CIDToGIDMap").map(|o| self.resolve(o)) {
            if map.as_name() != Some(b"Identity".as_slice()) {
                return None;
            }
        }
        let fd = cid
            .get(b"FontDescriptor")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        let ttf = if let Some(ff) = fd
            .get(b"FontFile2")
            .map(|o| self.resolve(o))
            .and_then(Object::as_stream)
        {
            let bytes = decode_stream(ff).ok()?;
            crate::font::truetype::TrueTypeFont::parse(&bytes)?
        } else {
            let ff = fd
                .get(b"FontFile3")
                .map(|o| self.resolve(o))
                .and_then(Object::as_stream)?;
            let bytes = decode_stream(ff).ok()?;
            crate::font::truetype::TrueTypeFont::parse_metrics(&bytes)?
        };
        let map: std::collections::BTreeMap<u16, String> = ttf
            .gid_to_unicode_map()
            .into_iter()
            .filter_map(|(gid, cp)| char::from_u32(cp).map(|c| (gid, c.to_string())))
            .collect();
        (!map.is_empty()).then_some(map)
    }

    /// Build a flattened `name → `[`content::FormXObject`]` map of every form
    /// XObject reachable from `page_no` (the page's `/Resources /XObject`, and
    /// recursively each form's own `/Resources /XObject`), so text drawn inside
    /// reusable form XObjects becomes addressable.
    ///
    /// Each form carries its decoded content, its per-font decoders (its *own*
    /// `/Resources /Font` overlaid on the page's, form wins), and its `/Matrix`.
    /// Outer scopes win on name collision (insert-if-absent during the descent),
    /// and a visited-`ObjectId` set breaks self/mutual cycles. The returned map
    /// backs the `resolve_form` closure passed to
    /// [`content::extract_text_runs_resolved`] / [`content::extract_elements_resolved`].
    fn page_form_xobjects(&self, page_no: u32) -> BTreeMap<Vec<u8>, content::FormXObject> {
        let mut forms = BTreeMap::new();
        let Ok(page) = self.page_dict(page_no) else {
            return forms;
        };
        let Some(page_res) = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
        else {
            return forms;
        };
        let mut visited: BTreeSet<ObjectId> = BTreeSet::new();
        self.collect_form_xobjects(&page_res, &page_res, &mut forms, &mut visited, 0);
        forms
    }

    /// Recursive worker for [`page_form_xobjects`](Self::page_form_xobjects):
    /// scan `scope`'s `/XObject` for `/Subtype /Form` entries, record each under
    /// its name (insert-if-absent), then descend into the form's own resources
    /// (falling back to `page_res`). `visited` holds the object ids on the
    /// current path to break cycles; `depth` caps pathological nesting.
    fn collect_form_xobjects(
        &self,
        scope: &Dictionary,
        page_res: &Dictionary,
        forms: &mut BTreeMap<Vec<u8>, content::FormXObject>,
        visited: &mut BTreeSet<ObjectId>,
        depth: usize,
    ) {
        if depth > 16 {
            return;
        }
        let Some(xobjects) = scope
            .get(b"XObject")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
        else {
            return;
        };
        for (name, value) in &xobjects.0 {
            // Cycle guard: skip a form already on the current resolution path.
            let oid = value.as_reference();
            if let Some(id) = oid {
                if visited.contains(&id) {
                    continue;
                }
            }
            let Some(stream) = self.resolve(value).as_stream() else {
                continue;
            };
            if stream.dict.get(b"Subtype").and_then(Object::as_name)
                != Some(b"Form".as_slice())
            {
                continue;
            }
            let Ok(content) = decode_stream(stream) else {
                continue;
            };
            // The form's own resources take precedence; fall back to the page's
            // for inherited fonts/XObjects (PDF resource-inheritance rule).
            let own_res = stream
                .dict
                .get(b"Resources")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict);
            let fns = self.form_font_decoders(own_res, page_res);
            let matrix = self.form_matrix(&stream.dict);
            // Insert-if-absent: an outer scope's definition of `name` wins.
            forms
                .entry(name.clone())
                .or_insert_with(|| content::FormXObject {
                    content,
                    fns,
                    matrix,
                    ref_id: oid,
                });
            // Descend into this form's resources for nested forms, guarding the
            // path against cycles.
            if let Some(res) = own_res {
                let res = res.clone();
                let pushed = oid.map(|id| visited.insert(id)).unwrap_or(false);
                self.collect_form_xobjects(&res, page_res, forms, visited, depth + 1);
                if pushed {
                    if let Some(id) = oid {
                        visited.remove(&id);
                    }
                }
            }
        }
    }

    /// Per-font decoders for a form XObject: its own `/Resources /Font` overlaid
    /// on the page's (form definitions win), honouring resource inheritance so a
    /// form that omits a font still decodes via the page's.
    fn form_font_decoders(
        &self,
        own_res: Option<&Dictionary>,
        page_res: &Dictionary,
    ) -> content::FontDecoders {
        let mut decoders = self.font_decoders_for(page_res);
        if let Some(res) = own_res {
            for (name, decoder) in self.font_decoders_for(res) {
                decoders.insert(name, decoder);
            }
        }
        decoders
    }

    /// A form XObject's `/Matrix` (default identity) as a [`content::PageMatrix`].
    fn form_matrix(&self, dict: &Dictionary) -> content::PageMatrix {
        let m = dict
            .get(b"Matrix")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(|a| {
                let v: Vec<f64> = a.iter().filter_map(|o| self.resolve(o).as_f64()).collect();
                v
            })
            .filter(|v| v.len() == 6);
        match m {
            Some(v) => content::PageMatrix::new(v[0], v[1], v[2], v[3], v[4], v[5]),
            None => content::PageMatrix::IDENTITY,
        }
    }

    /// A simple font's per-code advance widths from `/FirstChar` + `/Widths`
    /// (with `/FontDescriptor /MissingWidth` as the default). When the font has
    /// no `/Widths` array, base-14 Helvetica/Courier fall back to their built-in
    /// AFM/monospace metrics; other base-14 (Times) return `None` (estimate).
    fn simple_font_widths(&self, font: &Dictionary) -> Option<crate::font::cmap::CodeWidths> {
        let Some(widths) = font
            .get(b"Widths")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        else {
            return self.base14_widths(font);
        };
        let first = font
            .get(b"FirstChar")
            .and_then(Object::as_i64)
            .unwrap_or(0)
            .max(0) as u32;
        let missing = font
            .get(b"FontDescriptor")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|d| d.get(b"MissingWidth"))
            .and_then(Object::as_f64)
            .unwrap_or(0.0);
        let mut map = BTreeMap::new();
        for (i, w) in widths.iter().enumerate() {
            if let Some(v) = self.resolve(w).as_f64() {
                map.insert(first + i as u32, v);
            }
        }
        Some(crate::font::cmap::CodeWidths::new(map, missing))
    }

    /// Built-in metrics for a base-14 simple font that omits `/Widths`: the
    /// Helvetica AFM table for Helvetica/Arial, a flat 600 for Courier
    /// (monospace). Codes 0x20–0x7E (ASCII/WinAnsi). `None` for fonts whose
    /// metrics we don't ship (Times) — the caller then estimates.
    fn base14_widths(&self, font: &Dictionary) -> Option<crate::font::cmap::CodeWidths> {
        let base = font.get(b"BaseFont").and_then(Object::as_name)?;
        let name = String::from_utf8_lossy(base);
        let face = name.rsplit('+').next().unwrap_or(&name).to_lowercase();
        let mut map = BTreeMap::new();
        if face.contains("courier") || face.contains("mono") {
            for code in 0x20u32..=0x7e {
                map.insert(code, 600.0);
            }
            return Some(crate::font::cmap::CodeWidths::new(map, 600.0));
        }
        if face.contains("helvetica") || face.contains("arial") {
            for (i, &w) in HELVETICA_AFM.iter().enumerate() {
                map.insert(0x20 + i as u32, w as f64);
            }
            return Some(crate::font::cmap::CodeWidths::new(map, 500.0));
        }
        None
    }

    /// A Type0 font's per-CID advance widths from its descendant font's `/W`
    /// array (and `/DW` default, 1000 when absent). Honours both `/W` forms:
    /// `c [w1 w2 …]` (consecutive CIDs) and `cFirst cLast w` (a CID range).
    fn cid_font_widths(&self, font: &Dictionary) -> Option<crate::font::cmap::CodeWidths> {
        let descendant = font
            .get(b"DescendantFonts")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .and_then(|a| a.first())
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        let dw = descendant.get(b"DW").and_then(Object::as_f64).unwrap_or(1000.0);
        let mut map = BTreeMap::new();
        if let Some(w) = descendant
            .get(b"W")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            let mut i = 0;
            while i < w.len() {
                let Some(c) = self.resolve(&w[i]).as_i64() else {
                    break;
                };
                if let Some(list) = w
                    .get(i + 1)
                    .map(|o| self.resolve(o))
                    .and_then(Object::as_array)
                {
                    // Form: c [w1 w2 …] — CIDs c, c+1, … each with its width.
                    for (j, wv) in list.iter().enumerate() {
                        if let Some(v) = self.resolve(wv).as_f64() {
                            map.insert((c as u32).wrapping_add(j as u32), v);
                        }
                    }
                    i += 2;
                } else if let (Some(c2), Some(wv)) = (
                    w.get(i + 1).map(|o| self.resolve(o)).and_then(Object::as_i64),
                    w.get(i + 2).map(|o| self.resolve(o)).and_then(Object::as_f64),
                ) {
                    // Form: cFirst cLast w — every CID in the range gets `w`.
                    let (lo, hi) = (c.max(0) as u32, c2.max(0) as u32);
                    if hi >= lo && hi - lo < 70_000 {
                        for code in lo..=hi {
                            map.insert(code, wv);
                        }
                    }
                    i += 3;
                } else {
                    break;
                }
            }
        }
        Some(crate::font::cmap::CodeWidths::new(map, dw))
    }

    /// Map each font resource name on a page to a recovered [`TextStyle`]
    /// (family/weight/style) parsed from its `/BaseFont`. Used by the Office
    /// exporters to carry real fonts, not just sizes.
    fn page_base_fonts(&self, page_no: u32) -> BTreeMap<String, crate::convert::TextStyle> {
        let mut out = BTreeMap::new();
        let Ok(page) = self.page_dict(page_no) else {
            return out;
        };
        let font_dict = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|res| res.get(b"Font"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let Some(font_dict) = font_dict else {
            return out;
        };
        for (name, value) in &font_dict.0 {
            let Some(font) = self.resolve(value).as_dict() else {
                continue;
            };
            if let Some(base) = font.get(b"BaseFont").and_then(Object::as_name) {
                let style = crate::convert::style::parse_base_font(&String::from_utf8_lossy(base));
                out.insert(String::from_utf8_lossy(name).into_owned(), style);
            }
        }
        out
    }

    /// Replace a text run's text in place (keeps position and font).
    pub fn replace_text_run(&mut self, page_no: u32, index: usize, new_text: &str) -> Result<()> {
        let content = self.page_content(page_no)?;
        // Font-aware: a Type0/Identity-H run stores 2-byte glyph ids, so re-encode
        // `new_text` through the font's char→GID map; simple fonts take the
        // WinAnsi single-byte path. This makes modify work with *any* font.
        let edited = match self.encode_run_for_font(page_no, &content, index, new_text) {
            Some((bytes, kind)) => content::replace_text_run_encoded(&content, index, bytes, kind)?,
            None => content::replace_text_run(&content, index, new_text)?,
        };
        self.set_page_content(page_no, edited)
    }

    /// If the `index`-th run on `page_no` is set in a Type0/Identity-H font,
    /// encode `new_text` to its 2-byte glyph ids (returned as `Hex` bytes) and
    /// record those gids for subsetting; otherwise `None` (the caller falls back
    /// to single-byte WinAnsi). The bridge that lets [`replace_text_run`] handle
    /// embedded TrueType and OpenType-CFF faces, not just base-14.
    fn encode_run_for_font(
        &mut self,
        page_no: u32,
        content: &[u8],
        index: usize,
        new_text: &str,
    ) -> Option<(Vec<u8>, StringKind)> {
        let res = content::text_run_font_name(content, index).ok()??;
        let font_obj = self.page_font_object(page_no, &res)?;
        if !self.is_identity_h_font(font_obj) {
            return None;
        }
        let ttf = self.embedded_truetype(font_obj)?;
        let mut bytes = Vec::with_capacity(new_text.chars().count() * 2);
        let used = self.font_used_gids.entry(font_obj).or_default();
        for ch in new_text.chars() {
            let gid = ttf.gid_for_unicode(ch as u32).unwrap_or(0);
            used.insert(gid);
            bytes.extend_from_slice(&gid.to_be_bytes());
        }
        Some((bytes, StringKind::Hex))
    }

    /// The object number of the font registered under resource `name` in
    /// `page_no`'s `/Resources /Font`, or `None` (inline font dicts have no id).
    fn page_font_object(&self, page_no: u32, name: &[u8]) -> Option<u32> {
        let page = self.page_dict(page_no).ok()?;
        let font_dict = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|res| res.get(b"Font"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        match font_dict.get(name)? {
            Object::Reference((num, _)) => Some(*num),
            _ => None,
        }
    }

    /// True when `font_obj` is a Type0 font with the `Identity-H` encoding name
    /// (its content codes are raw 2-byte glyph ids).
    fn is_identity_h_font(&self, font_obj: u32) -> bool {
        let Some(t0) = self.objects.get(&(font_obj, 0)).and_then(Object::as_dict) else {
            return false;
        };
        t0.get(b"Subtype").and_then(Object::as_name) == Some(b"Type0".as_slice())
            && t0.get(b"Encoding").and_then(Object::as_name) == Some(b"Identity-H".as_slice())
    }

    /// Remove a text run, preserving the rest of the page (background intact).
    pub fn remove_text_run(&mut self, page_no: u32, index: usize) -> Result<()> {
        let content = self.page_content(page_no)?;
        let edited = content::remove_text_run(&content, index)?;
        self.set_page_content(page_no, edited)
    }

    /// All addressable elements (text, images, shapes) of a page's **top-level**
    /// content stream, in order. Does **not** descend into form XObjects, so each
    /// element's `index` and `op_start..=op_end` map one-to-one onto the editable
    /// top-level operations — this is the list the index-based mutation and
    /// hit-test APIs ([`remove_element`](Self::remove_element),
    /// [`move_element`](Self::move_element),
    /// [`duplicate_element`](Self::duplicate_element),
    /// [`element_at`](Self::element_at), [`redact_region`](Self::redact_region))
    /// rely on. For *extraction/display* that also reaches text drawn inside
    /// reusable form XObjects, use [`page_elements_deep`](Self::page_elements_deep).
    pub fn page_elements(&self, page_no: u32) -> Result<Vec<ContentElement>> {
        let content = self.page_content(page_no)?;
        let fonts = self.page_font_decoders(page_no);
        let gstate_alpha = self.page_gstate_alpha(page_no);
        content::extract_elements_with(&content, &fonts, &gstate_alpha)
    }

    /// All addressable elements of a page **recursing into form XObjects** (`Do`):
    /// text/graphics drawn through reusable forms (invoice/template content) yield
    /// their nested elements — in page user space — instead of one opaque image.
    ///
    /// This is the **extraction** view (reading, conversion, structured text and
    /// `page_text_elements`); its element indices include form content and so do
    /// **not** line up with the top-level op stream — never feed them to the
    /// index-based mutation APIs (use [`page_elements`](Self::page_elements) for
    /// that). Nested elements collapse their `op_start`/`op_end` onto the `Do`.
    pub fn page_elements_deep(&self, page_no: u32) -> Result<Vec<ContentElement>> {
        let content = self.page_content(page_no)?;
        let fonts = self.page_font_decoders(page_no);
        let gstate_alpha = self.page_gstate_alpha(page_no);
        let forms = self.page_form_xobjects(page_no);
        content::extract_elements_resolved(&content, &fonts, &gstate_alpha, &|name| {
            forms.get(name).cloned()
        })
    }

    /// Map each `/ExtGState` resource name on `page_no` to its `/ca` (non-stroking
    /// fill alpha). The element walker reads this when it hits a `gs` operator so
    /// elements carry their effective opacity. Derived from
    /// [`page_gstate_alpha_pair`](Self::page_gstate_alpha_pair); names without a
    /// `/ca` are skipped (they leave the alpha unchanged).
    fn page_gstate_alpha(&self, page_no: u32) -> BTreeMap<String, f64> {
        self.page_gstate_alpha_pair(page_no)
            .into_iter()
            .filter_map(|(name, (ca, _))| ca.map(|v| (name, v)))
            .collect()
    }

    /// Map each `/ExtGState` resource name on `page_no` to its `(/ca, /CA)`
    /// non-stroking and stroking alphas (each `None` when the key is absent so
    /// the walker leaves that alpha unchanged). Drives both element opacity and
    /// vector-path fill/stroke opacity.
    fn page_gstate_alpha_pair(&self, page_no: u32) -> BTreeMap<String, (Option<f64>, Option<f64>)> {
        let mut out = BTreeMap::new();
        let Ok(page) = self.page_dict(page_no) else {
            return out;
        };
        let gs_dict = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|res| res.get(b"ExtGState"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let Some(gs_dict) = gs_dict else {
            return out;
        };
        for (name, value) in &gs_dict.0 {
            let Some(state) = self.resolve(value).as_dict() else {
                continue;
            };
            let ca = state.get(b"ca").and_then(Object::as_f64);
            let ca_stroke = state.get(b"CA").and_then(Object::as_f64);
            if ca.is_some() || ca_stroke.is_some() {
                out.insert(String::from_utf8_lossy(name).into_owned(), (ca, ca_stroke));
            }
        }
        out
    }

    /// Every painted vector path on a page (frames, rules, lines, filled shapes…)
    /// as geometry + style: segments in user space (origin bottom-left), the
    /// fill/stroke RGB colours, line width, alpha and dash. Clip-only paths are
    /// omitted. The native equivalent of a reader's vector/shape layer, driving a
    /// host editor without a rasteriser.
    pub fn page_vector_paths(&self, page_no: u32) -> Result<Vec<content::vector::VectorPath>> {
        let content = self.page_content(page_no)?;
        let gstate = self.page_gstate_alpha_pair(page_no);
        let operations = content::parse_content(&content)?;
        Ok(content::vector::vector_paths_from_ops(&operations, &gstate))
    }

    /// Every **text** element on a page, enriched with everything a host editor
    /// needs to recreate each run: bounding box (user space, bottom-left), the
    /// resolved `/BaseFont` family + bold/italic, the effective point size, the
    /// RGB fill colour and the baseline rotation. **Recurses into form XObjects**
    /// so text drawn through reusable forms (invoice/template content) is
    /// returned too, positioned in page space — previously visible on screen but
    /// missing from this list.
    ///
    /// For **top-level** runs the returned `index` is the text-run index accepted
    /// by [`replace_text_run`](Self::replace_text_run), so a host can extract,
    /// display and edit in one model. Text that lives **inside a form XObject**
    /// is included for display but is not editable in place (it is shared across
    /// every placement of the form); such elements carry `index == usize::MAX`,
    /// which `replace_text_run` rejects rather than misrouting to a top-level run.
    pub fn page_text_elements(&self, page_no: u32) -> Vec<TextElementInfo> {
        let styles = self.page_base_fonts(page_no);
        let Ok(elements) = self.page_elements_deep(page_no) else {
            return Vec::new();
        };
        // Top-level text runs keep their ordinal (it indexes `replace_text_run`,
        // which counts only the page's own `Tj`/`TJ`); form-XObject text gets a
        // sentinel index so editing it is a safe no-op, not a wrong-run edit.
        let mut top_run = 0usize;
        elements
            .into_iter()
            .filter(|e| e.kind == content::ElementKind::Text)
            .map(|e| {
                let index = if e.nested {
                    usize::MAX
                } else {
                    let idx = top_run;
                    top_run += 1;
                    idx
                };
                let style = e.font.as_ref().and_then(|name| styles.get(name));
                let b = e.bounds.unwrap_or(content::Bounds {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                });
                TextElementInfo {
                    index,
                    text: e.label,
                    x: b.x,
                    y: b.y,
                    width: b.width,
                    height: b.height,
                    font_family: style
                        .map(|s| s.family.clone())
                        .unwrap_or_else(|| "Helvetica".to_string()),
                    bold: style.map(|s| s.bold).unwrap_or(false),
                    italic: style.map(|s| s.italic).unwrap_or(false),
                    font_size: e.font_size.filter(|s| *s > 0.0).unwrap_or(b.height),
                    color: e.color.unwrap_or([0.0, 0.0, 0.0]),
                    rotation_deg: e.rotation_deg.unwrap_or(0.0),
                }
            })
            .collect()
    }

    /// Every **image** element on a page: its placement box (user space), the
    /// embeddable encoded bytes + format, and the source pixel dimensions.
    /// DCTDecode/JPXDecode images pass through as `jpeg`/`jp2`; Flate/raw
    /// DeviceRGB|DeviceGray 8-bit images are re-encoded to PNG; anything else is
    /// reported `unknown` with empty bytes. The native equivalent of a reader's
    /// image extraction (placement + bytes, not just a render).
    pub fn page_image_elements(&self, page_no: u32) -> Vec<ImageElementInfo> {
        let Ok(elements) = self.page_elements(page_no) else {
            return Vec::new();
        };
        let images: Vec<(content::Bounds, String, f64, f64)> = elements
            .into_iter()
            .filter(|e| e.kind == content::ElementKind::Image)
            .map(|e| {
                let b = e.bounds.unwrap_or(content::Bounds {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                });
                let rotation = e.rotation_deg.unwrap_or(0.0);
                let opacity = e.fill_alpha.unwrap_or(1.0);
                (b, e.label, rotation, opacity)
            })
            .collect();
        let mut out = Vec::new();
        for (idx, (b, name, rotation, opacity)) in images.into_iter().enumerate() {
            let Some((format, data, pw, ph)) = self.image_xobject_bytes(page_no, name.as_bytes())
            else {
                continue;
            };
            out.push(ImageElementInfo {
                index: idx,
                x: b.x,
                y: b.y,
                width: b.width,
                height: b.height,
                format,
                pixel_width: pw,
                pixel_height: ph,
                data,
                rotation,
                opacity,
            });
        }
        out
    }

    /// Resolve image XObject `name` in `page_no`'s `/Resources /XObject` to
    /// `(format, encoded bytes, pixel width, pixel height)`. `None` when the
    /// name isn't an image XObject.
    fn image_xobject_bytes(&self, page_no: u32, name: &[u8]) -> Option<(String, Vec<u8>, u32, u32)> {
        let page = self.page_dict(page_no).ok()?;
        let stream = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|res| res.get(b"XObject"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|xo| xo.get(name))
            .map(|o| self.resolve(o))
            .and_then(Object::as_stream)?;
        let dict = &stream.dict;
        if dict.get(b"Subtype").and_then(Object::as_name) != Some(b"Image".as_slice()) {
            return None;
        }
        let pw = dict.get(b"Width").and_then(Object::as_i64).unwrap_or(0).max(0) as u32;
        let ph = dict.get(b"Height").and_then(Object::as_i64).unwrap_or(0).max(0) as u32;
        match self.first_filter(dict).as_deref() {
            Some(b"DCTDecode") => Some(("jpeg".to_string(), stream.raw.clone(), pw, ph)),
            Some(b"JPXDecode") => Some(("jp2".to_string(), stream.raw.clone(), pw, ph)),
            _ => match self.image_to_png(stream) {
                Some(png) => Some(("png".to_string(), png, pw, ph)),
                None => Some(("unknown".to_string(), Vec::new(), pw, ph)),
            },
        }
    }

    /// Decode a (Flate/raw, DeviceRGB|DeviceGray, 8-bit) image stream to RGBA —
    /// honouring an 8-bit DeviceGray `/SMask` for alpha — and PNG-encode it.
    /// `None` when the colour space / bit depth isn't one we decode.
    fn image_to_png(&self, stream: &Stream) -> Option<Vec<u8>> {
        let dict = &stream.dict;
        if dict.get(b"BitsPerComponent").and_then(Object::as_i64).unwrap_or(8) != 8 {
            return None;
        }
        let components = match dict
            .get(b"ColorSpace")
            .map(|o| self.resolve(o))
            .and_then(Object::as_name)
        {
            Some(b"DeviceRGB") => 3,
            Some(b"DeviceGray") => 1,
            _ => return None,
        };
        let width = dict.get(b"Width").and_then(Object::as_i64).unwrap_or(0).max(0) as usize;
        let height = dict.get(b"Height").and_then(Object::as_i64).unwrap_or(0).max(0) as usize;
        let samples = decode_stream(stream).ok()?;
        if width == 0 || height == 0 || samples.len() < width * height * components {
            return None;
        }
        let smask = self.decode_gray_smask(dict);
        let mut rgba = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) * components;
                let (r, g, b) = if components == 1 {
                    (samples[i], samples[i], samples[i])
                } else {
                    (samples[i], samples[i + 1], samples[i + 2])
                };
                let a = match &smask {
                    Some((sw, sh, alpha)) => {
                        let sx = if *sw == width { x } else { x * *sw / width };
                        let sy = if *sh == height { y } else { y * *sh / height };
                        alpha.get(sy * *sw + sx).copied().unwrap_or(255)
                    }
                    None => 255,
                };
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }
        Some(crate::raster::png::encode_png(width as u32, height as u32, &rgba))
    }

    /// Redact a rectangular region (page user space): permanently **remove**
    /// every content element overlapping it from the content stream. Returns how
    /// many elements were removed.
    ///
    /// This is true redaction by deletion — the text/graphics are gone from the
    /// stream (uncopyable, unrecoverable) and **whatever was behind them (a
    /// gradient, image or pattern background) is preserved untouched**. Pass a
    /// `cover` colour only when you want to visibly mark the censored area
    /// (legal redaction); `None` leaves the background showing through.
    pub fn redact_region(
        &mut self,
        page_no: u32,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        cover: Option<[f64; 3]>,
    ) -> Result<usize> {
        let region = content::Bounds {
            x,
            y,
            width,
            height,
        };
        let mut hits: Vec<usize> = self
            .page_elements(page_no)?
            .into_iter()
            .filter(|e| e.bounds.is_some_and(|b| b.intersects(&region)))
            .map(|e| e.index)
            .collect();
        // Remove highest index first so the remaining target indices stay valid.
        hits.sort_unstable_by(|a, b| b.cmp(a));
        for index in &hits {
            self.remove_element(page_no, *index)?;
        }
        // Optional visible cover; by default the background shows through.
        if let Some(color) = cover {
            self.add_rectangle(page_no, x, y, width, height, None, Some(color), 0.0, 1.0)?;
        }
        Ok(hits.len())
    }

    /// The page's `/MediaBox` `[x0 y0 x1 y1]`, defaulting to US Letter.
    fn read_media_box(&self, page: &Dictionary) -> [f64; 4] {
        if let Some(values) = page
            .get(b"MediaBox")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            let nums: Vec<f64> = values.iter().filter_map(Object::as_f64).collect();
            if nums.len() == 4 {
                return [nums[0], nums[1], nums[2], nums[3]];
            }
        }
        [0.0, 0.0, 612.0, 792.0]
    }

    /// A page's `(width, height, rotation)`: dimensions from `/MediaBox`, and
    /// `/Rotate` normalized to 0/90/180/270 (the orientation a viewer applies).
    pub fn page_info(&self, page_no: u32) -> Result<(f64, f64, i32)> {
        let page = self.page_dict(page_no)?;
        let mb = self.read_media_box(page);
        let width = (mb[2] - mb[0]).abs();
        let height = (mb[3] - mb[1]).abs();
        let rotate = page.get(b"Rotate").and_then(Object::as_i64).unwrap_or(0);
        let rotation = (((rotate % 360) + 360) % 360) as i32;
        Ok((width, height, rotation))
    }

    /// A page's raw `/MediaBox` `[x0, y0, x1, y1]` in user-space points (default
    /// `[0, 0, 612, 792]` US-Letter when absent). Unlike [`page_info`](Self::page_info)
    /// (which returns the size), this preserves the box origin, so a host can
    /// reconstruct the exact page coordinate frame.
    pub fn page_media_box(&self, page_no: u32) -> Result<[f64; 4]> {
        Ok(self.read_media_box(self.page_dict(page_no)?))
    }

    /// Rasterize a page to a PNG at `scale` device pixels per PDF point, using
    /// the built-in zero-dependency renderer (vector graphics; text glyphs and
    /// images are added by later renderer slices).
    pub fn render_page(&self, page_no: u32, scale: f64) -> Result<Vec<u8>> {
        Ok(self.render_page_canvas(page_no, scale)?.to_png())
    }

    /// Rasterize a page to an RGBA [`Canvas`](crate::raster::Canvas) at `scale`
    /// device pixels per point. Shared by `render_page` and `ocr_page`.
    fn render_page_canvas(&self, page_no: u32, scale: f64) -> Result<crate::raster::Canvas> {
        let media_box = self.read_media_box(self.page_dict(page_no)?);
        let [x0, y0, x1, y1] = media_box;
        let w_pts = (x1 - x0).abs();
        let h_pts = (y1 - y0).abs();
        let scale = scale.max(0.01);
        let width = ((w_pts * scale).ceil() as u32).max(1);
        let height = ((h_pts * scale).ceil() as u32).max(1);
        let base =
            content::PageMatrix::new(scale, 0.0, 0.0, -scale, -x0 * scale, (y0 + h_pts) * scale);
        let content = self.page_content(page_no)?;
        let fonts = self.page_render_fonts(page_no);
        let images = self.page_images(page_no);
        let resources = self.page_resources(page_no);
        let ctx = PageResourceCtx::new(self, resources, width, height, base);
        let mut canvas = crate::raster::Canvas::new(width, height);
        crate::raster::render_content_into_ctx(
            &mut canvas,
            &content,
            base,
            &fonts,
            &images,
            1.0,
            &ctx,
            0,
            None,
        );
        // Paint annotation appearances (`/AP /N`) over the page content, the way
        // every viewer does: page content is the body, annotations layer on top.
        self.render_annotation_appearances(page_no, &mut canvas, base);
        Ok(canvas)
    }

    /// Paint each visible annotation's normal appearance stream (`/AP /N`) onto
    /// `canvas`, over the already-rendered page content. `base` is the page
    /// user-space → device matrix.
    ///
    /// For every annotation on the page we skip the Hidden (bit 2) and NoView
    /// (bit 6) flags, resolve `/AP /N` (selecting an appearance-state sub-dict
    /// via `/AS`), then map the appearance's `/BBox` (transformed by its
    /// `/Matrix`) onto the annotation `/Rect` with the standard appearance
    /// transform (ISO 32000-1 §12.5.5) and rasterize the form through the shared
    /// content renderer. Annotations without an appearance are skipped. `/CA`
    /// (non-stroking opacity) scales the paint.
    fn render_annotation_appearances(
        &self,
        page_no: u32,
        canvas: &mut crate::raster::Canvas,
        base: content::PageMatrix,
    ) {
        let Ok(page) = self.page_dict(page_no) else {
            return;
        };
        let annots = match page.get(b"Annots").map(|o| self.resolve(o)) {
            Some(Object::Array(items)) => items,
            _ => return,
        };
        for item in annots {
            let Some(dict) = self.resolve(item).as_dict() else {
                continue;
            };
            // Skip Hidden (bit 2 → value 2) and NoView (bit 6 → value 32).
            let flags = dict.get(b"F").and_then(Object::as_i64).unwrap_or(0);
            if flags & 0b10 != 0 || flags & 0b10_0000 != 0 {
                continue;
            }
            let Some(form_id) = self.annotation_appearance_id(dict) else {
                continue; // No appearance to draw.
            };
            let Some(stream) = self.objects.get(&form_id).and_then(Object::as_stream) else {
                continue;
            };
            let Ok(appearance) = decode_stream(stream) else {
                continue;
            };
            let rect = self.normalized_rect(self.read_rect(dict));
            let bbox = self.read_bbox(&stream.dict);
            let matrix = self.form_matrix(&stream.dict);
            let Some(ap_matrix) = appearance_matrix(bbox, matrix, rect) else {
                continue; // Degenerate BBox/Rect — nothing sensible to map.
            };
            let resources = stream
                .dict
                .get(b"Resources")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
                .cloned()
                .unwrap_or_default();
            let fonts = self.render_fonts_for(&resources);
            let images = self.images_for(&resources);
            let ctx =
                PageResourceCtx::new(self, resources.clone(), canvas.width, canvas.height, base);
            // Compose: appearance space → page user space → device.
            let device = ap_matrix.then(&base);
            let alpha = dict
                .get(b"CA")
                .map(|o| self.resolve(o))
                .and_then(|o| o.as_f64())
                .unwrap_or(1.0);
            crate::raster::render_content_into_ctx(
                canvas,
                &appearance,
                device,
                &fonts,
                &images,
                alpha,
                &ctx,
                0,
                None,
            );
        }
    }

    /// A page's `/Resources` dictionary (empty when absent).
    fn page_resources(&self, page_no: u32) -> Dictionary {
        self.page_dict(page_no)
            .ok()
            .and_then(|page| {
                page.get(b"Resources")
                    .map(|o| self.resolve(o))
                    .and_then(Object::as_dict)
                    .cloned()
            })
            .unwrap_or_default()
    }

    // ── rasterizer resource resolution (form XObjects, shadings, ExtGState) ──

    /// Resolve a name in a `/Resources` sub-dictionary (e.g. `/XObject`, `/Shading`,
    /// `/Pattern`, `/ExtGState`) to its object id (when written indirectly) and the
    /// resolved object. The id is `None` for an inline dictionary.
    fn resource_entry<'a>(
        &'a self,
        resources: &'a Dictionary,
        category: &[u8],
        name: &[u8],
    ) -> Option<(Option<ObjectId>, &'a Object)> {
        let sub = resources
            .get(category)
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        let raw = sub.get(name)?;
        let id = raw.as_reference();
        Some((id, self.resolve(raw)))
    }

    /// Build a [`raster::FormXObject`] for a form stream and child resources, with
    /// the form's object id pushed onto the cycle-guard `seen` set. `None` when the
    /// stream isn't a form or can't be decoded.
    #[allow(clippy::too_many_arguments)]
    fn build_form_xobject<'a>(
        &'a self,
        id: Option<ObjectId>,
        obj: &'a Object,
        parent_resources: &Dictionary,
        seen: &std::collections::BTreeSet<ObjectId>,
        width: u32,
        height: u32,
        base: content::PageMatrix,
    ) -> Option<crate::raster::render::FormXObject<'a>> {
        let stream = obj.as_stream()?;
        if stream.dict.get(b"Subtype").and_then(Object::as_name) != Some(b"Form".as_slice()) {
            return None;
        }
        if let Some(id) = id {
            if seen.contains(&id) {
                return None; // cycle: this form is already being drawn
            }
        }
        let content = decode_stream(stream).ok()?;
        let matrix = self.form_matrix(&stream.dict);
        let bbox = self.read_bbox(&stream.dict);
        // A form's own /Resources, else (per ISO 32000-1) the page's.
        let resources = stream
            .dict
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(|| parent_resources.clone());
        let fonts = self.render_fonts_for(&resources);
        let images = self.images_for(&resources);
        let mut child_seen = seen.clone();
        if let Some(id) = id {
            child_seen.insert(id);
        }
        let ctx = PageResourceCtx {
            doc: self,
            resources,
            seen: child_seen,
            width,
            height,
            base,
        };
        Some(crate::raster::render::FormXObject {
            content,
            matrix,
            bbox,
            fonts,
            images,
            ctx: Box::new(ctx),
        })
    }

    /// Read a shading dictionary (axial type 2 / radial type 3) into a renderer
    /// [`raster::Shading`] with a pre-sampled 256-entry colour ramp. `pattern_matrix`,
    /// when given, is folded into `to_device` (a shading pattern's `/Matrix`);
    /// otherwise `to_device` is identity and the caller sets it (`sh`). Mesh
    /// shadings (types 4–7) return `None`.
    fn read_shading(
        &self,
        dict: &Dictionary,
        pattern_matrix: Option<content::PageMatrix>,
    ) -> Option<crate::raster::render::Shading> {
        use crate::raster::render::{Shading, ShadingKind};
        let stype = dict.get(b"ShadingType").and_then(Object::as_i64)?;
        let coords: Vec<f64> = dict
            .get(b"Coords")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)?
            .iter()
            .filter_map(|o| self.resolve(o).as_f64())
            .collect();
        let kind = match stype {
            2 if coords.len() >= 4 => ShadingKind::Axial {
                x0: coords[0],
                y0: coords[1],
                x1: coords[2],
                y1: coords[3],
            },
            3 if coords.len() >= 6 => ShadingKind::Radial {
                x0: coords[0],
                y0: coords[1],
                r0: coords[2],
                x1: coords[3],
                y1: coords[4],
                r1: coords[5],
            },
            _ => return None, // unsupported / mesh shading
        };
        let extend = dict
            .get(b"Extend")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(|a| {
                let f = |i: usize| matches!(a.get(i).map(|o| self.resolve(o)), Some(Object::Boolean(true)));
                [f(0), f(1)]
            })
            .unwrap_or([false, false]);
        // The shading's [Domain t0 t1] maps the geometric parameter to the function
        // input; default [0 1].
        let domain = dict
            .get(b"Domain")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .and_then(|a| {
                let v: Vec<f64> = a.iter().filter_map(|o| self.resolve(o).as_f64()).collect();
                (v.len() >= 2).then(|| [v[0], v[1]])
            })
            .unwrap_or([0.0, 1.0]);
        let func = dict.get(b"Function").map(|o| self.resolve(o));
        let mut ramp = Vec::with_capacity(256);
        for i in 0..256u32 {
            let frac = i as f64 / 255.0;
            let t = domain[0] + frac * (domain[1] - domain[0]);
            ramp.push(match func {
                Some(f) => self.sample_function_rgb(f, t),
                None => [0, 0, 0],
            });
        }
        Some(Shading {
            kind,
            ramp,
            extend,
            to_device: pattern_matrix.unwrap_or(content::PageMatrix::IDENTITY),
        })
    }

    /// Evaluate a colour `/Function` at parameter `t` (in the function's domain),
    /// returning an RGB triple. Handles a sampled function (type 0), an exponential
    /// interpolation (type 2), a stitching function (type 3), and an array of
    /// single-output functions (one per colour component). DeviceGray (1 output)
    /// and DeviceCMYK (4 outputs) are mapped to RGB; anything else falls back to
    /// the first three outputs as RGB.
    fn sample_function_rgb(&self, func: &Object, t: f64) -> [u8; 3] {
        let outputs = self.eval_function(func, t);
        match outputs.len() {
            0 => [0, 0, 0],
            1 => {
                let g = (outputs[0].clamp(0.0, 1.0) * 255.0).round() as u8;
                [g, g, g]
            }
            4 => {
                let r = (1.0 - outputs[0]) * (1.0 - outputs[3]);
                let g = (1.0 - outputs[1]) * (1.0 - outputs[3]);
                let b = (1.0 - outputs[2]) * (1.0 - outputs[3]);
                [
                    (r.clamp(0.0, 1.0) * 255.0).round() as u8,
                    (g.clamp(0.0, 1.0) * 255.0).round() as u8,
                    (b.clamp(0.0, 1.0) * 255.0).round() as u8,
                ]
            }
            _ => [
                (outputs[0].clamp(0.0, 1.0) * 255.0).round() as u8,
                (outputs.get(1).copied().unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u8,
                (outputs.get(2).copied().unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u8,
            ],
        }
    }

    /// Evaluate a PDF function object at scalar input `t`, returning its output
    /// component vector. Supports an array of functions (each 1-output, results
    /// concatenated), type 0 (sampled), type 2 (exponential), and type 3
    /// (stitching). Unknown types return an empty vector.
    fn eval_function(&self, func: &Object, t: f64) -> Vec<f64> {
        let func = self.resolve(func);
        // An array of functions: evaluate each, concatenate the first output of each.
        if let Object::Array(items) = func {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let v = self.eval_function(item, t);
                out.push(v.first().copied().unwrap_or(0.0));
            }
            return out;
        }
        let Some(dict) = func.as_dict() else {
            return Vec::new();
        };
        let ftype = dict.get(b"FunctionType").and_then(Object::as_i64).unwrap_or(-1);
        let domain = read_pair(self, dict, b"Domain").unwrap_or([0.0, 1.0]);
        let x = t.clamp(domain[0].min(domain[1]), domain[0].max(domain[1]));
        match ftype {
            2 => {
                let c0 = read_vec(self, dict, b"C0").unwrap_or_else(|| vec![0.0]);
                let c1 = read_vec(self, dict, b"C1").unwrap_or_else(|| vec![1.0]);
                let nexp = dict.get(b"N").and_then(|o| self.resolve(o).as_f64()).unwrap_or(1.0);
                let span = domain[1] - domain[0];
                let xn = if span.abs() < 1e-12 {
                    0.0
                } else {
                    ((x - domain[0]) / span).clamp(0.0, 1.0).powf(nexp)
                };
                let len = c0.len().max(c1.len());
                (0..len)
                    .map(|i| {
                        let a = c0.get(i).copied().unwrap_or(0.0);
                        let b = c1.get(i).copied().unwrap_or(0.0);
                        a + xn * (b - a)
                    })
                    .collect()
            }
            3 => {
                let functions = match dict.get(b"Functions").map(|o| self.resolve(o)) {
                    Some(Object::Array(items)) => items.clone(),
                    _ => return Vec::new(),
                };
                let bounds = read_vec(self, dict, b"Bounds").unwrap_or_default();
                let encode = read_vec(self, dict, b"Encode").unwrap_or_default();
                let k = functions.len();
                if k == 0 {
                    return Vec::new();
                }
                // Find the subdomain index for x.
                let mut idx = 0;
                while idx < bounds.len() && x >= bounds[idx] {
                    idx += 1;
                }
                idx = idx.min(k - 1);
                let lo = if idx == 0 { domain[0] } else { bounds[idx - 1] };
                let hi = if idx + 1 < k + 1 && idx < bounds.len() {
                    bounds[idx]
                } else {
                    domain[1]
                };
                let (e0, e1) = (
                    encode.get(2 * idx).copied().unwrap_or(0.0),
                    encode.get(2 * idx + 1).copied().unwrap_or(1.0),
                );
                let span = hi - lo;
                let enc = if span.abs() < 1e-12 {
                    e0
                } else {
                    e0 + (x - lo) / span * (e1 - e0)
                };
                self.eval_function(&functions[idx], enc)
            }
            0 => self.eval_sampled_function(func, x, domain),
            _ => Vec::new(),
        }
    }

    /// Evaluate a type-0 (sampled) function at `x` with linear interpolation along
    /// its single input dimension. Reads `/Size`, `/BitsPerSample`, `/Range`, and
    /// the decoded sample stream; multi-input sampled functions aren't used by
    /// shadings, so only the 1-D case is handled (others return empty).
    fn eval_sampled_function(&self, func: &Object, x: f64, domain: [f64; 2]) -> Vec<f64> {
        let Object::Stream(stream) = func else {
            return Vec::new();
        };
        let dict = &stream.dict;
        let size: Vec<i64> = dict
            .get(b"Size")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(|a| a.iter().filter_map(|o| self.resolve(o).as_i64()).collect())
            .unwrap_or_default();
        if size.len() != 1 || size[0] < 1 {
            return Vec::new();
        }
        let n = size[0] as usize;
        let bps = dict.get(b"BitsPerSample").and_then(Object::as_i64).unwrap_or(8);
        let range = read_vec(self, dict, b"Range").unwrap_or_default();
        if range.is_empty() || !range.len().is_multiple_of(2) {
            return Vec::new();
        }
        let outs = range.len() / 2;
        let data = match decode_stream(stream) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        // Map x → sample index (continuous), then linearly interpolate.
        let span = domain[1] - domain[0];
        let e = if span.abs() < 1e-12 {
            0.0
        } else {
            ((x - domain[0]) / span).clamp(0.0, 1.0) * (n - 1) as f64
        };
        let i0 = e.floor() as usize;
        let i1 = (i0 + 1).min(n - 1);
        let frac = e - i0 as f64;
        let max_val = ((1u64 << bps.min(32)) - 1) as f64;
        let mut out = Vec::with_capacity(outs);
        for c in 0..outs {
            let s0 = read_sample(&data, (i0 * outs + c) as u64, bps as u32) as f64 / max_val;
            let s1 = read_sample(&data, (i1 * outs + c) as u64, bps as u32) as f64 / max_val;
            let v = s0 + frac * (s1 - s0);
            let (r0, r1) = (range[2 * c], range[2 * c + 1]);
            out.push(r0 + v * (r1 - r0));
        }
        out
    }

    /// Resolve an ExtGState (`/ExtGState` resource) named `name`: its `/ca`,
    /// blend mode `/BM`, and `/SMask` rendered to a device-resolution alpha buffer
    /// over the page area (`width × height` at the page `base`). `None` when the
    /// ExtGState can't be found.
    fn ext_gstate_params(
        &self,
        resources: &Dictionary,
        name: &[u8],
        width: u32,
        height: u32,
        base: content::PageMatrix,
        seen: &std::collections::BTreeSet<ObjectId>,
    ) -> Option<crate::raster::render::ExtGStateParams> {
        let (_, obj) = self.resource_entry(resources, b"ExtGState", name)?;
        let gs = obj.as_dict()?;
        let fill_alpha = gs.get(b"ca").map(|o| self.resolve(o)).and_then(|o| o.as_f64());
        let blend = gs
            .get(b"BM")
            .map(|o| self.resolve(o))
            .and_then(|o| match o {
                Object::Name(n) => Some(crate::raster::canvas::BlendMode::from_name(n)),
                // /BM may be an array of names; use the first supported one.
                Object::Array(items) => items
                    .first()
                    .and_then(Object::as_name)
                    .map(crate::raster::canvas::BlendMode::from_name),
                _ => None,
            })
            .unwrap_or_default();
        let soft_mask = match gs.get(b"SMask").map(|o| self.resolve(o)) {
            Some(Object::Dictionary(sm)) => {
                self.render_soft_mask(sm, resources, width, height, base, seen)
            }
            _ => None, // `/None` or absent
        };
        Some(crate::raster::render::ExtGStateParams {
            fill_alpha,
            blend,
            soft_mask,
        })
    }

    /// Render an ExtGState `/SMask` luminosity/alpha group to a device-resolution
    /// alpha mask (`raster::ClipMask`). The mask's transparency group form (`/G`)
    /// is rasterized on a separate canvas; for a `/Luminosity` mask the alpha is
    /// the group's luminance, for an `/Alpha` mask we approximate with luminance
    /// too (our canvas is opaque). `None` if the group can't be drawn.
    fn render_soft_mask(
        &self,
        smask: &Dictionary,
        parent_resources: &Dictionary,
        width: u32,
        height: u32,
        base: content::PageMatrix,
        seen: &std::collections::BTreeSet<ObjectId>,
    ) -> Option<crate::raster::ClipMask> {
        let group_raw = smask.get(b"G")?;
        let group_id = group_raw.as_reference();
        let group = self.resolve(group_raw).as_stream()?.clone();
        if group.dict.get(b"Subtype").and_then(Object::as_name) != Some(b"Form".as_slice()) {
            return None;
        }
        let content = decode_stream(&group).ok()?;
        let matrix = self.form_matrix(&group.dict);
        let resources = group
            .dict
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(|| parent_resources.clone());
        let fonts = self.render_fonts_for(&resources);
        let images = self.images_for(&resources);
        let mut child_seen = seen.clone();
        if let Some(id) = group_id {
            child_seen.insert(id);
        }
        let ctx = PageResourceCtx {
            doc: self,
            resources,
            seen: child_seen,
            width,
            height,
            base,
        };
        // A luminosity mask is composited over black (areas the group doesn't
        // paint are fully masked out → alpha 0). Start the mask canvas black.
        let mut mask_canvas = crate::raster::Canvas::new(width, height);
        for px in mask_canvas.pixels.chunks_exact_mut(4) {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        }
        let group_base = matrix.then(&base);
        crate::raster::render_content_into_ctx(
            &mut mask_canvas,
            &content,
            group_base,
            &fonts,
            &images,
            1.0,
            &ctx,
            1,
            None,
        );
        // Luminance → alpha (Rec. 601 weights), per pixel.
        let mut cover = vec![0.0f32; (width as usize) * (height as usize)];
        for (i, px) in mask_canvas.pixels.chunks_exact(4).enumerate() {
            let lum = 0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32;
            cover[i] = (lum / 255.0).clamp(0.0, 1.0);
        }
        Some(crate::raster::ClipMask {
            width,
            height,
            cover,
        })
    }

    /// Read a form XObject's `/BBox` `[x0 y0 x1 y1]`; defaults to a unit box.
    fn read_bbox(&self, dict: &Dictionary) -> [f64; 4] {
        let mut bbox = [0.0, 0.0, 1.0, 1.0];
        if let Some(items) = dict
            .get(b"BBox")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for (i, value) in items.iter().take(4).enumerate() {
                bbox[i] = self.resolve(value).as_f64().unwrap_or(bbox[i]);
            }
        }
        bbox
    }

    /// Normalise a `/Rect` so `x0 <= x1` and `y0 <= y1` (PDF allows either order).
    fn normalized_rect(&self, r: [f64; 4]) -> [f64; 4] {
        [
            r[0].min(r[2]),
            r[1].min(r[3]),
            r[0].max(r[2]),
            r[1].max(r[3]),
        ]
    }

    /// OCR a page with the built-in zero-dependency recognizer. The page is
    /// rasterized at `scale` (≥ 2.0 recommended for small text), binarized, and
    /// recognized; returns the text plus word boxes in **PDF user space** so the
    /// host can highlight or overlay. Works on scanned (image-only) pages — for
    /// pages that already carry a text layer, prefer [`structured_text`](Self::structured_text).
    pub fn ocr_page(&self, page_no: u32, scale: f64) -> Vec<crate::raster::ocr::OcrWord> {
        let Ok(canvas) = self.render_page_canvas(page_no, scale) else {
            return Vec::new();
        };
        let (w, h) = (canvas.width as usize, canvas.height as usize);
        let gray: Vec<u8> = canvas
            .pixels
            .chunks_exact(4)
            .map(|p| ((p[0] as u32 + p[1] as u32 + p[2] as u32) / 3) as u8)
            .collect();
        let result = crate::raster::ocr::ocr(&gray, w, h);

        // Map image pixels (top-left origin) back to PDF user space (bottom-left).
        let media = self
            .page_dict(page_no)
            .map(|p| self.read_media_box(p))
            .unwrap_or([0.0, 0.0, 612.0, 792.0]);
        let (x0, y0) = (media[0], media[1]);
        let page_h = (media[3] - media[1]).abs();
        let s = scale.max(0.01);
        result
            .words
            .into_iter()
            .map(|word| crate::raster::ocr::OcrWord {
                text: word.text,
                x: x0 + word.x / s,
                y: y0 + page_h - (word.y + word.height) / s,
                width: word.width / s,
                height: word.height / s,
            })
            .collect()
    }

    /// OCR a page and return only the recognized text (newline-separated lines).
    pub fn ocr_page_text(&self, page_no: u32, scale: f64) -> String {
        let Ok(canvas) = self.render_page_canvas(page_no, scale) else {
            return String::new();
        };
        let (w, h) = (canvas.width as usize, canvas.height as usize);
        let gray: Vec<u8> = canvas
            .pixels
            .chunks_exact(4)
            .map(|p| ((p[0] as u32 + p[1] as u32 + p[2] as u32) / 3) as u8)
            .collect();
        crate::raster::ocr::ocr(&gray, w, h).text
    }

    /// Extract every page's editable content (positioned text, re-embedded
    /// images, shape rectangles) into the conversion model, normalizing PDF
    /// bottom-up user space to top-down points. This is the shared front-end for
    /// all the Office exporters — they reconstruct real objects from this, never
    /// a page raster.
    fn convert_pages(&self) -> Vec<crate::convert::ConvPage> {
        use crate::content::vector::PathSeg;
        use crate::content::ElementKind;
        use crate::convert::{ConvPage, PlacedImage, PlacedShape, PlacedText};

        let mut pages = Vec::new();
        for page_no in 1..=self.page_count() as u32 {
            let Ok(page) = self.page_dict(page_no) else {
                continue;
            };
            let media = self.read_media_box(page);
            let (x0, y0) = (media[0], media[1]);
            let page_w = (media[2] - media[0]).abs();
            let page_h = (media[3] - media[1]).abs();

            let elements = self.page_elements(page_no).unwrap_or_default();
            // Vector paths carry the geometry + paint state (fill/stroke RGB,
            // alpha, width, dash) the `ElementKind::Path` boxes drop. They come
            // from the same top-level content stream as `elements`, but exclude
            // clip-only / no-paint (`n`) paths — which the element walker keeps —
            // so they cannot be zipped positionally against the Path elements.
            // Shapes are therefore sourced solely from here (a single coherent
            // source: correct colours, no stray grey rectangles for clip paths).
            let vpaths = self.page_vector_paths(page_no).unwrap_or_default();
            let images = self.page_images(page_no);
            let font_styles = self.page_base_fonts(page_no);
            // Encode each referenced image once per page (a single XObject may be
            // drawn several times) so repeated placements share the PNG bytes.
            let mut png_cache: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

            let mut conv = ConvPage {
                width: page_w,
                height: page_h,
                ..ConvPage::default()
            };
            for element in elements {
                let Some(b) = element.bounds else { continue };
                let left = b.x - x0;
                let top = page_h - (b.y - y0) - b.height;
                match element.kind {
                    ElementKind::Text => {
                        if element.label.trim().is_empty() {
                            continue;
                        }
                        // Recover the run's style from its font resource, then
                        // overlay the fill colour the interpreter captured.
                        let mut style = element
                            .font
                            .as_deref()
                            .and_then(|f| font_styles.get(f))
                            .cloned()
                            .unwrap_or_default();
                        style.color = element.color;
                        conv.texts.push(PlacedText {
                            text: element.label,
                            x: left,
                            y: top,
                            width: b.width,
                            height: b.height,
                            style,
                        });
                    }
                    ElementKind::Image => {
                        let key = element.label.into_bytes();
                        if let Some(image) = images.get(&key) {
                            let png = png_cache
                                .entry(key)
                                .or_insert_with(|| {
                                    crate::raster::png::encode_png(
                                        image.width,
                                        image.height,
                                        &image.rgba,
                                    )
                                })
                                .clone();
                            conv.images.push(PlacedImage {
                                png,
                                x: left,
                                y: top,
                                width: b.width,
                                height: b.height,
                            });
                        }
                    }
                    // Shapes are emitted from `vpaths` below (geometry + paint
                    // state), not from these bounding-box-only Path elements.
                    ElementKind::Path => {}
                }
            }

            // Emit one enriched shape per painted vector path. Each path's points
            // are in PDF user space (origin bottom-left); flip to top-down points
            // matching the rest of the model: `x' = x - x0`, `y' = page_h - (y - y0)`.
            // The bounding rectangle is flipped the same way the element loop does
            // (anchor at the box's top-left), so the rect fallback stays exact.
            for vp in &vpaths {
                let segments: Vec<PathSeg> = vp
                    .segments
                    .iter()
                    .map(|seg| match *seg {
                        PathSeg::Move(x, y) => PathSeg::Move(x - x0, page_h - (y - y0)),
                        PathSeg::Line(x, y) => PathSeg::Line(x - x0, page_h - (y - y0)),
                        PathSeg::Cubic(x1, y1, x2, y2, x3, y3) => PathSeg::Cubic(
                            x1 - x0,
                            page_h - (y1 - y0),
                            x2 - x0,
                            page_h - (y2 - y0),
                            x3 - x0,
                            page_h - (y3 - y0),
                        ),
                        PathSeg::Close => PathSeg::Close,
                    })
                    .collect();
                let (x, y, width, height) = match vp.bounds {
                    Some(b) => (b.x - x0, page_h - (b.y - y0) - b.height, b.width, b.height),
                    None => continue,
                };
                conv.shapes.push(PlacedShape {
                    x,
                    y,
                    width,
                    height,
                    segments,
                    fill: vp.fill,
                    stroke: vp.stroke,
                    stroke_width: vp.stroke_width,
                    fill_alpha: vp.fill_alpha,
                    stroke_alpha: vp.stroke_alpha,
                    dash: vp.dash.clone(),
                });
            }
            pages.push(conv);
        }
        pages
    }

    /// Convert the document to an editable OpenDocument Text (`.odt`): every text
    /// run becomes a positioned text box, every image a placed picture — real,
    /// editable content rather than a page image.
    pub fn to_odt(&self) -> Vec<u8> {
        crate::convert::office::to_odt(&self.convert_pages())
    }

    /// Convert the document to an editable Word document (`.docx`): positioned
    /// text boxes + anchored pictures + shape rectangles, one section per page.
    pub fn to_docx(&self) -> Vec<u8> {
        crate::convert::office::to_docx(&self.convert_pages())
    }

    /// Convert the document to an editable PowerPoint presentation (`.pptx`):
    /// one slide per page, each text run a positioned box, each image a picture.
    pub fn to_pptx(&self) -> Vec<u8> {
        crate::convert::office::to_pptx(&self.convert_pages())
    }

    /// Convert the document to an editable OpenDocument Presentation (`.odp`):
    /// one slide per page, each text run a positioned box, each image a picture.
    pub fn to_odp(&self) -> Vec<u8> {
        crate::convert::office::to_odp(&self.convert_pages())
    }

    /// Reconstruct each page's text into a row/column grid and export an Excel
    /// workbook (`.xlsx`), one sheet per page. Tabular PDFs become real cells;
    /// prose collapses to a single column so all document text is preserved.
    pub fn to_xlsx(&self) -> Vec<u8> {
        let (grids, shapes) = self.convert_grids_and_shapes();
        crate::convert::office::to_xlsx_with_shapes(&grids, &[], &shapes)
    }

    /// As [`to_xlsx`](Self::to_xlsx) but to an OpenDocument Spreadsheet (`.ods`).
    pub fn to_ods(&self) -> Vec<u8> {
        let (grids, shapes) = self.convert_grids_and_shapes();
        crate::convert::office::to_ods_with_shapes(&grids, &[], &shapes)
    }

    /// Convert the document's text to an RTF document (one paragraph per text
    /// line). Pairs with [`reverse::rtf_to_pdf`](crate::convert::reverse::rtf_to_pdf).
    pub fn to_rtf(&self) -> Vec<u8> {
        let text = self.to_text();
        let paragraphs: Vec<String> = text
            .split(['\n', '\u{000C}'])
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        crate::convert::reverse::to_rtf(&paragraphs)
    }

    /// Re-serialize the document with PDF/A-2b archival metadata: an XMP
    /// identification packet, an sRGB `OutputIntent` (embedded ICC profile) and a
    /// trailer `/ID`. **Note:** full PDF/A conformance also requires every font to
    /// be embedded; embed non-embedded fonts via [`needed_fonts`](Self::needed_fonts)
    /// and [`embed_truetype_font`](Self::embed_truetype_font) first for a
    /// validator-clean result.
    pub fn to_pdfa(&self) -> Vec<u8> {
        use crate::object::StringKind::{Hex, Literal};
        let Ok(catalog_id) = self.catalog_id() else {
            return self.save();
        };
        let mut objects = self.objects.clone();
        let mut trailer = self.trailer.clone();

        let meta_id = (self.next_object_number(), 0u16);
        let icc_id = (meta_id.0 + 1, 0u16);

        // XMP metadata stream (must stay uncompressed for PDF/A).
        let xmp = crate::convert::pdfa::xmp_metadata("GigaPDF Document", "GigaPDF Engine");
        let mut mdict = Dictionary::new();
        mdict.set(b"Type", annot::name(b"Metadata"));
        mdict.set(b"Subtype", annot::name(b"XML"));
        mdict.set(b"Length", Object::Integer(xmp.len() as i64));
        objects.insert(meta_id, Object::Stream(Stream::new(mdict, xmp)));

        // sRGB ICC profile stream.
        let icc = crate::convert::srgb_icc::SRGB_ICC;
        let mut idict = Dictionary::new();
        idict.set(b"N", Object::Integer(3));
        idict.set(b"Length", Object::Integer(icc.len() as i64));
        objects.insert(icc_id, Object::Stream(Stream::new(idict, icc.to_vec())));

        // OutputIntent referencing the profile.
        let mut oi = Dictionary::new();
        oi.set(b"Type", annot::name(b"OutputIntent"));
        oi.set(b"S", annot::name(b"GTS_PDFA1"));
        oi.set(
            b"OutputConditionIdentifier",
            Object::String(b"sRGB IEC61966-2.1".to_vec(), Literal),
        );
        oi.set(
            b"Info",
            Object::String(b"sRGB IEC61966-2.1".to_vec(), Literal),
        );
        oi.set(b"DestOutputProfile", Object::Reference(icc_id));

        // Attach Metadata + OutputIntents to the catalog.
        let mut catalog = objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        catalog.set(b"Metadata", Object::Reference(meta_id));
        catalog.set(
            b"OutputIntents",
            Object::Array(vec![Object::Dictionary(oi)]),
        );
        objects.insert(catalog_id, Object::Dictionary(catalog));

        // PDF/A requires a trailer /ID. Derive one deterministically.
        if !trailer.contains(b"ID") {
            let seed = format!("gigapdf:{}", objects.len());
            let digest = crate::crypto::md5::md5(seed.as_bytes()).to_vec();
            let id = Object::String(digest, Hex);
            trailer.set(b"ID", Object::Array(vec![id.clone(), id]));
        }

        crate::serialize::to_pdf(&objects, &trailer)
    }

    /// Per-page reconstructed table grids and floating shapes (shared by the
    /// spreadsheet exporters), from a single [`convert_pages`](Self::convert_pages)
    /// pass: grids drive the cells, shapes the drawing layer.
    #[allow(clippy::type_complexity)]
    fn convert_grids_and_shapes(
        &self,
    ) -> (Vec<Vec<Vec<String>>>, Vec<Vec<crate::convert::PlacedShape>>) {
        let pages = self.convert_pages();
        let grids = pages
            .iter()
            .map(|page| crate::convert::table::reconstruct(&page.texts))
            .collect();
        let shapes = pages.into_iter().map(|page| page.shapes).collect();
        (grids, shapes)
    }

    /// Decode the page's image XObjects (`DeviceRGB`/`DeviceGray`, 8 bpc, Flate
    /// or raw — JPEG/JPX are skipped) into RGBA buffers for the rasterizer.
    fn page_images(&self, page_no: u32) -> crate::raster::render::RenderImages {
        match self.page_dict(page_no).ok().and_then(|page| {
            page.get(b"Resources")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
                .cloned()
        }) {
            Some(res) => self.images_for(&res),
            None => crate::raster::render::RenderImages::new(),
        }
    }

    /// Build the decodable image XObjects of a `/Resources` dictionary's
    /// `/XObject` sub-dictionary, keyed by resource name. Works for a page's
    /// resources *and* an annotation appearance's own `/Resources`, so an
    /// appearance that draws images rasterizes them too.
    fn images_for(&self, resources: &Dictionary) -> crate::raster::render::RenderImages {
        let mut out = crate::raster::render::RenderImages::new();
        let xobjects = resources
            .get(b"XObject")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let Some(xobjects) = xobjects else {
            return out;
        };
        for (name, value) in &xobjects.0 {
            let Some(stream) = self.resolve(value).as_stream() else {
                continue;
            };
            let dict = &stream.dict;
            if dict.get(b"Subtype").and_then(Object::as_name) != Some(b"Image".as_slice()) {
                continue;
            }
            let width = dict.get(b"Width").and_then(Object::as_i64).unwrap_or(0);
            let height = dict.get(b"Height").and_then(Object::as_i64).unwrap_or(0);
            let bpc = dict
                .get(b"BitsPerComponent")
                .and_then(Object::as_i64)
                .unwrap_or(8);
            if width <= 0 || height <= 0 || bpc != 8 {
                continue;
            }
            // Skip compressed-photo filters we don't decode yet.
            let filter = self.first_filter(dict);
            if matches!(filter.as_deref(), Some(b"DCTDecode") | Some(b"JPXDecode")) {
                continue;
            }
            let components = match dict
                .get(b"ColorSpace")
                .map(|o| self.resolve(o))
                .and_then(Object::as_name)
            {
                Some(b"DeviceRGB") => 3,
                Some(b"DeviceGray") => 1,
                _ => continue, // Indexed/ICCBased/CMYK not handled yet
            };
            let Ok(samples) = decode_stream(stream) else {
                continue;
            };
            let (w, h) = (width as usize, height as usize);
            if samples.len() < w * h * components {
                continue;
            }
            // A `/SMask` (8-bit DeviceGray image) supplies per-pixel alpha — this
            // is how PNG transparency survives embedding. Sampled nearest-
            // neighbour so a soft mask of a different size still maps (identity
            // when the dimensions match, the common case).
            let smask = self.decode_gray_smask(dict);
            let mut rgba = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) * components;
                    let (r, g, b) = if components == 1 {
                        (samples[i], samples[i], samples[i])
                    } else {
                        (samples[i], samples[i + 1], samples[i + 2])
                    };
                    let a = match &smask {
                        Some((sw, sh, alpha)) => {
                            let sx = if *sw == w { x } else { x * *sw / w };
                            let sy = if *sh == h { y } else { y * *sh / h };
                            alpha.get(sy * *sw + sx).copied().unwrap_or(255)
                        }
                        None => 255,
                    };
                    rgba.extend_from_slice(&[r, g, b, a]);
                }
            }
            out.insert(
                name.clone(),
                crate::raster::render::RenderImage {
                    width: width as u32,
                    height: height as u32,
                    rgba,
                },
            );
        }
        out
    }

    /// The first filter name of a stream dict (`/Filter` may be a name or array).
    fn first_filter(&self, dict: &Dictionary) -> Option<Vec<u8>> {
        match dict.get(b"Filter").map(|o| self.resolve(o)) {
            Some(Object::Name(n)) => Some(n.clone()),
            Some(Object::Array(items)) => items
                .first()
                .map(|o| self.resolve(o))
                .and_then(Object::as_name)
                .map(<[u8]>::to_vec),
            _ => None,
        }
    }

    /// Decode an image's `/SMask` (an 8-bit `/DeviceGray` image XObject) into its
    /// `(width, height, gray samples)` so the rasterizer can use it as per-pixel
    /// alpha. Returns `None` when absent or in a form we don't decode (e.g. a
    /// JPEG-coded mask), in which case the image is treated as opaque.
    fn decode_gray_smask(&self, dict: &Dictionary) -> Option<(usize, usize, Vec<u8>)> {
        let stream = dict.get(b"SMask").map(|o| self.resolve(o))?.as_stream()?;
        let sd = &stream.dict;
        let sw = sd.get(b"Width").and_then(Object::as_i64).unwrap_or(0);
        let sh = sd.get(b"Height").and_then(Object::as_i64).unwrap_or(0);
        let bpc = sd
            .get(b"BitsPerComponent")
            .and_then(Object::as_i64)
            .unwrap_or(8);
        if sw <= 0 || sh <= 0 || bpc != 8 {
            return None;
        }
        if matches!(
            self.first_filter(sd).as_deref(),
            Some(b"DCTDecode") | Some(b"JPXDecode")
        ) {
            return None;
        }
        let samples = decode_stream(stream).ok()?;
        let (sw, sh) = (sw as usize, sh as usize);
        if samples.len() < sw * sh {
            return None;
        }
        Some((sw, sh, samples))
    }

    /// Serialize the document, Flate-compressing every uncompressed stream.
    /// Already-filtered streams are left as-is (never double-compressed); a
    /// stream is only replaced when compression actually shrinks it.
    pub fn save_compressed(&self) -> Vec<u8> {
        let mut objects = self.objects.clone();
        self.subset_embedded_fonts(&mut objects);
        for object in objects.values_mut() {
            if let Object::Stream(stream) = object {
                if stream.dict.contains(b"Filter") || stream.raw.len() <= 64 {
                    continue;
                }
                let compressed = crate::filters::deflate::flate_encode(&stream.raw);
                if compressed.len() < stream.raw.len() {
                    stream
                        .dict
                        .set(b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec()));
                    stream
                        .dict
                        .set(b"Length".to_vec(), Object::Integer(compressed.len() as i64));
                    stream.raw = compressed;
                }
            }
        }
        crate::serialize::to_pdf(&objects, &self.trailer)
    }

    /// Reading-order text lines of a page (structured text): each line's text
    /// plus its union bounding box. Replaces an external structured-text engine.
    pub fn structured_text(&self, page_no: u32) -> Vec<content::TextLine> {
        content::group_lines(&self.page_elements(page_no).unwrap_or_default())
    }

    /// Full-text search across the document. Returns one [`SearchMatch`] per line
    /// containing `query` (substring; `case_insensitive` folds ASCII case), with
    /// the line text and its bounding box for highlighting.
    pub fn search(&self, query: &str, case_insensitive: bool) -> Vec<SearchMatch> {
        let needle = if case_insensitive {
            query.to_lowercase()
        } else {
            query.to_string()
        };
        let mut matches = Vec::new();
        if needle.is_empty() {
            return matches;
        }
        for page in 1..=self.page_count() as u32 {
            for line in self.structured_text(page) {
                let hay = if case_insensitive {
                    line.text.to_lowercase()
                } else {
                    line.text.clone()
                };
                if hay.contains(&needle) {
                    matches.push(SearchMatch {
                        page,
                        text: line.text,
                        bounds: line.bounds,
                    });
                }
            }
        }
        matches
    }

    /// Extract the document's text, one run per line, pages separated by a form
    /// feed (`\x0C`). Font-aware (zero tofu).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for page in 1..=self.page_count() as u32 {
            if let Ok(runs) = self.page_text_runs(page) {
                for run in runs {
                    out.push_str(&run.text);
                    out.push('\n');
                }
            }
            out.push('\u{000C}');
        }
        out
    }

    /// Convert the document to standalone HTML with absolutely-positioned,
    /// styled text (font/weight/colour) and inlined images — real selectable
    /// content, not a page raster. A reflow-level conversion (layout, not
    /// pixel-perfect rendering).
    pub fn to_html(&self) -> String {
        crate::convert::web::to_html(&self.convert_pages())
    }

    /// Build per-font render data (embedded TrueType program + decoder) from a
    /// page's `/Resources /Font`, for the rasterizer's glyph rendering.
    fn page_render_fonts(&self, page_no: u32) -> crate::raster::render::RenderFonts {
        match self.page_dict(page_no).ok().and_then(|page| {
            page.get(b"Resources")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
                .cloned()
        }) {
            Some(res) => self.render_fonts_for(&res),
            None => crate::raster::render::RenderFonts::new(),
        }
    }

    /// Build the glyph-bearing render fonts of a `/Resources` dictionary's
    /// `/Font` sub-dictionary, keyed by resource name. Reused for a page's
    /// resources *and* an annotation appearance's own `/Resources` so an
    /// appearance's text (e.g. a FreeText box) rasterizes with the right faces.
    fn render_fonts_for(&self, resources: &Dictionary) -> crate::raster::render::RenderFonts {
        let mut out = crate::raster::render::RenderFonts::new();
        let font_dict = resources
            .get(b"Font")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let Some(font_dict) = font_dict else {
            return out;
        };
        for (name, value) in &font_dict.0 {
            let Some(font) = self.resolve(value).as_dict() else {
                continue;
            };
            let two_byte =
                font.get(b"Subtype").and_then(Object::as_name) == Some(b"Type0".as_slice());
            let to_unicode = font
                .get(b"ToUnicode")
                .map(|o| self.resolve(o))
                .and_then(Object::as_stream)
                .and_then(|s| decode_stream(s).ok())
                .map(|bytes| crate::font::cmap::ToUnicode::parse(&bytes))
                .filter(|c| !c.is_empty());
            out.insert(
                name.clone(),
                crate::raster::render::RenderFont {
                    program: self.font_program(font),
                    decoder: crate::font::cmap::TextDecoder {
                        two_byte,
                        to_unicode,
                        // Rendering advances glyphs by the font program's own
                        // metrics, so the PDF width table isn't needed here.
                        widths: None,
                        // Rasterising uses the glyph id directly; no cmap-derived
                        // Unicode fallback is required for drawing.
                        cid_to_unicode: None,
                    },
                    two_byte,
                },
            );
        }
        out
    }

    /// Extract and parse the embedded glyph program of a font, descending into
    /// the CIDFont for a Type0 font. `/FontFile2` is TrueType; `/FontFile3` is
    /// CFF/OpenType (tried as both). Type1 (`/FontFile`) is not yet rasterized.
    fn font_program(&self, font: &Dictionary) -> Option<crate::font::GlyphSource> {
        let carrier = if font.get(b"Subtype").and_then(Object::as_name) == Some(b"Type0".as_slice())
        {
            font.get(b"DescendantFonts")
                .map(|o| self.resolve(o))
                .and_then(Object::as_array)
                .and_then(|a| a.first())
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)?
        } else {
            font
        };
        let descriptor = carrier
            .get(b"FontDescriptor")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;

        if let Some(bytes) = self.font_file_bytes(descriptor, b"FontFile2") {
            if let Some(ttf) = crate::font::truetype::TrueTypeFont::parse(&bytes) {
                return Some(crate::font::GlyphSource::TrueType(ttf));
            }
        }
        if let Some(bytes) = self.font_file_bytes(descriptor, b"FontFile3") {
            if let Some(cff) = crate::font::cff::CffFont::parse(&bytes) {
                return Some(crate::font::GlyphSource::Cff(cff));
            }
            if let Some(ttf) = crate::font::truetype::TrueTypeFont::parse(&bytes) {
                return Some(crate::font::GlyphSource::TrueType(ttf));
            }
        }
        None
    }

    fn font_file_bytes(&self, descriptor: &Dictionary, key: &[u8]) -> Option<Vec<u8>> {
        let stream = descriptor
            .get(key)
            .map(|o| self.resolve(o))
            .and_then(Object::as_stream)?;
        decode_stream(stream).ok()
    }

    /// Enumerate the fonts **embedded** in the document — every `/Font` whose
    /// descriptor carries a font program (`/FontFile2` TrueType, `/FontFile3`
    /// CFF/OpenType, `/FontFile` Type1), with its `/BaseFont` name and format,
    /// deduplicated and sorted. Pair with
    /// [`extract_font_program`](Self::extract_font_program) to pull a font's
    /// bytes out and re-embed it (drawing new text in the document's own face).
    pub fn embedded_fonts(&self) -> Vec<EmbeddedFontInfo> {
        let mut out: Vec<EmbeddedFontInfo> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for object in self.objects.values() {
            let Some(dict) = object.as_dict() else {
                continue;
            };
            if dict.get(b"Type").and_then(Object::as_name) != Some(b"Font".as_slice()) {
                continue;
            }
            let base = dict
                .get(b"BaseFont")
                .and_then(Object::as_name)
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            if base.is_empty() {
                continue;
            }
            // Type0 composites carry the descriptor on the descendant CIDFont.
            let carrier = if dict.get(b"Subtype").and_then(Object::as_name)
                == Some(b"Type0".as_slice())
            {
                dict.get(b"DescendantFonts")
                    .map(|o| self.resolve(o))
                    .and_then(Object::as_array)
                    .and_then(|a| a.first())
                    .map(|o| self.resolve(o))
                    .and_then(Object::as_dict)
                    .cloned()
            } else {
                Some(dict.clone())
            };
            let Some(carrier) = carrier else {
                continue;
            };
            let Some(descriptor) = carrier
                .get(b"FontDescriptor")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
            else {
                continue;
            };
            let format = if descriptor.contains(b"FontFile2") {
                "truetype"
            } else if descriptor.contains(b"FontFile3") {
                "cff"
            } else if descriptor.contains(b"FontFile") {
                "type1"
            } else {
                continue;
            };
            if seen.insert(base.clone()) {
                out.push(EmbeddedFontInfo {
                    base_font: base,
                    format: format.to_string(),
                });
            }
        }
        out.sort_by(|a, b| a.base_font.cmp(&b.base_font));
        out
    }

    /// Find an embedded font program by (fuzzy) `/BaseFont` name and return its
    /// raw **decoded** bytes plus a format tag (`"truetype"`, `"cff"`, `"type1"`).
    /// Mirrors a host editor's "re-embed the original font when re-baking edited
    /// text" path, so the edit keeps the document's own glyphs. Handles Type0
    /// composites (via `/DescendantFonts`). `None` when nothing matches or the
    /// match carries no embedded program (only a `/FontDescriptor` reference).
    pub fn extract_font_program(&self, name: &str) -> Option<(Vec<u8>, &'static str)> {
        let target = normalize_font_name(name);
        if target.is_empty() {
            return None;
        }
        // Collect matching font dicts first (cloned) so we don't keep
        // `self.objects` borrowed while resolving descriptors via `self.resolve`.
        let mut candidates: Vec<Dictionary> = Vec::new();
        for object in self.objects.values() {
            let Some(dict) = object.as_dict() else {
                continue;
            };
            if dict.get(b"Type").and_then(Object::as_name) != Some(b"Font".as_slice()) {
                continue;
            }
            let base = dict
                .get(b"BaseFont")
                .and_then(Object::as_name)
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            let candidate = normalize_font_name(&base);
            if candidate.is_empty() {
                continue;
            }
            // Two-direction substring match absorbs subset prefixes and the
            // "-Regular"/"MT"/"PS" suffix variants without explicit stripping.
            if candidate == target || candidate.contains(&target) || target.contains(&candidate) {
                candidates.push(dict.clone());
            }
        }

        for dict in candidates {
            // Type0 composites carry the descriptor on the descendant CIDFont.
            let carrier =
                if dict.get(b"Subtype").and_then(Object::as_name) == Some(b"Type0".as_slice()) {
                    match dict
                        .get(b"DescendantFonts")
                        .map(|o| self.resolve(o))
                        .and_then(Object::as_array)
                        .and_then(|a| a.first())
                        .map(|o| self.resolve(o))
                        .and_then(Object::as_dict)
                    {
                        Some(descendant) => descendant.clone(),
                        None => continue,
                    }
                } else {
                    dict.clone()
                };
            let Some(descriptor) = carrier
                .get(b"FontDescriptor")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
            else {
                continue;
            };
            // FontFile2 (TrueType) is the most embeddable; then FontFile3 (CFF),
            // then the legacy Type1 FontFile.
            if let Some(bytes) = self.font_file_bytes(descriptor, b"FontFile2") {
                if !bytes.is_empty() {
                    return Some((bytes, "truetype"));
                }
            }
            if let Some(bytes) = self.font_file_bytes(descriptor, b"FontFile3") {
                if !bytes.is_empty() {
                    return Some((bytes, "cff"));
                }
            }
            if let Some(bytes) = self.font_file_bytes(descriptor, b"FontFile") {
                if !bytes.is_empty() {
                    return Some((bytes, "type1"));
                }
            }
        }
        None
    }

    /// Index of the element at page point `(x, y)` (user space), preferring the
    /// smallest box when several overlap. `None` if nothing is hit.
    pub fn element_at(&self, page_no: u32, x: f64, y: f64) -> Result<Option<usize>> {
        let elements = self.page_elements(page_no)?;
        let mut best: Option<(usize, f64)> = None;
        for element in &elements {
            if let Some(bounds) = element.bounds {
                if bounds.contains(x, y) {
                    let area = bounds.area();
                    if best.is_none_or(|(_, best_area)| area < best_area) {
                        best = Some((element.index, area));
                    }
                }
            }
        }
        Ok(best.map(|(index, _)| index))
    }

    /// Remove an element (text, image, or whole shape) by its index from
    /// [`page_elements`], preserving everything else.
    pub fn remove_element(&mut self, page_no: u32, index: usize) -> Result<()> {
        let content = self.page_content(page_no)?;
        let edited = content::remove_element(&content, index)?;
        self.set_page_content(page_no, edited)
    }

    /// Duplicate an element (text, image, or shape) in place.
    pub fn duplicate_element(&mut self, page_no: u32, index: usize) -> Result<()> {
        let content = self.page_content(page_no)?;
        let edited = content::duplicate_element(&content, index)?;
        self.set_page_content(page_no, edited)
    }

    /// Move an element (text, image, or shape) by `(dx, dy)` user-space units.
    pub fn move_element(&mut self, page_no: u32, index: usize, dx: f64, dy: f64) -> Result<()> {
        let content = self.page_content(page_no)?;
        let edited = content::move_element(&content, index, dx, dy)?;
        self.set_page_content(page_no, edited)
    }

    /// Draw a rectangle (frame / table cell / filled box) on a page. Colours are
    /// RGB in `0.0..=1.0`; pass `None` to skip stroke or fill.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn add_rectangle(
        &mut self,
        page_no: u32,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        stroke: Option<[f64; 3]>,
        fill: Option<[f64; 3]>,
        line_width: f64,
        opacity: f64,
    ) -> Result<()> {
        let ops = content::rectangle_ops(x, y, width, height, stroke, fill, line_width);
        let ops = self.with_opacity(page_no, ops, opacity)?;
        self.append_page_content(page_no, &ops)
    }

    /// Draw a straight line (table rule / separator / underline) on a page.
    #[allow(clippy::too_many_arguments)]
    pub fn add_line(
        &mut self,
        page_no: u32,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        stroke: [f64; 3],
        line_width: f64,
        opacity: f64,
    ) -> Result<()> {
        let ops = content::line_ops(x1, y1, x2, y2, stroke, line_width);
        let ops = self.with_opacity(page_no, ops, opacity)?;
        self.append_page_content(page_no, &ops)
    }

    /// Draw an ellipse (or circle when `rx == ry`) centred at `(cx, cy)` on a
    /// page. Colours are RGB in `0.0..=1.0`; pass `None` to skip stroke or fill.
    #[allow(clippy::too_many_arguments)]
    pub fn add_ellipse(
        &mut self,
        page_no: u32,
        cx: f64,
        cy: f64,
        rx: f64,
        ry: f64,
        stroke: Option<[f64; 3]>,
        fill: Option<[f64; 3]>,
        line_width: f64,
        opacity: f64,
    ) -> Result<()> {
        let ops = content::ellipse_ops(cx, cy, rx, ry, stroke, fill, line_width);
        let ops = self.with_opacity(page_no, ops, opacity)?;
        self.append_page_content(page_no, &ops)
    }

    /// Draw a polyline / polygon through `points` (flat `[x0, y0, x1, y1, …]`
    /// pairs) on a page. `close` joins the last vertex to the first. Colours
    /// are RGB in `0.0..=1.0`; pass `None` to skip stroke or fill.
    #[allow(clippy::too_many_arguments)]
    pub fn add_polygon(
        &mut self,
        page_no: u32,
        points: &[f64],
        close: bool,
        stroke: Option<[f64; 3]>,
        fill: Option<[f64; 3]>,
        line_width: f64,
        opacity: f64,
    ) -> Result<()> {
        let pairs: Vec<(f64, f64)> = points.chunks_exact(2).map(|p| (p[0], p[1])).collect();
        let ops = content::polygon_ops(&pairs, close, stroke, fill, line_width);
        let ops = self.with_opacity(page_no, ops, opacity)?;
        self.append_page_content(page_no, &ops)
    }

    /// Draw an arbitrary SVG path (`M`/`L`/`C`/`Q`/`Z`…) on a page, anchored so
    /// the SVG origin maps to PDF user-space `(ox, oy)` with the Y axis flipped
    /// (matches `pdf-lib`'s `drawSvgPath`). Covers freeform/polygon/triangle
    /// shapes. Colours are RGB in `0.0..=1.0`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_path(
        &mut self,
        page_no: u32,
        svg_path: &str,
        ox: f64,
        oy: f64,
        stroke: Option<[f64; 3]>,
        fill: Option<[f64; 3]>,
        line_width: f64,
        opacity: f64,
    ) -> Result<()> {
        let ops = content::svg_path::svg_path_ops(svg_path, ox, oy, stroke, fill, line_width);
        if ops.is_empty() {
            return Ok(()); // nothing drawable in the path
        }
        let ops = self.with_opacity(page_no, ops, opacity)?;
        self.append_page_content(page_no, &ops)
    }

    /// Parse SVG markup and draw it onto a page, fitting its viewBox into the box
    /// `(x, y, width, height)` in PDF user space (origin bottom-left). Renders as
    /// **native vector paths** — crisp at any zoom, not rasterized. Errors only if
    /// the SVG can't be parsed or has nothing drawable.
    pub fn add_svg(
        &mut self,
        page_no: u32,
        src: &str,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Result<()> {
        let img = crate::svg::parse_svg(src)
            .ok_or_else(|| EngineError::Missing("unsupported or empty SVG".into()))?;
        self.draw_svg_image(page_no, &img, x, y, width, height)
    }

    /// Draw an already-parsed [`crate::svg::SvgImage`] onto a page — the HTML
    /// renderer uses this for inline `<svg>` so it isn't re-serialized. Maps the
    /// viewBox onto `(x, y, width, height)` (Y-flipped) and emits PDF path ops.
    pub fn draw_svg_image(
        &mut self,
        page_no: u32,
        img: &crate::svg::SvgImage,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Result<()> {
        use crate::content::num;
        use crate::content::svg_path::Seg;
        use crate::svg::Fill;

        let [vbx, vby, vbw, vbh] = img.view_box;
        if vbw <= 0.0 || vbh <= 0.0 || width <= 0.0 || height <= 0.0 {
            return Ok(());
        }
        let sx = width / vbw;
        let sy = height / vbh;
        // viewBox (Y-down) → PDF box (Y-up, bottom-left at (x, y)).
        let map = |px: f64, py: f64| (x + (px - vbx) * sx, y + height - (py - vby) * sy);
        let stroke_scale = (sx * sy).sqrt();

        for prim in &img.prims {
            // Build the path once (m/l/c/h) in PDF coordinates.
            let mut path: Vec<u8> = Vec::new();
            let mut drew = false;
            for seg in &prim.segs {
                match *seg {
                    Seg::Move(px, py) => {
                        let (u, v) = map(px, py);
                        path.extend_from_slice(format!("{} {} m\n", num(u), num(v)).as_bytes());
                    }
                    Seg::Line(px, py) => {
                        let (u, v) = map(px, py);
                        path.extend_from_slice(format!("{} {} l\n", num(u), num(v)).as_bytes());
                        drew = true;
                    }
                    Seg::Cubic(x1, y1, x2, y2, x3, y3) => {
                        let (a, b) = map(x1, y1);
                        let (c, d) = map(x2, y2);
                        let (e, f) = map(x3, y3);
                        path.extend_from_slice(
                            format!(
                                "{} {} {} {} {} {} c\n",
                                num(a),
                                num(b),
                                num(c),
                                num(d),
                                num(e),
                                num(f)
                            )
                            .as_bytes(),
                        );
                        drew = true;
                    }
                    Seg::Close => path.extend_from_slice(b"h\n"),
                }
            }
            if !drew {
                continue; // move-only / empty subpath draws nothing
            }

            // Fill setup: a flat colour (`rg`) or a shading pattern (`/Pattern cs … scn`).
            let mut setup: Vec<u8> = Vec::new();
            let mut has_fill = false;
            // A gradient's stop alpha isn't carried in the shading itself (that
            // needs a soft-mask group); approximate it as a uniform fill alpha.
            let mut eff_fill_opacity = prim.fill_opacity;
            match &prim.fill {
                Some(Fill::Solid([r, g, b])) => {
                    setup.extend_from_slice(
                        format!("{} {} {} rg\n", num(*r), num(*g), num(*b)).as_bytes(),
                    );
                    has_fill = true;
                }
                Some(Fill::Gradient(grad)) => {
                    if let Some(name) =
                        self.register_svg_shading(page_no, grad, map, stroke_scale)?
                    {
                        setup.extend_from_slice(b"/Pattern cs\n/");
                        setup.extend_from_slice(&name);
                        setup.extend_from_slice(b" scn\n");
                        has_fill = true;
                        let n = grad.stops.len().max(1) as f64;
                        eff_fill_opacity *= grad.stops.iter().map(|s| s.alpha).sum::<f64>() / n;
                    }
                }
                None => {}
            }
            if let Some([r, g, b]) = prim.stroke {
                setup
                    .extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());
                setup.extend_from_slice(
                    format!("{} w\n", num((prim.stroke_w * stroke_scale).max(0.0))).as_bytes(),
                );
            }
            let has_stroke = prim.stroke.is_some();
            if !has_fill && !has_stroke {
                continue;
            }

            let mut ops: Vec<u8> = b"q\n".to_vec();
            ops.extend_from_slice(&setup);
            ops.extend_from_slice(&path);
            ops.extend_from_slice(match (has_fill, has_stroke) {
                (true, true) => b"B\n",
                (true, false) => b"f\n",
                _ => b"S\n",
            });
            ops.extend_from_slice(b"Q\n");

            // Per-primitive alpha (distinct fill/stroke) via a transient ExtGState.
            if eff_fill_opacity < 1.0 || prim.stroke_opacity < 1.0 {
                let mut gs = Dictionary::new();
                gs.set(b"Type".to_vec(), Object::Name(b"ExtGState".to_vec()));
                gs.set(
                    b"ca".to_vec(),
                    Object::Real(eff_fill_opacity.clamp(0.0, 1.0)),
                );
                gs.set(
                    b"CA".to_vec(),
                    Object::Real(prim.stroke_opacity.clamp(0.0, 1.0)),
                );
                let name = self.register_page_resource(
                    page_no,
                    b"ExtGState",
                    "GpGs",
                    Object::Dictionary(gs),
                )?;
                let mut wrapped = b"q\n/".to_vec();
                wrapped.extend_from_slice(&name);
                wrapped.extend_from_slice(b" gs\n");
                wrapped.extend_from_slice(&ops);
                wrapped.extend_from_slice(b"Q\n");
                ops = wrapped;
            }
            self.append_page_content(page_no, &ops)?;
        }
        Ok(())
    }

    /// Register a PDF shading pattern (axial/radial) for an SVG gradient and
    /// return its `/Pattern` resource name. The gradient's coordinates are mapped
    /// to PDF space by `map`; `rscale` scales a radial radius. `None` if there
    /// aren't enough stops to form a gradient.
    fn register_svg_shading(
        &mut self,
        page_no: u32,
        grad: &crate::svg::Gradient,
        map: impl Fn(f64, f64) -> (f64, f64),
        rscale: f64,
    ) -> Result<Option<Vec<u8>>> {
        use crate::svg::GradKind;
        if grad.stops.len() < 2 {
            return Ok(None);
        }
        // 1. A type-0 sampled function: 256 RGB samples across the gradient.
        let mut samples = Vec::with_capacity(256 * 3);
        for i in 0..256u32 {
            let [r, g, b] = sample_svg_gradient(&grad.stops, i as f64 / 255.0);
            samples.extend_from_slice(&[r, g, b]);
        }
        let mut fdict = Dictionary::new();
        fdict.set(b"FunctionType".to_vec(), Object::Integer(0));
        fdict.set(
            b"Domain".to_vec(),
            Object::Array(vec![Object::Integer(0), Object::Integer(1)]),
        );
        fdict.set(
            b"Range".to_vec(),
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(1),
                Object::Integer(0),
                Object::Integer(1),
                Object::Integer(0),
                Object::Integer(1),
            ]),
        );
        fdict.set(b"Size".to_vec(), Object::Array(vec![Object::Integer(256)]));
        fdict.set(b"BitsPerSample".to_vec(), Object::Integer(8));
        fdict.set(b"Length".to_vec(), Object::Integer(samples.len() as i64));
        let fn_id = (self.next_object_number(), 0u16);
        self.objects
            .insert(fn_id, Object::Stream(Stream::new(fdict, samples)));

        // 2. The shading dictionary (axial = type 2, radial = type 3).
        let mut sh = Dictionary::new();
        sh.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceRGB".to_vec()));
        sh.set(b"Function".to_vec(), Object::Reference(fn_id));
        sh.set(
            b"Extend".to_vec(),
            Object::Array(vec![Object::Boolean(true), Object::Boolean(true)]),
        );
        match grad.kind {
            GradKind::Linear { x1, y1, x2, y2 } => {
                let (a, b) = map(x1, y1);
                let (c, d) = map(x2, y2);
                sh.set(b"ShadingType".to_vec(), Object::Integer(2));
                sh.set(
                    b"Coords".to_vec(),
                    Object::Array(vec![
                        Object::Real(a),
                        Object::Real(b),
                        Object::Real(c),
                        Object::Real(d),
                    ]),
                );
            }
            GradKind::Radial { cx, cy, r, fx, fy } => {
                let (pcx, pcy) = map(cx, cy);
                let (pfx, pfy) = map(fx, fy);
                sh.set(b"ShadingType".to_vec(), Object::Integer(3));
                sh.set(
                    b"Coords".to_vec(),
                    Object::Array(vec![
                        Object::Real(pfx),
                        Object::Real(pfy),
                        Object::Real(0.0),
                        Object::Real(pcx),
                        Object::Real(pcy),
                        Object::Real((r * rscale).max(0.0)),
                    ]),
                );
            }
        }

        // 3. A shading pattern (PatternType 2) registered on the page.
        let mut pat = Dictionary::new();
        pat.set(b"Type".to_vec(), Object::Name(b"Pattern".to_vec()));
        pat.set(b"PatternType".to_vec(), Object::Integer(2));
        pat.set(b"Shading".to_vec(), Object::Dictionary(sh));
        pat.set(
            b"Matrix".to_vec(),
            Object::Array(vec![
                Object::Integer(1),
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(1),
                Object::Integer(0),
                Object::Integer(0),
            ]),
        );
        Ok(Some(self.register_page_resource(
            page_no,
            b"Pattern",
            "GpSh",
            Object::Dictionary(pat),
        )?))
    }

    /// Draw a **colour glyph** (COLR/CPAL emoji) as native vector layers at the
    /// baseline origin `(x, baseline)` (PDF user space, Y-up) for text `size`.
    /// Each layer fills its glyph's contours with the palette colour (`fg` for
    /// foreground-indexed layers). Returns the glyph advance in points so the
    /// caller can move the pen. A non-colour glyph draws nothing (returns its
    /// advance) so callers can fall back to normal text.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_color_glyph(
        &mut self,
        page_no: u32,
        face: &crate::font::truetype::TrueTypeFont,
        colors: &crate::font::color::ColorGlyphs,
        base_gid: u16,
        x: f64,
        baseline: f64,
        size: f64,
        fg: [f64; 3],
    ) -> Result<f64> {
        use crate::content::num;
        let upm = face.units_per_em().max(1.0);
        let s = size / upm;
        let advance = face.advance_width(base_gid) * s;
        let Some(layers) = colors.layers(base_gid) else {
            return Ok(advance);
        };
        // Font outlines are Y-up like PDF, so no flip: glyph y=0 sits on `baseline`.
        for layer in layers {
            let contours = face.glyph_polygons(layer.gid);
            if contours.is_empty() {
                continue;
            }
            let rgb = if layer.use_foreground { fg } else { layer.rgb };
            let mut ops: Vec<u8> = b"q\n".to_vec();
            ops.extend_from_slice(
                format!("{} {} {} rg\n", num(rgb[0]), num(rgb[1]), num(rgb[2])).as_bytes(),
            );
            for contour in &contours {
                if contour.len() < 2 {
                    continue;
                }
                let (fx, fy) = contour[0];
                ops.extend_from_slice(
                    format!("{} {} m\n", num(x + fx * s), num(baseline + fy * s)).as_bytes(),
                );
                for &(fx, fy) in &contour[1..] {
                    ops.extend_from_slice(
                        format!("{} {} l\n", num(x + fx * s), num(baseline + fy * s)).as_bytes(),
                    );
                }
                ops.extend_from_slice(b"h\n");
            }
            ops.extend_from_slice(b"f\n"); // nonzero-winding fill
            ops.extend_from_slice(b"Q\n");
            let ops = self.with_opacity(page_no, ops, layer.alpha)?;
            self.append_page_content(page_no, &ops)?;
        }
        Ok(advance)
    }

    /// Draw an Apple `sbix` colour-emoji glyph as a bitmap on the baseline at
    /// `(x, baseline)` for text `size`. Returns `true` if the glyph had a PNG
    /// bitmap (placed), `false` otherwise so the caller can fall back.
    pub fn draw_sbix_glyph(
        &mut self,
        page_no: u32,
        face: &crate::font::truetype::TrueTypeFont,
        gid: u16,
        x: f64,
        baseline: f64,
        size: f64,
    ) -> Result<bool> {
        let Some(sb) = face.sbix_glyphs() else {
            return Ok(false);
        };
        let Some(g) = sb.glyph(gid) else {
            return Ok(false);
        };
        // Origin offsets are pixels at the strike ppem → convert to points; the
        // bitmap covers roughly the em box, so place a `size × size` image.
        let scale = size / g.ppem.max(1.0);
        let _ = self.add_image(
            page_no,
            &g.png,
            x + g.origin_x * scale,
            baseline + g.origin_y * scale,
            size,
            size,
            1.0,
        );
        Ok(true)
    }

    /// Embed a raster image (PNG or JPEG) on a page and draw it at `(x, y)` with
    /// size `(width, height)` in PDF user space (origin bottom-left). `opacity`
    /// in `0.0..=1.0` sets fill alpha via a transient `/ExtGState`. PNG alpha is
    /// honoured through a soft mask; JPEG embeds losslessly via `/DCTDecode`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_image(
        &mut self,
        page_no: u32,
        data: &[u8],
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        opacity: f64,
    ) -> Result<()> {
        use content::image::{ImageColor, ImageFilter};
        let prep = content::image::prepare_image(data)
            .ok_or_else(|| EngineError::Missing("unsupported image (need PNG or JPEG)".into()))?;

        // PNG alpha → a /DeviceGray soft-mask image XObject referenced by /SMask.
        let smask_ref = match prep.smask {
            Some(alpha) => {
                let mut m = Dictionary::new();
                m.set(b"Type".to_vec(), Object::Name(b"XObject".to_vec()));
                m.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
                m.set(b"Width".to_vec(), Object::Integer(prep.width as i64));
                m.set(b"Height".to_vec(), Object::Integer(prep.height as i64));
                m.set(b"ColorSpace".to_vec(), Object::Name(b"DeviceGray".to_vec()));
                m.set(b"BitsPerComponent".to_vec(), Object::Integer(8));
                m.set(b"Filter".to_vec(), Object::Name(b"FlateDecode".to_vec()));
                m.set(b"Length".to_vec(), Object::Integer(alpha.len() as i64));
                let id = (self.next_object_number(), 0u16);
                self.objects
                    .insert(id, Object::Stream(Stream::new(m, alpha)));
                Some(id)
            }
            None => None,
        };

        // Main image XObject.
        let mut dict = Dictionary::new();
        dict.set(b"Type".to_vec(), Object::Name(b"XObject".to_vec()));
        dict.set(b"Subtype".to_vec(), Object::Name(b"Image".to_vec()));
        dict.set(b"Width".to_vec(), Object::Integer(prep.width as i64));
        dict.set(b"Height".to_vec(), Object::Integer(prep.height as i64));
        dict.set(
            b"ColorSpace".to_vec(),
            Object::Name(match prep.color {
                ImageColor::Gray => b"DeviceGray".to_vec(),
                ImageColor::Rgb => b"DeviceRGB".to_vec(),
                ImageColor::Cmyk => b"DeviceCMYK".to_vec(),
            }),
        );
        dict.set(b"BitsPerComponent".to_vec(), Object::Integer(8));
        dict.set(
            b"Filter".to_vec(),
            Object::Name(match prep.filter {
                ImageFilter::Dct => b"DCTDecode".to_vec(),
                ImageFilter::Flate => b"FlateDecode".to_vec(),
            }),
        );
        if prep.cmyk_invert {
            // Adobe CMYK JPEGs store inverted ink; flip every channel.
            dict.set(
                b"Decode".to_vec(),
                Object::Array(vec![
                    Object::Integer(1),
                    Object::Integer(0),
                    Object::Integer(1),
                    Object::Integer(0),
                    Object::Integer(1),
                    Object::Integer(0),
                    Object::Integer(1),
                    Object::Integer(0),
                ]),
            );
        }
        if let Some(id) = smask_ref {
            dict.set(b"SMask".to_vec(), Object::Reference(id));
        }
        dict.set(b"Length".to_vec(), Object::Integer(prep.data.len() as i64));
        let img_id = (self.next_object_number(), 0u16);
        self.objects
            .insert(img_id, Object::Stream(Stream::new(dict, prep.data)));

        // Register the image in the page's /Resources /XObject and get its name.
        let img_name =
            self.register_page_resource(page_no, b"XObject", "GpImg", Object::Reference(img_id))?;

        let ops = content::image_ops(&img_name, x, y, width, height);
        let ops = self.with_opacity(page_no, ops, opacity)?;
        self.append_page_content(page_no, &ops)
    }

    /// Register `value` under a fresh name in a page's `/Resources /{category}`
    /// sub-dictionary (e.g. `XObject`, `ExtGState`), cloning any inherited or
    /// indirect resource dictionaries onto the page so the addition is local.
    /// Returns the chosen resource name (`{prefix}{n}`).
    fn register_page_resource(
        &mut self,
        page_no: u32,
        category: &[u8],
        prefix: &str,
        value: Object,
    ) -> Result<Vec<u8>> {
        let page_id = self.page_object_id(page_no)?;
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();

        let mut resources = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        let mut sub = resources
            .get(category)
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();

        // First free `{prefix}{n}` name in this sub-dictionary.
        let mut n = 0usize;
        let name = loop {
            let candidate = format!("{prefix}{n}").into_bytes();
            if sub.get(&candidate).is_none() {
                break candidate;
            }
            n += 1;
        };

        sub.0.insert(name.clone(), value);
        resources.set(category.to_vec(), Object::Dictionary(sub));
        page.set(b"Resources".to_vec(), Object::Dictionary(resources));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(name)
    }

    /// Append `ops` to a page's content stream, on a fresh line.
    fn append_page_content(&mut self, page_no: u32, ops: &[u8]) -> Result<()> {
        let mut content = self.page_content(page_no)?;
        content.push(b'\n');
        content.extend_from_slice(ops);
        self.set_page_content(page_no, content)
    }

    /// Wrap `ops` in a `q /Gs gs … Q` block applying `opacity` (fill + stroke
    /// alpha) through an `/ExtGState`, or return `ops` unchanged when fully
    /// opaque. The outer graphics-state nesting lets the alpha reach the inner
    /// `q … Q` the shape/image ops already emit.
    fn with_opacity(&mut self, page_no: u32, ops: Vec<u8>, opacity: f64) -> Result<Vec<u8>> {
        if opacity >= 1.0 {
            return Ok(ops);
        }
        let ca = opacity.clamp(0.0, 1.0);
        let mut gs = Dictionary::new();
        gs.set(b"Type".to_vec(), Object::Name(b"ExtGState".to_vec()));
        gs.set(b"ca".to_vec(), Object::Real(ca));
        gs.set(b"CA".to_vec(), Object::Real(ca));
        let name =
            self.register_page_resource(page_no, b"ExtGState", "GpGs", Object::Dictionary(gs))?;
        let mut out = b"q\n/".to_vec();
        out.extend_from_slice(&name);
        out.extend_from_slice(b" gs\n");
        out.extend_from_slice(&ops);
        out.extend_from_slice(b"Q\n");
        Ok(out)
    }

    // ─── annotations ─────────────────────────────────────────────────────────

    fn read_rect(&self, dict: &Dictionary) -> [f64; 4] {
        let mut rect = [0.0f64; 4];
        if let Some(items) = dict
            .get(b"Rect")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for (i, value) in items.iter().take(4).enumerate() {
                rect[i] = self.resolve(value).as_f64().unwrap_or(0.0);
            }
        }
        rect
    }

    /// List a page's annotations.
    pub fn page_annotations(&self, page_no: u32) -> Result<Vec<Annotation>> {
        let page = self.page_dict(page_no)?;
        let items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let mut out = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let Some(dict) = self.resolve(item).as_dict() else {
                continue;
            };
            let subtype = dict
                .get(b"Subtype")
                .and_then(Object::as_name)
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            let rect = self.read_rect(dict);
            // `/Contents`, `/T`, `/Subj`, `/CreationDate`, `/M` are all text
            // strings; `/Name` (stamp) is a name. Dates are left raw (the host
            // parses the `D:YYYYMMDD…` form).
            let text_of = |key: &[u8]| match dict.get(key).map(|o| self.resolve(o)) {
                Some(Object::String(bytes, _)) => crate::font::decode_pdf_text(bytes),
                _ => String::new(),
            };
            let contents = text_of(b"Contents");
            let author = text_of(b"T");
            let subject = text_of(b"Subj");
            let created = text_of(b"CreationDate");
            let modified = text_of(b"M");
            let name = match dict.get(b"Name").map(|o| self.resolve(o)) {
                Some(Object::Name(bytes)) => String::from_utf8_lossy(bytes).into_owned(),
                Some(Object::String(bytes, _)) => crate::font::decode_pdf_text(bytes),
                _ => String::new(),
            };
            let color = self.annotation_rgb(dict);
            let opacity = dict
                .get(b"CA")
                .map(|o| self.resolve(o))
                .and_then(|o| o.as_f64())
                .unwrap_or(1.0);
            let quad_points = self.read_num_array(dict, b"QuadPoints");
            let ink_list = self.read_ink_list(dict);
            let (link_uri, link_page) = if subtype == "Link" {
                match self.link_target(dict) {
                    LinkTarget::Uri(uri) => (uri, 0),
                    LinkTarget::Page(p) => (String::new(), p),
                    LinkTarget::Unknown => (String::new(), 0),
                }
            } else {
                (String::new(), 0)
            };
            out.push(Annotation {
                index,
                subtype,
                rect,
                contents,
                author,
                subject,
                created,
                modified,
                color,
                opacity,
                quad_points,
                ink_list,
                name,
                link_uri,
                link_page,
            });
        }
        Ok(out)
    }

    /// Read an annotation's `/C` array and normalise it to RGB in `0.0..=1.0`:
    /// `[]` → empty (no colour), `[g]` → gray replicated, `[r g b]` → as-is,
    /// `[c m y k]` → naive CMYK→RGB. Anything else → empty.
    fn annotation_rgb(&self, dict: &Dictionary) -> Vec<f64> {
        let Some(arr) = dict.get(b"C").map(|o| self.resolve(o)) else {
            return Vec::new();
        };
        let Some(items) = arr.as_array() else {
            return Vec::new();
        };
        let c: Vec<f64> = items.iter().filter_map(Object::as_f64).collect();
        match c.len() {
            1 => vec![c[0], c[0], c[0]],
            3 => c,
            4 => {
                let k = c[3];
                vec![
                    (1.0 - c[0]) * (1.0 - k),
                    (1.0 - c[1]) * (1.0 - k),
                    (1.0 - c[2]) * (1.0 - k),
                ]
            }
            _ => Vec::new(),
        }
    }

    /// Read a flat number array (e.g. `/QuadPoints`) as `Vec<f64>`; empty when
    /// absent or not an array.
    fn read_num_array(&self, dict: &Dictionary, key: &[u8]) -> Vec<f64> {
        dict.get(key)
            .map(|o| self.resolve(o))
            .and_then(|o| o.as_array().map(<[Object]>::to_vec))
            .map(|items| items.iter().filter_map(Object::as_f64).collect())
            .unwrap_or_default()
    }

    /// Read an `/InkList` (array of number arrays) as `Vec<Vec<f64>>`; empty
    /// when absent.
    fn read_ink_list(&self, dict: &Dictionary) -> Vec<Vec<f64>> {
        let Some(arr) = dict.get(b"InkList").map(|o| self.resolve(o)) else {
            return Vec::new();
        };
        let Some(paths) = arr.as_array() else {
            return Vec::new();
        };
        paths
            .iter()
            .filter_map(|p| {
                self.resolve(p)
                    .as_array()
                    .map(|pts| pts.iter().filter_map(Object::as_f64).collect())
            })
            .collect()
    }

    /// Remove the annotation at `index` from a page's `/Annots`.
    pub fn remove_annotation(&mut self, page_no: u32, index: usize) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        let mut items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        if index >= items.len() {
            return Err(EngineError::Missing(format!("annotation #{index}")));
        }
        items.remove(index);
        page.set(b"Annots".to_vec(), Object::Array(items));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    fn add_annotation(&mut self, page_no: u32, mut built: annot::Built) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let rect = self.read_rect(&built.dict);

        let appearance_id = (self.next_object_number(), 0u16);
        let annotation_id = (appearance_id.0 + 1, 0u16);

        // Appearance form XObject.
        let mut form = Dictionary::new();
        form.set(b"Type".to_vec(), annot::name(b"XObject"));
        form.set(b"Subtype".to_vec(), annot::name(b"Form"));
        form.set(b"BBox".to_vec(), annot::real_array(&rect));
        form.set(b"Resources".to_vec(), Object::Dictionary(built.resources));
        form.set(
            b"Length".to_vec(),
            Object::Integer(built.appearance.len() as i64),
        );
        self.objects.insert(
            appearance_id,
            Object::Stream(Stream::new(form, built.appearance)),
        );

        // Annotation dict with /AP /N -> form.
        let mut appearance = Dictionary::new();
        appearance.set(b"N".to_vec(), Object::Reference(appearance_id));
        built
            .dict
            .set(b"AP".to_vec(), Object::Dictionary(appearance));
        built.dict.set(b"Type".to_vec(), annot::name(b"Annot"));
        self.objects
            .insert(annotation_id, Object::Dictionary(built.dict));

        // Append to the page's /Annots.
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        let mut items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        items.push(Object::Reference(annotation_id));
        page.set(b"Annots".to_vec(), Object::Array(items));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    /// Add a rectangle (Square) annotation.
    pub fn add_square_annotation(
        &mut self,
        page_no: u32,
        rect: [f64; 4],
        stroke: Option<[f64; 3]>,
        fill: Option<[f64; 3]>,
        line_width: f64,
    ) -> Result<()> {
        self.add_annotation(page_no, annot::square(rect, stroke, fill, line_width))
    }

    /// Add a Highlight annotation (translucent colour over the rectangle).
    pub fn add_highlight(&mut self, page_no: u32, rect: [f64; 4], color: [f64; 3]) -> Result<()> {
        self.add_annotation(page_no, annot::highlight(rect, color))
    }

    /// Add a text-markup annotation (`Highlight` / `Underline` / `StrikeOut` /
    /// `Squiggly`) spanning one or more `quads` (each `[x0, y0, x1, y1]` in PDF
    /// user space, bottom-left origin) — multi-quad covers wrapped text. Carries
    /// the full reviewer metadata: translucent `color` + `opacity`, popup
    /// `contents`, `author` (`/T`), stable `id` (`/NM`) and the modification
    /// `date` (`/M`, a PDF date string supplied by the host since the engine has
    /// no clock). Empty `contents`/`author`/`id`/`date` are omitted.
    #[allow(clippy::too_many_arguments)]
    pub fn add_markup_annotation(
        &mut self,
        page_no: u32,
        subtype: &str,
        quads: &[[f64; 4]],
        color: [f64; 3],
        opacity: f64,
        contents: &str,
        author: &str,
        id: &str,
        date: &str,
    ) -> Result<()> {
        if quads.is_empty() {
            return Err(EngineError::Unsupported(
                "markup annotation needs at least one quad".into(),
            ));
        }
        let subtype_name: &[u8] = match subtype {
            "Highlight" => b"Highlight",
            "Underline" => b"Underline",
            "StrikeOut" => b"StrikeOut",
            "Squiggly" => b"Squiggly",
            _ => return Err(EngineError::Unsupported("unknown markup subtype".into())),
        };

        // Bounding /Rect over every quad.
        let mut rect = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for q in quads {
            rect[0] = rect[0].min(q[0]).min(q[2]);
            rect[1] = rect[1].min(q[1]).min(q[3]);
            rect[2] = rect[2].max(q[0]).max(q[2]);
            rect[3] = rect[3].max(q[1]).max(q[3]);
        }

        // /QuadPoints (UL UR LL LR per quad) + matching per-quad appearance ops.
        let mut quad_points: Vec<f64> = Vec::with_capacity(quads.len() * 8);
        let mut appearance: Vec<u8> = Vec::new();
        for &[x0, y0, x1, y1] in quads {
            quad_points.extend_from_slice(&[x0, y1, x1, y1, x0, y0, x1, y0]);
            match subtype {
                "Highlight" => appearance.extend_from_slice(&content::rectangle_ops(
                    x0,
                    y0,
                    x1 - x0,
                    y1 - y0,
                    None,
                    Some(color),
                    0.0,
                )),
                "StrikeOut" => {
                    let w = ((y1 - y0) * 0.06).max(0.75);
                    let y = (y0 + y1) / 2.0;
                    appearance.extend_from_slice(&content::line_ops(x0, y, x1, y, color, w));
                }
                _ => {
                    // Underline / Squiggly: a rule near the baseline.
                    let w = ((y1 - y0) * 0.06).max(0.75);
                    let y = y0 + (y1 - y0) * 0.08;
                    appearance.extend_from_slice(&content::line_ops(x0, y, x1, y, color, w));
                }
            }
        }

        let mut dict = Dictionary::new();
        dict.set(b"Subtype".to_vec(), annot::name(subtype_name));
        dict.set(b"Rect".to_vec(), annot::real_array(&rect));
        dict.set(b"C".to_vec(), annot::real_array(&color));
        dict.set(b"CA".to_vec(), Object::Real(opacity));
        dict.set(b"QuadPoints".to_vec(), annot::real_array(&quad_points));
        set_annotation_metadata(&mut dict, contents, author, id, date);

        self.add_annotation(
            page_no,
            annot::Built {
                dict,
                appearance,
                resources: Dictionary::new(),
            },
        )
    }

    /// Add a sticky-note (`/Text`) annotation: a small badge that opens a popup
    /// with `contents`. `icon` is the `/Name` (`"Note"`, `"Comment"`, …) and
    /// `open` sets the initial popup state. A filled badge appearance keeps it
    /// visible in viewers that don't render the named icon.
    #[allow(clippy::too_many_arguments)]
    pub fn add_text_note(
        &mut self,
        page_no: u32,
        rect: [f64; 4],
        contents: &str,
        author: &str,
        id: &str,
        date: &str,
        open: bool,
        icon: &str,
        color: [f64; 3],
    ) -> Result<()> {
        let mut dict = Dictionary::new();
        dict.set(b"Subtype".to_vec(), annot::name(b"Text"));
        dict.set(b"Rect".to_vec(), annot::real_array(&rect));
        dict.set(b"Name".to_vec(), annot::name(icon.as_bytes()));
        dict.set(b"Open".to_vec(), Object::Boolean(open));
        dict.set(b"C".to_vec(), annot::real_array(&color));
        set_annotation_metadata(&mut dict, contents, author, id, date);
        let [x0, y0, x1, y1] = rect;
        let appearance =
            content::rectangle_ops(x0, y0, x1 - x0, y1 - y0, Some(color), Some(color), 1.0);
        self.add_annotation(
            page_no,
            annot::Built {
                dict,
                appearance,
                resources: Dictionary::new(),
            },
        )
    }

    /// Add a Line annotation.
    #[allow(clippy::too_many_arguments)]
    pub fn add_line_annotation(
        &mut self,
        page_no: u32,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color: [f64; 3],
        line_width: f64,
    ) -> Result<()> {
        self.add_annotation(page_no, annot::line(x1, y1, x2, y2, color, line_width))
    }

    /// Add a FreeText annotation (a text box on the page).
    pub fn add_free_text(
        &mut self,
        page_no: u32,
        rect: [f64; 4],
        text: &str,
        font_size: f64,
        color: [f64; 3],
    ) -> Result<()> {
        self.add_annotation(page_no, annot::free_text(rect, text, font_size, color))
    }

    /// Embed a TrueType font program (`.ttf`, glyf-based) as a Type0 /
    /// CIDFontType2 font with Identity-H encoding, full per-glyph widths and a
    /// `ToUnicode` map. Returns the Type0 font's object number — pass it to
    /// [`add_text`](Self::add_text). The host downloads the bytes (e.g. via
    /// [`font::google::css_url`](crate::font::google::css_url)) and the engine
    /// bakes them in, so the output renders the same font everywhere.
    pub fn embed_truetype_font(&mut self, family: &str, ttf: &[u8]) -> Result<u32> {
        self.embed_font(family, ttf)
    }

    /// Embed **any** outline font program — glyf-based TrueType *or* OpenType-CFF
    /// (`OTTO`) — as a subsettable Type0 font, returning its object number for
    /// [`add_text`](Self::add_text). The flavour is detected automatically:
    ///
    /// * **glyf TrueType** → `CIDFontType2` descendant + `FontFile2` + Identity
    ///   `CIDToGIDMap` (CID = GID), later subset by `save_compressed`.
    /// * **OpenType-CFF** (`OTTO`, outlines in the `CFF ` table) → `CIDFontType0`
    ///   descendant + `FontFile3` `/Subtype /OpenType` (the whole OpenType file;
    ///   for a non-CID-keyed CFF the viewer uses the CID directly as a glyph id,
    ///   ISO 32000-1 §9.7.4.2).
    ///
    /// Both flavours encode with Identity-H, carry a full `/W` width array and a
    /// `/ToUnicode` CMap (so copy/extract round-trips), and resolve their char→GID
    /// map at draw time — making `add_text`, `replace_text_run` and every other
    /// text operation work with arbitrary families (base-14, Google Fonts, or a
    /// face extracted from the document itself). Kept under the historical name
    /// [`embed_truetype_font`](Self::embed_truetype_font) too.
    pub fn embed_font(&mut self, family: &str, program: &[u8]) -> Result<u32> {
        // glyf TrueType parses with the strict reader; OpenType-CFF (no glyf) is
        // recognised by its `OTTO` sfnt tag and read for metrics/cmap only.
        let no_extra = std::collections::BTreeMap::new();
        if let Some(parsed) = crate::font::truetype::TrueTypeFont::parse(program) {
            self.embed_cid_font(family, program, &parsed, false, &no_extra)
        } else if program.get(0..4) == Some(b"OTTO".as_slice()) {
            // An already-OpenType-CFF program: recover the CFF charset's ligature
            // names from its `CFF ` table for /ToUnicode (the synthesised cmap on
            // some such files omits ligature glyphs).
            let parsed = crate::font::truetype::TrueTypeFont::parse_metrics(program)
                .ok_or_else(|| EngineError::Unsupported("unparseable OpenType-CFF font".into()))?;
            let extra = cff_ligature_unicode_from_otf(program);
            self.embed_cid_font(family, program, &parsed, true, &extra)
        } else if program.len() >= 4
            && program[0] == 1
            && program[1] == 0
            && program[2] >= 4
            && (1..=4).contains(&program[3])
        {
            // Bare CFF (a PDF `FontFile3 /Subtype /Type1C`): wrap it in a
            // synthesised OpenType-CFF sfnt (cmap/head/hmtx/… built from the
            // CFF's own metrics + charset) so the OpenType-CFF path above can
            // re-embed it — no external fontforge conversion needed.
            let extra = crate::font::cff::CffFont::parse(program)
                .map(|cff| crate::font::cff_to_otf::cff_gid_unicode_strings(&cff))
                .unwrap_or_default();
            let otf = crate::font::cff_to_otf::wrap(program)
                .ok_or_else(|| EngineError::Unsupported("unparseable bare CFF font".into()))?;
            let parsed = crate::font::truetype::TrueTypeFont::parse_metrics(&otf)
                .ok_or_else(|| EngineError::Unsupported("wrapped CFF not parseable".into()))?;
            self.embed_cid_font(family, &otf, &parsed, true, &extra)
        } else if is_raw_type1(program) {
            // Raw Type 1 (PDF `FontFile`, `.pfb` or `.pfa`): decrypt eexec,
            // transcode each Type 1 charstring to Type 2, pack a bare CFF, then
            // route it through the same wrap → OpenType-CFF path as above.
            let cff = crate::font::type1::parse_type1(program)
                .and_then(|font| crate::font::type1::to_cff(&font))
                .ok_or_else(|| EngineError::Unsupported("unparseable Type1 font".into()))?;
            let extra = crate::font::cff::CffFont::parse(&cff)
                .map(|c| crate::font::cff_to_otf::cff_gid_unicode_strings(&c))
                .unwrap_or_default();
            let otf = crate::font::cff_to_otf::wrap(&cff)
                .ok_or_else(|| EngineError::Unsupported("unparseable Type1 font".into()))?;
            let parsed = crate::font::truetype::TrueTypeFont::parse_metrics(&otf)
                .ok_or_else(|| EngineError::Unsupported("unparseable Type1 font".into()))?;
            self.embed_cid_font(family, &otf, &parsed, true, &extra)
        } else {
            Err(EngineError::Unsupported(
                "not a glyf TrueType, OpenType-CFF, bare CFF or Type1 font program".into(),
            ))
        }
    }

    /// Assemble the Type0 object graph shared by both font flavours. `is_cff`
    /// selects `FontFile3`/`CIDFontType0` (OpenType-CFF) over the default
    /// `FontFile2`/`CIDFontType2` (glyf TrueType). `extra_unicode` overlays extra
    /// glyph-id → Unicode mappings onto the cmap-derived `/ToUnicode` (used to add
    /// CFF ligature-name expansions, which the synthesised cmap can't carry).
    fn embed_cid_font(
        &mut self,
        family: &str,
        program: &[u8],
        parsed: &crate::font::truetype::TrueTypeFont,
        is_cff: bool,
        extra_unicode: &std::collections::BTreeMap<u16, String>,
    ) -> Result<u32> {
        use crate::object::StringKind::Literal;
        let ps_name = postscript_name(family);

        let advances = crate::font::embed::scaled_advances(parsed);
        // Full-Unicode cmap reverse-map PLUS ligature-glyph expansions (from the
        // font's own GSUB), so ligated/astral-plane text extracts & copies
        // faithfully instead of as tofu.
        let ligatures = crate::font::shape::Shaper::new(parsed).ligature_rules();
        let mut unicode_map: std::collections::BTreeMap<u16, String> =
            crate::font::embed::gid_to_unicode_with_ligatures(parsed, &ligatures)
                .into_iter()
                .collect();
        // CFF ligature names (`ffi`, …) that no code point maps to: overlay them
        // so a ligated CFF run still extracts as the source characters.
        for (gid, s) in extra_unicode {
            unicode_map.entry(*gid).or_insert_with(|| s.clone());
        }
        let unicode: Vec<(u16, String)> = unicode_map.into_iter().collect();
        let tounicode = crate::font::embed::to_unicode_cmap(&unicode);

        // Five consecutive ids: FontFile, FontDescriptor, CIDFont, Type0, ToUnicode.
        let ff_id = (self.next_object_number(), 0u16);
        let fd_id = (ff_id.0 + 1, 0u16);
        let cid_id = (ff_id.0 + 2, 0u16);
        let t0_id = (ff_id.0 + 3, 0u16);
        let tu_id = (ff_id.0 + 4, 0u16);

        // The raw font program (compressed later by save_compressed). glyf goes in
        // FontFile2 (with Length1); CFF/OpenType in FontFile3 (/Subtype /OpenType).
        let mut ff = Dictionary::new();
        ff.set(b"Length".to_vec(), Object::Integer(program.len() as i64));
        let ff_key: &[u8] = if is_cff {
            ff.set(b"Subtype".to_vec(), annot::name(b"OpenType"));
            b"FontFile3"
        } else {
            ff.set(b"Length1".to_vec(), Object::Integer(program.len() as i64));
            b"FontFile2"
        };
        self.objects
            .insert(ff_id, Object::Stream(Stream::new(ff, program.to_vec())));

        // FontDescriptor — generic metrics (fine for display; exact values would
        // need OS/2/hhea parsing).
        let mut fd = Dictionary::new();
        fd.set(b"Type", annot::name(b"FontDescriptor"));
        fd.set(b"FontName", annot::name(ps_name.as_bytes()));
        fd.set(b"Flags", Object::Integer(32)); // Nonsymbolic
        fd.set(
            b"FontBBox",
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(-200),
                Object::Integer(1000),
                Object::Integer(900),
            ]),
        );
        fd.set(b"ItalicAngle", Object::Integer(0));
        fd.set(b"Ascent", Object::Integer(800));
        fd.set(b"Descent", Object::Integer(-200));
        fd.set(b"CapHeight", Object::Integer(700));
        fd.set(b"StemV", Object::Integer(80));
        fd.set(ff_key, Object::Reference(ff_id));
        self.objects.insert(fd_id, Object::Dictionary(fd));

        // Descendant CIDFont — Type2 (TrueType, with Identity CIDToGIDMap) or
        // Type0 (CFF). Identity ordering + full widths in either case.
        let w_inner: Vec<Object> = advances
            .iter()
            .map(|&w| Object::Integer(w as i64))
            .collect();
        let mut cidsi = Dictionary::new();
        cidsi.set(b"Registry", Object::String(b"Adobe".to_vec(), Literal));
        cidsi.set(b"Ordering", Object::String(b"Identity".to_vec(), Literal));
        cidsi.set(b"Supplement", Object::Integer(0));
        let mut cid = Dictionary::new();
        cid.set(b"Type", annot::name(b"Font"));
        cid.set(
            b"Subtype",
            annot::name(if is_cff {
                b"CIDFontType0"
            } else {
                b"CIDFontType2"
            }),
        );
        cid.set(b"BaseFont", annot::name(ps_name.as_bytes()));
        cid.set(b"CIDSystemInfo", Object::Dictionary(cidsi));
        cid.set(b"FontDescriptor", Object::Reference(fd_id));
        if !is_cff {
            // CIDToGIDMap is a CIDFontType2-only key (TrueType); CIDFontType0 maps
            // CID→glyph through the CFF charset.
            cid.set(b"CIDToGIDMap", annot::name(b"Identity"));
        }
        cid.set(b"DW", Object::Integer(1000));
        cid.set(
            b"W",
            Object::Array(vec![Object::Integer(0), Object::Array(w_inner)]),
        );
        self.objects.insert(cid_id, Object::Dictionary(cid));

        // ToUnicode CMap (copy/extract round-trips).
        let mut tu = Dictionary::new();
        tu.set(b"Length", Object::Integer(tounicode.len() as i64));
        self.objects
            .insert(tu_id, Object::Stream(Stream::new(tu, tounicode)));

        // Type0 wrapper.
        let mut t0 = Dictionary::new();
        t0.set(b"Type", annot::name(b"Font"));
        t0.set(b"Subtype", annot::name(b"Type0"));
        t0.set(b"BaseFont", annot::name(ps_name.as_bytes()));
        t0.set(b"Encoding", annot::name(b"Identity-H"));
        t0.set(
            b"DescendantFonts",
            Object::Array(vec![Object::Reference(cid_id)]),
        );
        t0.set(b"ToUnicode", Object::Reference(tu_id));
        self.objects.insert(t0_id, Object::Dictionary(t0));

        Ok(t0_id.0)
    }

    /// Add a real, selectable text run to a page's content stream, set in a font
    /// previously embedded with [`embed_truetype_font`](Self::embed_truetype_font).
    /// `x`/`y` are the text origin in PDF user space (origin bottom-left); `size`
    /// is in points; `color` is the RGB fill `0..=1`.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn add_text(
        &mut self,
        page_no: u32,
        x: f64,
        y: f64,
        size: f64,
        text: &str,
        font_obj: u32,
        color: [f64; 3],
        opacity: f64,
        rotation_deg: f64,
    ) -> Result<()> {
        let ttf = self.embedded_truetype(font_obj).ok_or_else(|| {
            EngineError::Unsupported("font_obj is not an embedded TrueType font".into())
        })?;
        // Identity-H shows two-byte glyph ids directly.
        let mut hex = String::new();
        let used = self.font_used_gids.entry(font_obj).or_default();
        for ch in text.chars() {
            let gid = ttf.gid_for_unicode(ch as u32).unwrap_or(0);
            used.insert(gid);
            hex.push_str(&format!("{gid:04X}"));
        }
        let res_name = format!("GF{font_obj}");
        // Rotation rides on the text matrix [cos sin -sin cos x y]; at 0° this is
        // the identity rotation (cos=1, sin=0), so glyphs sit at (x, y).
        let (sin, cos) = rotation_deg.to_radians().sin_cos();
        let inner = format!(
            "q\n{r} {g} {b} rg\nBT\n/{res} {size} Tf\n{ma} {mb} {mc} {md} {x} {y} Tm\n<{hex}> Tj\nET\nQ\n",
            r = content::num(color[0]),
            g = content::num(color[1]),
            b = content::num(color[2]),
            res = res_name,
            size = content::num(size),
            ma = content::num(cos),
            mb = content::num(sin),
            mc = content::num(-sin),
            md = content::num(cos),
            x = content::num(x),
            y = content::num(y),
        )
        .into_bytes();
        // `with_opacity` wraps the run in an ExtGState (/ca + /CA) when opacity < 1
        // (and registers it on the page); at opacity 1 it returns `inner` as-is.
        let ops = self.with_opacity(page_no, inner, opacity)?;
        let mut content = self.page_content(page_no)?;
        content.extend_from_slice(&ops);
        self.set_page_content(page_no, content)?;
        self.register_page_font(page_no, res_name.as_bytes(), (font_obj, 0))?;
        Ok(())
    }

    /// Like [`add_text`](Self::add_text) but **shaped**: GSUB standard ligatures
    /// and substitutions are applied (so `ffi` draws as one glyph) and GPOS pair
    /// kerning is emitted via a `TJ` array, so the drawn glyphs sit exactly where
    /// the HTML layout measured them. Falls back to a plain `Tj` run (identical to
    /// `add_text`) when the embedded face carries no `GSUB`/`GPOS`. Text still
    /// extracts/copies faithfully: ligature and astral-plane glyphs are reverse-
    /// mapped in the font's `/ToUnicode` at embed time.
    #[allow(clippy::too_many_arguments)]
    pub fn add_text_shaped(
        &mut self,
        page_no: u32,
        x: f64,
        y: f64,
        size: f64,
        text: &str,
        font_obj: u32,
        color: [f64; 3],
        opacity: f64,
        rotation_deg: f64,
    ) -> Result<()> {
        let ttf = self.embedded_truetype(font_obj).ok_or_else(|| {
            EngineError::Unsupported("font_obj is not an embedded TrueType font".into())
        })?;
        let shaper = crate::font::shape::Shaper::new(&ttf);
        if shaper.is_empty() {
            // No layout tables → identical to the plain run (and cheaper).
            return self.add_text(page_no, x, y, size, text, font_obj, color, opacity, rotation_deg);
        }
        let upm = ttf.units_per_em().max(1.0);
        let raw: Vec<u16> = text
            .chars()
            .map(|c| ttf.gid_for_unicode(c as u32).unwrap_or(0))
            .collect();
        let gids = shaper.substitute(&raw);

        // Build a TJ array: glyph hex strings separated by kern adjustments.
        // PDF TJ numbers are in thousandths of an em and SUBTRACT from the
        // advance (positive → move left), so a GPOS x-advance of `k` font units
        // becomes the TJ number `-(k · 1000 / upm)`.
        let used = self.font_used_gids.entry(font_obj).or_default();
        let mut tj = String::from("[<");
        for (i, &g) in gids.iter().enumerate() {
            used.insert(g);
            tj.push_str(&format!("{g:04X}"));
            if i + 1 < gids.len() {
                let k = shaper.kern(g, gids[i + 1]);
                if k != 0 {
                    let adj = -(k as f64) * 1000.0 / upm;
                    tj.push_str(&format!("> {} <", content::num(adj)));
                }
            }
        }
        tj.push_str(">] TJ");

        let res_name = format!("GF{font_obj}");
        let (sin, cos) = rotation_deg.to_radians().sin_cos();
        let inner = format!(
            "q\n{r} {g} {b} rg\nBT\n/{res} {size} Tf\n{ma} {mb} {mc} {md} {x} {y} Tm\n{tj}\nET\nQ\n",
            r = content::num(color[0]),
            g = content::num(color[1]),
            b = content::num(color[2]),
            res = res_name,
            size = content::num(size),
            ma = content::num(cos),
            mb = content::num(sin),
            mc = content::num(-sin),
            md = content::num(cos),
            x = content::num(x),
            y = content::num(y),
        )
        .into_bytes();
        let ops = self.with_opacity(page_no, inner, opacity)?;
        let mut content = self.page_content(page_no)?;
        content.extend_from_slice(&ops);
        self.set_page_content(page_no, content)?;
        self.register_page_font(page_no, res_name.as_bytes(), (font_obj, 0))?;
        Ok(())
    }

    /// The PostScript name of a base-14 standard font, or `None` if `name` is
    /// not one of the 14 (Helvetica/Times/Courier × 4 styles + Symbol +
    /// ZapfDingbats). These need no embedding — every PDF viewer ships them.
    pub fn standard_base14(name: &str) -> Option<&'static [u8]> {
        Some(match name {
            "Helvetica" => b"Helvetica",
            "Helvetica-Bold" => b"Helvetica-Bold",
            "Helvetica-Oblique" => b"Helvetica-Oblique",
            "Helvetica-BoldOblique" => b"Helvetica-BoldOblique",
            "Times-Roman" => b"Times-Roman",
            "Times-Bold" => b"Times-Bold",
            "Times-Italic" => b"Times-Italic",
            "Times-BoldItalic" => b"Times-BoldItalic",
            "Courier" => b"Courier",
            "Courier-Bold" => b"Courier-Bold",
            "Courier-Oblique" => b"Courier-Oblique",
            "Courier-BoldOblique" => b"Courier-BoldOblique",
            "Symbol" => b"Symbol",
            "ZapfDingbats" => b"ZapfDingbats",
            _ => return None,
        })
    }

    /// Draw `text` at `(x, y)` in a built-in **base-14 standard font**
    /// (`font_name`, e.g. `"Times-Bold"`) — no embedding needed. `size` pt,
    /// `color` (RGB 0–1), `opacity` (0–1), rotated `rotation_deg`° CCW. Text is
    /// WinAnsi-encoded. For arbitrary families embed a TrueType font and use
    /// [`add_text`](Self::add_text) instead.
    #[allow(clippy::too_many_arguments)]
    pub fn add_text_standard(
        &mut self,
        page_no: u32,
        x: f64,
        y: f64,
        size: f64,
        text: &str,
        font_name: &str,
        color: [f64; 3],
        opacity: f64,
        rotation_deg: f64,
    ) -> Result<()> {
        let base = Self::standard_base14(font_name)
            .ok_or_else(|| EngineError::Unsupported(format!("not a base-14 font: {font_name}")))?;
        let res_name = self.ensure_standard_font(page_no, base)?;
        let (sin, cos) = rotation_deg.to_radians().sin_cos();

        let mut inner: Vec<u8> = Vec::new();
        inner.extend_from_slice(b"q\n");
        inner.extend_from_slice(
            format!(
                "{} {} {} rg\nBT\n/",
                content::num(color[0]),
                content::num(color[1]),
                content::num(color[2]),
            )
            .as_bytes(),
        );
        inner.extend_from_slice(&res_name);
        inner.extend_from_slice(format!(" {} Tf\n", content::num(size)).as_bytes());
        // Text matrix carries the rotation: [cos sin -sin cos x y].
        inner.extend_from_slice(
            format!(
                "{} {} {} {} {} {} Tm\n",
                content::num(cos),
                content::num(sin),
                content::num(-sin),
                content::num(cos),
                content::num(x),
                content::num(y),
            )
            .as_bytes(),
        );
        inner.push(b'(');
        for &byte in &crate::font::encode_winansi(text) {
            if byte == b'(' || byte == b')' || byte == b'\\' {
                inner.push(b'\\');
            }
            inner.push(byte);
        }
        inner.extend_from_slice(b") Tj\nET\nQ\n");

        let ops = self.with_opacity(page_no, inner, opacity)?;
        let mut content = self.page_content(page_no)?;
        content.extend_from_slice(&ops);
        self.set_page_content(page_no, content)?;
        Ok(())
    }

    /// Stamp a watermark: `text` in standard **Helvetica** (no embed) at
    /// `(x, y)`, rotated `rotation_deg`° CCW, with `color` (RGB 0–1) and
    /// `opacity` (0–1). A thin wrapper over [`add_text_standard`](Self::add_text_standard).
    #[allow(clippy::too_many_arguments)]
    pub fn add_watermark(
        &mut self,
        page_no: u32,
        x: f64,
        y: f64,
        size: f64,
        text: &str,
        color: [f64; 3],
        opacity: f64,
        rotation_deg: f64,
    ) -> Result<()> {
        self.add_text_standard(page_no, x, y, size, text, "Helvetica", color, opacity, rotation_deg)
    }

    /// Add an invisible (text render mode 3) text layer to `page_no` in a SINGLE
    /// content-stream append. Built for OCR layers of many words: glyphs are
    /// selectable and searchable but never painted. Returns the number of runs
    /// actually written.
    ///
    /// Runs whose characters are all WinAnsi take the compact standard-Helvetica
    /// path (`(...) Tj`, one font resource). A run containing any non-WinAnsi
    /// character (Cyrillic, Greek, Arabic, CJK…) is written through an embedded
    /// **glyphless Type0 font** (empty outlines, `/Identity-H`) plus a
    /// `/ToUnicode` CMap — so multi-script OCR is searchable regardless of
    /// script. Only empty runs and characters outside the BMP are skipped.
    pub fn add_text_layer(&mut self, page_no: u32, runs: &[TextLayerRun]) -> Result<usize> {
        if runs.is_empty() {
            return Ok(0);
        }
        // Partition: WinAnsi-representable runs keep the compact path; the rest
        // need the Unicode (glyphless Type0) path. Empty runs are dropped either way.
        let mut winansi: Vec<&TextLayerRun> = Vec::new();
        let mut unicode: Vec<&TextLayerRun> = Vec::new();
        for run in runs {
            if run.text.is_empty() {
                continue;
            }
            if run
                .text
                .chars()
                .any(|c| crate::font::char_to_winansi(c).is_none())
            {
                unicode.push(run);
            } else {
                winansi.push(run);
            }
        }

        let mut written = self.add_winansi_text_runs(page_no, &winansi)?;
        written += self.add_unicode_text_runs(page_no, &unicode)?;
        Ok(written)
    }

    /// WinAnsi text-layer path: a single `3 Tr` block set in standard Helvetica,
    /// appended to the page content. Returns the number of runs written.
    fn add_winansi_text_runs(&mut self, page_no: u32, runs: &[&TextLayerRun]) -> Result<usize> {
        if runs.is_empty() {
            return Ok(0);
        }
        let res_name = self.ensure_helvetica_font(page_no)?;
        let mut inner: Vec<u8> = Vec::new();
        inner.extend_from_slice(b"q\nBT\n3 Tr\n");
        for run in runs {
            let (sin, cos) = run.rotation_deg.to_radians().sin_cos();
            inner.push(b'/');
            inner.extend_from_slice(&res_name);
            inner.extend_from_slice(format!(" {} Tf\n", content::num(run.size)).as_bytes());
            inner.extend_from_slice(
                format!(
                    "{} {} {} {} {} {} Tm\n",
                    content::num(cos),
                    content::num(sin),
                    content::num(-sin),
                    content::num(cos),
                    content::num(run.x),
                    content::num(run.y),
                )
                .as_bytes(),
            );
            inner.push(b'(');
            for &b in &crate::font::encode_winansi(&run.text) {
                if b == b'(' || b == b')' || b == b'\\' {
                    inner.push(b'\\');
                }
                inner.push(b);
            }
            inner.extend_from_slice(b") Tj\n");
        }
        inner.extend_from_slice(b"ET\nQ\n");
        let mut content = self.page_content(page_no)?;
        content.extend_from_slice(&inner);
        self.set_page_content(page_no, content)?;
        Ok(runs.len())
    }

    /// Unicode text-layer path: embed a glyphless Type0 font carrying one empty
    /// glyph per distinct (BMP) character of `runs`, with a `/ToUnicode` CMap so
    /// the invisible text is searchable for any script. Text is shown via
    /// `/Identity-H` (2-byte CIDs). Returns the number of runs written; a run
    /// is counted once at least one of its characters lands in the BMP.
    fn add_unicode_text_runs(&mut self, page_no: u32, runs: &[&TextLayerRun]) -> Result<usize> {
        if runs.is_empty() {
            return Ok(0);
        }
        // CID 0 is `.notdef`; assign CID 1.. to each distinct BMP character in a
        // stable order. Out-of-BMP characters (> U+FFFF) are skipped in v1.
        let mut cid_of: std::collections::HashMap<char, u16> = std::collections::HashMap::new();
        let mut chars: Vec<char> = Vec::new(); // chars[i] has CID i+1
        for run in runs {
            for ch in run.text.chars() {
                if (ch as u32) <= 0xFFFF {
                    cid_of.entry(ch).or_insert_with(|| {
                        chars.push(ch);
                        chars.len() as u16
                    });
                }
            }
        }
        if chars.is_empty() {
            return Ok(0);
        }

        // Build the encoded runs first; any run with no in-BMP character is dropped.
        let mut inner: Vec<u8> = Vec::new();
        inner.extend_from_slice(b"q\nBT\n3 Tr\n");
        let mut written = 0usize;
        // Resource name is resolved after the font object is allocated (below);
        // use a placeholder marker and patch the resource selects afterwards is
        // avoided by pre-computing the object number deterministically.
        let res_name = format!("GpU{}", self.next_object_number() + 3); // Type0 id = ff_id + 3
        for run in runs {
            let mut hex = String::new();
            for ch in run.text.chars() {
                if let Some(&cid) = cid_of.get(&ch) {
                    hex.push_str(&format!("{cid:04X}"));
                }
            }
            if hex.is_empty() {
                continue; // every character was out-of-BMP
            }
            let (sin, cos) = run.rotation_deg.to_radians().sin_cos();
            inner.extend_from_slice(
                format!(
                    "/{res_name} {sz} Tf\n{ma} {mb} {mc} {md} {x} {y} Tm\n<{hex}> Tj\n",
                    res_name = res_name,
                    sz = content::num(run.size),
                    ma = content::num(cos),
                    mb = content::num(sin),
                    mc = content::num(-sin),
                    md = content::num(cos),
                    x = content::num(run.x),
                    y = content::num(run.y),
                )
                .as_bytes(),
            );
            written += 1;
        }
        inner.extend_from_slice(b"ET\nQ\n");
        if written == 0 {
            return Ok(0);
        }

        // Embed the glyphless Type0 font (object graph mirrors `embed_cid_font`,
        // but with a uniform width and a ToUnicode CMap built from `chars`).
        let font_id = self.embed_glyphless_type0(chars.len() as u16, &chars)?;
        debug_assert_eq!(format!("GpU{}", font_id), res_name);
        self.register_page_font(page_no, res_name.as_bytes(), (font_id, 0))?;

        let mut content = self.page_content(page_no)?;
        content.extend_from_slice(&inner);
        self.set_page_content(page_no, content)?;
        Ok(written)
    }

    /// Build the glyphless Type0 object graph: `FontFile2` (empty `glyf`),
    /// `FontDescriptor`, `CIDFontType2` (`/Identity` `CIDToGIDMap`, uniform `/W`),
    /// `Type0` (`/Identity-H`) and a `/ToUnicode` CMap mapping each CID to its
    /// Unicode scalar. `chars[i]` is the character for CID `i + 1`; CID 0 is
    /// `.notdef`. Returns the Type0 font object number. Mirrors `embed_cid_font`.
    fn embed_glyphless_type0(&mut self, char_count: u16, chars: &[char]) -> Result<u32> {
        let num_glyphs = char_count + 1; // +1 for the .notdef glyph at CID 0
        let program = crate::font::glyphless::build_glyphless_ttf(num_glyphs);
        // CID i+1 → Unicode of chars[i]; CID 0 (.notdef) is left unmapped.
        let pairs: Vec<(u16, String)> = chars
            .iter()
            .enumerate()
            .map(|(i, &c)| (i as u16 + 1, c.to_string()))
            .collect();
        let tounicode = crate::font::embed::to_unicode_cmap(&pairs);

        // Five consecutive ids: FontFile, FontDescriptor, CIDFont, Type0, ToUnicode.
        let ff_id = (self.next_object_number(), 0u16);
        let fd_id = (ff_id.0 + 1, 0u16);
        let cid_id = (ff_id.0 + 2, 0u16);
        let t0_id = (ff_id.0 + 3, 0u16);
        let tu_id = (ff_id.0 + 4, 0u16);

        // FontFile2 — the glyphless TrueType program (compressed later by save).
        let mut ff = Dictionary::new();
        ff.set(b"Length".to_vec(), Object::Integer(program.len() as i64));
        ff.set(b"Length1".to_vec(), Object::Integer(program.len() as i64));
        self.objects
            .insert(ff_id, Object::Stream(Stream::new(ff, program)));

        // FontDescriptor — generic metrics; Symbolic since the glyphless face
        // carries no usable cmap (text is shown via Identity-H, not encoding).
        let mut fd = Dictionary::new();
        fd.set(b"Type", annot::name(b"FontDescriptor"));
        fd.set(b"FontName", annot::name(b"GigaPDFOCR"));
        fd.set(b"Flags", Object::Integer(4)); // Symbolic
        fd.set(
            b"FontBBox",
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(-200),
                Object::Integer(1000),
                Object::Integer(800),
            ]),
        );
        fd.set(b"ItalicAngle", Object::Integer(0));
        fd.set(b"Ascent", Object::Integer(800));
        fd.set(b"Descent", Object::Integer(-200));
        fd.set(b"CapHeight", Object::Integer(700));
        fd.set(b"StemV", Object::Integer(80));
        fd.set(b"FontFile2", Object::Reference(ff_id));
        self.objects.insert(fd_id, Object::Dictionary(fd));

        // Descendant CIDFontType2 — Identity ordering, Identity CIDToGIDMap,
        // uniform width (the glyphs are invisible so the exact value is cosmetic).
        let mut cidsi = Dictionary::new();
        cidsi.set(
            b"Registry",
            Object::String(b"Adobe".to_vec(), crate::object::StringKind::Literal),
        );
        cidsi.set(
            b"Ordering",
            Object::String(b"Identity".to_vec(), crate::object::StringKind::Literal),
        );
        cidsi.set(b"Supplement", Object::Integer(0));
        let mut cid = Dictionary::new();
        cid.set(b"Type", annot::name(b"Font"));
        cid.set(b"Subtype", annot::name(b"CIDFontType2"));
        cid.set(b"BaseFont", annot::name(b"GigaPDFOCR"));
        cid.set(b"CIDSystemInfo", Object::Dictionary(cidsi));
        cid.set(b"FontDescriptor", Object::Reference(fd_id));
        cid.set(b"CIDToGIDMap", annot::name(b"Identity"));
        cid.set(b"DW", Object::Integer(500));
        self.objects.insert(cid_id, Object::Dictionary(cid));

        // ToUnicode CMap — makes the invisible text searchable/copyable.
        let mut tu = Dictionary::new();
        tu.set(b"Length", Object::Integer(tounicode.len() as i64));
        self.objects
            .insert(tu_id, Object::Stream(Stream::new(tu, tounicode)));

        // Type0 wrapper.
        let mut t0 = Dictionary::new();
        t0.set(b"Type", annot::name(b"Font"));
        t0.set(b"Subtype", annot::name(b"Type0"));
        t0.set(b"BaseFont", annot::name(b"GigaPDFOCR"));
        t0.set(b"Encoding", annot::name(b"Identity-H"));
        t0.set(
            b"DescendantFonts",
            Object::Array(vec![Object::Reference(cid_id)]),
        );
        t0.set(b"ToUnicode", Object::Reference(tu_id));
        self.objects.insert(t0_id, Object::Dictionary(t0));

        Ok(t0_id.0)
    }

    /// Register a standard `/Type1 /Helvetica /WinAnsiEncoding` font as a page
    /// resource and return its resource name.
    fn ensure_helvetica_font(&mut self, page_no: u32) -> Result<Vec<u8>> {
        self.ensure_standard_font(page_no, b"Helvetica")
    }

    /// Register a standard (base-14) `/Type1` font with `base_font` as a page
    /// resource and return a *unique* resource name (so several different
    /// standard fonts can coexist on one page). Symbol/ZapfDingbats keep their
    /// built-in encoding; the others use `/WinAnsiEncoding`.
    fn ensure_standard_font(&mut self, page_no: u32, base_font: &[u8]) -> Result<Vec<u8>> {
        let id = (self.next_object_number(), 0u16);
        let mut f = Dictionary::new();
        f.set(b"Type".to_vec(), Object::Name(b"Font".to_vec()));
        f.set(b"Subtype".to_vec(), Object::Name(b"Type1".to_vec()));
        f.set(b"BaseFont".to_vec(), Object::Name(base_font.to_vec()));
        if base_font != b"Symbol" && base_font != b"ZapfDingbats" {
            f.set(
                b"Encoding".to_vec(),
                Object::Name(b"WinAnsiEncoding".to_vec()),
            );
        }
        self.objects.insert(id, Object::Dictionary(f));
        let res_name = format!("GpStd{}", id.0).into_bytes();
        self.register_page_font(page_no, &res_name, id)?;
        Ok(res_name)
    }

    /// Width of `text` set in standard Helvetica at `size` points (AFM metrics);
    /// lets a host position watermark/header text without embedding a font.
    pub fn helvetica_width(text: &str, size: f64) -> f64 {
        let units: u32 = text
            .chars()
            .map(|ch| {
                let c = ch as u32;
                if (0x20..=0x7E).contains(&c) {
                    HELVETICA_AFM[(c - 0x20) as usize] as u32
                } else {
                    556
                }
            })
            .sum();
        units as f64 * size / 1000.0
    }

    /// List the `/BaseFont` names that the document **references but does not
    /// embed** — the fonts a host would download (Google Fonts) and embed to make
    /// the document self-contained or editable. Deduplicated, sorted.
    pub fn needed_fonts(&self) -> Vec<String> {
        let mut needed = std::collections::BTreeSet::new();
        for page_no in 1..=self.page_count() as u32 {
            let resources = self.effective_resources(page_no);
            let Some(fonts) = resources
                .get(b"Font")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
            else {
                continue;
            };
            for value in fonts.0.values() {
                let Some(font) = self.resolve(value).as_dict() else {
                    continue;
                };
                if self.font_is_embedded(font) {
                    continue;
                }
                if let Some(base) = font.get(b"BaseFont").and_then(Object::as_name) {
                    // Strip a subset prefix ("ABCDEF+") for a clean family name.
                    let name = String::from_utf8_lossy(base);
                    let clean = name.split_once('+').map_or(name.as_ref(), |(_, n)| n);
                    needed.insert(clean.to_string());
                }
            }
        }
        needed.into_iter().collect()
    }

    /// Whether a font dictionary embeds its program (`FontFile`/`2`/`3`), looking
    /// through a Type0's descendant `FontDescriptor`.
    fn font_is_embedded(&self, font: &Dictionary) -> bool {
        let descriptor = if font.get(b"Subtype").and_then(Object::as_name) == Some(b"Type0") {
            font.get(b"DescendantFonts")
                .map(|o| self.resolve(o))
                .and_then(Object::as_array)
                .and_then(<[Object]>::first)
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
                .and_then(|cid| cid.get(b"FontDescriptor"))
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
        } else {
            font.get(b"FontDescriptor")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
        };
        descriptor.is_some_and(|fd| {
            fd.contains(b"FontFile") || fd.contains(b"FontFile2") || fd.contains(b"FontFile3")
        })
    }

    /// Parse the embedded TrueType program behind a Type0 font object, by walking
    /// Type0 → DescendantFonts → FontDescriptor → FontFile2.
    fn embedded_truetype(&self, font_obj: u32) -> Option<crate::font::truetype::TrueTypeFont> {
        let t0 = self.objects.get(&(font_obj, 0)).and_then(Object::as_dict)?;
        let desc = t0
            .get(b"DescendantFonts")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)?
            .first()?;
        let cid = self.resolve(desc).as_dict()?;
        let fd = cid
            .get(b"FontDescriptor")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        // glyf TrueType lives in FontFile2; CFF/OpenType in FontFile3. Read either
        // so add_text / replace_text resolve the char→GID map for *any* embedded
        // face (FontFile3 outlines aren't needed — only its cmap/metrics).
        if let Some(ff) = fd
            .get(b"FontFile2")
            .map(|o| self.resolve(o))
            .and_then(Object::as_stream)
        {
            let bytes = decode_stream(ff).ok()?;
            return crate::font::truetype::TrueTypeFont::parse(&bytes);
        }
        let ff = fd
            .get(b"FontFile3")
            .map(|o| self.resolve(o))
            .and_then(Object::as_stream)?;
        let bytes = decode_stream(ff).ok()?;
        crate::font::truetype::TrueTypeFont::parse_metrics(&bytes)
    }

    /// Subset every embedded Type0 font's `FontFile2` to the glyphs actually
    /// drawn (tracked in [`font_used_gids`](Self::font_used_gids)), shrinking the
    /// saved file. Operates on a (cloned) objects map and drops the stream
    /// `/Filter` so a later compression pass re-flates the smaller program. A
    /// no-op for fonts with no recorded use or when subsetting wouldn't shrink.
    fn subset_embedded_fonts(&self, objects: &mut BTreeMap<ObjectId, Object>) {
        for (&font_obj, used) in &self.font_used_gids {
            if used.is_empty() {
                continue;
            }
            let Some(ff_id) = Self::fontfile2_id(objects, font_obj) else {
                continue;
            };
            let decoded = match objects.get(&ff_id) {
                Some(Object::Stream(s)) => match decode_stream(s) {
                    Ok(b) => b,
                    Err(_) => continue,
                },
                _ => continue,
            };
            let Some(ttf) = crate::font::truetype::TrueTypeFont::parse(&decoded) else {
                continue;
            };
            let Some(sub) = ttf.subset(used) else {
                continue;
            };
            if sub.len() >= decoded.len() {
                continue; // subsetting would not shrink — keep the original
            }
            if let Some(Object::Stream(s)) = objects.get_mut(&ff_id) {
                s.dict.remove(b"Filter");
                s.dict
                    .set(b"Length".to_vec(), Object::Integer(sub.len() as i64));
                s.dict
                    .set(b"Length1".to_vec(), Object::Integer(sub.len() as i64));
                s.raw = sub;
            }
        }
    }

    /// The `FontFile2` stream object id for an embedded Type0 font, resolved
    /// within `objects` (Type0 → DescendantFonts[0] → FontDescriptor → FontFile2).
    fn fontfile2_id(objects: &BTreeMap<ObjectId, Object>, font_obj: u32) -> Option<ObjectId> {
        fn deref<'a>(objects: &'a BTreeMap<ObjectId, Object>, o: &'a Object) -> Option<&'a Object> {
            match o {
                Object::Reference(id) => objects.get(id),
                other => Some(other),
            }
        }
        let t0 = objects.get(&(font_obj, 0))?.as_dict()?;
        let df = deref(objects, t0.get(b"DescendantFonts")?)?.as_array()?;
        let cid = deref(objects, df.first()?)?.as_dict()?;
        let fd = deref(objects, cid.get(b"FontDescriptor")?)?.as_dict()?;
        match fd.get(b"FontFile2")? {
            Object::Reference(id) => Some(*id),
            _ => None,
        }
    }

    /// The nearest `/Resources` dictionary up the page tree (own or inherited),
    /// cloned so the caller can mutate and re-attach it to the page.
    fn effective_resources(&self, page_no: u32) -> Dictionary {
        let Ok(page_id) = self.page_object_id(page_no) else {
            return Dictionary::new();
        };
        let mut current = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .cloned();
        while let Some(dict) = current {
            if let Some(res) = dict
                .get(b"Resources")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
            {
                return res.clone();
            }
            current = dict
                .get(b"Parent")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
                .cloned();
        }
        Dictionary::new()
    }

    /// Register `name -> font_ref` in a page's `/Resources /Font`, preserving any
    /// inherited resources by materializing them onto the page first.
    fn register_page_font(&mut self, page_no: u32, name: &[u8], font_ref: ObjectId) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let mut resources = self.effective_resources(page_no);
        let mut fonts = resources
            .get(b"Font")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        fonts.set(name.to_vec(), Object::Reference(font_ref));
        resources.set(b"Font".to_vec(), Object::Dictionary(fonts));
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        page.set(b"Resources".to_vec(), Object::Dictionary(resources));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    /// Add an Underline annotation under a text rectangle.
    pub fn add_underline(&mut self, page_no: u32, rect: [f64; 4], color: [f64; 3]) -> Result<()> {
        self.add_annotation(page_no, annot::underline(rect, color))
    }

    /// Add a StrikeOut annotation through a text rectangle.
    pub fn add_strike_out(&mut self, page_no: u32, rect: [f64; 4], color: [f64; 3]) -> Result<()> {
        self.add_annotation(page_no, annot::strike_out(rect, color))
    }

    /// Add an Ink (freehand) annotation from one or more polylines (each a list
    /// of `(x, y)` points in page user space).
    pub fn add_ink(
        &mut self,
        page_no: u32,
        paths: &[Vec<(f64, f64)>],
        color: [f64; 3],
        line_width: f64,
    ) -> Result<()> {
        self.add_annotation(page_no, annot::ink(paths, color, line_width))
    }

    /// Add a rubber-stamp annotation (a labelled, bordered box).
    pub fn add_stamp(
        &mut self,
        page_no: u32,
        rect: [f64; 4],
        label: &str,
        color: [f64; 3],
    ) -> Result<()> {
        self.add_annotation(page_no, annot::stamp(rect, label, color))
    }

    /// The form-XObject id of an annotation's normal appearance (`/AP /N`),
    /// resolving an appearance-state sub-dictionary via `/AS` when present.
    fn annotation_appearance_id(&self, dict: &Dictionary) -> Option<ObjectId> {
        let normal = dict
            .get(b"AP")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?
            .get(b"N")?;
        if let Some(id) = normal.as_reference() {
            if self.objects.get(&id).and_then(Object::as_stream).is_some() {
                return Some(id);
            }
        }
        let states = self.resolve(normal).as_dict()?;
        if let Some(key) = dict.get(b"AS").and_then(Object::as_name) {
            if let Some(id) = states.get(key).and_then(Object::as_reference) {
                return Some(id);
            }
        }
        states.0.values().find_map(Object::as_reference)
    }

    /// "Flatten" a page's annotations: paint each annotation's appearance into
    /// the page content as an XObject, then drop the `/Annots` markup. Returns
    /// how many annotations were baked. Annotations without an appearance are
    /// left untouched (and the markup is kept if any couldn't be baked).
    pub fn flatten_annotations(&mut self, page_no: u32) -> Result<usize> {
        let page_id = self.page_object_id(page_no)?;
        let page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        let annots = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => return Ok(0),
        };

        let mut forms: Vec<ObjectId> = Vec::new();
        let mut all_bakeable = true;
        for item in &annots {
            match self.resolve(item).as_dict() {
                Some(dict) => match self.annotation_appearance_id(dict) {
                    Some(id) => forms.push(id),
                    None => all_bakeable = false,
                },
                None => all_bakeable = false,
            }
        }
        if forms.is_empty() {
            return Ok(0);
        }

        // A content stream that draws every appearance form, named uniquely.
        let mut draw = Vec::new();
        let mut xobjects = Dictionary::new();
        for (i, form_id) in forms.iter().enumerate() {
            let resource_name = format!("GpFlat{i}");
            xobjects.set(
                resource_name.clone().into_bytes(),
                Object::Reference(*form_id),
            );
            draw.extend_from_slice(format!("q /{resource_name} Do Q\n").as_bytes());
        }
        let draw_id = (self.next_object_number(), 0u16);
        let mut draw_dict = Dictionary::new();
        draw_dict.set(b"Length".to_vec(), Object::Integer(draw.len() as i64));
        self.objects
            .insert(draw_id, Object::Stream(Stream::new(draw_dict, draw)));

        // Re-fetch and edit the page: append the draw stream to /Contents,
        // merge the XObject resources, and drop the baked annotations.
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();

        let mut contents = match page.get(b"Contents").map(|o| self.resolve(o)) {
            Some(Object::Array(items)) => items.clone(),
            Some(_) => vec![page.get(b"Contents").cloned().unwrap()],
            None => Vec::new(),
        };
        contents.push(Object::Reference(draw_id));
        page.set(b"Contents".to_vec(), Object::Array(contents));

        let mut resources = page
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        let mut xobject_dict = resources
            .get(b"XObject")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        for (key, value) in xobjects.0 {
            xobject_dict.0.insert(key, value);
        }
        resources.set(b"XObject".to_vec(), Object::Dictionary(xobject_dict));
        page.set(b"Resources".to_vec(), Object::Dictionary(resources));

        // Only drop the markup if every annotation was baked; otherwise keep the
        // un-bakeable ones rather than silently losing them.
        if all_bakeable {
            page.remove(b"Annots");
        }
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(forms.len())
    }

    /// Flatten the interactive form: bake every field widget's appearance into
    /// its page content (across all pages) and drop `/AcroForm` so the document
    /// is no longer fillable — nor enumerable as a form (`fields()` returns
    /// empty afterwards). Returns the number of widgets baked. A no-op
    /// (returns 0) when there is no AcroForm.
    pub fn flatten_form(&mut self) -> Result<usize> {
        let has_form = self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"AcroForm"))
            .is_some();
        if !has_form {
            return Ok(0);
        }

        // Bake widget appearances into the page content, page by page.
        let mut baked = 0;
        for page_no in 1..=self.page_count() as u32 {
            baked += self.flatten_annotations(page_no)?;
        }

        // Drop the interactive layer: the values are now painted into the
        // pages, so remove `/AcroForm` (fields, /DR, /NeedAppearances) entirely.
        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        catalog.remove(b"AcroForm");
        self.objects.insert(catalog_id, Object::Dictionary(catalog));

        Ok(baked)
    }

    /// Inline every `/Subtype /Form` XObject invoked via `Do` on `page_no`
    /// **into the page's content stream**, so the form's text/graphics become
    /// ordinary page content. Each `Do` invocation gets its own inlined copy
    /// (the form is **de-shared**): editing the run of one placement no longer
    /// affects another. Image XObjects (`/Subtype /Image`) and unresolvable
    /// `Do`s are left untouched.
    ///
    /// After this, [`page_text_elements`](Self::page_text_elements) returns the
    /// former form text as normal page runs with **real, editable indices** (no
    /// form sentinel), so [`replace_text_run`](Self::replace_text_run),
    /// [`move_element`](Self::move_element) and
    /// [`remove_element`](Self::remove_element) work on them — the enabler for
    /// in-place invoice/template editing.
    ///
    /// Mechanics: each `name Do` is replaced by `q [cm] <inlined form content> Q`,
    /// where `cm` reproduces the form's `/Matrix` when non-identity. The inlined
    /// content references resources by NAME; those names are resolved against the
    /// page's resources, copying each resource the form uses into the page (under
    /// the same name when free / pointing at the same object, otherwise under a
    /// fresh `{name}_fx{n}` name with the matching operand rewritten). A form
    /// without `/Resources` inherits the page's, so its names already resolve and
    /// no merge/rename happens. Nested forms (a form whose content has its own
    /// `Do`) are inlined recursively, reusing the depth cap + visited-ref cycle
    /// guard so self/mutually-referencing forms terminate.
    ///
    /// The fully inlined form entries are dropped from the page's
    /// `/Resources /XObject` (they are now unreferenced); forms that were only
    /// partially reachable (e.g. cut off by the depth cap) are kept so nothing is
    /// silently lost. Returns the number of form XObjects inlined (counting every
    /// `Do` invocation, since each is de-shared).
    pub fn flatten_form_xobjects(&mut self, page_no: u32) -> Result<usize> {
        let page_id = self.page_object_id(page_no)?;
        let content = self.page_content(page_no)?;
        let operations = content::parse_content(&content)?;

        // Working copy of the page's resource sub-dictionaries, into which we
        // merge (and possibly rename) the resources used by inlined forms.
        let page_res = self
            .page_dict(page_no)?
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        let mut work = FlattenResources::from_dict(self, &page_res);

        // Names of XObject forms fully inlined (to drop from /Resources /XObject).
        let mut inlined_forms: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut count = 0usize;
        let mut visited: BTreeSet<ObjectId> = BTreeSet::new();

        // The top-level ops resolve names against the page's own resources, so no
        // rename is applied to them (identity rename map).
        let out = self.inline_form_ops(
            &operations,
            &page_res,
            &Renames::identity(),
            &mut work,
            &mut inlined_forms,
            &mut visited,
            &mut count,
            0,
        );

        if count == 0 {
            return Ok(0); // nothing to inline — leave the page byte-identical
        }

        let new_content = content::encode_content(&out);
        self.set_page_content(page_no, new_content)?;

        // Write the merged resources back onto the page, dropping the XObject
        // entries that were fully inlined (now unreferenced).
        let mut resources = work.into_dict();
        if !inlined_forms.is_empty() {
            if let Some(Object::Dictionary(xobj)) = resources.0.get_mut(b"XObject".as_slice()) {
                for name in &inlined_forms {
                    xobj.0.remove(name);
                }
                if xobj.is_empty() {
                    resources.remove(b"XObject");
                }
            }
        }
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        page.set(b"Resources".to_vec(), Object::Dictionary(resources));
        self.objects.insert(page_id, Object::Dictionary(page));

        Ok(count)
    }

    /// Recursive worker for [`flatten_form_xobjects`](Self::flatten_form_xobjects).
    ///
    /// Rewrites `operations` (which resolve resource names against `scope_res`)
    /// into a flat operation list, applying `renames` to this scope's
    /// resource-name operands and splicing each form `Do` as `q [cm] … Q`.
    /// `work` accumulates the merged page resources; `inlined_forms` collects the
    /// names of fully inlined top-level forms; `visited` is the per-path set of
    /// form object-refs (cycle guard); `count` tallies inlined invocations.
    #[allow(clippy::too_many_arguments)]
    fn inline_form_ops(
        &self,
        operations: &[content::Operation],
        scope_res: &Dictionary,
        renames: &Renames,
        work: &mut FlattenResources,
        inlined_forms: &mut BTreeSet<Vec<u8>>,
        visited: &mut BTreeSet<ObjectId>,
        count: &mut usize,
        depth: usize,
    ) -> Vec<content::Operation> {
        let mut out = Vec::with_capacity(operations.len());
        for op in operations {
            // A form `Do` we can resolve (and haven't hit the depth/cycle guard)
            // is inlined; everything else is emitted with its names rewritten.
            if op.operator == b"Do" && depth < content::MAX_FORM_DEPTH {
                if let Some(name) = op.operands.iter().find_map(Object::as_name) {
                    // The name as it appears in the page (post-rename), then the
                    // form object behind it (looked up against this scope).
                    if let Some(form) = self.resolve_form_in(scope_res, name) {
                        let on_path = form.id.is_some_and(|id| visited.contains(&id));
                        if !on_path {
                            *count += 1;
                            if depth == 0 {
                                inlined_forms.insert(name.to_vec());
                            }
                            out.extend(self.inline_one_form(
                                &form,
                                scope_res,
                                work,
                                inlined_forms,
                                visited,
                                count,
                                depth,
                            ));
                            continue;
                        }
                        // On the current path (cycle): drop the self-reference,
                        // matching the extractor's expand-at-most-once behaviour.
                        continue;
                    }
                }
            }
            // Non-inlined op: rewrite its resource-name operands per `renames`.
            out.push(renames.apply(op));
        }
        out
    }

    /// Inline a single resolved form: emit `q [cm] <body> Q`, where `<body>` is
    /// the form's content with its resources merged into the page (renaming on
    /// collision) and nested `Do`s recursively inlined.
    #[allow(clippy::too_many_arguments)]
    fn inline_one_form(
        &self,
        form: &ResolvedForm,
        parent_scope: &Dictionary,
        work: &mut FlattenResources,
        inlined_forms: &mut BTreeSet<Vec<u8>>,
        visited: &mut BTreeSet<ObjectId>,
        count: &mut usize,
        depth: usize,
    ) -> Vec<content::Operation> {
        let mut out = Vec::new();
        out.push(op0(b"q"));
        // Reproduce the form `/Matrix` (default identity ⇒ no `cm`).
        let m = form.matrix.0;
        if m != [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] {
            out.push(content::Operation {
                operator: b"cm".to_vec(),
                operands: m.iter().map(|&v| Object::Real(v)).collect(),
            });
        }

        // A form with its own `/Resources` resolves names against them: merge
        // those into the page and build the rename map. A form WITHOUT
        // `/Resources` inherits the parent scope — its names already resolve
        // there, so we inline its body against `parent_scope` with no renames.
        let body_ops = content::parse_content(&form.content).unwrap_or_default();
        match &form.resources {
            Some(form_res) => {
                let renames = work.merge_scope(self, form_res);
                let pushed = form.id.map(|id| visited.insert(id)).unwrap_or(false);
                out.extend(self.inline_form_ops(
                    &body_ops,
                    form_res,
                    &renames,
                    work,
                    inlined_forms,
                    visited,
                    count,
                    depth + 1,
                ));
                if pushed {
                    if let Some(id) = form.id {
                        visited.remove(&id);
                    }
                }
            }
            None => {
                let pushed = form.id.map(|id| visited.insert(id)).unwrap_or(false);
                out.extend(self.inline_form_ops(
                    &body_ops,
                    parent_scope,
                    &Renames::identity(),
                    work,
                    inlined_forms,
                    visited,
                    count,
                    depth + 1,
                ));
                if pushed {
                    if let Some(id) = form.id {
                        visited.remove(&id);
                    }
                }
            }
        }

        out.push(op0(b"Q"));
        out
    }

    /// Resolve XObject resource `name` against `scope_res /XObject` to a
    /// [`ResolvedForm`] when it is a `/Subtype /Form`; `None` for images,
    /// non-form XObjects, or undecodable streams.
    fn resolve_form_in(&self, scope_res: &Dictionary, name: &[u8]) -> Option<ResolvedForm> {
        let value = scope_res
            .get(b"XObject")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|x| x.get(name))?;
        let id = value.as_reference();
        let stream = self.resolve(value).as_stream()?;
        if stream.dict.get(b"Subtype").and_then(Object::as_name) != Some(b"Form".as_slice()) {
            return None;
        }
        let content = decode_stream(stream).ok()?;
        let resources = stream
            .dict
            .get(b"Resources")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned();
        Some(ResolvedForm {
            content,
            resources,
            matrix: self.form_matrix(&stream.dict),
            id,
        })
    }

    // ─── page operations & metadata ──────────────────────────────────────────

    /// Set a page's rotation (normalized to 0/90/180/270 degrees).
    pub fn rotate_page(&mut self, page_no: u32, degrees: i32) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let normalized = (degrees.rem_euclid(360) / 90) * 90;
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        page.set(b"Rotate".to_vec(), Object::Integer(normalized as i64));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    /// The /Pages tree node whose /Kids contains `child`, if any.
    fn find_kids_parent(&self, child: ObjectId) -> Option<ObjectId> {
        for (id, object) in &self.objects {
            if let Some(kids) = object
                .as_dict()
                .and_then(|d| d.get(b"Kids"))
                .and_then(Object::as_array)
            {
                if kids.iter().any(|o| o.as_reference() == Some(child)) {
                    return Some(*id);
                }
            }
        }
        None
    }

    /// Decrement /Count on `node` and all its /Parent ancestors.
    fn decrement_count(&mut self, start: ObjectId) {
        let mut node = start;
        for _ in 0..64 {
            let mut dict = match self.objects.get(&node).and_then(Object::as_dict) {
                Some(d) => d.clone(),
                None => break,
            };
            let count = dict.get(b"Count").and_then(Object::as_i64).unwrap_or(0);
            dict.set(b"Count".to_vec(), Object::Integer((count - 1).max(0)));
            let parent = dict.get(b"Parent").and_then(Object::as_reference);
            self.objects.insert(node, Object::Dictionary(dict));
            match parent {
                Some(p) => node = p,
                None => break,
            }
        }
    }

    /// Delete a page (cannot delete the last remaining page).
    pub fn delete_page(&mut self, page_no: u32) -> Result<()> {
        if self.page_count() <= 1 {
            return Err(EngineError::Unsupported(
                "cannot delete the only page".into(),
            ));
        }
        let page_id = self.page_object_id(page_no)?;
        let parent_id = self
            .find_kids_parent(page_id)
            .ok_or_else(|| EngineError::Missing("page tree parent".into()))?;

        let mut parent = self
            .objects
            .get(&parent_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("page tree parent".into()))?
            .clone();
        if let Some(kids) = parent.get(b"Kids").and_then(Object::as_array) {
            let remaining: Vec<Object> = kids
                .iter()
                .filter(|o| o.as_reference() != Some(page_id))
                .cloned()
                .collect();
            parent.set(b"Kids".to_vec(), Object::Array(remaining));
        }
        self.objects.insert(parent_id, Object::Dictionary(parent));
        self.decrement_count(parent_id);
        Ok(())
    }

    /// Rebuild the page tree as a single flat `/Pages` node with `ordered` pages.
    fn rebuild_page_tree(&mut self, ordered: &[ObjectId]) -> Result<()> {
        let root_id = self
            .catalog()?
            .get(b"Pages")
            .and_then(Object::as_reference)
            .ok_or_else(|| EngineError::Missing("catalog /Pages".into()))?;

        let mut root = self
            .objects
            .get(&root_id)
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        root.set(b"Type".to_vec(), Object::Name(b"Pages".to_vec()));
        root.set(
            b"Kids".to_vec(),
            Object::Array(ordered.iter().map(|id| Object::Reference(*id)).collect()),
        );
        root.set(b"Count".to_vec(), Object::Integer(ordered.len() as i64));
        root.remove(b"Parent");
        self.objects.insert(root_id, Object::Dictionary(root));

        for id in ordered {
            if let Some(mut page) = self.objects.get(id).and_then(Object::as_dict).cloned() {
                page.set(b"Parent".to_vec(), Object::Reference(root_id));
                self.objects.insert(*id, Object::Dictionary(page));
            }
        }
        Ok(())
    }

    /// Move a page from 1-based position `from` to 1-based position `to`.
    pub fn move_page(&mut self, from: u32, to: u32) -> Result<()> {
        let mut ids = self.page_ids()?;
        let len = ids.len();
        let from = from.saturating_sub(1) as usize;
        let to = to.saturating_sub(1) as usize;
        if from >= len || to >= len {
            return Err(EngineError::PageNotFound((from.max(to) + 1) as u32));
        }
        let id = ids.remove(from);
        ids.insert(to.min(ids.len()), id);
        self.rebuild_page_tree(&ids)
    }

    /// Drop every object not reachable from the trailer's `/Root` or `/Info`.
    fn gc(&mut self) {
        let mut reachable: BTreeSet<ObjectId> = BTreeSet::new();
        let mut stack: Vec<ObjectId> = Vec::new();
        for key in [b"Root".as_slice(), b"Info".as_slice()] {
            if let Some(id) = self.trailer.get(key).and_then(Object::as_reference) {
                stack.push(id);
            }
        }
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            if let Some(object) = self.objects.get(&id) {
                collect_refs(object, &mut stack);
            }
        }
        self.objects.retain(|id, _| reachable.contains(id));
    }

    /// Produce a new PDF containing only the given 1-based pages, in that order.
    ///
    /// The extracted chunk is **self-contained**: references that point at pages
    /// left behind (GoTo links, AcroForm fields, named destinations, outline
    /// dests) are neutralised or dropped so no orphaned page survives the gc.
    pub fn extract_pages(&self, pages: &[u32]) -> Result<Vec<u8>> {
        let all = self.page_ids()?;
        let selected: Vec<ObjectId> = pages
            .iter()
            .filter_map(|&p| all.get(p.saturating_sub(1) as usize).copied())
            .collect();
        if selected.is_empty() {
            return Err(EngineError::PageNotFound(0));
        }
        let kept: BTreeSet<ObjectId> = selected.iter().copied().collect();
        let dropped: BTreeSet<ObjectId> =
            all.iter().copied().filter(|id| !kept.contains(id)).collect();
        let mut clone = self.clone();
        clone.rebuild_page_tree(&selected)?;
        if !dropped.is_empty() {
            clone.prune_cross_page_references(&kept, &dropped);
        }
        clone.gc();
        Ok(clone.save())
    }

    /// The page object id an explicit destination array points at (its first
    /// element), if that element is a page reference.
    fn explicit_dest_target(&self, dest: &Object) -> Option<ObjectId> {
        self.resolve(dest)
            .as_array()
            .and_then(<[Object]>::first)
            .and_then(Object::as_reference)
    }

    /// The page object id a GoTo annotation/outline dict targets — from its
    /// `/Dest` array or its `/A` `GoTo` `/D` array — when it is an explicit page
    /// reference (named-string destinations resolve via `/Dests`, pruned apart).
    fn goto_target_page(&self, dict: &Dictionary) -> Option<ObjectId> {
        if let Some(dest) = dict.get(b"Dest") {
            if let Some(id) = self.explicit_dest_target(dest) {
                return Some(id);
            }
        }
        let action = dict
            .get(b"A")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        if action.get(b"S").and_then(Object::as_name) == Some(b"GoTo".as_slice()) {
            if let Some(d) = action.get(b"D") {
                return self.explicit_dest_target(d);
            }
        }
        None
    }

    /// Remove references to pages outside the extracted set so the chunk is
    /// self-contained. Called by [`Self::extract_pages`] before `gc`.
    fn prune_cross_page_references(
        &mut self,
        kept: &BTreeSet<ObjectId>,
        dropped: &BTreeSet<ObjectId>,
    ) {
        let widget_page = self.build_widget_page_map(kept, dropped);
        self.neutralise_cross_page_links(kept, dropped);
        self.prune_acroform_fields(dropped, &widget_page);
        self.prune_named_dests(dropped);
        self.prune_outline_dests(dropped);
    }

    /// Map each page annotation/widget id to the page object id whose `/Annots`
    /// contains it, across all original pages (kept ∪ dropped). Lets us locate a
    /// form widget's page even when its `/P` back-pointer is absent.
    fn build_widget_page_map(
        &self,
        kept: &BTreeSet<ObjectId>,
        dropped: &BTreeSet<ObjectId>,
    ) -> BTreeMap<ObjectId, ObjectId> {
        let mut map = BTreeMap::new();
        for page_id in kept.iter().chain(dropped.iter()) {
            let annots: Vec<ObjectId> = self
                .objects
                .get(page_id)
                .and_then(Object::as_dict)
                .and_then(|d| d.get(b"Annots"))
                .map(|o| self.resolve(o))
                .and_then(Object::as_array)
                .map(|arr| arr.iter().filter_map(Object::as_reference).collect())
                .unwrap_or_default();
            for annot in annots {
                map.entry(annot).or_insert(*page_id);
            }
        }
        map
    }

    /// Whether a form widget sits on a dropped page — by `/Annots` membership
    /// first (covers widgets with no `/P`), then by its `/P` back-pointer.
    fn widget_on_dropped(
        &self,
        widget_id: ObjectId,
        dropped: &BTreeSet<ObjectId>,
        widget_page: &BTreeMap<ObjectId, ObjectId>,
    ) -> bool {
        if let Some(page_id) = widget_page.get(&widget_id) {
            return dropped.contains(page_id);
        }
        self.objects
            .get(&widget_id)
            .and_then(Object::as_dict)
            .and_then(|d| d.get(b"P"))
            .and_then(Object::as_reference)
            .is_some_and(|p| dropped.contains(&p))
    }

    /// On every kept page, strip the `/A`/`/Dest` of Link annotations whose
    /// GoTo target is a dropped page (the annotation stays, but goes inert).
    fn neutralise_cross_page_links(
        &mut self,
        kept: &BTreeSet<ObjectId>,
        dropped: &BTreeSet<ObjectId>,
    ) {
        for &page_id in kept {
            let annot_ids: Vec<ObjectId> = self
                .objects
                .get(&page_id)
                .and_then(Object::as_dict)
                .and_then(|d| d.get(b"Annots"))
                .map(|o| self.resolve(o))
                .and_then(Object::as_array)
                .map(|arr| arr.iter().filter_map(Object::as_reference).collect())
                .unwrap_or_default();
            for annot_id in annot_ids {
                let Some(dict) = self.objects.get(&annot_id).and_then(Object::as_dict).cloned()
                else {
                    continue;
                };
                if self
                    .goto_target_page(&dict)
                    .is_some_and(|t| dropped.contains(&t))
                {
                    let mut updated = dict;
                    updated.remove(b"A");
                    updated.remove(b"Dest");
                    self.objects.insert(annot_id, Object::Dictionary(updated));
                }
            }
        }
    }

    /// Drop AcroForm fields whose every widget sits on a dropped page; for a
    /// multi-widget field, drop only its on-dropped-page widget kids.
    fn prune_acroform_fields(
        &mut self,
        dropped: &BTreeSet<ObjectId>,
        widget_page: &BTreeMap<ObjectId, ObjectId>,
    ) {
        let Ok(catalog_id) = self.catalog_id() else {
            return;
        };
        let Some(catalog) = self.objects.get(&catalog_id).and_then(Object::as_dict).cloned() else {
            return;
        };
        let Some(acro_obj) = catalog.get(b"AcroForm") else {
            return;
        };
        // `/AcroForm` is stored inline in the catalog here, but may be an indirect
        // reference in third-party PDFs — handle both.
        let (acro_id, acro) = match acro_obj {
            Object::Reference(id) => match self.objects.get(id).and_then(Object::as_dict).cloned() {
                Some(d) => (Some(*id), d),
                None => return,
            },
            other => match other.as_dict() {
                Some(d) => (None, d.clone()),
                None => return,
            },
        };
        let fields = acro
            .get(b"Fields")
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        let mut kept_fields: Vec<Object> = Vec::with_capacity(fields.len());
        for field in &fields {
            match field.as_reference() {
                Some(fid) if !self.retain_field(fid, dropped, widget_page) => {}
                _ => kept_fields.push(field.clone()),
            }
        }
        if kept_fields.len() == fields.len() {
            return;
        }
        let mut acro = acro;
        acro.set(b"Fields".to_vec(), Object::Array(kept_fields));
        match acro_id {
            Some(id) => {
                self.objects.insert(id, Object::Dictionary(acro));
            }
            None => {
                let mut catalog = catalog;
                catalog.set(b"AcroForm".to_vec(), Object::Dictionary(acro));
                self.objects.insert(catalog_id, Object::Dictionary(catalog));
            }
        }
    }

    /// Whether an AcroForm field keeps at least one widget on a kept page,
    /// pruning its on-dropped-page widget kids in place. `true` = keep.
    fn retain_field(
        &mut self,
        field_id: ObjectId,
        dropped: &BTreeSet<ObjectId>,
        widget_page: &BTreeMap<ObjectId, ObjectId>,
    ) -> bool {
        let Some(dict) = self.objects.get(&field_id).and_then(Object::as_dict).cloned() else {
            return true;
        };
        if let Some(kids) = dict.get(b"Kids").and_then(Object::as_array) {
            let kids = kids.to_vec();
            let mut surviving: Vec<Object> = Vec::with_capacity(kids.len());
            for kid in &kids {
                let on_dropped = kid
                    .as_reference()
                    .is_some_and(|kid_id| self.widget_on_dropped(kid_id, dropped, widget_page));
                if !on_dropped {
                    surviving.push(kid.clone());
                }
            }
            if surviving.is_empty() {
                return false;
            }
            if surviving.len() != kids.len() {
                let mut updated = dict;
                updated.set(b"Kids".to_vec(), Object::Array(surviving));
                self.objects.insert(field_id, Object::Dictionary(updated));
            }
            return true;
        }
        // Merged field/widget: the field dict is itself the widget.
        !self.widget_on_dropped(field_id, dropped, widget_page)
    }

    /// Remove catalog `/Dests` named destinations that target a dropped page.
    fn prune_named_dests(&mut self, dropped: &BTreeSet<ObjectId>) {
        let Ok(catalog_id) = self.catalog_id() else {
            return;
        };
        let Some(catalog) = self.objects.get(&catalog_id).and_then(Object::as_dict).cloned() else {
            return;
        };
        let Some(dests_obj) = catalog.get(b"Dests") else {
            return;
        };
        // `/Dests` is usually an indirect reference, occasionally an inline dict.
        let (dict_id, mut dict) = match dests_obj {
            Object::Reference(id) => match self.objects.get(id).and_then(Object::as_dict).cloned() {
                Some(d) => (Some(*id), d),
                None => return,
            },
            other => match other.as_dict() {
                Some(d) => (None, d.clone()),
                None => return,
            },
        };
        let names: Vec<Vec<u8>> = dict.0.keys().cloned().collect();
        let mut changed = false;
        for name in names {
            let drop_entry = dict
                .get(&name)
                .and_then(|val| self.dest_value_target(val))
                .is_some_and(|t| dropped.contains(&t));
            if drop_entry {
                dict.remove(&name);
                changed = true;
            }
        }
        if !changed {
            return;
        }
        match dict_id {
            Some(id) => {
                self.objects.insert(id, Object::Dictionary(dict));
            }
            None => {
                let mut catalog = catalog;
                catalog.set(b"Dests".to_vec(), Object::Dictionary(dict));
                self.objects.insert(catalog_id, Object::Dictionary(catalog));
            }
        }
    }

    /// The page id a named-destination value (an array `[pageRef …]` or a dict
    /// `{ /D [pageRef …] }`) resolves to, if it is an explicit page reference.
    fn dest_value_target(&self, val: &Object) -> Option<ObjectId> {
        let resolved = self.resolve(val);
        if let Some(arr) = resolved.as_array() {
            return arr.first().and_then(Object::as_reference);
        }
        if let Some(inner) = resolved.as_dict().and_then(|d| d.get(b"D")) {
            return self.explicit_dest_target(inner);
        }
        None
    }

    /// Strip the `/Dest`/`/A` of outline (bookmark) items whose GoTo target is a
    /// dropped page, so the outline keeps no orphaned page alive.
    fn prune_outline_dests(&mut self, dropped: &BTreeSet<ObjectId>) {
        let Some(outlines_id) = self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Outlines"))
            .and_then(Object::as_reference)
        else {
            return;
        };
        let mut ids: Vec<ObjectId> = Vec::new();
        let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
        let mut stack: Vec<ObjectId> = self
            .objects
            .get(&outlines_id)
            .and_then(Object::as_dict)
            .and_then(|d| d.get(b"First"))
            .and_then(Object::as_reference)
            .into_iter()
            .collect();
        let mut guard = 0;
        while let Some(id) = stack.pop() {
            guard += 1;
            if guard > 100_000 || !seen.insert(id) {
                continue;
            }
            ids.push(id);
            if let Some(d) = self.objects.get(&id).and_then(Object::as_dict) {
                if let Some(child) = d.get(b"First").and_then(Object::as_reference) {
                    stack.push(child);
                }
                if let Some(next) = d.get(b"Next").and_then(Object::as_reference) {
                    stack.push(next);
                }
            }
        }
        for id in ids {
            let Some(dict) = self.objects.get(&id).and_then(Object::as_dict).cloned() else {
                continue;
            };
            if self
                .goto_target_page(&dict)
                .is_some_and(|t| dropped.contains(&t))
            {
                let mut updated = dict;
                updated.remove(b"Dest");
                updated.remove(b"A");
                self.objects.insert(id, Object::Dictionary(updated));
            }
        }
    }

    /// Append all pages of another PDF to the end of this document.
    pub fn append_pages_from(&mut self, other_pdf: &[u8]) -> Result<()> {
        let other = Document::open(other_pdf)?;
        let other_pages = other.page_ids()?;

        // Objects reachable from the other document's pages.
        let mut reachable: Vec<ObjectId> = Vec::new();
        let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
        let mut stack = other_pages.clone();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(object) = other.objects.get(&id) {
                reachable.push(id);
                collect_refs(object, &mut stack);
            }
        }

        // Allocate fresh ids in this document and copy + remap.
        let mut next = self.next_object_number();
        let mut map: BTreeMap<ObjectId, ObjectId> = BTreeMap::new();
        for &id in &reachable {
            map.insert(id, (next, 0));
            next += 1;
        }
        for &id in &reachable {
            if let Some(object) = other.objects.get(&id) {
                self.objects.insert(map[&id], remap_object(object, &map));
            }
        }

        // Attach the new pages under this document's root.
        let root_id = self
            .catalog()?
            .get(b"Pages")
            .and_then(Object::as_reference)
            .ok_or_else(|| EngineError::Missing("catalog /Pages".into()))?;
        let mut root = self
            .objects
            .get(&root_id)
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        let mut kids = root
            .get(b"Kids")
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        let count = root
            .get(b"Count")
            .and_then(Object::as_i64)
            .unwrap_or(kids.len() as i64);

        for &page in &other_pages {
            let new_page = map[&page];
            kids.push(Object::Reference(new_page));
            if let Some(mut page_dict) = self
                .objects
                .get(&new_page)
                .and_then(Object::as_dict)
                .cloned()
            {
                page_dict.set(b"Parent".to_vec(), Object::Reference(root_id));
                self.objects.insert(new_page, Object::Dictionary(page_dict));
            }
        }
        root.set(b"Kids".to_vec(), Object::Array(kids));
        root.set(
            b"Count".to_vec(),
            Object::Integer(count + other_pages.len() as i64),
        );
        self.objects.insert(root_id, Object::Dictionary(root));
        Ok(())
    }

    /// The document's `/Info` dictionary id, creating it if absent.
    fn info_dict_id(&mut self) -> ObjectId {
        if let Some(id) = self.trailer.get(b"Info").and_then(Object::as_reference) {
            return id;
        }
        let id = (self.next_object_number(), 0u16);
        self.objects
            .insert(id, Object::Dictionary(Dictionary::new()));
        self.trailer.set(b"Info".to_vec(), Object::Reference(id));
        id
    }

    /// Set a document metadata entry (e.g. "Title", "Author", "Subject",
    /// "Keywords", "Creator", "Producer").
    pub fn set_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        let id = self.info_dict_id();
        let mut info = self
            .objects
            .get(&id)
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        info.set(
            key.as_bytes().to_vec(),
            Object::String(crate::font::encode_pdf_text(value), StringKind::Literal),
        );
        self.objects.insert(id, Object::Dictionary(info));
        Ok(())
    }

    /// Read a document metadata entry.
    pub fn get_metadata(&self, key: &str) -> Option<String> {
        let info = self.trailer.get(b"Info").map(|o| self.resolve(o))?;
        match info.as_dict()?.get(key.as_bytes()).map(|o| self.resolve(o)) {
            Some(Object::String(bytes, _)) => Some(crate::font::decode_pdf_text(bytes)),
            _ => None,
        }
    }

    // ─── form field creation (AcroForm, ISO 32000-1 §12.7) ───────────────────

    /// Ensure the catalog has an `/AcroForm` carrying a Helvetica in
    /// `/DR /Font /Helv`, a default `/DA`, and `NeedAppearances true`. Returns
    /// the Helvetica font object id (every widget's appearance resources point
    /// at it). Re-uses an existing `/Helv` if the document already has one.
    fn ensure_acroform(&mut self, default_da: &Object) -> Result<ObjectId> {
        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        let mut acro = catalog
            .get(b"AcroForm")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();

        let existing = acro
            .get(b"DR")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|dr| dr.get(b"Font").map(|o| self.resolve(o)))
            .and_then(Object::as_dict)
            .and_then(|fonts| fonts.get(b"Helv").and_then(Object::as_reference));
        let helv_id = match existing {
            Some(id) => id,
            None => {
                let id = (self.next_object_number(), 0u16);
                let mut f = Dictionary::new();
                f.set(b"Type", annot::name(b"Font"));
                f.set(b"Subtype", annot::name(b"Type1"));
                f.set(b"BaseFont", annot::name(b"Helvetica"));
                f.set(b"Encoding", annot::name(b"WinAnsiEncoding"));
                self.objects.insert(id, Object::Dictionary(f));

                let mut fonts = acro
                    .get(b"DR")
                    .map(|o| self.resolve(o))
                    .and_then(Object::as_dict)
                    .and_then(|dr| dr.get(b"Font").map(|o| self.resolve(o)))
                    .and_then(Object::as_dict)
                    .cloned()
                    .unwrap_or_default();
                fonts.set(b"Helv", Object::Reference(id));
                let mut dr = acro
                    .get(b"DR")
                    .map(|o| self.resolve(o))
                    .and_then(Object::as_dict)
                    .cloned()
                    .unwrap_or_default();
                dr.set(b"Font", Object::Dictionary(fonts));
                acro.set(b"DR", Object::Dictionary(dr));
                id
            }
        };

        if acro.get(b"DA").is_none() {
            acro.set(b"DA", default_da.clone());
        }
        acro.set(b"NeedAppearances", Object::Boolean(true));
        if acro.get(b"Fields").is_none() {
            acro.set(b"Fields", Object::Array(Vec::new()));
        }
        catalog.set(b"AcroForm", Object::Dictionary(acro));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(helv_id)
    }

    /// Allocate a Form XObject holding `content`, sized `[0 0 w h]`, with a
    /// `/Helv` font resource; returns its object id.
    fn make_form_xobject(
        &mut self,
        content: Vec<u8>,
        w: f64,
        h: f64,
        helv_id: ObjectId,
    ) -> ObjectId {
        let id = (self.next_object_number(), 0u16);
        let mut fonts = Dictionary::new();
        fonts.set(b"Helv", Object::Reference(helv_id));
        let mut resources = Dictionary::new();
        resources.set(b"Font", Object::Dictionary(fonts));
        let mut d = Dictionary::new();
        d.set(b"Type", annot::name(b"XObject"));
        d.set(b"Subtype", annot::name(b"Form"));
        d.set(b"FormType", Object::Integer(1));
        d.set(b"BBox", annot::real_array(&[0.0, 0.0, w, h]));
        d.set(b"Resources", Object::Dictionary(resources));
        d.set(b"Length", Object::Integer(content.len() as i64));
        self.objects
            .insert(id, Object::Stream(Stream::new(d, content)));
        id
    }

    /// Append an annotation/widget reference to a page's `/Annots`.
    fn append_to_page_annots(&mut self, page_id: ObjectId, annot_id: ObjectId) {
        if let Some(mut page) = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .cloned()
        {
            let mut annots = page
                .get(b"Annots")
                .map(|o| self.resolve(o))
                .and_then(Object::as_array)
                .map(<[Object]>::to_vec)
                .unwrap_or_default();
            annots.push(Object::Reference(annot_id));
            page.set(b"Annots", Object::Array(annots));
            self.objects.insert(page_id, Object::Dictionary(page));
        }
    }

    /// Append a field reference to the AcroForm `/Fields`.
    fn register_in_acroform(&mut self, field_id: ObjectId) -> Result<()> {
        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .cloned()
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?;
        let mut acro = catalog
            .get(b"AcroForm")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_default();
        let mut fields = acro
            .get(b"Fields")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        fields.push(Object::Reference(field_id));
        acro.set(b"Fields", Object::Array(fields));
        catalog.set(b"AcroForm", Object::Dictionary(acro));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(())
    }

    /// Register a terminal field: page `/Annots` + AcroForm `/Fields`.
    fn register_field(&mut self, page_id: ObjectId, field_id: ObjectId) -> Result<()> {
        self.append_to_page_annots(page_id, field_id);
        self.register_in_acroform(field_id)
    }

    /// Add a single- or multi-line **text field** to `page` (1-based) covering
    /// `rect` = `[x0, y0, x1, y1]` (PDF user space).
    #[allow(clippy::too_many_arguments)]
    pub fn add_text_field(
        &mut self,
        page: u32,
        name: &str,
        rect: [f64; 4],
        value: &str,
        max_len: Option<u32>,
        multiline: bool,
        password: bool,
        style: &form::FieldStyle,
    ) -> Result<()> {
        let da = form::da_string(style);
        let helv_id = self.ensure_acroform(&da)?;
        let page_id = self.page_object_id(page)?;
        let (w, h) = ((rect[2] - rect[0]).abs(), (rect[3] - rect[1]).abs());

        let mut ff = 0u32;
        if multiline {
            ff |= form::flags::MULTILINE;
        }
        if password {
            ff |= form::flags::PASSWORD;
        }

        let mut field = Dictionary::new();
        field.set(b"FT", annot::name(b"Tx"));
        field.set(b"T", pdf_text(name));
        field.set(b"V", pdf_text(value));
        field.set(b"DA", da);
        if ff != 0 {
            field.set(b"Ff", Object::Integer(i64::from(ff)));
        }
        if let Some(ml) = max_len {
            field.set(b"MaxLen", Object::Integer(i64::from(ml)));
        }
        if let Some(mk) = form::mk_dict(style) {
            field.set(b"MK", Object::Dictionary(mk));
        }

        let ap_id =
            self.make_form_xobject(form::text_appearance(value, style, w, h), w, h, helv_id);
        let mut ap = Dictionary::new();
        ap.set(b"N", Object::Reference(ap_id));

        field.set(b"Type", annot::name(b"Annot"));
        field.set(b"Subtype", annot::name(b"Widget"));
        field.set(b"Rect", annot::real_array(&rect));
        field.set(b"F", Object::Integer(4)); // Print
        field.set(b"P", Object::Reference(page_id));
        field.set(b"AP", Object::Dictionary(ap));

        let field_id = (self.next_object_number(), 0u16);
        self.objects.insert(field_id, Object::Dictionary(field));
        self.register_field(page_id, field_id)
    }

    /// Add a **checkbox** to `page`. `export` is the "on" state name (defaults
    /// to `On`); `checked` sets the initial state.
    pub fn add_checkbox(
        &mut self,
        page: u32,
        name: &str,
        rect: [f64; 4],
        checked: bool,
        export: &str,
        style: &form::FieldStyle,
    ) -> Result<()> {
        let da = form::da_string(style);
        let helv_id = self.ensure_acroform(&da)?;
        let page_id = self.page_object_id(page)?;
        let (w, h) = ((rect[2] - rect[0]).abs(), (rect[3] - rect[1]).abs());
        let on = if export.is_empty() { "On" } else { export };

        let on_id = self.make_form_xobject(form::check_appearance(style, w, h), w, h, helv_id);
        let off_id = self.make_form_xobject(form::empty_appearance(style, w, h), w, h, helv_id);
        let mut n = Dictionary::new();
        n.set(on.as_bytes().to_vec(), Object::Reference(on_id));
        n.set(b"Off", Object::Reference(off_id));
        let mut ap = Dictionary::new();
        ap.set(b"N", Object::Dictionary(n));

        let state = if checked { on } else { "Off" };
        let mut field = Dictionary::new();
        field.set(b"FT", annot::name(b"Btn"));
        field.set(b"T", pdf_text(name));
        field.set(b"V", annot::name(state.as_bytes()));
        field.set(b"AS", annot::name(state.as_bytes()));
        if let Some(mk) = form::mk_dict(style) {
            field.set(b"MK", Object::Dictionary(mk));
        }
        field.set(b"Type", annot::name(b"Annot"));
        field.set(b"Subtype", annot::name(b"Widget"));
        field.set(b"Rect", annot::real_array(&rect));
        field.set(b"F", Object::Integer(4));
        field.set(b"P", Object::Reference(page_id));
        field.set(b"AP", Object::Dictionary(ap));

        let field_id = (self.next_object_number(), 0u16);
        self.objects.insert(field_id, Object::Dictionary(field));
        self.register_field(page_id, field_id)
    }

    /// Add a **radio-button group**: one logical field (`/Ff Radio`) whose
    /// `/Kids` are the individual buttons. Each option is `(export_name, rect)`;
    /// `selected` is the initially-chosen export name.
    pub fn add_radio_group(
        &mut self,
        page: u32,
        name: &str,
        options: &[(String, [f64; 4])],
        selected: Option<&str>,
        style: &form::FieldStyle,
    ) -> Result<()> {
        let da = form::da_string(style);
        let helv_id = self.ensure_acroform(&da)?;
        let page_id = self.page_object_id(page)?;

        // Reserve the parent id first so the kids can point at it via /Parent.
        let parent_id = (self.next_object_number(), 0u16);
        self.objects.insert(parent_id, Object::Null);

        let mut kids: Vec<Object> = Vec::with_capacity(options.len());
        for (export, rect) in options {
            let (w, h) = ((rect[2] - rect[0]).abs(), (rect[3] - rect[1]).abs());
            let on_id = self.make_form_xobject(form::radio_appearance(style, w, h), w, h, helv_id);
            let off_id = self.make_form_xobject(form::empty_appearance(style, w, h), w, h, helv_id);
            let mut n = Dictionary::new();
            n.set(export.as_bytes().to_vec(), Object::Reference(on_id));
            n.set(b"Off", Object::Reference(off_id));
            let mut ap = Dictionary::new();
            ap.set(b"N", Object::Dictionary(n));

            let state: &str = if selected == Some(export.as_str()) {
                export
            } else {
                "Off"
            };
            let mut kid = Dictionary::new();
            kid.set(b"Type", annot::name(b"Annot"));
            kid.set(b"Subtype", annot::name(b"Widget"));
            kid.set(b"Rect", annot::real_array(rect));
            kid.set(b"F", Object::Integer(4));
            kid.set(b"P", Object::Reference(page_id));
            kid.set(b"Parent", Object::Reference(parent_id));
            kid.set(b"AS", annot::name(state.as_bytes()));
            if let Some(mk) = form::mk_dict(style) {
                kid.set(b"MK", Object::Dictionary(mk));
            }
            kid.set(b"AP", Object::Dictionary(ap));

            let kid_id = (self.next_object_number(), 0u16);
            self.objects.insert(kid_id, Object::Dictionary(kid));
            kids.push(Object::Reference(kid_id));
            self.append_to_page_annots(page_id, kid_id);
        }

        let mut parent = Dictionary::new();
        parent.set(b"FT", annot::name(b"Btn"));
        parent.set(b"Ff", Object::Integer(i64::from(form::flags::RADIO)));
        parent.set(b"T", pdf_text(name));
        parent.set(b"V", annot::name(selected.unwrap_or("Off").as_bytes()));
        parent.set(b"DA", da);
        parent.set(b"Kids", Object::Array(kids));
        self.objects.insert(parent_id, Object::Dictionary(parent));

        self.register_in_acroform(parent_id)
    }

    /// Add a drop-down **combo box**. `editable` lets the user type a value
    /// outside `options`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_combo_box(
        &mut self,
        page: u32,
        name: &str,
        rect: [f64; 4],
        options: &[String],
        selected: Option<&str>,
        editable: bool,
        style: &form::FieldStyle,
    ) -> Result<()> {
        self.add_choice_field(
            page, name, rect, options, selected, true, editable, false, style,
        )
    }

    /// Add a scrolling **list box**. `multi` allows selecting several options.
    #[allow(clippy::too_many_arguments)]
    pub fn add_list_box(
        &mut self,
        page: u32,
        name: &str,
        rect: [f64; 4],
        options: &[String],
        selected: Option<&str>,
        multi: bool,
        style: &form::FieldStyle,
    ) -> Result<()> {
        self.add_choice_field(
            page, name, rect, options, selected, false, false, multi, style,
        )
    }

    /// Shared implementation for combo boxes and list boxes (both `/FT Ch`).
    #[allow(clippy::too_many_arguments)]
    fn add_choice_field(
        &mut self,
        page: u32,
        name: &str,
        rect: [f64; 4],
        options: &[String],
        selected: Option<&str>,
        combo: bool,
        editable: bool,
        multi: bool,
        style: &form::FieldStyle,
    ) -> Result<()> {
        let da = form::da_string(style);
        let helv_id = self.ensure_acroform(&da)?;
        let page_id = self.page_object_id(page)?;
        let (w, h) = ((rect[2] - rect[0]).abs(), (rect[3] - rect[1]).abs());

        let mut ff = 0u32;
        if combo {
            ff |= form::flags::COMBO;
        }
        if editable {
            ff |= form::flags::EDIT;
        }
        if multi {
            ff |= form::flags::MULTI_SELECT;
        }

        let opt = Object::Array(options.iter().map(|o| pdf_text(o)).collect());
        let value = selected.unwrap_or("");

        let mut field = Dictionary::new();
        field.set(b"FT", annot::name(b"Ch"));
        field.set(b"T", pdf_text(name));
        field.set(b"Opt", opt);
        if ff != 0 {
            field.set(b"Ff", Object::Integer(i64::from(ff)));
        }
        field.set(b"V", pdf_text(value));
        field.set(b"DA", da);
        if let Some(mk) = form::mk_dict(style) {
            field.set(b"MK", Object::Dictionary(mk));
        }

        let ap_id =
            self.make_form_xobject(form::text_appearance(value, style, w, h), w, h, helv_id);
        let mut ap = Dictionary::new();
        ap.set(b"N", Object::Reference(ap_id));

        field.set(b"Type", annot::name(b"Annot"));
        field.set(b"Subtype", annot::name(b"Widget"));
        field.set(b"Rect", annot::real_array(&rect));
        field.set(b"F", Object::Integer(4));
        field.set(b"P", Object::Reference(page_id));
        field.set(b"AP", Object::Dictionary(ap));

        let field_id = (self.next_object_number(), 0u16);
        self.objects.insert(field_id, Object::Dictionary(field));
        self.register_field(page_id, field_id)
    }

    // ─── destinations, hyperlinks & outline ──────────────────────────────────

    /// Object id of the document catalog (the `/Root`).
    fn catalog_id(&self) -> Result<ObjectId> {
        if let Some(id) = self.trailer.get(b"Root").and_then(Object::as_reference) {
            return Ok(id);
        }
        // Fallback: the id of any /Type /Catalog object.
        self.objects
            .iter()
            .find(|(_, obj)| {
                obj.as_dict()
                    .and_then(|d| d.get(b"Type"))
                    .and_then(Object::as_name)
                    == Some(b"Catalog".as_slice())
            })
            .map(|(id, _)| *id)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))
    }

    /// 1-based page number of a page object id, if it is a page.
    fn page_number_of(&self, target: ObjectId) -> Option<u32> {
        self.page_ids()
            .ok()?
            .iter()
            .position(|id| *id == target)
            .map(|i| i as u32 + 1)
    }

    /// Resolve a named destination (catalog `/Dests` dict or `/Names /Dests`
    /// name tree, top level) to its destination object.
    fn lookup_named_dest(&self, key: &[u8]) -> Option<Object> {
        let catalog = self.catalog().ok()?;
        // PDF 1.1 style: catalog /Dests is a dictionary of name -> dest.
        if let Some(dests) = catalog
            .get(b"Dests")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
        {
            if let Some(entry) = dests.get(key) {
                let resolved = self.resolve(entry);
                // A named dest may wrap its array in a /D dictionary entry.
                if let Some(d) = resolved.as_dict().and_then(|d| d.get(b"D")) {
                    return Some(self.resolve(d).clone());
                }
                return Some(resolved.clone());
            }
        }
        // PDF 1.2+ style: catalog /Names /Dests is a name tree.
        let names = catalog
            .get(b"Names")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?
            .get(b"Dests")
            .map(|o| self.resolve(o))?;
        self.search_name_tree(names, key, 0)
    }

    /// Walk a name tree looking for `key`, returning the associated value.
    fn search_name_tree(&self, node: &Object, key: &[u8], depth: usize) -> Option<Object> {
        if depth > 32 {
            return None;
        }
        let dict = self.resolve(node).as_dict()?;
        if let Some(names) = dict
            .get(b"Names")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            let mut i = 0;
            while i + 1 < names.len() {
                if let Object::String(bytes, _) = self.resolve(&names[i]) {
                    if bytes.as_slice() == key {
                        let value = self.resolve(&names[i + 1]);
                        if let Some(d) = value.as_dict().and_then(|d| d.get(b"D")) {
                            return Some(self.resolve(d).clone());
                        }
                        return Some(value.clone());
                    }
                }
                i += 2;
            }
        }
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for kid in kids {
                if let Some(found) = self.search_name_tree(kid, key, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Resolve a destination object (`[pageRef …]` array, or a named dest as a
    /// name/string) to a 1-based page number.
    fn dest_to_page(&self, dest: &Object) -> Option<u32> {
        match self.resolve(dest) {
            Object::Array(items) => {
                let page_id = items.first()?.as_reference()?;
                self.page_number_of(page_id)
            }
            Object::Name(name) => {
                let target = self.lookup_named_dest(name)?;
                self.dest_to_page(&target)
            }
            Object::String(bytes, _) => {
                let target = self.lookup_named_dest(bytes)?;
                self.dest_to_page(&target)
            }
            _ => None,
        }
    }

    /// Destination page of an annotation/outline dict, from `/Dest` or a
    /// `/A << /S /GoTo /D … >>` action.
    fn dest_page_of(&self, dict: &Dictionary) -> Option<u32> {
        if let Some(dest) = dict.get(b"Dest") {
            if let Some(page) = self.dest_to_page(dest) {
                return Some(page);
            }
        }
        let action = dict
            .get(b"A")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        if action.get(b"S").and_then(Object::as_name) == Some(b"GoTo".as_slice()) {
            if let Some(d) = action.get(b"D") {
                return self.dest_to_page(d);
            }
        }
        None
    }

    /// Resolve a destination to its explicit `[pageRef, /Fit, …]` array, chasing
    /// a name/string through the name tree (or inline `/Dests`) and unwrapping a
    /// `<< /D […] >>` dictionary.
    fn resolve_dest_array(&self, dest: &Object) -> Option<Vec<Object>> {
        match self.resolve(dest) {
            Object::Array(items) => Some(items.clone()),
            Object::Name(name) => {
                let target = self.lookup_named_dest(name)?;
                self.resolve_dest_array(&target)
            }
            Object::String(bytes, _) => {
                let target = self.lookup_named_dest(bytes)?;
                self.resolve_dest_array(&target)
            }
            Object::Dictionary(d) => {
                let inner = d.get(b"D")?.clone();
                self.resolve_dest_array(&inner)
            }
            _ => None,
        }
    }

    /// Resolve an outline/annotation dict's destination to
    /// `(page, kind, x, y, zoom)`: `kind` is the lowercased fit type
    /// (`"xyz"`/`"fit"`/`"fith"`/…); for `/XYZ`, `x`/`y` are the top-left point
    /// and `zoom` the magnification (a `null` operand yields `None`).
    fn dest_detail_of(
        &self,
        dict: &Dictionary,
    ) -> (Option<u32>, String, Option<f64>, Option<f64>, Option<f64>) {
        let dest_obj = dict.get(b"Dest").cloned().or_else(|| {
            dict.get(b"A")
                .map(|o| self.resolve(o))
                .and_then(Object::as_dict)
                .filter(|a| a.get(b"S").and_then(Object::as_name) == Some(b"GoTo".as_slice()))
                .and_then(|a| a.get(b"D").cloned())
        });
        let Some(arr) = dest_obj.as_ref().and_then(|d| self.resolve_dest_array(d)) else {
            return (None, String::new(), None, None, None);
        };
        let page = arr
            .first()
            .and_then(Object::as_reference)
            .and_then(|id| self.page_number_of(id));
        let kind = arr
            .get(1)
            .map(|o| self.resolve(o))
            .and_then(Object::as_name)
            .map(|n| String::from_utf8_lossy(n).to_lowercase())
            .unwrap_or_default();
        let num = |i: usize| {
            arr.get(i).and_then(|o| match self.resolve(o) {
                Object::Null => None,
                r => r.as_f64(),
            })
        };
        let (x, y, zoom) = if kind == "xyz" {
            (num(2), num(3), num(4))
        } else {
            (None, None, None)
        };
        (page, kind, x, y, zoom)
    }

    /// List a page's hyperlink annotations.
    pub fn page_links(&self, page_no: u32) -> Result<Vec<Link>> {
        let page = self.page_dict(page_no)?;
        let items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let mut out = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let Some(dict) = self.resolve(item).as_dict() else {
                continue;
            };
            if dict.get(b"Subtype").and_then(Object::as_name) != Some(b"Link".as_slice()) {
                continue;
            }
            let rect = self.read_rect(dict);
            let target = self.link_target(dict);
            out.push(Link {
                index,
                rect,
                target,
            });
        }
        Ok(out)
    }

    fn link_target(&self, dict: &Dictionary) -> LinkTarget {
        if let Some(action) = dict
            .get(b"A")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
        {
            if action.get(b"S").and_then(Object::as_name) == Some(b"URI".as_slice()) {
                if let Some(Object::String(bytes, _)) = action.get(b"URI").map(|o| self.resolve(o))
                {
                    return LinkTarget::Uri(String::from_utf8_lossy(bytes).into_owned());
                }
            }
        }
        match self.dest_page_of(dict) {
            Some(page) => LinkTarget::Page(page),
            None => LinkTarget::Unknown,
        }
    }

    /// Append a ready-made annotation dictionary to a page's `/Annots`.
    fn append_annotation_dict(&mut self, page_no: u32, dict: Dictionary) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;
        let annotation_id = (self.next_object_number(), 0u16);
        self.objects.insert(annotation_id, Object::Dictionary(dict));
        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        let mut items = match page.get(b"Annots") {
            Some(obj) => self
                .resolve(obj)
                .as_array()
                .map(<[Object]>::to_vec)
                .unwrap_or_default(),
            None => Vec::new(),
        };
        items.push(Object::Reference(annotation_id));
        page.set(b"Annots".to_vec(), Object::Array(items));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    fn base_link_dict(rect: [f64; 4]) -> Dictionary {
        let mut dict = Dictionary::new();
        dict.set(b"Type".to_vec(), annot::name(b"Annot"));
        dict.set(b"Subtype".to_vec(), annot::name(b"Link"));
        dict.set(b"Rect".to_vec(), annot::real_array(&rect));
        // A zero-width border so the link has no visible outline.
        dict.set(
            b"Border".to_vec(),
            Object::Array(vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(0),
            ]),
        );
        dict
    }

    /// Add a hyperlink to an external URI over `rect`.
    pub fn add_uri_link(&mut self, page_no: u32, rect: [f64; 4], uri: &str) -> Result<()> {
        let mut dict = Self::base_link_dict(rect);
        let mut action = Dictionary::new();
        action.set(b"Type".to_vec(), annot::name(b"Action"));
        action.set(b"S".to_vec(), annot::name(b"URI"));
        action.set(
            b"URI".to_vec(),
            Object::String(uri.as_bytes().to_vec(), StringKind::Literal),
        );
        dict.set(b"A".to_vec(), Object::Dictionary(action));
        self.append_annotation_dict(page_no, dict)
    }

    /// Add an internal hyperlink over `rect` that jumps to `target_page`.
    pub fn add_goto_link(&mut self, page_no: u32, rect: [f64; 4], target_page: u32) -> Result<()> {
        let target_id = self.page_object_id(target_page)?;
        let mut dict = Self::base_link_dict(rect);
        dict.set(
            b"Dest".to_vec(),
            Object::Array(vec![Object::Reference(target_id), annot::name(b"Fit")]),
        );
        self.append_annotation_dict(page_no, dict)
    }

    /// Register a **named destination** `name` → `target_page` (a whole-page
    /// `/Fit` view) in the catalog's `/Dests` dictionary, creating it if needed.
    /// Links and outline items can then jump by name via
    /// [`add_goto_link_named`](Self::add_goto_link_named); because resolution
    /// goes through the catalog (not a frozen page number), the anchor survives
    /// page extraction/split as long as its page is kept. Re-using a `name`
    /// overwrites its target.
    pub fn add_named_dest(&mut self, name: &str, target_page: u32) -> Result<()> {
        let target_id = self.page_object_id(target_page)?;
        let catalog_id = self.catalog_id()?;
        let dest = Object::Array(vec![Object::Reference(target_id), annot::name(b"Fit")]);

        // `/Dests` may be an indirect reference (mutate that object) or live
        // inline in the catalog (mutate the catalog). Create it inline if absent.
        let dests_ref = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .and_then(|c| c.get(b"Dests"))
            .and_then(Object::as_reference);
        if let Some(id) = dests_ref {
            let mut dict = self
                .objects
                .get(&id)
                .and_then(Object::as_dict)
                .cloned()
                .unwrap_or_else(Dictionary::new);
            dict.set(name.as_bytes().to_vec(), dest);
            self.objects.insert(id, Object::Dictionary(dict));
            return Ok(());
        }
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("catalog".into()))?
            .clone();
        let mut dict = catalog
            .get(b"Dests")
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(Dictionary::new);
        dict.set(name.as_bytes().to_vec(), dest);
        catalog.set(b"Dests".to_vec(), Object::Dictionary(dict));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(())
    }

    /// Every named destination in the catalog's `/Dests` dictionary as
    /// `(name, 1-based page)` pairs (entries that don't resolve to a page are
    /// skipped). The PDF 1.2+ name-tree form (`/Names /Dests`) is honoured by
    /// link/outline resolution but not enumerated here.
    pub fn named_dests(&self) -> Vec<(String, u32)> {
        let mut out = Vec::new();

        // Legacy inline `/Dests` dictionary (PDF 1.1).
        if let Some(dests) = self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Dests").map(|o| self.resolve(o)))
            .and_then(|o| o.as_dict().cloned())
        {
            for (name, value) in &dests.0 {
                if let Some(page) = self.dest_to_page(value) {
                    out.push((String::from_utf8_lossy(name).into_owned(), page));
                }
            }
        }

        // PDF 1.2+ `/Names /Dests` name tree. A tree value may be a dest array
        // directly or a `<< /D [dest] >>` wrapper. Enumerated here (not just
        // resolved on demand) so the list matches a reader's `getDestinations()`.
        if let Some(root) = self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Names").map(|o| self.resolve(o)))
            .and_then(Object::as_dict)
            .and_then(|n| n.get(b"Dests").map(|o| self.resolve(o).clone()))
        {
            let mut pairs = Vec::new();
            self.collect_name_tree(&root, 0, &mut pairs);
            for (key, value) in pairs {
                let page = match value.as_dict().and_then(|d| d.get(b"D")) {
                    Some(d) => self.dest_to_page(d),
                    None => self.dest_to_page(&value),
                };
                if let Some(page) = page {
                    out.push((String::from_utf8_lossy(&key).into_owned(), page));
                }
            }
        }

        out
    }

    /// Every embedded file attachment in the document's `/Names /EmbeddedFiles`
    /// name tree (ISO 32000-1 §7.7.4 / §7.11.4), decoded. Each [`Attachment`]
    /// carries the name-tree key, the filespec's `/UF`/`/F` display name, the
    /// embedded stream's `/Subtype` (MIME) and `/Params` dates, and the decoded
    /// bytes. Entries that don't resolve to a readable embedded stream are
    /// skipped, so the result only contains extractable files.
    pub fn attachments(&self) -> Vec<Attachment> {
        let Some(root) = self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Names").map(|o| self.resolve(o)))
            .and_then(Object::as_dict)
            .and_then(|n| n.get(b"EmbeddedFiles").map(|o| self.resolve(o).clone()))
        else {
            return Vec::new();
        };
        let mut pairs = Vec::new();
        self.collect_name_tree(&root, 0, &mut pairs);
        pairs
            .iter()
            .filter_map(|(key, value)| self.filespec_to_attachment(key, value))
            .collect()
    }

    /// Collect every `(key, value)` pair in a name tree — the enumerate-all
    /// counterpart of [`search_name_tree`](Self::search_name_tree).
    fn collect_name_tree(&self, node: &Object, depth: usize, out: &mut Vec<(Vec<u8>, Object)>) {
        if depth > 32 {
            return; // defend against a cyclic /Kids chain
        }
        let Some(dict) = self.resolve(node).as_dict() else {
            return;
        };
        if let Some(names) = dict
            .get(b"Names")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            let mut i = 0;
            while i + 1 < names.len() {
                if let Some(bytes) = self.resolve(&names[i]).as_string() {
                    out.push((bytes.to_vec(), self.resolve(&names[i + 1]).clone()));
                }
                i += 2;
            }
        }
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for kid in kids {
                self.collect_name_tree(kid, depth + 1, out);
            }
        }
    }

    /// Resolve a filespec dictionary (the value side of an `/EmbeddedFiles`
    /// name-tree entry) to a decoded [`Attachment`], or `None` if there is no
    /// readable `/EF` embedded-file stream.
    fn filespec_to_attachment(&self, key: &[u8], value: &Object) -> Option<Attachment> {
        let spec = self.resolve(value).as_dict()?;
        let text = |o: &Object| self.resolve(o).as_string().map(crate::font::decode_pdf_text);
        let filename = spec
            .get(b"UF")
            .or_else(|| spec.get(b"F"))
            .and_then(&text)
            .unwrap_or_else(|| String::from_utf8_lossy(key).into_owned());
        let description = spec.get(b"Desc").and_then(&text);
        let ef = spec
            .get(b"EF")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        let stream = ef
            .get(b"F")
            .or_else(|| ef.get(b"UF"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_stream)?;
        let data = crate::filters::decode_stream(stream).ok()?;
        let mime = stream
            .dict
            .get(b"Subtype")
            .and_then(Object::as_name)
            .map(|n| String::from_utf8_lossy(n).into_owned());
        let params = stream
            .dict
            .get(b"Params")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let date = |k: &[u8]| params.and_then(|p| p.get(k)).and_then(&text);
        Some(Attachment {
            name: String::from_utf8_lossy(key).into_owned(),
            filename,
            mime,
            description,
            creation_date: date(b"CreationDate"),
            mod_date: date(b"ModDate"),
            data,
        })
    }

    /// Add an internal hyperlink over `rect` that jumps to the **named
    /// destination** `dest_name` (define it with [`add_named_dest`]). Unlike
    /// [`add_goto_link`](Self::add_goto_link) (an explicit page reference), this
    /// stores `/Dest /dest_name` — the indirection that lets the anchor be
    /// retargeted and keeps cross-references intact through split/extract.
    pub fn add_goto_link_named(
        &mut self,
        page_no: u32,
        rect: [f64; 4],
        dest_name: &str,
    ) -> Result<()> {
        let mut dict = Self::base_link_dict(rect);
        dict.set(b"Dest".to_vec(), annot::name(dest_name.as_bytes()));
        self.append_annotation_dict(page_no, dict)
    }

    /// The document outline (bookmarks) flattened in reading order.
    pub fn outline_items(&self) -> Vec<OutlineItem> {
        let mut out = Vec::new();
        let root = match self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"Outlines"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
        {
            Some(dict) => dict,
            None => return out,
        };
        if let Some(first) = root.get(b"First").and_then(Object::as_reference) {
            self.walk_outline(first, 0, &mut out, 0);
        }
        out
    }

    fn walk_outline(
        &self,
        start: ObjectId,
        level: usize,
        out: &mut Vec<OutlineItem>,
        depth: usize,
    ) {
        if depth > 64 {
            return;
        }
        let mut current = Some(start);
        let mut guard = 0;
        while let Some(id) = current {
            guard += 1;
            if guard > 100_000 {
                break; // defend against a cyclic /Next chain
            }
            let Some(dict) = self.objects.get(&id).and_then(Object::as_dict) else {
                break;
            };
            let title = dict
                .get(b"Title")
                .map(|o| self.string_value(o))
                .unwrap_or_default();
            let flags = dict.get(b"F").and_then(Object::as_i64).unwrap_or(0);
            let color = dict
                .get(b"C")
                .map(|o| self.resolve(o))
                .and_then(Object::as_array)
                .map(|a| {
                    let c = |i: usize| a.get(i).and_then(|o| self.resolve(o).as_f64()).unwrap_or(0.0);
                    [c(0), c(1), c(2)]
                })
                .unwrap_or([0.0, 0.0, 0.0]);
            let (page, dest_kind, dest_x, dest_y, dest_zoom) = self.dest_detail_of(dict);
            out.push(OutlineItem {
                title,
                level,
                page,
                bold: flags & 2 != 0,
                italic: flags & 1 != 0,
                color,
                dest_kind,
                dest_x,
                dest_y,
                dest_zoom,
            });
            if let Some(child) = dict.get(b"First").and_then(Object::as_reference) {
                self.walk_outline(child, level + 1, out, depth + 1);
            }
            current = dict.get(b"Next").and_then(Object::as_reference);
        }
    }

    /// Replace the entire document outline from a flat `(title, page, level)`
    /// list (pre-order; `level` 0 = top). An empty list clears the outline.
    pub fn set_outline(&mut self, items: &[(String, Option<u32>, usize)]) -> Result<()> {
        let catalog_id = self.catalog_id()?;
        if items.is_empty() {
            if let Some(mut catalog) = self
                .objects
                .get(&catalog_id)
                .and_then(Object::as_dict)
                .cloned()
            {
                catalog.remove(b"Outlines");
                self.objects.insert(catalog_id, Object::Dictionary(catalog));
            }
            return Ok(());
        }

        let base = self.next_object_number();
        let outlines_id = (base, 0u16);
        let item_ids: Vec<ObjectId> = (0..items.len())
            .map(|i| (base + 1 + i as u32, 0u16))
            .collect();

        // Tree linkage computed from the flat level list via an ancestor stack.
        let mut parent = vec![outlines_id; items.len()];
        let mut prev_idx: Vec<Option<usize>> = vec![None; items.len()];
        let mut next_idx: Vec<Option<usize>> = vec![None; items.len()];
        let mut first_child: BTreeMap<ObjectId, usize> = BTreeMap::new();
        let mut last_child: BTreeMap<ObjectId, usize> = BTreeMap::new();
        let mut stack: Vec<usize> = Vec::new();

        for i in 0..items.len() {
            let level = items[i].2;
            while let Some(&top) = stack.last() {
                if items[top].2 >= level {
                    stack.pop();
                } else {
                    break;
                }
            }
            let parent_id = stack.last().map(|&t| item_ids[t]).unwrap_or(outlines_id);
            parent[i] = parent_id;
            if let Some(&prev) = last_child.get(&parent_id) {
                next_idx[prev] = Some(i);
                prev_idx[i] = Some(prev);
            } else {
                first_child.insert(parent_id, i);
            }
            last_child.insert(parent_id, i);
            stack.push(i);
        }

        // Number of descendants of item `i` = contiguous block of deeper levels.
        let subtree_size = |i: usize| -> usize {
            let level = items[i].2;
            items[i + 1..]
                .iter()
                .take_while(|(_, _, l)| *l > level)
                .count()
        };

        for (i, (title, page, _)) in items.iter().enumerate() {
            let id = item_ids[i];
            let mut dict = Dictionary::new();
            dict.set(
                b"Title".to_vec(),
                Object::String(crate::font::encode_pdf_text(title), StringKind::Literal),
            );
            dict.set(b"Parent".to_vec(), Object::Reference(parent[i]));
            if let Some(prev) = prev_idx[i] {
                dict.set(b"Prev".to_vec(), Object::Reference(item_ids[prev]));
            }
            if let Some(next) = next_idx[i] {
                dict.set(b"Next".to_vec(), Object::Reference(item_ids[next]));
            }
            if let Some(&child) = first_child.get(&id) {
                dict.set(b"First".to_vec(), Object::Reference(item_ids[child]));
            }
            if let Some(&child) = last_child.get(&id) {
                dict.set(b"Last".to_vec(), Object::Reference(item_ids[child]));
            }
            let descendants = subtree_size(i);
            if descendants > 0 {
                // Positive: the item is open, showing all its descendants.
                dict.set(b"Count".to_vec(), Object::Integer(descendants as i64));
            }
            if let Some(p) = page {
                if let Ok(target_id) = self.page_object_id(*p) {
                    dict.set(
                        b"Dest".to_vec(),
                        Object::Array(vec![Object::Reference(target_id), annot::name(b"Fit")]),
                    );
                }
            }
            self.objects.insert(id, Object::Dictionary(dict));
        }

        // The /Outlines root.
        let mut root = Dictionary::new();
        root.set(b"Type".to_vec(), annot::name(b"Outlines"));
        if let Some(&child) = first_child.get(&outlines_id) {
            root.set(b"First".to_vec(), Object::Reference(item_ids[child]));
        }
        if let Some(&child) = last_child.get(&outlines_id) {
            root.set(b"Last".to_vec(), Object::Reference(item_ids[child]));
        }
        root.set(b"Count".to_vec(), Object::Integer(items.len() as i64));
        self.objects.insert(outlines_id, Object::Dictionary(root));

        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        catalog.set(b"Outlines".to_vec(), Object::Reference(outlines_id));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(())
    }

    // ─── optional content (layers / OCG) ─────────────────────────────────────

    /// The document's optional-content layers (PDF OCGs), ordered as in the
    /// default configuration's `/Order` (then discovery order).
    pub fn layers(&self) -> Vec<Layer> {
        let Some(ocp) = self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"OCProperties"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
        else {
            return Vec::new();
        };
        let Some(ocgs) = ocp
            .get(b"OCGs")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        else {
            return Vec::new();
        };
        let cfg = ocp
            .get(b"D")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict);
        let off = self.oc_ref_ids(cfg.and_then(|c| c.get(b"OFF")));
        let locked = self.oc_ref_ids(cfg.and_then(|c| c.get(b"Locked")));
        let mut order = Vec::new();
        self.oc_order_ids(cfg.and_then(|c| c.get(b"Order")), &mut order);

        let mut out = Vec::new();
        for obj in ocgs {
            let Some(oid) = obj.as_reference() else {
                continue;
            };
            let name = self
                .objects
                .get(&oid)
                .and_then(Object::as_dict)
                .and_then(|d| d.get(b"Name"))
                .map(|o| self.string_value(o))
                .unwrap_or_default();
            out.push(Layer {
                id: oid.0,
                name,
                visible: !off.contains(&oid.0),
                locked: locked.contains(&oid.0),
                order: order.iter().position(|&x| x == oid.0).unwrap_or(usize::MAX),
            });
        }
        // /Order entries first (ascending), then any remaining in discovery order.
        for (i, layer) in out.iter_mut().enumerate() {
            if layer.order == usize::MAX {
                layer.order = order.len() + i;
            }
        }
        out.sort_by_key(|l| l.order);
        out
    }

    /// Create a new (initially visible, unlocked) optional-content layer.
    /// Returns the OCG's object number — the id for the toggle/remove calls.
    pub fn add_layer(&mut self, name: &str) -> Result<u32> {
        let ocg_id = (self.next_object_number(), 0u16);
        let mut ocg = Dictionary::new();
        ocg.set(b"Type".to_vec(), annot::name(b"OCG"));
        ocg.set(
            b"Name".to_vec(),
            Object::String(crate::font::encode_pdf_text(name), StringKind::Literal),
        );
        self.objects.insert(ocg_id, Object::Dictionary(ocg));

        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        let mut ocp = catalog
            .get(b"OCProperties")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(Dictionary::new);
        let mut ocgs = ocp
            .get(b"OCGs")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        ocgs.push(Object::Reference(ocg_id));
        ocp.set(b"OCGs".to_vec(), Object::Array(ocgs));

        let mut cfg = ocp
            .get(b"D")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(Dictionary::new);
        if cfg.get(b"Name").is_none() {
            cfg.set(
                b"Name".to_vec(),
                Object::String(b"Default".to_vec(), StringKind::Literal),
            );
        }
        let mut order = cfg
            .get(b"Order")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        order.push(Object::Reference(ocg_id));
        cfg.set(b"Order".to_vec(), Object::Array(order));
        ocp.set(b"D".to_vec(), Object::Dictionary(cfg));

        catalog.set(b"OCProperties".to_vec(), Object::Dictionary(ocp));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(ocg_id.0)
    }

    /// Show or hide a layer (membership of `/D /OFF`).
    pub fn set_layer_visibility(&mut self, layer_id: u32, visible: bool) -> Result<()> {
        let oid = self
            .oc_object_id(layer_id)
            .ok_or_else(|| EngineError::Missing("optional content group".into()))?;
        self.with_oc_default_config(|cfg| Self::set_oc_membership(cfg, b"OFF", oid, !visible))
    }

    /// Lock or unlock a layer (membership of `/D /Locked`).
    pub fn set_layer_locked(&mut self, layer_id: u32, locked: bool) -> Result<()> {
        let oid = self
            .oc_object_id(layer_id)
            .ok_or_else(|| EngineError::Missing("optional content group".into()))?;
        self.with_oc_default_config(|cfg| Self::set_oc_membership(cfg, b"Locked", oid, locked))
    }

    /// Remove a layer from the optional-content configuration. Content still
    /// tagged with the OCG renders unconditionally afterwards (spec-compliant).
    pub fn remove_layer(&mut self, layer_id: u32) -> Result<()> {
        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        let Some(mut ocp) = catalog
            .get(b"OCProperties")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
        else {
            return Ok(());
        };
        if let Some(mut ocgs) = ocp
            .get(b"OCGs")
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
        {
            ocgs.retain(|o| o.as_reference().map(|r| r.0) != Some(layer_id));
            ocp.set(b"OCGs".to_vec(), Object::Array(ocgs));
        }
        if let Some(mut cfg) = ocp
            .get(b"D")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
        {
            for key in [b"OFF".as_ref(), b"ON", b"Locked", b"Order"] {
                if let Some(mut arr) = cfg
                    .get(key)
                    .and_then(Object::as_array)
                    .map(<[Object]>::to_vec)
                {
                    Self::remove_oc_ref_deep(&mut arr, layer_id);
                    if arr.is_empty() {
                        cfg.remove(key);
                    } else {
                        cfg.set(key.to_vec(), Object::Array(arr));
                    }
                }
            }
            ocp.set(b"D".to_vec(), Object::Dictionary(cfg));
        }
        catalog.set(b"OCProperties".to_vec(), Object::Dictionary(ocp));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(())
    }

    /// Resolve a layer's object number to its full `ObjectId` (preserving the
    /// generation) by locating it in `/OCProperties /OCGs`.
    fn oc_object_id(&self, layer_id: u32) -> Option<ObjectId> {
        self.catalog()
            .ok()
            .and_then(|c| c.get(b"OCProperties"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|ocp| ocp.get(b"OCGs"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .and_then(|arr| {
                arr.iter()
                    .filter_map(|o| o.as_reference())
                    .find(|r| r.0 == layer_id)
            })
    }

    /// Object numbers of the top-level references in an `/OFF`-style array.
    fn oc_ref_ids(&self, obj: Option<&Object>) -> Vec<u32> {
        obj.map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| o.as_reference().map(|r| r.0))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Flatten the (possibly nested) `/Order` array into layer object numbers.
    fn oc_order_ids(&self, obj: Option<&Object>, out: &mut Vec<u32>) {
        if let Some(arr) = obj.map(|o| self.resolve(o)).and_then(Object::as_array) {
            for item in arr {
                match item {
                    Object::Reference(r) => out.push(r.0),
                    Object::Array(_) => self.oc_order_ids(Some(item), out),
                    _ => {}
                }
            }
        }
    }

    /// Get-or-create the default OC configuration (`/OCProperties /D`), apply
    /// `f`, and write it back through the catalog.
    fn with_oc_default_config<F: FnOnce(&mut Dictionary)>(&mut self, f: F) -> Result<()> {
        let catalog_id = self.catalog_id()?;
        let mut catalog = self
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing("document catalog".into()))?
            .clone();
        let mut ocp = catalog
            .get(b"OCProperties")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(Dictionary::new);
        let mut cfg = ocp
            .get(b"D")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(Dictionary::new);
        f(&mut cfg);
        ocp.set(b"D".to_vec(), Object::Dictionary(cfg));
        catalog.set(b"OCProperties".to_vec(), Object::Dictionary(ocp));
        self.objects.insert(catalog_id, Object::Dictionary(catalog));
        Ok(())
    }

    /// Ensure `oid` is present in (or absent from) `cfg[key]`, dropping the key
    /// when the resulting array is empty.
    fn set_oc_membership(cfg: &mut Dictionary, key: &[u8], oid: ObjectId, present: bool) {
        let mut arr: Vec<Object> = cfg
            .get(key)
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        arr.retain(|o| o.as_reference().map(|r| r.0) != Some(oid.0));
        if present {
            arr.push(Object::Reference(oid));
        }
        if arr.is_empty() {
            cfg.remove(key);
        } else {
            cfg.set(key.to_vec(), Object::Array(arr));
        }
    }

    /// Remove every reference to `layer_id` from an array, recursing into nested
    /// `/Order` sub-arrays.
    fn remove_oc_ref_deep(arr: &mut Vec<Object>, layer_id: u32) {
        arr.retain(|o| o.as_reference().map(|r| r.0) != Some(layer_id));
        for o in arr.iter_mut() {
            if let Object::Array(inner) = o {
                Self::remove_oc_ref_deep(inner, layer_id);
            }
        }
    }

    // ─── page structure (resize / insert / duplicate) ────────────────────────

    /// Set a page's `/MediaBox` to `[0 0 width height]` (points).
    pub fn resize_page(&mut self, page_no: u32, width: f64, height: f64) -> Result<()> {
        let id = self.page_object_id(page_no)?;
        let mut page = self
            .objects
            .get(&id)
            .and_then(Object::as_dict)
            .cloned()
            .ok_or(EngineError::PageNotFound(page_no))?;
        page.set(b"MediaBox".to_vec(), Self::media_box_array(width, height));
        self.objects.insert(id, Object::Dictionary(page));
        Ok(())
    }

    /// Insert a blank page of `width`×`height` points immediately after the
    /// 1-based `after` page (`after == 0` prepends). Returns its object number.
    pub fn add_page(&mut self, width: f64, height: f64, after: u32) -> Result<u32> {
        let content_id = (self.next_object_number(), 0u16);
        self.objects.insert(
            content_id,
            Object::Stream(Stream::new(Dictionary::new(), Vec::new())),
        );
        let page_id = (self.next_object_number(), 0u16);
        let mut page = Dictionary::new();
        page.set(b"Type".to_vec(), annot::name(b"Page"));
        page.set(b"MediaBox".to_vec(), Self::media_box_array(width, height));
        page.set(b"Contents".to_vec(), Object::Reference(content_id));
        page.set(b"Resources".to_vec(), Object::Dictionary(Dictionary::new()));
        self.objects.insert(page_id, Object::Dictionary(page));
        self.insert_page_after(page_id, after)?;
        Ok(page_id.0)
    }

    /// Duplicate the 1-based `page_no`, inserting the copy right after it. The
    /// content streams are cloned (independent edits); resources are shared.
    /// Returns the new page's object number.
    pub fn copy_page(&mut self, page_no: u32) -> Result<u32> {
        let src_id = self.page_object_id(page_no)?;
        let mut page = self
            .objects
            .get(&src_id)
            .and_then(Object::as_dict)
            .cloned()
            .ok_or(EngineError::PageNotFound(page_no))?;
        let new_contents = self.clone_page_contents(&page);
        page.set(b"Contents".to_vec(), new_contents);
        let new_page_id = (self.next_object_number(), 0u16);
        self.objects.insert(new_page_id, Object::Dictionary(page));
        self.insert_page_after(new_page_id, page_no)?;
        Ok(new_page_id.0)
    }

    fn media_box_array(width: f64, height: f64) -> Object {
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(0),
            Object::Real(width),
            Object::Real(height),
        ])
    }

    /// Clone a page's content stream object(s) into fresh objects and return the
    /// new `/Contents` value (a single reference, or an array of them).
    fn clone_page_contents(&mut self, page: &Dictionary) -> Object {
        let Some(contents) = page.get(b"Contents").cloned() else {
            return Object::Null;
        };
        let stream_ids: Vec<ObjectId> = match &contents {
            Object::Reference(r) => match self.objects.get(r) {
                Some(Object::Array(arr)) => arr.iter().filter_map(Object::as_reference).collect(),
                Some(_) => vec![*r],
                None => Vec::new(),
            },
            Object::Array(arr) => arr.iter().filter_map(Object::as_reference).collect(),
            _ => Vec::new(),
        };
        let mut new_refs = Vec::new();
        for sid in stream_ids {
            if let Some(obj) = self.objects.get(&sid).cloned() {
                let nid = (self.next_object_number(), 0u16);
                self.objects.insert(nid, obj);
                new_refs.push(Object::Reference(nid));
            }
        }
        match new_refs.len() {
            0 => Object::Null,
            1 => new_refs.into_iter().next().unwrap(),
            _ => Object::Array(new_refs),
        }
    }

    /// Insert `new_page_id` into the page tree just after the 1-based `after`
    /// page (`0` = front). Sets the new page's `/Parent` and bumps `/Count` up
    /// the ancestor chain.
    fn insert_page_after(&mut self, new_page_id: ObjectId, after: u32) -> Result<()> {
        let ids = self.page_ids()?;
        if ids.is_empty() {
            return Err(EngineError::Missing("document has no pages".into()));
        }
        let ref_idx = (after.max(1) as usize - 1).min(ids.len() - 1);
        let ref_page_id = ids[ref_idx];
        let parent_id = self
            .objects
            .get(&ref_page_id)
            .and_then(Object::as_dict)
            .and_then(|d| d.get(b"Parent"))
            .and_then(Object::as_reference)
            .ok_or_else(|| EngineError::Missing("page /Parent".into()))?;

        let mut parent = self
            .objects
            .get(&parent_id)
            .and_then(Object::as_dict)
            .cloned()
            .ok_or_else(|| EngineError::Missing("pages tree node".into()))?;
        let mut kids = parent
            .get(b"Kids")
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        let pos = if after == 0 {
            0
        } else {
            kids.iter()
                .position(|o| o.as_reference() == Some(ref_page_id))
                .map(|p| p + 1)
                .unwrap_or(kids.len())
        };
        kids.insert(pos, Object::Reference(new_page_id));
        parent.set(b"Kids".to_vec(), Object::Array(kids));
        self.objects.insert(parent_id, Object::Dictionary(parent));

        if let Some(mut page) = self
            .objects
            .get(&new_page_id)
            .and_then(Object::as_dict)
            .cloned()
        {
            page.set(b"Parent".to_vec(), Object::Reference(parent_id));
            self.objects.insert(new_page_id, Object::Dictionary(page));
        }

        // Increment /Count on the parent and every ancestor Pages node.
        let mut node = Some(parent_id);
        let mut guard = 0;
        while let Some(nid) = node {
            guard += 1;
            if guard > 64 {
                break;
            }
            let Some(mut d) = self.objects.get(&nid).and_then(Object::as_dict).cloned() else {
                break;
            };
            let count = d.get(b"Count").and_then(Object::as_i64).unwrap_or(0);
            d.set(b"Count".to_vec(), Object::Integer(count + 1));
            let up = d.get(b"Parent").and_then(Object::as_reference);
            self.objects.insert(nid, Object::Dictionary(d));
            node = up;
        }
        Ok(())
    }

    // ─── interactive forms (AcroForm) ────────────────────────────────────────

    fn string_value(&self, object: &Object) -> String {
        match self.resolve(object) {
            Object::String(bytes, _) => crate::font::decode_pdf_text(bytes),
            Object::Name(name) => String::from_utf8_lossy(name).into_owned(),
            _ => String::new(),
        }
    }

    /// List the document's interactive form fields.
    pub fn form_fields(&self) -> Result<Vec<FormField>> {
        let mut out = Vec::new();
        let acroform = match self.catalog().ok().and_then(|c| c.get(b"AcroForm")) {
            Some(obj) => self.resolve(obj).clone(),
            None => return Ok(out),
        };
        let fields = acroform
            .as_dict()
            .and_then(|d| d.get(b"Fields"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(<[Object]>::to_vec)
            .unwrap_or_default();
        for field in &fields {
            self.collect_field(field, "", &mut out, 0);
        }
        Ok(out)
    }

    fn collect_field(&self, field: &Object, prefix: &str, out: &mut Vec<FormField>, depth: usize) {
        if depth > 32 {
            return;
        }
        let Some(dict) = self.resolve(field).as_dict() else {
            return;
        };
        let partial = dict
            .get(b"T")
            .map(|o| self.string_value(o))
            .unwrap_or_default();
        let name = match (prefix.is_empty(), partial.is_empty()) {
            (true, _) => partial.clone(),
            (false, true) => prefix.to_string(),
            (false, false) => format!("{prefix}.{partial}"),
        };

        // A field with kids that are themselves named fields is a branch node.
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            let has_named_kids = kids
                .iter()
                .any(|k| self.resolve(k).as_dict().is_some_and(|d| d.contains(b"T")));
            if has_named_kids {
                for kid in kids {
                    self.collect_field(kid, &name, out, depth + 1);
                }
                return;
            }
        }

        let field_type = dict
            .get(b"FT")
            .and_then(Object::as_name)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        let value = self.field_value_string(dict);
        let flags = dict.get(b"Ff").and_then(Object::as_i64).unwrap_or(0) as u32;
        let max_len = dict
            .get(b"MaxLen")
            .and_then(Object::as_i64)
            .filter(|n| *n >= 0)
            .map(|n| n as u32);
        let options = match field_type.as_str() {
            "Ch" => self
                .choice_options(dict)
                .into_iter()
                .map(|(_, display)| display)
                .collect(),
            "Btn" => self.button_export_states(dict),
            _ => Vec::new(),
        };
        // Widget geometry: the field dict itself (merged field+widget) carries
        // /Rect, or its first widget kid does; /P points at the widget's page.
        let widget = if dict.contains(b"Rect") {
            Some(dict)
        } else {
            dict.get(b"Kids")
                .map(|o| self.resolve(o))
                .and_then(Object::as_array)
                .and_then(|kids| kids.first())
                .map(|k| self.resolve(k))
                .and_then(Object::as_dict)
        };
        let (page, bounds) = match widget {
            Some(w) => self.widget_geometry(w),
            None => (None, None),
        };

        out.push(FormField {
            name,
            field_type,
            value,
            flags,
            options,
            max_len,
            page,
            bounds,
        });
    }

    /// A widget's page number (1-based, from `/P`) and top-left bounds
    /// `[x, y, width, height]` (points) from its `/Rect`. `(None, None)` when
    /// the widget has no rectangle.
    fn widget_geometry(&self, widget: &Dictionary) -> (Option<u32>, Option<[f64; 4]>) {
        let Some(rect) = widget
            .get(b"Rect")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .filter(|a| a.len() >= 4)
        else {
            return (None, None);
        };
        let v = |i: usize| self.resolve(&rect[i]).as_f64().unwrap_or(0.0);
        let (x0, x1) = (v(0).min(v(2)), v(0).max(v(2)));
        let (y0, y1) = (v(1).min(v(3)), v(1).max(v(3)));

        // Page number from /P; default to the first page if absent.
        let page = widget
            .get(b"P")
            .and_then(Object::as_reference)
            .and_then(|p_ref| {
                self.page_ids()
                    .ok()
                    .and_then(|ids| ids.iter().position(|id| *id == p_ref))
            })
            .map(|idx| idx as u32 + 1)
            .unwrap_or(1);

        let page_height = self.page_info(page).map(|(_, h, _)| h).unwrap_or(792.0);
        // /Rect is bottom-left origin; flip to top-left for the host UI.
        let bounds = [x0, page_height - y1, x1 - x0, y1 - y0];
        (Some(page), Some(bounds))
    }

    /// Read a field's `/V` as a display string, joining array values (multi-
    /// select choice) with newlines.
    fn field_value_string(&self, dict: &Dictionary) -> String {
        match dict.get(b"V").map(|o| self.resolve(o)) {
            Some(Object::Array(items)) => items
                .iter()
                .map(|i| self.string_value(i))
                .collect::<Vec<_>>()
                .join("\n"),
            Some(other) => self.string_value(other),
            None => String::new(),
        }
    }

    /// Choice `/Opt` entries as `(export, display)` pairs. An entry may be a
    /// bare string (export == display) or a `[export, display]` array.
    fn choice_options(&self, dict: &Dictionary) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(opt) = dict
            .get(b"Opt")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for entry in opt {
                match self.resolve(entry) {
                    Object::Array(pair) if pair.len() >= 2 => {
                        out.push((self.string_value(&pair[0]), self.string_value(&pair[1])));
                    }
                    Object::Array(pair) if pair.len() == 1 => {
                        let s = self.string_value(&pair[0]);
                        out.push((s.clone(), s));
                    }
                    other => {
                        let s = self.string_value(other);
                        out.push((s.clone(), s));
                    }
                }
            }
        }
        out
    }

    /// Export "on" states of a button field (the non-`Off` keys of every
    /// widget's `/AP /N` appearance sub-dictionary), de-duplicated in order.
    fn button_export_states(&self, dict: &Dictionary) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push_from = |this: &Self, d: &Dictionary| {
            if let Some(states) = d
                .get(b"AP")
                .map(|o| this.resolve(o))
                .and_then(Object::as_dict)
                .and_then(|ap| ap.get(b"N"))
                .map(|o| this.resolve(o))
                .and_then(Object::as_dict)
            {
                for key in states.0.keys() {
                    if key.as_slice() != b"Off" {
                        let name = String::from_utf8_lossy(key).into_owned();
                        if !out.contains(&name) {
                            out.push(name);
                        }
                    }
                }
            }
        };
        push_from(self, dict);
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for kid in kids {
                if let Some(kid_dict) = self.resolve(kid).as_dict() {
                    push_from(self, kid_dict);
                }
            }
        }
        out
    }

    /// Object id of a terminal field with the given fully-qualified name.
    fn find_field_id(&self, target: &str) -> Option<ObjectId> {
        let acroform = self
            .catalog()
            .ok()?
            .get(b"AcroForm")
            .map(|o| self.resolve(o))?;
        let fields = acroform
            .as_dict()?
            .get(b"Fields")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)?
            .to_vec();
        fields
            .iter()
            .find_map(|f| self.find_field_rec(f, "", target, 0))
    }

    fn find_field_rec(
        &self,
        field: &Object,
        prefix: &str,
        target: &str,
        depth: usize,
    ) -> Option<ObjectId> {
        if depth > 32 {
            return None;
        }
        let id = field.as_reference();
        let dict = self.resolve(field).as_dict()?;
        let partial = dict
            .get(b"T")
            .map(|o| self.string_value(o))
            .unwrap_or_default();
        let name = match (prefix.is_empty(), partial.is_empty()) {
            (true, _) => partial.clone(),
            (false, true) => prefix.to_string(),
            (false, false) => format!("{prefix}.{partial}"),
        };
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            let has_named_kids = kids
                .iter()
                .any(|k| self.resolve(k).as_dict().is_some_and(|d| d.contains(b"T")));
            if has_named_kids {
                return kids
                    .iter()
                    .find_map(|k| self.find_field_rec(k, &name, target, depth + 1));
            }
        }
        if name == target {
            id
        } else {
            None
        }
    }

    fn set_need_appearances(&mut self) {
        let acro_id = match self
            .catalog()
            .ok()
            .and_then(|c| c.get(b"AcroForm"))
            .and_then(Object::as_reference)
        {
            Some(id) => id,
            None => return,
        };
        if let Some(mut acro) = self
            .objects
            .get(&acro_id)
            .and_then(Object::as_dict)
            .cloned()
        {
            acro.set(b"NeedAppearances".to_vec(), Object::Boolean(true));
            self.objects.insert(acro_id, Object::Dictionary(acro));
        }
    }

    /// Regenerate a widget's `/AP /N` to display `text`, or flag the form for
    /// viewer-side regeneration when the field has no own rectangle.
    fn regenerate_text_appearance(&mut self, widget: &mut Dictionary, text: &str) {
        if !widget.contains(b"Rect") {
            self.set_need_appearances();
            return;
        }
        let rect = self.read_rect(widget);
        let (mut form, content) = build_text_field_appearance(rect, text);
        form.set(b"Length".to_vec(), Object::Integer(content.len() as i64));
        let ap_id = (self.next_object_number(), 0u16);
        self.objects
            .insert(ap_id, Object::Stream(Stream::new(form, content)));
        let mut appearance = Dictionary::new();
        appearance.set(b"N".to_vec(), Object::Reference(ap_id));
        widget.set(b"AP".to_vec(), Object::Dictionary(appearance));
    }

    fn require_field(&self, name: &str) -> Result<(ObjectId, Dictionary)> {
        let id = self
            .find_field_id(name)
            .ok_or_else(|| EngineError::Missing(format!("form field '{name}'")))?;
        let dict = self
            .objects
            .get(&id)
            .and_then(Object::as_dict)
            .ok_or_else(|| EngineError::Missing(format!("form field '{name}'")))?
            .clone();
        Ok((id, dict))
    }

    /// Fill a text field (single- or multi-line) by fully-qualified name,
    /// regenerating its appearance. `/MaxLen` is honoured by truncation.
    pub fn set_text_field(&mut self, name: &str, value: &str) -> Result<()> {
        let (id, mut dict) = self.require_field(name)?;
        let value = match dict.get(b"MaxLen").and_then(Object::as_i64) {
            Some(max) if max >= 0 && !value.contains('\n') => {
                value.chars().take(max as usize).collect::<String>()
            }
            _ => value.to_string(),
        };
        dict.set(
            b"V".to_vec(),
            Object::String(crate::font::encode_pdf_text(&value), StringKind::Literal),
        );
        self.regenerate_text_appearance(&mut dict, &value);
        self.objects.insert(id, Object::Dictionary(dict));
        Ok(())
    }

    /// First non-`Off` appearance state of a widget's `/AP /N`, if any.
    fn widget_on_state(&self, dict: &Dictionary) -> Option<Vec<u8>> {
        let states = dict
            .get(b"AP")
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|ap| ap.get(b"N"))
            .map(|o| self.resolve(o))
            .and_then(Object::as_dict)?;
        states.0.keys().find(|k| k.as_slice() != b"Off").cloned()
    }

    /// The "on" state of a checkbox, looking at the field and then its widgets.
    fn checkbox_on_state(&self, dict: &Dictionary) -> Vec<u8> {
        if let Some(state) = self.widget_on_state(dict) {
            return state;
        }
        if let Some(kids) = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
        {
            for kid in kids {
                if let Some(kid_dict) = self.resolve(kid).as_dict() {
                    if let Some(state) = self.widget_on_state(kid_dict) {
                        return state;
                    }
                }
            }
        }
        b"Yes".to_vec()
    }

    /// Check or uncheck a checkbox by fully-qualified name. The appearance
    /// state `/AS` is set on the field and on every widget kid.
    pub fn set_checkbox(&mut self, name: &str, checked: bool) -> Result<()> {
        let (id, mut dict) = self.require_field(name)?;
        let state = if checked {
            self.checkbox_on_state(&dict)
        } else {
            b"Off".to_vec()
        };
        dict.set(b"V".to_vec(), Object::Name(state.clone()));
        dict.set(b"AS".to_vec(), Object::Name(state.clone()));
        let kids: Vec<ObjectId> = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(|a| a.iter().filter_map(Object::as_reference).collect())
            .unwrap_or_default();
        for kid_id in kids {
            if let Some(mut kid) = self.objects.get(&kid_id).and_then(Object::as_dict).cloned() {
                kid.set(b"AS".to_vec(), Object::Name(state.clone()));
                self.objects.insert(kid_id, Object::Dictionary(kid));
            }
        }
        self.objects.insert(id, Object::Dictionary(dict));
        Ok(())
    }

    /// Select one option of a radio-button group by its export value. Every
    /// widget kid's `/AS` is set to that value (matching kid) or `/Off`.
    pub fn set_radio(&mut self, name: &str, export_value: &str) -> Result<()> {
        let (id, mut dict) = self.require_field(name)?;
        let target = export_value.as_bytes().to_vec();
        let mut matched = false;

        let kids: Vec<ObjectId> = dict
            .get(b"Kids")
            .map(|o| self.resolve(o))
            .and_then(Object::as_array)
            .map(|a| a.iter().filter_map(Object::as_reference).collect())
            .unwrap_or_default();

        if kids.is_empty() {
            // A radio whose single widget is merged into the field object.
            if self.widget_on_state(&dict).as_deref() == Some(target.as_slice()) {
                matched = true;
            }
            dict.set(b"AS".to_vec(), Object::Name(target.clone()));
        } else {
            for kid_id in &kids {
                let Some(mut kid) = self.objects.get(kid_id).and_then(Object::as_dict).cloned()
                else {
                    continue;
                };
                let is_target = self.widget_on_state(&kid).as_deref() == Some(target.as_slice());
                let state = if is_target {
                    matched = true;
                    target.clone()
                } else {
                    b"Off".to_vec()
                };
                kid.set(b"AS".to_vec(), Object::Name(state));
                self.objects.insert(*kid_id, Object::Dictionary(kid));
            }
        }

        if !matched {
            return Err(EngineError::Unsupported(format!(
                "radio '{name}' has no option '{export_value}'"
            )));
        }
        dict.set(b"V".to_vec(), Object::Name(target));
        self.objects.insert(id, Object::Dictionary(dict));
        Ok(())
    }

    /// Set the selection of a choice field (combo box or list box) by
    /// fully-qualified name. Values match an option's export *or* display
    /// string; an editable combo also accepts a free-text value. `/V`, `/I`
    /// (indices) and the appearance are updated.
    pub fn set_choice_field(&mut self, name: &str, values: &[&str]) -> Result<()> {
        let (id, mut dict) = self.require_field(name)?;
        let options = self.choice_options(&dict);
        let flags = dict.get(b"Ff").and_then(Object::as_i64).unwrap_or(0) as u32;
        let editable =
            flags & crate::form::flags::COMBO != 0 && flags & crate::form::flags::EDIT != 0;

        // Resolve each requested value to (export, display, index).
        let mut chosen: Vec<(String, String, Option<usize>)> = Vec::new();
        for &want in values {
            if let Some((idx, (export, display))) = options
                .iter()
                .enumerate()
                .find(|(_, (e, d))| e == want || d == want)
            {
                chosen.push((export.clone(), display.clone(), Some(idx)));
            } else if editable {
                chosen.push((want.to_string(), want.to_string(), None));
            } else {
                return Err(EngineError::Unsupported(format!(
                    "choice field '{name}' has no option '{want}'"
                )));
            }
        }

        // /V: a single string, or an array for a multi-selection.
        if chosen.len() <= 1 {
            let export = chosen.first().map(|c| c.0.clone()).unwrap_or_default();
            dict.set(
                b"V".to_vec(),
                Object::String(crate::font::encode_pdf_text(&export), StringKind::Literal),
            );
        } else {
            let array = chosen
                .iter()
                .map(|c| Object::String(crate::font::encode_pdf_text(&c.0), StringKind::Literal))
                .collect();
            dict.set(b"V".to_vec(), Object::Array(array));
        }

        // /I: selected indices (ascending), omitted when nothing is indexable.
        let mut indices: Vec<i64> = chosen
            .iter()
            .filter_map(|c| c.2)
            .map(|i| i as i64)
            .collect();
        indices.sort_unstable();
        if indices.is_empty() {
            dict.remove(b"I");
        } else {
            dict.set(
                b"I".to_vec(),
                Object::Array(indices.into_iter().map(Object::Integer).collect()),
            );
        }

        let display = chosen
            .iter()
            .map(|c| c.1.clone())
            .collect::<Vec<_>>()
            .join("\n");
        self.regenerate_text_appearance(&mut dict, &display);
        self.objects.insert(id, Object::Dictionary(dict));
        Ok(())
    }

    /// Replace a page's content with `content` bytes, stored as a single new
    /// uncompressed stream. The page `/Contents` is repointed at it.
    pub fn set_page_content(&mut self, page_no: u32, content: Vec<u8>) -> Result<()> {
        let page_id = self.page_object_id(page_no)?;

        let new_id = (self.next_object_number(), 0u16);
        let mut dict = Dictionary::new();
        dict.set(b"Length".to_vec(), Object::Integer(content.len() as i64));
        self.objects
            .insert(new_id, Object::Stream(Stream::new(dict, content)));

        let mut page = self
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .ok_or(EngineError::PageNotFound(page_no))?
            .clone();
        page.set(b"Contents".to_vec(), Object::Reference(new_id));
        self.objects.insert(page_id, Object::Dictionary(page));
        Ok(())
    }

    fn append_content(&self, object: &Object, out: &mut Vec<u8>) -> Result<()> {
        match self.resolve(object) {
            Object::Stream(stream) => {
                let decoded = decode_stream(stream)?;
                out.extend_from_slice(&decoded);
                out.push(b'\n');
            }
            Object::Array(items) => {
                for item in items {
                    self.append_content(item, out)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// A [`raster::ResourceCtx`](crate::raster::render::ResourceCtx) backed by a
/// [`Document`] and a `/Resources` scope: it resolves the named resources a
/// content stream refers to (form XObjects, shadings, shading/tiling patterns,
/// ExtGStates) against `resources`, delegating the heavy lifting to the
/// `Document`'s resource-resolution helpers.
///
/// `seen` is the cycle-guard set of form/pattern object ids currently being
/// drawn (so a self-referential form doesn't recurse forever). `width`, `height`
/// and `base` are the page-level device dimensions and user-space → device
/// matrix; they're constant across the whole render tree and are needed to
/// rasterize an ExtGState `/SMask` group at device resolution.
struct PageResourceCtx<'a> {
    doc: &'a Document,
    resources: Dictionary,
    seen: std::collections::BTreeSet<ObjectId>,
    width: u32,
    height: u32,
    base: content::PageMatrix,
}

impl<'a> PageResourceCtx<'a> {
    /// A fresh top-level context (empty cycle-guard set) for a page or annotation
    /// appearance, carrying the page device size and base matrix.
    fn new(
        doc: &'a Document,
        resources: Dictionary,
        width: u32,
        height: u32,
        base: content::PageMatrix,
    ) -> Self {
        PageResourceCtx {
            doc,
            resources,
            seen: std::collections::BTreeSet::new(),
            width,
            height,
            base,
        }
    }
}

impl crate::raster::render::ResourceCtx for PageResourceCtx<'_> {
    fn form_xobject(&self, name: &[u8]) -> Option<crate::raster::render::FormXObject<'_>> {
        let (id, obj) = self.doc.resource_entry(&self.resources, b"XObject", name)?;
        self.doc.build_form_xobject(
            id,
            obj,
            &self.resources,
            &self.seen,
            self.width,
            self.height,
            self.base,
        )
    }

    fn shading(&self, name: &[u8]) -> Option<crate::raster::render::Shading> {
        let (_, obj) = self.doc.resource_entry(&self.resources, b"Shading", name)?;
        self.doc.read_shading(obj.as_dict()?, None)
    }

    fn pattern_shading(&self, name: &[u8]) -> Option<crate::raster::render::Shading> {
        let (_, obj) = self.doc.resource_entry(&self.resources, b"Pattern", name)?;
        let dict = obj.as_dict()?;
        // Only PatternType 2 (shading pattern) carries a `/Shading`.
        if dict.get(b"PatternType").and_then(Object::as_i64) != Some(2) {
            return None;
        }
        let shading = dict.get(b"Shading").map(|o| self.doc.resolve(o))?;
        let matrix = self.doc.form_matrix(dict);
        self.doc.read_shading(shading.as_dict()?, Some(matrix))
    }

    fn tiling_pattern(&self, name: &[u8]) -> Option<crate::raster::render::FormXObject<'_>> {
        let (id, obj) = self.doc.resource_entry(&self.resources, b"Pattern", name)?;
        let stream = obj.as_stream()?;
        // Only PatternType 1 (tiling pattern) is a content stream we can stamp.
        if stream.dict.get(b"PatternType").and_then(Object::as_i64) != Some(1) {
            return None;
        }
        if let Some(id) = id {
            if self.seen.contains(&id) {
                return None; // cycle guard
            }
        }
        let content = decode_stream(stream).ok()?;
        let matrix = self.doc.form_matrix(&stream.dict);
        let bbox = self.doc.read_bbox(&stream.dict);
        let resources = stream
            .dict
            .get(b"Resources")
            .map(|o| self.doc.resolve(o))
            .and_then(Object::as_dict)
            .cloned()
            .unwrap_or_else(|| self.resources.clone());
        let fonts = self.doc.render_fonts_for(&resources);
        let images = self.doc.images_for(&resources);
        let mut child_seen = self.seen.clone();
        if let Some(id) = id {
            child_seen.insert(id);
        }
        let ctx = PageResourceCtx {
            doc: self.doc,
            resources,
            seen: child_seen,
            width: self.width,
            height: self.height,
            base: self.base,
        };
        Some(crate::raster::render::FormXObject {
            content,
            matrix,
            bbox,
            fonts,
            images,
            ctx: Box::new(ctx),
        })
    }

    fn ext_gstate(&self, name: &[u8]) -> Option<crate::raster::render::ExtGStateParams> {
        self.doc.ext_gstate_params(
            &self.resources,
            name,
            self.width,
            self.height,
            self.base,
            &self.seen,
        )
    }
}

/// Read a two-element numeric array entry (`key`) from `dict`, resolving each
/// element. `None` if the entry is absent, not an array, or has fewer than two
/// numeric values. Used for function `/Domain` ranges.
fn read_pair(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<[f64; 2]> {
    let arr = dict.get(key).map(|o| doc.resolve(o)).and_then(Object::as_array)?;
    let v: Vec<f64> = arr.iter().filter_map(|o| doc.resolve(o).as_f64()).collect();
    (v.len() >= 2).then(|| [v[0], v[1]])
}

/// Read a numeric array entry (`key`) from `dict` into a `Vec<f64>`, resolving
/// each element. `None` if the entry is absent or not an array (an empty array
/// yields `Some(vec![])`). Used for function `/C0`, `/C1`, `/Bounds`, `/Encode`,
/// and a sampled function's `/Range`.
fn read_vec(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<Vec<f64>> {
    let arr = dict.get(key).map(|o| doc.resolve(o)).and_then(Object::as_array)?;
    Some(arr.iter().filter_map(|o| doc.resolve(o).as_f64()).collect())
}

/// Extract the `idx`-th `bits`-wide unsigned sample from a big-endian packed
/// sample buffer (`data`), MSB-first, as used by a type-0 (sampled) function's
/// stream. Samples are not byte-aligned for `bits` other than 8/16; out-of-range
/// reads yield `0`. `bits` is clamped to `1..=32`.
fn read_sample(data: &[u8], idx: u64, bits: u32) -> u32 {
    let bits = bits.clamp(1, 32);
    let start_bit = idx * bits as u64;
    let mut value: u32 = 0;
    for k in 0..bits as u64 {
        let bit_pos = start_bit + k;
        let byte = (bit_pos / 8) as usize;
        let bit = 7 - (bit_pos % 8) as u32; // MSB-first within each byte
        let b = data.get(byte).map(|&x| (x >> bit) & 1).unwrap_or(0);
        value = (value << 1) | b as u32;
    }
    value
}

/// Decrypt every object's strings and stream bytes in place when the trailer
/// declares an `/Encrypt` dictionary. A wrong or unsupported password leaves
/// the objects untouched (the document stays unreadable rather than corrupted).
fn decrypt_objects(
    objects: &mut BTreeMap<ObjectId, Object>,
    trailer: &Dictionary,
    password: &[u8],
) -> Result<()> {
    let Some(encrypt_ref) = trailer.get(b"Encrypt").and_then(Object::as_reference) else {
        return Ok(()); // not encrypted
    };
    let id0 = match trailer.get(b"ID") {
        Some(Object::Array(items)) => match items.first() {
            Some(Object::String(b, _)) => b.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    let Some(encrypt_dict) = objects.get(&encrypt_ref).and_then(Object::as_dict).cloned() else {
        return Ok(()); // malformed /Encrypt reference — best effort, leave as-is
    };
    let Some(security) = crate::security::Security::open(&encrypt_dict, &id0, password) else {
        return Err(EngineError::Unsupported(
            "encrypted PDF: wrong password or unsupported security handler".into(),
        ));
    };
    let ids: Vec<ObjectId> = objects.keys().copied().collect();
    for id in ids {
        if id == encrypt_ref {
            continue;
        }
        // Cross-reference streams are never encrypted.
        let is_xref = objects
            .get(&id)
            .and_then(Object::as_dict)
            .and_then(|d| d.get(b"Type"))
            .and_then(Object::as_name)
            == Some(b"XRef".as_slice());
        if is_xref {
            continue;
        }
        if let Some(obj) = objects.remove(&id) {
            objects.insert(id, decrypt_in_object(obj, id.0, id.1, &security));
        }
    }
    Ok(())
}

fn decrypt_in_object(
    object: Object,
    num: u32,
    gen: u16,
    sec: &crate::security::Security,
) -> Object {
    match object {
        Object::String(bytes, kind) => Object::String(sec.decrypt(num, gen, &bytes), kind),
        Object::Array(items) => Object::Array(
            items
                .into_iter()
                .map(|o| decrypt_in_object(o, num, gen, sec))
                .collect(),
        ),
        Object::Dictionary(dict) => Object::Dictionary(decrypt_in_dict(dict, num, gen, sec)),
        Object::Stream(stream) => {
            let dict = decrypt_in_dict(stream.dict, num, gen, sec);
            let raw = sec.decrypt(num, gen, &stream.raw);
            Object::Stream(Stream::new(dict, raw))
        }
        other => other,
    }
}

fn decrypt_in_dict(
    dict: Dictionary,
    num: u32,
    gen: u16,
    sec: &crate::security::Security,
) -> Dictionary {
    let mut out = Dictionary::new();
    for (key, value) in dict.0 {
        out.0.insert(key, decrypt_in_object(value, num, gen, sec));
    }
    out
}

/// Sanitize a family name into a valid PostScript `/BaseFont` name (ASCII
/// letters/digits/hyphen; spaces and other characters dropped).
fn postscript_name(family: &str) -> String {
    let cleaned: String = family
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    if cleaned.is_empty() {
        "EmbeddedFont".to_string()
    } else {
        cleaned
    }
}

/// Extract the `CFF ` table from an OpenType-CFF (`OTTO`) sfnt and build the
/// glyph-id → Unicode-string ligature map from its charset names. Empty when the
/// program isn't an OTTO with a parseable `CFF ` table. Used to add CFF ligature
/// (`ffi`, …) expansions to a font's `/ToUnicode`.
fn cff_ligature_unicode_from_otf(program: &[u8]) -> std::collections::BTreeMap<u16, String> {
    let empty = std::collections::BTreeMap::new();
    if program.get(0..4) != Some(b"OTTO".as_slice()) || program.len() < 12 {
        return empty;
    }
    let be16 = |o: usize| -> usize {
        if o + 2 <= program.len() {
            ((program[o] as usize) << 8) | program[o + 1] as usize
        } else {
            0
        }
    };
    let be32 = |o: usize| -> usize {
        if o + 4 <= program.len() {
            ((program[o] as usize) << 24)
                | ((program[o + 1] as usize) << 16)
                | ((program[o + 2] as usize) << 8)
                | program[o + 3] as usize
        } else {
            0
        }
    };
    let num_tables = be16(4);
    for i in 0..num_tables {
        let rec = 12 + i * 16;
        if rec + 16 > program.len() {
            break;
        }
        if &program[rec..rec + 4] == b"CFF " {
            let off = be32(rec + 8);
            let len = be32(rec + 12);
            if let Some(bytes) = program.get(off..off + len) {
                if let Some(cff) = crate::font::cff::CffFont::parse(bytes) {
                    return crate::font::cff_to_otf::cff_gid_unicode_strings(&cff);
                }
            }
            break;
        }
    }
    empty
}

/// Recognise a raw Type 1 font program: a `.pfb` (binary segment marker
/// `0x80 0x01`), a `.pfa` (`%!` clear-text header), or a PDF `FontFile` body —
/// all of which carry an `eexec`-encrypted private section. The `eexec` token is
/// the reliable discriminator; the leading-byte checks short-circuit common
/// cases. Called only after the glyf/OpenType-CFF/bare-CFF checks, so it never
/// shadows those flavours.
fn is_raw_type1(program: &[u8]) -> bool {
    let pfb = program.first() == Some(&0x80) && program.get(1) == Some(&0x01);
    let pfa = program.get(0..2) == Some(b"%!".as_slice());
    let has_eexec = program
        .windows(5)
        .take(4096) // the eexec keyword sits early, in the clear-text header.
        .any(|w| w == b"eexec");
    pfb || (pfa && has_eexec) || (has_eexec && program.first() != Some(&1))
}

/// Linearly interpolate an SVG gradient's stops at `t` ∈ [0,1] → 8-bit RGB (for
/// the shading function samples). Stop alpha is not applied (opaque shading).
fn sample_svg_gradient(stops: &[crate::svg::GradStop], t: f64) -> [u8; 3] {
    let to8 = |c: [f64; 3]| {
        [
            (c[0] * 255.0).round().clamp(0.0, 255.0) as u8,
            (c[1] * 255.0).round().clamp(0.0, 255.0) as u8,
            (c[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        ]
    };
    let Some(first) = stops.first() else {
        return [0, 0, 0];
    };
    let last = stops[stops.len() - 1];
    if t <= first.offset {
        return to8(first.rgb);
    }
    if t >= last.offset {
        return to8(last.rgb);
    }
    for w in stops.windows(2) {
        let (a, b) = (w[0], w[1]);
        if t >= a.offset && t <= b.offset {
            let span = (b.offset - a.offset).max(1e-9);
            let f = ((t - a.offset) / span).clamp(0.0, 1.0);
            return to8([
                a.rgb[0] + (b.rgb[0] - a.rgb[0]) * f,
                a.rgb[1] + (b.rgb[1] - a.rgb[1]) * f,
                a.rgb[2] + (b.rgb[2] - a.rgb[2]) * f,
            ]);
        }
    }
    to8(last.rgb)
}

/// First index of `needle` within `haystack`.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Scan a whole PDF for `n g obj` definitions and `trailer` dictionaries.
fn scan(data: &[u8]) -> (BTreeMap<ObjectId, Object>, Dictionary) {
    let mut objects = BTreeMap::new();
    let mut trailer = Dictionary::new();
    let mut lexer = Lexer::new(data);

    loop {
        let token = match lexer.next_token() {
            Ok(Token::Eof) => break,
            Ok(token) => token,
            Err(_) => continue, // lexer guarantees progress on error
        };

        match token {
            Token::Integer(n) if n >= 0 => {
                let after_n = lexer.position();
                if let Some(id) = try_object_header(&mut lexer, n) {
                    let body_start = lexer.position();
                    let mut parser = Parser::at(data, body_start);
                    if let Ok(object) = parser.parse_value() {
                        objects.insert(id, object);
                        lexer.set_position(parser.position());
                        continue;
                    }
                }
                lexer.set_position(after_n);
            }
            Token::Keyword(k) if k == b"trailer" => {
                let mut parser = Parser::at(data, lexer.position());
                if let Ok(Object::Dictionary(dict)) = parser.parse_value() {
                    for (key, value) in dict.0 {
                        trailer.0.insert(key, value); // last trailer wins (most recent)
                    }
                    lexer.set_position(parser.position());
                }
            }
            _ => {}
        }
    }

    (objects, trailer)
}

/// After an `Integer(n)`, check for `g obj`. Returns the object id on match,
/// leaving the lexer right after `obj`. On no match the lexer is left wherever
/// it stopped; callers rewind.
fn try_object_header(lexer: &mut Lexer, n: i64) -> Option<ObjectId> {
    let g = match lexer.next_token() {
        Ok(Token::Integer(g)) if (0..=u16::MAX as i64).contains(&g) => g,
        _ => return None,
    };
    match lexer.next_token() {
        Ok(Token::Keyword(k)) if k == b"obj" => Some((n as u32, g as u16)),
        _ => None,
    }
}

/// PDF 1.5+ keeps `/Root` in the cross-reference *stream* dictionary rather than
/// a classic `trailer`. If the scanned trailer lacks `/Root`, lift it (and
/// `/Info`) from any `/Type /XRef` stream object.
fn recover_trailer_from_xref(trailer: &mut Dictionary, objects: &BTreeMap<ObjectId, Object>) {
    if trailer.contains(b"Root") {
        return;
    }
    for object in objects.values() {
        let Some(stream) = object.as_stream() else {
            continue;
        };
        if stream.dict.get(b"Type").and_then(Object::as_name) != Some(b"XRef".as_slice()) {
            continue;
        }
        if let Some(root) = stream.dict.get(b"Root") {
            trailer.set(b"Root".to_vec(), root.clone());
        }
        if let Some(info) = stream.dict.get(b"Info") {
            trailer.set(b"Info".to_vec(), info.clone());
        }
        if trailer.contains(b"Root") {
            return;
        }
    }
}

/// PDF 1.5+ packs non-stream objects (catalog, pages, fonts…) into compressed
/// `/Type /ObjStm` streams. Decode each and add the objects it carries to the
/// map, without overriding objects already found directly.
fn extract_object_streams(objects: &mut BTreeMap<ObjectId, Object>) {
    let streams: Vec<Stream> = objects
        .values()
        .filter_map(Object::as_stream)
        .filter(|s| s.dict.get(b"Type").and_then(Object::as_name) == Some(b"ObjStm".as_slice()))
        .cloned()
        .collect();

    for stream in streams {
        let decoded = match decode_stream(&stream) {
            Ok(bytes) => bytes,
            Err(_) => continue, // a bad ObjStm must not fail the whole open
        };
        let count = stream
            .dict
            .get(b"N")
            .and_then(Object::as_i64)
            .unwrap_or(0)
            .max(0) as usize;
        let first = stream
            .dict
            .get(b"First")
            .and_then(Object::as_i64)
            .unwrap_or(0)
            .max(0) as usize;

        // The decoded stream starts with `count` pairs of (object number, offset).
        let mut header = Parser::new(&decoded);
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let number = match header.parse_value() {
                Ok(Object::Integer(v)) if v >= 0 => v as u32,
                _ => break,
            };
            let offset = match header.parse_value() {
                Ok(Object::Integer(v)) if v >= 0 => v as usize,
                _ => break,
            };
            entries.push((number, offset));
        }

        for (number, offset) in entries {
            let pos = first + offset;
            if pos >= decoded.len() {
                continue;
            }
            let mut parser = Parser::at(&decoded, pos);
            if let Ok(object) = parser.parse_value() {
                objects.entry((number, 0)).or_insert(object);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> Vec<u8> {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("../../fixtures");
        path.push(name);
        std::fs::read(&path).unwrap_or_else(|_| panic!("missing fixture {}", path.display()))
    }

    fn has_op(content: &[u8], op: &[u8]) -> bool {
        content.windows(op.len()).any(|w| w == op)
    }

    #[test]
    fn layers_create_toggle_remove_roundtrip() {
        let pdf = crate::convert::reverse::txt_to_pdf("layer test");
        let mut doc = Document::open(&pdf).unwrap();
        assert!(doc.layers().is_empty());

        // Create → visible, unlocked.
        let id = doc.add_layer("Watermark").unwrap();
        assert!(id > 0);
        let layers = doc.layers();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].name, "Watermark");
        assert!(layers[0].visible && !layers[0].locked);

        // Hide + lock.
        doc.set_layer_visibility(id, false).unwrap();
        doc.set_layer_locked(id, true).unwrap();
        let layers = doc.layers();
        assert!(!layers[0].visible && layers[0].locked);

        // Survives a save/open round-trip.
        let reopened = Document::open(&doc.save()).unwrap();
        let layers = reopened.layers();
        assert_eq!(layers.len(), 1);
        assert!(!layers[0].visible && layers[0].locked);

        // Show again, then remove.
        doc.set_layer_visibility(id, true).unwrap();
        assert!(doc.layers()[0].visible);
        doc.remove_layer(id).unwrap();
        assert!(doc.layers().is_empty());
    }

    #[test]
    fn page_resize_add_copy_roundtrip() {
        let pdf = crate::convert::reverse::txt_to_pdf("page ops");
        let mut doc = Document::open(&pdf).unwrap();
        assert_eq!(doc.page_ids().unwrap().len(), 1);

        doc.resize_page(1, 200.0, 300.0).unwrap();
        let (w, h) = {
            let mb = doc
                .page_dict(1)
                .unwrap()
                .get(b"MediaBox")
                .and_then(Object::as_array)
                .unwrap();
            (mb[2].as_f64(), mb[3].as_f64())
        };
        assert_eq!((w, h), (Some(200.0), Some(300.0)));

        assert!(doc.add_page(400.0, 500.0, 1).unwrap() > 0);
        assert_eq!(doc.page_ids().unwrap().len(), 2);

        assert!(doc.copy_page(1).unwrap() > 0);
        assert_eq!(doc.page_ids().unwrap().len(), 3);

        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(reopened.page_ids().unwrap().len(), 3);
    }

    #[test]
    fn opens_simple_text_and_decodes_content() {
        let doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let ids = doc.page_ids().unwrap();
        assert!(!ids.is_empty(), "expected at least one page");

        let content = doc.page_content(1).unwrap();
        assert!(
            has_op(&content, b"Tj") || has_op(&content, b"TJ"),
            "decoded content should contain a text operator ({} bytes)",
            content.len()
        );
    }

    #[test]
    fn opens_pdf_with_image_background() {
        // The "complex background" case: text drawn over an image.
        let doc = Document::open(&fixture("with-images.pdf")).unwrap();
        let content = doc.page_content(1).unwrap();
        // An image is painted with `Do`; if present, our inflate decoded it.
        assert!(
            has_op(&content, b"Do") || has_op(&content, b"Tj"),
            "expected drawing operators in decoded content ({} bytes)",
            content.len()
        );
    }

    #[test]
    fn reports_object_count() {
        let doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        assert!(doc.object_count() >= 3, "a PDF has several objects");
    }

    #[test]
    fn save_roundtrips_through_our_own_reader() {
        // Open a real (object-stream) PDF, re-serialize it with our writer, and
        // confirm the output re-opens with pages and decodable content intact.
        let doc = Document::open(&fixture("with-images.pdf")).unwrap();
        let saved = doc.save();

        let reopened = Document::open(&saved).unwrap();
        assert!(
            !reopened.page_ids().unwrap().is_empty(),
            "pages survived save"
        );
        let content = reopened.page_content(1).unwrap();
        assert!(
            has_op(&content, b"Do") || has_op(&content, b"Tj"),
            "content survived save ({} bytes)",
            content.len()
        );
    }

    #[test]
    fn edits_text_in_place_and_persists_through_save() {
        // The full Word-like cycle on our own engine: open a real PDF, edit a
        // text run, save with our serializer, reopen, confirm the new text.
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        assert!(!doc.page_text_runs(1).unwrap().is_empty());

        doc.replace_text_run(1, 0, "Edited by gigapdf-engine")
            .unwrap();
        let saved = doc.save();

        let reopened = Document::open(&saved).unwrap();
        let runs = reopened.page_text_runs(1).unwrap();
        assert!(
            runs.iter()
                .any(|r| r.text.contains("Edited by gigapdf-engine")),
            "edited text should survive the save; got {:?}",
            runs.iter().map(|r| r.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn adds_a_frame_and_persists_through_save() {
        use crate::content::ElementKind;
        let paths = |doc: &Document| {
            doc.page_elements(1)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == ElementKind::Path)
                .count()
        };

        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let before = paths(&doc);
        doc.add_rectangle(
            1,
            50.0,
            50.0,
            200.0,
            100.0,
            Some([0.0, 0.0, 0.0]),
            None,
            1.5,
            1.0,
        )
        .unwrap();

        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(
            paths(&reopened),
            before + 1,
            "one frame added and persisted"
        );
    }

    #[test]
    fn adds_lists_and_persists_annotations() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let before = doc.page_annotations(1).unwrap().len();

        doc.add_square_annotation(
            1,
            [50.0, 50.0, 250.0, 150.0],
            Some([1.0, 0.0, 0.0]),
            None,
            2.0,
        )
        .unwrap();
        doc.add_highlight(1, [60.0, 200.0, 260.0, 220.0], [1.0, 1.0, 0.0])
            .unwrap();
        doc.add_free_text(
            1,
            [50.0, 300.0, 300.0, 340.0],
            "Note",
            14.0,
            [0.0, 0.0, 1.0],
        )
        .unwrap();

        let annots = Document::open(&doc.save())
            .unwrap()
            .page_annotations(1)
            .unwrap();
        assert_eq!(annots.len(), before + 3, "three annotations persisted");
        assert!(annots.iter().any(|a| a.subtype == "Square"));
        assert!(annots.iter().any(|a| a.subtype == "Highlight"));
        assert!(annots
            .iter()
            .any(|a| a.subtype == "FreeText" && a.contents == "Note"));
    }

    #[test]
    fn rotates_a_page() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.rotate_page(1, 90).unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        let rotate = reopened
            .page_dict(1)
            .unwrap()
            .get(b"Rotate")
            .and_then(|o| o.as_i64());
        assert_eq!(rotate, Some(90));
    }

    #[test]
    fn sets_and_reads_metadata() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.set_metadata("Title", "My Title").unwrap();
        doc.set_metadata("Author", "Rony").unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(reopened.get_metadata("Title"), Some("My Title".to_string()));
        assert_eq!(reopened.get_metadata("Author"), Some("Rony".to_string()));
    }

    #[test]
    fn embeds_a_png_image_as_xobject() {
        let pdf = crate::convert::reverse::txt_to_pdf("image host page");
        let mut doc = Document::open(&pdf).unwrap();

        // A 2x2 opaque RGB image with four distinct colours.
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
        ];
        let png = crate::raster::png::encode_png(2, 2, &rgba);
        doc.add_image(1, &png, 100.0, 100.0, 64.0, 64.0, 1.0)
            .unwrap();

        // Reopen the serialized document and confirm the image XObject survives.
        let reopened = Document::open(&doc.save()).unwrap();
        let images = reopened.page_images(1);
        let embedded = images
            .values()
            .find(|img| img.width == 2 && img.height == 2)
            .expect("2x2 image XObject present after round-trip");
        // PNG → Flate /DeviceRGB embed is lossless: the samples must match.
        assert_eq!(embedded.rgba, rgba, "round-tripped pixels match the source");
    }

    #[test]
    fn deletes_a_page() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        let before = doc.page_count();
        assert!(before > 1, "fixture should have several pages");
        doc.delete_page(1).unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(reopened.page_count(), before - 1);
    }

    #[test]
    fn moves_a_page() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        let ids = doc.page_ids().unwrap();
        assert!(ids.len() >= 2);
        let first = ids[0];

        doc.move_page(1, ids.len() as u32).unwrap();
        let reordered = doc.page_ids().unwrap();
        assert_eq!(reordered.len(), ids.len());
        assert_eq!(
            reordered.last().copied(),
            Some(first),
            "page 1 moved to last"
        );

        assert_eq!(Document::open(&doc.save()).unwrap().page_count(), ids.len());
    }

    #[test]
    fn extracts_a_single_page() {
        let doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        assert!(doc.page_count() >= 2);
        let extracted = doc.extract_pages(&[1]).unwrap();
        let reopened = Document::open(&extracted).unwrap();
        assert_eq!(reopened.page_count(), 1, "extracted exactly one page");
    }

    #[test]
    fn merges_pages_from_another_pdf() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let before = doc.page_count();
        let other = fixture("multi-page.pdf");
        let other_count = Document::open(&other).unwrap().page_count();

        doc.append_pages_from(&other).unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(reopened.page_count(), before + other_count, "pages merged");
    }

    #[test]
    fn lists_form_fields() {
        let doc = Document::open(&fixture("with-forms.pdf")).unwrap();
        let fields = doc.form_fields().unwrap();
        eprintln!(
            "with-forms.pdf -> {} field(s): {:?}",
            fields.len(),
            fields
                .iter()
                .map(|f| (f.name.clone(), f.field_type.clone(), f.value.clone()))
                .collect::<Vec<_>>()
        );
        assert!(fields
            .iter()
            .any(|f| f.name == "name" && f.field_type == "Tx" && f.value == "John Doe"));
        assert!(fields
            .iter()
            .any(|f| f.name == "country" && f.field_type == "Ch" && f.value == "France"));
    }

    #[test]
    fn classifies_every_field_kind() {
        use crate::form::FieldKind;
        let doc = Document::open(&fixture("with-forms.pdf")).unwrap();
        let fields = doc.form_fields().unwrap();
        for f in &fields {
            eprintln!(
                "  {:<10} type={} kind={:?} flags={:#06x} opts={:?}",
                f.name,
                f.field_type,
                f.kind(),
                f.flags,
                f.options
            );
        }
        let by = |n: &str| fields.iter().find(|f| f.name == n).unwrap();
        assert_eq!(by("name").kind(), FieldKind::Text);
        assert_eq!(by("country").kind(), FieldKind::ComboBox);
        // `agree` and `gender` are buttons; at least one must be a checkbox/radio.
        assert!(matches!(
            by("gender").kind(),
            FieldKind::Radio | FieldKind::Checkbox
        ));
    }

    #[test]
    fn fills_text_checkbox_radio_and_choice() {
        let mut doc = Document::open(&fixture("with-forms.pdf")).unwrap();

        doc.set_text_field("name", "Jane Smith").unwrap();
        doc.set_text_field("email", "jane@example.com").unwrap();
        doc.set_checkbox("agree", true).unwrap();
        doc.set_choice_field("country", &["Germany"]).unwrap();

        // `gender` is a radio group: pick whichever export option it offers.
        let gender = doc
            .form_fields()
            .unwrap()
            .into_iter()
            .find(|f| f.name == "gender")
            .unwrap();
        if gender.kind() == crate::form::FieldKind::Radio {
            let option = gender.options.first().cloned().unwrap();
            doc.set_radio("gender", &option).unwrap();
        }

        let reopened = Document::open(&doc.save()).unwrap();
        let fields = reopened.form_fields().unwrap();
        let value = |n: &str| fields.iter().find(|f| f.name == n).unwrap().value.clone();

        assert_eq!(value("name"), "Jane Smith");
        assert_eq!(value("email"), "jane@example.com");
        assert_eq!(value("agree"), "Yes");
        assert_eq!(value("country"), "Germany");
    }

    #[test]
    fn rejects_unknown_choice_option() {
        let mut doc = Document::open(&fixture("with-forms.pdf")).unwrap();
        // `country` is a non-editable combo, so an off-list value must fail.
        let result = doc.set_choice_field("country", &["Atlantis"]);
        assert!(
            result.is_err(),
            "off-list value on a closed combo must error"
        );
    }

    #[test]
    fn adds_and_reads_hyperlinks() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        doc.add_uri_link(1, [72.0, 700.0, 300.0, 720.0], "https://giga-pdf.com")
            .unwrap();
        doc.add_goto_link(1, [72.0, 650.0, 300.0, 670.0], 2)
            .unwrap();

        let reopened = Document::open(&doc.save()).unwrap();
        let links = reopened.page_links(1).unwrap();
        assert_eq!(links.len(), 2, "two links round-tripped");
        assert!(
            links
                .iter()
                .any(|l| l.target == LinkTarget::Uri("https://giga-pdf.com".to_string())),
            "external URI link survived"
        );
        assert!(
            links.iter().any(|l| l.target == LinkTarget::Page(2)),
            "internal go-to-page link resolved to page 2 after renumbering"
        );
    }

    #[test]
    fn builds_and_reads_outline() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        let toc: Vec<(String, Option<u32>, usize)> = vec![
            ("Chapter 1".to_string(), Some(1), 0),
            ("Section 1.1".to_string(), Some(1), 1),
            ("Section 1.2".to_string(), Some(2), 1),
            ("Chapter 2".to_string(), Some(3), 0),
        ];
        doc.set_outline(&toc).unwrap();

        let reopened = Document::open(&doc.save()).unwrap();
        let items = reopened.outline_items();
        assert_eq!(items.len(), 4, "all outline items flattened");
        assert_eq!(items[0].title, "Chapter 1");
        assert_eq!(items[0].level, 0);
        assert_eq!(items[0].page, Some(1));
        assert_eq!(items[1].title, "Section 1.1");
        assert_eq!(items[1].level, 1, "nested under Chapter 1");
        assert_eq!(items[3].title, "Chapter 2");
        assert_eq!(items[3].level, 0);
        assert_eq!(
            items[3].page,
            Some(3),
            "dest page resolved after renumbering"
        );
    }

    #[test]
    fn clears_the_outline() {
        let mut doc = Document::open(&fixture("multi-page.pdf")).unwrap();
        doc.set_outline(&[("Only".to_string(), Some(1), 0)])
            .unwrap();
        doc.set_outline(&[]).unwrap();
        let reopened = Document::open(&doc.save()).unwrap();
        assert!(reopened.outline_items().is_empty(), "outline cleared");
    }

    #[test]
    fn adds_text_markup_and_ink_and_stamp() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let red = [1.0, 0.0, 0.0];
        doc.add_underline(1, [72.0, 700.0, 300.0, 712.0], red)
            .unwrap();
        doc.add_strike_out(1, [72.0, 680.0, 300.0, 692.0], red)
            .unwrap();
        doc.add_ink(
            1,
            &[vec![(100.0, 100.0), (130.0, 140.0), (160.0, 110.0)]],
            [0.0, 0.0, 1.0],
            2.0,
        )
        .unwrap();
        doc.add_stamp(1, [400.0, 700.0, 520.0, 740.0], "DRAFT", red)
            .unwrap();

        let reopened = Document::open(&doc.save()).unwrap();
        let subtypes: Vec<String> = reopened
            .page_annotations(1)
            .unwrap()
            .into_iter()
            .map(|a| a.subtype)
            .collect();
        for expected in ["Underline", "StrikeOut", "Ink", "Stamp"] {
            assert!(
                subtypes.iter().any(|s| s == expected),
                "{expected} annotation present, got {subtypes:?}"
            );
        }
    }

    #[test]
    fn flattens_annotations_into_content() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.add_highlight(1, [72.0, 700.0, 300.0, 712.0], [1.0, 1.0, 0.0])
            .unwrap();
        doc.add_free_text(
            1,
            [72.0, 650.0, 300.0, 680.0],
            "Note",
            12.0,
            [0.0, 0.0, 0.0],
        )
        .unwrap();

        let baked = doc.flatten_annotations(1).unwrap();
        assert_eq!(baked, 2, "both annotations baked");

        let reopened = Document::open(&doc.save()).unwrap();
        assert!(
            reopened.page_annotations(1).unwrap().is_empty(),
            "markup removed after flatten"
        );
        // The appearances are now XObject draws in the page content.
        let images = reopened
            .page_elements(1)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == content::ElementKind::Image)
            .count();
        assert!(
            images >= 2,
            "baked appearances drawn as XObjects ({images})"
        );
    }

    #[test]
    fn signs_a_document() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let randomness: Vec<u8> = (0..256).map(|i| (i * 53 + 7) as u8).collect();
        let signer = crate::sign::Signer::generate(
            "GigaPDF Tester",
            "260614000000Z",
            "360614000000Z",
            512,
            &randomness,
        )
        .unwrap();

        let signed = doc
            .sign(&signer, "GigaPDF Tester", "Approval", "D:20260614120000Z")
            .unwrap();

        assert_eq!(&signed[0..5], b"%PDF-", "valid PDF header");
        // The fixed-width /ByteRange placeholders were patched with real offsets.
        assert!(
            !signed.windows(10).any(|w| w == b"9999999999"),
            "ByteRange placeholders patched"
        );
        let text = String::from_utf8_lossy(&signed);
        assert!(
            text.contains("adbe.pkcs7.detached"),
            "detached signature subfilter"
        );
        assert!(text.contains("/ByteRange"), "byte range present");
        // The signed file still parses as a structurally valid PDF.
        let reopened = Document::open(&signed).unwrap();
        assert!(reopened.page_count() >= 1, "signed PDF re-opens");
    }

    #[test]
    fn signs_a_document_with_an_imported_p12_identity() {
        // A real OpenSSL .p12 (PBES2/AES) imported and used to sign — the
        // embedded certificate must be the user's, not a self-signed one.
        const MODERN_P12: &[u8] = include_bytes!("sign/fixtures/modern.p12");
        const CERT_DER: &[u8] = include_bytes!("sign/fixtures/cert.der");
        let identity = crate::sign::pkcs12::parse(MODERN_P12, "gigapdf").unwrap();

        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let signed = doc
            .sign_p12(
                &identity,
                "GigaPDF Tester",
                "Approval",
                "D:20260614120000Z",
                "Paris",
                "tester@example.com",
            )
            .unwrap();

        assert_eq!(&signed[0..5], b"%PDF-", "valid PDF header");
        let text = String::from_utf8_lossy(&signed);
        assert!(
            text.contains("adbe.pkcs7.detached"),
            "detached signature subfilter"
        );
        // The CMS /Contents (uppercase hex) must contain the imported cert DER.
        let cert_hex: String = CERT_DER.iter().map(|b| format!("{b:02X}")).collect();
        assert!(
            text.contains(&cert_hex),
            "imported certificate embedded in the signature"
        );
        assert!(Document::open(&signed).unwrap().page_count() >= 1);
    }

    #[test]
    fn add_text_standard_draws_with_base14_fonts() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.add_text_standard(1, 100.0, 100.0, 14.0, "Bonjour", "Times-Bold", [0.0, 0.0, 0.0], 1.0, 0.0)
            .unwrap();
        doc.add_text_standard(1, 100.0, 80.0, 12.0, "Code", "Courier", [0.0, 0.0, 0.0], 1.0, 0.0)
            .unwrap();
        // A name outside the base-14 set is rejected.
        assert!(doc
            .add_text_standard(1, 0.0, 0.0, 10.0, "x", "NotAFont", [0.0; 3], 1.0, 0.0)
            .is_err());

        let bytes = doc.save();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("Times-Bold"), "Times-Bold registered");
        assert!(text.contains("Courier"), "Courier registered");
        // Two distinct standard fonts must get distinct resource names.
        assert!(text.contains("/GpStd"), "unique standard-font resources");
        assert!(Document::open(&bytes).unwrap().page_count() >= 1, "re-opens");
    }

    #[test]
    fn embedded_fonts_lists_embedded_programs() {
        let doc = Document::open(&fixture("embedded-fonts.pdf")).unwrap();
        let fonts = doc.embedded_fonts();
        assert!(!fonts.is_empty(), "fixture carries embedded fonts");
        assert!(
            fonts
                .iter()
                .any(|f| f.base_font.contains("DejaVu") && f.format == "truetype"),
            "DejaVu TrueType is listed: {fonts:?}"
        );
        // Round-trip: the listed font can be pulled out and re-embedded.
        let name = &fonts.iter().find(|f| f.format == "truetype").unwrap().base_font;
        let (program, format) = doc.extract_font_program(name).expect("extractable");
        assert_eq!(format, "truetype");
        assert!(!program.is_empty());

        // A standard-font-only PDF lists nothing embedded.
        let plain = Document::open(&fixture("simple-text.pdf")).unwrap();
        assert!(plain.embedded_fonts().is_empty(), "no embedded program");
    }

    #[test]
    fn redaction_removes_content_from_the_stream() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        let before = doc.page_text_runs(1).unwrap().len();
        assert!(before > 0, "fixture has text");

        // No cover: pure stream deletion so a complex background would survive.
        let removed = doc.redact_region(1, 0.0, 0.0, 612.0, 792.0, None).unwrap();
        assert!(removed > 0, "elements were removed");

        // After save + reopen, the redacted text is gone from the stream — not
        // merely covered. (A cosmetic overlay would leave the runs intact.)
        let reopened = Document::open(&doc.save()).unwrap();
        let after = reopened.page_text_runs(1).unwrap().len();
        assert!(after < before, "text runs removed ({before} → {after})");
    }

    #[test]
    fn encrypts_and_decrypts_round_trip() {
        let original = Document::open(&fixture("simple-text.pdf")).unwrap();
        let want: String = original
            .page_text_runs(1)
            .unwrap()
            .iter()
            .map(|r| r.text.clone())
            .collect();
        assert!(!want.is_empty());

        let encrypted = original.save_encrypted(b"s3cret", b"", b"file-id-bytes-01", b"", 0, -44);

        // Opening with the right password recovers the exact text.
        let opened = Document::open_with_password(&encrypted, b"s3cret").unwrap();
        let got: String = opened
            .page_text_runs(1)
            .unwrap()
            .iter()
            .map(|r| r.text.clone())
            .collect();
        assert_eq!(got, want, "decrypted text matches original");

        // The wrong (empty) password is rejected at open time.
        assert!(
            Document::open(&encrypted).is_err(),
            "wrong password must be rejected"
        );
    }

    #[test]
    fn encryption_info_reads_p_v_r_without_password() {
        let doc = Document::open(&fixture("simple-text.pdf")).unwrap();

        // Unencrypted document → encrypted: false.
        let plain = Document::encryption_info(&doc.save());
        assert!(!plain.encrypted);
        assert_eq!(plain.permissions, 0);

        // AES-256 (V5/R6) encrypted → info read WITHOUT the password.
        let fek = [0x33u8; 32];
        let enc = doc.save_encrypted(b"user", b"owner", b"id0-1234567890ab", &fek, 2, -44);
        let info = Document::encryption_info(&enc);
        assert!(info.encrypted);
        assert_eq!(info.version, 5);
        assert_eq!(info.revision, 6);
        assert_eq!(info.permissions, -44);
    }

    #[test]
    fn add_watermark_stamps_rotated_text() {
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.add_watermark(
            1,
            100.0,
            400.0,
            48.0,
            "CONFIDENTIAL",
            [0.5, 0.5, 0.5],
            0.3,
            45.0,
        )
        .unwrap();
        let content = String::from_utf8_lossy(&doc.page_content(1).unwrap()).into_owned();
        assert!(
            content.contains("(CONFIDENTIAL) Tj"),
            "watermark text drawn"
        );
        assert!(content.contains("Tm"), "rotation via text matrix");
        assert!(content.contains(" gs"), "opacity via ExtGState");
        // Serializes + re-opens cleanly (standard Helvetica needs no embedding).
        let saved = doc.save();
        assert!(Document::open(&saved).is_ok());
    }

    #[test]
    fn helvetica_width_matches_afm() {
        // Space is 278/1000 em; "AV" = 667 + 667.
        assert!((Document::helvetica_width(" ", 1000.0) - 278.0).abs() < 1e-6);
        assert!((Document::helvetica_width("AV", 1000.0) - 1334.0).abs() < 1e-6);
        assert!(Document::helvetica_width("WWWW", 12.0) > Document::helvetica_width("iiii", 12.0));
    }

    #[test]
    fn renders_a_page_to_png() {
        // Add a vector rectangle so there is guaranteed ink, then rasterize.
        let mut doc = Document::open(&fixture("simple-text.pdf")).unwrap();
        doc.add_rectangle(
            1,
            50.0,
            50.0,
            200.0,
            100.0,
            None,
            Some([1.0, 0.0, 0.0]),
            0.0,
            1.0,
        )
        .unwrap();
        let png = doc.render_page(1, 1.0).unwrap();
        assert_eq!(&png[0..4], &[0x89, b'P', b'N', b'G'], "valid PNG header");
        assert!(png.len() > 1000, "non-trivial PNG ({} bytes)", png.len());
    }

    /// A blank 200×200 page → `Document`, for annotation-render tests.
    fn blank_200() -> Document {
        let mut b = crate::convert::build::PdfBuilder::new();
        b.add_page(200.0, 200.0);
        Document::open(&b.finish()).unwrap()
    }

    /// Count non-white pixels of a rendered page inside a device-pixel rectangle
    /// `[x0, y0, x1, y1)` (top-left origin). Asserts a valid PNG first.
    fn nonwhite_in(png: &[u8], x0: usize, y0: usize, x1: usize, y1: usize) -> usize {
        let img = crate::raster::decode_png(png).expect("valid PNG");
        let w = img.width as usize;
        let mut n = 0;
        for y in y0..y1.min(img.height as usize) {
            for x in x0..x1.min(w) {
                let i = (y * w + x) * 4;
                if img.rgba[i] != 255 || img.rgba[i + 1] != 255 || img.rgba[i + 2] != 255 {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn renders_square_annotation_appearance_into_its_rect() {
        // A red-filled Square annotation carries an `/AP /N` appearance. The page
        // renderer must paint that appearance — previously annotations were
        // skipped entirely and the page rendered all-white.
        let mut doc = blank_200();
        doc.add_square_annotation(
            1,
            [50.0, 50.0, 150.0, 150.0],
            None,
            Some([1.0, 0.0, 0.0]),
            2.0,
        )
        .unwrap();
        let png = doc.render_page(1, 1.0).unwrap();

        // Page is 200 tall → user y is flipped to device y. The Rect maps to the
        // device box rows/cols 50..150; its centre must be the red fill.
        let img = crate::raster::decode_png(&png).unwrap();
        let centre = (100 * img.width as usize + 100) * 4;
        assert_eq!(
            &img.rgba[centre..centre + 3],
            &[255, 0, 0],
            "annotation appearance fill is drawn inside the Rect"
        );
        // Inside the Rect: substantial ink. Outside (top-left 40×40 corner): none.
        assert!(
            nonwhite_in(&png, 55, 55, 145, 145) > 5000,
            "appearance fills its Rect"
        );
        assert_eq!(
            nonwhite_in(&png, 0, 0, 40, 40),
            0,
            "nothing painted outside the Rect"
        );
    }

    #[test]
    fn hidden_annotation_appearance_is_not_rendered() {
        // Same red Square, but flagged Hidden (`/F` bit 2). A viewer must not
        // paint it — the page stays all-white.
        let mut doc = blank_200();
        doc.add_square_annotation(
            1,
            [50.0, 50.0, 150.0, 150.0],
            None,
            Some([1.0, 0.0, 0.0]),
            2.0,
        )
        .unwrap();
        // Set the Hidden flag on the (only) annotation object.
        let annot_id = *doc
            .objects
            .iter()
            .find(|(_, o)| {
                o.as_dict()
                    .and_then(|d| d.get(b"Subtype"))
                    .and_then(Object::as_name)
                    == Some(b"Square".as_slice())
            })
            .map(|(id, _)| id)
            .expect("square annotation object");
        if let Some(mut dict) = doc
            .objects
            .get(&annot_id)
            .and_then(Object::as_dict)
            .cloned()
        {
            dict.set(b"F".to_vec(), Object::Integer(2)); // Hidden
            doc.objects.insert(annot_id, Object::Dictionary(dict));
        }
        let png = doc.render_page(1, 1.0).unwrap();
        assert_eq!(
            nonwhite_in(&png, 0, 0, 200, 200),
            0,
            "hidden annotation must not be painted"
        );
    }

    #[test]
    fn appearance_matrix_scales_and_translates_bbox_onto_rect() {
        // BBox [0 0 10 10], identity /Matrix, Rect [100 100 150 140]:
        // the box must map onto the Rect with sx = 50/10 = 5, sy = 40/10 = 4 and
        // origin translated to (100, 100).
        let m = appearance_matrix(
            [0.0, 0.0, 10.0, 10.0],
            content::PageMatrix::IDENTITY,
            [100.0, 100.0, 150.0, 140.0],
        )
        .expect("non-degenerate mapping");
        let (x0, y0) = m.apply(0.0, 0.0);
        let (x1, y1) = m.apply(10.0, 10.0);
        assert!(
            (x0 - 100.0).abs() < 1e-9 && (y0 - 100.0).abs() < 1e-9,
            "origin → Rect corner"
        );
        assert!(
            (x1 - 150.0).abs() < 1e-9 && (y1 - 140.0).abs() < 1e-9,
            "far corner → Rect corner"
        );

        // The engine's own annotations use BBox == Rect, identity Matrix → the
        // mapping reduces to the identity.
        let id = appearance_matrix(
            [50.0, 50.0, 150.0, 150.0],
            content::PageMatrix::IDENTITY,
            [50.0, 50.0, 150.0, 150.0],
        )
        .unwrap();
        let (px, py) = id.apply(75.0, 80.0);
        assert!(
            (px - 75.0).abs() < 1e-9 && (py - 80.0).abs() < 1e-9,
            "identity for BBox==Rect"
        );

        // Degenerate Rect (zero height) → no mapping.
        assert!(appearance_matrix(
            [0.0, 0.0, 10.0, 10.0],
            content::PageMatrix::IDENTITY,
            [10.0, 10.0, 20.0, 10.0],
        )
        .is_none());
    }

    #[test]
    fn appearance_with_offset_bbox_is_scaled_onto_rect() {
        // A hand-built annotation whose appearance draws a green fill across its
        // own small BBox [0 0 10 10], placed in a larger Rect. Exercises the
        // general (non-identity) BBox→Rect appearance transform, not just the
        // engine's own BBox==Rect annotations.
        let mut doc = blank_200();
        let rect = [40.0, 60.0, 160.0, 120.0]; // device rows 80..140, cols 40..160

        // Appearance form XObject: BBox [0 0 10 10], fills it green.
        let appearance = b"0 1 0 rg 0 0 10 10 re f".to_vec();
        let mut form = Dictionary::new();
        form.set(b"Type".to_vec(), Object::Name(b"XObject".to_vec()));
        form.set(b"Subtype".to_vec(), Object::Name(b"Form".to_vec()));
        form.set(
            b"BBox".to_vec(),
            Object::Array(vec![
                Object::Real(0.0),
                Object::Real(0.0),
                Object::Real(10.0),
                Object::Real(10.0),
            ]),
        );
        form.set(b"Length".to_vec(), Object::Integer(appearance.len() as i64));
        let form_id = (doc.next_object_number(), 0u16);
        doc.objects
            .insert(form_id, Object::Stream(Stream::new(form, appearance)));

        // Annotation dict with /AP /N → form, and a Rect bigger than the BBox.
        let mut ap = Dictionary::new();
        ap.set(b"N".to_vec(), Object::Reference(form_id));
        let mut annot = Dictionary::new();
        annot.set(b"Type".to_vec(), Object::Name(b"Annot".to_vec()));
        annot.set(b"Subtype".to_vec(), Object::Name(b"Square".to_vec()));
        annot.set(
            b"Rect".to_vec(),
            Object::Array(rect.iter().map(|&v| Object::Real(v)).collect()),
        );
        annot.set(b"AP".to_vec(), Object::Dictionary(ap));
        let annot_id = (doc.next_object_number(), 0u16);
        doc.objects.insert(annot_id, Object::Dictionary(annot));

        // Attach to the page's /Annots.
        let page_id = doc.page_object_id(1).unwrap();
        let mut page = doc
            .objects
            .get(&page_id)
            .and_then(Object::as_dict)
            .unwrap()
            .clone();
        page.set(
            b"Annots".to_vec(),
            Object::Array(vec![Object::Reference(annot_id)]),
        );
        doc.objects.insert(page_id, Object::Dictionary(page));

        let png = doc.render_page(1, 1.0).unwrap();
        // The 10×10 appearance must be scaled to fill the whole Rect: its centre
        // (device row 110, col 100) is green, and most of the Rect is inked.
        let img = crate::raster::decode_png(&png).unwrap();
        let centre = (110 * img.width as usize + 100) * 4;
        assert_eq!(
            &img.rgba[centre..centre + 3],
            &[0, 255, 0],
            "scaled appearance fills the Rect centre"
        );
        assert!(
            nonwhite_in(&png, 45, 85, 155, 135) > 4000,
            "appearance scaled across the Rect, not left at BBox size"
        );
    }

    #[test]
    fn rasterizer_honours_png_smask_alpha() {
        use crate::raster::png::encode_png;
        // 2×2 RGBA: top-left fully transparent, the rest opaque red.
        let rgba = [
            255, 0, 0, 0, 255, 0, 0, 255, //
            255, 0, 0, 255, 255, 0, 0, 255,
        ];
        let png = encode_png(2, 2, &rgba);
        let mut doc = Document::open(&crate::convert::reverse::txt_to_pdf("image host")).unwrap();
        doc.add_image(1, &png, 100.0, 100.0, 50.0, 50.0, 1.0)
            .unwrap();

        // Re-open the serialized PDF and decode the image the way the rasterizer
        // does: the `/SMask` must surface as per-pixel alpha (0 where transparent).
        let reopened = Document::open(&doc.save()).unwrap();
        let imgs = reopened.page_images(1);
        assert_eq!(imgs.len(), 1, "one image XObject decoded");
        let alphas: Vec<u8> = imgs
            .values()
            .next()
            .unwrap()
            .rgba
            .chunks_exact(4)
            .map(|p| p[3])
            .collect();
        assert!(
            alphas.contains(&0),
            "transparent pixel survived: {alphas:?}"
        );
        assert!(alphas.contains(&255), "opaque pixels survived: {alphas:?}");
    }

    #[test]
    fn add_svg_emits_native_vector_paths() {
        let svg = r##"<svg viewBox="0 0 100 100">
            <rect x="10" y="10" width="80" height="80" fill="#3366cc"/>
            <circle cx="50" cy="50" r="20" fill="none" stroke="red" stroke-width="3"/>
        </svg>"##;
        let mut doc = Document::open(&crate::convert::reverse::txt_to_pdf("svg host")).unwrap();
        doc.add_svg(1, svg, 100.0, 100.0, 200.0, 200.0).unwrap();
        let content = String::from_utf8_lossy(&doc.page_content(1).unwrap()).into_owned();
        // Filled rectangle: fill colour set + `f` paint.
        assert!(
            content.contains(" rg\n") && content.contains("\nf\n"),
            "filled rect ops: {content}"
        );
        // Stroked circle: stroke colour + width + `S` paint, with cubic arcs.
        assert!(
            content.contains(" RG\n") && content.contains("\nS\n"),
            "stroked circle ops present"
        );
        assert!(
            content.contains(" c\n"),
            "circle emitted as cubic Bézier arcs"
        );
    }

    #[test]
    fn add_svg_gradient_emits_shading_pattern() {
        let svg = r##"<svg viewBox="0 0 100 100"><defs>
            <linearGradient id="g"><stop offset="0" stop-color="#ff0000"/><stop offset="1" stop-color="#0000ff"/></linearGradient>
            </defs><rect x="0" y="0" width="100" height="100" fill="url(#g)"/></svg>"##;
        let mut doc =
            Document::open(&crate::convert::reverse::txt_to_pdf("svg grad host")).unwrap();
        doc.add_svg(1, svg, 0.0, 0.0, 200.0, 200.0).unwrap();
        let content = String::from_utf8_lossy(&doc.page_content(1).unwrap()).into_owned();
        assert!(
            content.contains("/Pattern cs") && content.contains(" scn"),
            "shading-pattern fill: {content}"
        );
        // Round-trip: the Function/Shading objects serialize into a valid PDF.
        let reopened = Document::open(&doc.save()).unwrap();
        assert_eq!(reopened.page_count(), 1, "gradient PDF re-opens");
    }

    #[test]
    fn draw_color_glyph_emits_palette_filled_layers() {
        use crate::font::truetype::TrueTypeFont;
        use crate::font::GlyphSource;
        // Pull a real embedded TrueType face out of the fixture.
        let src = Document::open(&fixture("embedded-fonts.pdf")).unwrap();
        let page = src.page_dict(1).unwrap();
        let fonts = page
            .get(b"Resources")
            .map(|o| src.resolve(o))
            .and_then(Object::as_dict)
            .and_then(|r| r.get(b"Font"))
            .map(|o| src.resolve(o))
            .and_then(Object::as_dict)
            .expect("page has a Font dict");
        let face: TrueTypeFont = fonts
            .0
            .values()
            .find_map(|v| match src.font_program(src.resolve(v).as_dict()?)? {
                GlyphSource::TrueType(f) => Some(f),
                _ => None,
            })
            .expect("an embedded TrueType face");
        let gid = (1..face.num_glyphs())
            .find(|&g| !face.glyph_polygons(g).is_empty())
            .expect("a glyph with an outline");

        // Synthesize a 1-layer colour glyph: base `gid` → layer `gid`, palette 0 = red.
        let g = gid.to_be_bytes();
        let mut colr = vec![0, 0, 0, 1, 0, 0, 0, 14, 0, 0, 0, 20, 0, 1];
        colr.extend_from_slice(&[g[0], g[1], 0, 0, 0, 1]); // base: gid, first 0, num 1
        colr.extend_from_slice(&[g[0], g[1], 0, 0]); // layer: gid, palette 0
        let mut cpal = vec![0, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 14, 0, 0];
        cpal.extend_from_slice(&[0, 0, 255, 255]); // BGRA red
        let colors = crate::font::color::ColorGlyphs::parse(&colr, &cpal).unwrap();

        let mut doc = Document::open(&crate::convert::reverse::txt_to_pdf("emoji host")).unwrap();
        let adv = doc
            .draw_color_glyph(1, &face, &colors, gid, 100.0, 100.0, 40.0, [0.0, 0.0, 0.0])
            .unwrap();
        assert!(adv > 0.0, "advance returned for pen movement");
        let content = String::from_utf8_lossy(&doc.page_content(1).unwrap()).into_owned();
        assert!(
            content.contains("1 0 0 rg") && content.contains("\nf\n"),
            "colour layer filled in red"
        );
    }

    #[test]
    fn renders_embedded_font_glyphs() {
        // embedded-fonts.pdf uses a DejaVu TTF subset — glyphs must paint ink,
        // which only happens if the /FontFile2 program is parsed and filled.
        let doc = Document::open(&fixture("embedded-fonts.pdf")).unwrap();
        let png = doc.render_page(1, 2.0).unwrap();
        // Decode the (stored) zlib IDAT and count non-white pixels.
        let idat = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes([png[idat - 4], png[idat - 3], png[idat - 2], png[idat - 1]])
            as usize;
        let zlib = &png[idat + 4..idat + 4 + len];
        let raw = crate::filters::inflate::inflate(&zlib[2..zlib.len() - 4]).unwrap();
        let dark = raw.iter().filter(|&&b| b < 200).count();
        assert!(
            dark > 500,
            "embedded-font glyphs painted ink ({dark} dark samples)"
        );
    }

    #[test]
    fn extracts_text_without_tofu() {
        // Embedded TTF subsets with custom encodings only extract cleanly when
        // the font's /ToUnicode CMap is honoured — otherwise it's all tofu.
        for fixture_name in ["embedded-fonts.pdf", "mixed-fonts.pdf", "simple-text.pdf"] {
            let doc = Document::open(&fixture(fixture_name)).unwrap();
            let runs = doc.page_text_runs(1).unwrap();
            let text: String = runs.iter().map(|r| r.text.as_str()).collect();
            assert!(!text.is_empty(), "{fixture_name}: extracted some text");
            let tofu = text.chars().filter(|&c| c == '\u{FFFD}').count();
            assert_eq!(
                tofu, 0,
                "{fixture_name}: no replacement chars, got {text:?}"
            );
        }
    }

    #[test]
    fn creates_all_acroform_field_types_round_trip() {
        let pdf = crate::convert::reverse::txt_to_pdf("form host page");
        let mut doc = Document::open(&pdf).unwrap();
        assert!(
            doc.form_fields().unwrap().is_empty(),
            "starts with no fields"
        );
        let style = form::FieldStyle::default();

        doc.add_text_field(
            1,
            "fullname",
            [50.0, 700.0, 300.0, 720.0],
            "Jane",
            Some(40),
            false,
            false,
            &style,
        )
        .unwrap();
        doc.add_checkbox(
            1,
            "subscribe",
            [50.0, 670.0, 64.0, 684.0],
            true,
            "Yes",
            &style,
        )
        .unwrap();
        doc.add_radio_group(
            1,
            "plan",
            &[
                ("Basic".to_string(), [50.0, 640.0, 64.0, 654.0]),
                ("Pro".to_string(), [80.0, 640.0, 94.0, 654.0]),
            ],
            Some("Pro"),
            &style,
        )
        .unwrap();
        doc.add_combo_box(
            1,
            "country",
            [50.0, 610.0, 200.0, 626.0],
            &["FR".into(), "US".into()],
            Some("FR"),
            false,
            &style,
        )
        .unwrap();
        doc.add_list_box(
            1,
            "langs",
            [50.0, 560.0, 200.0, 600.0],
            &["en".into(), "fr".into()],
            None,
            true,
            &style,
        )
        .unwrap();

        // Re-parse the serialized bytes and read the fields back.
        let reopened = Document::open(&doc.save()).unwrap();
        let fields = reopened.form_fields().unwrap();
        assert_eq!(fields.len(), 5, "five fields registered: {fields:#?}");
        let by = |name: &str| fields.iter().find(|f| f.name == name).unwrap().clone();

        let text = by("fullname");
        assert_eq!(text.kind(), crate::form::FieldKind::Text);
        assert_eq!(text.value, "Jane");
        assert_eq!(text.max_len, Some(40));
        // Widget geometry: page 1, /Rect [50 700 300 720] (bottom-left) maps to
        // top-left bounds [50, H-720, 250, 20].
        assert_eq!(text.page, Some(1), "fullname is on page 1");
        let page_h = reopened.page_info(1).unwrap().1;
        let b = text.bounds.expect("fullname has widget bounds");
        assert!((b[0] - 50.0).abs() < 0.5, "x = {}", b[0]);
        assert!((b[1] - (page_h - 720.0)).abs() < 0.5, "y = {}", b[1]);
        assert!((b[2] - 250.0).abs() < 0.5, "w = {}", b[2]);
        assert!((b[3] - 20.0).abs() < 0.5, "h = {}", b[3]);

        let cb = by("subscribe");
        assert_eq!(cb.kind(), crate::form::FieldKind::Checkbox);
        assert_eq!(cb.value, "Yes");
        assert!(cb.options.contains(&"Yes".to_string()));

        let radio = by("plan");
        assert_eq!(radio.kind(), crate::form::FieldKind::Radio);
        assert_eq!(radio.value, "Pro");
        assert!(
            radio.options.contains(&"Basic".to_string())
                && radio.options.contains(&"Pro".to_string())
        );

        let combo = by("country");
        assert_eq!(combo.kind(), crate::form::FieldKind::ComboBox);
        assert_eq!(combo.value, "FR");
        assert_eq!(combo.options, vec!["FR".to_string(), "US".to_string()]);

        let list = by("langs");
        assert_eq!(list.kind(), crate::form::FieldKind::ListBox);
        assert!(list.is_multi_select());
        assert_eq!(list.options, vec!["en".to_string(), "fr".to_string()]);

        // Every widget got a visible appearance stream (no reliance on the
        // viewer regenerating from /V alone).
        let saved = doc.save();
        assert!(
            saved.windows(7).any(|w| w == b"/Tx BMC"),
            "text appearance present"
        );
        assert!(
            saved.windows(16).any(|w| w == b"/NeedAppearances"),
            "NeedAppearances set"
        );
    }

    #[test]
    fn flatten_form_bakes_and_removes_acroform() {
        let pdf = crate::convert::reverse::txt_to_pdf("flatten host page");
        let mut doc = Document::open(&pdf).unwrap();
        let style = form::FieldStyle::default();
        doc.add_text_field(
            1,
            "name",
            [50.0, 700.0, 300.0, 720.0],
            "Jane",
            None,
            false,
            false,
            &style,
        )
        .unwrap();
        doc.add_checkbox(1, "agree", [50.0, 670.0, 64.0, 684.0], true, "Yes", &style)
            .unwrap();
        assert_eq!(
            doc.form_fields().unwrap().len(),
            2,
            "two fields before flat"
        );

        let baked = doc.flatten_form().unwrap();
        assert_eq!(baked, 2, "both widgets baked");

        // After flattening, the form is gone: no fields, no /AcroForm.
        let reopened = Document::open(&doc.save()).unwrap();
        assert!(
            reopened.form_fields().unwrap().is_empty(),
            "no fields after flatten"
        );
        let saved = doc.save();
        assert!(
            !saved.windows(9).any(|w| w == b"/AcroForm"),
            "/AcroForm removed"
        );
        // A second flatten is a harmless no-op.
        assert_eq!(doc.flatten_form().unwrap(), 0, "re-flatten is a no-op");
    }

    #[test]
    fn extract_pages_yields_self_contained_chunks() {
        // Five-page host doc with a form field on page 2 and a page-1 → page-5 link.
        let pdf = crate::convert::reverse::txt_to_pdf("page one");
        let mut doc = Document::open(&pdf).unwrap();
        for _ in 0..4 {
            let after = doc.page_ids().unwrap().len() as u32;
            doc.add_page(612.0, 792.0, after).unwrap();
        }
        assert_eq!(doc.page_ids().unwrap().len(), 5);
        let style = form::FieldStyle::default();
        doc.add_text_field(2, "fld", [50.0, 700.0, 300.0, 720.0], "", None, false, false, &style)
            .unwrap();
        doc.add_goto_link(1, [50.0, 600.0, 200.0, 620.0], 5).unwrap();
        assert_eq!(doc.form_fields().unwrap().len(), 1);

        // Chunk A = pages 1-3: the field's page (2) is in-chunk → field survives;
        // the page-1 link targets page 5 (dropped) → neutralised, no orphan kept.
        let chunk_a = Document::open(&doc.extract_pages(&[1, 2, 3]).unwrap()).unwrap();
        assert_eq!(chunk_a.page_ids().unwrap().len(), 3, "chunk A keeps 3 pages");
        assert_eq!(
            chunk_a.form_fields().unwrap().len(),
            1,
            "in-chunk field survives extraction"
        );

        // Chunk B = pages 4-5: the field lived on page 2 (out-of-chunk) → dropped.
        let chunk_b = Document::open(&doc.extract_pages(&[4, 5]).unwrap()).unwrap();
        assert_eq!(chunk_b.page_ids().unwrap().len(), 2, "chunk B keeps 2 pages");
        assert!(
            chunk_b.form_fields().unwrap().is_empty(),
            "out-of-chunk field dropped from extraction"
        );
    }

    #[test]
    fn add_text_layer_writes_winansi_and_unicode_runs() {
        let pdf = crate::convert::reverse::txt_to_pdf("ocr host page");
        let mut doc = Document::open(&pdf).unwrap();
        let runs = vec![
            TextLayerRun { x: 50.0, y: 700.0, size: 10.0, text: "café".into(), rotation_deg: 0.0 },
            TextLayerRun { x: 50.0, y: 680.0, size: 10.0, text: "résumé".into(), rotation_deg: 0.0 },
            // Non-WinAnsi (CJK) now goes through the glyphless Type0 path instead
            // of being skipped.
            TextLayerRun { x: 50.0, y: 660.0, size: 10.0, text: "日本語".into(), rotation_deg: 0.0 },
            TextLayerRun { x: 50.0, y: 640.0, size: 10.0, text: String::new(), rotation_deg: 0.0 },
        ];
        // Two WinAnsi + one Unicode run written; only the empty run is skipped.
        assert_eq!(doc.add_text_layer(1, &runs).unwrap(), 3);
        assert_eq!(doc.add_text_layer(1, &[]).unwrap(), 0, "empty input is a no-op");

        let saved = doc.save();
        let body = String::from_utf8_lossy(&saved);
        assert!(body.contains("3 Tr"), "invisible text render mode present");
        assert!(body.contains(" Tj"), "text-show operator present");
        assert!(body.contains("caf"), "the café run's glyphs were written");
        assert!(body.contains("Type0"), "a Type0 font was embedded for the CJK run");
        assert!(body.contains("Identity-H"), "the Type0 font uses Identity-H");
        // The result re-opens as a valid single-page document.
        assert_eq!(Document::open(&saved).unwrap().page_ids().unwrap().len(), 1);
    }

    #[test]
    fn add_text_layer_unicode_round_trips_for_extraction() {
        // The decisive criterion: a multi-script OCR layer (Latin + Cyrillic +
        // Greek) must be re-extractable from the saved PDF, proving the ToUnicode
        // CMap of the glyphless Type0 font carries the real code points.
        let pdf = crate::convert::reverse::txt_to_pdf("ocr host page");
        let mut doc = Document::open(&pdf).unwrap();
        let runs = vec![
            TextLayerRun { x: 50.0, y: 700.0, size: 10.0, text: "Latin café".into(), rotation_deg: 0.0 },
            TextLayerRun { x: 50.0, y: 680.0, size: 10.0, text: "Привет".into(), rotation_deg: 0.0 },
            TextLayerRun { x: 50.0, y: 660.0, size: 10.0, text: "Ελληνικά".into(), rotation_deg: 0.0 },
        ];
        assert_eq!(doc.add_text_layer(1, &runs).unwrap(), 3);

        let saved = doc.save();
        let reopened = Document::open(&saved).unwrap();
        let extracted: String = reopened
            .structured_text(1)
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            extracted.contains("Привет"),
            "Cyrillic text must be extractable, got: {extracted:?}"
        );
        assert!(
            extracted.contains("Ελληνικά"),
            "Greek text must be extractable, got: {extracted:?}"
        );
    }

    #[test]
    fn markup_annotation_and_text_note_round_trip() {
        let pdf = crate::convert::reverse::txt_to_pdf("annotation host page");
        let mut doc = Document::open(&pdf).unwrap();
        // Two-quad highlight (wrapped text) with full reviewer metadata.
        doc.add_markup_annotation(
            1,
            "Highlight",
            &[[50.0, 700.0, 200.0, 715.0], [50.0, 680.0, 150.0, 695.0]],
            [1.0, 1.0, 0.0],
            0.4,
            "my comment",
            "Rony",
            "annot-1",
            "D:20260616120000Z",
        )
        .unwrap();
        doc.add_text_note(
            1,
            [300.0, 700.0, 320.0, 720.0],
            "sticky note body",
            "Rony",
            "note-1",
            "D:20260616120000Z",
            true,
            "Note",
            [1.0, 0.9, 0.0],
        )
        .unwrap();

        let reopened = Document::open(&doc.save()).unwrap();
        let annots = reopened.page_annotations(1).unwrap();
        assert!(
            annots.iter().any(|a| a.subtype == "Highlight"),
            "highlight present: {annots:?}"
        );
        assert!(
            annots.iter().any(|a| a.subtype == "Text"),
            "sticky note present"
        );
        assert!(
            annots.iter().any(|a| a.contents.contains("my comment")),
            "highlight popup contents preserved"
        );
        assert!(
            annots
                .iter()
                .any(|a| a.contents.contains("sticky note body")),
            "note contents preserved"
        );
    }

    #[test]
    fn subset_preserves_used_glyphs_and_strips_unused() {
        // Extract the embedded TrueType program from the fixture.
        let doc = Document::open(&fixture("embedded-fonts.pdf")).unwrap();
        let (bytes, format) = doc
            .extract_font_program("DejaVu")
            .expect("embedded DejaVu font present");
        assert_eq!(format, "truetype");
        let ttf = crate::font::truetype::TrueTypeFont::parse(&bytes).unwrap();

        // Scan glyph ids directly (a subset font's cmap may not map ASCII): keep
        // the first inked glyph, leave at least one other inked glyph out.
        let inked: Vec<u16> = (1..ttf.num_glyphs())
            .filter(|&g| !ttf.glyph_polygons(g).is_empty())
            .collect();
        assert!(inked.len() >= 2, "fixture font has several inked glyphs");
        // Keep a higher-GID inked glyph; a lower inked glyph stays in range but
        // unused, so it must be stripped (its outline emptied).
        let kept = inked[1];
        let dropped = inked[0];
        let used: std::collections::BTreeSet<u16> = std::iter::once(kept).collect();

        let sub = ttf.subset(&used).expect("subset built");
        assert!(sub.len() <= bytes.len(), "subset is never larger");

        let subttf = crate::font::truetype::TrueTypeFont::parse(&sub).unwrap();
        assert_eq!(
            subttf.num_glyphs(),
            kept + 1,
            "glyph table truncated to the highest used id + 1 (GIDs preserved, not remapped)"
        );
        assert_eq!(
            subttf.glyph_polygons(kept).len(),
            ttf.glyph_polygons(kept).len(),
            "kept glyph keeps its outline"
        );
        assert!(
            subttf.glyph_polygons(dropped).is_empty(),
            "in-range unused glyph {dropped} stripped from the subset"
        );
    }

    /// A minimal but valid SFNT font with a format-6 cmap mapping `'A'..='Z'` →
    /// glyph ids `1..=26`. `is_cff` selects an **OpenType-CFF** (`OTTO`, a `CFF `
    /// stub, no outlines) over a **glyf TrueType** (`0x00010000`, empty
    /// `glyf`/`loca`). Carries exactly the tables the metrics reader needs
    /// (`head`/`maxp`/`hhea`/`hmtx`/`cmap`), so `embed_font` routes each flavour
    /// to its proper FontFile/CIDFont without an external fixture.
    fn minimal_sfnt(is_cff: bool) -> Vec<u8> {
        fn b16(v: u16) -> [u8; 2] {
            v.to_be_bytes()
        }
        fn b32(v: u32) -> [u8; 4] {
            v.to_be_bytes()
        }
        let num_glyphs: u16 = 27;

        let mut head = vec![0u8; 54];
        head[18..20].copy_from_slice(&b16(1000)); // unitsPerEm
        // indexToLocFormat @50 stays 0 (short loca).

        let mut maxp = vec![0u8; 6];
        let maxp_ver: u32 = if is_cff { 0x0000_5000 } else { 0x0001_0000 };
        maxp[0..4].copy_from_slice(&b32(maxp_ver)); // 0.5 (CFF) / 1.0 (TrueType)
        maxp[4..6].copy_from_slice(&b16(num_glyphs));

        let mut hhea = vec![0u8; 36];
        hhea[34..36].copy_from_slice(&b16(num_glyphs)); // numberOfHMetrics

        let mut hmtx = Vec::new();
        for _ in 0..num_glyphs {
            hmtx.extend_from_slice(&b16(500)); // advanceWidth
            hmtx.extend_from_slice(&b16(0)); // lsb
        }

        // cmap: header + one (3,1) record → format-6 subtable.
        let entry_count: u16 = 26;
        let mut sub = Vec::new();
        sub.extend_from_slice(&b16(6)); // format
        sub.extend_from_slice(&b16(10 + entry_count * 2)); // length
        sub.extend_from_slice(&b16(0)); // language
        sub.extend_from_slice(&b16(0x41)); // firstCode 'A'
        sub.extend_from_slice(&b16(entry_count));
        for g in 1..=entry_count {
            sub.extend_from_slice(&b16(g)); // 'A'→1, 'B'→2, …
        }
        let mut cmap = Vec::new();
        cmap.extend_from_slice(&b16(0)); // version
        cmap.extend_from_slice(&b16(1)); // numTables
        cmap.extend_from_slice(&b16(3)); // platformID Windows
        cmap.extend_from_slice(&b16(1)); // encodingID BMP Unicode
        cmap.extend_from_slice(&b32(12)); // offset to subtable (4 header + 8 record)
        cmap.extend_from_slice(&sub);

        // glyf TrueType needs glyf+loca (empty glyphs); OpenType-CFF needs `CFF `.
        let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"cmap", cmap),
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"maxp", maxp),
        ];
        if is_cff {
            tables.push((b"CFF ", b"\x01\x00\x04\x01".to_vec()));
        } else {
            tables.push((b"glyf", Vec::new())); // all glyphs empty
            tables.push((b"loca", vec![0u8; (num_glyphs as usize + 1) * 2])); // short
        }

        let body_start = 12 + tables.len() * 16;
        let mut dir = Vec::new();
        let mut body = Vec::new();
        for (tag, data) in &tables {
            let off = body_start + body.len();
            dir.extend_from_slice(*tag);
            dir.extend_from_slice(&b32(0)); // checksum (parser ignores it)
            dir.extend_from_slice(&b32(off as u32));
            dir.extend_from_slice(&b32(data.len() as u32));
            body.extend_from_slice(data);
            while body.len() % 4 != 0 {
                body.push(0);
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(if is_cff { b"OTTO" } else { &[0, 1, 0, 0] }); // sfnt version
        out.extend_from_slice(&b16(tables.len() as u16));
        out.extend_from_slice(&[0u8; 6]); // searchRange/entrySelector/rangeShift
        out.extend_from_slice(&dir);
        out.extend_from_slice(&body);
        out
    }

    /// A one-page document with **no** existing text — so a single `add_text`
    /// call leaves its run at index 0 (unlike `txt_to_pdf`, which seeds a run).
    fn blank_doc() -> Document {
        let mut b = crate::convert::build::PdfBuilder::new();
        b.add_page(612.0, 792.0);
        Document::open(&b.finish()).unwrap()
    }

    #[test]
    fn attachments_reads_embedded_files_name_tree() {
        let mut doc = blank_doc();

        // An embedded-file stream "hello" with /Subtype + /Params metadata.
        let mut sdict = Dictionary::new();
        sdict.set(b"Type", Object::Name(b"EmbeddedFile".to_vec()));
        sdict.set(b"Subtype", Object::Name(b"text/plain".to_vec()));
        let mut params = Dictionary::new();
        params.set(b"Size", Object::Integer(5));
        params.set(
            b"CreationDate",
            Object::String(b"D:20260101000000Z".to_vec(), StringKind::Literal),
        );
        sdict.set(b"Params", Object::Dictionary(params));
        let ef_id = (doc.next_object_number(), 0u16);
        doc.objects
            .insert(ef_id, Object::Stream(Stream::new(sdict, b"hello".to_vec())));

        // The filespec dictionary referencing that stream via /EF /F.
        let mut ef = Dictionary::new();
        ef.set(b"F", Object::Reference(ef_id));
        let mut spec = Dictionary::new();
        spec.set(b"Type", Object::Name(b"Filespec".to_vec()));
        spec.set(
            b"F",
            Object::String(b"notes.txt".to_vec(), StringKind::Literal),
        );
        spec.set(
            b"UF",
            Object::String(b"notes.txt".to_vec(), StringKind::Literal),
        );
        spec.set(
            b"Desc",
            Object::String(b"a test file".to_vec(), StringKind::Literal),
        );
        spec.set(b"EF", Object::Dictionary(ef));

        // Catalog /Names /EmbeddedFiles → inline name-tree leaf.
        let mut leaf = Dictionary::new();
        leaf.set(
            b"Names",
            Object::Array(vec![
                Object::String(b"notes.txt".to_vec(), StringKind::Literal),
                Object::Dictionary(spec),
            ]),
        );
        let mut names = Dictionary::new();
        names.set(b"EmbeddedFiles", Object::Dictionary(leaf));
        let catalog_id = doc.catalog_id().unwrap();
        let mut catalog = doc
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .unwrap()
            .clone();
        catalog.set(b"Names".to_vec(), Object::Dictionary(names));
        doc.objects.insert(catalog_id, Object::Dictionary(catalog));

        let atts = doc.attachments();
        assert_eq!(atts.len(), 1, "one attachment extracted");
        assert_eq!(atts[0].name, "notes.txt");
        assert_eq!(atts[0].filename, "notes.txt");
        assert_eq!(atts[0].data, b"hello");
        assert_eq!(atts[0].mime.as_deref(), Some("text/plain"));
        assert_eq!(atts[0].description.as_deref(), Some("a test file"));
        assert_eq!(atts[0].creation_date.as_deref(), Some("D:20260101000000Z"));

        // A document with no /Names /EmbeddedFiles yields nothing.
        assert!(blank_doc().attachments().is_empty());
    }

    #[test]
    fn named_dests_enumerates_name_tree() {
        let mut doc = blank_doc();
        let page_id = doc.page_object_id(1).unwrap();
        let dest_array =
            || Object::Array(vec![Object::Reference(page_id), Object::Name(b"Fit".to_vec())]);

        // `chapter2` wraps its array in a `<< /D [...] >>` dictionary.
        let mut wrapper = Dictionary::new();
        wrapper.set(b"D", dest_array());

        let mut tree = Dictionary::new();
        tree.set(
            b"Names",
            Object::Array(vec![
                Object::String(b"chapter1".to_vec(), StringKind::Literal),
                dest_array(),
                Object::String(b"chapter2".to_vec(), StringKind::Literal),
                Object::Dictionary(wrapper),
            ]),
        );
        let mut names_dict = Dictionary::new();
        names_dict.set(b"Dests", Object::Dictionary(tree));

        let catalog_id = doc.catalog_id().unwrap();
        let mut catalog = doc
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .unwrap()
            .clone();
        catalog.set(b"Names".to_vec(), Object::Dictionary(names_dict));
        doc.objects.insert(catalog_id, Object::Dictionary(catalog));

        let dests = doc.named_dests();
        let names: Vec<&str> = dests.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"chapter1"), "inline-array name-tree dest");
        assert!(names.contains(&"chapter2"), "/D-wrapped name-tree dest");
        assert!(dests.iter().all(|(_, p)| *p == 1), "both resolve to page 1");
    }

    #[test]
    fn outline_items_carry_style_and_dest_detail() {
        let mut doc = blank_doc();
        let page_id = doc.page_object_id(1).unwrap();
        let base = doc.next_object_number();
        let outlines_id = (base, 0u16);
        let item_id = (base + 1, 0u16);

        let mut item = Dictionary::new();
        item.set(b"Title", Object::String(b"Chapter 1".to_vec(), StringKind::Literal));
        item.set(b"Parent", Object::Reference(outlines_id));
        item.set(b"F", Object::Integer(2)); // bold
        item.set(
            b"C",
            Object::Array(vec![Object::Real(1.0), Object::Real(0.0), Object::Real(0.0)]),
        );
        item.set(
            b"Dest",
            Object::Array(vec![
                Object::Reference(page_id),
                Object::Name(b"XYZ".to_vec()),
                Object::Integer(100),
                Object::Integer(700),
                Object::Real(2.0),
            ]),
        );
        doc.objects.insert(item_id, Object::Dictionary(item));

        let mut outlines = Dictionary::new();
        outlines.set(b"Type", Object::Name(b"Outlines".to_vec()));
        outlines.set(b"First", Object::Reference(item_id));
        outlines.set(b"Last", Object::Reference(item_id));
        doc.objects.insert(outlines_id, Object::Dictionary(outlines));

        let catalog_id = doc.catalog_id().unwrap();
        let mut catalog = doc
            .objects
            .get(&catalog_id)
            .and_then(Object::as_dict)
            .unwrap()
            .clone();
        catalog.set(b"Outlines".to_vec(), Object::Reference(outlines_id));
        doc.objects.insert(catalog_id, Object::Dictionary(catalog));

        let items = doc.outline_items();
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.title, "Chapter 1");
        assert_eq!(it.page, Some(1));
        assert!(it.bold && !it.italic, "F=2 → bold, not italic");
        assert_eq!(it.color, [1.0, 0.0, 0.0], "red /C");
        assert_eq!(it.dest_kind, "xyz");
        assert_eq!(it.dest_x, Some(100.0));
        assert_eq!(it.dest_y, Some(700.0));
        assert_eq!(it.dest_zoom, Some(2.0));
    }

    #[test]
    fn page_text_elements_carry_font_size_colour_family() {
        let mut doc = blank_doc();
        doc.add_text_standard(
            1,
            120.0,
            650.0,
            20.0,
            "Bold",
            "Helvetica-Bold",
            [1.0, 0.0, 0.0],
            1.0,
            0.0,
        )
        .unwrap();

        let els = doc.page_text_elements(1);
        assert_eq!(els.len(), 1, "one text element");
        let e = &els[0];
        assert_eq!(e.text, "Bold");
        assert_eq!(e.index, 0, "text-run index (feeds replace_text_run)");
        assert!((e.font_size - 20.0).abs() < 1.5, "size ~20, got {}", e.font_size);
        assert!((e.x - 120.0).abs() < 3.0, "x ~120, got {}", e.x);
        assert!((e.y - 650.0).abs() < 15.0, "y ~650, got {}", e.y);
        assert!(
            (e.color[0] - 1.0).abs() < 0.02 && e.color[1] < 0.02 && e.color[2] < 0.02,
            "red fill, got {:?}",
            e.color
        );
        assert_eq!(e.font_family, "Helvetica", "/BaseFont family");
        assert!(e.bold, "Helvetica-Bold resolves bold");
        assert!(e.rotation_deg.abs() < 0.5, "upright, got {}", e.rotation_deg);
    }

    #[test]
    fn text_width_uses_real_metrics_not_estimate() {
        // Helvetica AFM: W=944. "WWWW" at 20pt = 4·944·20/1000 = 75.52 pt — far
        // from the old 0.5-em estimate (4·0.5·20 = 40), so this pins that
        // base-14 fonts without /Widths now measure by real AFM advances.
        let mut doc = blank_doc();
        doc.add_text_standard(1, 50.0, 700.0, 20.0, "WWWW", "Helvetica", [0.0, 0.0, 0.0], 1.0, 0.0)
            .unwrap();
        let els = doc.page_text_elements(1);
        assert_eq!(els.len(), 1);
        assert!(
            (els[0].width - 75.52).abs() < 1.5,
            "AFM advance ~75.5 expected, got {}",
            els[0].width,
        );

        // Courier is monospace (600): "iiii" measures the same as "WWWW".
        let mut mono = blank_doc();
        mono.add_text_standard(1, 50.0, 700.0, 20.0, "iiii", "Courier", [0.0, 0.0, 0.0], 1.0, 0.0)
            .unwrap();
        let mels = mono.page_text_elements(1);
        assert!(
            (mels[0].width - 48.0).abs() < 0.5,
            "Courier 600-em: 4·600·20/1000 = 48, got {}",
            mels[0].width,
        );
    }

    #[test]
    fn page_image_elements_extract_png_and_jpeg() {
        let mut doc = blank_doc();
        // A 2x2 opaque RGB image embedded two ways: PNG (→ Flate) + JPEG (→ DCT).
        let rgba = [
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
        ];
        let png = crate::raster::png::encode_png(2, 2, &rgba);
        let jpeg = crate::raster::jpeg::encode_jpeg(2, 2, &rgba, 90);
        doc.add_image(1, &png, 50.0, 600.0, 64.0, 64.0, 1.0).unwrap();
        doc.add_image(1, &jpeg, 200.0, 400.0, 80.0, 80.0, 1.0).unwrap();

        let imgs = doc.page_image_elements(1);
        assert_eq!(imgs.len(), 2, "two image elements");
        for img in &imgs {
            assert_eq!((img.pixel_width, img.pixel_height), (2, 2), "2x2 px");
            assert!(!img.data.is_empty(), "embeddable bytes present");
            assert!(img.width > 0.0 && img.height > 0.0, "placement box set");
            // Axis-aligned placement, fully opaque (`add_image` opacity = 1.0).
            assert_eq!(img.rotation, 0.0, "upright placement");
            assert!((img.opacity - 1.0).abs() < 1e-9, "fully opaque");
        }
        let formats: Vec<&str> = imgs.iter().map(|i| i.format.as_str()).collect();
        assert!(formats.contains(&"png"), "Flate image → png, got {formats:?}");
        assert!(formats.contains(&"jpeg"), "DCTDecode image → jpeg, got {formats:?}");

        // The PNG one re-decodes to the original RGB (lossless Flate round-trip).
        let png_el = imgs.iter().find(|i| i.format == "png").unwrap();
        let decoded = crate::raster::decode_png(&png_el.data).expect("valid PNG");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        for px in 0..4 {
            assert_eq!(
                decoded.rgba[px * 4..px * 4 + 3],
                rgba[px * 4..px * 4 + 3],
                "pixel {px} RGB round-trips"
            );
        }
        // The JPEG one is a real JPEG (SOI marker) passed through untouched.
        let jpeg_el = imgs.iter().find(|i| i.format == "jpeg").unwrap();
        assert_eq!(&jpeg_el.data[0..2], &[0xFF, 0xD8], "JPEG SOI passthrough");
    }

    #[test]
    fn page_image_elements_report_opacity_from_extgstate() {
        // `add_image` with opacity < 1 wraps the draw in a `q … gs … Q` block
        // referencing an /ExtGState whose /ca the walker must surface.
        let mut doc = blank_doc();
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255];
        let png = crate::raster::png::encode_png(2, 2, &rgba);
        doc.add_image(1, &png, 50.0, 600.0, 64.0, 64.0, 0.5).unwrap();

        let imgs = doc.page_image_elements(1);
        assert_eq!(imgs.len(), 1, "one image element");
        assert!(
            (imgs[0].opacity - 0.5).abs() < 1e-9,
            "image carries /ca 0.5 from its /ExtGState, got {}",
            imgs[0].opacity
        );
    }

    #[test]
    fn page_media_box_preserves_origin() {
        let mut doc = blank_doc();
        doc.resize_page(1, 200.0, 100.0).unwrap();
        let mb = doc.page_media_box(1).unwrap();
        // resize_page sets MediaBox to [0, 0, w, h].
        assert_eq!(mb, [0.0, 0.0, 200.0, 100.0]);
        let (w, h, _) = doc.page_info(1).unwrap();
        assert_eq!((w, h), (200.0, 100.0), "page_info size matches the box");
    }

    #[test]
    fn page_annotations_carry_rich_metadata() {
        let mut doc = blank_doc();
        doc.add_markup_annotation(
            1,
            "Highlight",
            &[[100.0, 700.0, 200.0, 712.0]],
            [1.0, 1.0, 0.0],
            0.5,
            "review note",
            "Alice",
            "nm-1",
            "D:20260101120000Z",
        )
        .unwrap();
        doc.add_uri_link(1, [10.0, 10.0, 50.0, 20.0], "https://giga-pdf.com")
            .unwrap();

        let annots = doc.page_annotations(1).unwrap();
        let hl = annots
            .iter()
            .find(|a| a.subtype == "Highlight")
            .expect("highlight present");
        assert_eq!(hl.author, "Alice", "/T author");
        assert_eq!(hl.contents, "review note", "/Contents");
        assert_eq!(hl.modified, "D:20260101120000Z", "/M raw date");
        assert!((hl.opacity - 0.5).abs() < 1e-9, "/CA opacity");
        assert_eq!(hl.color, vec![1.0, 1.0, 0.0], "/C yellow → RGB");
        assert_eq!(hl.quad_points.len(), 8, "one quad = 8 values");

        let link = annots
            .iter()
            .find(|a| a.subtype == "Link")
            .expect("link present");
        assert_eq!(link.link_uri, "https://giga-pdf.com", "/A /URI target");
        assert_eq!(link.link_page, 0, "external link has no internal page");
    }

    #[test]
    fn embed_font_handles_opentype_cff() {
        let otf = minimal_sfnt(true);
        let mut doc = blank_doc();
        let font = doc.embed_font("MyCff", &otf).unwrap();

        // The Type0 graph routes a CFF program to FontFile3/CIDFontType0.
        let t0 = doc.objects.get(&(font, 0)).and_then(Object::as_dict).unwrap();
        assert_eq!(
            t0.get(b"Subtype").and_then(Object::as_name),
            Some(b"Type0".as_slice())
        );
        let desc_ref = match &t0.get(b"DescendantFonts").and_then(Object::as_array).unwrap()[0] {
            Object::Reference(id) => *id,
            _ => panic!("descendant is a reference"),
        };
        let cid = doc.objects.get(&desc_ref).and_then(Object::as_dict).unwrap();
        assert_eq!(
            cid.get(b"Subtype").and_then(Object::as_name),
            Some(b"CIDFontType0".as_slice()),
            "CFF descendant is CIDFontType0"
        );
        assert!(
            cid.get(b"CIDToGIDMap").is_none(),
            "CIDFontType0 omits CIDToGIDMap (that key is TrueType-only)"
        );
        let fd_ref = match cid.get(b"FontDescriptor").unwrap() {
            Object::Reference(id) => *id,
            _ => panic!(),
        };
        let fd = doc.objects.get(&fd_ref).and_then(Object::as_dict).unwrap();
        assert!(fd.get(b"FontFile2").is_none(), "no FontFile2 for CFF");
        let ff_ref = match fd.get(b"FontFile3").expect("FontFile3 present") {
            Object::Reference(id) => *id,
            _ => panic!(),
        };
        let ff = doc.objects.get(&ff_ref).and_then(Object::as_stream).unwrap();
        assert_eq!(
            ff.dict.get(b"Subtype").and_then(Object::as_name),
            Some(b"OpenType".as_slice()),
            "FontFile3 carries /Subtype /OpenType"
        );

        // add_text resolves the CFF cmap (A→gid1, B→gid2) → 2-byte Identity-H GIDs.
        doc.add_text(1, 72.0, 700.0, 18.0, "AB", font, [0.0; 3], 1.0, 0.0)
            .unwrap();
        let content = doc.page_content(1).unwrap();
        assert!(
            has_op(&content, b"<00010002>"),
            "CFF text drawn as 2-byte glyph ids"
        );

        // Font-aware replace re-encodes through the same cmap (BA → gid2,gid1).
        doc.replace_text_run(1, 0, "BA").unwrap();
        let content = doc.page_content(1).unwrap();
        assert!(
            has_op(&content, b"<00020001>"),
            "replace re-encodes for the CFF font, not WinAnsi"
        );
    }

    #[test]
    fn embed_font_handles_raw_type1() {
        // A raw Type 1 face (PDF `FontFile` / `.pfb`) must route through the
        // decrypt → Type 2 → bare-CFF → OpenType-CFF path and embed as a Type0.
        // Best-effort on a system PFB; a no-op when none is installed.
        const CANDIDATES: &[&str] = &[
            "/usr/share/fonts/X11/Type1/NimbusSansNarrow-Bold.pfb",
            "/usr/share/fonts/X11/Type1/c0419bt_.pfb",
            "/usr/share/fonts/X11/Type1/C059-Roman.pfb",
        ];
        let Some(pfb) = CANDIDATES
            .iter()
            .find_map(|p| std::fs::read(p).ok().filter(|b| b.len() > 2000))
        else {
            eprintln!("no system Type1 fixture; skipped embed_font_handles_raw_type1");
            return;
        };
        assert!(super::is_raw_type1(&pfb), "PFB recognised as raw Type1");

        let mut doc = blank_doc();
        let font = doc.embed_font("MyType1", &pfb).expect("Type1 embeds");
        assert_ne!(font, 0, "non-zero Type0 object id");

        // The descendant is a CFF-flavoured CIDFontType0 (Type1 → CFF route).
        let t0 = doc.objects.get(&(font, 0)).and_then(Object::as_dict).unwrap();
        assert_eq!(
            t0.get(b"Subtype").and_then(Object::as_name),
            Some(b"Type0".as_slice())
        );
        let desc_ref = match &t0.get(b"DescendantFonts").and_then(Object::as_array).unwrap()[0] {
            Object::Reference(id) => *id,
            _ => panic!("descendant is a reference"),
        };
        let cid = doc.objects.get(&desc_ref).and_then(Object::as_dict).unwrap();
        assert_eq!(
            cid.get(b"Subtype").and_then(Object::as_name),
            Some(b"CIDFontType0".as_slice()),
            "Type1→CFF descendant is CIDFontType0"
        );

        // 'A' renders (its glyph resolved via the synthesised cmap) and the
        // document survives a save/open round-trip.
        doc.add_text(1, 72.0, 700.0, 18.0, "A", font, [0.0; 3], 1.0, 0.0)
            .expect("draw text in the Type1 face");
        let bytes = doc.save();
        Document::open(&bytes).expect("re-opens after embedding a Type1 font");
    }

    #[test]
    fn replace_text_run_reencodes_for_embedded_truetype() {
        // A run set in an embedded Type0/Identity-H glyf-TrueType face must be
        // edited through its char→GID map (not WinAnsi). Verified end-to-end: the
        // edit reads back through the font's /ToUnicode after a save round-trip.
        let ttf = minimal_sfnt(false);
        let mut doc = blank_doc();
        let font = doc.embed_font("Synthetic", &ttf).unwrap();
        // 'A'..='Z' → glyph ids 1..=26 in this face.
        doc.add_text(1, 72.0, 700.0, 18.0, "CAB", font, [0.0; 3], 1.0, 0.0)
            .unwrap();
        let content = doc.page_content(1).unwrap();
        assert!(
            has_op(&content, b"<000300010002>"),
            "TrueType text drawn as 2-byte glyph ids (C=3,A=1,B=2)"
        );

        doc.replace_text_run(1, 0, "BAC").unwrap();
        let re = Document::open(&doc.save()).unwrap();
        let texts: Vec<String> = re
            .page_text_runs(1)
            .unwrap()
            .into_iter()
            .map(|r| r.text)
            .collect();
        assert!(texts.iter().any(|t| t == "BAC"), "edited text: {texts:?}");
        assert!(!texts.iter().any(|t| t == "CAB"), "old text replaced");
    }

    #[test]
    fn named_destinations_register_resolve_and_link() {
        let mut b = crate::convert::build::PdfBuilder::new();
        for _ in 0..3 {
            b.add_page(612.0, 792.0);
        }
        let mut doc = Document::open(&b.finish()).unwrap();
        doc.add_named_dest("chapter2", 2).unwrap();
        doc.add_named_dest("chapter3", 3).unwrap();

        let mut dests = doc.named_dests();
        dests.sort();
        assert_eq!(
            dests,
            vec![("chapter2".to_string(), 2), ("chapter3".to_string(), 3)]
        );

        // A link to a named destination resolves to its target page…
        doc.add_goto_link_named(1, [10.0, 10.0, 50.0, 30.0], "chapter3")
            .unwrap();
        assert!(
            doc.page_links(1)
                .unwrap()
                .iter()
                .any(|l| l.target == LinkTarget::Page(3)),
            "named link resolves to page 3"
        );

        // …and both the dests and the named link survive a save round-trip.
        let re = Document::open(&doc.save()).unwrap();
        assert_eq!(re.named_dests().len(), 2, "named dests persisted");
        assert!(
            re.page_links(1)
                .unwrap()
                .iter()
                .any(|l| l.target == LinkTarget::Page(3)),
            "named link still resolves after save"
        );
    }

    // ── form-XObject text extraction (`Do` recursion) ────────────────────────

    /// Assemble a minimal one-page PDF from a list of `(number, body)` objects.
    /// The parser scans objects + the `trailer` (it ignores the xref), so a
    /// placeholder xref is fine. Used to exercise form-XObject recursion.
    fn raw_pdf(objects: &[(u32, String)]) -> Vec<u8> {
        let mut out = String::from("%PDF-1.7\n");
        for (num, body) in objects {
            out.push_str(&format!("{num} 0 obj\n{body}\nendobj\n"));
        }
        // Minimal (dummy) xref + trailer: open() locates objects by scanning and
        // the catalog via `trailer /Root`, so the offsets need not be accurate.
        out.push_str("xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Root 1 0 R >>\nstartxref\n0\n%%EOF");
        out.into_bytes()
    }

    /// A page whose text comes BOTH from the top-level stream and from a form
    /// XObject drawn via `Do` under a non-identity `cm` and a non-identity form
    /// `/Matrix`. The form text must be extracted at the correct page-space
    /// position by both `page_text_runs` and `page_text_elements`.
    fn form_xobject_fixture() -> Vec<u8> {
        // Page content: top-level "PAGE" at (50,150); then draw the form under
        // cm = translate(30,40).
        let page_stream = "BT /F1 12 Tf 50 150 Td (PAGE) Tj ET\nq 1 0 0 1 30 40 cm /Fm0 Do Q";
        // Form content: "FORM" at baseline (5,5) in form space.
        let form_stream = "BT /F1 12 Tf 5 5 Td (FORM) Tj ET";
        raw_pdf(&[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font << /F1 6 0 R >> /XObject << /Fm0 5 0 R >> >> \
                 /Contents 4 0 R >>"
                    .into(),
            ),
            (
                4,
                format!("<< /Length {} >> stream\n{page_stream}\nendstream", page_stream.len()),
            ),
            (
                5,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Matrix [1 0 0 1 10 20] \
                     /Resources << /Font << /F1 6 0 R >> >> \
                     /Length {} >> stream\n{form_stream}\nendstream",
                    form_stream.len()
                ),
            ),
            (6, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into()),
        ])
    }

    #[test]
    fn page_text_runs_recurse_into_form_xobjects() {
        let doc = Document::open(&form_xobject_fixture()).unwrap();
        let texts: Vec<String> = doc
            .page_text_runs(1)
            .unwrap()
            .into_iter()
            .map(|r| r.text)
            .collect();
        assert!(texts.iter().any(|t| t == "PAGE"), "top-level text present: {texts:?}");
        assert!(
            texts.iter().any(|t| t == "FORM"),
            "form-XObject text must be extracted via Do recursion: {texts:?}"
        );
    }

    #[test]
    fn page_text_elements_recurse_into_forms_with_page_space_bounds() {
        let doc = Document::open(&form_xobject_fixture()).unwrap();
        let els = doc.page_text_elements(1);

        let page = els.iter().find(|e| e.text == "PAGE").expect("top-level PAGE element");
        // Top-level run keeps an editable text-run index (0 here, the first run).
        assert_eq!(page.index, 0, "top-level run index feeds replace_text_run");
        assert!((page.x - 50.0).abs() < 1.0, "PAGE x≈50, got {}", page.x);

        let form = els.iter().find(|e| e.text == "FORM").expect("form-XObject FORM element");
        // Page space = form unit → /Matrix(10,20) → cm(30,40) = translate(40,60).
        // Form baseline (5,5) → (45, 65); bounds.y is the descender (−0.2·12).
        assert!((form.x - 45.0).abs() < 1.0, "FORM x≈45 (page space), got {}", form.x);
        assert!(
            (form.y - 62.6).abs() < 2.5,
            "FORM y≈62.6 (page space, descender of baseline 65), got {}",
            form.y
        );
        assert!(form.width > 0.0 && form.height > 0.0, "FORM has positive size");
        // Form text is not editable in place → sentinel index, which
        // replace_text_run rejects rather than misrouting to a top-level run.
        assert_eq!(form.index, usize::MAX, "form-XObject text carries the sentinel index");
        assert!(
            doc.clone().replace_text_run(1, form.index, "x").is_err(),
            "editing a form-XObject run is a safe no-op error, not a wrong-run edit"
        );
    }

    #[test]
    fn self_referencing_form_terminates() {
        // A form that draws itself via `Do` must not loop the extractor.
        let form_stream = "BT /F1 12 Tf 5 5 Td (LOOP) Tj ET\n/Fm0 Do";
        let pdf = raw_pdf(&[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font << /F1 6 0 R >> /XObject << /Fm0 5 0 R >> >> \
                 /Contents 4 0 R >>"
                    .into(),
            ),
            (4, {
                let s = "q 1 0 0 1 0 0 cm /Fm0 Do Q";
                format!("<< /Length {} >> stream\n{s}\nendstream", s.len())
            }),
            (
                5,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Resources << /Font << /F1 6 0 R >> /XObject << /Fm0 5 0 R >> >> \
                     /Length {} >> stream\n{form_stream}\nendstream",
                    form_stream.len()
                ),
            ),
            (6, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into()),
        ]);
        let doc = Document::open(&pdf).unwrap();
        // Must return (no infinite loop / panic) and surface the form text.
        let runs = doc.page_text_runs(1).unwrap();
        assert!(runs.iter().any(|r| r.text == "LOOP"), "self-ref form text extracted once: {runs:?}");
        let els = doc.page_text_elements(1);
        assert!(els.iter().any(|e| e.text == "LOOP"), "self-ref form text in elements");
    }

    #[test]
    fn mutually_recursive_forms_terminate() {
        // FmA draws FmB and FmB draws FmA: extraction must terminate.
        let a_stream = "BT /F1 12 Tf 5 5 Td (AAA) Tj ET\n/FmB Do";
        let b_stream = "BT /F1 12 Tf 5 5 Td (BBB) Tj ET\n/FmA Do";
        let pdf = raw_pdf(&[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font << /F1 6 0 R >> /XObject << /FmA 5 0 R /FmB 7 0 R >> >> \
                 /Contents 4 0 R >>"
                    .into(),
            ),
            (4, {
                let s = "/FmA Do";
                format!("<< /Length {} >> stream\n{s}\nendstream", s.len())
            }),
            (
                5,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Resources << /Font << /F1 6 0 R >> /XObject << /FmB 7 0 R >> >> \
                     /Length {} >> stream\n{a_stream}\nendstream",
                    a_stream.len()
                ),
            ),
            (6, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into()),
            (
                7,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Resources << /Font << /F1 6 0 R >> /XObject << /FmA 5 0 R >> >> \
                     /Length {} >> stream\n{b_stream}\nendstream",
                    b_stream.len()
                ),
            ),
        ]);
        let doc = Document::open(&pdf).unwrap();
        let runs = doc.page_text_runs(1).unwrap();
        // Both forms' text appears and extraction terminates (no hang/panic).
        assert!(runs.iter().any(|r| r.text == "AAA"), "FmA text extracted: {runs:?}");
        assert!(runs.iter().any(|r| r.text == "BBB"), "FmB text extracted: {runs:?}");
    }

    // ── form-XObject FLATTENING (`flatten_form_xobjects`) ────────────────────

    #[test]
    fn flatten_form_xobjects_makes_form_text_editable_in_place() {
        // Reuse the recursion fixture: "PAGE" top-level + "FORM" via `Do` under a
        // non-identity `cm` AND a non-identity form `/Matrix`. After flattening,
        // the form text is a normal page run with a real (non-sentinel) index.
        let mut doc = Document::open(&form_xobject_fixture()).unwrap();
        let n = doc.flatten_form_xobjects(1).unwrap();
        assert_eq!(n, 1, "exactly one form XObject inlined");

        let els = doc.page_text_elements(1);
        let form = els.iter().find(|e| e.text == "FORM").expect("FORM still present after flatten");
        assert_ne!(form.index, usize::MAX, "form text now carries a real, editable index");
        // Page space = form unit → /Matrix(10,20) → cm(30,40) = translate(40,60);
        // baseline (5,5) → (45,65). Position must survive the inlining `cm`.
        assert!((form.x - 45.0).abs() < 1.0, "FORM x≈45 (page space) after flatten, got {}", form.x);

        // The new index drives replace_text_run end-to-end.
        doc.replace_text_run(1, form.index, "DONE").unwrap();
        let text = doc.to_text();
        assert!(text.contains("DONE"), "edited form text via real index: {text:?}");
        assert!(!text.contains("FORM"), "old form text replaced");
        assert!(text.contains("PAGE"), "top-level text untouched");

        // The fully inlined form entry is dropped from /Resources /XObject.
        let page = doc.page_dict(1).unwrap();
        let still_xobj = page
            .get(b"Resources")
            .and_then(Object::as_dict)
            .and_then(|r| r.get(b"XObject"))
            .and_then(Object::as_dict)
            .map(|x| x.contains(b"Fm0"))
            .unwrap_or(false);
        assert!(!still_xobj, "inlined form /Fm0 dropped from page XObject resources");
    }

    /// A page that places the SAME form XObject TWICE, at two different `cm`
    /// translations. Each placement must inline its own copy (de-shared).
    fn form_placed_twice_fixture() -> Vec<u8> {
        // Draw /Fm0 once at translate(0,0) and once at translate(0,100).
        let page_stream =
            "q 1 0 0 1 0 0 cm /Fm0 Do Q\nq 1 0 0 1 0 100 cm /Fm0 Do Q";
        let form_stream = "BT /F1 12 Tf 5 5 Td (DUP) Tj ET";
        raw_pdf(&[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font << /F1 6 0 R >> /XObject << /Fm0 5 0 R >> >> \
                 /Contents 4 0 R >>"
                    .into(),
            ),
            (
                4,
                format!("<< /Length {} >> stream\n{page_stream}\nendstream", page_stream.len()),
            ),
            (
                5,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Resources << /Font << /F1 6 0 R >> >> \
                     /Length {} >> stream\n{form_stream}\nendstream",
                    form_stream.len()
                ),
            ),
            (6, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into()),
        ])
    }

    #[test]
    fn flatten_form_xobjects_de_shares_repeated_placements() {
        let mut doc = Document::open(&form_placed_twice_fixture()).unwrap();
        // Two placements → two inlined copies.
        let n = doc.flatten_form_xobjects(1).unwrap();
        assert_eq!(n, 2, "both placements inlined");

        let els = doc.page_text_elements(1);
        let dups: Vec<&TextElementInfo> = els.iter().filter(|e| e.text == "DUP").collect();
        assert_eq!(dups.len(), 2, "both copies present as page text");
        // They are at different y (0+5 vs 100+5) → distinct, de-shared runs.
        let (i0, i1) = (dups[0].index, dups[1].index);
        assert_ne!(i0, usize::MAX, "first copy editable");
        assert_ne!(i1, usize::MAX, "second copy editable");
        assert_ne!(i0, i1, "the two copies are independent runs (de-shared)");

        // Editing one copy must NOT change the other.
        let lower = dups.iter().min_by(|a, b| a.y.total_cmp(&b.y)).unwrap();
        doc.replace_text_run(1, lower.index, "ONE").unwrap();
        let after: Vec<String> = doc.page_text_elements(1).into_iter().map(|e| e.text).collect();
        assert!(after.iter().any(|t| t == "ONE"), "edited copy changed: {after:?}");
        assert!(
            after.iter().filter(|t| *t == "DUP").count() == 1,
            "the other copy is untouched (still 'DUP'): {after:?}"
        );
    }

    #[test]
    fn flatten_form_xobjects_renames_colliding_resource() {
        // Page `/F1` = Helvetica (obj 6); form `/F1` = Courier (obj 7) — SAME
        // name, DIFFERENT font objects. After flatten the form's run must use the
        // renamed font so both the page's and the form's text extract correctly.
        let page_stream = "BT /F1 12 Tf 50 150 Td (PAGE) Tj ET\nq 1 0 0 1 30 40 cm /Fm0 Do Q";
        let form_stream = "BT /F1 12 Tf 5 5 Td (FORM) Tj ET";
        let pdf = raw_pdf(&[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font << /F1 6 0 R >> /XObject << /Fm0 5 0 R >> >> \
                 /Contents 4 0 R >>"
                    .into(),
            ),
            (
                4,
                format!("<< /Length {} >> stream\n{page_stream}\nendstream", page_stream.len()),
            ),
            (
                5,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Resources << /Font << /F1 7 0 R >> >> \
                     /Length {} >> stream\n{form_stream}\nendstream",
                    form_stream.len()
                ),
            ),
            (6, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into()),
            (7, "<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".into()),
        ]);
        let mut doc = Document::open(&pdf).unwrap();
        assert_eq!(doc.flatten_form_xobjects(1).unwrap(), 1);

        let els = doc.page_text_elements(1);
        let page = els.iter().find(|e| e.text == "PAGE").expect("PAGE present");
        let form = els.iter().find(|e| e.text == "FORM").expect("FORM present");
        // Page text keeps Helvetica; form text resolves through the renamed
        // Courier font (proves the rename targeted the right /BaseFont).
        assert!(
            page.font_family.contains("Helvetica"),
            "page run stays Helvetica, got {}",
            page.font_family
        );
        assert!(
            form.font_family.contains("Courier"),
            "form run resolves the renamed Courier font, got {}",
            form.font_family
        );
        // Two distinct fonts live in the merged page resources (F1 + F1_fx0).
        let font_names: Vec<Vec<u8>> = doc
            .page_dict(1)
            .unwrap()
            .get(b"Resources")
            .and_then(Object::as_dict)
            .and_then(|r| r.get(b"Font"))
            .and_then(Object::as_dict)
            .map(|f| f.0.keys().cloned().collect())
            .unwrap_or_default();
        assert!(font_names.contains(&b"F1".to_vec()), "page F1 kept: {font_names:?}");
        assert!(
            font_names.iter().any(|n| n.starts_with(b"F1_fx")),
            "colliding form font copied under a fresh name: {font_names:?}"
        );
        // Editing each run still hits the right text.
        doc.replace_text_run(1, page.index, "PG2").unwrap();
        doc.replace_text_run(1, form.index, "FM2").unwrap();
        let t = doc.to_text();
        assert!(t.contains("PG2") && t.contains("FM2"), "both edited: {t:?}");
    }

    #[test]
    fn flatten_form_xobjects_nested_forms_and_cycle_terminate() {
        // Form A's content invokes form B (`Do`). Flatten must inline both, and a
        // self-referencing form must terminate (no infinite loop / panic).
        let a_stream = "BT /F1 12 Tf 5 5 Td (AOUT) Tj ET\n/FmB Do";
        let b_stream = "BT /F1 12 Tf 5 20 Td (BINNER) Tj ET";
        let pdf = raw_pdf(&[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font << /F1 6 0 R >> /XObject << /FmA 5 0 R >> >> \
                 /Contents 4 0 R >>"
                    .into(),
            ),
            (4, {
                let s = "/FmA Do";
                format!("<< /Length {} >> stream\n{s}\nendstream", s.len())
            }),
            (
                5,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Resources << /Font << /F1 6 0 R >> /XObject << /FmB 7 0 R >> >> \
                     /Length {} >> stream\n{a_stream}\nendstream",
                    a_stream.len()
                ),
            ),
            (6, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into()),
            (
                7,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Resources << /Font << /F1 6 0 R >> >> \
                     /Length {} >> stream\n{b_stream}\nendstream",
                    b_stream.len()
                ),
            ),
        ]);
        let mut doc = Document::open(&pdf).unwrap();
        // FmA (1) + nested FmB (1) inlined.
        let n = doc.flatten_form_xobjects(1).unwrap();
        assert_eq!(n, 2, "outer + nested form both inlined");
        let texts: Vec<String> = doc.page_text_elements(1).into_iter().map(|e| e.text).collect();
        assert!(texts.iter().any(|t| t == "AOUT"), "outer form text inlined: {texts:?}");
        assert!(texts.iter().any(|t| t == "BINNER"), "nested form text inlined: {texts:?}");

        // A self-referencing form must terminate without hanging or panicking.
        let loop_stream = "BT /F1 12 Tf 5 5 Td (LOOP) Tj ET\n/Fm0 Do";
        let cyc = raw_pdf(&[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /Font << /F1 6 0 R >> /XObject << /Fm0 5 0 R >> >> \
                 /Contents 4 0 R >>"
                    .into(),
            ),
            (4, {
                let s = "/Fm0 Do";
                format!("<< /Length {} >> stream\n{s}\nendstream", s.len())
            }),
            (
                5,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Resources << /Font << /F1 6 0 R >> /XObject << /Fm0 5 0 R >> >> \
                     /Length {} >> stream\n{loop_stream}\nendstream",
                    loop_stream.len()
                ),
            ),
            (6, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into()),
        ]);
        let mut cdoc = Document::open(&cyc).unwrap();
        let cn = cdoc.flatten_form_xobjects(1).unwrap(); // must return, not hang
        assert!(cn >= 1, "self-ref form inlined at least once");
        let ctexts: Vec<String> = cdoc.page_text_elements(1).into_iter().map(|e| e.text).collect();
        assert!(ctexts.iter().any(|t| t == "LOOP"), "self-ref form text present once-ish: {ctexts:?}");
    }

    #[test]
    fn flatten_form_xobjects_leaves_image_do_untouched() {
        // A page that draws an /Image XObject via `Do`. Flattening forms must NOT
        // touch the image `Do` (it stays a `Do`, still references /Im0).
        let page_stream = "q 64 0 0 64 50 600 cm /Im0 Do Q";
        // Tiny 1x1 DeviceRGB image (raw, no filter). The 3 pixel bytes are
        // ASCII-safe placeholders ('A','B','C') — the flatten logic only inspects
        // `/Subtype`, not the sample data.
        let img_obj = "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 \
             /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length 3 >> \
             stream\nABC\nendstream";
        let pdf = raw_pdf(&[
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] \
                 /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>"
                    .into(),
            ),
            (
                4,
                format!("<< /Length {} >> stream\n{page_stream}\nendstream", page_stream.len()),
            ),
            (5, img_obj.into()),
        ]);
        let mut doc = Document::open(&pdf).unwrap();
        let n = doc.flatten_form_xobjects(1).unwrap();
        assert_eq!(n, 0, "no form XObjects → nothing inlined");
        // The image is still drawn via a `Do` referencing /Im0.
        let content = doc.page_content(1).unwrap();
        assert!(has_op(&content, b"/Im0"), "image still referenced by name");
        assert!(has_op(&content, b"Do"), "image `Do` preserved");
        // And the image is still extractable as an image element.
        assert_eq!(doc.page_image_elements(1).len(), 1, "image element intact");
    }

    // ── rasterizer fidelity: form XObjects (Do), clipping (W/W*), shadings (sh),
    //    and ExtGState blend modes (gs /BM) ─────────────────────────────────────

    /// Assemble a 100×100 single-page PDF whose page content is `page_stream`,
    /// with the given extra `/Resources` entries (e.g. `/XObject << … >>`) and
    /// extra raw objects, then render it to an RGBA `Canvas` at scale 1.0.
    fn render_canvas(
        page_stream: &str,
        resources: &str,
        extra_objects: &[(u32, String)],
    ) -> crate::raster::Canvas {
        let mut objects: Vec<(u32, String)> = vec![
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
            (
                3,
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
                     /Resources << {resources} >> /Contents 4 0 R >>"
                ),
            ),
            (
                4,
                format!("<< /Length {} >> stream\n{page_stream}\nendstream", page_stream.len()),
            ),
        ];
        objects.extend(extra_objects.iter().cloned());
        let doc = Document::open(&raw_pdf(&objects)).unwrap();
        doc.render_page_canvas(1, 1.0).unwrap()
    }

    /// The `[r, g, b]` of a device pixel `(x, y)` (top-left origin) of a canvas.
    fn px(canvas: &crate::raster::Canvas, x: u32, y: u32) -> [u8; 3] {
        let i = ((y as usize) * (canvas.width as usize) + x as usize) * 4;
        [canvas.pixels[i], canvas.pixels[i + 1], canvas.pixels[i + 2]]
    }

    #[test]
    fn renders_form_xobject_via_do() {
        // A form XObject draws a blue 60×60 square at user (20,20)→(80,80); the
        // page invokes it with `Do`. The form's marks must land on the page —
        // this exercises `render_content_into_ctx`'s `Do` → form-recursion path.
        let form_stream = "0 0 1 rg 20 20 60 60 re f";
        let canvas = render_canvas(
            "/Fm0 Do",
            "/XObject << /Fm0 5 0 R >>",
            &[(
                5,
                format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                     /Length {} >> stream\n{form_stream}\nendstream",
                    form_stream.len()
                ),
            )],
        );
        // Centre (device 50,50 — the square spans device x,y 20..80) is blue.
        let c = px(&canvas, 50, 50);
        assert!(
            c[2] > 200 && c[0] < 60 && c[1] < 60,
            "form square centre must be blue, got {c:?}"
        );
        // A corner outside the square stays white paper.
        assert_eq!(px(&canvas, 5, 5), [255, 255, 255], "outside the form square is white");
    }

    #[test]
    fn clip_path_confines_fill() {
        // Clip to a 30×30 box (user 10..40), then fill the WHOLE page red. Only
        // the clipped region may receive ink — content outside the `W n` clip
        // must be absent (stay white).
        let canvas = render_canvas(
            "10 10 30 30 re W n 1 0 0 rg 0 0 100 100 re f",
            "",
            &[],
        );
        // Inside the clip (user (25,25) → device (25, 75)) is red.
        let inside = px(&canvas, 25, 75);
        assert!(
            inside[0] > 200 && inside[1] < 60 && inside[2] < 60,
            "inside the clip must be red, got {inside:?}"
        );
        // Outside the clip stays white despite the full-page fill.
        assert_eq!(px(&canvas, 80, 20), [255, 255, 255], "outside the clip is unpainted");
        assert_eq!(px(&canvas, 5, 5), [255, 255, 255], "outside the clip is unpainted");
    }

    #[test]
    fn axial_shading_varies_across_the_gradient() {
        // A horizontal axial (type-2) gradient from black (t=0, left) to white
        // (t=1, right), painted with `sh` across a full-page clip. The colour
        // must vary along x — a flat fill would be a bug.
        let shading = "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [0 0 100 0] \
                       /Function 6 0 R /Extend [true true] >>";
        let func = "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>";
        let canvas = render_canvas(
            "q 0 0 100 100 re W n /Sh0 sh Q",
            "/Shading << /Sh0 5 0 R >>",
            &[(5, shading.into()), (6, func.into())],
        );
        let left = px(&canvas, 5, 50)[0];
        let right = px(&canvas, 95, 50)[0];
        // Shading space x maps directly to device x (identity CTM, base only
        // flips y): left ≈ black, right ≈ white, and strictly increasing.
        assert!(left < 60, "gradient left edge ≈ black, got {left}");
        assert!(right > 200, "gradient right edge ≈ white, got {right}");
        assert!(
            right as i32 - left as i32 > 100,
            "gradient must vary across x (left {left}, right {right})"
        );
    }

    #[test]
    fn multiply_blend_mode_darkens() {
        // Backdrop: mid-grey over the whole page. Overlay the same mid-grey with
        // an ExtGState `/BM /Multiply`. Multiply(0.5, 0.5) = 0.25 → the overlap
        // must be markedly DARKER than either the backdrop or the source grey.
        let canvas = render_canvas(
            "0.5 g 0 0 100 100 re f q /GS0 gs 0.5 g 0 0 100 100 re f Q",
            "/ExtGState << /GS0 5 0 R >>",
            &[(5, "<< /Type /ExtGState /BM /Multiply >>".into())],
        );
        let c = px(&canvas, 50, 50);
        // 0.5·0.5 ≈ 0.25 → ~64; comfortably below the 128 backdrop/source grey.
        assert!(
            c[0] < 100 && c[1] < 100 && c[2] < 100,
            "Multiply of two mid-greys must darken below 128, got {c:?}"
        );
        // Sanity: with the default Normal blend the same overlay stays grey.
        let normal = render_canvas("0.5 g 0 0 100 100 re f 0.5 g 0 0 100 100 re f", "", &[]);
        let n = px(&normal, 50, 50);
        assert!(n[0] > 110, "Normal-blend overlay stays mid-grey (~128), got {n:?}");
        assert!(c[0] + 30 < n[0], "Multiply ({c:?}) must be darker than Normal ({n:?})");
    }

    #[test]
    fn luminosity_soft_mask_gates_paint() {
        // An ExtGState `/SMask` luminosity group paints the LEFT half white (the
        // right half stays black → alpha 0). Painting a full-page red rect under
        // that gs must show red only where the mask's luminance is non-zero —
        // the right half stays white paper.
        let group_stream = "1 g 0 0 50 100 re f"; // left half white, right half black
        let canvas = render_canvas(
            "q /GS0 gs 1 0 0 rg 0 0 100 100 re f Q",
            "/ExtGState << /GS0 5 0 R >>",
            &[
                (
                    5,
                    "<< /Type /ExtGState /SMask << /S /Luminosity /G 6 0 R >> >>".into(),
                ),
                (
                    6,
                    format!(
                        "<< /Type /XObject /Subtype /Form /BBox [0 0 100 100] \
                         /Group << /S /Transparency /CS /DeviceGray >> \
                         /Length {} >> stream\n{group_stream}\nendstream",
                        group_stream.len()
                    ),
                ),
            ],
        );
        // Left half (mask alpha 1) shows red.
        let left = px(&canvas, 25, 50);
        assert!(
            left[0] > 200 && left[1] < 60 && left[2] < 60,
            "left half (mask=white) must show red, got {left:?}"
        );
        // Right half (mask alpha 0) is masked out → white paper.
        assert_eq!(px(&canvas, 75, 50), [255, 255, 255], "right half (mask=black) is masked out");
    }

    #[test]
    fn shading_pattern_fill_paints_gradient_inside_path() {
        // Fill a 60×60 path with a `/Pattern` colour naming a shading pattern
        // (PatternType 2, horizontal black→white axial). The gradient must fill
        // the path and vary across it; outside the path stays white.
        let pattern = "<< /Type /Pattern /PatternType 2 /Matrix [1 0 0 1 0 0] /Shading 6 0 R >>";
        let shading = "<< /ShadingType 2 /ColorSpace /DeviceGray /Coords [20 0 80 0] \
                       /Function 7 0 R /Extend [true true] >>";
        let func = "<< /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [1] /N 1 >>";
        let canvas = render_canvas(
            "/Pattern cs /P0 scn 20 20 60 60 re f",
            "/Pattern << /P0 5 0 R >>",
            &[(5, pattern.into()), (6, shading.into()), (7, func.into())],
        );
        // Inside the filled box (device x 20..80): a gradient that varies in x.
        let left = px(&canvas, 25, 50)[0];
        let right = px(&canvas, 75, 50)[0];
        assert!(
            right as i32 - left as i32 > 80,
            "shading-pattern fill must vary across x (left {left}, right {right})"
        );
        // Outside the filled path stays white paper.
        assert_eq!(px(&canvas, 5, 5), [255, 255, 255], "outside the pattern-filled path is white");
    }
}
