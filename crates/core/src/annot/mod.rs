//! PDF annotations (ISO 32000-1 §12.5): list, create (with appearance streams)
//! and remove. Each created annotation gets an appearance stream (`/AP /N`) so
//! it renders consistently in every viewer.

use crate::content::{self, num};
use crate::object::{Dictionary, Object, StringKind};

/// An annotation as read from a page's `/Annots`. Carries the common markup
/// metadata (author, dates, colour, opacity) plus the type-specific geometry a
/// host editor needs (quad points, ink paths, stamp name, link target) — the
/// native equivalent of a reader's annotation layer.
#[derive(Debug, Clone, Default)]
pub struct Annotation {
    /// 0-based index in the page `/Annots` array.
    pub index: usize,
    /// `/Subtype` (e.g. "Square", "Highlight", "Line", "FreeText").
    pub subtype: String,
    /// `/Rect` `[x0 y0 x1 y1]`.
    pub rect: [f64; 4],
    /// `/Contents` text, if any.
    pub contents: String,
    /// `/T` — the annotation author / title. Empty when absent.
    pub author: String,
    /// `/Subj` — the annotation subject. Empty when absent.
    pub subject: String,
    /// `/CreationDate` — raw PDF date string (e.g. `D:20260616120000Z`). Empty
    /// when absent; the host parses it.
    pub created: String,
    /// `/M` — raw PDF modification date string. Empty when absent.
    pub modified: String,
    /// `/C` normalised to RGB in `0.0..=1.0` (gray → replicated, CMYK → naive).
    /// Empty when `/C` is absent or `[]` (no colour).
    pub color: Vec<f64>,
    /// `/CA` non-stroking opacity in `0.0..=1.0` (`1.0` = opaque; the default
    /// when `/CA` is absent).
    pub opacity: f64,
    /// `/QuadPoints` (8 values per quad) for text-markup annotations
    /// (highlight/underline/strikeout/squiggly). PDF user space (bottom-left).
    pub quad_points: Vec<f64>,
    /// `/InkList` — one inner `Vec` per freehand stroke, `x y x y …` in PDF
    /// user space. Empty for non-ink annotations.
    pub ink_list: Vec<Vec<f64>>,
    /// `/Name` — the stamp name (e.g. "Approved", "Draft") for Stamp
    /// annotations. Empty when absent.
    pub name: String,
    /// For Link annotations: the external URI (`/A /URI`). Empty when the link
    /// targets an internal page or is absent.
    pub link_uri: String,
    /// For Link annotations: the 1-based internal destination page (`/Dest` or
    /// `/A /GoTo /D`). `0` when the link is external or has no resolvable page.
    pub link_page: u32,
}

pub(crate) fn name(bytes: &[u8]) -> Object {
    Object::Name(bytes.to_vec())
}

pub(crate) fn real_array(values: &[f64]) -> Object {
    Object::Array(values.iter().map(|&v| Object::Real(v)).collect())
}

fn border_style(width: f64) -> Object {
    let mut bs = Dictionary::new();
    bs.set(b"Type".to_vec(), name(b"Border"));
    bs.set(b"W".to_vec(), Object::Real(width));
    Object::Dictionary(bs)
}

fn literal(text: &str) -> Object {
    Object::String(text.as_bytes().to_vec(), StringKind::Literal)
}

/// A built annotation: its dictionary (without `/AP`), the appearance stream
/// content, and the resources the appearance needs.
pub(crate) struct Built {
    pub dict: Dictionary,
    pub appearance: Vec<u8>,
    pub resources: Dictionary,
}

/// Square (rectangle) annotation.
pub(crate) fn square(
    rect: [f64; 4],
    stroke: Option<[f64; 3]>,
    fill: Option<[f64; 3]>,
    line_width: f64,
) -> Built {
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Square"));
    dict.set(b"Rect".to_vec(), real_array(&rect));
    if let Some(c) = stroke {
        dict.set(b"C".to_vec(), real_array(&c));
    }
    if let Some(c) = fill {
        dict.set(b"IC".to_vec(), real_array(&c));
    }
    dict.set(b"BS".to_vec(), border_style(line_width));

    let [x0, y0, x1, y1] = rect;
    let inset = line_width / 2.0;
    let appearance = content::rectangle_ops(
        x0 + inset,
        y0 + inset,
        (x1 - x0) - line_width,
        (y1 - y0) - line_width,
        stroke,
        fill,
        line_width,
    );
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// Circle (ellipse) annotation — the ellipse inscribed in `rect`.
pub(crate) fn circle(
    rect: [f64; 4],
    stroke: Option<[f64; 3]>,
    fill: Option<[f64; 3]>,
    line_width: f64,
) -> Built {
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Circle"));
    dict.set(b"Rect".to_vec(), real_array(&rect));
    if let Some(c) = stroke {
        dict.set(b"C".to_vec(), real_array(&c));
    }
    if let Some(c) = fill {
        dict.set(b"IC".to_vec(), real_array(&c));
    }
    dict.set(b"BS".to_vec(), border_style(line_width));

    let [x0, y0, x1, y1] = rect;
    let inset = line_width / 2.0;
    let cx = (x0 + x1) / 2.0;
    let cy = (y0 + y1) / 2.0;
    let rx = ((x1 - x0).abs() / 2.0 - inset).max(0.0);
    let ry = ((y1 - y0).abs() / 2.0 - inset).max(0.0);
    let appearance = content::ellipse_ops(cx, cy, rx, ry, stroke, fill, line_width);
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// The axis-aligned bounding box of `vertices` padded by `margin`.
fn vertices_rect(vertices: &[(f64, f64)], margin: f64) -> [f64; 4] {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for &(x, y) in vertices {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    if !min_x.is_finite() {
        return [0.0, 0.0, 0.0, 0.0];
    }
    [
        min_x - margin,
        min_y - margin,
        max_x + margin,
        max_y + margin,
    ]
}

/// `/Vertices` flat array `[x0 y0 x1 y1 …]` for Polygon/PolyLine.
fn vertices_array(vertices: &[(f64, f64)]) -> Object {
    Object::Array(
        vertices
            .iter()
            .flat_map(|&(x, y)| [Object::Real(x), Object::Real(y)])
            .collect(),
    )
}

/// Polygon annotation — a closed shape through `vertices` (ISO 32000-1 §12.5.6.9).
pub(crate) fn polygon(
    vertices: &[(f64, f64)],
    stroke: Option<[f64; 3]>,
    fill: Option<[f64; 3]>,
    line_width: f64,
) -> Built {
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Polygon"));
    dict.set(
        b"Rect".to_vec(),
        real_array(&vertices_rect(vertices, line_width.max(1.0))),
    );
    dict.set(b"Vertices".to_vec(), vertices_array(vertices));
    if let Some(c) = stroke {
        dict.set(b"C".to_vec(), real_array(&c));
    }
    if let Some(c) = fill {
        dict.set(b"IC".to_vec(), real_array(&c));
    }
    dict.set(b"BS".to_vec(), border_style(line_width));
    let appearance = content::polygon_ops(vertices, true, stroke, fill, line_width);
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// PolyLine annotation — an open path through `vertices` (ISO 32000-1 §12.5.6.9).
pub(crate) fn polyline(vertices: &[(f64, f64)], color: [f64; 3], line_width: f64) -> Built {
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"PolyLine"));
    dict.set(
        b"Rect".to_vec(),
        real_array(&vertices_rect(vertices, line_width.max(1.0))),
    );
    dict.set(b"Vertices".to_vec(), vertices_array(vertices));
    dict.set(b"C".to_vec(), real_array(&color));
    dict.set(b"BS".to_vec(), border_style(line_width));
    let appearance = content::polygon_ops(vertices, false, Some(color), None, line_width);
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// Caret annotation — a small filled upward wedge marking an insertion point
/// (ISO 32000-1 §12.5.6.11).
pub(crate) fn caret(rect: [f64; 4], color: [f64; 3]) -> Built {
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Caret"));
    dict.set(b"Rect".to_vec(), real_array(&rect));
    dict.set(b"C".to_vec(), real_array(&color));

    let [x0, y0, x1, y1] = rect;
    // An upward triangle filling the rect: base along the bottom, apex centred top.
    let apex = ((x0 + x1) / 2.0, y1);
    let tri = [(x0, y0), (x1, y0), apex];
    let appearance = content::polygon_ops(&tri, true, None, Some(color), 0.0);
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// Highlight annotation — a translucent colour fill over the rectangle.
pub(crate) fn highlight(rect: [f64; 4], color: [f64; 3]) -> Built {
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Highlight"));
    dict.set(b"Rect".to_vec(), real_array(&rect));
    dict.set(b"C".to_vec(), real_array(&color));
    dict.set(b"CA".to_vec(), Object::Real(0.4));
    let [x0, y0, x1, y1] = rect;
    dict.set(
        b"QuadPoints".to_vec(),
        real_array(&[x0, y1, x1, y1, x0, y0, x1, y0]),
    );
    let appearance = content::rectangle_ops(x0, y0, x1 - x0, y1 - y0, None, Some(color), 0.0);
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// Line annotation. When `end_arrow` is set, an open arrowhead is drawn at the
/// `(x2,y2)` end and `/LE [/None /OpenArrow]` is recorded so conforming readers
/// render the same ending even if they regenerate the appearance.
pub(crate) fn line(
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    color: [f64; 3],
    line_width: f64,
    end_arrow: bool,
) -> Built {
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Line"));
    // Pad the bounding box so the arrowhead is never clipped by /Rect.
    let pad = if end_arrow {
        (3.0 * line_width).max(8.0)
    } else {
        0.0
    };
    let rect = [
        x1.min(x2) - pad,
        y1.min(y2) - pad,
        x1.max(x2) + pad,
        y1.max(y2) + pad,
    ];
    dict.set(b"Rect".to_vec(), real_array(&rect));
    dict.set(b"L".to_vec(), real_array(&[x1, y1, x2, y2]));
    dict.set(b"C".to_vec(), real_array(&color));
    dict.set(b"BS".to_vec(), border_style(line_width));
    let mut appearance = content::line_ops(x1, y1, x2, y2, color, line_width);
    if end_arrow {
        dict.set(
            b"LE".to_vec(),
            Object::Array(vec![name(b"None"), name(b"OpenArrow")]),
        );
        appearance.extend_from_slice(&content::arrowhead_ops(x1, y1, x2, y2, color, line_width));
    }
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// Free-text annotation (a text box drawn directly on the page).
pub(crate) fn free_text(rect: [f64; 4], text: &str, font_size: f64, color: [f64; 3]) -> Built {
    let [r, g, b] = color;
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"FreeText"));
    dict.set(b"Rect".to_vec(), real_array(&rect));
    dict.set(b"Contents".to_vec(), literal(text));
    dict.set(
        b"DA".to_vec(),
        literal(&format!(
            "/Helv {} Tf {} {} {} rg",
            num(font_size),
            num(r),
            num(g),
            num(b)
        )),
    );

    // Appearance: draw the text near the top-left of the rect.
    let [x0, _y0, _x1, y1] = rect;
    let mut appearance = Vec::new();
    appearance.extend_from_slice(b"BT\n");
    appearance.extend_from_slice(format!("/Helv {} Tf\n", num(font_size)).as_bytes());
    appearance.extend_from_slice(format!("{} {} {} rg\n", num(r), num(g), num(b)).as_bytes());
    appearance
        .extend_from_slice(format!("{} {} Td\n", num(x0 + 2.0), num(y1 - font_size)).as_bytes());
    appearance.push(b'(');
    for &byte in &crate::font::encode_winansi(text) {
        if matches!(byte, b'(' | b')' | b'\\') {
            appearance.push(b'\\');
        }
        appearance.push(byte);
    }
    appearance.extend_from_slice(b") Tj\nET\n");

    // Resources: a non-embedded Helvetica named /Helv.
    let mut helv = Dictionary::new();
    helv.set(b"Type".to_vec(), name(b"Font"));
    helv.set(b"Subtype".to_vec(), name(b"Type1"));
    helv.set(b"BaseFont".to_vec(), name(b"Helvetica"));
    let mut fonts = Dictionary::new();
    fonts.set(b"Helv".to_vec(), Object::Dictionary(helv));
    let mut resources = Dictionary::new();
    resources.set(b"Font".to_vec(), Object::Dictionary(fonts));

    Built {
        dict,
        appearance,
        resources,
    }
}

/// Text-markup quad covering a rectangle (upper-left, upper-right, lower-left,
/// lower-right — the order ISO 32000-1 §12.5.6.10 expects).
fn markup_quad(rect: [f64; 4]) -> Object {
    let [x0, y0, x1, y1] = rect;
    real_array(&[x0, y1, x1, y1, x0, y0, x1, y0])
}

/// Underline annotation — a line along the bottom of the text rectangle.
pub(crate) fn underline(rect: [f64; 4], color: [f64; 3]) -> Built {
    let [x0, y0, _x1, y1] = rect;
    let x1 = rect[2];
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Underline"));
    dict.set(b"Rect".to_vec(), real_array(&rect));
    dict.set(b"C".to_vec(), real_array(&color));
    dict.set(b"QuadPoints".to_vec(), markup_quad(rect));
    let width = ((y1 - y0) * 0.06).max(0.75);
    let y = y0 + (y1 - y0) * 0.08;
    let appearance = content::line_ops(x0, y, x1, y, color, width);
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// Strike-out annotation — a line through the middle of the text rectangle.
pub(crate) fn strike_out(rect: [f64; 4], color: [f64; 3]) -> Built {
    let [x0, y0, x1, y1] = rect;
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"StrikeOut"));
    dict.set(b"Rect".to_vec(), real_array(&rect));
    dict.set(b"C".to_vec(), real_array(&color));
    dict.set(b"QuadPoints".to_vec(), markup_quad(rect));
    let width = ((y1 - y0) * 0.06).max(0.75);
    let y = (y0 + y1) / 2.0;
    let appearance = content::line_ops(x0, y, x1, y, color, width);
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// Ink (freehand) annotation from one or more polylines.
pub(crate) fn ink(paths: &[Vec<(f64, f64)>], color: [f64; 3], line_width: f64) -> Built {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for path in paths {
        for &(x, y) in path {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if !min_x.is_finite() {
        min_x = 0.0;
        min_y = 0.0;
        max_x = 0.0;
        max_y = 0.0;
    }
    let margin = line_width.max(1.0);
    let rect = [
        min_x - margin,
        min_y - margin,
        max_x + margin,
        max_y + margin,
    ];

    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Ink"));
    dict.set(b"Rect".to_vec(), real_array(&rect));
    dict.set(b"C".to_vec(), real_array(&color));
    dict.set(b"BS".to_vec(), border_style(line_width));
    let ink_list = Object::Array(
        paths
            .iter()
            .map(|path| {
                Object::Array(
                    path.iter()
                        .flat_map(|&(x, y)| [Object::Real(x), Object::Real(y)])
                        .collect(),
                )
            })
            .collect(),
    );
    dict.set(b"InkList".to_vec(), ink_list);

    let [r, g, b] = color;
    let mut appearance = Vec::new();
    appearance.extend_from_slice(b"q\n");
    appearance.extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());
    appearance.extend_from_slice(format!("{} w\n", num(line_width)).as_bytes());
    appearance.extend_from_slice(b"1 J\n1 j\n"); // round caps and joins
    for path in paths {
        let mut points = path.iter();
        if let Some(&(x, y)) = points.next() {
            appearance.extend_from_slice(format!("{} {} m\n", num(x), num(y)).as_bytes());
            for &(x, y) in points {
                appearance.extend_from_slice(format!("{} {} l\n", num(x), num(y)).as_bytes());
            }
            appearance.extend_from_slice(b"S\n");
        }
    }
    appearance.extend_from_slice(b"Q\n");
    Built {
        dict,
        appearance,
        resources: Dictionary::new(),
    }
}

/// Rubber-stamp annotation — a labelled, bordered box.
pub(crate) fn stamp(rect: [f64; 4], label: &str, color: [f64; 3]) -> Built {
    let [x0, y0, x1, y1] = rect;
    let [r, g, b] = color;
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Stamp"));
    dict.set(b"Rect".to_vec(), real_array(&rect));
    dict.set(b"Name".to_vec(), name(b"Draft"));
    dict.set(b"C".to_vec(), real_array(&color));

    let width = x1 - x0;
    let height = y1 - y0;
    let line_width = (height * 0.06).clamp(1.0, 3.0);
    let font_size = (height * 0.5).clamp(8.0, 24.0);
    let mut appearance = content::rectangle_ops(
        x0 + line_width,
        y0 + line_width,
        width - 2.0 * line_width,
        height - 2.0 * line_width,
        Some(color),
        None,
        line_width,
    );
    let text_width = label.chars().count() as f64 * font_size * 0.5;
    let tx = x0 + ((width - text_width) / 2.0).max(line_width + 2.0);
    let ty = y0 + (height - font_size) / 2.0 + font_size * 0.2;
    appearance.extend_from_slice(b"BT\n");
    appearance.extend_from_slice(format!("/Helv {} Tf\n", num(font_size)).as_bytes());
    appearance.extend_from_slice(format!("{} {} {} rg\n", num(r), num(g), num(b)).as_bytes());
    appearance.extend_from_slice(format!("{} {} Td\n", num(tx), num(ty)).as_bytes());
    appearance.push(b'(');
    for &byte in &crate::font::encode_winansi(label) {
        if matches!(byte, b'(' | b')' | b'\\') {
            appearance.push(b'\\');
        }
        appearance.push(byte);
    }
    appearance.extend_from_slice(b") Tj\nET\n");

    let mut helv = Dictionary::new();
    helv.set(b"Type".to_vec(), name(b"Font"));
    helv.set(b"Subtype".to_vec(), name(b"Type1"));
    helv.set(b"BaseFont".to_vec(), name(b"Helvetica"));
    let mut fonts = Dictionary::new();
    fonts.set(b"Helv".to_vec(), Object::Dictionary(helv));
    let mut resources = Dictionary::new();
    resources.set(b"Font".to_vec(), Object::Dictionary(fonts));
    Built {
        dict,
        appearance,
        resources,
    }
}

// ─── appearance regeneration ─────────────────────────────────────────────────

/// Read a colour array (`/C`, `/IC`) as RGB: 1 value = gray, 3 = RGB, 4 = CMYK
/// (naive conversion). `None` when absent or not a number array.
fn read_rgb(dict: &Dictionary, key: &[u8]) -> Option<[f64; 3]> {
    let nums: Vec<f64> = dict
        .get(key)
        .and_then(Object::as_array)?
        .iter()
        .filter_map(Object::as_f64)
        .collect();
    match nums.len() {
        1 => Some([nums[0], nums[0], nums[0]]),
        3 => Some([nums[0], nums[1], nums[2]]),
        4 => {
            let k = nums[3];
            Some([
                (1.0 - nums[0]) * (1.0 - k),
                (1.0 - nums[1]) * (1.0 - k),
                (1.0 - nums[2]) * (1.0 - k),
            ])
        }
        _ => None,
    }
}

/// Read a 4-number rectangle array.
fn read_rect4(dict: &Dictionary, key: &[u8]) -> Option<[f64; 4]> {
    let nums: Vec<f64> = dict
        .get(key)
        .and_then(Object::as_array)?
        .iter()
        .filter_map(Object::as_f64)
        .collect();
    (nums.len() == 4).then(|| [nums[0], nums[1], nums[2], nums[3]])
}

/// Read a flat `[x y x y …]` array into `(x, y)` pairs.
fn read_points(dict: &Dictionary, key: &[u8]) -> Vec<(f64, f64)> {
    let Some(arr) = dict.get(key).and_then(Object::as_array) else {
        return Vec::new();
    };
    let nums: Vec<f64> = arr.iter().filter_map(Object::as_f64).collect();
    nums.chunks_exact(2).map(|c| (c[0], c[1])).collect()
}

/// The annotation's border width (`/BS /W`), defaulting to `1.0`.
fn read_line_width(dict: &Dictionary) -> f64 {
    dict.get(b"BS")
        .and_then(Object::as_dict)
        .and_then(|bs| bs.get(b"W"))
        .and_then(Object::as_f64)
        .unwrap_or(1.0)
}

/// Rebuild the appearance ([`Built`]) of an existing annotation from its stored
/// geometry, for [`Document::regenerate_appearance`]. Supports the geometric and
/// text-markup subtypes the engine authors (Square, Circle, Line, Polygon,
/// PolyLine, Highlight, Underline, StrikeOut, Ink, Caret). Returns `None` for
/// subtypes whose appearance cannot be reconstructed from the dictionary alone
/// (e.g. FreeText, Stamp, Text, Link).
pub(crate) fn rebuild(dict: &Dictionary) -> Option<Built> {
    let subtype = dict.get(b"Subtype").and_then(Object::as_name)?.to_vec();
    let rect = read_rect4(dict, b"Rect").unwrap_or([0.0, 0.0, 0.0, 0.0]);
    let stroke = read_rgb(dict, b"C");
    let fill = read_rgb(dict, b"IC");
    let lw = read_line_width(dict);
    let black = [0.0, 0.0, 0.0];
    Some(match subtype.as_slice() {
        b"Square" => square(rect, stroke, fill, lw),
        b"Circle" => circle(rect, stroke, fill, lw),
        b"Highlight" => highlight(rect, stroke.unwrap_or([1.0, 1.0, 0.0])),
        b"Underline" => underline(rect, stroke.unwrap_or(black)),
        b"StrikeOut" => strike_out(rect, stroke.unwrap_or(black)),
        b"Caret" => caret(rect, stroke.unwrap_or(black)),
        b"Line" => {
            let l = read_points(dict, b"L");
            if l.len() < 2 {
                return None;
            }
            let arrow = dict
                .get(b"LE")
                .and_then(Object::as_array)
                .is_some_and(|le| {
                    le.iter()
                        .any(|o| o.as_name() == Some(b"OpenArrow".as_slice()))
                });
            line(
                l[0].0,
                l[0].1,
                l[1].0,
                l[1].1,
                stroke.unwrap_or(black),
                lw,
                arrow,
            )
        }
        b"Polygon" => {
            let v = read_points(dict, b"Vertices");
            if v.len() < 2 {
                return None;
            }
            polygon(&v, stroke, fill, lw)
        }
        b"PolyLine" => {
            let v = read_points(dict, b"Vertices");
            if v.len() < 2 {
                return None;
            }
            polyline(&v, stroke.unwrap_or(black), lw)
        }
        b"Ink" => {
            let paths: Vec<Vec<(f64, f64)>> = dict
                .get(b"InkList")
                .and_then(Object::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Object::as_array)
                        .map(|stroke_path| {
                            let nums: Vec<f64> =
                                stroke_path.iter().filter_map(Object::as_f64).collect();
                            nums.chunks_exact(2).map(|c| (c[0], c[1])).collect()
                        })
                        .collect()
                })
                .unwrap_or_default();
            ink(&paths, stroke.unwrap_or(black), lw)
        }
        _ => return None,
    })
}

// ── default-appearance synthesis (rendering an annotation that has no /AP) ──
//
// ISO 32000-1 §12.5.5: a conforming reader that finds an annotation without an
// appearance stream synthesises a default appearance from the annotation dict.
// The functions below build that appearance as content-stream bytes **in page
// user space** (bottom-left origin) — they are mapped straight onto the page
// with the page matrix (no BBox→Rect appearance transform, since the geometry is
// computed directly against `/Rect`/`/QuadPoints`). They are append-only paint
// helpers; none mutate document state. They mirror the create-side appearance
// generators above so a synthesised look matches the engine's own annotations.

/// A `/Resources` dictionary exposing a non-embedded Helvetica as `/Helv` — the
/// font a synthesised text appearance (FreeText / Stamp) references. The
/// rasterizer draws it through the base-14 substitution in `render_fonts_for`.
pub(crate) fn helv_resources() -> Dictionary {
    let mut helv = Dictionary::new();
    helv.set(b"Type".to_vec(), name(b"Font"));
    helv.set(b"Subtype".to_vec(), name(b"Type1"));
    helv.set(b"BaseFont".to_vec(), name(b"Helvetica"));
    let mut fonts = Dictionary::new();
    fonts.set(b"Helv".to_vec(), Object::Dictionary(helv));
    let mut resources = Dictionary::new();
    resources.set(b"Font".to_vec(), Object::Dictionary(fonts));
    resources
}

/// Append a `(text)` string-literal operand (WinAnsi-encoded, parens/backslash
/// escaped) to a content stream being built.
fn push_text_literal(out: &mut Vec<u8>, text: &str) {
    out.push(b'(');
    for &byte in &crate::font::encode_winansi(text) {
        if matches!(byte, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b')');
}

/// Synthesised FreeText appearance: paint `text` inside `rect` in the `/DA`
/// colour at `font_size`, honouring `/Q` quadding (0 left, 1 centre, 2 right).
/// Lines are split on `\n` / `\r` and stacked from the top of the rect. Returns
/// the page-space content bytes and the `/Helv` resources the text needs.
pub(crate) fn free_text_default(
    rect: [f64; 4],
    text: &str,
    font_size: f64,
    color: [f64; 3],
    quadding: u8,
) -> (Vec<u8>, Dictionary) {
    let [x0, y0, x1, y1] = rect;
    let [r, g, b] = color;
    let size = if font_size > 0.0 { font_size } else { 12.0 };
    let leading = size * 1.15;
    let pad = 2.0;
    let box_w = (x1 - x0 - 2.0 * pad).max(0.0);
    // Approximate Helvetica advance for crude horizontal placement.
    let char_w = size * 0.5;

    let mut out = Vec::new();
    if text.trim().is_empty() {
        return (out, helv_resources());
    }
    out.extend_from_slice(b"q\nBT\n");
    out.extend_from_slice(format!("/Helv {} Tf\n", num(size)).as_bytes());
    out.extend_from_slice(format!("{} {} {} rg\n", num(r), num(g), num(b)).as_bytes());

    let mut baseline = y1 - pad - size;
    for line in text.split(['\n', '\r']) {
        // Stop once we run past the bottom of the box (still emit at least one).
        if baseline < y0 - size {
            break;
        }
        let text_w = line.chars().count() as f64 * char_w;
        let tx = match quadding {
            1 => x0 + pad + ((box_w - text_w) / 2.0).max(0.0), // centre
            2 => x1 - pad - text_w.min(box_w),                 // right
            _ => x0 + pad,                                     // left (default)
        };
        out.extend_from_slice(format!("1 0 0 1 {} {} Tm\n", num(tx), num(baseline)).as_bytes());
        push_text_literal(&mut out, line);
        out.extend_from_slice(b" Tj\n");
        baseline -= leading;
    }
    out.extend_from_slice(b"ET\nQ\n");
    (out, helv_resources())
}

/// Synthesised Squiggly appearance: a wavy underline in `color` along each quad
/// of `quad_points` (8 values per quad, ISO order UL UR LL LR). When
/// `quad_points` is empty the whole `rect` is treated as one span. The wave is a
/// stroked zigzag near the baseline — more faithful than a flat rule.
pub(crate) fn squiggly_default(rect: [f64; 4], quad_points: &[f64], color: [f64; 3]) -> Vec<u8> {
    let mut out = Vec::new();
    let [r, g, b] = color;
    out.extend_from_slice(b"q\n");
    out.extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());

    // Collect spans as (x_left, x_right, y_bottom, y_top) in user space.
    let mut spans: Vec<(f64, f64, f64, f64)> = Vec::new();
    if quad_points.len() >= 8 {
        for q in quad_points.chunks_exact(8) {
            let xs = [q[0], q[2], q[4], q[6]];
            let ys = [q[1], q[3], q[5], q[7]];
            let xl = xs.iter().copied().fold(f64::INFINITY, f64::min);
            let xr = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let yb = ys.iter().copied().fold(f64::INFINITY, f64::min);
            let yt = ys.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            spans.push((xl, xr, yb, yt));
        }
    } else {
        spans.push((rect[0], rect[2], rect[1], rect[3]));
    }

    for (xl, xr, yb, yt) in spans {
        let h = (yt - yb).max(1.0);
        let line_w = (h * 0.06).max(0.75);
        let amp = (h * 0.06).clamp(0.6, 2.0); // wave amplitude
        let base = yb + h * 0.08; // sit near the baseline
        let period = (amp * 4.0).max(3.0);
        out.extend_from_slice(format!("{} w\n", num(line_w)).as_bytes());
        out.extend_from_slice(b"1 J\n1 j\n"); // round caps/joins
        out.extend_from_slice(format!("{} {} m\n", num(xl), num(base)).as_bytes());
        let mut x = xl;
        let mut up = true;
        while x < xr {
            let nx = (x + period / 2.0).min(xr);
            let y = if up { base + amp } else { base - amp };
            out.extend_from_slice(format!("{} {} l\n", num(nx), num(y)).as_bytes());
            x = nx;
            up = !up;
        }
        out.extend_from_slice(b"S\n");
    }
    out.extend_from_slice(b"Q\n");
    out
}

/// Synthesised Link appearance: the border rectangle around `rect`, drawn only
/// when an effective border width `> 0` is given (`/Border` element 3 or `/BS
/// /W`). Links are otherwise invisible. `color` is the `/C` border colour
/// (defaults to black at the call site when `/C` is absent).
pub(crate) fn link_border_default(rect: [f64; 4], width: f64, color: [f64; 3]) -> Vec<u8> {
    if width <= 0.0 {
        return Vec::new();
    }
    let [x0, y0, x1, y1] = rect;
    let inset = width / 2.0;
    content::rectangle_ops(
        x0 + inset,
        y0 + inset,
        (x1 - x0 - width).max(0.0),
        (y1 - y0 - width).max(0.0),
        Some(color),
        None,
        width,
    )
}

/// Synthesised Text (sticky-note) appearance: a small note icon at the
/// **top-left** of `rect` — a filled badge with a folded corner and three
/// "text" rules — in `color` (defaults to yellow at the call site). Independent
/// of the rect size (a comment marker, ~18pt square), like a reader's note pin.
pub(crate) fn text_note_default(rect: [f64; 4], color: [f64; 3]) -> Vec<u8> {
    let [r, g, b] = color;
    let x = rect[0].min(rect[2]);
    let top = rect[1].max(rect[3]);
    let s = 18.0_f64; // icon size in points
    let y = top - s; // grow downward from the top-left corner
    let fold = s * 0.3;

    let mut out = Vec::new();
    out.extend_from_slice(b"q\n");
    // Filled body (page corner) + dark outline.
    out.extend_from_slice(format!("{} {} {} rg\n", num(r), num(g), num(b)).as_bytes());
    out.extend_from_slice(b"0 0 0 RG\n0.6 w\n");
    out.extend_from_slice(format!("{} {} m\n", num(x), num(y)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(x), num(y + s)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(x + s), num(y + s)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(x + s), num(y + fold)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(x + s - fold), num(y)).as_bytes());
    out.extend_from_slice(b"h\nB\n"); // close, fill + stroke
                                      // Folded corner triangle.
    out.extend_from_slice(format!("{} {} m\n", num(x + s - fold), num(y)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(x + s - fold), num(y + fold)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(x + s), num(y + fold)).as_bytes());
    out.extend_from_slice(b"S\n");
    // Three short "text" rules.
    let mut ry = y + s - s * 0.32;
    for _ in 0..3 {
        out.extend_from_slice(format!("{} {} m\n", num(x + s * 0.2), num(ry)).as_bytes());
        out.extend_from_slice(format!("{} {} l\n", num(x + s * 0.8), num(ry)).as_bytes());
        out.extend_from_slice(b"S\n");
        ry -= s * 0.22;
    }
    out.extend_from_slice(b"Q\n");
    out
}

/// Synthesised FileAttachment appearance: a paperclip-like icon at the
/// **top-left** of `rect` in `color` (defaults to a muted grey at the call
/// site). Size-independent (~18pt), like a reader's attachment marker.
pub(crate) fn file_attachment_default(rect: [f64; 4], color: [f64; 3]) -> Vec<u8> {
    let [r, g, b] = color;
    let x = rect[0].min(rect[2]);
    let top = rect[1].max(rect[3]);
    let s = 18.0_f64;
    let y = top - s;

    // Paperclip: an outer rounded "U" and an inner shorter "U", drawn as two
    // nested rounded rectangles' open loops (approximated with straight legs).
    let cx = x + s * 0.5;
    let half_out = s * 0.22;
    let half_in = s * 0.12;
    let bottom = y + s * 0.18;
    let outer_top = y + s * 0.82;
    let inner_top = y + s * 0.62;

    let mut out = Vec::new();
    out.extend_from_slice(b"q\n");
    out.extend_from_slice(format!("{} {} {} RG\n", num(r), num(g), num(b)).as_bytes());
    out.extend_from_slice(format!("{} w\n", num((s * 0.08).max(0.8))).as_bytes());
    out.extend_from_slice(b"1 J\n1 j\n");
    // Outer loop (open at the top).
    out.extend_from_slice(format!("{} {} m\n", num(cx - half_out), num(outer_top)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(cx - half_out), num(bottom)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(cx + half_out), num(bottom)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(cx + half_out), num(outer_top)).as_bytes());
    out.extend_from_slice(b"S\n");
    // Inner loop (shorter, open at the bottom).
    out.extend_from_slice(format!("{} {} m\n", num(cx - half_in), num(y + s * 0.95)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(cx - half_in), num(inner_top)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(cx + half_in), num(inner_top)).as_bytes());
    out.extend_from_slice(format!("{} {} l\n", num(cx + half_in), num(y + s * 0.95)).as_bytes());
    out.extend_from_slice(b"S\n");
    out.extend_from_slice(b"Q\n");
    out
}

/// Synthesised Stamp appearance: a labelled, bordered box filling `rect`, the
/// label taken from `/Name`. Reuses the create-side look (`stamp`) so a
/// synthesised stamp matches an engine-authored one. Returns the page-space
/// content bytes and the `/Helv` resources.
pub(crate) fn stamp_default(rect: [f64; 4], label: &str, color: [f64; 3]) -> (Vec<u8>, Dictionary) {
    let [x0, y0, x1, y1] = rect;
    let [r, g, b] = color;
    let width = (x1 - x0).abs();
    let height = (y1 - y0).abs();
    let line_width = (height * 0.06).clamp(1.0, 3.0);
    let font_size = (height * 0.5).clamp(8.0, 24.0);
    let mut out = content::rectangle_ops(
        x0 + line_width,
        y0 + line_width,
        width - 2.0 * line_width,
        height - 2.0 * line_width,
        Some(color),
        None,
        line_width,
    );
    if !label.is_empty() {
        let text_width = label.chars().count() as f64 * font_size * 0.5;
        let tx = x0 + ((width - text_width) / 2.0).max(line_width + 2.0);
        let ty = y0 + (height - font_size) / 2.0 + font_size * 0.2;
        out.extend_from_slice(b"BT\n");
        out.extend_from_slice(format!("/Helv {} Tf\n", num(font_size)).as_bytes());
        out.extend_from_slice(format!("{} {} {} rg\n", num(r), num(g), num(b)).as_bytes());
        out.extend_from_slice(format!("{} {} Td\n", num(tx), num(ty)).as_bytes());
        push_text_literal(&mut out, label);
        out.extend_from_slice(b" Tj\nET\n");
    }
    (out, helv_resources())
}
