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

use std::collections::BTreeMap;

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
    /// `0` for upright text). `None` for image/path.
    pub rotation_deg: Option<f64>,
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
    operator == b"Tj" || operator == b"TJ"
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

/// List the text runs in a decoded content stream, decoding each with the
/// active font's [`TextDecoder`] (selected by the `Tf` operator). Fonts not in
/// `fonts` — and the state before any `Tf` — fall back to WinAnsi.
pub fn extract_text_runs_with(content: &[u8], fonts: &FontDecoders) -> Result<Vec<TextRun>> {
    let operations = parse_content(content)?;
    let mut runs = Vec::new();
    let fallback = TextDecoder::winansi();
    let mut current: &TextDecoder = &fallback;
    for (op_position, operation) in operations.iter().enumerate() {
        if operation.operator == b"Tf" {
            if let Some(Object::Name(name)) = operation.operands.first() {
                current = fonts.get(name).unwrap_or(&fallback);
            }
        } else if is_text_show(&operation.operator) {
            runs.push(TextRun {
                index: runs.len(),
                operator: operation.operator.clone(),
                text: decode_operand_text(&operation.operands, current),
                op_position,
            });
        }
    }
    Ok(runs)
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
    if operation.operator == b"Tj" {
        operation.operands = vec![operand];
    } else {
        // TJ: collapse the positioned array to a single string.
        operation.operands = vec![Object::Array(vec![operand])];
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
fn elements_from_ops(operations: &[Operation], fonts: &FontDecoders) -> Vec<ContentElement> {
    let mut elements = Vec::new();

    // Graphics state.
    let mut ctm = Matrix::IDENTITY;
    let mut ctm_stack: Vec<Matrix> = Vec::new();
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

    for (i, op) in operations.iter().enumerate() {
        let operator = op.operator.as_slice();
        match operator {
            b"q" => ctm_stack.push(ctm),
            b"Q" => {
                if let Some(m) = ctm_stack.pop() {
                    ctm = m;
                }
            }
            b"cm" => {
                let n = nums(op);
                if n.len() == 6 {
                    ctm = Matrix::new(n[0], n[1], n[2], n[3], n[4], n[5]).then(&ctm);
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
                });
                tm = Matrix::translate(width, 0.0).then(&tm);
            }
            b"Do" => {
                let label = op
                    .operands
                    .iter()
                    .find_map(|o| o.as_name())
                    .map(|n| String::from_utf8_lossy(n).into_owned())
                    .unwrap_or_default();
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
                    rotation_deg: None,
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
/// decoding text labels with the page's fonts (WinAnsi + `/ToUnicode`).
pub fn extract_elements_with(content: &[u8], fonts: &FontDecoders) -> Result<Vec<ContentElement>> {
    let operations = parse_content(content)?;
    Ok(elements_from_ops(&operations, fonts))
}

/// List all addressable elements (text, images, shapes) of a content stream.
pub fn extract_elements(content: &[u8]) -> Result<Vec<ContentElement>> {
    extract_elements_with(content, &FontDecoders::new())
}

/// Remove the element at `index` (a text, image, or whole shape), preserving
/// everything else verbatim.
pub fn remove_element(content: &[u8], index: usize) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let element = elements_from_ops(&operations, &FontDecoders::new())
        .into_iter()
        .nth(index)
        .ok_or_else(|| EngineError::Missing(format!("content element #{index}")))?;
    operations.drain(element.op_start..=element.op_end);
    Ok(encode_content(&operations))
}

/// Duplicate the element at `index`, inserting the copy right after it (it lands
/// at the same position, ready to be moved). Works for text, images and shapes.
pub fn duplicate_element(content: &[u8], index: usize) -> Result<Vec<u8>> {
    let mut operations = parse_content(content)?;
    let element = elements_from_ops(&operations, &FontDecoders::new())
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
    let element = elements_from_ops(&operations, &FontDecoders::new())
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
}
