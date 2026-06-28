//! From-scratch OOXML **chart** and **SmartArt** lowering — a self-contained
//! mini charting engine plus a diagram-data-model reader, both producing nodes of
//! the unified editable [`model`](crate::model).
//!
//! ## Why this exists
//!
//! A DrawingML chart (`c:chartSpace`) and a SmartArt diagram (`dgm:dataModel`)
//! carry their *data* (series, categories, hierarchy) but their *picture* is laid
//! out by the consuming app, not stored as ready-made geometry. Dropping them, or
//! keeping only a flat list of numbers, loses the visual. This module instead
//! **renders the chart itself** into real model [`Shape`](crate::model::Shape)s
//! (bars, wedges, polylines, axes, a legend) plus text labels, and lowers the
//! chart's numbers to a [`Table`](crate::model::Table) so both the figure *and*
//! the data survive. SmartArt becomes a nested bullet [`List`](crate::model::List)
//! (its hierarchy) and, when the laid-out drawing part is present, its
//! [`Shape`](crate::model::Shape)s too.
//!
//! ## Public API
//!
//! - [`parse_chart`] — a `c:chartSpace` XML string → `Vec<Block>` (a vector
//!   rendering block group followed by a data-`Table` block).
//! - [`parse_smartart`] — a `dgm:dataModel` XML string (+ an optional
//!   `dsp:drawing` XML) → `Vec<Block>` (a nested `List`, then any drawing shapes).
//!
//! Both are **pure parsers**: the Office importer wires them to the OOXML parts.
//! Malformed XML or missing pieces yield `Vec::new()` (or whatever parsed) —
//! never a panic.
//!
//! ## Coordinate convention
//!
//! Identical to the rest of the model: a [`Block::frame`](crate::model::Block) is
//! a top-left / Y-down [`Rect`](crate::model::Rect) in **PDF points**, while a
//! [`Shape`](crate::model::Shape)'s `segments` are **box-local, Y-up** (origin at
//! the frame's bottom-left, `0..w` × `0..h`) — exactly what the ODF/PPTX shape
//! lowering emits and what every exporter ([`web`](super::web),
//! [`export_model`](super::export_model)) flips back. Every chart shape here uses
//! the shared canvas [`Rect::new(0.0, 0.0, CHART_W, CHART_H)`] as its frame and
//! draws in that canvas's Y-up space.

use crate::content::vector::PathSeg;
use crate::model::style::Align;
use crate::model::{
    Block, BlockKind, CharStyle, Inline, InlineRun, List, ListItem, ListMarker, Paragraph,
    ParagraphStyle, Rect, Row, Shape, Table,
};

// ─────────────────────────────── canvas geometry ──────────────────────────────

/// Overall chart canvas width in points (a sensible default figure size).
const CHART_W: f64 = 480.0;
/// Overall chart canvas height in points.
const CHART_H: f64 = 300.0;

/// Left margin of the plot area (room for the value-axis tick labels).
const MARGIN_LEFT: f64 = 46.0;
/// Right margin of the plot area.
const MARGIN_RIGHT: f64 = 12.0;
/// Top margin of the plot area (room for the title).
const MARGIN_TOP: f64 = 28.0;
/// Bottom margin of the plot area (room for the category labels + the legend).
const MARGIN_BOTTOM: f64 = 54.0;

/// Height reserved at the very bottom for the legend strip.
const LEGEND_H: f64 = 22.0;

/// Default qualitative palette (Office-like). Series `i` uses `PALETTE[i % len]`.
/// Components are RGB in `0.0..=1.0`.
const PALETTE: [[f64; 3]; 10] = [
    [0.271, 0.471, 0.741], // blue
    [0.918, 0.486, 0.196], // orange
    [0.643, 0.643, 0.643], // grey
    [1.000, 0.753, 0.000], // gold
    [0.357, 0.608, 0.835], // light blue
    [0.439, 0.678, 0.278], // green
    [0.498, 0.310, 0.612], // purple
    [0.749, 0.349, 0.357], // brick
    [0.388, 0.706, 0.804], // teal
    [0.918, 0.682, 0.220], // amber
];

/// A near-black for axis lines and labels.
const AXIS_RGB: [f64; 3] = [0.20, 0.20, 0.20];
/// A light grey for the plot grid lines.
const GRID_RGB: [f64; 3] = [0.80, 0.80, 0.80];

/// The palette colour for series index `i`.
fn series_color(i: usize) -> [f64; 3] {
    PALETTE[i % PALETTE.len()]
}

// ───────────────────────────── parsed chart model ─────────────────────────────

/// The kind of plot detected in a `c:plotArea`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ChartKind {
    /// `c:barChart` with `c:barDir` = `col` (vertical columns).
    Column,
    /// `c:barChart` with `c:barDir` = `bar` (horizontal bars).
    Bar,
    #[default]
    /// `c:lineChart`.
    Line,
    /// `c:pieChart` / `c:doughnutChart` (doughnut sets `inner > 0`).
    Pie,
    /// `c:areaChart`.
    Area,
    /// `c:scatterChart`.
    Scatter,
    /// `c:radarChart`.
    Radar,
}

/// Grouping of a bar/area chart (`c:grouping`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Grouping {
    #[default]
    Clustered,
    Stacked,
    /// `percentStacked` — treated like stacked but normalised per category.
    PercentStacked,
}

/// One data series.
#[derive(Debug, Clone, Default)]
struct Series {
    /// Series display name (`c:tx`), if any.
    name: String,
    /// Category labels (`c:cat`) — may be empty (then indices are used).
    categories: Vec<String>,
    /// Y / value numbers (`c:val` or, for scatter, `c:yVal`).
    values: Vec<f64>,
    /// X numbers for a scatter series (`c:xVal`); empty for non-scatter.
    x_values: Vec<f64>,
}

/// The fully-parsed chart.
#[derive(Debug, Clone, Default)]
struct Chart {
    kind: ChartKind,
    grouping: Grouping,
    /// Doughnut inner-radius fraction (`0.0` = a solid pie).
    inner_fraction: f64,
    series: Vec<Series>,
    /// Optional chart title text.
    title: String,
    /// Whether a `c:legend` element was present.
    has_legend: bool,
    /// Category-axis title text, if any.
    cat_axis_title: String,
    /// Value-axis title text, if any.
    val_axis_title: String,
}

impl Chart {
    /// Category labels for the chart: the longest series' categories, falling back
    /// to `1..=n` strings when none are cached.
    fn categories(&self) -> Vec<String> {
        let cats = self
            .series
            .iter()
            .map(|s| &s.categories)
            .max_by_key(|c| c.len())
            .cloned()
            .unwrap_or_default();
        if !cats.is_empty() {
            return cats;
        }
        let n = self
            .series
            .iter()
            .map(|s| s.values.len())
            .max()
            .unwrap_or(0);
        (1..=n).map(|i| i.to_string()).collect()
    }

    /// Number of category slots.
    fn cat_count(&self) -> usize {
        self.series
            .iter()
            .map(|s| s.values.len())
            .max()
            .unwrap_or(0)
    }
}

// ──────────────────────────────── public API ─────────────────────────────────

/// Parse a DrawingML `c:chartSpace` document and lower it to model blocks: a
/// **native vector rendering** of the chart (axes, plotted series, legend) as a
/// group of [`Shape`](crate::model::Shape)/text blocks, followed by a
/// [`Table`](crate::model::Table) of the underlying numbers. Returns `Vec::new()`
/// when nothing parses (no series, malformed XML).
pub fn parse_chart(chart_xml: &str) -> Vec<Block> {
    let chart = parse_chart_xml(chart_xml);
    if chart.series.is_empty() || chart.cat_count() == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    render_chart(&chart, &mut out);
    if let Some(table) = chart_data_table(&chart) {
        out.push(table_block(table));
    }
    out
}

/// Parse a SmartArt diagram **data model** (`dgm:dataModel`) into a nested bullet
/// [`List`](crate::model::List) reflecting the point hierarchy (depth from the
/// `dgm:cxnLst` parent/child connections). When `drawing_xml` (the laid-out
/// `dsp:drawing` part) is given, its `dsp:sp` shapes + text are **also** lowered
/// to [`Shape`](crate::model::Shape)/text blocks so the visual survives. Returns
/// whatever parsed (possibly empty) — never panics.
pub fn parse_smartart(data_xml: &str, drawing_xml: Option<&str>) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(list) = smartart_list(data_xml) {
        out.push(Block {
            kind: BlockKind::List(list),
            ..Block::default()
        });
    }
    if let Some(dxml) = drawing_xml {
        smartart_drawing_shapes(dxml, &mut out);
    }
    out
}

// ───────────────────────────────── XML reader ─────────────────────────────────
//
// A minimal, self-contained streaming XML tokenizer (this module must not reach
// into `office_import.rs`'s private parser). Handles start/end/self-closing tags,
// attributes, text, and skips comments / PIs / declarations / CDATA framing.

/// An XML token.
#[derive(Debug, Clone)]
enum Tok {
    /// `<name …>` — `(name, attrs, self_closing)`.
    Open(String, Vec<(String, String)>, bool),
    /// `</name>`.
    Close(String),
    /// Character data between tags (entity-decoded).
    Text(String),
}

/// A tiny pull parser over an XML string.
struct Xml<'a> {
    src: &'a str,
    b: &'a [u8],
    i: usize,
}

impl<'a> Xml<'a> {
    fn new(src: &'a str) -> Xml<'a> {
        Xml {
            src,
            b: src.as_bytes(),
            i: 0,
        }
    }

    fn next(&mut self) -> Option<Tok> {
        if self.i >= self.b.len() {
            return None;
        }
        if self.b[self.i] == b'<' {
            // Comment.
            if self.src[self.i..].starts_with("<!--") {
                self.i = self.src[self.i..]
                    .find("-->")
                    .map(|j| self.i + j + 3)
                    .unwrap_or(self.b.len());
                return self.next();
            }
            // CDATA — surface its contents as text.
            if self.src[self.i..].starts_with("<![CDATA[") {
                let start = self.i + 9;
                let end = self.src[start..]
                    .find("]]>")
                    .map(|j| start + j)
                    .unwrap_or(self.b.len());
                let text = self.src[start..end].to_string();
                self.i = (end + 3).min(self.b.len());
                if text.is_empty() {
                    return self.next();
                }
                return Some(Tok::Text(text));
            }
            // Declaration / PI / doctype.
            if matches!(self.b.get(self.i + 1), Some(b'!') | Some(b'?')) {
                self.i = self.src[self.i..]
                    .find('>')
                    .map(|j| self.i + j + 1)
                    .unwrap_or(self.b.len());
                return self.next();
            }
            // End tag.
            if self.b.get(self.i + 1) == Some(&b'/') {
                let end = self.src[self.i..]
                    .find('>')
                    .map(|j| self.i + j)
                    .unwrap_or(self.b.len());
                let name = self.src[self.i + 2..end].trim().to_string();
                self.i = (end + 1).min(self.b.len());
                return Some(Tok::Close(name));
            }
            // Start tag.
            let end = match self.src[self.i..].find('>') {
                Some(j) => self.i + j,
                None => {
                    self.i = self.b.len();
                    return None;
                }
            };
            let raw = &self.src[self.i + 1..end];
            self.i = end + 1;
            let self_closing = raw.trim_end().ends_with('/');
            let raw = raw.trim_end().trim_end_matches('/');
            let (name, attrs) = parse_start(raw);
            if name.is_empty() {
                return self.next();
            }
            Some(Tok::Open(name, attrs, self_closing))
        } else {
            let end = self.src[self.i..]
                .find('<')
                .map(|j| self.i + j)
                .unwrap_or(self.b.len());
            let text = unescape(&self.src[self.i..end]);
            self.i = end;
            if text.is_empty() {
                return self.next();
            }
            Some(Tok::Text(text))
        }
    }
}

/// Split a start-tag body into `(name, attrs)`.
fn parse_start(raw: &str) -> (String, Vec<(String, String)>) {
    let raw = raw.trim();
    let mut name_end = raw.len();
    for (i, c) in raw.char_indices() {
        if c.is_whitespace() {
            name_end = i;
            break;
        }
    }
    let name = raw[..name_end].to_string();
    let mut attrs = Vec::new();
    let b = raw.as_bytes();
    let mut i = name_end;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let ns = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' {
            i += 1;
        }
        let an = raw[ns..i].to_string();
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut av = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                i += 1;
                let vs = i;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
                av = unescape(&raw[vs..i.min(raw.len())]);
                i = (i + 1).min(b.len());
            } else {
                let vs = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                av = unescape(&raw[vs..i]);
            }
        }
        if !an.is_empty() {
            attrs.push((an, av));
        }
    }
    (name, attrs)
}

/// The local name of a namespaced tag (`c:barChart` → `barChart`).
fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Look up an attribute by local name (namespace prefix ignored).
fn attr<'b>(attrs: &'b [(String, String)], name: &str) -> Option<&'b str> {
    attrs
        .iter()
        .find(|(k, _)| local(k).eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Decode the five predefined XML entities plus numeric (`&#NN;` / `&#xHH;`)
/// references. Self-contained so the module has no cross-file coupling.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        if let Some(semi) = tail.find(';') {
            let ent = &tail[1..semi];
            let decoded = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            match decoded {
                Some(c) => {
                    out.push(c);
                    rest = &tail[semi + 1..];
                }
                None => {
                    out.push('&');
                    rest = &tail[1..];
                }
            }
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

// ─────────────────────────────── chart XML → model ────────────────────────────

/// Walk a `c:chartSpace` and build the [`Chart`]. Tolerant: unknown elements are
/// ignored, and a partial document yields a partial chart.
fn parse_chart_xml(xml: &str) -> Chart {
    let mut chart = Chart::default();
    let mut x = Xml::new(xml);

    // Streaming context. `kind` is set the first time a plot element is seen.
    let mut kind_set = false;

    // Title accumulation (`c:title` … `a:t` text). We only keep the chart title,
    // and axis titles, distinguished by which container we are inside.
    #[derive(PartialEq, Clone, Copy)]
    enum TitleScope {
        None,
        Chart,
        CatAxis,
        ValAxis,
    }
    let mut title_scope = TitleScope::None;
    let mut in_title = false; // inside any `c:title`
    let mut title_buf = String::new();

    // Axis discrimination: we are inside a `c:catAx` or `c:valAx` block.
    let mut axis_scope = TitleScope::None;

    // Series accumulation.
    let mut in_ser = false;
    let mut ser = Series::default();
    // Which numeric field of the series the current cache feeds.
    #[derive(PartialEq, Clone, Copy)]
    enum Field {
        None,
        SerTx,
        Cat,
        Val,
        XVal,
        YVal,
    }
    let mut field = Field::None;
    // `c:tx` may wrap a `c:strRef/c:v` (series name) — track to avoid treating its
    // inner `c:v` as a value.
    let mut depth_in_v = false;
    let mut v_buf = String::new();
    // Number of strRef/numRef nesting for the current field (so a stray `c:v`
    // outside cat/val isn't captured).

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    // Plot type — set once.
                    "barChart" if !kind_set => {
                        // Default to Column; a later `c:barDir` may flip to Bar.
                        chart.kind = ChartKind::Column;
                        kind_set = true;
                    }
                    "lineChart" if !kind_set => {
                        chart.kind = ChartKind::Line;
                        kind_set = true;
                    }
                    "pieChart" if !kind_set => {
                        chart.kind = ChartKind::Pie;
                        kind_set = true;
                    }
                    "doughnutChart" if !kind_set => {
                        chart.kind = ChartKind::Pie;
                        chart.inner_fraction = 0.5;
                        kind_set = true;
                    }
                    "areaChart" if !kind_set => {
                        chart.kind = ChartKind::Area;
                        kind_set = true;
                    }
                    "scatterChart" if !kind_set => {
                        chart.kind = ChartKind::Scatter;
                        kind_set = true;
                    }
                    "radarChart" if !kind_set => {
                        chart.kind = ChartKind::Radar;
                        kind_set = true;
                    }
                    "barDir" => {
                        if let Some(v) = attr(&attrs, "val") {
                            chart.kind = if v.eq_ignore_ascii_case("bar") {
                                ChartKind::Bar
                            } else {
                                ChartKind::Column
                            };
                        }
                    }
                    "grouping" => {
                        if let Some(v) = attr(&attrs, "val") {
                            chart.grouping = match v {
                                "stacked" => Grouping::Stacked,
                                "percentStacked" => Grouping::PercentStacked,
                                _ => Grouping::Clustered,
                            };
                        }
                    }
                    "holeSize" => {
                        if let Some(v) = attr(&attrs, "val").and_then(|s| s.parse::<f64>().ok()) {
                            chart.inner_fraction = (v / 100.0).clamp(0.0, 0.95);
                        }
                    }
                    "legend" => chart.has_legend = true,
                    "catAx" if !sc => axis_scope = TitleScope::CatAxis,
                    "valAx" if !sc => axis_scope = TitleScope::ValAxis,
                    "title" if !sc => {
                        in_title = true;
                        title_buf.clear();
                        title_scope = match axis_scope {
                            TitleScope::CatAxis => TitleScope::CatAxis,
                            TitleScope::ValAxis => TitleScope::ValAxis,
                            _ => TitleScope::Chart,
                        };
                    }
                    "ser" if !sc => {
                        in_ser = true;
                        ser = Series::default();
                    }
                    "tx" if in_ser => field = Field::SerTx,
                    "cat" if in_ser => field = Field::Cat,
                    "val" if in_ser => field = Field::Val,
                    "xVal" if in_ser => field = Field::XVal,
                    "yVal" if in_ser => field = Field::YVal,
                    "v" if !sc => {
                        depth_in_v = true;
                        v_buf.clear();
                    }
                    _ => {}
                }
            }
            Tok::Text(t) => {
                if depth_in_v {
                    v_buf.push_str(&t);
                } else if in_title {
                    title_buf.push_str(&t);
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                match ln {
                    "v" => {
                        depth_in_v = false;
                        let raw = v_buf.trim();
                        if !raw.is_empty() {
                            match field {
                                Field::SerTx => {
                                    if ser.name.is_empty() {
                                        ser.name = raw.to_string();
                                    }
                                }
                                Field::Cat => ser.categories.push(raw.to_string()),
                                Field::Val | Field::YVal => {
                                    ser.values.push(raw.parse::<f64>().unwrap_or(0.0))
                                }
                                Field::XVal => ser.x_values.push(raw.parse::<f64>().unwrap_or(0.0)),
                                Field::None => {}
                            }
                        }
                    }
                    "tx" | "cat" | "val" | "xVal" | "yVal" => field = Field::None,
                    "ser" => {
                        in_ser = false;
                        chart.series.push(std::mem::take(&mut ser));
                    }
                    "catAx" => axis_scope = TitleScope::None,
                    "valAx" => axis_scope = TitleScope::None,
                    "title" => {
                        in_title = false;
                        let t = title_buf.trim().to_string();
                        if !t.is_empty() {
                            match title_scope {
                                TitleScope::Chart => {
                                    if chart.title.is_empty() {
                                        chart.title = t;
                                    }
                                }
                                TitleScope::CatAxis => {
                                    if chart.cat_axis_title.is_empty() {
                                        chart.cat_axis_title = t;
                                    }
                                }
                                TitleScope::ValAxis => {
                                    if chart.val_axis_title.is_empty() {
                                        chart.val_axis_title = t;
                                    }
                                }
                                TitleScope::None => {}
                            }
                        }
                        title_scope = TitleScope::None;
                        title_buf.clear();
                    }
                    _ => {}
                }
            }
        }
    }
    chart
}

// ─────────────────────────────── value scaling ────────────────────────────────

/// The `[min, max]` of the plotted values, expanded so a baseline of `0` is
/// always included (charts read against zero). For stacked groupings the per-
/// category stack total is used as the maximum. Returns a non-degenerate range
/// (a flat dataset gets a unit span so axis ticks are still drawn).
fn value_range(chart: &Chart) -> (f64, f64) {
    let mut lo = 0.0_f64;
    let mut hi = 0.0_f64;
    match chart.grouping {
        Grouping::Stacked | Grouping::PercentStacked
            if matches!(
                chart.kind,
                ChartKind::Column | ChartKind::Bar | ChartKind::Area
            ) =>
        {
            let n = chart.cat_count();
            for c in 0..n {
                let mut pos = 0.0;
                let mut neg = 0.0;
                for s in &chart.series {
                    let v = s.values.get(c).copied().unwrap_or(0.0);
                    if v >= 0.0 {
                        pos += v;
                    } else {
                        neg += v;
                    }
                }
                hi = hi.max(pos);
                lo = lo.min(neg);
            }
            if matches!(chart.grouping, Grouping::PercentStacked) {
                // Normalised: each category sums to 1.0 (100%).
                hi = hi.max(1.0);
            }
        }
        _ => {
            for s in &chart.series {
                for &v in &s.values {
                    hi = hi.max(v);
                    lo = lo.min(v);
                }
            }
        }
    }
    if (hi - lo).abs() < f64::EPSILON {
        hi = lo + 1.0;
    }
    (lo, hi)
}

/// A "nice" axis step for a value span and an approximate desired tick count.
/// Rounds the raw step up to the nearest 1/2/5 × 10ⁿ. Always `> 0`.
fn nice_step(span: f64, target_ticks: usize) -> f64 {
    let target = target_ticks.max(1) as f64;
    let raw = (span / target).abs().max(f64::MIN_POSITIVE);
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let step = if norm <= 1.0 {
        1.0
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 5.0 {
        5.0
    } else {
        10.0
    };
    (step * mag).max(f64::MIN_POSITIVE)
}

/// Format a numeric tick label compactly: integers print without a fractional
/// part; non-integers keep up to two decimals, trailing zeros trimmed.
fn fmt_num(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    if (v - v.round()).abs() < 1e-9 {
        return format!("{}", v.round() as i64);
    }
    let mut s = format!("{v:.2}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

// ─────────────────────────── shape & text block helpers ───────────────────────

/// A `Shape` block spanning the chart canvas, with `segments` in canvas-local
/// Y-up space, an optional `fill`, and an optional `stroke`/`width`.
fn shape_block(
    segments: Vec<PathSeg>,
    fill: Option<[f64; 3]>,
    stroke: Option<[f64; 3]>,
    stroke_width: f64,
) -> Block {
    Block {
        frame: Some(Rect::new(0.0, 0.0, CHART_W, CHART_H)),
        kind: BlockKind::Shape(Shape {
            segments,
            fill,
            stroke,
            stroke_width,
            dash: Vec::new(),
        }),
        ..Block::default()
    }
}

/// A filled rectangle `Shape` block in **canvas Y-up** coordinates: `(x, y)` is
/// the lower-left corner, `w`×`h` the size. An optional `stroke` outlines it.
fn rect_shape(
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    fill: Option<[f64; 3]>,
    stroke: Option<[f64; 3]>,
    stroke_width: f64,
) -> Block {
    let segs = vec![
        PathSeg::Move(x, y),
        PathSeg::Line(x + w, y),
        PathSeg::Line(x + w, y + h),
        PathSeg::Line(x, y + h),
        PathSeg::Close,
    ];
    shape_block(segs, fill, stroke, stroke_width)
}

/// A straight line `Shape` block from `(x1, y1)` to `(x2, y2)` (canvas Y-up).
fn line_shape(x1: f64, y1: f64, x2: f64, y2: f64, stroke: [f64; 3], w: f64) -> Block {
    shape_block(
        vec![PathSeg::Move(x1, y1), PathSeg::Line(x2, y2)],
        None,
        Some(stroke),
        w,
    )
}

/// A small text label as a positioned [`Paragraph`] block. `(x, y_top)` is the
/// top-left of the label box in **top-left / Y-down** canvas points (the block
/// frame convention); `align` sets the in-box horizontal alignment.
fn label_block(text: &str, x: f64, y_top: f64, w: f64, size_pt: f64, align: Align) -> Block {
    let h = size_pt * 1.4;
    Block {
        frame: Some(Rect::new(x, y_top, w, h)),
        kind: BlockKind::Paragraph(Paragraph {
            style: ParagraphStyle {
                align,
                ..ParagraphStyle::default()
            },
            runs: vec![Inline::Run(InlineRun {
                text: text.to_string(),
                style: CharStyle {
                    size_pt,
                    color: Some(AXIS_RGB),
                    ..CharStyle::default()
                },
                source_index: None,
            })],
            ..Paragraph::default()
        }),
        ..Block::default()
    }
}

/// Wrap a [`Table`] in a block.
fn table_block(table: Table) -> Block {
    Block {
        kind: BlockKind::Table(table),
        ..Block::default()
    }
}

// ───────────────────────── stroke-font (rotated text paths) ────────────────────
//
// `Paragraph` blocks carry a `frame` but the model has no per-paragraph text
// rotation that the flow exporters honour, so a rotated axis title is drawn as a
// **vector text path**: each glyph is a handful of polylines on a unit cell that
// scale, advance along a baseline, and rotate by an arbitrary angle — rendering
// identically everywhere a `Shape` does. The font is a minimal single-stroke set
// (uppercase A–Z, 0–9, space, and a few separators) sufficient for axis titles
// and ring labels; unknown characters advance as a blank cell.

/// One glyph as a list of polylines (strokes). Each point is on the unit cell
/// `0.0..=1.0` in X (advance) and Y (baseline at `0`, cap height at `1`).
type Glyph = &'static [&'static [(f64, f64)]];

/// The advance width of a glyph cell as a fraction of the cap height.
const GLYPH_ADVANCE: f64 = 0.62;
/// Extra gap between glyph cells, as a fraction of the cap height.
const GLYPH_GAP: f64 = 0.16;

/// Strokes for a character (uppercased). Unknown ⇒ an empty glyph (blank cell).
fn glyph(c: char) -> Glyph {
    match c.to_ascii_uppercase() {
        'A' => &[
            &[(0.0, 0.0), (0.5, 1.0), (1.0, 0.0)],
            &[(0.18, 0.36), (0.82, 0.36)],
        ],
        'B' => &[
            &[
                (0.0, 0.0),
                (0.0, 1.0),
                (0.7, 1.0),
                (0.9, 0.82),
                (0.7, 0.5),
                (0.0, 0.5),
            ],
            &[(0.7, 0.5), (0.95, 0.28), (0.7, 0.0), (0.0, 0.0)],
        ],
        'C' => &[&[(1.0, 0.85), (0.5, 1.0), (0.0, 0.5), (0.5, 0.0), (1.0, 0.15)]],
        'D' => &[&[
            (0.0, 0.0),
            (0.0, 1.0),
            (0.6, 1.0),
            (1.0, 0.5),
            (0.6, 0.0),
            (0.0, 0.0),
        ]],
        'E' => &[
            &[(1.0, 1.0), (0.0, 1.0), (0.0, 0.0), (1.0, 0.0)],
            &[(0.0, 0.5), (0.75, 0.5)],
        ],
        'F' => &[
            &[(1.0, 1.0), (0.0, 1.0), (0.0, 0.0)],
            &[(0.0, 0.5), (0.75, 0.5)],
        ],
        'G' => &[&[
            (1.0, 0.85),
            (0.5, 1.0),
            (0.0, 0.5),
            (0.5, 0.0),
            (1.0, 0.15),
            (1.0, 0.45),
            (0.6, 0.45),
        ]],
        'H' => &[
            &[(0.0, 1.0), (0.0, 0.0)],
            &[(1.0, 1.0), (1.0, 0.0)],
            &[(0.0, 0.5), (1.0, 0.5)],
        ],
        'I' => &[
            &[(0.5, 0.0), (0.5, 1.0)],
            &[(0.2, 1.0), (0.8, 1.0)],
            &[(0.2, 0.0), (0.8, 0.0)],
        ],
        'J' => &[&[(1.0, 1.0), (1.0, 0.2), (0.7, 0.0), (0.3, 0.0), (0.05, 0.25)]],
        'K' => &[
            &[(0.0, 1.0), (0.0, 0.0)],
            &[(1.0, 1.0), (0.0, 0.5), (1.0, 0.0)],
        ],
        'L' => &[&[(0.0, 1.0), (0.0, 0.0), (1.0, 0.0)]],
        'M' => &[&[(0.0, 0.0), (0.0, 1.0), (0.5, 0.4), (1.0, 1.0), (1.0, 0.0)]],
        'N' => &[&[(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0)]],
        'O' => &[&[(0.5, 1.0), (0.0, 0.5), (0.5, 0.0), (1.0, 0.5), (0.5, 1.0)]],
        'P' => &[&[
            (0.0, 0.0),
            (0.0, 1.0),
            (0.8, 1.0),
            (1.0, 0.75),
            (0.8, 0.5),
            (0.0, 0.5),
        ]],
        'Q' => &[
            &[(0.5, 1.0), (0.0, 0.5), (0.5, 0.0), (1.0, 0.5), (0.5, 1.0)],
            &[(0.6, 0.3), (1.0, 0.0)],
        ],
        'R' => &[
            &[
                (0.0, 0.0),
                (0.0, 1.0),
                (0.8, 1.0),
                (1.0, 0.75),
                (0.8, 0.5),
                (0.0, 0.5),
            ],
            &[(0.4, 0.5), (1.0, 0.0)],
        ],
        'S' => &[&[
            (1.0, 0.85),
            (0.5, 1.0),
            (0.1, 0.78),
            (0.9, 0.32),
            (0.5, 0.0),
            (0.0, 0.15),
        ]],
        'T' => &[&[(0.0, 1.0), (1.0, 1.0)], &[(0.5, 1.0), (0.5, 0.0)]],
        'U' => &[&[(0.0, 1.0), (0.0, 0.2), (0.5, 0.0), (1.0, 0.2), (1.0, 1.0)]],
        'V' => &[&[(0.0, 1.0), (0.5, 0.0), (1.0, 1.0)]],
        'W' => &[&[(0.0, 1.0), (0.25, 0.0), (0.5, 0.6), (0.75, 0.0), (1.0, 1.0)]],
        'X' => &[&[(0.0, 1.0), (1.0, 0.0)], &[(0.0, 0.0), (1.0, 1.0)]],
        'Y' => &[
            &[(0.0, 1.0), (0.5, 0.5), (1.0, 1.0)],
            &[(0.5, 0.5), (0.5, 0.0)],
        ],
        'Z' => &[&[(0.0, 1.0), (1.0, 1.0), (0.0, 0.0), (1.0, 0.0)]],
        '0' => &[
            &[(0.5, 1.0), (0.0, 0.5), (0.5, 0.0), (1.0, 0.5), (0.5, 1.0)],
            &[(0.2, 0.2), (0.8, 0.8)],
        ],
        '1' => &[
            &[(0.25, 0.8), (0.5, 1.0), (0.5, 0.0)],
            &[(0.2, 0.0), (0.8, 0.0)],
        ],
        '2' => &[&[(0.0, 0.8), (0.5, 1.0), (0.95, 0.7), (0.0, 0.0), (1.0, 0.0)]],
        '3' => &[&[
            (0.0, 0.85),
            (0.5, 1.0),
            (0.9, 0.75),
            (0.5, 0.55),
            (0.9, 0.25),
            (0.5, 0.0),
            (0.0, 0.15),
        ]],
        '4' => &[&[(0.75, 0.0), (0.75, 1.0), (0.0, 0.35), (1.0, 0.35)]],
        '5' => &[&[
            (1.0, 1.0),
            (0.1, 1.0),
            (0.1, 0.55),
            (0.7, 0.6),
            (0.95, 0.3),
            (0.6, 0.0),
            (0.0, 0.1),
        ]],
        '6' => &[&[
            (0.85, 0.9),
            (0.4, 1.0),
            (0.1, 0.55),
            (0.1, 0.2),
            (0.5, 0.0),
            (0.9, 0.2),
            (0.6, 0.5),
            (0.1, 0.45),
        ]],
        '7' => &[&[(0.0, 1.0), (1.0, 1.0), (0.35, 0.0)]],
        '8' => &[
            &[
                (0.5, 0.55),
                (0.1, 0.78),
                (0.5, 1.0),
                (0.9, 0.78),
                (0.5, 0.55),
            ],
            &[
                (0.5, 0.55),
                (0.05, 0.28),
                (0.5, 0.0),
                (0.95, 0.28),
                (0.5, 0.55),
            ],
        ],
        '9' => &[&[
            (0.9, 0.45),
            (0.5, 0.55),
            (0.15, 0.78),
            (0.5, 1.0),
            (0.9, 0.78),
            (0.9, 0.1),
            (0.4, 0.0),
        ]],
        '.' => &[&[
            (0.4, 0.0),
            (0.55, 0.0),
            (0.55, 0.12),
            (0.4, 0.12),
            (0.4, 0.0),
        ]],
        ',' => &[&[(0.5, 0.12), (0.35, -0.12)]],
        '-' => &[&[(0.15, 0.5), (0.85, 0.5)]],
        '/' => &[&[(0.0, 0.0), (1.0, 1.0)]],
        '(' => &[&[(0.7, 1.0), (0.3, 0.5), (0.7, 0.0)]],
        ')' => &[&[(0.3, 1.0), (0.7, 0.5), (0.3, 0.0)]],
        '%' => &[
            &[(0.0, 0.0), (1.0, 1.0)],
            &[
                (0.1, 0.85),
                (0.3, 0.85),
                (0.3, 1.0),
                (0.1, 1.0),
                (0.1, 0.85),
            ],
            &[(0.7, 0.0), (0.9, 0.0), (0.9, 0.15), (0.7, 0.15), (0.7, 0.0)],
        ],
        _ => &[],
    }
}

/// Render `text` as a baseline of glyph polylines, scaled to cap height `size`,
/// starting at `(x, y)` (canvas Y-up) and rotated `angle_deg` degrees CCW about
/// that anchor. Returns one stroked, fill-less [`Shape`] block holding all the
/// strokes (a `Move` per stroke start, `Line`s along it). Empty when `text` has
/// no drawable glyphs.
fn text_path_shape(
    text: &str,
    x: f64,
    y: f64,
    size: f64,
    angle_deg: f64,
    color: [f64; 3],
) -> Option<Block> {
    let (sin, cos) = angle_deg.to_radians().sin_cos();
    // Map a unit-cell point at pen offset `pen` (along the baseline) to canvas.
    let place = |pen: f64, gx: f64, gy: f64| -> (f64, f64) {
        // Local baseline coords (before rotation): X along the text, Y up.
        let lx = pen + gx * size;
        let ly = gy * size;
        (x + lx * cos - ly * sin, y + lx * sin + ly * cos)
    };
    let mut segs = Vec::new();
    let mut pen = 0.0_f64;
    let advance = GLYPH_ADVANCE * size;
    let gap = GLYPH_GAP * size;
    for ch in text.chars() {
        for stroke in glyph(ch) {
            for (i, &(gx, gy)) in stroke.iter().enumerate() {
                let (px, py) = place(pen, gx, gy);
                if i == 0 {
                    segs.push(PathSeg::Move(px, py));
                } else {
                    segs.push(PathSeg::Line(px, py));
                }
            }
        }
        pen += advance + gap;
    }
    if segs.is_empty() {
        return None;
    }
    Some(shape_block(segs, None, Some(color), (size * 0.09).max(0.4)))
}

/// The drawn width of `text` at cap height `size` in the stroke font (points).
fn text_path_width(text: &str, size: f64) -> f64 {
    let n = text.chars().count();
    if n == 0 {
        return 0.0;
    }
    let advance = GLYPH_ADVANCE * size;
    let gap = GLYPH_GAP * size;
    n as f64 * advance + (n - 1) as f64 * gap
}

// ──────────────────────────────── chart rendering ─────────────────────────────

/// Render the whole chart into `out`: a title (if any), the axes + grid, the
/// plotted series for the detected [`ChartKind`], and a legend (when present or
/// when there are multiple series / a pie). The blocks share the chart canvas via
/// their frames.
fn render_chart(chart: &Chart, out: &mut Vec<Block>) {
    // Title at the top of the canvas.
    if !chart.title.is_empty() {
        out.push(label_block(
            &chart.title,
            0.0,
            6.0,
            CHART_W,
            12.0,
            Align::Center,
        ));
    }

    match chart.kind {
        ChartKind::Column => render_bars(chart, out, false),
        ChartKind::Bar => render_bars(chart, out, true),
        ChartKind::Line => render_line_or_area(chart, out, false),
        ChartKind::Area => render_line_or_area(chart, out, true),
        ChartKind::Scatter => render_scatter(chart, out),
        ChartKind::Radar => render_radar(chart, out),
        ChartKind::Pie => render_pie(chart, out),
    }
}

/// The plot rectangle (where data is drawn) in **canvas Y-up** coordinates:
/// returns `(x0, y0, w, h)` with `(x0, y0)` the lower-left corner.
fn plot_rect() -> (f64, f64, f64, f64) {
    let x0 = MARGIN_LEFT;
    let y0 = MARGIN_BOTTOM;
    let w = CHART_W - MARGIN_LEFT - MARGIN_RIGHT;
    let h = CHART_H - MARGIN_TOP - MARGIN_BOTTOM;
    (x0, y0, w.max(1.0), h.max(1.0))
}

/// Map a value `v` to a Y-up canvas coordinate within the plot's vertical extent,
/// given the value range `(lo, hi)`.
fn map_value_y(v: f64, lo: f64, hi: f64, y0: f64, h: f64) -> f64 {
    let span = (hi - lo).max(f64::MIN_POSITIVE);
    y0 + (v - lo) / span * h
}

/// Draw the value-axis grid lines + tick labels and the two axis lines, for a
/// vertically-scaled plot (column/line/area). Returns the baseline Y (where
/// `value == 0`, clamped into the plot) in canvas Y-up coordinates.
fn draw_value_axis(chart: &Chart, out: &mut Vec<Block>, lo: f64, hi: f64) -> f64 {
    let (x0, y0, w, h) = plot_rect();
    let step = nice_step(hi - lo, 5);
    // First tick at or below `lo`, last at or above `hi`.
    let start = (lo / step).floor() * step;
    let mut t = start;
    while t <= hi + step * 0.5 {
        if t >= lo - step * 0.5 {
            let y = map_value_y(t, lo, hi, y0, h);
            // Grid line across the plot.
            out.push(line_shape(x0, y, x0 + w, y, GRID_RGB, 0.5));
            // Tick label to the left (Y-down top from the Y-up tick).
            let y_top = CHART_H - y - 5.0;
            out.push(label_block(
                &fmt_num(t),
                2.0,
                y_top,
                MARGIN_LEFT - 6.0,
                8.0,
                Align::Right,
            ));
        }
        t += step;
    }
    // Axis lines: left (value) and bottom (category).
    out.push(line_shape(x0, y0, x0, y0 + h, AXIS_RGB, 1.0));
    out.push(line_shape(x0, y0, x0 + w, y0, AXIS_RGB, 1.0));
    // Value-axis title: a text path rotated −90° (reading bottom-to-top) along
    // the left margin, vertically centred against the plot height.
    if !chart.val_axis_title.is_empty() {
        let size = 8.0;
        // The rotated baseline runs upward; centre the title over the plot by
        // starting it half its drawn width below the plot midpoint.
        let drawn = text_path_width(&chart.val_axis_title, size);
        let start_y = y0 + (h - drawn) / 2.0;
        // Anchor near the left edge; the cells extend rightward (−90° ⇒ +X→−Y
        // local maps to up the page), so place the baseline a touch in from x=0.
        let anchor_x = 9.0;
        if let Some(b) = text_path_shape(
            &chart.val_axis_title,
            anchor_x,
            start_y,
            size,
            90.0,
            AXIS_RGB,
        ) {
            out.push(b);
        }
    }
    map_value_y(0.0_f64.clamp(lo, hi), lo, hi, y0, h)
}

/// Place the category labels under the plot, one per category slot, centred in
/// each slot. Used by column/line/area/radar.
fn draw_category_labels(cats: &[String], out: &mut Vec<Block>) {
    let (x0, _y0, w, _h) = plot_rect();
    let n = cats.len().max(1);
    let slot = w / n as f64;
    let y_top = CHART_H - MARGIN_BOTTOM + 3.0;
    for (i, c) in cats.iter().enumerate() {
        let cx = x0 + slot * (i as f64 + 0.5);
        out.push(label_block(
            c,
            cx - slot / 2.0,
            y_top,
            slot,
            8.0,
            Align::Center,
        ));
    }
    if !cats.is_empty() {
        // Category-axis title centred below the labels.
        // (Looked up by the caller via the chart; passed implicitly through `out`.)
    }
}

/// Bar / column renderer (clustered and stacked). `horizontal` swaps the axes
/// (a `c:barChart` with `barDir=bar`). Draws axes, the bars per category/series,
/// and a legend.
fn render_bars(chart: &Chart, out: &mut Vec<Block>, horizontal: bool) {
    let cats = chart.categories();
    let (lo, hi) = value_range(chart);
    let (x0, y0, w, h) = plot_rect();
    let n_cat = chart.cat_count().max(1);
    let n_ser = chart.series.len().max(1);
    let stacked = matches!(chart.grouping, Grouping::Stacked | Grouping::PercentStacked);
    let percent = matches!(chart.grouping, Grouping::PercentStacked);

    if horizontal {
        // Horizontal bars: value runs along X, categories down Y.
        // Draw a vertical grid + value labels along the bottom, category labels at
        // the left.
        let step = nice_step(hi - lo, 5);
        let start = (lo / step).floor() * step;
        let mut t = start;
        while t <= hi + step * 0.5 {
            if t >= lo - step * 0.5 {
                let vx = map_value_x(t, lo, hi, x0, w);
                out.push(line_shape(vx, y0, vx, y0 + h, GRID_RGB, 0.5));
                out.push(label_block(
                    &fmt_num(t),
                    vx - 18.0,
                    CHART_H - MARGIN_BOTTOM + 3.0,
                    36.0,
                    8.0,
                    Align::Center,
                ));
            }
            t += step;
        }
        out.push(line_shape(x0, y0, x0, y0 + h, AXIS_RGB, 1.0));
        out.push(line_shape(x0, y0, x0 + w, y0, AXIS_RGB, 1.0));
        // Horizontal bars: the category axis is the vertical (left) one — its
        // title is rotated along the left margin; the value axis is the bottom
        // X axis — its title is centred below the value labels.
        draw_cat_axis_title(chart, out, true);
        draw_val_axis_title_below(chart, out);

        let slot = h / n_cat as f64;
        let baseline_x = map_value_x(0.0_f64.clamp(lo, hi), lo, hi, x0, w);
        for c in 0..n_cat {
            // Category labels at the left, centred in the slot.
            let cy_up = y0 + slot * (n_cat as f64 - 1.0 - c as f64 + 0.5);
            if let Some(label) = cats.get(c) {
                out.push(label_block(
                    label,
                    2.0,
                    CHART_H - cy_up - 5.0,
                    MARGIN_LEFT - 6.0,
                    8.0,
                    Align::Right,
                ));
            }
            if stacked {
                let total: f64 = if percent {
                    chart
                        .series
                        .iter()
                        .map(|s| s.values.get(c).copied().unwrap_or(0.0).max(0.0))
                        .sum::<f64>()
                        .max(f64::MIN_POSITIVE)
                } else {
                    1.0
                };
                let mut acc = 0.0;
                let bh = slot * 0.7;
                let by = cy_up - bh / 2.0;
                for (si, s) in chart.series.iter().enumerate() {
                    let raw = s.values.get(c).copied().unwrap_or(0.0);
                    let v = if percent { raw / total } else { raw };
                    let x_from = map_value_x(acc, lo, hi, x0, w);
                    let x_to = map_value_x(acc + v, lo, hi, x0, w);
                    let bx = x_from.min(x_to);
                    let bw = (x_to - x_from).abs();
                    out.push(rect_shape(
                        bx,
                        by,
                        bw,
                        bh,
                        Some(series_color(si)),
                        Some([1.0, 1.0, 1.0]),
                        0.5,
                    ));
                    acc += v;
                }
            } else {
                let group_h = slot * 0.8;
                let bar_h = group_h / n_ser as f64;
                for (si, s) in chart.series.iter().enumerate() {
                    let v = s.values.get(c).copied().unwrap_or(0.0);
                    let x_to = map_value_x(v, lo, hi, x0, w);
                    let bx = baseline_x.min(x_to);
                    let bw = (x_to - baseline_x).abs();
                    let by = cy_up - group_h / 2.0 + bar_h * si as f64;
                    out.push(rect_shape(
                        bx,
                        by,
                        bw,
                        bar_h * 0.9,
                        Some(series_color(si)),
                        None,
                        0.0,
                    ));
                }
            }
        }
    } else {
        // Vertical columns.
        let baseline = draw_value_axis(chart, out, lo, hi);
        draw_category_labels(&cats, out);
        draw_cat_axis_title(chart, out, false);
        let slot = w / n_cat as f64;
        for c in 0..n_cat {
            let cx = x0 + slot * c as f64;
            if stacked {
                let total: f64 = if percent {
                    chart
                        .series
                        .iter()
                        .map(|s| s.values.get(c).copied().unwrap_or(0.0).max(0.0))
                        .sum::<f64>()
                        .max(f64::MIN_POSITIVE)
                } else {
                    1.0
                };
                let bw = slot * 0.7;
                let bx = cx + (slot - bw) / 2.0;
                let mut acc = 0.0;
                for (si, s) in chart.series.iter().enumerate() {
                    let raw = s.values.get(c).copied().unwrap_or(0.0);
                    let v = if percent { raw / total } else { raw };
                    let y_from = map_value_y(acc, lo, hi, y0, h);
                    let y_to = map_value_y(acc + v, lo, hi, y0, h);
                    let by = y_from.min(y_to);
                    let bh = (y_to - y_from).abs();
                    out.push(rect_shape(
                        bx,
                        by,
                        bw,
                        bh,
                        Some(series_color(si)),
                        Some([1.0, 1.0, 1.0]),
                        0.5,
                    ));
                    acc += v;
                }
            } else {
                let group_w = slot * 0.8;
                let bar_w = group_w / n_ser as f64;
                for (si, s) in chart.series.iter().enumerate() {
                    let v = s.values.get(c).copied().unwrap_or(0.0);
                    let y_to = map_value_y(v, lo, hi, y0, h);
                    let by = baseline.min(y_to);
                    let bh = (y_to - baseline).abs();
                    let bx = cx + (slot - group_w) / 2.0 + bar_w * si as f64;
                    out.push(rect_shape(
                        bx,
                        by,
                        bar_w * 0.9,
                        bh,
                        Some(series_color(si)),
                        None,
                        0.0,
                    ));
                }
            }
        }
    }

    draw_legend(chart, out, &cats);
}

/// Map a value `v` to an X canvas coordinate within the plot's horizontal extent
/// (used by the horizontal-bar and scatter renderers).
fn map_value_x(v: f64, lo: f64, hi: f64, x0: f64, w: f64) -> f64 {
    let span = (hi - lo).max(f64::MIN_POSITIVE);
    x0 + (v - lo) / span * w
}

/// Line / area renderer. `area` fills under each series' polyline. Categories are
/// laid out evenly along X; values scale to the shared range.
fn render_line_or_area(chart: &Chart, out: &mut Vec<Block>, area: bool) {
    let cats = chart.categories();
    let (lo, hi) = value_range(chart);
    let (x0, y0, w, h) = plot_rect();
    let baseline = draw_value_axis(chart, out, lo, hi);
    draw_category_labels(&cats, out);
    draw_cat_axis_title(chart, out, false);

    let n_cat = chart.cat_count().max(1);
    // Point at the centre of each category slot.
    let px = |i: usize| -> f64 {
        let slot = w / n_cat as f64;
        x0 + slot * (i as f64 + 0.5)
    };

    for (si, s) in chart.series.iter().enumerate() {
        let color = series_color(si);
        let mut pts: Vec<(f64, f64)> = Vec::new();
        for (i, &v) in s.values.iter().enumerate() {
            pts.push((px(i), map_value_y(v, lo, hi, y0, h)));
        }
        if pts.is_empty() {
            continue;
        }
        if area {
            // Filled region: along the points, then back along the baseline.
            let mut segs = Vec::with_capacity(pts.len() + 3);
            segs.push(PathSeg::Move(pts[0].0, baseline));
            for &(x, y) in &pts {
                segs.push(PathSeg::Line(x, y));
            }
            segs.push(PathSeg::Line(pts[pts.len() - 1].0, baseline));
            segs.push(PathSeg::Close);
            // Lighten the fill so overlapping areas remain legible.
            out.push(shape_block(
                segs,
                Some(lighten(color, 0.45)),
                Some(color),
                1.0,
            ));
        }
        // The polyline itself (always drawn, also atop an area fill).
        let mut segs = Vec::with_capacity(pts.len());
        segs.push(PathSeg::Move(pts[0].0, pts[0].1));
        for &(x, y) in &pts[1..] {
            segs.push(PathSeg::Line(x, y));
        }
        out.push(shape_block(segs, None, Some(color), 1.5));
        // Point markers.
        for &(x, y) in &pts {
            out.push(marker_shape(x, y, 2.0, color));
        }
    }

    draw_legend(chart, out, &cats);
}

/// Scatter renderer: each series is a cloud of point marks at `(xVal, yVal)`.
/// Both axes are value axes scaled to the data extents.
fn render_scatter(chart: &Chart, out: &mut Vec<Block>) {
    let (x0, y0, w, h) = plot_rect();
    // X range from x_values, Y range from values.
    let mut xlo = f64::INFINITY;
    let mut xhi = f64::NEG_INFINITY;
    let mut ylo = 0.0_f64;
    let mut yhi = 0.0_f64;
    for s in &chart.series {
        for &x in &s.x_values {
            xlo = xlo.min(x);
            xhi = xhi.max(x);
        }
        for &y in &s.values {
            ylo = ylo.min(y);
            yhi = yhi.max(y);
        }
    }
    let has_x = xlo.is_finite() && xhi.is_finite();
    if !has_x {
        // No explicit X — index the points 1..n.
        xlo = 1.0;
        xhi = chart.cat_count().max(1) as f64;
    }
    if (xhi - xlo).abs() < f64::EPSILON {
        xhi = xlo + 1.0;
    }
    if (yhi - ylo).abs() < f64::EPSILON {
        yhi = ylo + 1.0;
    }

    // Y grid + labels.
    let _ = draw_value_axis(chart, out, ylo, yhi);

    // X axis. With explicit X values it is a "nice"-stepped numeric value axis.
    // Without X values but WITH categories, the points are evenly spaced and the
    // axis shows the category text (not a bare integer index). With neither, fall
    // back to numeric index ticks.
    let cats = chart.categories();
    let cats_no_x = !has_x && !cats.is_empty();
    if cats_no_x {
        // Evenly-spaced category labels under the plot (the same slot layout the
        // markers use, since points without X are placed at indices 1..n which
        // map across the plot width).
        draw_category_labels(&cats, out);
    } else {
        let xstep = nice_step(xhi - xlo, 5);
        let start = (xlo / xstep).floor() * xstep;
        let mut t = start;
        while t <= xhi + xstep * 0.5 {
            if t >= xlo - xstep * 0.5 {
                let vx = map_value_x(t, xlo, xhi, x0, w);
                out.push(line_shape(vx, y0, vx, y0 + h, GRID_RGB, 0.5));
                out.push(label_block(
                    &fmt_num(t),
                    vx - 18.0,
                    CHART_H - MARGIN_BOTTOM + 3.0,
                    36.0,
                    8.0,
                    Align::Center,
                ));
            }
            t += xstep;
        }
    }

    // Number of category slots (used to centre points when X is index-based).
    let n_slot = chart.cat_count().max(1);
    let slot = w / n_slot as f64;
    for (si, s) in chart.series.iter().enumerate() {
        let color = series_color(si);
        for (i, &y) in s.values.iter().enumerate() {
            let cx = if cats_no_x {
                // Place at the centre of category slot `i`, matching the labels.
                x0 + slot * (i as f64 + 0.5)
            } else {
                let xv = s.x_values.get(i).copied().unwrap_or((i + 1) as f64);
                map_value_x(xv, xlo, xhi, x0, w)
            };
            let cy = map_value_y(y, ylo, yhi, y0, h);
            out.push(marker_shape(cx, cy, 2.5, color));
        }
    }

    draw_legend(chart, out, &cats);
}

/// Radar renderer: each category is an axis radiating from the centre; each
/// series becomes a closed polygon connecting its per-category values.
fn render_radar(chart: &Chart, out: &mut Vec<Block>) {
    let (x0, y0, w, h) = plot_rect();
    let cx = x0 + w / 2.0;
    let cy = y0 + h / 2.0;
    let radius = w.min(h) / 2.0 * 0.85;
    let n_cat = chart.cat_count().max(1);
    let (_lo, hi) = value_range(chart);
    let hi = hi.max(f64::MIN_POSITIVE);

    // Spoke for each category (Y-up). Angles start at the top, clockwise.
    let angle = |i: usize| -> f64 {
        let frac = i as f64 / n_cat as f64;
        std::f64::consts::FRAC_PI_2 - frac * std::f64::consts::TAU
    };
    let cats = chart.categories();
    for i in 0..n_cat {
        let a = angle(i);
        let ex = cx + radius * a.cos();
        let ey = cy + radius * a.sin();
        out.push(line_shape(cx, cy, ex, ey, GRID_RGB, 0.5));
        // Category label just past the spoke end.
        if let Some(label) = cats.get(i) {
            let lx = cx + (radius + 6.0) * a.cos();
            let ly_up = cy + (radius + 6.0) * a.sin();
            out.push(label_block(
                label,
                lx - 24.0,
                CHART_H - ly_up - 4.0,
                48.0,
                7.0,
                Align::Center,
            ));
        }
    }
    // Concentric grid rings at "nice"-stepped value levels (not a hardcoded
    // 50%/100%). The number of rings follows the data range; the topmost spoke
    // (index 0, pointing up) is labelled with each ring's value.
    let step = nice_step(hi, 4);
    let ring_max = (hi / step).ceil().max(1.0); // how many steps cover `hi`
    let n_rings = (ring_max as usize).clamp(1, 6);
    let a0_top = angle(0);
    for ring in 1..=n_rings {
        let level = step * ring as f64;
        let rr = (level / hi).clamp(0.0, 1.0) * radius;
        let mut segs = Vec::with_capacity(n_cat + 1);
        for i in 0..n_cat {
            let a = angle(i);
            let x = cx + rr * a.cos();
            let y = cy + rr * a.sin();
            if i == 0 {
                segs.push(PathSeg::Move(x, y));
            } else {
                segs.push(PathSeg::Line(x, y));
            }
        }
        segs.push(PathSeg::Close);
        out.push(shape_block(segs, None, Some(GRID_RGB), 0.5));
        // Ring value label, nudged just to the left of the top spoke.
        let lx = cx + rr * a0_top.cos();
        let ly_up = cy + rr * a0_top.sin();
        out.push(label_block(
            &fmt_num(level),
            lx - 30.0,
            CHART_H - ly_up - 5.0,
            26.0,
            7.0,
            Align::Right,
        ));
    }

    for (si, s) in chart.series.iter().enumerate() {
        let color = series_color(si);
        let mut segs = Vec::with_capacity(n_cat + 1);
        for i in 0..n_cat {
            let v = s.values.get(i).copied().unwrap_or(0.0);
            let r = (v / hi).clamp(0.0, 1.0) * radius;
            let a = angle(i);
            let x = cx + r * a.cos();
            let y = cy + r * a.sin();
            if i == 0 {
                segs.push(PathSeg::Move(x, y));
            } else {
                segs.push(PathSeg::Line(x, y));
            }
        }
        if !segs.is_empty() {
            segs.push(PathSeg::Close);
            out.push(shape_block(
                segs,
                Some(lighten(color, 0.55)),
                Some(color),
                1.5,
            ));
        }
    }

    draw_legend(chart, out, &cats);
}

/// Pie / doughnut renderer: the **first** series' values become wedges around a
/// circle (the conventional pie layout). A doughnut (`inner_fraction > 0`) cuts a
/// hole. The legend lists the categories (each wedge's slice).
fn render_pie(chart: &Chart, out: &mut Vec<Block>) {
    let series = match chart.series.first() {
        Some(s) if !s.values.is_empty() => s,
        _ => return,
    };
    let total: f64 = series.values.iter().map(|v| v.max(0.0)).sum();
    if total <= 0.0 {
        return;
    }
    let (x0, y0, w, h) = plot_rect();
    let cx = x0 + w / 2.0;
    let cy = y0 + h / 2.0;
    let radius = w.min(h) / 2.0 * 0.9;
    let inner = radius * chart.inner_fraction;

    // Wedges sweep clockwise from the top (12 o'clock), as Office draws them.
    let mut a0 = std::f64::consts::FRAC_PI_2;
    for (i, &v) in series.values.iter().enumerate() {
        let frac = v.max(0.0) / total;
        if frac <= 0.0 {
            continue;
        }
        let a1 = a0 - frac * std::f64::consts::TAU;
        let segs = wedge_segments(cx, cy, radius, inner, a0, a1);
        out.push(shape_block(
            segs,
            Some(series_color(i)),
            Some([1.0, 1.0, 1.0]),
            0.75,
        ));
        a0 = a1;
    }

    // Doughnut centre label: when a hole is cut and the chart carries a title,
    // place it centred in the hole at `(cx, cy)` (the conventional doughnut KPI
    // label). Solid pies have no hole, so nothing is drawn there.
    if inner > 0.0 && !chart.title.is_empty() {
        let size = 9.0;
        // The hole's inner diameter caps the usable label width.
        let box_w = (inner * 2.0 - 4.0).max(20.0);
        out.push(label_block(
            &chart.title,
            cx - box_w / 2.0,
            CHART_H - cy - size * 0.7,
            box_w,
            size,
            Align::Center,
        ));
    }

    // Pie legend lists the categories (each is a slice).
    let cats = if !series.categories.is_empty() {
        series.categories.clone()
    } else {
        (1..=series.values.len()).map(|i| i.to_string()).collect()
    };
    draw_legend_items(&cats, out, series_color);
}

/// A filled wedge (pie slice) or annular sector (doughnut slice) as a path in
/// canvas Y-up coordinates: from `a0` to `a1` (radians, sweeping the short way as
/// given), outer radius `r`, inner radius `inner` (`0` ⇒ a solid pie wedge from
/// the centre). Arcs are approximated with cubic Béziers (≤ 90° each).
fn wedge_segments(cx: f64, cy: f64, r: f64, inner: f64, a0: f64, a1: f64) -> Vec<PathSeg> {
    let mut segs = Vec::new();
    let p = |rad: f64, ang: f64| (cx + rad * ang.cos(), cy + rad * ang.sin());

    let (sx, sy) = p(r, a0);
    if inner <= 0.0 {
        segs.push(PathSeg::Move(cx, cy));
        segs.push(PathSeg::Line(sx, sy));
        arc_to(&mut segs, cx, cy, r, a0, a1);
        segs.push(PathSeg::Close);
    } else {
        // Outer arc forward, line in to the inner radius, inner arc back, close.
        segs.push(PathSeg::Move(sx, sy));
        arc_to(&mut segs, cx, cy, r, a0, a1);
        let (ix, iy) = p(inner, a1);
        segs.push(PathSeg::Line(ix, iy));
        arc_to(&mut segs, cx, cy, inner, a1, a0);
        segs.push(PathSeg::Close);
    }
    segs
}

/// Append cubic-Bézier segments approximating a circular arc on centre `(cx, cy)`
/// radius `r`, from angle `a0` to `a1` (radians). The arc is split into spans of
/// ≤ 90° for accuracy. Assumes the current point is already at the arc start.
fn arc_to(segs: &mut Vec<PathSeg>, cx: f64, cy: f64, r: f64, a0: f64, a1: f64) {
    let total = a1 - a0;
    let n = (total.abs() / std::f64::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let delta = total / n as f64;
    let mut a = a0;
    for _ in 0..n {
        let next = a + delta;
        // Bézier handle length for a circular arc of half-angle `delta/2`.
        let k = 4.0 / 3.0 * (delta / 4.0).tan();
        let (x0, y0) = (cx + r * a.cos(), cy + r * a.sin());
        let (x1, y1) = (cx + r * next.cos(), cy + r * next.sin());
        let c1x = x0 - k * r * a.sin();
        let c1y = y0 + k * r * a.cos();
        let c2x = x1 + k * r * next.sin();
        let c2y = y1 - k * r * next.cos();
        segs.push(PathSeg::Cubic(c1x, c1y, c2x, c2y, x1, y1));
        a = next;
    }
}

/// A small filled square marker (a tiny `Shape`) centred at `(x, y)` (canvas
/// Y-up) with half-size `hs`. Used for line/scatter point marks.
fn marker_shape(x: f64, y: f64, hs: f64, color: [f64; 3]) -> Block {
    rect_shape(x - hs, y - hs, hs * 2.0, hs * 2.0, Some(color), None, 0.0)
}

/// Blend `color` toward white by fraction `t` (`0` ⇒ unchanged, `1` ⇒ white).
fn lighten(color: [f64; 3], t: f64) -> [f64; 3] {
    [
        color[0] + (1.0 - color[0]) * t,
        color[1] + (1.0 - color[1]) * t,
        color[2] + (1.0 - color[2]) * t,
    ]
}

/// Place the category-axis title, if present. `vertical` is `true` when the
/// category axis runs down the **left** of the plot (horizontal-bar charts):
/// the title is then a text path rotated −90° along the left margin. Otherwise
/// the category axis is the bottom X axis and the title is centred below the
/// category labels (column / line / area).
fn draw_cat_axis_title(chart: &Chart, out: &mut Vec<Block>, vertical: bool) {
    if chart.cat_axis_title.is_empty() {
        return;
    }
    if vertical {
        let (_x0, y0, _w, h) = plot_rect();
        let size = 8.0;
        let drawn = text_path_width(&chart.cat_axis_title, size);
        let start_y = y0 + (h - drawn) / 2.0;
        if let Some(b) = text_path_shape(&chart.cat_axis_title, 9.0, start_y, size, 90.0, AXIS_RGB)
        {
            out.push(b);
        }
        return;
    }
    out.push(label_block(
        &chart.cat_axis_title,
        MARGIN_LEFT,
        CHART_H - LEGEND_H - 9.0,
        CHART_W - MARGIN_LEFT - MARGIN_RIGHT,
        8.0,
        Align::Center,
    ));
}

/// Place the value-axis title centred below the value (X) axis labels — used by
/// the horizontal-bar chart, whose value axis is the bottom X axis.
fn draw_val_axis_title_below(chart: &Chart, out: &mut Vec<Block>) {
    if chart.val_axis_title.is_empty() {
        return;
    }
    out.push(label_block(
        &chart.val_axis_title,
        MARGIN_LEFT,
        CHART_H - LEGEND_H - 9.0,
        CHART_W - MARGIN_LEFT - MARGIN_RIGHT,
        8.0,
        Align::Center,
    ));
}

/// Draw the legend strip along the bottom of the canvas: a colour swatch + the
/// series name for each series. Skipped when there is only one unnamed series and
/// the chart declared no legend.
fn draw_legend(chart: &Chart, out: &mut Vec<Block>, _cats: &[String]) {
    if chart.series.is_empty() {
        return;
    }
    // Only suppress for a single anonymous series with no explicit legend.
    if chart.series.len() == 1 && chart.series[0].name.is_empty() && !chart.has_legend {
        return;
    }
    let labels: Vec<String> = chart
        .series
        .iter()
        .enumerate()
        .map(|(i, s)| {
            if s.name.is_empty() {
                format!("Series {}", i + 1)
            } else {
                s.name.clone()
            }
        })
        .collect();
    draw_legend_items(&labels, out, series_color);
}

/// The minimum width (points) a single legend entry cell may shrink to before
/// the strip wraps onto another row.
const LEGEND_MIN_CELL: f64 = 64.0;
/// The vertical pitch (points) between wrapped legend rows.
const LEGEND_ROW_PITCH: f64 = 12.0;

/// Render legend entries (swatch + label) along the bottom strip. Entries flow
/// left-to-right; when fitting them all on one row would shrink each cell below
/// [`LEGEND_MIN_CELL`], the strip **wraps onto multiple rows** (the first row at
/// the bottom, later rows stacked above it), growing the legend's height so the
/// labels stay legible. `color_of(i)` gives entry `i`'s swatch colour.
fn draw_legend_items(
    labels: &[String],
    out: &mut Vec<Block>,
    color_of: impl Fn(usize) -> [f64; 3],
) {
    if labels.is_empty() {
        return;
    }
    let n = labels.len();
    let avail = CHART_W - 8.0;
    // Columns per row: as many as fit at the minimum cell width (≥ 1), capped at
    // the entry count so a short legend still spreads across the full width.
    let cols = ((avail / LEGEND_MIN_CELL).floor() as usize).clamp(1, n);
    let cell = avail / cols as f64;
    let swatch = 8.0;
    // First (bottom) row sits on the legend baseline; each further row is one
    // pitch higher (Y-up), so the block stack grows upward into the plot margin.
    let base_y_up = LEGEND_H / 2.0;
    for (i, label) in labels.iter().enumerate() {
        let col = i % cols;
        let row = i / cols;
        let cx = 4.0 + cell * col as f64;
        let y_up = base_y_up + row as f64 * LEGEND_ROW_PITCH;
        let y_top = CHART_H - y_up - 5.0;
        // Swatch (Y-up rect centred on this row's line).
        out.push(rect_shape(
            cx,
            y_up - swatch / 2.0,
            swatch,
            swatch,
            Some(color_of(i)),
            None,
            0.0,
        ));
        // Label to the right of the swatch.
        out.push(label_block(
            label,
            cx + swatch + 3.0,
            y_top,
            cell - swatch - 5.0,
            8.0,
            Align::Left,
        ));
    }
}

// ──────────────────────────────── data table ──────────────────────────────────

/// Build a [`Table`] of the chart's numbers: a header row (`""` + each series
/// name) and one body row per category (`category, v₀, v₁, …`). Returns `None`
/// when there is nothing tabular to show.
fn chart_data_table(chart: &Chart) -> Option<Table> {
    if chart.series.is_empty() {
        return None;
    }
    let cats = chart.categories();
    if cats.is_empty() {
        return None;
    }
    let mut rows = Vec::new();

    // Header: blank corner + series names.
    let mut header_cells = vec![cell_text("")];
    for (i, s) in chart.series.iter().enumerate() {
        let name = if s.name.is_empty() {
            format!("Series {}", i + 1)
        } else {
            s.name.clone()
        };
        header_cells.push(cell_text(&name));
    }
    rows.push(Row {
        cells: header_cells,
        height: None,
        is_header: true,
    });

    // One body row per category.
    for (ci, cat) in cats.iter().enumerate() {
        let mut cells = vec![cell_text(cat)];
        for s in &chart.series {
            let v = s.values.get(ci).copied().unwrap_or(0.0);
            cells.push(cell_text(&fmt_num(v)));
        }
        rows.push(Row {
            cells,
            height: None,
            is_header: false,
        });
    }

    // Leave `col_widths` empty: the model fills equal defaults for a width-less
    // table, matching the ODF/PPTX shape-table lowering.
    Some(Table {
        rows,
        col_widths: Vec::new(),
        border: crate::model::BorderStyle {
            width: 0.5,
            color: [0.0, 0.0, 0.0],
        },
    })
}

/// A single-paragraph table cell holding `text`.
fn cell_text(text: &str) -> crate::model::Cell {
    crate::model::Cell {
        blocks: vec![Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: text.to_string(),
                    style: CharStyle::default(),
                    source_index: None,
                })],
                ..Paragraph::default()
            }),
            ..Block::default()
        }],
        ..crate::model::Cell::default()
    }
}

// ────────────────────────────────── SmartArt ──────────────────────────────────

/// One SmartArt data point with its model id, text, and (optional) parent.
#[derive(Debug, Clone, Default)]
struct SaPoint {
    /// `dgm:pt@modelId`.
    id: String,
    /// Whether this is a real node point (`type="node"`/absent) vs a presentation
    /// or document point we skip for the hierarchy.
    is_node: bool,
    /// The point's text (joined paragraphs of its `dgm:t` body).
    text: String,
}

/// Parse a SmartArt `dgm:dataModel` into a nested [`List`]: each real node point
/// becomes an item; the `dgm:cxnLst` `parOf` connections give parent→child links,
/// so an item's [`ListItem::level`] is its depth in that tree. Returns `None` when
/// no node text is found.
fn smartart_list(data_xml: &str) -> Option<List> {
    let (points, children) = parse_smartart_model(data_xml);
    if points.is_empty() {
        return None;
    }

    // Roots: node points that are no one's child.
    let mut is_child = std::collections::BTreeSet::new();
    for kids in children.values() {
        for k in kids {
            is_child.insert(k.clone());
        }
    }
    let id_to_text: std::collections::BTreeMap<&str, &str> = points
        .iter()
        .map(|p| (p.id.as_str(), p.text.as_str()))
        .collect();

    let mut items: Vec<ListItem> = Vec::new();
    // Depth-first walk from each root, preserving document order of `points`.
    let mut visited = std::collections::BTreeSet::new();
    for p in &points {
        if is_child.contains(&p.id) || visited.contains(&p.id) {
            continue;
        }
        walk_smartart(&p.id, 0, &children, &id_to_text, &mut items, &mut visited);
    }
    // If nothing rooted (e.g. all points cross-linked), fall back to a flat list
    // of every node point in document order.
    if items.is_empty() {
        for p in &points {
            if !p.text.is_empty() {
                items.push(list_item(&p.text, 0));
            }
        }
    }
    if items.is_empty() {
        return None;
    }
    Some(List {
        ordered: false,
        marker: ListMarker::Bullet('\u{2022}'),
        items,
    
    ..Default::default()
})
}

/// Recursive depth-first emit of a SmartArt node and its children as list items.
fn walk_smartart(
    id: &str,
    level: u8,
    children: &std::collections::BTreeMap<String, Vec<String>>,
    id_to_text: &std::collections::BTreeMap<&str, &str>,
    items: &mut Vec<ListItem>,
    visited: &mut std::collections::BTreeSet<String>,
) {
    if !visited.insert(id.to_string()) {
        return; // guard against cyclic connections
    }
    if let Some(text) = id_to_text.get(id) {
        if !text.is_empty() {
            items.push(list_item(text, level));
        }
    }
    if let Some(kids) = children.get(id) {
        let next = level.saturating_add(1);
        for kid in kids {
            walk_smartart(kid, next, children, id_to_text, items, visited);
        }
    }
}

/// A single-paragraph [`ListItem`] holding `text` at nesting `level`.
fn list_item(text: &str, level: u8) -> ListItem {
    ListItem {
        blocks: vec![Block {
            kind: BlockKind::Paragraph(Paragraph {
                runs: vec![Inline::Run(InlineRun {
                    text: text.to_string(),
                    style: CharStyle::default(),
                    source_index: None,
                })],
                ..Paragraph::default()
            }),
            ..Block::default()
        }],
        level,
    }
}

/// Walk the diagram data model. Returns `(points, parent→children)` where
/// `points` holds the node points (id + joined text) in document order and the
/// map carries the `dgm:cxnLst` `parOf` hierarchy.
fn parse_smartart_model(
    xml: &str,
) -> (
    Vec<SaPoint>,
    std::collections::BTreeMap<String, Vec<String>>,
) {
    let mut points: Vec<SaPoint> = Vec::new();
    let mut children: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    let mut x = Xml::new(xml);

    // Point context.
    let mut cur: Option<SaPoint> = None;
    let mut in_text_body = false; // inside the point's `dgm:t`
    let mut in_run = false; // inside an `a:t`
    let mut text_buf = String::new();

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "pt" => {
                        let id = attr(&attrs, "modelId").unwrap_or("").to_string();
                        let ty = attr(&attrs, "type").unwrap_or("node");
                        // Keep only real node points for the hierarchy; `pres`,
                        // `doc`, `asst`, `parTrans`/`sibTrans` are layout/aux.
                        let is_node = matches!(ty, "node" | "norm" | "");
                        let pt = SaPoint {
                            id,
                            is_node,
                            text: String::new(),
                        };
                        if sc {
                            if pt.is_node {
                                points.push(pt);
                            }
                        } else {
                            cur = Some(pt);
                            text_buf.clear();
                            in_text_body = false;
                        }
                    }
                    "t" if cur.is_some() => {
                        // The outer `dgm:t` opens the text body; the inner `a:t`
                        // carries the run text.
                        if !in_text_body {
                            in_text_body = true;
                            text_buf.clear();
                        } else {
                            in_run = true;
                        }
                    }
                    "cxn" => {
                        // A connection: `parOf` links a parent to one child.
                        let ty = attr(&attrs, "type").unwrap_or("parOf");
                        if ty == "parOf" {
                            if let (Some(src), Some(dst)) =
                                (attr(&attrs, "srcId"), attr(&attrs, "destId"))
                            {
                                children
                                    .entry(src.to_string())
                                    .or_default()
                                    .push(dst.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Tok::Text(t) => {
                if in_text_body && in_run {
                    text_buf.push_str(&t);
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                match ln {
                    "t" => {
                        if in_run {
                            in_run = false;
                        } else if in_text_body {
                            in_text_body = false;
                            if let Some(c) = cur.as_mut() {
                                let t = text_buf.trim();
                                if c.text.is_empty() && !t.is_empty() {
                                    c.text = t.to_string();
                                }
                            }
                        }
                    }
                    "pt" => {
                        if let Some(c) = cur.take() {
                            if c.is_node {
                                points.push(c);
                            }
                        }
                        in_text_body = false;
                        in_run = false;
                    }
                    _ => {}
                }
            }
        }
    }
    (points, children)
}

/// Lower the laid-out SmartArt drawing part (`dsp:drawing`): each `dsp:sp` with a
/// transform and (fill/stroke or text) becomes a positioned block — a
/// [`Shape`](crate::model::Shape) box and, when the shape carries text, a text
/// [`Paragraph`] over it. Best-effort; shapes lacking a transform are skipped.
fn smartart_drawing_shapes(xml: &str, out: &mut Vec<Block>) {
    let mut x = Xml::new(xml);
    // Per-shape accumulation.
    let mut in_sp = false;
    let mut depth_sp = 0usize;
    let mut xfrm: Option<(f64, f64, f64, f64)> = None; // off x,y + ext cx,cy (EMU)
    let mut fill: Option<[f64; 3]> = None;
    let mut stroke: Option<[f64; 3]> = None;
    let mut text = String::new();
    let mut in_txbody = false;
    let mut in_run = false;
    // Colour context: distinguish solidFill in a line vs shape fill.
    let mut in_ln = false;
    let mut got_shape_fill = false;
    // We are inside an `a:xfrm` (capture off/ext).
    let mut in_xfrm = false;
    let mut off: Option<(f64, f64)> = None;
    let mut ext: Option<(f64, f64)> = None;

    let flush = |xfrm: &Option<(f64, f64, f64, f64)>,
                 fill: &Option<[f64; 3]>,
                 stroke: &Option<[f64; 3]>,
                 text: &str,
                 out: &mut Vec<Block>| {
        let Some((ox, oy, cx, cy)) = *xfrm else {
            return;
        };
        let x = emu_to_pt(ox);
        let y = emu_to_pt(oy);
        let w = emu_to_pt(cx).max(1.0);
        let h = emu_to_pt(cy).max(1.0);
        if fill.is_some() || stroke.is_some() {
            // Box rectangle in box-local Y-up space.
            let segs = vec![
                PathSeg::Move(0.0, 0.0),
                PathSeg::Line(w, 0.0),
                PathSeg::Line(w, h),
                PathSeg::Line(0.0, h),
                PathSeg::Close,
            ];
            out.push(Block {
                frame: Some(Rect::new(x, y, w, h)),
                kind: BlockKind::Shape(Shape {
                    segments: segs,
                    fill: *fill,
                    stroke: *stroke,
                    stroke_width: if stroke.is_some() { 1.0 } else { 0.0 },
                    dash: Vec::new(),
                }),
                ..Block::default()
            });
        }
        let t = text.trim();
        if !t.is_empty() {
            out.push(Block {
                frame: Some(Rect::new(x, y, w, h)),
                kind: BlockKind::Paragraph(Paragraph {
                    style: ParagraphStyle {
                        align: Align::Center,
                        ..ParagraphStyle::default()
                    },
                    runs: vec![Inline::Run(InlineRun {
                        text: t.to_string(),
                        style: CharStyle::default(),
                        source_index: None,
                    })],
                    ..Paragraph::default()
                }),
                ..Block::default()
            });
        }
    };

    while let Some(tok) = x.next() {
        match tok {
            Tok::Open(name, attrs, sc) => {
                let ln = local(&name);
                match ln {
                    "sp" if !sc => {
                        in_sp = true;
                        depth_sp += 1;
                        xfrm = None;
                        off = None;
                        ext = None;
                        fill = None;
                        stroke = None;
                        text.clear();
                        got_shape_fill = false;
                    }
                    "xfrm" if in_sp => in_xfrm = true,
                    "off" if in_xfrm => {
                        let ex = attr(&attrs, "x").and_then(|v| v.parse::<f64>().ok());
                        let ey = attr(&attrs, "y").and_then(|v| v.parse::<f64>().ok());
                        if let (Some(a), Some(b)) = (ex, ey) {
                            off = Some((a, b));
                        }
                    }
                    "ext" if in_xfrm => {
                        let cx = attr(&attrs, "cx").and_then(|v| v.parse::<f64>().ok());
                        let cy = attr(&attrs, "cy").and_then(|v| v.parse::<f64>().ok());
                        if let (Some(a), Some(b)) = (cx, cy) {
                            ext = Some((a, b));
                        }
                    }
                    "ln" if in_sp => in_ln = true,
                    "srgbClr" if in_sp => {
                        if let Some(rgb) = attr(&attrs, "val").and_then(parse_hex_rgb) {
                            if in_ln {
                                if stroke.is_none() {
                                    stroke = Some(rgb);
                                }
                            } else if !got_shape_fill {
                                fill = Some(rgb);
                                got_shape_fill = true;
                            }
                        }
                    }
                    "txBody" if in_sp => in_txbody = true,
                    "t" if in_txbody => in_run = true,
                    _ => {}
                }
            }
            Tok::Text(t) => {
                if in_txbody && in_run {
                    text.push_str(&t);
                }
            }
            Tok::Close(name) => {
                let ln = local(&name);
                match ln {
                    "xfrm" => {
                        in_xfrm = false;
                        if let (Some((ox, oy)), Some((cx, cy))) = (off, ext) {
                            xfrm = Some((ox, oy, cx, cy));
                        }
                    }
                    "ln" => in_ln = false,
                    "t" if in_txbody => in_run = false,
                    "txBody" => in_txbody = false,
                    "sp" => {
                        depth_sp = depth_sp.saturating_sub(1);
                        if depth_sp == 0 && in_sp {
                            in_sp = false;
                            flush(&xfrm, &fill, &stroke, &text, out);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// EMU → points (914 400 EMU = 1 inch = 72 pt).
fn emu_to_pt(emu: f64) -> f64 {
    emu / 914_400.0 * 72.0
}

/// Parse a 6-hex-digit `RRGGBB` colour to RGB `0.0..=1.0`. `None` if malformed.
fn parse_hex_rgb(s: &str) -> Option<[f64; 3]> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0])
}

// ──────────────────────────────────── tests ───────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Count the `Shape` blocks in a slice.
    fn shapes(blocks: &[Block]) -> Vec<&Shape> {
        blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Shape(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    /// The first `Table` block, if any.
    fn first_table(blocks: &[Block]) -> Option<&Table> {
        blocks.iter().find_map(|b| match &b.kind {
            BlockKind::Table(t) => Some(t),
            _ => None,
        })
    }

    /// Count filled rectangle shapes (4 line segments + close, with a fill).
    fn filled_rects(blocks: &[Block]) -> usize {
        shapes(blocks)
            .iter()
            .filter(|s| {
                s.fill.is_some()
                    && s.segments.len() == 5
                    && matches!(s.segments[0], PathSeg::Move(..))
                    && matches!(s.segments.last(), Some(PathSeg::Close))
                    && s.segments[1..4]
                        .iter()
                        .all(|seg| matches!(seg, PathSeg::Line(..)))
            })
            .count()
    }

    /// Concatenate all paragraph text in a cell.
    fn cell_str(cell: &crate::model::Cell) -> String {
        let mut s = String::new();
        for b in &cell.blocks {
            if let BlockKind::Paragraph(p) = &b.kind {
                for run in &p.runs {
                    if let Inline::Run(r) = run {
                        s.push_str(&r.text);
                    }
                }
            }
        }
        s
    }

    const BAR_CHART: &str = r#"<?xml version="1.0"?>
<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
              xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart>
    <c:title><c:tx><c:rich><a:p><a:r><a:t>Quarterly Sales</a:t></a:r></a:p></c:rich></c:tx></c:title>
    <c:plotArea>
      <c:barChart>
        <c:barDir val="col"/>
        <c:grouping val="clustered"/>
        <c:ser>
          <c:idx val="0"/>
          <c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>Revenue</c:v></c:pt></c:strCache></c:strRef></c:tx>
          <c:cat>
            <c:strRef><c:strCache>
              <c:pt idx="0"><c:v>Q1</c:v></c:pt>
              <c:pt idx="1"><c:v>Q2</c:v></c:pt>
              <c:pt idx="2"><c:v>Q3</c:v></c:pt>
            </c:strCache></c:strRef>
          </c:cat>
          <c:val>
            <c:numRef><c:numCache>
              <c:pt idx="0"><c:v>10</c:v></c:pt>
              <c:pt idx="1"><c:v>25</c:v></c:pt>
              <c:pt idx="2"><c:v>17</c:v></c:pt>
            </c:numCache></c:numRef>
          </c:val>
        </c:ser>
        <c:axId val="1"/>
        <c:axId val="2"/>
      </c:barChart>
      <c:catAx><c:axId val="1"/></c:catAx>
      <c:valAx><c:axId val="2"/></c:valAx>
    </c:plotArea>
    <c:legend><c:legendPos val="b"/></c:legend>
  </c:chart>
</c:chartSpace>"#;

    #[test]
    fn bar_chart_renders_three_bars_and_a_data_table() {
        let blocks = parse_chart(BAR_CHART);
        assert!(!blocks.is_empty(), "bar chart should lower to blocks");

        // Title survives.
        assert!(
            blocks.iter().any(|b| matches!(&b.kind, BlockKind::Paragraph(p)
                if p.runs.iter().any(|r| matches!(r, Inline::Run(ir) if ir.text == "Quarterly Sales")))),
            "chart title text should appear",
        );

        // At least the 3 category bars are present as filled rectangles (axis grid
        // lines are unfilled strokes, so they don't count here).
        assert!(
            filled_rects(&blocks) >= 3,
            "expected ≥3 filled bar rects, got {}",
            filled_rects(&blocks),
        );

        // Data table: header + 3 category rows, values 10/25/17 in the value column.
        let table = first_table(&blocks).expect("a data table block");
        assert_eq!(table.rows.len(), 4, "header + 3 category rows");
        assert!(table.rows[0].is_header);
        let vals: Vec<String> = table.rows[1..]
            .iter()
            .map(|r| cell_str(&r.cells[1]))
            .collect();
        assert_eq!(vals, vec!["10", "25", "17"]);
        // Category labels in column 0.
        let cats: Vec<String> = table.rows[1..]
            .iter()
            .map(|r| cell_str(&r.cells[0]))
            .collect();
        assert_eq!(cats, vec!["Q1", "Q2", "Q3"]);
    }

    const PIE_CHART: &str = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart><c:plotArea>
    <c:pieChart>
      <c:ser>
        <c:cat><c:strRef><c:strCache>
          <c:pt idx="0"><c:v>Apples</c:v></c:pt>
          <c:pt idx="1"><c:v>Pears</c:v></c:pt>
          <c:pt idx="2"><c:v>Plums</c:v></c:pt>
        </c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache>
          <c:pt idx="0"><c:v>3</c:v></c:pt>
          <c:pt idx="1"><c:v>5</c:v></c:pt>
          <c:pt idx="2"><c:v>2</c:v></c:pt>
        </c:numCache></c:numRef></c:val>
      </c:ser>
    </c:pieChart>
  </c:plotArea></c:chart>
</c:chartSpace>"#;

    #[test]
    fn pie_chart_renders_three_arc_wedges() {
        let blocks = parse_chart(PIE_CHART);
        // Three slices → three wedge shapes that include a cubic Bézier arc.
        let wedge_count = shapes(&blocks)
            .iter()
            .filter(|s| {
                s.fill.is_some()
                    && s.segments
                        .iter()
                        .any(|seg| matches!(seg, PathSeg::Cubic(..)))
            })
            .count();
        assert_eq!(wedge_count, 3, "expected 3 arc wedges, got {wedge_count}");

        // The data table still carries the three values.
        let table = first_table(&blocks).expect("pie data table");
        assert_eq!(table.rows.len(), 4);
    }

    const LINE_CHART: &str = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart><c:plotArea>
    <c:lineChart>
      <c:ser>
        <c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>Temp</c:v></c:pt></c:strCache></c:strRef></c:tx>
        <c:cat><c:strRef><c:strCache>
          <c:pt idx="0"><c:v>Mon</c:v></c:pt>
          <c:pt idx="1"><c:v>Tue</c:v></c:pt>
          <c:pt idx="2"><c:v>Wed</c:v></c:pt>
          <c:pt idx="3"><c:v>Thu</c:v></c:pt>
        </c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache>
          <c:pt idx="0"><c:v>12</c:v></c:pt>
          <c:pt idx="1"><c:v>15</c:v></c:pt>
          <c:pt idx="2"><c:v>9</c:v></c:pt>
          <c:pt idx="3"><c:v>20</c:v></c:pt>
        </c:numCache></c:numRef></c:val>
      </c:ser>
    </c:lineChart>
  </c:plotArea></c:chart>
</c:chartSpace>"#;

    #[test]
    fn line_chart_renders_a_polyline() {
        let blocks = parse_chart(LINE_CHART);
        // A polyline series: a stroked, unfilled shape with one Move + ≥3 Lines and
        // NO Close (distinguishes it from the axis/grid single-segment lines and
        // from filled rects).
        let polylines = shapes(&blocks)
            .iter()
            .filter(|s| {
                s.fill.is_none()
                    && s.stroke.is_some()
                    && matches!(s.segments.first(), Some(PathSeg::Move(..)))
                    && s.segments
                        .iter()
                        .filter(|seg| matches!(seg, PathSeg::Line(..)))
                        .count()
                        >= 3
                    && !s.segments.iter().any(|seg| matches!(seg, PathSeg::Close))
            })
            .count();
        assert!(
            polylines >= 1,
            "expected ≥1 polyline shape, got {polylines}",
        );

        let table = first_table(&blocks).expect("line data table");
        assert_eq!(table.rows.len(), 5, "header + 4 categories");
    }

    const SMARTART_DATA: &str = r#"<dgm:dataModel xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram"
                xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <dgm:ptLst>
    <dgm:pt modelId="1" type="node"><dgm:t><a:p><a:r><a:t>Root</a:t></a:r></a:p></dgm:t></dgm:pt>
    <dgm:pt modelId="2" type="node"><dgm:t><a:p><a:r><a:t>Child A</a:t></a:r></a:p></dgm:t></dgm:pt>
    <dgm:pt modelId="3" type="node"><dgm:t><a:p><a:r><a:t>Child B</a:t></a:r></a:p></dgm:t></dgm:pt>
    <dgm:pt modelId="4" type="node"><dgm:t><a:p><a:r><a:t>Grandchild</a:t></a:r></a:p></dgm:t></dgm:pt>
  </dgm:ptLst>
  <dgm:cxnLst>
    <dgm:cxn modelId="10" type="parOf" srcId="1" destId="2"/>
    <dgm:cxn modelId="11" type="parOf" srcId="1" destId="3"/>
    <dgm:cxn modelId="12" type="parOf" srcId="2" destId="4"/>
  </dgm:cxnLst>
</dgm:dataModel>"#;

    #[test]
    fn smartart_two_level_hierarchy_becomes_nested_list() {
        let blocks = parse_smartart(SMARTART_DATA, None);
        let list = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::List(l) => Some(l),
                _ => None,
            })
            .expect("a List block");

        // Flatten (text, level) in document order.
        let flat: Vec<(String, u8)> = list
            .items
            .iter()
            .map(|it| {
                let mut t = String::new();
                for b in &it.blocks {
                    if let BlockKind::Paragraph(p) = &b.kind {
                        for run in &p.runs {
                            if let Inline::Run(r) = run {
                                t.push_str(&r.text);
                            }
                        }
                    }
                }
                (t, it.level)
            })
            .collect();

        assert_eq!(
            flat,
            vec![
                ("Root".to_string(), 0),
                ("Child A".to_string(), 1),
                ("Grandchild".to_string(), 2),
                ("Child B".to_string(), 1),
            ],
            "depth-first hierarchy with correct nesting levels",
        );
    }

    #[test]
    fn smartart_drawing_part_lowers_shapes_and_text() {
        let drawing = r#"<dsp:drawing xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram"
                          xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <dsp:spTree>
    <dsp:sp>
      <dsp:spPr>
        <a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="457200"/></a:xfrm>
        <a:solidFill><a:srgbClr val="4472C4"/></a:solidFill>
      </dsp:spPr>
      <dsp:txBody><a:p><a:r><a:t>Step 1</a:t></a:r></a:p></dsp:txBody>
    </dsp:sp>
  </dsp:spTree>
</dsp:drawing>"#;
        let blocks = parse_smartart(SMARTART_DATA, Some(drawing));
        // The list is still there.
        assert!(blocks.iter().any(|b| matches!(&b.kind, BlockKind::List(_))));
        // Plus a shape (the box) and a text paragraph from the drawing.
        assert!(
            shapes(&blocks).iter().any(|s| s.fill
                == Some([
                    0x44 as f64 / 255.0,
                    0x72 as f64 / 255.0,
                    0xC4 as f64 / 255.0
                ])),
            "drawing shape with its fill colour",
        );
        assert!(
            blocks
                .iter()
                .any(|b| matches!(&b.kind, BlockKind::Paragraph(p)
                if p.runs.iter().any(|r| matches!(r, Inline::Run(ir) if ir.text == "Step 1")))),
            "drawing shape text",
        );
    }

    #[test]
    fn malformed_inputs_never_panic_and_return_empty() {
        assert!(parse_chart("").is_empty());
        assert!(parse_chart("<c:chartSpace><not closed").is_empty());
        assert!(parse_chart("<c:chartSpace></c:chartSpace>").is_empty());
        // A chart with a type but no series → nothing.
        assert!(parse_chart(
            r#"<c:chartSpace xmlns:c="x"><c:chart><c:plotArea><c:barChart/></c:plotArea></c:chart></c:chartSpace>"#
        )
        .is_empty());
        assert!(parse_smartart("", None).is_empty());
        assert!(parse_smartart("<dgm:dataModel>", None).is_empty());
    }

    #[test]
    fn stacked_columns_share_a_category_slot() {
        let xml = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart><c:plotArea>
    <c:barChart>
      <c:barDir val="col"/><c:grouping val="stacked"/>
      <c:ser>
        <c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt></c:strCache></c:strRef></c:tx>
        <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>X</c:v></c:pt><c:pt idx="1"><c:v>Y</c:v></c:pt></c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>3</c:v></c:pt><c:pt idx="1"><c:v>4</c:v></c:pt></c:numCache></c:numRef></c:val>
      </c:ser>
      <c:ser>
        <c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>B</c:v></c:pt></c:strCache></c:strRef></c:tx>
        <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>X</c:v></c:pt><c:pt idx="1"><c:v>Y</c:v></c:pt></c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>2</c:v></c:pt><c:pt idx="1"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val>
      </c:ser>
    </c:barChart>
  </c:plotArea></c:chart>
</c:chartSpace>"#;
        let blocks = parse_chart(xml);
        // Two series × two categories = 4 stacked segment rects.
        assert!(
            filled_rects(&blocks) >= 4,
            "expected ≥4 stacked segment rects, got {}",
            filled_rects(&blocks),
        );
        // Table: header + 2 category rows, 3 columns (cat + 2 series).
        let table = first_table(&blocks).unwrap();
        assert_eq!(table.rows.len(), 3);
        assert_eq!(table.rows[0].cells.len(), 3);
    }

    /// All paragraph/label texts in document order.
    fn label_texts(blocks: &[Block]) -> Vec<String> {
        blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Paragraph(p) => {
                    let mut s = String::new();
                    for run in &p.runs {
                        if let Inline::Run(r) = run {
                            s.push_str(&r.text);
                        }
                    }
                    Some(s)
                }
                _ => None,
            })
            .collect()
    }

    /// Count fill-less stroked shapes carrying **≥2** `Move` segments: a glyph
    /// text path (one `Move` per stroke) — distinct from axis/grid lines and data
    /// polylines, which all start with exactly one `Move`.
    fn text_path_shapes(blocks: &[Block]) -> usize {
        shapes(blocks)
            .iter()
            .filter(|s| {
                s.fill.is_none()
                    && s.stroke.is_some()
                    && s.segments
                        .iter()
                        .filter(|seg| matches!(seg, PathSeg::Move(..)))
                        .count()
                        >= 2
            })
            .count()
    }

    // #109 — the value-axis title is drawn as a rotated text path (a stroked,
    // fill-less shape with one Move per glyph stroke), not a horizontal label.
    #[test]
    fn value_axis_title_renders_as_a_rotated_text_path() {
        let xml = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
              xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart><c:plotArea>
    <c:barChart><c:barDir val="col"/><c:grouping val="clustered"/>
      <c:ser>
        <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt><c:pt idx="1"><c:v>B</c:v></c:pt></c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>4</c:v></c:pt><c:pt idx="1"><c:v>9</c:v></c:pt></c:numCache></c:numRef></c:val>
      </c:ser>
    </c:barChart>
    <c:catAx><c:axId val="1"/></c:catAx>
    <c:valAx><c:axId val="2"/><c:title><c:tx><c:rich><a:p><a:r><a:t>UNITS</a:t></a:r></a:p></c:rich></c:tx></c:title></c:valAx>
  </c:plotArea></c:chart>
</c:chartSpace>"#;
        let blocks = parse_chart(xml);
        assert!(
            text_path_shapes(&blocks) >= 1,
            "value-axis title should be a rotated text path, found {} glyph-path shapes",
            text_path_shapes(&blocks),
        );
        // And it is NOT emitted as a plain horizontal "UNITS" paragraph.
        assert!(
            !label_texts(&blocks).iter().any(|t| t == "UNITS"),
            "value-axis title must be a path, not a horizontal label",
        );
    }

    // #110 — the category-axis title is placed in BOTH bar and column paths: a
    // horizontal label below a column chart, a rotated text path beside a bar.
    #[test]
    fn category_axis_title_in_both_column_and_bar_paths() {
        let column = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
              xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart><c:plotArea>
    <c:barChart><c:barDir val="col"/>
      <c:ser>
        <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>A</c:v></c:pt><c:pt idx="1"><c:v>B</c:v></c:pt></c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>4</c:v></c:pt><c:pt idx="1"><c:v>9</c:v></c:pt></c:numCache></c:numRef></c:val>
      </c:ser>
    </c:barChart>
    <c:catAx><c:axId val="1"/><c:title><c:tx><c:rich><a:p><a:r><a:t>Quarter</a:t></a:r></a:p></c:rich></c:tx></c:title></c:catAx>
    <c:valAx><c:axId val="2"/></c:valAx>
  </c:plotArea></c:chart>
</c:chartSpace>"#;
        let blocks = parse_chart(column);
        assert!(
            label_texts(&blocks).iter().any(|t| t == "Quarter"),
            "column chart should render the category-axis title below the plot",
        );

        // The same title on a horizontal bar chart → a rotated text path (the
        // category axis runs vertically there), so no horizontal "Quarter" label
        // but a glyph-path shape is present.
        let bar = column.replace(r#"<c:barDir val="col"/>"#, r#"<c:barDir val="bar"/>"#);
        let bblocks = parse_chart(&bar);
        assert!(
            text_path_shapes(&bblocks) >= 1,
            "horizontal bar chart should render the category-axis title as a rotated path",
        );
    }

    // #111 — the radar grid rings follow `nice_step` (more than the old hardcoded
    // two) and at least one ring carries a numeric value label.
    #[test]
    fn radar_rings_use_nice_step_and_are_labelled() {
        let xml = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart><c:plotArea>
    <c:radarChart>
      <c:ser>
        <c:cat><c:strRef><c:strCache>
          <c:pt idx="0"><c:v>N</c:v></c:pt><c:pt idx="1"><c:v>E</c:v></c:pt>
          <c:pt idx="2"><c:v>S</c:v></c:pt><c:pt idx="3"><c:v>W</c:v></c:pt>
        </c:strCache></c:strRef></c:cat>
        <c:val><c:numRef><c:numCache>
          <c:pt idx="0"><c:v>10</c:v></c:pt><c:pt idx="1"><c:v>40</c:v></c:pt>
          <c:pt idx="2"><c:v>25</c:v></c:pt><c:pt idx="3"><c:v>50</c:v></c:pt>
        </c:numCache></c:numRef></c:val>
      </c:ser>
    </c:radarChart>
  </c:plotArea></c:chart>
</c:chartSpace>"#;
        let blocks = parse_chart(xml);
        // Closed, fill-less, GRID-coloured polygons are the rings. With hi=50 and
        // nice_step(50, 4) = 20, rings at 20/40/60 → 3 rings (> the old 2).
        let rings = shapes(&blocks)
            .iter()
            .filter(|s| {
                s.fill.is_none()
                    && s.stroke == Some(GRID_RGB)
                    && s.segments.iter().any(|seg| matches!(seg, PathSeg::Close))
                    && s.segments
                        .iter()
                        .filter(|seg| matches!(seg, PathSeg::Line(..)))
                        .count()
                        >= 3
            })
            .count();
        assert!(rings >= 3, "expected ≥3 nice-stepped rings, got {rings}");
        // At least one ring value label (a multiple of the step, e.g. "20").
        assert!(
            label_texts(&blocks).iter().any(|t| t == "20"),
            "a ring level should be labelled along the top spoke",
        );
    }

    // #112 — a scatter with categories but no xVal labels the X axis with the
    // evenly-spaced category text, not bare integer indices.
    #[test]
    fn scatter_without_x_values_labels_categories() {
        let xml = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart">
  <c:chart><c:plotArea>
    <c:scatterChart>
      <c:ser>
        <c:cat><c:strRef><c:strCache>
          <c:pt idx="0"><c:v>Alpha</c:v></c:pt>
          <c:pt idx="1"><c:v>Beta</c:v></c:pt>
          <c:pt idx="2"><c:v>Gamma</c:v></c:pt>
        </c:strCache></c:strRef></c:cat>
        <c:yVal><c:numRef><c:numCache>
          <c:pt idx="0"><c:v>7</c:v></c:pt>
          <c:pt idx="1"><c:v>3</c:v></c:pt>
          <c:pt idx="2"><c:v>9</c:v></c:pt>
        </c:numCache></c:numRef></c:yVal>
      </c:ser>
    </c:scatterChart>
  </c:plotArea></c:chart>
</c:chartSpace>"#;
        let blocks = parse_chart(xml);
        let texts = label_texts(&blocks);
        for want in ["Alpha", "Beta", "Gamma"] {
            assert!(
                texts.iter().any(|t| t == want),
                "scatter without xVal should show category label {want:?}",
            );
        }
    }

    // #113 — a doughnut (a hole + a title) draws a centred label in the hole, so
    // the title text appears twice (top title + centre), while a solid pie with
    // the same title draws it only once.
    #[test]
    fn doughnut_draws_a_centre_label_from_the_title() {
        let doughnut = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
              xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart>
    <c:title><c:tx><c:rich><a:p><a:r><a:t>Share</a:t></a:r></a:p></c:rich></c:tx></c:title>
    <c:plotArea>
      <c:doughnutChart>
        <c:ser>
          <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>X</c:v></c:pt><c:pt idx="1"><c:v>Y</c:v></c:pt></c:strCache></c:strRef></c:cat>
          <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>6</c:v></c:pt><c:pt idx="1"><c:v>4</c:v></c:pt></c:numCache></c:numRef></c:val>
        </c:ser>
        <c:holeSize val="55"/>
      </c:doughnutChart>
    </c:plotArea>
  </c:chart>
</c:chartSpace>"#;
        let dblocks = parse_chart(doughnut);
        let dcount = label_texts(&dblocks)
            .iter()
            .filter(|t| *t == "Share")
            .count();
        assert!(
            dcount >= 2,
            "doughnut should add a centre label (title at top + centre), got {dcount}",
        );

        // A solid pie (no hole) draws the title only at the top.
        let pie = doughnut
            .replace(r#"<c:doughnutChart>"#, r#"<c:pieChart>"#)
            .replace(r#"</c:doughnutChart>"#, r#"</c:pieChart>"#)
            .replace(r#"<c:holeSize val="55"/>"#, "");
        let pblocks = parse_chart(&pie);
        let pcount = label_texts(&pblocks)
            .iter()
            .filter(|t| *t == "Share")
            .count();
        assert_eq!(pcount, 1, "a solid pie draws no centre label");
    }

    // #114 — a legend with many series wraps onto multiple rows: the swatches are
    // placed at more than one distinct Y (the strip grew taller).
    #[test]
    fn legend_wraps_many_series_onto_multiple_rows() {
        let mut xml = String::from(
            r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"
              xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <c:chart><c:plotArea><c:barChart><c:barDir val="col"/>"#,
        );
        // Twelve named series, each with a single category value.
        for i in 0..12 {
            xml.push_str(&format!(
                r#"<c:ser>
  <c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>SeriesNumber{i}</c:v></c:pt></c:strCache></c:strRef></c:tx>
  <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>K</c:v></c:pt></c:strCache></c:strRef></c:cat>
  <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>{}</c:v></c:pt></c:numCache></c:numRef></c:val>
</c:ser>"#,
                i + 1
            ));
        }
        xml.push_str(
            r#"</c:barChart><c:catAx><c:axId val="1"/></c:catAx><c:valAx><c:axId val="2"/></c:valAx></c:plotArea><c:legend/></c:chart></c:chartSpace>"#,
        );
        let blocks = parse_chart(&xml);
        // Legend swatches are the 8×8 filled rects sitting in the bottom strip
        // (Y-up centre < LEGEND_H, i.e. their top in Y-down terms is large).
        // Collect distinct frame tops of the small legend labels instead: legend
        // labels are paragraphs named "SeriesNumberN"; rows differ in frame.y.
        let mut rows: Vec<i64> = blocks
            .iter()
            .filter_map(|b| match (&b.kind, b.frame) {
                (BlockKind::Paragraph(p), Some(f))
                    if p.runs.iter().any(|r| {
                        matches!(r, Inline::Run(ir)
                        if ir.text.starts_with("SeriesNumber"))
                    }) =>
                {
                    Some((f.y * 10.0).round() as i64)
                }
                _ => None,
            })
            .collect();
        rows.sort_unstable();
        rows.dedup();
        assert!(
            rows.len() >= 2,
            "legend with 12 series should wrap onto ≥2 rows, got {} distinct row(s)",
            rows.len(),
        );
    }
}
