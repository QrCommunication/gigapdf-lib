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
        !matches!(self.kind(), FieldKind::PushButton | FieldKind::Signature)
            && !self.is_read_only()
    }
}
