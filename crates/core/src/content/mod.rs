//! Content-stream editing (ISO 32000-1 §8–9). Pure std.
//!
//! A page's decoded content stream is a flat list of [`Operation`]s — operands
//! (objects) followed by an operator keyword (`Tj`, `Do`, `re`, `cm`, …). We
//! parse it, act on the text-show operators (`Tj`/`TJ`), and re-encode. Every
//! operator we don't touch — images (`Do`), paths, graphics state, and even
//! inline images (`BI…EI`, captured verbatim) — round-trips unchanged, so the
//! background is preserved by construction.

pub mod image;
mod interpret;
pub mod svg_path;
pub mod vector;

use std::collections::{BTreeMap, BTreeSet};

use crate::error::{EngineError, Result};
use crate::font::cmap::TextDecoder;
use crate::lexer::{Lexer, Token};
use crate::object::{Dictionary, Object, StringKind};
use crate::serialize::encode_value;
pub use interpret::{Bounds, Matrix as PageMatrix};
use interpret::{BoundsBuilder, Matrix};

/// One content-stream operation: its operands and the operator keyword.
#[derive(Debug, Clone)]
pub struct Operation {
    /// The operator keyword, e.g. `b"Tj"`. The synthetic `b"BI"` carries a raw
    /// inline image (dict + data + `EI`) in its single string operand.
    pub operator: Vec<u8>,
    /// The operands preceding the operator.
    pub operands: Vec<Object>,
}

/// A located text-show operation on a page.
#[derive(Debug, Clone)]
pub struct TextRun {
    /// 0-based index among the page's text runs (a click target id).
    pub index: usize,
    /// `b"Tj"` or `b"TJ"`.
    pub operator: Vec<u8>,
    /// Decoded text (font-aware: WinAnsi, or `/ToUnicode` for CID/Type0 and
    /// custom-encoded simple fonts).
    pub text: String,
    /// Index of the operation within the parsed operation list.
    pub op_position: usize,
}

/// The kind of a page content element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementKind {
    /// A text-show operation (`Tj`/`TJ`).
    Text,
    /// An XObject draw (`Do`) — usually an image.
    Image,
    /// A vector path object (frame, table rule, line, rectangle…): a run of
    /// construction operators ending in a painting operator.
    Path,
}

/// A high-level, addressable element of a page's content: text, image or shape.
/// `op_start..=op_end` is the inclusive operation range it occupies.
#[derive(Debug, Clone)]
pub struct ContentElement {
    /// 0-based index among the page's elements (a click/selection target id).
    pub index: usize,
    /// What this element is.
    pub kind: ElementKind,
    /// A short label: the text, the image's XObject name, or "shape".
    pub label: String,
    /// First operation of the element (inclusive).
    pub op_start: usize,
    /// Last operation of the element (inclusive).
    pub op_end: usize,
    /// Bounding box in page user space, if it could be computed.
    pub bounds: Option<Bounds>,
    /// For text: the font resource name in effect (`Tf` operand), used to look
    /// up the `/BaseFont` for family/weight/style. `None` for image/path.
    pub font: Option<String>,
    /// For text: the RGB fill colour in effect (`rg`/`g`/`k`), `0..=1` per
    /// channel. `None` means default (black) or non-text.
    pub color: Option<[f64; 3]>,
    /// For text: the effective glyph size in user-space points (the `Tf` size
    /// scaled by the text·CTM vertical scale). `None` for image/path.
    pub font_size: Option<f64>,
    /// For text: the baseline rotation in degrees (from the text·CTM matrix;
    /// `0` for upright text). For images: the rotation of the placement CTM.
    /// `None` for path.
    pub rotation_deg: Option<f64>,
    /// The non-stroking (fill) alpha in effect, `0.0..=1.0`, from the active
    /// `/ExtGState`'s `/ca` (set via the `gs` operator). `None` means the
    /// default (fully opaque). Populated for images; drives editor opacity.
    pub fill_alpha: Option<f64>,
    /// `true` when this element comes from **inside a form XObject** (reached by
    /// recursing through a `Do`), not the top-level content stream. Such elements
    /// have their `op_start`/`op_end` collapsed onto the `Do` op and are **not**
    /// addressable by the top-level index-based mutation/edit APIs (their bounds
    /// are page-space and correct for display). Always `false` for top-level
    /// elements.
    pub nested: bool,
}

/// A reading-order text line: the concatenated runs that share a baseline band,
/// with the union bounding box (PDF user space).
#[derive(Debug, Clone)]
pub struct TextLine {
    pub text: String,
    pub bounds: Bounds,
}

/// Group a page's text elements into reading-order lines (top→bottom, then
/// left→right), clustering by vertical centre. Drives structured-text and search.
pub fn group_lines(elements: &[ContentElement]) -> Vec<TextLine> {
    let mut runs: Vec<(&str, Bounds)> = elements
        .iter()
        .filter(|e| e.kind == ElementKind::Text)
        .filter_map(|e| {
            let b = e.bounds?;
            let t = e.label.trim();
            (!t.is_empty()).then_some((t, b))
        })
        .collect();
    // Top→bottom (PDF y is up, so descending centre), then left→right.
    let center = |b: &Bounds| b.y + b.height / 2.0;
    runs.sort_by(|a, b| {
        center(&b.1)
            .partial_cmp(&center(&a.1))
            .unwrap_or(core::cmp::Ordering::Equal)
            .then(
                a.1.x
                    .partial_cmp(&b.1.x)
                    .unwrap_or(core::cmp::Ordering::Equal),
            )
    });

    let mut lines: Vec<TextLine> = Vec::new();
    let mut row_center = f64::INFINITY;
    let mut row_height = 0.0f64;
    for (text, b) in runs {
        let c = center(&b);
        let tol = b.height.max(row_height).max(1.0) * 0.6;
        if lines.is_empty() || (row_center - c).abs() > tol {
            lines.push(TextLine {
                text: text.to_string(),
                bounds: b,
            });
            row_center = c;
            row_height = b.height;
        } else {
            let line = lines.last_mut().unwrap();
            // Same line: append with a space and union the bounds.
            line.text.push(' ');
            line.text.push_str(text);
            let x0 = line.bounds.x.min(b.x);
            let y0 = line.bounds.y.min(b.y);
            let x1 = (line.bounds.x + line.bounds.width).max(b.x + b.width);
            let y1 = (line.bounds.y + line.bounds.height).max(b.y + b.height);
            line.bounds = Bounds {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            };
            row_height = row_height.max(b.height);
        }
    }
    lines
}

#[inline]
fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0C | 0x00)
}

fn is_text_show(operator: &[u8]) -> bool {
    // `'` (next-line-show) and `"` (set-spacing + next-line-show) are text-show
    // operators too. Counting them keeps the run ordinal consistent across
    // extraction, font lookup and the index-based editing APIs; the
    // interpreters apply their implicit `T*` line move where positions matter.
    matches!(operator, b"Tj" | b"TJ" | b"'" | b"\"")
}

// ─── parsing ────────────────────────────────────────────────────────────────

/// Parse a decoded content stream into operations.
pub fn parse_content(data: &[u8]) -> Result<Vec<Operation>> {
    let mut lexer = Lexer::new(data);
    let mut operands: Vec<Object> = Vec::new();
    let mut operations: Vec<Operation> = Vec::new();

    loop {
        let token = lexer.next_token()?;
        match token {
            Token::Eof => break,
            Token::ArrayOpen => operands.push(Object::Array(read_array(&mut lexer)?)),
            Token::DictOpen => operands.push(Object::Dictionary(read_dict(&mut lexer)?)),
            Token::Integer(n) => operands.push(Object::Integer(n)),
            Token::Real(r) => operands.push(Object::Real(r)),
            Token::Name(n) => operands.push(Object::Name(n)),
            Token::LiteralString(s) => operands.push(Object::String(s, StringKind::Literal)),
            Token::HexString(s) => operands.push(Object::String(s, StringKind::Hex)),
            Token::Keyword(keyword) => match keyword.as_slice() {
                b"true" => operands.push(Object::Boolean(true)),
                b"false" => operands.push(Object::Boolean(false)),
                b"null" => operands.push(Object::Null),
                b"BI" => {
                    // Inline image: capture dict + binary + EI verbatim so it
                    // round-trips byte-for-byte.
                    let raw = capture_inline_image(&mut lexer);
                    operations.push(Operation {
                        operator: b"BI".to_vec(),
                        operands: vec![Object::String(raw, StringKind::Literal)],
                    });
                    operands.clear();
                }
                _ => operations.push(Operation {
                    operator: keyword,
                    operands: std::mem::take(&mut operands),
                }),
            },
            // Stray close-delimiters: tolerate and drop pending operands.
            Token::ArrayClose | Token::DictClose => operands.clear(),
        }
    }

    Ok(operations)
}

fn read_value(lexer: &mut Lexer, token: Token) -> Result<Object> {
    Ok(match token {
        Token::Integer(n) => Object::Integer(n),
        Token::Real(r) => Object::Real(r),
        Token::Name(n) => Object::Name(n),
        Token::LiteralString(s) => Object::String(s, StringKind::Literal),
        Token::HexString(s) => Object::String(s, StringKind::Hex),
        Token::ArrayOpen => Object::Array(read_array(lexer)?),
        Token::DictOpen => Object::Dictionary(read_dict(lexer)?),
        Token::Keyword(k) => match k.as_slice() {
            b"true" => Object::Boolean(true),
            b"false" => Object::Boolean(false),
            b"null" => Object::Null,
            _ => {
                return Err(EngineError::Content(format!(
                    "unexpected operator '{}' inside a value",
                    String::from_utf8_lossy(&k)
                )))
            }
        },
        other => {
            return Err(EngineError::Content(format!(
                "unexpected token inside a value: {other:?}"
            )))
        }
    })
}

fn read_array(lexer: &mut Lexer) -> Result<Vec<Object>> {
    let mut items = Vec::new();
    loop {
        match lexer.next_token()? {
            Token::ArrayClose => break,
            Token::Eof => return Err(EngineError::Content("unterminated array".into())),
            other => items.push(read_value(lexer, other)?),
        }
    }
    Ok(items)
}

fn read_dict(lexer: &mut Lexer) -> Result<Dictionary> {
    let mut dict = Dictionary::new();
    loop {
        match lexer.next_token()? {
            Token::DictClose => break,
            Token::Eof => break,
            Token::Name(key) => {
                let value_token = lexer.next_token()?;
                let value = read_value(lexer, value_token)?;
                dict.set(key, value);
            }
            _ => {} // tolerate junk inside marked-content dicts
        }
    }
    Ok(dict)
}

/// Capture an inline image's body (everything after `BI` up to and including the
/// terminating `EI`) so it can be re-emitted verbatim.
fn capture_inline_image(lexer: &mut Lexer) -> Vec<u8> {
    let data = lexer.data();
    let start = lexer.position();
    let mut pos = start;
    while pos + 1 < data.len() {
        let at_word = data[pos] == b'E'
            && data[pos + 1] == b'I'
            && (pos == 0 || is_whitespace(data[pos - 1]))
            && (pos + 2 >= data.len() || is_whitespace(data[pos + 2]));
        if at_word {
            pos += 2;
            break;
        }
        pos += 1;
    }
    let end = pos.min(data.len());
    lexer.set_position(end);
    data[start..end].to_vec()
}

// ─── encoding ───────────────────────────────────────────────────────────────

/// Re-encode operations back into content-stream bytes.
pub fn encode_content(operations: &[Operation]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in operations {
        if op.operator == b"BI" {
            out.extend_from_slice(b"BI");
            if let Some(Object::String(raw, _)) = op.operands.first() {
                out.extend_from_slice(raw);
            }
            out.push(b'\n');
            continue;
        }
        for operand in &op.operands {
            encode_value(&mut out, operand);
            out.push(b' ');
        }
        out.extend_from_slice(&op.operator);
        out.push(b'\n');
    }
    out
}

// ─── text run operations ──────────────────────────────────────────────────────

/// Per-font-resource decoders for a page, keyed by font resource name (the
/// name used by the `Tf` operator, without the leading `/`).
pub type FontDecoders = BTreeMap<Vec<u8>, TextDecoder>;

fn decode_operand_text(operands: &[Object], decoder: &TextDecoder) -> String {
    let mut text = String::new();
    for operand in operands {
        match operand {
            Object::String(bytes, _) => text.push_str(&decoder.decode(bytes)),
            Object::Array(items) => {
                for item in items {
                    if let Object::String(bytes, _) = item {
                        text.push_str(&decoder.decode(bytes));
                    }
                }
            }
            _ => {}
        }
    }
    text
}

/// A form XObject resolved for text extraction: its decoded content stream, the
/// per-font decoders built from *its own* `/Resources /Font` (falling back to the
/// page's), and its `/Matrix` (form unit space → the space in which it is drawn,
/// default identity). Returned by a [`extract_text_runs_resolved`] resolver when
/// it recognises a `Do` operand as a `/Subtype /Form` XObject.
#[derive(Clone, Debug)]
pub struct FormXObject {
    /// The form's decoded content stream.
    pub content: Vec<u8>,
    /// Per-font decoders for the form's content (its `/Resources /Font`, with the
    /// page's as a fallback per the inheritance rule).
    pub fns: FontDecoders,
    /// The form's `/Matrix` (default identity).
    pub matrix: Matrix,
    /// The form XObject's object id `(number, generation)`, when it is an
    /// indirect object. Threaded into a per-path visited set so a form that
    /// references itself (directly or transitively) is expanded at most once on
    /// any recursion path — the runtime cycle guard, complementing the depth cap.
    pub ref_id: Option<(u32, u16)>,
}

/// Max recursion depth for nested form XObjects (`Do` inside a form inside a
/// form …). Beyond this we stop descending and return what we have — a guard
/// against pathological nesting, complementing the resolver's cycle set.
pub const MAX_FORM_DEPTH: usize = 12;

/// A sentinel `op_position` for runs that live **inside a form XObject**, not in
/// the page's top-level operation list. The top-level op index is meaningless
/// for them, so we flag them rather than report a bogus position. (No consumer
/// edits a form-XObject run by op position; editing targets top-level runs.)
pub const NESTED_OP_POSITION: usize = usize::MAX;

/// List the text runs in a decoded content stream, **recursing into form
/// XObjects** invoked via `Do`. `initial_ctm` seeds the CTM (use
/// [`Matrix::IDENTITY`] at the top level); recursed forms start from the CTM in
/// effect at their `Do`, composed with the form's `/Matrix`, so nested text is
/// gathered correctly. `resolve_form(name)` maps an XObject resource name to a
/// [`FormXObject`] (its content, fonts and matrix) when it is a form, or `None`
/// for image/unresolvable XObjects (ignored, the historical behaviour).
///
/// Runs are returned in document order: each form's runs are appended at the
/// point its `Do` is reached. `depth` is the current nesting level; recursion
/// stops past [`MAX_FORM_DEPTH`]. The `index` field is reassigned sequentially
/// over the whole flattened result; form-XObject runs carry
/// [`NESTED_OP_POSITION`] as their `op_position`.
pub fn extract_text_runs_resolved(
    content: &[u8],
    fonts: &FontDecoders,
    initial_ctm: Matrix,
    resolve_form: &dyn Fn(&[u8]) -> Option<FormXObject>,
    depth: usize,
) -> Result<Vec<TextRun>> {
    let mut visited: BTreeSet<(u32, u16)> = BTreeSet::new();
    let mut runs = text_runs_inner(content, fonts, initial_ctm, resolve_form, depth, &mut visited)?;
    // Renumber so `index` is sequential over the whole (possibly flattened) list.
    for (i, run) in runs.iter_mut().enumerate() {
        run.index = i;
    }
    Ok(runs)
}

/// Recursive worker for [`extract_text_runs_resolved`], threading the per-path
/// `visited` set of form object-refs (the runtime cycle guard).
fn text_runs_inner(
    content: &[u8],
    fonts: &FontDecoders,
    initial_ctm: Matrix,
    resolve_form: &dyn Fn(&[u8]) -> Option<FormXObject>,
    depth: usize,
    visited: &mut BTreeSet<(u32, u16)>,
) -> Result<Vec<TextRun>> {
    let operations = parse_content(content)?;
    let mut runs = Vec::new();
    let fallback = TextDecoder::winansi();
    let mut current: &TextDecoder = &fallback;

    // Track the CTM (graphics state) so a `Do` recurses with the right matrix.
    // Only `cm`/`q`/`Q` affect it; text matrices don't change the CTM.
    let mut ctm = initial_ctm;
    let mut ctm_stack: Vec<Matrix> = Vec::new();
    let nested = depth > 0;

    for (op_position, operation) in operations.iter().enumerate() {
        match operation.operator.as_slice() {
            b"q" => ctm_stack.push(ctm),
            b"Q" => {
                if let Some(m) = ctm_stack.pop() {
                    ctm = m;
                }
            }
            b"cm" => {
                let n = nums(operation);
                if n.len() == 6 {
                    ctm = Matrix::new(n[0], n[1], n[2], n[3], n[4], n[5]).then(&ctm);
                }
            }
            b"Tf" => {
                if let Some(Object::Name(name)) = operation.operands.first() {
                    current = fonts.get(name).unwrap_or(&fallback);
                }
            }
            b"Do" => {
                if depth >= MAX_FORM_DEPTH {
                    continue;
                }
                if let Some(name) = operation.operands.iter().find_map(|o| o.as_name()) {
                    if let Some(form) = resolve_form(name) {
                        // Runtime cycle guard: skip a form already on this path.
                        if form.ref_id.is_some_and(|id| visited.contains(&id)) {
                            continue;
                        }
                        // Child CTM: form unit space → its `/Matrix` → the CTM in
                        // effect at this `Do`. Gives page-space coordinates after
                        // the recursion's own bounds composition.
                        let child_ctm = form.matrix.then(&ctm);
                        let pushed = form.ref_id.map(|id| visited.insert(id)).unwrap_or(false);
                        if let Ok(child) = text_runs_inner(
                            &form.content,
                            &form.fns,
                            child_ctm,
                            resolve_form,
                            depth + 1,
                            visited,
                        ) {
                            runs.extend(child);
                        }
                        if pushed {
                            if let Some(id) = form.ref_id {
                                visited.remove(&id);
                            }
                        }
                    }
                }
            }
            _ if is_text_show(&operation.operator) => {
                runs.push(TextRun {
                    index: runs.len(),
                    operator: operation.operator.clone(),
                    text: decode_operand_text(&operation.operands, current),
                    op_position: if nested { NESTED_OP_POSITION } else { op_position },
                });
            }
            _ => {}
        }
    }
    Ok(runs)
}

/// List the text runs in a decoded content stream, decoding each with the
/// active font's [`TextDecoder`] (selected by the `Tf` operator). Fonts not in
/// `fonts` — and the state before any `Tf` — fall back to WinAnsi. Does **not**
/// descend into form XObjects; use [`extract_text_runs_resolved`] (with a
/// resolver) for that.
pub fn extract_text_runs_with(content: &[u8], fonts: &FontDecoders) -> Result<Vec<TextRun>> {
    extract_text_runs_resolved(content, fonts, Matrix::IDENTITY, &|_| None, 0)
}

/// List the text runs in a decoded content stream using WinAnsi decoding.
pub fn extract_text_runs(content: &[u8]) -> Result<Vec<TextRun>> {
    extract_text_runs_with(content, &FontDecoders::new())
}

fn nth_text_run(operations: &[Operation], index: usize) -> Result<usize> {
    let mut seen = 0usize;
    for (pos, op) in operations.iter().enumerate() {
        if is_text_show(&op.operator) {
            if seen == index {
                return Ok(pos);
            }
            seen += 1;
        }
    }
    Err(EngineError::Missing(format!("text run #{index}")))
}

/// Encode a string for a single-byte WinAnsi font. CID/Type0 (2-byte) fonts are
/// handled by the font-aware path.
fn encode_single_byte(text: &str) -> Vec<u8> {
    crate::font::encode_winansi(text)
}

/// Remove the `index`-th text run, preserving every other operator.
pub fn remove_text_run(content: &[u8], index: usize) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let pos = nth_text_run(&operations, index)?;
    operations.remove(pos);
    Ok(encode_content(&operations))
}

/// Replace the `index`-th text run's text, keeping its position and font.
/// WinAnsi single-byte encoding — correct for base-14 and simple TrueType fonts.
/// Type0/Identity-H runs (2-byte glyph ids) must use
/// [`replace_text_run_encoded`] with pre-encoded glyph bytes.
pub fn replace_text_run(content: &[u8], index: usize, new_text: &str) -> Result<Vec<u8>> {
    replace_text_run_encoded(content, index, encode_single_byte(new_text), StringKind::Literal)
}

/// Replace the `index`-th text run's operand with **pre-encoded** bytes,
/// preserving its position and font. `kind` selects the on-wire form: `Hex`
/// (`<...>`) for 2-byte CID/Identity-H glyph ids (bytes round-trip exactly,
/// embedded NULs and all), `Literal` (`(...)`) for single-byte WinAnsi. This is
/// the font-agnostic primitive behind text editing — the caller (the
/// [`Document`](crate::Document) layer) inspects the run's font and encodes
/// accordingly so modify works with *any* font.
pub fn replace_text_run_encoded(
    content: &[u8],
    index: usize,
    encoded: Vec<u8>,
    kind: StringKind,
) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let pos = nth_text_run(&operations, index)?;
    let operand = Object::String(encoded, kind);
    let operation = &mut operations[pos];
    match operation.operator.as_slice() {
        // `Tj` and `'` (next-line-show) both take a single string operand; the
        // `'` keeps its implicit line move by preserving the operator.
        b"Tj" | b"'" => operation.operands = vec![operand],
        // `"` is `aw ac string "` — preserve the spacing operands, swap the text.
        b"\"" => {
            let aw = operation.operands.first().cloned().unwrap_or(Object::Integer(0));
            let ac = operation.operands.get(1).cloned().unwrap_or(Object::Integer(0));
            operation.operands = vec![aw, ac, operand];
        }
        // TJ: collapse the positioned array to a single string.
        _ => operation.operands = vec![Object::Array(vec![operand])],
    }
    Ok(encode_content(&operations))
}

/// The font **resource name** (the `Tf` operand, e.g. `b"GF7"`) in effect for
/// the `index`-th text run, or `None` if no font was selected before it. Lets
/// the [`Document`](crate::Document) layer resolve the run's font object and
/// encode a replacement for the right font program (simple vs Type0).
pub fn text_run_font_name(content: &[u8], index: usize) -> Result<Option<Vec<u8>>> {
    let operations = parse_content(content)?;
    let mut current: Option<Vec<u8>> = None;
    let mut seen = 0usize;
    for op in &operations {
        if op.operator == b"Tf" {
            if let Some(Object::Name(name)) = op.operands.first() {
                current = Some(name.clone());
            }
        } else if is_text_show(&op.operator) {
            if seen == index {
                return Ok(current);
            }
            seen += 1;
        }
    }
    Err(EngineError::Missing(format!("text run #{index}")))
}

// ─── shape / image / element operations ──────────────────────────────────────

fn is_path_construction(operator: &[u8]) -> bool {
    matches!(operator, b"m" | b"l" | b"c" | b"v" | b"y" | b"re" | b"h")
}

fn is_path_painting(operator: &[u8]) -> bool {
    matches!(
        operator,
        b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n"
    )
}

/// Numeric operands of an operation, as `f64`.
fn nums(op: &Operation) -> Vec<f64> {
    op.operands
        .iter()
        .filter_map(|o| match o {
            Object::Integer(i) => Some(*i as f64),
            Object::Real(r) => Some(*r),
            _ => None,
        })
        .collect()
}

/// Bounding box of a text run from its text/line matrix, the CTM, the font size
/// and the run's user-space advance `width` (real glyph advances when the font
/// carries widths, an average-advance estimate otherwise).
fn text_bounds(tm: &Matrix, ctm: &Matrix, font_size: f64, width: f64) -> Option<Bounds> {
    if font_size == 0.0 {
        return None;
    }
    let m = tm.then(ctm);
    let mut bb = BoundsBuilder::new();
    bb.add_through(&m, 0.0, -0.2 * font_size); // descender
    bb.add_through(&m, width, -0.2 * font_size);
    bb.add_through(&m, width, font_size); // ascender
    bb.add_through(&m, 0.0, font_size);
    bb.build()
}

/// The user-space advance of a text-show operand, summed from real glyph widths
/// (`Tj` string, or `TJ` array with its `1000`-em kerning adjustments applied).
/// `None` when the font has no width table — the caller then estimates.
fn text_run_advance(operands: &[Object], decoder: &TextDecoder, font_size: f64) -> Option<f64> {
    let mut total = 0.0;
    let mut measured = false;
    for operand in operands {
        match operand {
            Object::String(bytes, _) => {
                total += decoder.string_advance(bytes, font_size)?;
                measured = true;
            }
            Object::Array(items) => {
                for item in items {
                    match item {
                        Object::String(bytes, _) => {
                            total += decoder.string_advance(bytes, font_size)?;
                            measured = true;
                        }
                        // TJ number: a position adjustment in 1000-em units,
                        // subtracted from the advance (positive moves left).
                        Object::Integer(_) | Object::Real(_) => {
                            total -= item.as_f64().unwrap_or(0.0) * font_size / 1000.0;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    measured.then_some(total.max(0.0))
}

/// Bounding box of an XObject draw (`Do`): the unit square through the CTM.
fn unit_square_bounds(ctm: &Matrix) -> Option<Bounds> {
    let mut bb = BoundsBuilder::new();
    bb.add_through(ctm, 0.0, 0.0);
    bb.add_through(ctm, 1.0, 0.0);
    bb.add_through(ctm, 1.0, 1.0);
    bb.add_through(ctm, 0.0, 1.0);
    bb.build()
}

/// Add a path construction operator's control points (through the CTM).
fn accumulate_path(operator: &[u8], n: &[f64], ctm: &Matrix, bb: &mut BoundsBuilder) {
    match operator {
        b"re" if n.len() == 4 => {
            let (x, y, w, h) = (n[0], n[1], n[2], n[3]);
            bb.add_through(ctm, x, y);
            bb.add_through(ctm, x + w, y);
            bb.add_through(ctm, x + w, y + h);
            bb.add_through(ctm, x, y + h);
        }
        b"m" | b"l" if n.len() >= 2 => bb.add_through(ctm, n[0], n[1]),
        b"c" if n.len() == 6 => {
            bb.add_through(ctm, n[0], n[1]);
            bb.add_through(ctm, n[2], n[3]);
            bb.add_through(ctm, n[4], n[5]);
        }
        b"v" | b"y" if n.len() == 4 => {
            bb.add_through(ctm, n[0], n[1]);
            bb.add_through(ctm, n[2], n[3]);
        }
        _ => {}
    }
}

/// Group a flat operation list into addressable elements, computing each one's
/// bounding box by interpreting the graphics + text state.
fn elements_from_ops(
    operations: &[Operation],
    fonts: &FontDecoders,
    gstate_alpha: &BTreeMap<String, f64>,
) -> Vec<ContentElement> {
    let mut visited: BTreeSet<(u32, u16)> = BTreeSet::new();
    elements_from_ops_resolved(
        operations,
        fonts,
        gstate_alpha,
        Matrix::IDENTITY,
        &|_| None,
        0,
        &mut visited,
    )
}

/// Like [`elements_from_ops`] but **recursing into form XObjects** (`Do`):
/// `initial_ctm` seeds the CTM (identity at top level), and `resolve_form` maps
/// an XObject resource name to a [`FormXObject`] when it is a form. A form's
/// elements are interpreted with the CTM in effect at its `Do` composed with the
/// form's `/Matrix`, so their bounds come out in page space exactly like
/// top-level elements. A `Do` that resolves to a form yields the form's nested
/// elements (text/shapes/images) instead of a single opaque `Image`; a `Do` that
/// is an image (or unresolvable) keeps the historical single-`Image` behaviour.
/// `visited` holds the form object-refs on the current path (runtime cycle guard).
#[allow(clippy::too_many_arguments)]
fn elements_from_ops_resolved(
    operations: &[Operation],
    fonts: &FontDecoders,
    gstate_alpha: &BTreeMap<String, f64>,
    initial_ctm: Matrix,
    resolve_form: &dyn Fn(&[u8]) -> Option<FormXObject>,
    depth: usize,
    visited: &mut BTreeSet<(u32, u16)>,
) -> Vec<ContentElement> {
    let mut elements = Vec::new();

    // Graphics state. The q/Q stack saves the CTM and the fill alpha together,
    // mirroring the PDF graphics-state save/restore semantics for the bits we
    // surface on elements.
    let mut ctm = initial_ctm;
    let mut fill_alpha = 1.0f64;
    let mut ctm_stack: Vec<(Matrix, f64)> = Vec::new();
    // Text state.
    let mut tm = Matrix::IDENTITY;
    let mut tlm = Matrix::IDENTITY;
    let mut font_size = 0.0f64;
    let mut leading = 0.0f64;
    let fallback = TextDecoder::winansi();
    let mut text_decoder: &TextDecoder = &fallback;
    let mut current_font: Option<String> = None;
    let mut fill_color: Option<[f64; 3]> = None;
    // Current path.
    let mut path_start: Option<usize> = None;
    let mut path_bb = BoundsBuilder::new();
    // Elements built at depth > 0 come from inside a form XObject.
    let nested = depth > 0;

    for (i, op) in operations.iter().enumerate() {
        let operator = op.operator.as_slice();
        match operator {
            b"q" => ctm_stack.push((ctm, fill_alpha)),
            b"Q" => {
                if let Some((m, a)) = ctm_stack.pop() {
                    ctm = m;
                    fill_alpha = a;
                }
            }
            b"cm" => {
                let n = nums(op);
                if n.len() == 6 {
                    ctm = Matrix::new(n[0], n[1], n[2], n[3], n[4], n[5]).then(&ctm);
                }
            }
            // `gs` selects a named `/ExtGState`; the caller pre-resolves each
            // graphics-state dict's `/ca` (fill alpha) into `gstate_alpha`.
            b"gs" => {
                if let Some(Object::Name(name)) = op.operands.first() {
                    let key = String::from_utf8_lossy(name);
                    if let Some(&a) = gstate_alpha.get(key.as_ref()) {
                        fill_alpha = a;
                    }
                }
            }
            b"BT" => {
                tm = Matrix::IDENTITY;
                tlm = Matrix::IDENTITY;
            }
            b"Tf" => {
                if let Some(&size) = nums(op).last() {
                    font_size = size;
                }
                if let Some(Object::Name(name)) = op.operands.first() {
                    text_decoder = fonts.get(name).unwrap_or(&fallback);
                    current_font = Some(String::from_utf8_lossy(name).into_owned());
                }
            }
            // Fill colour (non-stroking). Text inherits the fill colour.
            b"rg" => {
                let n = nums(op);
                if n.len() == 3 {
                    fill_color = Some([n[0], n[1], n[2]]);
                }
            }
            b"g" => {
                let n = nums(op);
                if n.len() == 1 {
                    fill_color = Some([n[0], n[0], n[0]]);
                }
            }
            b"k" => {
                let n = nums(op);
                if n.len() == 4 {
                    // Naive CMYK → RGB: channel = (1 − ink)(1 − black).
                    let kk = n[3];
                    fill_color = Some([
                        (1.0 - n[0]) * (1.0 - kk),
                        (1.0 - n[1]) * (1.0 - kk),
                        (1.0 - n[2]) * (1.0 - kk),
                    ]);
                }
            }
            b"TL" => {
                if let Some(&l) = nums(op).first() {
                    leading = l;
                }
            }
            b"Td" => {
                let n = nums(op);
                if n.len() == 2 {
                    tlm = Matrix::translate(n[0], n[1]).then(&tlm);
                    tm = tlm;
                }
            }
            b"TD" => {
                let n = nums(op);
                if n.len() == 2 {
                    leading = -n[1];
                    tlm = Matrix::translate(n[0], n[1]).then(&tlm);
                    tm = tlm;
                }
            }
            b"Tm" => {
                let n = nums(op);
                if n.len() == 6 {
                    tlm = Matrix::new(n[0], n[1], n[2], n[3], n[4], n[5]);
                    tm = tlm;
                }
            }
            b"T*" => {
                tlm = Matrix::translate(0.0, -leading).then(&tlm);
                tm = tlm;
            }
            _ if is_text_show(operator) => {
                // `'` (next-line-show) and `"` (set-spacing + next-line-show)
                // carry an implicit `T*` BEFORE showing: advance the text line
                // matrix by the leading. Without this the run lands on the
                // previous baseline (the bug that left whole invoice blocks
                // shifted up by their cumulative leading and dropped the run
                // shown by each `'`).
                if matches!(operator, b"'" | b"\"") {
                    tlm = Matrix::translate(0.0, -leading).then(&tlm);
                    tm = tlm;
                }
                let text = decode_operand_text(&op.operands, text_decoder);
                let char_count = text.chars().count();
                // Real glyph advances when the font carries widths; otherwise a
                // 0.5-em estimate. Drives both the run's width and the pen
                // advance for the following run on the line.
                let width = text_run_advance(&op.operands, text_decoder, font_size)
                    .unwrap_or(char_count as f64 * 0.5 * font_size);
                let bounds = text_bounds(&tm, &ctm, font_size, width);
                // Combined text→device matrix: the `Tf` size scaled by its
                // vertical scale gives the on-page glyph size; its x-axis angle
                // gives the baseline rotation.
                let m = tm.then(&ctm).0;
                let scale_y = (m[2] * m[2] + m[3] * m[3]).sqrt();
                let eff_size = if scale_y > 0.0 { font_size * scale_y } else { font_size };
                let rot = m[1].atan2(m[0]).to_degrees();
                elements.push(ContentElement {
                    index: 0,
                    kind: ElementKind::Text,
                    label: text,
                    op_start: i,
                    op_end: i,
                    bounds,
                    font: current_font.clone(),
                    color: fill_color,
                    font_size: Some(eff_size),
                    rotation_deg: Some(if rot.abs() < 1e-6 { 0.0 } else { rot }),
                    fill_alpha: Some(fill_alpha),
                    nested,
                });
                tm = Matrix::translate(width, 0.0).then(&tm);
            }
            b"Do" => {
                let name = op.operands.iter().find_map(|o| o.as_name());
                // A form XObject: recurse, interpreting its content under the
                // CTM in effect here · the form's `/Matrix`, so its nested
                // elements land in page space. Falls through to the image case
                // when the name isn't a (resolvable) form, or nesting is capped.
                let form = if depth < MAX_FORM_DEPTH {
                    name.and_then(resolve_form)
                } else {
                    None
                };
                // Runtime cycle guard: a form already on this path is skipped
                // entirely (don't recurse, and don't fall back to an image box).
                if let Some(form) = form {
                    if form.ref_id.is_some_and(|id| visited.contains(&id)) {
                        continue;
                    }
                    let child_ctm = form.matrix.then(&ctm);
                    let pushed = form.ref_id.map(|id| visited.insert(id)).unwrap_or(false);
                    let mut child = elements_from_ops_resolved(
                        &parse_content(&form.content).unwrap_or_default(),
                        &form.fns,
                        gstate_alpha,
                        child_ctm,
                        resolve_form,
                        depth + 1,
                        visited,
                    );
                    if pushed {
                        if let Some(id) = form.ref_id {
                            visited.remove(&id);
                        }
                    }
                    // Re-anchor the form's elements to this `Do`'s op index so the
                    // stable `op_start` sort places them where the form is drawn
                    // (their own op indices are relative to the form's stream and
                    // would otherwise scramble document order). Nested elements
                    // aren't edited by op position, so collapsing the range is safe.
                    for c in &mut child {
                        c.op_start = i;
                        c.op_end = i;
                    }
                    elements.extend(child);
                    continue;
                }
                let label = name
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .unwrap_or_default();
                // The placement CTM's x-axis angle is the image's rotation,
                // exactly as for a text baseline.
                let m = ctm.0;
                let img_rot = m[1].atan2(m[0]).to_degrees();
                elements.push(ContentElement {
                    index: 0,
                    kind: ElementKind::Image,
                    label,
                    op_start: i,
                    op_end: i,
                    bounds: unit_square_bounds(&ctm),
                    font: None,
                    color: None,
                    font_size: None,
                    rotation_deg: Some(if img_rot.abs() < 1e-6 { 0.0 } else { img_rot }),
                    fill_alpha: Some(fill_alpha),
                    nested,
                });
            }
            _ if is_path_construction(operator) => {
                path_start.get_or_insert(i);
                accumulate_path(operator, &nums(op), &ctm, &mut path_bb);
            }
            _ if is_path_painting(operator) => {
                if let Some(start) = path_start.take() {
                    elements.push(ContentElement {
                        index: 0,
                        kind: ElementKind::Path,
                        label: "shape".to_string(),
                        op_start: start,
                        op_end: i,
                        bounds: path_bb.build(),
                        font: None,
                        color: None,
                        font_size: None,
                        rotation_deg: None,
                        fill_alpha: Some(fill_alpha),
                        nested,
                    });
                }
                path_bb = BoundsBuilder::new();
            }
            _ => {}
        }
    }

    elements.sort_by_key(|e| e.op_start);
    for (idx, element) in elements.iter_mut().enumerate() {
        element.index = idx;
    }
    elements
}

/// List all addressable elements (text, images, shapes) of a content stream,
/// decoding text labels with the page's fonts (WinAnsi + `/ToUnicode`) and
/// resolving each element's fill alpha through `gstate_alpha` (a map of
/// `/ExtGState` resource name → `/ca`, built from the page's resources).
pub fn extract_elements_with(
    content: &[u8],
    fonts: &FontDecoders,
    gstate_alpha: &BTreeMap<String, f64>,
) -> Result<Vec<ContentElement>> {
    let operations = parse_content(content)?;
    Ok(elements_from_ops(&operations, fonts, gstate_alpha))
}

/// List all addressable elements (text, images, shapes) of a content stream,
/// **recursing into form XObjects** invoked via `Do`. `resolve_form(name)` maps
/// an XObject resource name to a [`FormXObject`] when it is a form (its content,
/// own fonts, and `/Matrix`); image/unresolvable XObjects fall back to a single
/// opaque `Image` element. Each form's elements are interpreted with the CTM in
/// effect at the `Do` composed with the form's `/Matrix`, so their bounds come
/// out in page user space just like top-level elements. This is what makes text
/// drawn inside reusable form XObjects (invoice/template content) addressable.
pub fn extract_elements_resolved(
    content: &[u8],
    fonts: &FontDecoders,
    gstate_alpha: &BTreeMap<String, f64>,
    resolve_form: &dyn Fn(&[u8]) -> Option<FormXObject>,
) -> Result<Vec<ContentElement>> {
    let operations = parse_content(content)?;
    let mut visited: BTreeSet<(u32, u16)> = BTreeSet::new();
    Ok(elements_from_ops_resolved(
        &operations,
        fonts,
        gstate_alpha,
        Matrix::IDENTITY,
        resolve_form,
        0,
        &mut visited,
    ))
}

/// List all addressable elements (text, images, shapes) of a content stream.
pub fn extract_elements(content: &[u8]) -> Result<Vec<ContentElement>> {
    extract_elements_with(content, &FontDecoders::new(), &BTreeMap::new())
}

/// Remove the element at `index` (a text, image, or whole shape), preserving
/// everything else verbatim.
pub fn remove_element(content: &[u8], index: usize) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let element = elements_from_ops(&operations, &FontDecoders::new(), &BTreeMap::new())
        .into_iter()
        .nth(index)
        .ok_or_else(|| EngineError::Missing(format!("content element #{index}")))?;
    operations.drain(element.op_start..=element.op_end);
    Ok(encode_content(&operations))
}

/// Strip every marked-content block tagged `/GPHF` whose property dictionary's
/// `/T` value equals `subtype` (`b"h"` for headers, `b"f"` for footers), removing
/// the `BDC … EMC` span **and everything between** from the content stream.
///
/// This is how previously-baked running headers/footers are removed
/// idempotently: the bake wraps each H/F draw in `/GPHF <</T (h)>> BDC … EMC`, so
/// stripping the tagged spans deletes exactly the baked operations (the text is
/// physically gone from the stream, not merely covered) and leaves all other
/// content — including any non-H/F marked content — untouched. Nesting is tracked
/// so a matching `EMC` is found at the depth where the tagged `BDC` opened.
pub fn strip_marked_content(content: &[u8], subtype: &[u8]) -> Result<Vec<u8>> {
    let operations = parse_content(content)?;
    let mut kept: Vec<Operation> = Vec::with_capacity(operations.len());
    // Depth of the currently-open `GPHF`-of-this-subtype block we are dropping;
    // `None` when we are keeping ops. We also track the global BDC/BMC nesting so
    // the right `EMC` closes our block even if it contains nested marked content.
    let mut drop_open_depth: Option<usize> = None;
    let mut depth: usize = 0;
    for op in operations {
        let is_open = op.operator == b"BDC" || op.operator == b"BMC";
        let is_close = op.operator == b"EMC";
        if drop_open_depth.is_some() {
            // Inside a dropped block: skip ops, only tracking depth to find the end.
            if is_open {
                depth += 1;
            } else if is_close {
                depth -= 1;
                if Some(depth) == drop_open_depth {
                    drop_open_depth = None;
                }
            }
            continue;
        }
        if is_open {
            if op.operator == b"BDC" && bdc_is_gphf(&op.operands, subtype) {
                // Open a dropped block at the current depth; drop this BDC too.
                drop_open_depth = Some(depth);
                depth += 1;
                continue;
            }
            depth += 1;
        } else if is_close {
            depth = depth.saturating_sub(1);
        }
        kept.push(op);
    }
    Ok(encode_content(&kept))
}

/// `true` when a `BDC` operator's operands are `/GPHF <</T (subtype) …>>` — our
/// stable marker tagging a baked header (`subtype == b"h"`) or footer (`b"f"`).
fn bdc_is_gphf(operands: &[Object], subtype: &[u8]) -> bool {
    if operands.first().and_then(Object::as_name) != Some(b"GPHF".as_slice()) {
        return false;
    }
    let Some(Object::Dictionary(props)) = operands.get(1) else {
        return false;
    };
    match props.get(b"T") {
        Some(Object::String(bytes, _)) => bytes.as_slice() == subtype,
        Some(Object::Name(name)) => name.as_slice() == subtype,
        _ => false,
    }
}

/// Recover the text shown inside the **first** `/GPHF <</T (subtype)>> BDC … EMC`
/// span of a content stream (`b"h"` for a baked header, `b"f"` for a footer),
/// the reader counterpart of [`strip_marked_content`]. Walks the operations,
/// and once inside the tagged block collects the string operands of every
/// text-show operator (`Tj`/`TJ`/`'`/`"` — the `"` numeric word/char-spacing
/// operands are ignored, only its string is taken), decoding each as a PDF text
/// string (UTF-16BE BOM, else WinAnsi — the inverse of the bake's
/// `encode_winansi`). Returns the joined text, or `None` when no such span
/// exists. Nesting is tracked like [`strip_marked_content`] so a matching `EMC`
/// closes the block at the depth its `BDC` opened.
pub fn extract_marked_content_text(content: &[u8], subtype: &[u8]) -> Option<String> {
    let operations = parse_content(content).ok()?;
    // Depth at which the tagged block opened (we are collecting while `Some`);
    // `depth` tracks the global BDC/BMC nesting so the right `EMC` ends it.
    let mut collect_open_depth: Option<usize> = None;
    let mut depth: usize = 0;
    let mut out = String::new();
    let mut found = false;
    for op in &operations {
        let is_open = op.operator == b"BDC" || op.operator == b"BMC";
        let is_close = op.operator == b"EMC";
        if collect_open_depth.is_some() {
            if is_open {
                depth += 1;
            } else if is_close {
                depth -= 1;
                if Some(depth) == collect_open_depth {
                    // First tagged span closed — return what we gathered.
                    return Some(out);
                }
            } else if is_text_show(&op.operator) {
                append_show_text(&mut out, &op.operands);
            }
            continue;
        }
        if is_open {
            if op.operator == b"BDC" && bdc_is_gphf(&op.operands, subtype) {
                collect_open_depth = Some(depth);
                found = true;
                depth += 1;
                continue;
            }
            depth += 1;
        } else if is_close {
            depth = depth.saturating_sub(1);
        }
    }
    // An unterminated tagged block (no closing `EMC`) still yields its text.
    if found {
        Some(out)
    } else {
        None
    }
}

/// Append the decoded string operands of one text-show operator to `out`,
/// decoding each `Object::String` (and the strings inside a `TJ` array) as a PDF
/// text string. Non-string operands (the `aw`/`ac` numbers of `"`) are skipped.
fn append_show_text(out: &mut String, operands: &[Object]) {
    for operand in operands {
        match operand {
            Object::String(bytes, _) => out.push_str(&crate::font::decode_pdf_text(bytes)),
            Object::Array(items) => {
                for item in items {
                    if let Object::String(bytes, _) = item {
                        out.push_str(&crate::font::decode_pdf_text(bytes));
                    }
                }
            }
            _ => {}
        }
    }
}

/// Duplicate the element at `index`, inserting the copy right after it (it lands
/// at the same position, ready to be moved). Works for text, images and shapes.
pub fn duplicate_element(content: &[u8], index: usize) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let element = elements_from_ops(&operations, &FontDecoders::new(), &BTreeMap::new())
        .into_iter()
        .nth(index)
        .ok_or_else(|| EngineError::Missing(format!("content element #{index}")))?;
    let copy: Vec<Operation> = operations[element.op_start..=element.op_end].to_vec();
    let insert_at = element.op_end + 1;
    for (offset, op) in copy.into_iter().enumerate() {
        operations.insert(insert_at + offset, op);
    }
    Ok(encode_content(&operations))
}

/// Move the element at `index` by `(dx, dy)` user-space units, by wrapping its
/// operations in `q … Q` with a translation matrix. Works for text, images and
/// shapes without touching their internal coordinates.
pub fn move_element(content: &[u8], index: usize, dx: f64, dy: f64) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let element = elements_from_ops(&operations, &FontDecoders::new(), &BTreeMap::new())
        .into_iter()
        .nth(index)
        .ok_or_else(|| EngineError::Missing(format!("content element #{index}")))?;

    // Insert closing `Q` after the element, then `q` + `cm` before it, so the
    // final order is: q  1 0 0 1 dx dy cm  <element ops>  Q
    operations.insert(
        element.op_end + 1,
        Operation {
            operator: b"Q".to_vec(),
            operands: Vec::new(),
        },
    );
    operations.insert(
        element.op_start,
        Operation {
            operator: b"cm".to_vec(),
            operands: vec![
                Object::Integer(1),
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(1),
                Object::Real(dx),
                Object::Real(dy),
            ],
        },
    );
    operations.insert(
        element.op_start,
        Operation {
            operator: b"q".to_vec(),
            operands: Vec::new(),
        },
    );
    Ok(encode_content(&operations))
}

/// Apply an arbitrary affine transform to the element at `index` by wrapping its
/// operations in `q … Q` with the caller's matrix `m = [a, b, c, d, e, f]`.
///
/// This generalises [`move_element`] (whose `cm` is the pure translate
/// `[1, 0, 0, 1, dx, dy]`) to a full `cm`, so it covers scale, rotation, shear
/// and translation in one call. Because everything happens via the matrix, it
/// works identically for text, images and shapes — their internal coordinates
/// are never touched. The emitted wrapping is exactly:
/// `q  a b c d e f cm  <element ops>  Q`.
pub fn transform_element(content: &[u8], index: usize, m: [f64; 6]) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let element = elements_from_ops(&operations, &FontDecoders::new(), &BTreeMap::new())
        .into_iter()
        .nth(index)
        .ok_or_else(|| EngineError::Missing(format!("content element #{index}")))?;

    // Insert closing `Q` after the element, then `q` + `cm` before it, so the
    // final order is: q  a b c d e f cm  <element ops>  Q
    operations.insert(
        element.op_end + 1,
        Operation {
            operator: b"Q".to_vec(),
            operands: Vec::new(),
        },
    );
    operations.insert(
        element.op_start,
        Operation {
            operator: b"cm".to_vec(),
            operands: vec![
                Object::Real(m[0]),
                Object::Real(m[1]),
                Object::Real(m[2]),
                Object::Real(m[3]),
                Object::Real(m[4]),
                Object::Real(m[5]),
            ],
        },
    );
    operations.insert(
        element.op_start,
        Operation {
            operator: b"q".to_vec(),
            operands: Vec::new(),
        },
    );
    Ok(encode_content(&operations))
}

/// An in-place restyle for a vector path: any `Some(_)` field overrides that part
/// of the graphics state for the path's paint; `None` fields keep the inherited
/// state. RGB colours are `0.0..=1.0` per channel; `dash` is the `d` array
/// (empty = solid). `*_alpha` are best-effort (see [`set_path_style`]).
#[derive(Debug, Clone, Default)]
pub struct PathStyle {
    /// Non-stroking (fill) colour, `r g b rg`.
    pub fill: Option<[f64; 3]>,
    /// Stroking colour, `r g b RG`.
    pub stroke: Option<[f64; 3]>,
    /// Line width, `w w`.
    pub stroke_width: Option<f64>,
    /// Non-stroking alpha (`/ca`). Cannot be set by an inline content operator —
    /// see [`set_path_style`]; ignored when no resource-level `gs` is available.
    pub fill_alpha: Option<f64>,
    /// Stroking alpha (`/CA`). Same limitation as `fill_alpha`.
    pub stroke_alpha: Option<f64>,
    /// Dash pattern (`[ … ] 0 d`), empty array = solid.
    pub dash: Option<Vec<f64>>,
}

/// Re-style the **path** element at `index` in place, wrapping its operation
/// range in `q … Q` and **injecting** the requested state operators *before* the
/// path's construction + paint ops, so they override the inherited graphics
/// state for that run only. The original paint operator (`f`/`S`/`B`/`b`…) is
/// preserved — it now paints with the overridden state — and the `q/Q` isolates
/// the change from following content. The original colour operators in the
/// element are **not** mutated in situ.
///
/// Returns `Err` if the element at `index` is not a [`ElementKind::Path`].
///
/// Only `Some(_)` fields emit an operator (`fill`→`r g b rg`, `stroke`→
/// `r g b RG`, `stroke_width`→`w w`, `dash`→`[ … ] 0 d`); `None` fields are left
/// to the inherited state.
///
/// **Opacity:** PDF alpha (`/ca`, `/CA`) can only be set via the `gs` operator,
/// which references a *named* `/ExtGState` resource — something a pure
/// content-stream edit cannot create on its own. When `gstate` is `Some(name)`
/// (the caller having registered an `/ExtGState` resource named `name` carrying
/// the requested `/ca`/`/CA`), a `/<name> gs` op is injected first inside the
/// `q … Q` wrap so the alpha applies to this path run only. When `gstate` is
/// `None`, the `fill_alpha`/`stroke_alpha` fields are ignored (no `gs` is
/// emitted). The document-level [`Document::set_path_style`] registers the
/// resource and passes the name through, so opacity works end-to-end there.
pub fn set_path_style(
    content: &[u8],
    index: usize,
    style: &PathStyle,
    gstate: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let element = elements_from_ops(&operations, &FontDecoders::new(), &BTreeMap::new())
        .into_iter()
        .nth(index)
        .ok_or_else(|| EngineError::Missing(format!("content element #{index}")))?;
    if element.kind != ElementKind::Path {
        return Err(EngineError::Unsupported(format!(
            "set_path_style: element #{index} is not a path"
        )));
    }

    // Build the override operators to inject right after the opening `q`, before
    // the path's own construction + paint ops.
    let mut overrides: Vec<Operation> = Vec::new();
    // Opacity first: a `/<name> gs` selects the caller-registered `/ExtGState`
    // (its `/ca`/`/CA`) for this run. Only emitted when the caller supplies a
    // name AND an alpha was actually requested.
    if let Some(name) = gstate {
        if style.fill_alpha.is_some() || style.stroke_alpha.is_some() {
            overrides.push(gs_op(name));
        }
    }
    if let Some([r, g, b]) = style.fill {
        overrides.push(rgb_op(b"rg", r, g, b));
    }
    if let Some([r, g, b]) = style.stroke {
        overrides.push(rgb_op(b"RG", r, g, b));
    }
    if let Some(width) = style.stroke_width {
        overrides.push(Operation {
            operator: b"w".to_vec(),
            operands: vec![Object::Real(width)],
        });
    }
    if let Some(dash) = &style.dash {
        overrides.push(Operation {
            operator: b"d".to_vec(),
            operands: vec![
                Object::Array(dash.iter().map(|v| Object::Real(*v)).collect()),
                Object::Integer(0),
            ],
        });
    }
    // Insert closing `Q` after the element, then `q` + the overrides before it,
    // so the final order is: q  <overrides>  <element ops>  Q
    operations.insert(
        element.op_end + 1,
        Operation {
            operator: b"Q".to_vec(),
            operands: Vec::new(),
        },
    );
    // Insert overrides in reverse at op_start so they end up in declared order.
    for op in overrides.into_iter().rev() {
        operations.insert(element.op_start, op);
    }
    operations.insert(
        element.op_start,
        Operation {
            operator: b"q".to_vec(),
            operands: Vec::new(),
        },
    );
    Ok(encode_content(&operations))
}

/// One RGB colour-setting operation (`rg` for fill, `RG` for stroke).
fn rgb_op(operator: &[u8], r: f64, g: f64, b: f64) -> Operation {
    Operation {
        operator: operator.to_vec(),
        operands: vec![Object::Real(r), Object::Real(g), Object::Real(b)],
    }
}

/// A `/<name> gs` operation selecting a named `/ExtGState` resource.
fn gs_op(name: &[u8]) -> Operation {
    Operation {
        operator: b"gs".to_vec(),
        operands: vec![Object::Name(name.to_vec())],
    }
}

/// Apply a constant opacity to the element at `index` (text, image **or** shape)
/// by wrapping its operation range in `q /<gstate> gs … Q`. `gstate` is the
/// resource name of a `/ExtGState` (carrying `/ca` and optionally `/CA`) the
/// caller has registered on the page's `/Resources`. Like [`transform_element`]
/// the element's internal coordinates are untouched; only the graphics state in
/// effect for that run changes. For an image (`Do`) this is the only way to set
/// its alpha; shapes can use this or [`set_path_style`]'s alpha (same mechanism).
///
/// The emitted wrapping is exactly: `q  /<gstate> gs  <element ops>  Q`.
pub fn set_element_opacity(content: &[u8], index: usize, gstate: &[u8]) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let element = elements_from_ops(&operations, &FontDecoders::new(), &BTreeMap::new())
        .into_iter()
        .nth(index)
        .ok_or_else(|| EngineError::Missing(format!("content element #{index}")))?;

    // Insert closing `Q` after the element, then `q` + `/<gstate> gs` before it,
    // so the final order is: q  /<gstate> gs  <element ops>  Q
    operations.insert(
        element.op_end + 1,
        Operation {
            operator: b"Q".to_vec(),
            operands: Vec::new(),
        },
    );
    operations.insert(element.op_start, gs_op(gstate));
    operations.insert(
        element.op_start,
        Operation {
            operator: b"q".to_vec(),
            operands: Vec::new(),
        },
    );
    Ok(encode_content(&operations))
}

/// Compute the **effective** graphics state in force at operation `boundary` by a
/// last-write-wins scan over `operations[0..boundary]`, returning the original
/// state-setting [`Operation`]s (cloned, in a canonical re-emit order) that the
/// paint at `boundary` depends on but which live *outside* the element's range.
///
/// Captured slots (each tracks the last value still in scope, honouring the
/// `q`/`Q` save/restore stack): fill colour (`rg`/`g`/`k`, or `cs`+`scn`/`sc`),
/// stroke colour (`RG`/`G`/`K`, or `CS`+`SCN`/`SC`), line width (`w`), dash (`d`),
/// line cap (`J`), line join (`j`), miter limit (`M`), the active `/ExtGState`
/// (`gs`), and the text font+size (`Tf`, painted with the fill colour). Only
/// operators that were *actually set* before `boundary` are emitted — no defaults
/// are fabricated. The colour-space (`cs`/`CS`) op is paired with its `scn`/`SCN`
/// so the colour resolves in the right space at the new position.
///
/// Re-emit order: graphics-state resource (`gs`) first, then line params
/// (`w`/`d`/`J`/`j`/`M`), then fill colour-space + colour, then stroke
/// colour-space + colour, then font (`Tf`). This places appearance state before
/// the moved run inside its `q … Q`; the trailing `Q` restores, so nothing leaks.
fn effective_state_ops(operations: &[Operation], boundary: usize) -> Vec<Operation> {
    /// One save/restore frame: the last-seen op for each tracked slot, `None`
    /// until set. `fill_cs`/`stroke_cs` hold the colour-space op so a later
    /// `scn`/`SCN` re-emits in the right space; `fill`/`stroke` hold the colour op.
    #[derive(Clone, Default)]
    struct Frame {
        gs: Option<Operation>,
        line_width: Option<Operation>,
        dash: Option<Operation>,
        line_cap: Option<Operation>,
        line_join: Option<Operation>,
        miter: Option<Operation>,
        fill_cs: Option<Operation>,
        fill: Option<Operation>,
        stroke_cs: Option<Operation>,
        stroke: Option<Operation>,
        font: Option<Operation>,
    }

    let mut st = Frame::default();
    let mut stack: Vec<Frame> = Vec::new();

    for op in operations.iter().take(boundary) {
        match op.operator.as_slice() {
            b"q" => stack.push(st.clone()),
            b"Q" => {
                if let Some(prev) = stack.pop() {
                    st = prev;
                }
            }
            // Fill colour. A shorthand colour op (`rg`/`g`/`k`) also fixes the
            // space, so a stale `cs` no longer applies → drop the paired `cs`.
            b"rg" | b"g" | b"k" => {
                st.fill = Some(op.clone());
                st.fill_cs = None;
            }
            // Stroke colour shorthand — same reasoning for the stroke `CS`.
            b"RG" | b"G" | b"K" => {
                st.stroke = Some(op.clone());
                st.stroke_cs = None;
            }
            // Explicit colour-space selection (paired with a following scn/SCN).
            b"cs" => st.fill_cs = Some(op.clone()),
            b"CS" => st.stroke_cs = Some(op.clone()),
            b"scn" | b"sc" => st.fill = Some(op.clone()),
            b"SCN" | b"SC" => st.stroke = Some(op.clone()),
            b"w" => st.line_width = Some(op.clone()),
            b"d" => st.dash = Some(op.clone()),
            b"J" => st.line_cap = Some(op.clone()),
            b"j" => st.line_join = Some(op.clone()),
            b"M" => st.miter = Some(op.clone()),
            b"gs" => st.gs = Some(op.clone()),
            b"Tf" => st.font = Some(op.clone()),
            _ => {}
        }
    }

    // Emit in a stable, render-correct order. `cs`/`CS` precede their `scn`/`SCN`.
    let mut out: Vec<Operation> = Vec::new();
    out.extend(st.gs);
    out.extend(st.line_width);
    out.extend(st.dash);
    out.extend(st.line_cap);
    out.extend(st.line_join);
    out.extend(st.miter);
    out.extend(st.fill_cs);
    out.extend(st.fill);
    out.extend(st.stroke_cs);
    out.extend(st.stroke);
    out.extend(st.font);
    out
}

/// Change the paint order (z-order) of the element at `index` by splicing its
/// whole operation range to a new position in the page content stream and
/// re-wrapping it in `q … Q`.
///
/// To keep the element's **appearance** identical at its new home, the graphics
/// state it depends on but does not itself set — fill/stroke colour (`rg`/`g`/`k`,
/// `RG`/`G`/`K`, `cs`/`CS`+`scn`/`SCN`), line width (`w`), dash (`d`), caps/joins
/// (`J`/`j`/`M`), the active `/ExtGState` (`gs`) and, for text, the font (`Tf`) —
/// is captured by a last-write-wins scan over the operators preceding the element
/// and **re-emitted inside the `q … Q`, before the moved run**. The trailing `Q`
/// restores the prior state, so the move neither inherits a wrong state at its new
/// position nor leaks state onto neighbours.
///
/// * `to_front == true` → moved to the **end** of the stream → painted last →
///   visually on top of everything else.
/// * `to_front == false` → moved to the **start** of the stream → painted first
///   → behind everything else.
///
/// Works for text, image and shape elements addressed by their unified index.
/// (Images carry no colour state; capturing nothing extra is harmless.) The
/// element's index changes after the splice (it is now first or last among the
/// elements) — callers should re-read [`extract_elements`]. Returns `Err` for an
/// out-of-range index. The stream stays balanced (the spliced run is
/// self-contained and the `q … Q` it gains is itself balanced).
pub fn reorder_element(content: &[u8], index: usize, to_front: bool) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let element = elements_from_ops(&operations, &FontDecoders::new(), &BTreeMap::new())
        .into_iter()
        .nth(index)
        .ok_or_else(|| EngineError::Missing(format!("content element #{index}")))?;

    // Capture the effective graphics state at the element's first op, BEFORE the
    // range is lifted (indices still refer to the original stream).
    let state_ops = effective_state_ops(&operations, element.op_start);

    // Lift the element's operation range out of the stream (it is contiguous).
    let moved: Vec<Operation> = operations
        .drain(element.op_start..=element.op_end)
        .collect();

    // Re-wrap the lifted run in a balanced `q … Q`, re-emitting the captured
    // state before it so it renders identically at its new position and does not
    // leak its own state onto neighbours.
    let mut wrapped: Vec<Operation> = Vec::with_capacity(moved.len() + state_ops.len() + 2);
    wrapped.push(Operation {
        operator: b"q".to_vec(),
        operands: Vec::new(),
    });
    wrapped.extend(state_ops);
    wrapped.extend(moved);
    wrapped.push(Operation {
        operator: b"Q".to_vec(),
        operands: Vec::new(),
    });

    if to_front {
        // Painted last → on top. Append at the very end.
        operations.extend(wrapped);
    } else {
        // Painted first → behind. Prepend at the very start.
        for (offset, op) in wrapped.into_iter().enumerate() {
            operations.insert(offset, op);
        }
    }
    Ok(encode_content(&operations))
}

// ─── content creation (add shapes/frames) ────────────────────────────────────

/// Format a number for a content stream (no scientific notation, trimmed).
pub(crate) fn num(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return (value as i64).to_string();
    }
    let mut text = format!("{value:.3}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

/// RGB colour, components in `0.0..=1.0`.
pub type Rgb = [f64; 3];

/// Build content-stream bytes that draw a rectangle: a **stroked** frame (table
/// cell / box border), a **filled** box, or both. Coordinates are PDF user space
/// (origin bottom-left). Wrapped in `q … Q` so it never leaks graphics state.
pub fn rectangle_ops(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    stroke: Option<Rgb>,
    fill: Option<Rgb>,
    line_width: f64,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"q\n");
    if let Some([r, g, b]) = fill {
        out.extend_from_slice(format!("{} {} {} rg\n", num(r), num(g), num(b)).as_bytes());
    }
    if let Some([r, g, b]) = stroke {
        out.extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());
    }
    out.extend_from_slice(format!("{} w\n", num(line_width)).as_bytes());
    out.extend_from_slice(
        format!("{} {} {} {} re\n", num(x), num(y), num(width), num(height)).as_bytes(),
    );
    let paint: &[u8] = match (fill.is_some(), stroke.is_some()) {
        (true, true) => b"B\n",  // fill then stroke
        (true, false) => b"f\n", // fill only
        _ => b"S\n",             // stroke only (default)
    };
    out.extend_from_slice(paint);
    out.extend_from_slice(b"Q\n");
    out
}

/// Build content-stream bytes that draw a straight line from `(x1,y1)` to
/// `(x2,y2)` — table rules, separators, underlines.
pub fn line_ops(x1: f64, y1: f64, x2: f64, y2: f64, stroke: Rgb, line_width: f64) -> Vec<u8> {
    let [r, g, b] = stroke;
    let mut out = Vec::new();
    out.extend_from_slice(b"q\n");
    out.extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());
    out.extend_from_slice(format!("{} w\n", num(line_width)).as_bytes());
    out.extend_from_slice(format!("{} {} m\n", num(x1), num(y1)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(x2), num(y2)).as_bytes());
    out.extend_from_slice(b"S\nQ\n");
    out
}

/// Build content-stream bytes that draw an open arrowhead (a stroked "V") at the
/// `(x2,y2)` end of a line whose direction is `(x1,y1) -> (x2,y2)`. Matches the
/// `/LE [/None /OpenArrow]` line-ending of a `/Line` annotation. Returns empty
/// bytes for a degenerate (zero-length) line.
pub fn arrowhead_ops(x1: f64, y1: f64, x2: f64, y2: f64, stroke: Rgb, line_width: f64) -> Vec<u8> {
    let [r, g, b] = stroke;
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt();
    let mut out = Vec::new();
    if len < 1e-6 {
        return out;
    }
    // Unit vector along the line, then its reverse (the barbs splay backwards
    // from the tip). Barb length scales with the stroke but stays visible.
    let (ux, uy) = (dx / len, dy / len);
    let (rx, ry) = (-ux, -uy);
    let head = (3.0 * line_width).max(8.0);
    let angle = 25.0_f64.to_radians();
    let (sin_a, cos_a) = angle.sin_cos();
    // Reversed unit vector rotated by +/- angle, scaled to the barb length.
    let lx = x2 + head * (rx * cos_a - ry * sin_a);
    let ly = y2 + head * (rx * sin_a + ry * cos_a);
    let r2x = x2 + head * (rx * cos_a + ry * sin_a);
    let r2y = y2 + head * (-rx * sin_a + ry * cos_a);
    out.extend_from_slice(b"q\n");
    out.extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());
    out.extend_from_slice(format!("{} w\n", num(line_width)).as_bytes());
    out.extend_from_slice(format!("{} {} m\n", num(lx), num(ly)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(x2), num(y2)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(r2x), num(r2y)).as_bytes());
    out.extend_from_slice(b"S\nQ\n");
    out
}

/// Build content-stream bytes that draw an ellipse centered at `(cx, cy)` with
/// radii `(rx, ry)`, approximated by four cubic Béziers. Stroked, filled or both.
pub fn ellipse_ops(
    cx: f64,
    cy: f64,
    rx: f64,
    ry: f64,
    stroke: Option<Rgb>,
    fill: Option<Rgb>,
    line_width: f64,
) -> Vec<u8> {
    const K: f64 = 0.552_284_749_8; // 4/3 * (sqrt(2) - 1) — circle→Bézier constant
    let (kx, ky) = (rx * K, ry * K);
    let mut out = Vec::new();
    out.extend_from_slice(b"q\n");
    if let Some([r, g, b]) = fill {
        out.extend_from_slice(format!("{} {} {} rg\n", num(r), num(g), num(b)).as_bytes());
    }
    if let Some([r, g, b]) = stroke {
        out.extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());
    }
    out.extend_from_slice(format!("{} w\n", num(line_width)).as_bytes());
    let c = |x1: f64, y1: f64, x2: f64, y2: f64, x3: f64, y3: f64| {
        format!(
            "{} {} {} {} {} {} c\n",
            num(x1),
            num(y1),
            num(x2),
            num(y2),
            num(x3),
            num(y3)
        )
    };
    out.extend_from_slice(format!("{} {} m\n", num(cx + rx), num(cy)).as_bytes());
    out.extend_from_slice(c(cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry).as_bytes());
    out.extend_from_slice(c(cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy).as_bytes());
    out.extend_from_slice(c(cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry).as_bytes());
    out.extend_from_slice(c(cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy).as_bytes());
    let paint: &[u8] = match (fill.is_some(), stroke.is_some()) {
        (true, true) => b"B\n",
        (true, false) => b"f\n",
        _ => b"S\n",
    };
    out.extend_from_slice(paint);
    out.extend_from_slice(b"Q\n");
    out
}

/// Build content-stream bytes for a polyline/polygon through `points` (PDF
/// user-space). `close` joins the last point back to the first. Stroked, filled
/// or both. Empty when fewer than two points.
pub fn polygon_ops(
    points: &[(f64, f64)],
    close: bool,
    stroke: Option<Rgb>,
    fill: Option<Rgb>,
    line_width: f64,
) -> Vec<u8> {
    let mut out = Vec::new();
    if points.len() < 2 {
        return out;
    }
    out.extend_from_slice(b"q\n");
    if let Some([r, g, b]) = fill {
        out.extend_from_slice(format!("{} {} {} rg\n", num(r), num(g), num(b)).as_bytes());
    }
    if let Some([r, g, b]) = stroke {
        out.extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());
    }
    out.extend_from_slice(format!("{} w\n", num(line_width)).as_bytes());
    let (x0, y0) = points[0];
    out.extend_from_slice(format!("{} {} m\n", num(x0), num(y0)).as_bytes());
    for &(x, y) in &points[1..] {
        out.extend_from_slice(format!("{} {} l\n", num(x), num(y)).as_bytes());
    }
    if close {
        out.extend_from_slice(b"h\n");
    }
    let paint: &[u8] = match (fill.is_some(), stroke.is_some()) {
        (true, true) => b"B\n",
        (true, false) => b"f\n",
        _ if close => b"s\n",
        _ => b"S\n",
    };
    out.extend_from_slice(paint);
    out.extend_from_slice(b"Q\n");
    out
}

/// Build content-stream bytes that draw the image XObject named `name` (already
/// registered in the page's `/Resources /XObject`) into the rectangle at
/// `(x, y)` with size `(w, h)` in PDF user space. Opacity, if any, is applied by
/// the caller wrapping this in an `/ExtGState` block.
pub fn image_ops(name: &[u8], x: f64, y: f64, w: f64, h: f64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"q\n");
    // An image XObject is drawn through the unit square: this CTM scales it to
    // (w, h) and translates it to (x, y).
    out.extend_from_slice(
        format!("{} 0 0 {} {} {} cm\n", num(w), num(h), num(x), num(y)).as_bytes(),
    );
    out.push(b'/');
    out.extend_from_slice(name);
    out.extend_from_slice(b" Do\nQ\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_reencodes_roundtrip_structure() {
        let content = b"BT /F1 12 Tf 100 700 Td (Hello) Tj ET";
        let ops = parse_content(content).unwrap();
        // BT, Tf, Td, Tj, ET
        assert_eq!(ops.len(), 5);
        assert_eq!(ops[3].operator, b"Tj");
        let runs = extract_text_runs(content).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "Hello");
    }

    #[test]
    fn replaces_text_run_text() {
        let content = b"BT /F1 12 Tf 0 0 Td (old) Tj ET";
        let edited = replace_text_run(content, 0, "new").unwrap();
        let runs = extract_text_runs(&edited).unwrap();
        assert_eq!(runs[0].text, "new");
    }

    #[test]
    fn apostrophe_runs_apply_implicit_line_move_and_are_extracted() {
        // `'` (next-line-show) carries an implicit `T*`: it must drop the
        // baseline by the current leading BEFORE showing, and it is a text-show
        // op so it is extracted. Regression: invoice text shown via `'` was
        // dropped entirely and the whole block drifted up by its leading
        // (e.g. "Abonnement Freebox Ultra" rendered on the title's line).
        let content = b"BT /F1 10 Tf 100 700 Tm (A) Tj 12 TL (B) ' (C) ' ET";
        let texts: Vec<_> = extract_elements(content)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == ElementKind::Text)
            .collect();
        assert_eq!(texts.len(), 3, "Tj + two ' runs are all extracted");
        let ys: Vec<f64> = texts.iter().map(|e| e.bounds.unwrap().y).collect();
        // Same font size, so the baseline delta equals the 12-unit leading.
        assert!((ys[0] - ys[1] - 12.0).abs() < 1e-6, "first ' moved down 12: {ys:?}");
        assert!((ys[1] - ys[2] - 12.0).abs() < 1e-6, "second ' moved down 12: {ys:?}");
    }

    #[test]
    fn quote_run_is_counted_and_editable_in_place() {
        // `aw ac (txt) "` sets spacing then next-line-shows txt. It must be
        // reachable by run index and editable in place, preserving the `"`
        // operator (and so its line-move semantics).
        let content = b"BT /F1 10 Tf 0 700 Tm (a) Tj 14 TL 5 1 (b) \" ET";
        let runs = extract_text_runs(content).unwrap();
        assert_eq!(runs.len(), 2, "Tj + \" run both counted");
        assert_eq!(runs[1].text, "b");
        let edited = replace_text_run(content, 1, "Z").unwrap();
        let runs2 = extract_text_runs(&edited).unwrap();
        assert_eq!(runs2[1].text, "Z");
        assert!(count(&edited, b"\"") >= 1, "the \" operator is preserved");
    }

    #[test]
    fn removes_text_run_but_keeps_other_ops() {
        let content = b"q 1 0 0 1 0 0 cm /Im0 Do Q BT (gone) Tj ET";
        let edited = remove_text_run(content, 0).unwrap();
        assert!(extract_text_runs(&edited).unwrap().is_empty());
        // The image draw must survive.
        assert!(
            edited.windows(2).any(|w| w == b"Do"),
            "image Do must remain"
        );
    }

    #[test]
    fn inline_image_survives_reencode() {
        let content = b"q BI /W 1 /H 1 ID \x00\xFF EI Q BT (t) Tj ET";
        let ops = parse_content(content).unwrap();
        let reencoded = encode_content(&ops);
        assert!(reencoded.windows(2).any(|w| w == b"BI"));
        assert!(reencoded.windows(2).any(|w| w == b"EI"));
    }

    fn count(data: &[u8], needle: &[u8]) -> usize {
        if data.len() < needle.len() {
            return 0;
        }
        data.windows(needle.len()).filter(|w| *w == needle).count()
    }

    #[test]
    fn groups_shapes_images_and_text() {
        // a filled rectangle (frame), an image, and a text run.
        let content = b"10 10 100 50 re f q /Im0 Do Q BT (hi) Tj ET";
        let elements = extract_elements(content).unwrap();
        let kinds: Vec<ElementKind> = elements.iter().map(|e| e.kind.clone()).collect();
        assert!(kinds.contains(&ElementKind::Path), "rectangle => Path");
        assert!(kinds.contains(&ElementKind::Image), "Do => Image");
        assert!(kinds.contains(&ElementKind::Text), "Tj => Text");
    }

    #[test]
    fn removes_a_shape_keeps_the_rest() {
        let content = b"10 10 100 50 re f BT (keep) Tj ET";
        let path_index = extract_elements(content)
            .unwrap()
            .into_iter()
            .position(|e| e.kind == ElementKind::Path)
            .unwrap();
        let edited = remove_element(content, path_index).unwrap();
        assert_eq!(count(&edited, b"re"), 0, "shape removed");
        assert!(count(&edited, b"Tj") >= 1, "text kept");
    }

    #[test]
    fn duplicates_a_shape() {
        let content = b"10 10 100 50 re f";
        let edited = duplicate_element(content, 0).unwrap();
        assert_eq!(count(&edited, b"re"), 2, "shape now appears twice");
    }

    #[test]
    fn computes_shape_bounds_and_hit_tests() {
        let content = b"10 20 100 50 re f";
        let elements = extract_elements(content).unwrap();
        let bounds = elements[0].bounds.expect("shape has bounds");
        assert!((bounds.x - 10.0).abs() < 0.01);
        assert!((bounds.y - 20.0).abs() < 0.01);
        assert!((bounds.width - 100.0).abs() < 0.01);
        assert!((bounds.height - 50.0).abs() < 0.01);
        assert!(bounds.contains(50.0, 40.0), "point inside");
        assert!(!bounds.contains(0.0, 0.0), "point outside");
    }

    #[test]
    fn computes_text_bounds_from_matrices() {
        let content = b"BT /F1 24 Tf 100 700 Td (Hello) Tj ET";
        let elements = extract_elements(content).unwrap();
        let bounds = elements[0].bounds.expect("text has bounds");
        // origin around (100, 700), some positive size
        assert!((bounds.x - 100.0).abs() < 1.0);
        assert!(bounds.width > 0.0 && bounds.height > 0.0);
    }

    #[test]
    fn moves_a_shape_via_translation() {
        let content = b"10 10 100 50 re f";
        let edited = move_element(content, 0, 5.0, -3.0).unwrap();
        assert!(count(&edited, b"cm") >= 1, "translation matrix added");
        assert!(
            count(&edited, b"q") >= 1 && count(&edited, b"Q") >= 1,
            "wrapped in q/Q"
        );
        let paths = extract_elements(&edited)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == ElementKind::Path)
            .count();
        assert_eq!(paths, 1, "still one shape after move");
    }

    #[test]
    fn strip_marked_content_removes_only_the_matching_subtype() {
        // A header block (T=h), a footer block (T=f), and untagged body content.
        let content = b"10 10 100 50 re f\n\
/GPHF <</T (h)>> BDC\nBT /F 12 Tf (HEADER) Tj ET\nEMC\n\
/GPHF <</T (f)>> BDC\nBT /F 12 Tf (FOOTER) Tj ET\nEMC\n\
BT /F 12 Tf (BODY) Tj ET";
        let no_header = strip_marked_content(content, b"h").unwrap();
        let s = String::from_utf8_lossy(&no_header);
        assert!(!s.contains("HEADER"), "header stripped");
        assert!(s.contains("FOOTER"), "footer kept");
        assert!(s.contains("BODY"), "body kept");
        assert!(s.contains("re"), "untagged path kept");

        let no_footer = strip_marked_content(content, b"f").unwrap();
        let s = String::from_utf8_lossy(&no_footer);
        assert!(s.contains("HEADER"), "header kept");
        assert!(!s.contains("FOOTER"), "footer stripped");
    }

    #[test]
    fn strip_marked_content_handles_nested_marked_content() {
        // A tagged header block that itself contains a nested (untagged) marked
        // block: the matching outer EMC must close the dropped span, not the inner.
        let content = b"/GPHF <</T (h)>> BDC\n\
(A) Tj\n/Span BDC (inner) Tj EMC\n(B) Tj\nEMC\n(C) Tj";
        let stripped = strip_marked_content(content, b"h").unwrap();
        let s = String::from_utf8_lossy(&stripped);
        assert!(!s.contains("(A)") && !s.contains("inner") && !s.contains("(B)"));
        assert!(s.contains("(C)"), "content after the block is preserved");
    }

    #[test]
    fn strip_marked_content_is_noop_without_marker() {
        let content = b"BT /F 12 Tf (plain) Tj ET";
        let out = strip_marked_content(content, b"h").unwrap();
        assert!(String::from_utf8_lossy(&out).contains("plain"));
    }

    #[test]
    fn extract_marked_content_text_reads_the_matching_subtype() {
        let content = b"10 10 100 50 re f\n\
/GPHF <</T (h)>> BDC\nq 0 0 0 rg BT /F 12 Tf 1 0 0 1 72 760 Tm (HELLO) Tj ET Q\nEMC\n\
/GPHF <</T (f)>> BDC\nBT /F 12 Tf (PAGE 1) Tj ET\nEMC\n\
BT /F 12 Tf (BODY) Tj ET";
        assert_eq!(
            extract_marked_content_text(content, b"h").as_deref(),
            Some("HELLO")
        );
        assert_eq!(
            extract_marked_content_text(content, b"f").as_deref(),
            Some("PAGE 1")
        );
    }

    #[test]
    fn extract_marked_content_text_is_none_without_marker() {
        let content = b"BT /F 12 Tf (plain) Tj ET";
        assert_eq!(extract_marked_content_text(content, b"h"), None);
        assert_eq!(extract_marked_content_text(content, b"f"), None);
    }

    #[test]
    fn extract_marked_content_text_only_takes_the_first_span() {
        // Two header spans: only the first is recovered.
        let content = b"/GPHF <</T (h)>> BDC (FIRST) Tj EMC\n\
/GPHF <</T (h)>> BDC (SECOND) Tj EMC";
        assert_eq!(
            extract_marked_content_text(content, b"h").as_deref(),
            Some("FIRST")
        );
    }

    #[test]
    fn extract_marked_content_text_handles_nested_marked_content() {
        // The text shown directly in the tagged block (plus any nested span's
        // text) is gathered; the matching outer EMC ends collection.
        let content = b"/GPHF <</T (h)>> BDC\n\
(A) Tj\n/Span BDC (B) Tj EMC\n(C) Tj\nEMC\n(D) Tj";
        assert_eq!(
            extract_marked_content_text(content, b"h").as_deref(),
            Some("ABC")
        );
    }

    #[test]
    fn transform_element_wraps_in_q_cm_q() {
        // A scale matrix should wrap the shape in `q a b c d e f cm … Q` without
        // changing the structure (still one path) or its `re` op.
        let content = b"10 10 100 50 re f";
        let edited = transform_element(content, 0, [2.0, 0.0, 0.0, 2.0, 0.0, 0.0]).unwrap();
        assert_eq!(count(&edited, b"cm"), 1, "transform matrix added");
        assert!(
            count(&edited, b"q") >= 1 && count(&edited, b"Q") >= 1,
            "wrapped in q/Q"
        );
        assert_eq!(count(&edited, b"re"), 1, "the original re op is preserved");
        let paths = extract_elements(&edited)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == ElementKind::Path)
            .count();
        assert_eq!(paths, 1, "still one shape after transform");
    }

    #[test]
    fn transform_element_identity_roundtrips_structurally() {
        // The identity matrix changes nothing visually; structurally it adds the
        // q/cm/Q wrapper but re-parsing still yields the same kinds of elements.
        let content = b"BT /F1 12 Tf 100 700 Td (Hi) Tj ET";
        let edited = transform_element(content, 0, [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]).unwrap();
        let runs = extract_text_runs(&edited).unwrap();
        assert_eq!(runs.len(), 1, "still one text run");
        assert_eq!(runs[0].text, "Hi", "text unchanged");
        assert_eq!(count(&edited, b"cm"), 1);
        assert!(count(&edited, b"q") >= 1 && count(&edited, b"Q") >= 1);
    }

    #[test]
    fn set_path_style_injects_new_fill_before_paint() {
        // Recolour a black rectangle red: a `1 0 0 rg` must be injected inside the
        // q/Q wrapper, the original `re`/`f` ops preserved, and the path still
        // reports the new fill colour when re-extracted.
        let content = b"10 10 100 50 re f";
        let style = PathStyle {
            fill: Some([1.0, 0.0, 0.0]),
            ..PathStyle::default()
        };
        let edited = set_path_style(content, 0, &style, None).unwrap();
        assert!(count(&edited, b"rg") >= 1, "fill colour op injected");
        assert_eq!(count(&edited, b"re"), 1, "original construction preserved");
        assert!(count(&edited, b"f") >= 1, "original paint preserved");
        assert!(count(&edited, b"q") >= 1 && count(&edited, b"Q") >= 1, "wrapped");
        let paths = vector::vector_paths_from_ops(
            &parse_content(&edited).unwrap(),
            &std::collections::BTreeMap::new(),
            &vector::NoNamedColors,
        );
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].fill, Some([1.0, 0.0, 0.0]), "fill is now red");
    }

    #[test]
    fn set_path_style_sets_stroke_width_and_dash() {
        // Stroke a line: override stroke colour, width and dash. The injected ops
        // (`RG`, `w`, `d`) appear and the path reports the new style.
        let content = b"10 10 m 110 10 l S";
        let style = PathStyle {
            stroke: Some([0.0, 0.0, 1.0]),
            stroke_width: Some(3.0),
            dash: Some(vec![4.0, 2.0]),
            ..PathStyle::default()
        };
        let edited = set_path_style(content, 0, &style, None).unwrap();
        assert!(count(&edited, b"RG") >= 1, "stroke colour injected");
        assert!(count(&edited, b" w") >= 1 || count(&edited, b"3 w") >= 1, "width injected");
        assert!(count(&edited, b" d") >= 1, "dash injected");
        let paths = vector::vector_paths_from_ops(
            &parse_content(&edited).unwrap(),
            &std::collections::BTreeMap::new(),
            &vector::NoNamedColors,
        );
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].stroke, Some([0.0, 0.0, 1.0]));
        assert!((paths[0].stroke_width - 3.0).abs() < 1e-9);
        assert_eq!(paths[0].dash, vec![4.0, 2.0]);
    }

    #[test]
    fn set_path_style_on_non_path_index_errors() {
        // A text run is not a path → set_path_style must return Err.
        let content = b"BT /F1 12 Tf 0 0 Td (txt) Tj ET";
        let text_index = extract_elements(content)
            .unwrap()
            .into_iter()
            .position(|e| e.kind == ElementKind::Text)
            .unwrap();
        let result = set_path_style(content, text_index, &PathStyle::default(), None);
        assert!(result.is_err(), "styling a text element must fail");
    }

    #[test]
    fn set_path_style_emits_gs_for_alpha_when_named() {
        // With a registered gstate name and an alpha, a `/<name> gs` is injected
        // first inside the q/Q wrap, before the colour override.
        let content = b"10 10 100 50 re f";
        let style = PathStyle {
            fill: Some([1.0, 0.0, 0.0]),
            fill_alpha: Some(0.5),
            ..PathStyle::default()
        };
        let edited = set_path_style(content, 0, &style, Some(b"GpGs0")).unwrap();
        let s = String::from_utf8_lossy(&edited);
        assert!(s.contains("/GpGs0 gs"), "gs op for the named gstate injected");
        // gs must precede the fill colour op (declared-order injection).
        let gs_at = s.find("/GpGs0 gs").unwrap();
        let rg_at = s.find(" rg").or_else(|| s.find("rg")).unwrap();
        assert!(gs_at < rg_at, "gs comes before the colour op");
        assert!(count(&edited, b"re") == 1 && count(&edited, b"f") >= 1, "path preserved");
    }

    #[test]
    fn set_path_style_skips_gs_without_alpha() {
        // A name is supplied but no alpha was requested → no gs op.
        let content = b"10 10 100 50 re f";
        let style = PathStyle {
            fill: Some([1.0, 0.0, 0.0]),
            ..PathStyle::default()
        };
        let edited = set_path_style(content, 0, &style, Some(b"GpGs0")).unwrap();
        assert!(!String::from_utf8_lossy(&edited).contains("gs"), "no gs without alpha");
    }

    #[test]
    fn set_element_opacity_wraps_in_q_gs_q() {
        // An image element gets `q /<gs> gs … Do … Q`; the Do op is preserved and
        // the element still resolves after the edit.
        let content = b"q /Im0 Do Q BT (hi) Tj ET";
        let img_index = extract_elements(content)
            .unwrap()
            .into_iter()
            .position(|e| e.kind == ElementKind::Image)
            .unwrap();
        let edited = set_element_opacity(content, img_index, b"GpGs0").unwrap();
        let s = String::from_utf8_lossy(&edited);
        assert!(s.contains("/GpGs0 gs"), "gs op injected");
        assert!(s.contains("Do"), "the image draw is preserved");
        // The image element still exists when re-extracted.
        let imgs = extract_elements(&edited)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == ElementKind::Image)
            .count();
        assert_eq!(imgs, 1, "still one image after opacity wrap");
    }

    #[test]
    fn set_element_opacity_invalid_index_errors() {
        let content = b"10 10 100 50 re f";
        assert!(set_element_opacity(content, 99, b"GpGs0").is_err());
    }

    #[test]
    fn reorder_element_to_front_moves_range_to_end() {
        // Two shapes then a text run. Bring the FIRST shape to front: its `re`
        // must now appear after the text's `Tj` in the stream, wrapped in q/Q,
        // and re-extraction still yields the same kinds of elements.
        let content = b"10 10 20 20 re f 50 50 20 20 re f BT (T) Tj ET";
        let edited = reorder_element(content, 0, true).unwrap();
        let s = String::from_utf8_lossy(&edited);
        // The first shape's geometry now trails the text show.
        let tj_at = s.find("(T) Tj").unwrap();
        let first_re_at = s.find("10 10 20 20 re").unwrap();
        assert!(first_re_at > tj_at, "brought-to-front shape now painted last");
        assert!(count(&edited, b"q") >= 1 && count(&edited, b"Q") >= 1, "re-wrapped");
        // Stream still parses and the element set is unchanged in composition.
        let els = extract_elements(&edited).unwrap();
        assert_eq!(
            els.iter().filter(|e| e.kind == ElementKind::Path).count(),
            2,
            "still two shapes"
        );
        assert_eq!(
            els.iter().filter(|e| e.kind == ElementKind::Text).count(),
            1,
            "still one text run"
        );
    }

    #[test]
    fn reorder_element_to_back_moves_range_to_start() {
        // Bring the text run (last element) to the back: its `Tj` must precede the
        // first shape's `re`.
        let content = b"10 10 20 20 re f 50 50 20 20 re f BT (T) Tj ET";
        let text_index = extract_elements(content)
            .unwrap()
            .into_iter()
            .position(|e| e.kind == ElementKind::Text)
            .unwrap();
        let edited = reorder_element(content, text_index, false).unwrap();
        let s = String::from_utf8_lossy(&edited);
        let tj_at = s.find("(T) Tj").unwrap();
        let first_re_at = s.find("10 10 20 20 re").unwrap();
        assert!(tj_at < first_re_at, "sent-to-back text now painted first");
        // The text run is still resolvable and reads the same.
        let runs = extract_text_runs(&edited).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "T");
    }

    #[test]
    fn reorder_element_invalid_index_errors() {
        let content = b"10 10 100 50 re f";
        assert!(reorder_element(content, 7, true).is_err());
    }

    #[test]
    fn reorder_element_to_front_preserves_fill_colour() {
        // A red shape whose fill is set by a PRECEDING `rg` (outside its op range).
        // After bringing it to the front it must STILL report the red fill — the
        // captured `rg` is re-emitted inside the new `q … Q` wrap.
        let content = b"1 0 0 rg 10 10 20 20 re f 0 0 1 rg 50 50 20 20 re f";
        let no_color = std::collections::BTreeMap::new();
        // Sanity: before the move the first path is red.
        let before = vector::vector_paths_from_ops(&parse_content(content).unwrap(), &no_color, &vector::NoNamedColors);
        assert_eq!(before[0].fill, Some([1.0, 0.0, 0.0]), "first path starts red");

        let edited = reorder_element(content, 0, true).unwrap();
        let paths = vector::vector_paths_from_ops(&parse_content(&edited).unwrap(), &no_color, &vector::NoNamedColors);
        assert_eq!(paths.len(), 2, "still two painted paths");
        // The moved (red) shape is now painted last → last in the path list.
        assert_eq!(
            paths.last().unwrap().fill,
            Some([1.0, 0.0, 0.0]),
            "reordered shape keeps its red fill (not black)"
        );
        // The other (blue) shape is undisturbed.
        assert_eq!(
            paths[0].fill,
            Some([0.0, 0.0, 1.0]),
            "neighbour shape's blue fill is not corrupted"
        );
    }

    #[test]
    fn reorder_element_to_back_preserves_stroke_colour_width_and_dash() {
        // A stroked line whose stroke colour, width and dash are set by PRECEDING
        // ops. Send it to the back: it must keep blue / width 3 / dash [4,2].
        let content = b"0 0 1 RG 3 w [4 2] 0 d 10 10 m 110 10 l S 1 0 0 RG 0 80 m 100 80 l S";
        let no_color = std::collections::BTreeMap::new();

        // Path index 1 is the dashed blue line (declared first → drawn first).
        let before = vector::vector_paths_from_ops(&parse_content(content).unwrap(), &no_color, &vector::NoNamedColors);
        assert_eq!(before[0].stroke, Some([0.0, 0.0, 1.0]), "first line starts blue");

        let line_index = extract_elements(content)
            .unwrap()
            .into_iter()
            .position(|e| e.kind == ElementKind::Path)
            .unwrap();
        let edited = reorder_element(content, line_index, false).unwrap();
        let paths = vector::vector_paths_from_ops(&parse_content(&edited).unwrap(), &no_color, &vector::NoNamedColors);
        assert_eq!(paths.len(), 2, "still two painted paths");
        // Sent to back → painted first → first in the path list.
        let moved = &paths[0];
        assert_eq!(moved.stroke, Some([0.0, 0.0, 1.0]), "stroke stays blue");
        assert!((moved.stroke_width - 3.0).abs() < 1e-9, "stroke width preserved");
        assert_eq!(moved.dash, vec![4.0, 2.0], "dash preserved");
        // The red neighbour keeps its own stroke colour (not leaked to blue).
        assert_eq!(paths[1].stroke, Some([1.0, 0.0, 0.0]), "neighbour stays red");
    }

    #[test]
    fn reorder_element_does_not_corrupt_second_element_state() {
        // Two red shapes, each relying on a single leading `rg`. Reordering the
        // first must not alter the appearance of the second.
        let content = b"1 0 0 rg 10 10 20 20 re f 50 50 20 20 re f";
        let no_color = std::collections::BTreeMap::new();
        let edited = reorder_element(content, 0, true).unwrap();
        let paths = vector::vector_paths_from_ops(&parse_content(&edited).unwrap(), &no_color, &vector::NoNamedColors);
        assert_eq!(paths.len(), 2);
        // Both shapes must still be red.
        for p in &paths {
            assert_eq!(p.fill, Some([1.0, 0.0, 0.0]), "both shapes stay red");
        }
    }

    #[test]
    fn reorder_element_text_preserves_font_and_fill_colour() {
        // A green text run whose font and colour are set by PRECEDING ops. Sending
        // it to the back must keep the green fill colour on the run.
        let content = b"0 1 0 rg BT /F1 12 Tf 0 0 Td (T) Tj ET 10 10 20 20 re f";
        let text_index = extract_elements(content)
            .unwrap()
            .into_iter()
            .position(|e| e.kind == ElementKind::Text)
            .unwrap();
        let edited = reorder_element(content, text_index, false).unwrap();
        let s = String::from_utf8_lossy(&edited);
        // The captured fill colour and font are re-emitted before the moved run.
        assert!(s.contains("0 1 0 rg"), "fill colour re-emitted for the text");
        assert!(s.contains("/F1 12 Tf"), "font re-emitted for the text");
        // The element's colour, as read back, is green.
        let text = extract_elements(&edited)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == ElementKind::Text)
            .unwrap();
        assert_eq!(text.color, Some([0.0, 1.0, 0.0]), "text keeps its green fill");
        assert_eq!(text.font.as_deref(), Some("F1"), "text keeps its font");
    }

    #[test]
    fn reorder_element_skips_state_popped_before_boundary() {
        // A `rg` inside a `q … Q` that closes BEFORE the element must NOT be
        // captured (it is out of scope at the element's first op). The element
        // relies on no preceding colour → default black, no stray `rg` injected.
        let content = b"q 1 0 0 rg 0 0 5 5 re f Q 10 10 20 20 re f";
        let no_color = std::collections::BTreeMap::new();
        // Element index 1 is the second (black) rectangle.
        let edited = reorder_element(content, 1, true).unwrap();
        let s = String::from_utf8_lossy(&edited);
        // Exactly one `rg` (the original scoped one) — none injected for the move.
        assert_eq!(count(&edited, b"rg"), 1, "popped fill colour is not re-captured");
        let paths = vector::vector_paths_from_ops(&parse_content(&edited).unwrap(), &no_color, &vector::NoNamedColors);
        // The moved shape (last painted) is default black.
        assert_eq!(paths.last().unwrap().fill, Some([0.0, 0.0, 0.0]), "stays black");
        let _ = s;
    }
}
