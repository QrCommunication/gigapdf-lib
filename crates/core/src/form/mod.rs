//! Interactive forms — AcroForm (ISO 32000-1 §12.7).
//!
//! A form is the catalog's `/AcroForm` dictionary, whose `/Fields` array holds
//! the (possibly nested) field objects. A terminal field carries `/FT` (type),
//! `/T` (partial name), `/V` (value) and `/Ff` (flags); the traversal lives on
//! `Document` because it needs to resolve indirect references.
//!
//! There are only four field *types* (`Tx`, `Btn`, `Ch`, `Sig`) but the `/Ff`
//! flag bits split them into the concrete widgets a user sees — a `Btn` is a
//! checkbox, a radio group, or a push-button depending on its flags.

/// Field flag bits (`/Ff`, ISO 32000-1 Tables 226/228/230). Bits are numbered
/// from 1 in the spec, so "bit *n*" is `1 << (n - 1)`.
pub mod flags {
    // Common to every field type (Table 221).
    /// The field is read-only.
    pub const READ_ONLY: u32 = 1 << 0; // bit 1
    /// The field must have a value at submit time.
    pub const REQUIRED: u32 = 1 << 1; // bit 2
    /// The field must not be exported by a submit-form action.
    pub const NO_EXPORT: u32 = 1 << 2; // bit 3

    // Button fields (Table 226).
    /// A radio that cannot be toggled off once selected.
    pub const NO_TOGGLE_TO_OFF: u32 = 1 << 14; // bit 15
    /// The button is one of a set of radio buttons.
    pub const RADIO: u32 = 1 << 15; // bit 16
    /// The button is a push-button (no persistent value).
    pub const PUSHBUTTON: u32 = 1 << 16; // bit 17
    /// Radios with the same value toggle in unison.
    pub const RADIOS_IN_UNISON: u32 = 1 << 25; // bit 26

    // Text fields (Table 228).
    /// The text may span multiple lines.
    pub const MULTILINE: u32 = 1 << 12; // bit 13
    /// A password field (value hidden as the user types).
    pub const PASSWORD: u32 = 1 << 13; // bit 14
    /// A file-select field.
    pub const FILE_SELECT: u32 = 1 << 20; // bit 21
    /// Do not check spelling.
    pub const DO_NOT_SPELL_CHECK: u32 = 1 << 22; // bit 23
    /// Do not scroll long text.
    pub const DO_NOT_SCROLL: u32 = 1 << 23; // bit 24
    /// Comb of `/MaxLen` equally spaced cells.
    pub const COMB: u32 = 1 << 24; // bit 25
    /// Rich-text value.
    pub const RICH_TEXT: u32 = 1 << 25; // bit 26

    // Choice fields (Table 230).
    /// A combo box (drop-down); otherwise a list box.
    pub const COMBO: u32 = 1 << 17; // bit 18
    /// An editable combo box (the user may type a custom value).
    pub const EDIT: u32 = 1 << 18; // bit 19
    /// Options are presented sorted.
    pub const SORT: u32 = 1 << 19; // bit 20
    /// More than one option may be selected.
    pub const MULTI_SELECT: u32 = 1 << 21; // bit 22
    /// Commit the selected value as soon as it is chosen.
    pub const COMMIT_ON_SEL_CHANGE: u32 = 1 << 26; // bit 27
}

/// The concrete kind of a form field, derived from `/FT` and `/Ff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Single- or multi-line text input (`/Tx`).
    Text,
    /// On/off checkbox (`/Btn`, neither radio nor push-button).
    Checkbox,
    /// One of a set of mutually exclusive radio buttons (`/Btn` + Radio).
    Radio,
    /// A push-button with no persistent value (`/Btn` + Pushbutton).
    PushButton,
    /// Drop-down selection (`/Ch` + Combo).
    ComboBox,
    /// Scrolling list selection (`/Ch`, not Combo).
    ListBox,
    /// A digital signature field (`/Sig`).
    Signature,
    /// Unrecognised `/FT`.
    Unknown,
}

/// A terminal form field with its type, value, flags and (for buttons/choices)
/// the set of selectable options.
#[derive(Debug, Clone)]
pub struct FormField {
    /// Fully-qualified name (partial names joined by `.`).
    pub name: String,
    /// Field type: `"Tx"` (text), `"Btn"` (button), `"Ch"` (choice), `"Sig"`.
    pub field_type: String,
    /// Current value (text, or the selected export name for buttons/choices).
    pub value: String,
    /// Raw `/Ff` flag bits (see [`flags`]).
    pub flags: u32,
    /// Selectable options: choice display strings, or button export states.
    pub options: Vec<String>,
    /// `/MaxLen` for text fields, if present.
    pub max_len: Option<u32>,
    /// 1-based page number of the field's first widget (from its `/P`), if known.
    pub page: Option<u32>,
    /// First widget bounds `[x, y, width, height]` in **top-left** origin
    /// (points), if the widget has a `/Rect`.
    pub bounds: Option<[f64; 4]>,
}

impl FormField {
    /// The concrete widget kind, derived from `field_type` and `flags`.
    pub fn kind(&self) -> FieldKind {
        match self.field_type.as_str() {
            "Tx" => FieldKind::Text,
            "Sig" => FieldKind::Signature,
            "Btn" => {
                if self.flags & flags::PUSHBUTTON != 0 {
                    FieldKind::PushButton
                } else if self.flags & flags::RADIO != 0 {
                    FieldKind::Radio
                } else {
                    FieldKind::Checkbox
                }
            }
            "Ch" => {
                if self.flags & flags::COMBO != 0 {
                    FieldKind::ComboBox
                } else {
                    FieldKind::ListBox
                }
            }
            _ => FieldKind::Unknown,
        }
    }

    /// Whether a text field accepts multiple lines.
    pub fn is_multiline(&self) -> bool {
        self.field_type == "Tx" && self.flags & flags::MULTILINE != 0
    }

    /// Whether a text field hides its value (password).
    pub fn is_password(&self) -> bool {
        self.field_type == "Tx" && self.flags & flags::PASSWORD != 0
    }

    /// Whether a combo box lets the user type a custom value.
    pub fn is_editable_combo(&self) -> bool {
        self.kind() == FieldKind::ComboBox && self.flags & flags::EDIT != 0
    }

    /// Whether a list box allows selecting several options.
    pub fn is_multi_select(&self) -> bool {
        self.field_type == "Ch" && self.flags & flags::MULTI_SELECT != 0
    }

    /// Whether the field is read-only.
    pub fn is_read_only(&self) -> bool {
        self.flags & flags::READ_ONLY != 0
    }

    /// Whether the field must be filled before submission.
    pub fn is_required(&self) -> bool {
        self.flags & flags::REQUIRED != 0
    }

    /// Whether the value can be set programmatically (everything but
    /// push-buttons and signatures).
    pub fn is_fillable(&self) -> bool {
        !matches!(self.kind(), FieldKind::PushButton | FieldKind::Signature) && !self.is_read_only()
    }
}

// ─── field *creation* ────────────────────────────────────────────────────────
//
// The reading model above describes existing fields; the pieces below help
// *build* new ones. They are pure (no object-id allocation): a builder returns a
// `/MK` characteristics dict and/or an appearance content stream, and
// `Document` allocates the objects, appends to the page `/Annots` and registers
// the field in the AcroForm.

use crate::object::{Dictionary, Object, StringKind};

/// Visual styling applied to a newly created field.
#[derive(Debug, Clone)]
pub struct FieldStyle {
    /// Text size in points; `0.0` requests auto-size (`/DA … 0 Tf`).
    pub font_size: f64,
    /// Text / mark colour (RGB, components in `0.0..=1.0`).
    pub color: [f64; 3],
    /// Border colour (RGB), or `None` for no visible border.
    pub border: Option<[f64; 3]>,
    /// Background fill (RGB), or `None` for transparent.
    pub background: Option<[f64; 3]>,
    /// Border width in points.
    pub border_width: f64,
}

impl Default for FieldStyle {
    fn default() -> Self {
        FieldStyle {
            font_size: 0.0,
            color: [0.0, 0.0, 0.0],
            border: Some([0.0, 0.0, 0.0]),
            background: None,
            border_width: 1.0,
        }
    }
}

/// Format a coordinate compactly: up to 3 decimals, trailing zeros trimmed.
fn n(v: f64) -> String {
    let mut s = format!("{v:.3}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// Escape a string for use as a `( … )` literal operand inside a content
/// stream: backslash, parentheses, and control bytes.
fn escape_stream_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for b in s.bytes() {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'(' => out.push_str("\\("),
            b')' => out.push_str("\\)"),
            b'\r' => out.push_str("\\r"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            other => out.push_str(&format!("\\{other:03o}")),
        }
    }
    out
}

/// The default-appearance string (`/DA`): font, size and colour for the field's
/// variable text. `/Helv` resolves through the AcroForm `/DR`.
pub(crate) fn da_string(style: &FieldStyle) -> Object {
    let [r, g, b] = style.color;
    let da = format!(
        "/Helv {} Tf {} {} {} rg",
        n(style.font_size),
        n(r),
        n(g),
        n(b)
    );
    Object::String(da.into_bytes(), StringKind::Literal)
}

/// The widget `/MK` appearance-characteristics dict (border + background), or
/// `None` when the field has neither.
pub(crate) fn mk_dict(style: &FieldStyle) -> Option<Dictionary> {
    if style.border.is_none() && style.background.is_none() {
        return None;
    }
    let rgb = |c: [f64; 3]| Object::Array(c.iter().map(|v| Object::Real(*v)).collect());
    let mut mk = Dictionary::new();
    if let Some(bc) = style.border {
        mk.set(b"BC", rgb(bc));
    }
    if let Some(bg) = style.background {
        mk.set(b"BG", rgb(bg));
    }
    Some(mk)
}

/// Draw the field's background fill and border, in the box `[0,0,w,h]`. Shared
/// prefix for every appearance stream so the static `/AP` matches the `/MK`.
fn box_decoration(style: &FieldStyle, w: f64, h: f64) -> String {
    let mut s = String::new();
    if let Some([r, g, b]) = style.background {
        s.push_str(&format!(
            "{} {} {} rg\n0 0 {} {} re\nf\n",
            n(r),
            n(g),
            n(b),
            n(w),
            n(h)
        ));
    }
    if let Some([r, g, b]) = style.border {
        if style.border_width > 0.0 {
            let bw = style.border_width;
            let i = bw / 2.0;
            s.push_str(&format!(
                "{} {} {} RG\n{} w\n{} {} {} {} re\nS\n",
                n(r),
                n(g),
                n(b),
                n(bw),
                n(i),
                n(i),
                n(w - bw),
                n(h - bw)
            ));
        }
    }
    s
}

/// Concrete on-glyph text size for a static appearance (resolves auto-size to a
/// value that fits the box).
fn resolved_size(style: &FieldStyle, h: f64) -> f64 {
    if style.font_size > 0.0 {
        style.font_size
    } else {
        (h - 4.0).clamp(4.0, 12.0)
    }
}

/// Appearance content for a text / choice field showing `value` on one line.
pub(crate) fn text_appearance(value: &str, style: &FieldStyle, w: f64, h: f64) -> Vec<u8> {
    let size = resolved_size(style, h);
    let pad = 2.0;
    let ty = ((h - size) / 2.0 + size * 0.2).max(pad);
    let [r, g, b] = style.color;
    let mut s = box_decoration(style, w, h);
    s.push_str("/Tx BMC\nq\nBT\n");
    s.push_str(&format!("/Helv {} Tf\n", n(size)));
    s.push_str(&format!("{} {} {} rg\n", n(r), n(g), n(b)));
    s.push_str(&format!("{} {} Td\n", n(pad), n(ty)));
    s.push_str(&format!("({}) Tj\n", escape_stream_literal(value)));
    s.push_str("ET\nQ\nEMC\n");
    s.into_bytes()
}

/// Appearance content for a checked checkbox: the box decoration plus a tick
/// drawn as vector strokes (no font dependency).
pub(crate) fn check_appearance(style: &FieldStyle, w: f64, h: f64) -> Vec<u8> {
    let [r, g, b] = style.color;
    let lw = (w.min(h) * 0.1).max(0.6);
    let mut s = box_decoration(style, w, h);
    s.push_str(&format!(
        "{} {} {} RG\n{} w\n1 J 1 j\n",
        n(r),
        n(g),
        n(b),
        n(lw)
    ));
    s.push_str(&format!("{} {} m\n", n(w * 0.22), n(h * 0.50)));
    s.push_str(&format!("{} {} l\n", n(w * 0.42), n(h * 0.28)));
    s.push_str(&format!("{} {} l\nS\n", n(w * 0.80), n(h * 0.75)));
    s.into_bytes()
}

/// Appearance content for a selected radio button: a filled dot (a circle
/// approximated by four cubic Béziers).
pub(crate) fn radio_appearance(style: &FieldStyle, w: f64, h: f64) -> Vec<u8> {
    let [r, g, b] = style.color;
    let (cx, cy) = (w / 2.0, h / 2.0);
    let rad = w.min(h) * 0.3;
    let k = 0.5523 * rad;
    let mut s = box_decoration(style, w, h);
    s.push_str(&format!("{} {} {} rg\n", n(r), n(g), n(b)));
    s.push_str(&format!("{} {} m\n", n(cx + rad), n(cy)));
    s.push_str(&format!(
        "{} {} {} {} {} {} c\n",
        n(cx + rad),
        n(cy + k),
        n(cx + k),
        n(cy + rad),
        n(cx),
        n(cy + rad)
    ));
    s.push_str(&format!(
        "{} {} {} {} {} {} c\n",
        n(cx - k),
        n(cy + rad),
        n(cx - rad),
        n(cy + k),
        n(cx - rad),
        n(cy)
    ));
    s.push_str(&format!(
        "{} {} {} {} {} {} c\n",
        n(cx - rad),
        n(cy - k),
        n(cx - k),
        n(cy - rad),
        n(cx),
        n(cy - rad)
    ));
    s.push_str(&format!(
        "{} {} {} {} {} {} c\nf\n",
        n(cx + k),
        n(cy - rad),
        n(cx + rad),
        n(cy - k),
        n(cx + rad),
        n(cy)
    ));
    s.into_bytes()
}

/// Appearance content for the empty (Off / unfocused) state — just the box
/// decoration, no glyph.
pub(crate) fn empty_appearance(style: &FieldStyle, w: f64, h: f64) -> Vec<u8> {
    box_decoration(style, w, h).into_bytes()
}
