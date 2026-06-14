//! PDF annotations (ISO 32000-1 §12.5): list, create (with appearance streams)
//! and remove. Each created annotation gets an appearance stream (`/AP /N`) so
//! it renders consistently in every viewer.

use crate::content::{self, num};
use crate::object::{Dictionary, Object, StringKind};

/// An annotation as read from a page's `/Annots`.
#[derive(Debug, Clone)]
pub struct Annotation {
    /// 0-based index in the page `/Annots` array.
    pub index: usize,
    /// `/Subtype` (e.g. "Square", "Highlight", "Line", "FreeText").
    pub subtype: String,
    /// `/Rect` `[x0 y0 x1 y1]`.
    pub rect: [f64; 4],
    /// `/Contents` text, if any.
    pub contents: String,
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

/// Line annotation.
pub(crate) fn line(x1: f64, y1: f64, x2: f64, y2: f64, color: [f64; 3], line_width: f64) -> Built {
    let mut dict = Dictionary::new();
    dict.set(b"Subtype".to_vec(), name(b"Line"));
    let rect = [x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)];
    dict.set(b"Rect".to_vec(), real_array(&rect));
    dict.set(b"L".to_vec(), real_array(&[x1, y1, x2, y2]));
    dict.set(b"C".to_vec(), real_array(&color));
    dict.set(b"BS".to_vec(), border_style(line_width));
    let appearance = content::line_ops(x1, y1, x2, y2, color, line_width);
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
    let rect = [min_x - margin, min_y - margin, max_x + margin, max_y + margin];

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
