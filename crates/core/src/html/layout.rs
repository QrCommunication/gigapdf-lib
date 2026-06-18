//! Box-tree layout: turn a styled DOM into positioned fragments across pages.
//!
//! Implements a real (if pragmatic) CSS visual formatting model: the block
//! formatting context stacks block boxes vertically honouring the box model
//! (margin / border / padding / background), and the inline formatting context
//! flows text + inline boxes into line boxes, breaking lines using **actual font
//! metrics** supplied by [`Measure`] (the paint layer plugs in embedded Google
//! fonts). Lists get markers, tables lay cells side-by-side, and the whole flow
//! is sliced into pages with backgrounds/borders split across page bands.

use super::css::{Align, Display, Justify, Len, Style, Stylesheet};
use super::dom::{Element, Node};
use crate::svg::SvgImage;

/// Text-measurement hook. The paint layer implements this over the embedded
/// TrueType fonts (real advance widths); [`AverageMeasure`] is the fallback.
pub trait Measure {
    /// Advance width of `text` in points for the given computed style.
    fn width(&self, text: &str, style: &Style) -> f64;
}

/// A positioned output fragment in absolute top-down points (pre-pagination).
// `Text` legitimately carries the full computed `Style`, so it's larger than the
// geometric variants; these fragments are transient render output, so the size
// asymmetry is acceptable rather than boxing every text run.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Fragment {
    Text {
        x: f64,
        y: f64,
        style: Style,
        text: String,
    },
    Rect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        fill: Option<[f64; 3]>,
        stroke: Option<[f64; 3]>,
        stroke_w: f64,
        /// `opacity` (0..=1) applied to the fill and stroke.
        opacity: f64,
    },
    Image {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        src: String,
    },
    /// A vector SVG placed at `(x, y)` with size `w×h` (top-down), drawn as
    /// native PDF paths (not rasterized) by the paint layer.
    Svg {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        image: SvgImage,
    },
}

#[derive(Debug, Clone)]
struct Abs {
    /// z=0 backgrounds/borders, z=1 content (text/images) — paint order.
    z: u8,
    frag: Fragment,
}

/// The laid-out document: fragments grouped per page (top-down points).
#[derive(Debug, Clone)]
pub struct Layout {
    pub pages: Vec<Vec<Fragment>>,
    pub page_w: f64,
    pub page_h: f64,
}

/// The page box plus the content insets (per-side margins), in points.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub page_w: f64,
    pub page_h: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

/// Lay out `nodes` onto pages of `page_w`×`page_h` with a uniform `margin`
/// around the content area (convenience wrapper over [`layout_document_framed`]).
pub fn layout_document(
    nodes: &[Node],
    sheet: &Stylesheet,
    measure: &dyn Measure,
    page_w: f64,
    page_h: f64,
    margin: f64,
) -> Layout {
    layout_document_framed(
        nodes,
        sheet,
        measure,
        &Frame {
            page_w,
            page_h,
            top: margin,
            right: margin,
            bottom: margin,
            left: margin,
        },
    )
}

/// Lay out `nodes` into the content box described by `frame` (per-side margins),
/// paginating the body between `frame.top` and `page_h - frame.bottom`.
pub fn layout_document_framed(
    nodes: &[Node],
    sheet: &Stylesheet,
    measure: &dyn Measure,
    frame: &Frame,
) -> Layout {
    let mut flow = Flow {
        out: Vec::new(),
        m: measure,
        sheet,
        page_h: frame.page_h,
        top: frame.top,
        bottom: frame.bottom,
    };
    let content_w = (frame.page_w - frame.left - frame.right).max(1.0);
    // Find <body> if present, else lay out the whole forest.
    let roots = find_body(nodes).unwrap_or(nodes);
    let root_style = Style {
        display: Display::Block,
        ..Style::default()
    };
    let mut y = frame.top;
    y = flow.block_children(roots, &root_style, frame.left, content_w, y, &[]);
    let _ = y;

    Layout {
        pages: paginate(flow.out, frame.page_h, frame.top, frame.bottom),
        page_w: frame.page_w,
        page_h: frame.page_h,
    }
}

fn find_body(nodes: &[Node]) -> Option<&[Node]> {
    for n in nodes {
        if let Node::Element(e) = n {
            if e.tag == "body" {
                return Some(&e.children);
            }
            if e.tag == "html" {
                if let Some(b) = find_body(&e.children) {
                    return Some(b);
                }
            }
        }
    }
    None
}

struct Flow<'a> {
    out: Vec<Abs>,
    m: &'a dyn Measure,
    sheet: &'a Stylesheet,
    /// Page height (for resolving `page-break-*` to the next page boundary).
    page_h: f64,
    /// Content-area top inset (page `margin-top`); the body band starts here.
    top: f64,
    /// Content-area bottom inset (page `margin-bottom`).
    bottom: f64,
}

impl Flow<'_> {
    /// Advance `y` to the start of the next page (for `page-break-*: always`).
    /// A `y` already at a page boundary is left unchanged.
    fn break_to_next_page(&self, y: f64) -> f64 {
        let content_h = (self.page_h - self.top - self.bottom).max(1.0);
        let rel = (y - self.top).max(0.0);
        let next_k = (rel / content_h - 1e-9).ceil().max(0.0);
        self.top + next_k * content_h
    }
}

/// An atomic inline item for line breaking.
/// Inline replaced content laid out as a box on the line: a raster image
/// (`w, h, src`) or a vector SVG (`w, h, image`).
#[derive(Clone)]
enum Media {
    Raster(f64, f64, String),
    Svg(f64, f64, SvgImage),
}

impl Media {
    /// The reserved inline-box width in points.
    fn width(&self) -> f64 {
        match self {
            Media::Raster(w, ..) | Media::Svg(w, ..) => *w,
        }
    }
}

struct InlineItem {
    text: String,
    style: Style,
    /// Replaced content (`<img>` / inline `<svg>`) laid out as an inline box.
    media: Option<Media>,
}

impl Flow<'_> {
    /// Lay out the children of a block container, partitioning runs of
    /// inline-level content into inline formatting contexts. Returns the bottom
    /// `y`.
    fn block_children(
        &mut self,
        children: &[Node],
        parent_style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        let mut inline_run: Vec<&Node> = Vec::new();
        let mut list_index = 0usize;

        for child in children {
            let is_block = match child {
                Node::Text(t) => {
                    if t.trim().is_empty() {
                        continue; // collapse whitespace between blocks
                    }
                    false
                }
                Node::Element(e) => {
                    let st = self.style_of(e, parent_style, ancestors);
                    matches!(
                        st.display,
                        Display::Block
                            | Display::ListItem
                            | Display::Table
                            | Display::TableRow
                            | Display::Flex
                            | Display::Grid
                    )
                }
            };

            if is_block {
                if !inline_run.is_empty() {
                    y = self.inline_context(&inline_run, parent_style, x, avail_w, y, ancestors);
                    inline_run.clear();
                }
                if let Node::Element(e) = child {
                    let st = self.style_of(e, parent_style, ancestors);
                    if st.display == Display::ListItem {
                        list_index += 1;
                    }
                    if st.page_break_before {
                        y = self.break_to_next_page(y);
                    }
                    y = self.block(e, &st, parent_style, x, avail_w, y, ancestors, list_index);
                    if st.page_break_after {
                        y = self.break_to_next_page(y);
                    }
                }
            } else {
                inline_run.push(child);
            }
        }
        if !inline_run.is_empty() {
            y = self.inline_context(&inline_run, parent_style, x, avail_w, y, ancestors);
        }
        y
    }

    #[allow(clippy::too_many_arguments)]
    fn block(
        &mut self,
        el: &Element,
        style: &Style,
        _parent_style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
        list_index: usize,
    ) -> f64 {
        if style.display == Display::None {
            return y;
        }
        if el.tag == "table" {
            return self.table(el, style, x, avail_w, y, ancestors);
        }
        if style.display == Display::Flex {
            return self.flex(el, style, x, avail_w, y, ancestors);
        }
        if style.display == Display::Grid {
            return self.grid(el, style, x, avail_w, y, ancestors);
        }

        let m = &style.margin;
        let p = &style.padding;
        let b = &style.border_width;

        y += m.top;
        let box_top = y;
        let box_x = x + m.left;
        let resolve_w = |len: Len| match len {
            Len::Pt(w) => w,
            Len::Percent(pc) => avail_w * pc / 100.0,
        };
        let mut box_w = match style.width {
            // `box-sizing: border-box` → `width` already includes padding+border.
            Some(Len::Pt(w)) if style.border_box => w,
            Some(Len::Pt(w)) => w + p.left + p.right + b.left + b.right,
            Some(Len::Percent(pc)) => avail_w * pc / 100.0,
            None => avail_w - m.left - m.right,
        };
        if let Some(mw) = style.max_width {
            box_w = box_w.min(resolve_w(mw));
        }
        if let Some(mw) = style.min_width {
            box_w = box_w.max(resolve_w(mw));
        }
        let content_x = box_x + b.left + p.left;
        let content_w = (box_w - b.left - b.right - p.left - p.right).max(1.0);

        let mut cy = y + b.top + p.top;

        // Marker for list items (honours `list-style-type`).
        if style.display == Display::ListItem {
            if let Some(marker) = list_marker(style, list_marker_ordered(ancestors), list_index) {
                let mstyle = style.clone();
                let mw = self.m.width(&marker, &mstyle);
                self.out.push(Abs {
                    z: 1,
                    frag: Fragment::Text {
                        x: content_x - mw - 4.0,
                        y: cy,
                        style: mstyle,
                        text: marker,
                    },
                });
            }
        }

        let new_ancestors = push_ancestor(ancestors, el);
        cy = self.block_children(
            &el.children,
            style,
            content_x,
            content_w,
            cy,
            &new_ancestors,
        );

        cy += p.bottom + b.bottom;
        let mut box_h = (cy - box_top).max(0.1);
        if let Some(mh) = style.min_height {
            box_h = box_h.max(mh); // `height` / `min-height`
        }

        // Background + border behind the content (z=0). `visibility: hidden`
        // suppresses the paint but the box still occupies its space.
        if !style.hidden
            && (style.background.is_some() || b.top + b.bottom + b.left + b.right > 0.0)
        {
            self.out.push(Abs {
                z: 0,
                frag: Fragment::Rect {
                    x: box_x,
                    y: box_top,
                    w: box_w,
                    h: box_h,
                    fill: style.background,
                    stroke: if b.top > 0.0 {
                        Some(style.border_color)
                    } else {
                        None
                    },
                    stroke_w: b.top,
                    opacity: style.opacity,
                },
            });
        }

        box_top + box_h + m.bottom
    }

    /// Lay out a run of inline nodes into line boxes; returns the bottom `y`.
    fn inline_context(
        &mut self,
        nodes: &[&Node],
        style: &Style,
        x: f64,
        avail_w: f64,
        y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        let mut items = Vec::new();
        for n in nodes {
            self.collect_inline(n, style, ancestors, &mut items);
        }
        self.flow_lines(&items, x, avail_w, y, style.align, style.text_indent)
    }

    fn collect_inline(
        &mut self,
        node: &Node,
        parent_style: &Style,
        ancestors: &[&Element],
        out: &mut Vec<InlineItem>,
    ) {
        match node {
            Node::Text(t) => out.push(InlineItem {
                text: parent_style.text_transform.apply(t),
                style: parent_style.clone(),
                media: None,
            }),
            Node::Element(e) => {
                let st = self.style_of(e, parent_style, ancestors);
                if st.display == Display::None {
                    return;
                }
                if e.tag == "br" {
                    out.push(InlineItem {
                        text: "\n".into(),
                        style: st,
                        media: None,
                    });
                    return;
                }
                // Inline <svg> → native vector box (sized by width/height or viewBox).
                if e.tag == "svg" {
                    if let Some(img) = crate::svg::from_element(e) {
                        let w = e
                            .attr("width")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(img.width.max(1.0));
                        let h = e
                            .attr("height")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(img.height.max(1.0));
                        out.push(InlineItem {
                            text: String::new(),
                            style: st,
                            media: Some(Media::Svg(w, h, img)),
                        });
                    }
                    return;
                }
                if e.tag == "img" {
                    let w = e
                        .attr("width")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(64.0);
                    let h = e
                        .attr("height")
                        .and_then(|v| v.parse::<f64>().ok())
                        .unwrap_or(64.0);
                    let src = e.attr("src").unwrap_or_default().to_string();
                    // A `data:image/svg+xml` source renders as native vector, not a bitmap.
                    let media = crate::svg::parse_data_uri(&src)
                        .map(|img| Media::Svg(w, h, img))
                        .unwrap_or(Media::Raster(w, h, src));
                    out.push(InlineItem {
                        text: String::new(),
                        style: st,
                        media: Some(media),
                    });
                    return;
                }
                let na = push_ancestor(ancestors, e);
                for c in &e.children {
                    self.collect_inline(c, &st, &na, out);
                }
            }
        }
    }

    /// Break inline items into lines and emit positioned text/images.
    /// `indent` (`text-indent`) shifts and shortens the first line only.
    fn flow_lines(
        &mut self,
        items: &[InlineItem],
        x: f64,
        avail_w: f64,
        mut y: f64,
        align: Align,
        indent: f64,
    ) -> f64 {
        // A line is a vector of (text|image, style, width).
        struct Word {
            text: String,
            style: Style,
            w: f64,
            media: Option<Media>,
            space_after: bool,
        }
        let mut words: Vec<Word> = Vec::new();
        for it in items {
            if let Some(m) = &it.media {
                words.push(Word {
                    text: String::new(),
                    style: it.style.clone(),
                    w: m.width(),
                    media: Some(m.clone()),
                    space_after: true,
                });
                continue;
            }
            if it.style.pre {
                // Preserve whitespace: split only on newlines, keep runs.
                for (i, seg) in it.text.split('\n').enumerate() {
                    if i > 0 {
                        words.push(Word {
                            text: "\n".into(),
                            style: it.style.clone(),
                            w: 0.0,
                            media: None,
                            space_after: false,
                        });
                    }
                    if !seg.is_empty() {
                        words.push(Word {
                            text: seg.to_string(),
                            style: it.style.clone(),
                            w: self.m.width(seg, &it.style),
                            media: None,
                            space_after: false,
                        });
                    }
                }
                continue;
            }
            let normalized = collapse_ws(&it.text);
            for token in normalized.split(' ') {
                if token == "\n" {
                    words.push(Word {
                        text: "\n".into(),
                        style: it.style.clone(),
                        w: 0.0,
                        media: None,
                        space_after: false,
                    });
                    continue;
                }
                if token.is_empty() {
                    continue;
                }
                words.push(Word {
                    text: token.to_string(),
                    style: it.style.clone(),
                    w: self.m.width(token, &it.style),
                    media: None,
                    space_after: true,
                });
            }
        }

        let mut line: Vec<&Word> = Vec::new();
        let mut line_w = 0.0;
        let space_w = self.m.width(" ", &Style::default());

        // `line_x` / `line_avail` are per-line (the first line is indented and
        // therefore narrower), so they're passed in rather than captured.
        let flush = |this: &mut Self,
                     line: &mut Vec<&Word>,
                     line_w: f64,
                     y: &mut f64,
                     last: bool,
                     line_x: f64,
                     line_avail: f64| {
            if line.is_empty() {
                *y += default_line_height(&Style::default());
                return;
            }
            let line_h = line
                .iter()
                .map(|w| w.style.font_size * w.style.line_height.max(1.0))
                .fold(0.0_f64, f64::max);
            // Horizontal offset for alignment.
            let extra = (line_avail - line_w).max(0.0);
            let (mut cx, gap_extra) = match align {
                Align::Left => (line_x, 0.0),
                Align::Right => (line_x + extra, 0.0),
                Align::Center => (line_x + extra / 2.0, 0.0),
                Align::Justify => {
                    let gaps = line.iter().filter(|w| w.space_after).count().max(1);
                    (line_x, if last { 0.0 } else { extra / gaps as f64 })
                }
            };
            for w in line.iter() {
                match &w.media {
                    Some(Media::Raster(iw, ih, src)) => {
                        this.out.push(Abs {
                            z: 1,
                            frag: Fragment::Image {
                                x: cx,
                                y: *y,
                                w: *iw,
                                h: *ih,
                                src: src.clone(),
                            },
                        });
                        cx += iw + space_w;
                    }
                    Some(Media::Svg(iw, ih, image)) => {
                        this.out.push(Abs {
                            z: 1,
                            frag: Fragment::Svg {
                                x: cx,
                                y: *y,
                                w: *iw,
                                h: *ih,
                                image: image.clone(),
                            },
                        });
                        cx += iw + space_w;
                    }
                    None => {
                        this.out.push(Abs {
                            z: 1,
                            frag: Fragment::Text {
                                x: cx,
                                y: *y,
                                style: w.style.clone(),
                                text: w.text.clone(),
                            },
                        });
                        cx += w.w
                            + if w.space_after {
                                space_w + gap_extra
                            } else {
                                0.0
                            };
                    }
                }
            }
            *y += line_h;
        };

        // The first line uses a reduced budget `avail_w - indent` and starts at
        // `x + indent`; every subsequent line spans the full width at `x`.
        let mut first_line = true;
        let line_geom = |first: bool| -> (f64, f64) {
            if first {
                (x + indent, (avail_w - indent).max(1.0))
            } else {
                (x, avail_w)
            }
        };

        let mut i = 0;
        while i < words.len() {
            let (line_x, line_avail) = line_geom(first_line);
            let w = &words[i];
            if w.text == "\n" {
                flush(self, &mut line, line_w, &mut y, true, line_x, line_avail);
                line.clear();
                line_w = 0.0;
                first_line = false;
                i += 1;
                continue;
            }
            let add = w.w + if line.is_empty() { 0.0 } else { space_w };
            if !line.is_empty() && line_w + add > line_avail {
                flush(self, &mut line, line_w, &mut y, false, line_x, line_avail);
                line.clear();
                first_line = false;
                // Re-evaluate the same word on the fresh line.
                line.push(w);
                line_w = w.w;
            } else {
                line.push(w);
                line_w += add;
            }
            i += 1;
        }
        let (line_x, line_avail) = line_geom(first_line);
        flush(self, &mut line, line_w, &mut y, true, line_x, line_avail);
        y
    }

    /// Pragmatic table layout. Column widths come from a `<colgroup>`/`<col>`
    /// set or the first row's per-cell `width`, normalised to fit `avail_w`
    /// (fixed-layout style); columns with no declared width share the remainder
    /// equally, so a table that declares nothing keeps **equal** columns. Cells
    /// sit at the cumulative x of their starting column; `colspan` (including the
    /// physical-cell expansion the Office importers emit) covers the summed
    /// width of the columns it spans. Row height = tallest cell.
    fn table(
        &mut self,
        el: &Element,
        style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        y += style.margin.top;
        let na = push_ancestor(ancestors, el);
        let rows = collect_rows(el);

        // Resolve per-column widths once for the whole table, then prefix-sum
        // them so each cell can be placed by its starting column index.
        let ncols = table_column_count(&rows);
        let col_w = self.resolve_col_widths(el, style, &rows, &na, avail_w, ncols);
        let mut cum_x = Vec::with_capacity(col_w.len() + 1);
        let mut acc = 0.0;
        cum_x.push(0.0);
        for w in &col_w {
            acc += w;
            cum_x.push(acc);
        }
        // Width spanning columns `[start, start+span)`, clamped to the grid.
        let span_geom = |start: usize, span: usize| -> (f64, f64) {
            let s = start.min(col_w.len());
            let e = (start + span.max(1)).min(col_w.len());
            (cum_x[s], (cum_x[e] - cum_x[s]).max(1.0))
        };

        for row in rows {
            let cells = collect_cells(row);
            if cells.is_empty() {
                continue;
            }
            let row_top = y;
            let mut row_bottom = y;
            // First pass: lay out content; remember each cell's column span.
            let mut placed: Vec<(usize, usize)> = Vec::with_capacity(cells.len());
            let mut col = 0usize;
            for cell in &cells {
                let cstyle = self.style_of(cell, style, &na);
                let span = cell_colspan(cell);
                let (dx, cw) = span_geom(col, span);
                let cx = x + dx;
                let nca = push_ancestor(&na, cell);
                let p = &cstyle.padding;
                let mut cy = row_top + p.top + cstyle.border_width.top;
                cy = self.block_children(
                    &cell.children,
                    &cstyle,
                    cx + p.left + cstyle.border_width.left,
                    (cw - p.left - p.right).max(1.0),
                    cy,
                    &nca,
                );
                cy += p.bottom + cstyle.border_width.bottom;
                row_bottom = row_bottom.max(cy);
                placed.push((col, span));
                col += span.max(1);
            }
            // Cell borders/backgrounds spanning the full row height (z=0).
            for (cell, &(start, span)) in cells.iter().zip(&placed) {
                let cstyle = self.style_of(cell, style, &na);
                let (dx, cw) = span_geom(start, span);
                self.out.push(Abs {
                    z: 0,
                    frag: Fragment::Rect {
                        x: x + dx,
                        y: row_top,
                        w: cw,
                        h: (row_bottom - row_top).max(0.1),
                        fill: cstyle.background,
                        stroke: Some(cstyle.border_color),
                        stroke_w: cstyle.border_width.top.max(0.5),
                        opacity: cstyle.opacity,
                    },
                });
            }
            y = row_bottom;
        }
        y + style.margin.bottom
    }

    /// Resolve the table's column widths (length `ncols`) to absolute points
    /// summing to `avail_w`. Declared widths come first from `<col>` elements
    /// (honouring `span`), else from the first row's per-cell `width`. Columns
    /// without a declared width split the remaining space equally; if every
    /// column is declared the widths are scaled proportionally to fit `avail_w`
    /// (browser fixed-layout). With nothing declared this yields equal columns.
    fn resolve_col_widths(
        &self,
        table: &Element,
        style: &Style,
        rows: &[&Element],
        na: &[&Element],
        avail_w: f64,
        ncols: usize,
    ) -> Vec<f64> {
        if ncols == 0 {
            return Vec::new();
        }
        let equal = avail_w / ncols as f64;
        let mut decl: Vec<Option<f64>> = vec![None; ncols];

        // Source 1: <colgroup>/<col> declarations (each <col span="N">).
        let cols = collect_cols(table);
        if !cols.is_empty() {
            let mut ci = 0usize;
            for c in cols {
                if ci >= ncols {
                    break;
                }
                let span = cell_colspan(c); // reads `span`/`colspan`
                let w = col_declared_width(c, avail_w);
                for k in 0..span.max(1) {
                    if ci + k < ncols {
                        // A multi-column <col> applies its width per column.
                        decl[ci + k] = w;
                    }
                }
                ci += span.max(1);
            }
        } else if let Some(first) = rows.first() {
            // Source 2: per-cell width on the first row's cells. A colspan cell
            // distributes its declared width equally over the columns it covers.
            let mut ci = 0usize;
            for cell in collect_cells(first) {
                if ci >= ncols {
                    break;
                }
                let span = cell_colspan(cell);
                let cstyle = self.style_of(cell, style, na);
                let w = cstyle.width.map(|len| match len {
                    Len::Pt(pt) => pt.max(0.0),
                    Len::Percent(pc) => avail_w * pc / 100.0,
                });
                if let Some(total) = w {
                    let per = total / span.max(1) as f64;
                    for k in 0..span.max(1) {
                        if ci + k < ncols {
                            decl[ci + k] = Some(per);
                        }
                    }
                }
                ci += span.max(1);
            }
        }

        let declared_sum: f64 = decl.iter().filter_map(|d| *d).sum();
        let undeclared = decl.iter().filter(|d| d.is_none()).count();

        if undeclared == 0 {
            // All columns declared: scale to fit avail_w (fixed-layout). Guard a
            // zero/degenerate sum by falling back to equal columns.
            if declared_sum > 0.0 {
                let scale = avail_w / declared_sum;
                decl.iter().map(|d| d.unwrap_or(equal) * scale).collect()
            } else {
                vec![equal; ncols]
            }
        } else {
            // Undeclared columns share whatever space the declared ones leave.
            let fill = ((avail_w - declared_sum).max(0.0)) / undeclared as f64;
            decl.iter().map(|d| d.unwrap_or(fill)).collect()
        }
    }

    /// A flex container. Supports `flex-direction` (row | column),
    /// `justify-content`, and per-item `flex-grow`. Cross-axis sizing is
    /// `stretch` (items fill the cross dimension); wrap, shrink, `align-items`
    /// and `order` are not modelled.
    fn flex(
        &mut self,
        el: &Element,
        style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        y += style.margin.top;
        let m = &style.margin;
        let p = &style.padding;
        let b = &style.border_width;
        let na = push_ancestor(ancestors, el);

        let items: Vec<&Element> = el
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) => Some(e),
                _ => None,
            })
            .filter(|e| self.style_of(e, style, &na).display != Display::None)
            .collect();
        if items.is_empty() {
            return y + style.margin.bottom;
        }

        let content_x = x + m.left + b.left + p.left;
        let content_w = (avail_w - m.left - m.right - b.left - b.right - p.left - p.right).max(1.0);
        let row_top = y + b.top + p.top;

        let row_bottom = if style.flex_column {
            self.flex_column_axis(&items, style, content_x, content_w, row_top, &na)
        } else {
            self.flex_row_axis(&items, style, content_x, content_w, row_top, &na)
        };

        row_bottom + p.bottom + b.bottom + style.margin.bottom
    }

    /// Horizontal flex: resolve each item's main-axis width, then place them
    /// left-to-right. Width model:
    /// * if any item has an explicit `width`, those are honoured and remaining
    ///   free space is distributed by `flex-grow` (or, if none grow, positioned
    ///   via `justify-content`);
    /// * otherwise every item fills the row with weight `1 + flex-grow` (so the
    ///   default — no grow set — yields equal columns).
    fn flex_row_axis(
        &mut self,
        items: &[&Element],
        style: &Style,
        content_x: f64,
        content_w: f64,
        row_top: f64,
        na: &[&Element],
    ) -> f64 {
        let n = items.len();
        let mut ws: Vec<f64> = Vec::with_capacity(n);
        let mut grows: Vec<f64> = Vec::with_capacity(n);
        let mut any_explicit = false;
        for it in items {
            let st = self.style_of(it, style, na);
            grows.push(st.flex_grow.max(0.0));
            match st.width {
                Some(Len::Pt(w)) => {
                    ws.push(w.max(0.0));
                    any_explicit = true;
                }
                Some(Len::Percent(pc)) => {
                    ws.push(content_w * pc / 100.0);
                    any_explicit = true;
                }
                None => ws.push(f64::NAN), // resolved below
            }
        }
        let total_grow: f64 = grows.iter().sum();

        let (offset, gap) = if any_explicit {
            // Items with no explicit width share the leftover equally as basis.
            let known: f64 = ws.iter().filter(|w| !w.is_nan()).sum();
            let unknown = ws.iter().filter(|w| w.is_nan()).count();
            let fill = if unknown > 0 {
                (content_w - known).max(0.0) / unknown as f64
            } else {
                0.0
            };
            for w in ws.iter_mut() {
                if w.is_nan() {
                    *w = fill;
                }
            }
            let mut free = (content_w - ws.iter().sum::<f64>()).max(0.0);
            if total_grow > 0.0 {
                for (w, g) in ws.iter_mut().zip(&grows) {
                    *w += free * g / total_grow;
                }
                free = 0.0;
            }
            justify_offsets(style.justify, free, n)
        } else {
            // Fill model: weight = 1 + grow, sums to content_w exactly.
            let total_w: f64 = grows.iter().map(|g| 1.0 + g).sum();
            for (w, g) in ws.iter_mut().zip(&grows) {
                *w = content_w * (1.0 + g) / total_w;
            }
            (0.0, 0.0)
        };

        let mut xs: Vec<f64> = Vec::with_capacity(n);
        let mut cx = content_x + offset;
        for w in &ws {
            xs.push(cx);
            cx += w + gap;
        }

        let mut row_bottom = row_top;
        for (i, it) in items.iter().enumerate() {
            let istyle = self.style_of(it, style, na);
            let nca = push_ancestor(na, it);
            let ip = &istyle.padding;
            let ib = &istyle.border_width;
            let cy = self.block_children(
                &it.children,
                &istyle,
                xs[i] + ip.left + ib.left,
                (ws[i] - ip.left - ip.right - ib.left - ib.right).max(1.0),
                row_top + ip.top + ib.top,
                &nca,
            );
            row_bottom = row_bottom.max(cy + ip.bottom + ib.bottom);
        }

        for (i, it) in items.iter().enumerate() {
            let istyle = self.style_of(it, style, na);
            self.paint_item_box(&istyle, xs[i], row_top, ws[i], row_bottom - row_top);
        }
        row_bottom
    }

    /// Vertical flex: stack items top-to-bottom, each stretched to the full
    /// container width (`justify-content` along the block axis is not modelled
    /// since the container has no fixed height here).
    fn flex_column_axis(
        &mut self,
        items: &[&Element],
        style: &Style,
        content_x: f64,
        content_w: f64,
        row_top: f64,
        na: &[&Element],
    ) -> f64 {
        let mut y = row_top;
        for it in items {
            let istyle = self.style_of(it, style, na);
            let nca = push_ancestor(na, it);
            let im = &istyle.margin;
            let ip = &istyle.padding;
            let ib = &istyle.border_width;
            y += im.top;
            let item_top = y;
            let inner_w =
                (content_w - im.left - im.right - ib.left - ib.right - ip.left - ip.right).max(1.0);
            let cy = self.block_children(
                &it.children,
                &istyle,
                content_x + im.left + ib.left + ip.left,
                inner_w,
                y + ip.top + ib.top,
                &nca,
            );
            let item_bottom = cy + ip.bottom + ib.bottom;
            self.paint_item_box(
                &istyle,
                content_x + im.left,
                item_top,
                (content_w - im.left - im.right).max(0.1),
                item_bottom - item_top,
            );
            y = item_bottom + im.bottom;
        }
        y
    }

    /// A grid with a fixed column count (`grid-template-columns`). Children fill
    /// equal-width cells left-to-right, wrapping every `cols` items; each row's
    /// height is its tallest cell. No spanning or named lines.
    fn grid(
        &mut self,
        el: &Element,
        style: &Style,
        x: f64,
        avail_w: f64,
        mut y: f64,
        ancestors: &[&Element],
    ) -> f64 {
        y += style.margin.top;
        let m = &style.margin;
        let p = &style.padding;
        let b = &style.border_width;
        let na = push_ancestor(ancestors, el);

        let items: Vec<&Element> = el
            .children
            .iter()
            .filter_map(|n| match n {
                Node::Element(e) => Some(e),
                _ => None,
            })
            .filter(|e| self.style_of(e, style, &na).display != Display::None)
            .collect();
        if items.is_empty() {
            return y + style.margin.bottom;
        }

        let cols = style.grid_columns.max(1);
        let content_x = x + m.left + b.left + p.left;
        let content_w = (avail_w - m.left - m.right - b.left - b.right - p.left - p.right).max(1.0);
        let col_w = content_w / cols as f64;
        let mut y_cursor = y + b.top + p.top;

        for row in items.chunks(cols) {
            let row_top = y_cursor;
            let mut row_bottom = row_top;
            for (c, it) in row.iter().enumerate() {
                let istyle = self.style_of(it, style, &na);
                let nca = push_ancestor(&na, it);
                let ip = &istyle.padding;
                let ib = &istyle.border_width;
                let cx = content_x + c as f64 * col_w;
                let cy = self.block_children(
                    &it.children,
                    &istyle,
                    cx + ip.left + ib.left,
                    (col_w - ip.left - ip.right - ib.left - ib.right).max(1.0),
                    row_top + ip.top + ib.top,
                    &nca,
                );
                row_bottom = row_bottom.max(cy + ip.bottom + ib.bottom);
            }
            for (c, it) in row.iter().enumerate() {
                let istyle = self.style_of(it, style, &na);
                self.paint_item_box(
                    &istyle,
                    content_x + c as f64 * col_w,
                    row_top,
                    col_w,
                    row_bottom - row_top,
                );
            }
            y_cursor = row_bottom;
        }
        y_cursor + p.bottom + b.bottom + style.margin.bottom
    }

    /// Paint a flex/grid item's background + border as a single rect spanning
    /// its cell (z=0, behind the item's own content).
    fn paint_item_box(&mut self, istyle: &Style, x: f64, y: f64, w: f64, h: f64) {
        let has_border = istyle.border_width.top > 0.0;
        if istyle.background.is_some() || has_border {
            self.out.push(Abs {
                z: 0,
                frag: Fragment::Rect {
                    x,
                    y,
                    w,
                    h: h.max(0.1),
                    fill: istyle.background,
                    stroke: if has_border {
                        Some(istyle.border_color)
                    } else {
                        None
                    },
                    stroke_w: istyle.border_width.top.max(0.0),
                    opacity: istyle.opacity,
                },
            });
        }
    }

    fn style_of(&self, el: &Element, parent: &Style, ancestors: &[&Element]) -> Style {
        self.sheet.computed(el, parent, ancestors)
    }
}

fn default_line_height(style: &Style) -> f64 {
    style.font_size * style.line_height.max(1.0)
}

/// Leading offset and inter-item gap realising a `justify-content` value, given
/// the `free` main-axis space left over by `n` explicitly-sized items.
fn justify_offsets(j: Justify, free: f64, n: usize) -> (f64, f64) {
    if free <= 0.0 || n == 0 {
        return (0.0, 0.0);
    }
    match j {
        Justify::Start => (0.0, 0.0),
        Justify::Center => (free / 2.0, 0.0),
        Justify::End => (free, 0.0),
        Justify::SpaceBetween => (0.0, if n > 1 { free / (n - 1) as f64 } else { 0.0 }),
        Justify::SpaceAround => (free / (2.0 * n as f64), free / n as f64),
    }
}

/// Collapse runs of ASCII whitespace to single spaces (normal `white-space`).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

fn push_ancestor<'a>(ancestors: &[&'a Element], el: &'a Element) -> Vec<&'a Element> {
    let mut v = ancestors.to_vec();
    v.push(el);
    v
}

fn list_marker_ordered(ancestors: &[&Element]) -> bool {
    ancestors.last().map(|e| e.tag == "ol").unwrap_or(false)
}

/// The marker string for a list item, honouring `list-style-type`. An unset
/// type (`Disc`, the inherited default) inside an `<ol>` becomes `decimal`.
/// `None` ⇒ no marker (`list-style-type: none`).
fn list_marker(style: &Style, ordered_ancestor: bool, index: usize) -> Option<String> {
    use super::css::ListStyle as L;
    let kind = match style.list_style {
        L::Disc if ordered_ancestor => L::Decimal,
        other => other,
    };
    match kind {
        L::None => None,
        L::Disc => Some("•".to_string()),
        L::Circle => Some("◦".to_string()),
        L::Square => Some("▪".to_string()),
        L::Decimal => Some(format!("{index}.")),
        L::LowerAlpha => Some(format!("{}.", alpha_marker(index, false))),
        L::UpperAlpha => Some(format!("{}.", alpha_marker(index, true))),
        L::LowerRoman => Some(format!("{}.", roman_marker(index, false))),
        L::UpperRoman => Some(format!("{}.", roman_marker(index, true))),
    }
}

/// `1 → a`, `26 → z`, `27 → aa`, … (bijective base-26).
fn alpha_marker(mut n: usize, upper: bool) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let base = if upper { b'A' } else { b'a' };
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(base + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `1 → i`, `4 → iv`, `9 → ix`, … (Roman numerals; falls back to the number
/// past 3999).
fn roman_marker(n: usize, upper: bool) -> String {
    if n == 0 || n > 3999 {
        return n.to_string();
    }
    const VALUES: [(usize, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut n = n;
    let mut out = String::new();
    for (val, sym) in VALUES {
        while n >= val {
            out.push_str(sym);
            n -= val;
        }
    }
    if upper {
        out.to_uppercase()
    } else {
        out
    }
}

fn collect_rows(table: &Element) -> Vec<&Element> {
    let mut rows = Vec::new();
    fn walk<'a>(el: &'a Element, rows: &mut Vec<&'a Element>) {
        for c in &el.children {
            if let Node::Element(e) = c {
                if e.tag == "tr" {
                    rows.push(e);
                } else if matches!(e.tag.as_str(), "thead" | "tbody" | "tfoot") {
                    walk(e, rows);
                }
            }
        }
    }
    walk(table, &mut rows);
    rows
}

fn collect_cells(row: &Element) -> Vec<&Element> {
    row.children
        .iter()
        .filter_map(|c| match c {
            Node::Element(e) if e.tag == "td" || e.tag == "th" => Some(e),
            _ => None,
        })
        .collect()
}

/// `<col>` elements declared under the table's `<colgroup>` children (or a
/// `<colgroup>` that itself acts as a column via its `span`, when it has no
/// `<col>` children — per HTML semantics). Returns them in document order.
fn collect_cols(table: &Element) -> Vec<&Element> {
    let mut cols = Vec::new();
    for c in &table.children {
        if let Node::Element(group) = c {
            if group.tag != "colgroup" {
                continue;
            }
            let children: Vec<&Element> = group
                .children
                .iter()
                .filter_map(|n| match n {
                    Node::Element(e) if e.tag == "col" => Some(e),
                    _ => None,
                })
                .collect();
            if children.is_empty() {
                // A childless <colgroup> spans `span` columns itself.
                cols.push(group);
            } else {
                cols.extend(children);
            }
        }
    }
    cols
}

/// Number of physical columns a cell occupies: `colspan` (cells) or `span`
/// (`<col>`/`<colgroup>`), defaulting to 1. Zero/garbage clamps to 1.
fn cell_colspan(el: &Element) -> usize {
    el.attr("colspan")
        .or_else(|| el.attr("span"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1)
}

/// Column count of a table: the maximum, over its rows, of the sum of cell
/// `colspan`s in that row.
fn table_column_count(rows: &[&Element]) -> usize {
    rows.iter()
        .map(|r| collect_cells(r).iter().map(|c| cell_colspan(c)).sum())
        .max()
        .unwrap_or(0)
}

/// Declared width of a `<col>`: `style="width:.."` first, then a `width=".."`
/// attribute. Percentages resolve against `avail_w`; bare numbers and `px` are
/// pixels (1px = 0.75pt), `pt` is points — matching the CSS length convention.
fn col_declared_width(col: &Element, avail_w: f64) -> Option<f64> {
    if let Some(style) = col.attr("style") {
        // Scan the inline declarations for a `width:` (ignore `min/max-width`).
        for decl in style.split(';') {
            let mut kv = decl.splitn(2, ':');
            let key = kv.next().unwrap_or("").trim();
            if key.eq_ignore_ascii_case("width") {
                if let Some(val) = kv.next() {
                    if let Some(w) = parse_table_width(val.trim(), avail_w) {
                        return Some(w);
                    }
                }
            }
        }
    }
    col.attr("width")
        .and_then(|v| parse_table_width(v.trim(), avail_w))
}

/// Parse a column width to absolute points. `%` → fraction of `avail_w`; `pt`
/// stays; `px`/bare number → pixels (×0.75). Negatives and unparseable → None.
fn parse_table_width(v: &str, avail_w: f64) -> Option<f64> {
    let v = v.trim();
    if let Some(n) = v.strip_suffix('%') {
        return n
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|p| *p >= 0.0)
            .map(|p| avail_w * p / 100.0);
    }
    if let Some(n) = v.strip_suffix("pt") {
        return n.trim().parse::<f64>().ok().filter(|p| *p >= 0.0);
    }
    if let Some(n) = v.strip_suffix("px") {
        return n
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|p| *p >= 0.0)
            .map(|p| p * 0.75);
    }
    v.parse::<f64>()
        .ok()
        .filter(|p| *p >= 0.0)
        .map(|p| p * 0.75)
}

/// Slice the absolute-positioned fragments into pages, splitting rects that
/// straddle a page boundary so backgrounds/borders stay correct.
fn paginate(mut frags: Vec<Abs>, page_h: f64, top: f64, bottom: f64) -> Vec<Vec<Fragment>> {
    // Backgrounds (z=0) before content (z=1), preserving insertion order.
    frags.sort_by_key(|a| a.z);
    let content_h = (page_h - top - bottom).max(1.0);
    let mut pages: Vec<Vec<Fragment>> = Vec::new();

    let ensure = |pages: &mut Vec<Vec<Fragment>>, idx: usize| {
        while pages.len() <= idx {
            pages.push(Vec::new());
        }
    };
    let page_of = |y_abs: f64| ((y_abs - top).max(0.0) / content_h) as usize;
    let local_y = |y_abs: f64, page: usize| top + (y_abs - top) - page as f64 * content_h;

    for a in frags {
        match a.frag {
            Fragment::Text { x, y, style, text } => {
                let p = page_of(y);
                ensure(&mut pages, p);
                pages[p].push(Fragment::Text {
                    x,
                    y: local_y(y, p),
                    style,
                    text,
                });
            }
            Fragment::Image { x, y, w, h, src } => {
                let p = page_of(y);
                ensure(&mut pages, p);
                pages[p].push(Fragment::Image {
                    x,
                    y: local_y(y, p),
                    w,
                    h,
                    src,
                });
            }
            Fragment::Svg { x, y, w, h, image } => {
                let p = page_of(y);
                ensure(&mut pages, p);
                pages[p].push(Fragment::Svg {
                    x,
                    y: local_y(y, p),
                    w,
                    h,
                    image,
                });
            }
            Fragment::Rect {
                x,
                y,
                w,
                h,
                fill,
                stroke,
                stroke_w,
                opacity,
            } => {
                // Split the rect across the page bands it covers.
                let mut top = y;
                let bottom = y + h;
                while top < bottom {
                    let p = page_of(top);
                    let band_bottom = top + (p as f64 + 1.0) * content_h;
                    let seg_bottom = bottom.min(band_bottom);
                    ensure(&mut pages, p);
                    pages[p].push(Fragment::Rect {
                        x,
                        y: local_y(top, p),
                        w,
                        h: (seg_bottom - top).max(0.1),
                        fill,
                        stroke,
                        stroke_w,
                        opacity,
                    });
                    top = seg_bottom + 0.001;
                }
            }
        }
    }
    if pages.is_empty() {
        pages.push(Vec::new());
    }
    pages
}

/// A rough fallback metric (used for tests and when no embedded font matches):
/// per-glyph advance estimated from the font class. The paint layer overrides
/// this with real TrueType advance widths.
#[derive(Debug)]
pub struct AverageMeasure;

impl Measure for AverageMeasure {
    fn width(&self, text: &str, style: &Style) -> f64 {
        let per_em = if style.generic_mono { 0.6 } else { 0.5 };
        let bold_factor = if style.bold { 1.03 } else { 1.0 };
        text.chars().count() as f64 * style.font_size * per_em * bold_factor
    }
}

#[cfg(test)]
mod tests {
    use super::super::css::{collect_style_css, Stylesheet};
    use super::super::dom::parse;
    use super::*;

    fn run(html: &str) -> Layout {
        let nodes = parse(html);
        let sheet = Stylesheet::new(&collect_style_css(&nodes));
        layout_document(&nodes, &sheet, &AverageMeasure, 612.0, 792.0, 36.0)
    }

    #[test]
    fn wraps_long_text_into_multiple_lines() {
        let html = format!("<p>{}</p>", "word ".repeat(200));
        let layout = run(&html);
        let texts = layout
            .pages
            .iter()
            .flatten()
            .filter(|f| matches!(f, Fragment::Text { .. }))
            .count();
        assert!(texts > 50, "long paragraph wraps into many runs ({texts})");
    }

    #[test]
    fn paginates_tall_content() {
        let html = format!("<div>{}</div>", "<p>line</p>".repeat(120));
        let layout = run(&html);
        assert!(
            layout.pages.len() > 1,
            "tall content spans pages ({})",
            layout.pages.len()
        );
    }

    #[test]
    fn emits_background_rect_behind_text() {
        let layout = run(r#"<div style="background:#eee;padding:10pt">hello</div>"#);
        let page = &layout.pages[0];
        let rect_idx = page.iter().position(|f| matches!(f, Fragment::Rect { .. }));
        let text_idx = page.iter().position(|f| matches!(f, Fragment::Text { .. }));
        assert!(rect_idx.is_some() && text_idx.is_some());
        assert!(rect_idx < text_idx, "background paints before text");
    }

    #[test]
    fn css_page_break_before_starts_new_page() {
        let layout = run("<p>first</p><p style=\"page-break-before: always\">second</p>");
        assert!(
            layout.pages.len() >= 2,
            "page-break-before forces a new page ({})",
            layout.pages.len()
        );
        let on_p2 = layout.pages[1]
            .iter()
            .any(|f| matches!(f, Fragment::Text { text, .. } if text.contains("second")));
        assert!(on_p2, "second paragraph is on page 2");
    }

    #[test]
    fn pagebreak_tag_starts_new_page() {
        let layout = run("<p>a</p><pagebreak></pagebreak><p>b</p>");
        assert!(
            layout.pages.len() >= 2,
            "<pagebreak> forces a new page ({})",
            layout.pages.len()
        );
        let b_on_p2 = layout.pages[1]
            .iter()
            .any(|f| matches!(f, Fragment::Text { text, .. } if text == "b"));
        assert!(b_on_p2, "content after <pagebreak> is on page 2");
    }

    #[test]
    fn table_lays_cells_side_by_side() {
        let layout = run("<table><tr><td>A</td><td>B</td></tr></table>");
        let texts: Vec<_> = layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { x, text, .. } => Some((*x, text.clone())),
                _ => None,
            })
            .collect();
        let a = texts.iter().find(|(_, t)| t == "A").unwrap().0;
        let b = texts.iter().find(|(_, t)| t == "B").unwrap().0;
        assert!(b > a, "second cell is to the right of the first");
    }

    // x of a cell's text fragment.
    fn cell_x(layout: &Layout, label: &str) -> f64 {
        layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Text { x, text, .. } if text == label => Some(*x),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no text fragment {label:?}"))
    }

    // Page 612pt, margins 36pt ⇒ avail_w = 540. Default `td` padding 2pt +
    // border 1pt ⇒ cell text sits 3pt inside its column, so a cell starting at
    // column x renders its text at 36 + x + 3.
    const CELL_INSET: f64 = 36.0 + 3.0;

    #[test]
    fn table_honours_colgroup_widths() {
        // Declared 400/100pt (sum 500) is scaled to fill avail_w=540 (fixed
        // layout): scale 1.08 ⇒ col[0] = 432, so cell B sits at 39 + 432 = 471,
        // far past the equal-split midpoint (39 + 270 = 309).
        let layout = run(
            "<table><colgroup><col style=\"width:400pt\"><col style=\"width:100pt\"></colgroup>\
             <tr><td>A</td><td>B</td></tr></table>",
        );
        let a = cell_x(&layout, "A");
        let b = cell_x(&layout, "B");
        assert!((a - CELL_INSET).abs() < 1.0, "first cell at left ({a})");
        assert!(
            (b - (CELL_INSET + 432.0)).abs() < 1.0,
            "cell B starts at scaled col[0] width (~471), not avail_w/2 ({b})"
        );
    }

    #[test]
    fn table_honours_percent_col_widths() {
        // 75% / 25% of 540 ⇒ col[0] = 405; cell B at 39 + 405 = 444.
        let layout = run(
            "<table><colgroup><col style=\"width:75%\"><col style=\"width:25%\"></colgroup>\
             <tr><td>A</td><td>B</td></tr></table>",
        );
        let b = cell_x(&layout, "B");
        assert!(
            (b - (CELL_INSET + 405.0)).abs() < 1.0,
            "cell B near 39 + 75%×540 = 444 ({b})"
        );
    }

    #[test]
    fn table_without_widths_keeps_equal_columns() {
        // No declared widths ⇒ equal columns (270 each): cell B at 39 + 270.
        let layout = run("<table><tr><td>A</td><td>B</td></tr></table>");
        let b = cell_x(&layout, "B");
        assert!(
            (b - (CELL_INSET + 270.0)).abs() < 1.0,
            "equal columns put B at ~309 ({b})"
        );
    }

    #[test]
    fn table_colspan_sums_column_widths() {
        // Equal 3-col grid (180 each). A colspan=2 cell covers cols 0–1 (360),
        // so "Tail" starts at column 2 ⇒ 39 + 360 = 399. Row 2 fixes the grid.
        let layout = run("<table>\
             <tr><td colspan=\"2\">Wide</td><td>Tail</td></tr>\
             <tr><td>a</td><td>b</td><td>c</td></tr></table>");
        let wide = cell_x(&layout, "Wide");
        let tail = cell_x(&layout, "Tail");
        let c = cell_x(&layout, "c");
        assert!(
            (wide - CELL_INSET).abs() < 1.0,
            "spanning cell at left ({wide})"
        );
        assert!(
            (tail - (CELL_INSET + 360.0)).abs() < 1.0,
            "Tail after 2 columns (~399), proving colspan summed ({tail})"
        );
        // Third column of row 2 aligns under "Tail" (same start column index 2).
        assert!(
            (c - tail).abs() < 1.0,
            "col 2 aligns across rows ({c} vs {tail})"
        );
    }

    #[test]
    fn flex_lays_children_in_a_row() {
        let layout = run(r#"<div style="display:flex"><div>Left</div><div>Right</div></div>"#);
        let texts: Vec<_> = layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { x, text, .. } => Some((*x, text.clone())),
                _ => None,
            })
            .collect();
        let l = texts.iter().find(|(_, t)| t == "Left").unwrap().0;
        let r = texts.iter().find(|(_, t)| t == "Right").unwrap().0;
        assert!(
            r > l,
            "flex item 'Right' is to the right of 'Left' (l={l}, r={r})"
        );
    }

    fn text_xy(layout: &Layout) -> Vec<(f64, f64, String)> {
        layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { x, y, text, .. } => Some((*x, *y, text.clone())),
                _ => None,
            })
            .collect()
    }

    fn text_runs(layout: &Layout) -> Vec<String> {
        layout
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn list_style_type_markers() {
        let roman = text_runs(&run(
            r#"<ol style="list-style-type: lower-roman"><li>a</li><li>b</li><li>c</li></ol>"#,
        ));
        for m in ["i.", "ii.", "iii."] {
            assert!(roman.iter().any(|s| s == m), "roman marker {m}: {roman:?}");
        }
        let alpha = text_runs(&run(
            r#"<ul style="list-style-type: upper-alpha"><li>x</li><li>y</li></ul>"#,
        ));
        assert!(alpha.iter().any(|s| s == "A.") && alpha.iter().any(|s| s == "B."));
        let none = text_runs(&run(r#"<ul style="list-style-type: none"><li>z</li></ul>"#));
        assert!(!none.iter().any(|s| s == "•"), "no marker: {none:?}");
        // Bare <ol> defaults to decimal, bare <ul> to a disc bullet.
        let dec = text_runs(&run("<ol><li>a</li><li>b</li></ol>"));
        assert!(dec.iter().any(|s| s == "1.") && dec.iter().any(|s| s == "2."));
        assert!(text_runs(&run("<ul><li>a</li></ul>"))
            .iter()
            .any(|s| s == "•"));
    }

    #[test]
    fn width_clamps_min_and_max() {
        // width 500 but max-width 100 → the box (background rect) is clamped.
        let layout = run(r#"<div style="width:500pt;max-width:100pt;background:#eee">x</div>"#);
        let w = layout
            .pages
            .iter()
            .flatten()
            .find_map(|f| match f {
                Fragment::Rect { w, .. } => Some(*w),
                _ => None,
            })
            .expect("a background rect");
        assert!(w <= 101.0, "max-width clamps box width: {w}");
    }

    #[test]
    fn text_decoration_flags_on_runs() {
        let layout = run(r#"<p style="text-decoration: line-through overline">struck</p>"#);
        let st = layout.pages.iter().flatten().find_map(|f| match f {
            Fragment::Text { style, text, .. } if text.contains("struck") => Some(style.clone()),
            _ => None,
        });
        let st = st.expect("the text run");
        assert!(st.strike && st.overline, "line-through + overline flagged");
    }

    #[test]
    fn inline_svg_becomes_a_vector_fragment() {
        let layout = run(
            r#"<p>logo <svg width="20" height="20" viewBox="0 0 10 10"><rect width="10" height="10"/></svg> here</p>"#,
        );
        let svg = layout.pages.iter().flatten().find_map(|f| match f {
            Fragment::Svg { w, h, .. } => Some((*w, *h)),
            _ => None,
        });
        assert_eq!(
            svg,
            Some((20.0, 20.0)),
            "inline <svg> → a 20×20 vector fragment"
        );
        // Surrounding text still flows as text runs.
        assert!(text_runs(&layout).iter().any(|t| t == "logo"));
    }

    #[test]
    fn text_transform_cases_rendered_text() {
        let texts = |layout: &Layout| -> String {
            layout
                .pages
                .iter()
                .flatten()
                .filter_map(|f| match f {
                    Fragment::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(texts(&run(
            r#"<p style="text-transform: uppercase">hello world</p>"#
        ))
        .contains("HELLO"));
        assert!(texts(&run(r#"<p style="text-transform: lowercase">HELLO</p>"#)).contains("hello"));
        // `capitalize` upper-cases each word's first letter; it is inherited.
        let cap = texts(&run(
            r#"<div style="text-transform: capitalize"><span>the quick</span></div>"#,
        ));
        assert!(cap.contains("The") && cap.contains("Quick"), "got: {cap}");
    }

    #[test]
    fn text_indent_shifts_first_line_only() {
        // First line is pushed right by 40pt; wrapped lines start at the margin.
        let html = format!(r#"<p style="text-indent:40pt">{}</p>"#, "word ".repeat(80));
        let xs: Vec<f64> = run(&html)
            .pages
            .iter()
            .flatten()
            .filter_map(|f| match f {
                Fragment::Text { x, .. } => Some(*x),
                _ => None,
            })
            .collect();
        assert!(xs.len() > 2, "paragraph wrapped into several lines");
        let first_x = xs[0];
        let min_x = xs.iter().copied().fold(f64::INFINITY, f64::min);
        assert!(
            first_x > min_x + 30.0,
            "first line indented past later lines (first={first_x}, min={min_x})"
        );
    }

    #[test]
    fn flex_column_stacks_items_vertically() {
        let layout = run(
            r#"<div style="display:flex;flex-direction:column"><div>Top</div><div>Bot</div></div>"#,
        );
        let t = text_xy(&layout);
        let top = t.iter().find(|(_, _, s)| s == "Top").unwrap();
        let bot = t.iter().find(|(_, _, s)| s == "Bot").unwrap();
        assert!(
            bot.1 > top.1 && (bot.0 - top.0).abs() < 1.0,
            "column flex stacks 'Bot' below 'Top' at the same x (top={top:?}, bot={bot:?})"
        );
    }

    #[test]
    fn flex_grow_widens_the_growing_item() {
        // Item A grows (weight 4), item B does not (weight 1): A's column is wider,
        // so B starts much further right than the equal-split midpoint.
        let grow =
            run(r#"<div style="display:flex"><div style="flex:3">A</div><div>B</div></div>"#);
        let equal = run(r#"<div style="display:flex"><div>A</div><div>B</div></div>"#);
        let bx = |l: &Layout| text_xy(l).into_iter().find(|(_, _, s)| s == "B").unwrap().0;
        assert!(
            bx(&grow) > bx(&equal) + 50.0,
            "flex-grow pushes 'B' right vs equal split (grow={}, equal={})",
            bx(&grow),
            bx(&equal)
        );
    }

    #[test]
    fn grid_wraps_items_into_rows() {
        // 2 columns, 4 cells → 2 rows. Cell 3 sits below cell 1 at the same x.
        let layout = run(
            r#"<div style="display:grid;grid-template-columns:1fr 1fr"><div>C1</div><div>C2</div><div>C3</div><div>C4</div></div>"#,
        );
        let t = text_xy(&layout);
        let c1 = t.iter().find(|(_, _, s)| s == "C1").unwrap();
        let c2 = t.iter().find(|(_, _, s)| s == "C2").unwrap();
        let c3 = t.iter().find(|(_, _, s)| s == "C3").unwrap();
        assert!(c2.0 > c1.0, "C2 is right of C1 (same row)");
        assert!(
            c3.1 > c1.1 && (c3.0 - c1.0).abs() < 1.0,
            "C3 wraps below C1 in the next row (c1={c1:?}, c3={c3:?})"
        );
    }
}
