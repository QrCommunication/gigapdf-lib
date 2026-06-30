//! Page margins and running header/footer baking for an existing PDF.
//!
//! This module carries the **value types** shared by the [`Document`] margin and
//! header/footer API ([`Document::page_margins`], [`Document::set_page_margins`],
//! [`Document::set_header`], [`Document::set_footer`], and the matching
//! `remove_*`); the behaviour lives in [`crate::document`]. Everything is in
//! **PDF points** (`1pt = 1/72 in`), `f64`.
//!
//! [`Document`]: crate::document::Document
//! [`Document::page_margins`]: crate::document::Document::page_margins
//! [`Document::set_page_margins`]: crate::document::Document::set_page_margins
//! [`Document::set_header`]: crate::document::Document::set_header
//! [`Document::set_footer`]: crate::document::Document::set_footer

/// Per-side page margins, in points. Field names mirror
/// [`html::Margins`](crate::html::Margins) and
/// [`model::geom::Margins`](crate::model::geom::Margins) so the three never drift;
/// this one is the type the PDF [`Document`](crate::document::Document) margin API
/// speaks (the `model` tree stays self-contained — importers convert at the edges).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margins {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl Margins {
    /// The same margin on every side.
    pub fn uniform(m: f64) -> Self {
        Self {
            top: m,
            right: m,
            bottom: m,
            left: m,
        }
    }
}

impl Default for Margins {
    /// 0.5" on every side (`36pt`).
    fn default() -> Self {
        Self::uniform(36.0)
    }
}

/// Horizontal alignment of header/footer text within the printable width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

impl Align {
    /// Parse the JSON/string spelling (`"left"`/`"center"`/`"right"`,
    /// case-insensitive). Unknown values fall back to [`Align::Left`].
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "center" | "centre" | "middle" => Align::Center,
            "right" | "end" => Align::Right,
            _ => Align::Left,
        }
    }
}

/// A running header or footer to bake onto the pages of an existing PDF.
///
/// `text` may contain the tokens `{{page}}` (1-based page number) and `{{pages}}`
/// (total page count), substituted per page. The text is drawn in the standard
/// base-14 **Helvetica** (no embedding) inside the page's top (header) / bottom
/// (footer) margin band, horizontally aligned per [`align`](Self::align).
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderFooterSpec {
    /// Template text, with `{{page}}` / `{{pages}}` tokens.
    pub text: String,
    /// Horizontal alignment within the printable width.
    pub align: Align,
    /// Font size in points.
    pub font_size: f64,
    /// RGB fill colour, `0.0..=1.0` per channel.
    pub color: [f64; 3],
    /// Inclusive 1-based page range `(first, last)` the H/F applies to; `None`
    /// means every page.
    pub page_range: Option<(usize, usize)>,
    /// Draw on the first page of the range too. When `false`, page 1 (or the
    /// first in-range page) is skipped — the common "no header on the cover" case.
    pub show_on_first_page: bool,
    /// Height of the band (from the page edge inward) the text sits in, in points.
    /// The baseline is vertically centred in this band.
    pub band_height: f64,
}

impl Default for HeaderFooterSpec {
    fn default() -> Self {
        Self {
            text: String::new(),
            align: Align::Left,
            font_size: 10.0,
            color: [0.0, 0.0, 0.0],
            page_range: None,
            show_on_first_page: true,
            band_height: 36.0,
        }
    }
}

impl HeaderFooterSpec {
    /// Substitute `{{page}}` (1-based) and `{{pages}}` (total) into `text`.
    pub fn render_text(&self, page_1based: usize, total_pages: usize) -> String {
        self.text
            .replace("{{page}}", &page_1based.to_string())
            .replace("{{pages}}", &total_pages.to_string())
    }

    /// Parse a flat JSON object into a spec, filling absent fields from
    /// [`Default`]. Returns `None` only on malformed JSON (so a host gets a clear
    /// error rather than a silently-wrong header). Recognised keys:
    /// `text` (string), `align` (string), `fontSize`/`font_size` (number),
    /// `color` (3-number array), `pageRange`/`page_range` (2-number array or
    /// null), `showOnFirstPage`/`show_on_first_page` (bool),
    /// `bandHeight`/`band_height` (number).
    pub fn from_json(s: &str) -> Option<Self> {
        let mut spec = HeaderFooterSpec::default();
        let mut p = ObjReader::new(s);
        p.object(|p, key| {
            match key {
                "text" => spec.text = p.string()?,
                "align" => spec.align = Align::from_str_lossy(&p.string()?),
                "fontSize" | "font_size" => spec.font_size = p.number()?,
                "color" => {
                    let nums = p.number_array()?;
                    if nums.len() >= 3 {
                        spec.color = [nums[0], nums[1], nums[2]];
                    }
                }
                "pageRange" | "page_range" => {
                    if p.peek()? == b'n' {
                        p.null()?;
                        spec.page_range = None;
                    } else {
                        let nums = p.number_array()?;
                        if nums.len() >= 2 {
                            spec.page_range = Some((nums[0] as usize, nums[1] as usize));
                        }
                    }
                }
                "showOnFirstPage" | "show_on_first_page" => {
                    spec.show_on_first_page = p.boolean()?
                }
                "bandHeight" | "band_height" => spec.band_height = p.number()?,
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        p.ws();
        if p.done() {
            Some(spec)
        } else {
            None
        }
    }

    /// Serialise to a flat JSON object with the same keys [`from_json`] reads
    /// (`text`, `align`, `fontSize`, `color`, `pageRange`, `showOnFirstPage`,
    /// `bandHeight`), so `from_json(spec.to_json())` round-trips. Hand-rolled,
    /// zero-dependency; the string escaping matches the crate's other JSON
    /// emitters. `pageRange` is `null` when [`page_range`](Self::page_range) is
    /// `None`.
    ///
    /// [`from_json`]: Self::from_json
    /// [`page_range`]: Self::page_range
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"text\":");
        json_escape(&self.text, &mut out);
        out.push_str(",\"align\":");
        json_escape(
            match self.align {
                Align::Left => "left",
                Align::Center => "center",
                Align::Right => "right",
            },
            &mut out,
        );
        out.push_str(&format!(",\"fontSize\":{}", self.font_size));
        out.push_str(&format!(
            ",\"color\":[{},{},{}]",
            self.color[0], self.color[1], self.color[2]
        ));
        match self.page_range {
            Some((first, last)) => out.push_str(&format!(",\"pageRange\":[{first},{last}]")),
            None => out.push_str(",\"pageRange\":null"),
        }
        out.push_str(&format!(",\"showOnFirstPage\":{}", self.show_on_first_page));
        out.push_str(&format!(",\"bandHeight\":{}}}", self.band_height));
        out
    }

    /// Lower this flat one-sided spec into a rich [`RunningHeaderFooter`] carrying
    /// a single [`HFItem::Text`] in the `default` zone — on the **header** side
    /// when `header == true`, else the footer. `text`/`align`/`font_size`/`color`
    /// map across (colour 0..=1 → 0..=255); `band_height` becomes the matching
    /// `header_band`/`footer_band`. `show_on_first_page == false` sets
    /// `different_first_page` with an **empty** `first_page` zone, so the cover
    /// shows nothing — the rich equivalent of the flat "no header on the cover".
    /// `page_range` has no rich counterpart and is dropped (the flat
    /// [`Document::set_header`](crate::document::Document::set_header) path still
    /// honours it). This is the bridge that lets a host migrate the flat API onto
    /// the rich [`Document::set_running_header_footer`](crate::document::Document::set_running_header_footer).
    pub fn to_running(&self, header: bool) -> RunningHeaderFooter {
        let to_u8 = |c: f64| (c * 255.0).round().clamp(0.0, 255.0) as u8;
        let item = HFItem::Text {
            anchor: self.align,
            dx: 0.0,
            dy: 0.0,
            text: self.text.clone(),
            font_ref: None,
            size: self.font_size as f32,
            color: [
                to_u8(self.color[0]),
                to_u8(self.color[1]),
                to_u8(self.color[2]),
            ],
            bold: false,
            italic: false,
        };
        let mut zone = HFZone::default();
        if header {
            zone.header.push(item);
        } else {
            zone.footer.push(item);
        }
        let band = self.band_height as f32;
        RunningHeaderFooter {
            default: zone,
            // A blank cover zone when the flat spec hid the first page.
            first_page: (!self.show_on_first_page).then(HFZone::default),
            even_page: None,
            odd_page: None,
            different_first_page: !self.show_on_first_page,
            different_odd_even: false,
            header_band: band,
            footer_band: band,
        }
    }
}

/// Horizontal anchor of a header/footer item within the printable width. An
/// alias of [`Align`] so the rich [`HFItem`] and the flat [`HeaderFooterSpec`]
/// share a single spelling of left/center/right (they never drift).
pub type HFAlign = Align;

/// One drawable element of a running header or footer band: a line of **text**
/// or an **image**. Each item is anchored (`anchor`) within the printable width
/// then nudged by `(dx, dy)` points in PDF axes (`+dx` → right, `+dy` → up).
/// All distances are `f32` points (`1pt = 1/72in`).
#[derive(Debug, Clone, PartialEq)]
pub enum HFItem {
    /// A line of text. May contain the bake tokens `{{page}}`, `{{pages}}`,
    /// `{{date}}` and `{{title}}` (substituted at bake time). `font_ref` is an
    /// embedded-font object id (from
    /// [`Document::embed_font`](crate::document::Document::embed_font)); `None`
    /// uses the engine's bundled OFL face (a real embedded font, never base-14).
    /// `color` is RGB `0..=255`.
    Text {
        anchor: HFAlign,
        dx: f32,
        dy: f32,
        text: String,
        font_ref: Option<u32>,
        size: f32,
        color: [u8; 3],
        bold: bool,
        italic: bool,
    },
    /// A raster image, `w`×`h` points. `image_id` keys the image bytes supplied
    /// to the bake (the def itself stores only the id, not the pixels). `opacity`
    /// is `0..=1`.
    Image {
        anchor: HFAlign,
        dx: f32,
        dy: f32,
        w: f32,
        h: f32,
        image_id: u32,
        opacity: f32,
    },
}

impl HFItem {
    /// Append this item as a JSON object to `out` (the shape [`parse`](Self::parse)
    /// reads): `{"type":"text"|"image", "anchor":…, "dx":…, …}`.
    fn write_json(&self, out: &mut String) {
        let anchor_str = |a: HFAlign| match a {
            Align::Left => "left",
            Align::Center => "center",
            Align::Right => "right",
        };
        match self {
            HFItem::Text {
                anchor,
                dx,
                dy,
                text,
                font_ref,
                size,
                color,
                bold,
                italic,
            } => {
                out.push_str("{\"type\":\"text\",\"anchor\":");
                json_escape(anchor_str(*anchor), out);
                out.push_str(&format!(",\"dx\":{dx},\"dy\":{dy},\"text\":"));
                json_escape(text, out);
                match font_ref {
                    Some(r) => out.push_str(&format!(",\"fontRef\":{r}")),
                    None => out.push_str(",\"fontRef\":null"),
                }
                out.push_str(&format!(
                    ",\"size\":{size},\"color\":[{},{},{}],\"bold\":{bold},\"italic\":{italic}}}",
                    color[0], color[1], color[2]
                ));
            }
            HFItem::Image {
                anchor,
                dx,
                dy,
                w,
                h,
                image_id,
                opacity,
            } => {
                out.push_str("{\"type\":\"image\",\"anchor\":");
                json_escape(anchor_str(*anchor), out);
                out.push_str(&format!(
                    ",\"dx\":{dx},\"dy\":{dy},\"w\":{w},\"h\":{h},\"imageId\":{image_id},\"opacity\":{opacity}}}"
                ));
            }
        }
    }

    /// Read one item object at the cursor. The `"type"` key (`"image"` vs
    /// anything else → text) selects the variant; absent fields take sensible
    /// defaults. `None` only on malformed JSON.
    fn parse(p: &mut ObjReader) -> Option<HFItem> {
        let mut kind = String::new();
        let mut anchor = Align::Left;
        let (mut dx, mut dy) = (0.0f32, 0.0f32);
        let mut text = String::new();
        let mut font_ref: Option<u32> = None;
        let mut size = 10.0f32;
        let mut color = [0u8; 3];
        let (mut bold, mut italic) = (false, false);
        let (mut w, mut h) = (0.0f32, 0.0f32);
        let mut image_id = 0u32;
        let mut opacity = 1.0f32;
        p.object(|p, key| {
            match key {
                "type" => kind = p.string()?,
                "anchor" => anchor = Align::from_str_lossy(&p.string()?),
                "dx" => dx = p.number()? as f32,
                "dy" => dy = p.number()? as f32,
                "text" => text = p.string()?,
                "fontRef" | "font_ref" => {
                    if p.peek()? == b'n' {
                        p.null()?;
                        font_ref = None;
                    } else {
                        font_ref = Some(p.number()? as u32);
                    }
                }
                "size" => size = p.number()? as f32,
                "color" => {
                    let nums = p.number_array()?;
                    if nums.len() >= 3 {
                        color = [clamp_u8(nums[0]), clamp_u8(nums[1]), clamp_u8(nums[2])];
                    }
                }
                "bold" => bold = p.boolean()?,
                "italic" => italic = p.boolean()?,
                "w" => w = p.number()? as f32,
                "h" => h = p.number()? as f32,
                "imageId" | "image_id" => image_id = p.number()? as u32,
                "opacity" => opacity = p.number()? as f32,
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        Some(if kind == "image" {
            HFItem::Image {
                anchor,
                dx,
                dy,
                w,
                h,
                image_id,
                opacity,
            }
        } else {
            HFItem::Text {
                anchor,
                dx,
                dy,
                text,
                font_ref,
                size,
                color,
                bold,
                italic,
            }
        })
    }
}

/// Clamp/round a JSON number to a `u8` colour channel (`0..=255`).
fn clamp_u8(n: f64) -> u8 {
    n.round().clamp(0.0, 255.0) as u8
}

/// The header and footer item lists of one running-H/F **zone** (a page class:
/// the default, the first page, even pages, or odd pages). Either list may be
/// empty (that band is not drawn for the zone).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HFZone {
    /// Items drawn in the top (header) band.
    pub header: Vec<HFItem>,
    /// Items drawn in the bottom (footer) band.
    pub footer: Vec<HFItem>,
}

impl HFZone {
    /// Append this zone as a JSON object `{"header":[…],"footer":[…]}` to `out`.
    fn write_json(&self, out: &mut String) {
        out.push_str("{\"header\":[");
        for (i, it) in self.header.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            it.write_json(out);
        }
        out.push_str("],\"footer\":[");
        for (i, it) in self.footer.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            it.write_json(out);
        }
        out.push_str("]}");
    }

    /// Read a zone object at the cursor (`{"header":[…],"footer":[…]}`); absent
    /// lists default to empty. `None` only on malformed JSON.
    fn parse(p: &mut ObjReader) -> Option<HFZone> {
        let mut zone = HFZone::default();
        p.object(|p, key| {
            match key {
                "header" => p.array(|p| {
                    zone.header.push(HFItem::parse(p)?);
                    Some(())
                })?,
                "footer" => p.array(|p| {
                    zone.footer.push(HFItem::parse(p)?);
                    Some(())
                })?,
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        Some(zone)
    }

    /// Read an optional zone (`null` → `None`).
    fn parse_opt(p: &mut ObjReader) -> Option<Option<HFZone>> {
        if p.peek()? == b'n' {
            p.null()?;
            Some(None)
        } else {
            Some(Some(HFZone::parse(p)?))
        }
    }
}

/// A rich, Word-like running header/footer **definition**: per-page-class zones
/// of [`HFItem`]s plus the band geometry. This is the **source of truth** stored
/// in the GigaPDF editor-metadata sidecar; the bake
/// ([`Document::set_running_header_footer`](crate::document::Document::set_running_header_footer))
/// regenerates the *visible* `/GPHF` marked-content band from it.
///
/// Zone selection per 1-based page: page 1 → `first_page` when
/// `different_first_page` (else `default`); otherwise, when `different_odd_even`,
/// even pages → `even_page` and odd pages → `odd_page` (each falling back to
/// `default` when its zone is `None`); otherwise `default` everywhere.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningHeaderFooter {
    /// The zone used by every page not overridden by a more specific zone.
    pub default: HFZone,
    /// First-page override (used when [`different_first_page`](Self::different_first_page)).
    pub first_page: Option<HFZone>,
    /// Even-page override (used when [`different_odd_even`](Self::different_odd_even)).
    pub even_page: Option<HFZone>,
    /// Odd-page override (used when [`different_odd_even`](Self::different_odd_even)).
    pub odd_page: Option<HFZone>,
    /// Give page 1 its own [`first_page`](Self::first_page) zone.
    pub different_first_page: bool,
    /// Give even/odd pages their own [`even_page`](Self::even_page) /
    /// [`odd_page`](Self::odd_page) zones.
    pub different_odd_even: bool,
    /// Header band height (points): the distance from the top edge that the
    /// header baseline sits at.
    pub header_band: f32,
    /// Footer band height (points): the distance from the bottom edge that the
    /// footer baseline sits at.
    pub footer_band: f32,
}

impl Default for RunningHeaderFooter {
    /// Empty zones, no per-page overrides, 36pt (0.5") bands.
    fn default() -> Self {
        Self {
            default: HFZone::default(),
            first_page: None,
            even_page: None,
            odd_page: None,
            different_first_page: false,
            different_odd_even: false,
            header_band: 36.0,
            footer_band: 36.0,
        }
    }
}

impl RunningHeaderFooter {
    /// The zone effective for the 1-based `page` (see the type docs for the
    /// selection rule). Borrows from `self`.
    pub fn zone_for(&self, page: usize) -> &HFZone {
        if self.different_first_page && page == 1 {
            return self.first_page.as_ref().unwrap_or(&self.default);
        }
        if self.different_odd_even {
            return if page.is_multiple_of(2) {
                self.even_page.as_ref().unwrap_or(&self.default)
            } else {
                self.odd_page.as_ref().unwrap_or(&self.default)
            };
        }
        &self.default
    }

    /// Serialise to a compact JSON object (the inverse of [`from_json`](Self::from_json)).
    /// Hand-rolled, zero-dependency — the shape the SDK/WASM layer exchanges and
    /// the sidecar stores under its `headerFooter` key.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"default\":");
        self.default.write_json(&mut out);
        let opt = |k: &str, z: &Option<HFZone>, out: &mut String| {
            out.push_str(k);
            match z {
                Some(zone) => zone.write_json(out),
                None => out.push_str("null"),
            }
        };
        opt(",\"firstPage\":", &self.first_page, &mut out);
        opt(",\"evenPage\":", &self.even_page, &mut out);
        opt(",\"oddPage\":", &self.odd_page, &mut out);
        out.push_str(&format!(
            ",\"differentFirstPage\":{},\"differentOddEven\":{},\"headerBand\":{},\"footerBand\":{}}}",
            self.different_first_page, self.different_odd_even, self.header_band, self.footer_band
        ));
        out
    }

    /// Parse a JSON object into a definition, filling absent fields from
    /// [`Default`]. Recognised keys: `default`, `firstPage`/`first_page`,
    /// `evenPage`/`even_page`, `oddPage`/`odd_page` (each a zone or `null`),
    /// `differentFirstPage`/`different_first_page`,
    /// `differentOddEven`/`different_odd_even` (bools), `headerBand`/`header_band`,
    /// `footerBand`/`footer_band` (numbers). `None` only on malformed JSON or
    /// trailing junk (so a host gets a clear error, never a silently-wrong def).
    pub fn from_json(s: &str) -> Option<Self> {
        let mut def = RunningHeaderFooter::default();
        let mut p = ObjReader::new(s);
        p.object(|p, key| {
            match key {
                "default" => def.default = HFZone::parse(p)?,
                "firstPage" | "first_page" => def.first_page = HFZone::parse_opt(p)?,
                "evenPage" | "even_page" => def.even_page = HFZone::parse_opt(p)?,
                "oddPage" | "odd_page" => def.odd_page = HFZone::parse_opt(p)?,
                "differentFirstPage" | "different_first_page" => {
                    def.different_first_page = p.boolean()?
                }
                "differentOddEven" | "different_odd_even" => {
                    def.different_odd_even = p.boolean()?
                }
                "headerBand" | "header_band" => def.header_band = p.number()? as f32,
                "footerBand" | "footer_band" => def.footer_band = p.number()? as f32,
                _ => p.skip_value()?,
            }
            Some(())
        })?;
        p.ws();
        if p.done() {
            Some(def)
        } else {
            None
        }
    }
}

/// Append a JSON-escaped string literal (`"…"`) to `out`. Same escaping as the
/// crate's other JSON emitters (`model::json`, the WASM layer). `pub(crate)` so
/// other features (e.g. `CollectionConfig::to_json`) reuse the same escaping.
pub(crate) fn json_escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
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

/// The running header and footer currently baked into a PDF, as recovered by
/// [`Document::header_footer`](crate::document::Document::header_footer). Each
/// side is `Some` only when a `/GPHF`-tagged span is present on the page(s);
/// `None` means no baked header (resp. footer) was found.
///
/// The recovered [`HeaderFooterSpec`] carries the **text** faithfully (the
/// important field for reflecting document state — e.g. a Word-like editor
/// toggle); `align`, `font_size`, `color`, etc. are best-effort defaults, since
/// the bake stores only the drawn text, not the original spec.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeaderFooter {
    /// The recovered running header, or `None` when no baked header is present.
    pub header: Option<HeaderFooterSpec>,
    /// The recovered running footer, or `None` when no baked footer is present.
    pub footer: Option<HeaderFooterSpec>,
}

impl HeaderFooter {
    /// Serialise to JSON `{"header":<spec|null>,"footer":<spec|null>}`, each side
    /// using [`HeaderFooterSpec::to_json`] (or `null`). Hand-rolled,
    /// zero-dependency; the shape consumed by the SDK reader.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"header\":");
        match &self.header {
            Some(h) => out.push_str(&h.to_json()),
            None => out.push_str("null"),
        }
        out.push_str(",\"footer\":");
        match &self.footer {
            Some(f) => out.push_str(&f.to_json()),
            None => out.push_str("null"),
        }
        out.push('}');
        out
    }
}

/// A minimal, tolerant JSON reader for a **flat** object: strings, numbers,
/// booleans, `null`, and number arrays. Object/array *values* it does not need
/// are skipped structurally. Zero-dependency, in the same spirit as
/// [`convert::grids`](crate::convert)'s `Reader`.
/// A minimal, zero-dependency JSON reader for small config objects. `pub(crate)`
/// so other features (e.g. `InfoFields::from_json`) can reuse it.
pub(crate) struct ObjReader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> ObjReader<'a> {
    pub(crate) fn new(s: &'a str) -> Self {
        Self {
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn done(&self) -> bool {
        self.i >= self.b.len()
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    pub(crate) fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        if self.peek()? == c {
            self.i += 1;
            Some(())
        } else {
            None
        }
    }

    /// `{ "key": value (, "key": value)* }` — invokes `member(self, key)` for
    /// each pair, which must consume exactly that pair's value.
    pub(crate) fn object(
        &mut self,
        mut member: impl FnMut(&mut Self, &str) -> Option<()>,
    ) -> Option<()> {
        self.eat(b'{')?;
        if self.peek()? == b'}' {
            self.i += 1;
            return Some(());
        }
        loop {
            let key = self.string()?;
            self.eat(b':')?;
            member(self, &key)?;
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// `[ value (, value)* ]` — invokes `element(self)` for each item, which must
    /// consume exactly that element's value. The structural counterpart of
    /// [`object`](Self::object) for parsing arrays of non-number values
    /// (e.g. an array of objects).
    pub(crate) fn array(&mut self, mut element: impl FnMut(&mut Self) -> Option<()>) -> Option<()> {
        self.eat(b'[')?;
        if self.peek()? == b']' {
            self.i += 1;
            return Some(());
        }
        loop {
            element(self)?;
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// A JSON string. Standard escapes (`\" \\ \/ \n \r \t \b \f \uXXXX`,
    /// surrogate pairs) are decoded; UTF-8 bytes pass through.
    pub(crate) fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(buf).ok(),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => buf.push(b'"'),
                        b'\\' => buf.push(b'\\'),
                        b'/' => buf.push(b'/'),
                        b'n' => buf.push(b'\n'),
                        b'r' => buf.push(b'\r'),
                        b't' => buf.push(b'\t'),
                        b'b' => buf.push(0x08),
                        b'f' => buf.push(0x0C),
                        b'u' => {
                            let ch = self.unicode_escape()?;
                            let mut tmp = [0u8; 4];
                            buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                        }
                        _ => return None,
                    }
                }
                _ => buf.push(c),
            }
        }
    }

    fn unicode_escape(&mut self) -> Option<char> {
        let hi = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&hi) {
            if self.b.get(self.i) != Some(&b'\\') || self.b.get(self.i + 1) != Some(&b'u') {
                return None;
            }
            self.i += 2;
            let lo = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&lo) {
                return None;
            }
            let cp = 0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32;
            char::from_u32(cp)
        } else {
            char::from_u32(hi as u32)
        }
    }

    fn hex4(&mut self) -> Option<u16> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let d = *self.b.get(self.i)?;
            self.i += 1;
            let nibble = match d {
                b'0'..=b'9' => d - b'0',
                b'a'..=b'f' => d - b'a' + 10,
                b'A'..=b'F' => d - b'A' + 10,
                _ => return None,
            };
            v = (v << 4) | nibble as u16;
        }
        Some(v)
    }

    /// A JSON number (integer or float, incl. sign / exponent).
    pub(crate) fn number(&mut self) -> Option<f64> {
        self.ws();
        let start = self.i;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E' => self.i += 1,
                _ => break,
            }
        }
        if self.i == start {
            return None;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()?
            .parse()
            .ok()
    }

    /// `[ number (, number)* ]`.
    pub(crate) fn number_array(&mut self) -> Option<Vec<f64>> {
        self.eat(b'[')?;
        let mut out = Vec::new();
        if self.peek()? == b']' {
            self.i += 1;
            return Some(out);
        }
        loop {
            out.push(self.number()?);
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(out);
                }
                _ => return None,
            }
        }
    }

    fn boolean(&mut self) -> Option<bool> {
        match self.peek()? {
            b't' => {
                self.expect(b"true")?;
                Some(true)
            }
            b'f' => {
                self.expect(b"false")?;
                Some(false)
            }
            _ => None,
        }
    }

    fn null(&mut self) -> Option<()> {
        self.ws();
        self.expect(b"null")
    }

    fn expect(&mut self, word: &[u8]) -> Option<()> {
        self.ws();
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Some(())
        } else {
            None
        }
    }

    /// Consume and discard the value at the cursor (any JSON value), so unknown
    /// object keys don't abort the parse.
    pub(crate) fn skip_value(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => {
                self.string()?;
            }
            b'{' => {
                self.object(|p, _| p.skip_value())?;
            }
            b'[' => {
                self.eat(b'[')?;
                if self.peek()? == b']' {
                    self.i += 1;
                    return Some(());
                }
                loop {
                    self.skip_value()?;
                    match self.peek()? {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            b't' | b'f' => {
                self.boolean()?;
            }
            b'n' => {
                self.null()?;
            }
            _ => {
                self.number()?;
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_text_substitutes_tokens() {
        let spec = HeaderFooterSpec {
            text: "Doc {{page}}/{{pages}}".into(),
            ..Default::default()
        };
        assert_eq!(spec.render_text(2, 5), "Doc 2/5");
    }

    #[test]
    fn from_json_full_object() {
        let spec = HeaderFooterSpec::from_json(
            r#"{"text":"P {{page}}","align":"right","fontSize":12,
               "color":[1,0,0],"pageRange":[2,4],"showOnFirstPage":false,
               "bandHeight":40}"#,
        )
        .unwrap();
        assert_eq!(spec.text, "P {{page}}");
        assert_eq!(spec.align, Align::Right);
        assert_eq!(spec.font_size, 12.0);
        assert_eq!(spec.color, [1.0, 0.0, 0.0]);
        assert_eq!(spec.page_range, Some((2, 4)));
        assert!(!spec.show_on_first_page);
        assert_eq!(spec.band_height, 40.0);
    }

    #[test]
    fn from_json_defaults_and_null_range() {
        let spec = HeaderFooterSpec::from_json(r#"{"text":"x","pageRange":null}"#).unwrap();
        assert_eq!(spec.text, "x");
        assert_eq!(spec.align, Align::Left);
        assert_eq!(spec.page_range, None);
        assert!(spec.show_on_first_page);
    }

    #[test]
    fn from_json_ignores_unknown_keys() {
        let spec = HeaderFooterSpec::from_json(r#"{"extra":{"a":[1,2]},"text":"y"}"#).unwrap();
        assert_eq!(spec.text, "y");
    }

    #[test]
    fn from_json_rejects_garbage() {
        assert!(HeaderFooterSpec::from_json("not json").is_none());
        assert!(HeaderFooterSpec::from_json(r#"{"text":"x"} trailing"#).is_none());
    }

    #[test]
    fn align_from_str_lossy() {
        assert_eq!(Align::from_str_lossy("CENTER"), Align::Center);
        assert_eq!(Align::from_str_lossy(" right "), Align::Right);
        assert_eq!(Align::from_str_lossy("weird"), Align::Left);
    }

    #[test]
    fn spec_to_json_round_trips_through_from_json() {
        let spec = HeaderFooterSpec {
            text: "Doc \"{{page}}\"\n\\end".into(),
            align: Align::Center,
            font_size: 11.0,
            color: [0.1, 0.2, 0.3],
            page_range: Some((2, 9)),
            show_on_first_page: false,
            band_height: 24.0,
        };
        let back = HeaderFooterSpec::from_json(&spec.to_json()).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn header_footer_to_json_shapes_both_sides() {
        let hf = HeaderFooter {
            header: Some(HeaderFooterSpec {
                text: "H".into(),
                ..Default::default()
            }),
            footer: None,
        };
        let json = hf.to_json();
        assert!(json.starts_with("{\"header\":{"));
        assert!(json.contains("\"footer\":null"));

        let empty = HeaderFooter::default().to_json();
        assert_eq!(empty, "{\"header\":null,\"footer\":null}");
    }

    fn sample_def() -> RunningHeaderFooter {
        RunningHeaderFooter {
            default: HFZone {
                header: vec![HFItem::Text {
                    anchor: Align::Center,
                    dx: 1.5,
                    dy: -2.0,
                    text: "Doc {{page}}/{{pages}}".into(),
                    font_ref: Some(7),
                    size: 11.0,
                    color: [10, 20, 30],
                    bold: true,
                    italic: false,
                }],
                footer: vec![HFItem::Image {
                    anchor: Align::Right,
                    dx: 0.0,
                    dy: 0.0,
                    w: 40.0,
                    h: 20.0,
                    image_id: 3,
                    opacity: 0.8,
                }],
            },
            first_page: Some(HFZone::default()),
            even_page: Some(HFZone {
                header: vec![HFItem::Text {
                    anchor: Align::Left,
                    dx: 0.0,
                    dy: 0.0,
                    text: "even".into(),
                    font_ref: None,
                    size: 9.0,
                    color: [0, 0, 0],
                    bold: false,
                    italic: true,
                }],
                footer: vec![],
            }),
            odd_page: None,
            different_first_page: true,
            different_odd_even: true,
            header_band: 30.0,
            footer_band: 24.0,
        }
    }

    #[test]
    fn running_header_footer_round_trips_through_json() {
        let def = sample_def();
        let back = RunningHeaderFooter::from_json(&def.to_json()).unwrap();
        assert_eq!(back, def);
    }

    #[test]
    fn running_header_footer_defaults_and_null_zones() {
        let def =
            RunningHeaderFooter::from_json(r#"{"default":{"header":[],"footer":[]}}"#).unwrap();
        assert!(def.first_page.is_none());
        assert!(!def.different_first_page);
        assert_eq!(def.header_band, 36.0);
        assert_eq!(def.footer_band, 36.0);
    }

    #[test]
    fn running_header_footer_rejects_trailing_junk() {
        assert!(RunningHeaderFooter::from_json("not json").is_none());
        assert!(RunningHeaderFooter::from_json(r#"{"default":{}} extra"#).is_none());
    }

    #[test]
    fn zone_for_selects_first_even_odd_and_default() {
        let def = sample_def(); // different_first_page + different_odd_even both on
        assert_eq!(
            def.zone_for(1),
            def.first_page.as_ref().unwrap(),
            "page 1 → first"
        );
        assert_eq!(
            def.zone_for(2),
            def.even_page.as_ref().unwrap(),
            "page 2 → even"
        );
        // odd_page is None → fall back to default for odd pages > 1.
        assert_eq!(
            def.zone_for(3),
            &def.default,
            "page 3 → default (odd fallback)"
        );

        // With both flags off, every page is the default zone.
        let plain = RunningHeaderFooter {
            different_first_page: false,
            different_odd_even: false,
            ..sample_def()
        };
        assert_eq!(plain.zone_for(1), &plain.default);
        assert_eq!(plain.zone_for(2), &plain.default);
    }

    #[test]
    fn hf_item_image_round_trips() {
        let item = HFItem::Image {
            anchor: Align::Center,
            dx: -3.0,
            dy: 4.0,
            w: 50.0,
            h: 25.0,
            image_id: 9,
            opacity: 0.5,
        };
        let mut json = String::new();
        item.write_json(&mut json);
        let mut p = ObjReader::new(&json);
        assert_eq!(HFItem::parse(&mut p).unwrap(), item);
    }

    #[test]
    fn header_footer_spec_lowers_to_running() {
        let spec = HeaderFooterSpec {
            text: "P {{page}}".into(),
            align: Align::Right,
            font_size: 12.0,
            color: [1.0, 0.0, 0.0],
            page_range: Some((2, 4)),
            show_on_first_page: false,
            band_height: 40.0,
        };
        let def = spec.to_running(true);
        assert!(
            def.different_first_page,
            "hidden cover → different first page"
        );
        assert!(
            def.first_page.as_ref().unwrap().header.is_empty(),
            "cover is blank"
        );
        assert_eq!(def.header_band, 40.0);
        assert_eq!(def.default.footer.len(), 0);
        match &def.default.header[..] {
            [HFItem::Text {
                anchor,
                text,
                color,
                size,
                font_ref,
                ..
            }] => {
                assert_eq!(*anchor, Align::Right);
                assert_eq!(text, "P {{page}}");
                assert_eq!(*color, [255, 0, 0]);
                assert_eq!(*size, 12.0);
                assert_eq!(*font_ref, None);
            }
            other => panic!("expected one text item, got {other:?}"),
        }

        // The footer side lowers symmetrically.
        let footer_def = spec.to_running(false);
        assert_eq!(footer_def.default.header.len(), 0);
        assert_eq!(footer_def.default.footer.len(), 1);
    }
}
