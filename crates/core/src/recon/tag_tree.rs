//! Stage 0 — **tagged-PDF structure**. When the catalog carries a
//! `/StructTreeRoot` (ISO 32000-1 §14.7), the author already declared the
//! logical structure; we trust it over the geometric heuristics. We walk the
//! structure tree, map each standard structure type to a model construct
//! (`/P`→Paragraph, `/H1..6`→Heading, `/L /LI`→List, `/Table /TR /TH /TD`→Table,
//! `/Figure`→Image with its child text as alt text) and bind the marked-content
//! each leaf owns (`/K` → MCID) back to the page's decoded glyph runs. A
//! non-standard tag (`/ChapterTitle`, `/TOCItem`, …) is first resolved through
//! the tree's `/RoleMap` (ISO 32000-1 §14.7.4) to its standard type.
//!
//! Binding is done by a self-contained marked-content scan of each page's
//! content stream: `BDC … EMC` ranges tagged with an `/MCID` accumulate their
//! `Tj`/`TJ` text, keyed by `(page, mcid)`; a range whose `BDC` property dict
//! carries `/ActualText` (§14.9.4) uses that text verbatim instead of the
//! decoded glyphs, and an image XObject `Do`'d inside the range is recorded so a
//! `/Figure` leaf can recover its raster. A structure element's `/K` MCID
//! references then pull that text/image. When the document has **no** struct
//! tree — or the tree yields no bound content — this returns `None` and the
//! caller falls back to the heuristic pipeline.

use std::collections::BTreeMap;

use super::{char_style, IdGen};
use crate::content::{parse_content, FontDecoders};
use crate::convert::style::TextStyle;
use crate::font::cmap::TextDecoder;
use crate::model::{
    geom::{Rect, Rotation},
    Block, BlockKind, Cell, Heading, ImageRef, Inline, InlineRun, List, ListItem, ListMarker,
    Paragraph, ParagraphStyle, Row, Table,
};
use crate::object::{Dictionary, Object};

/// The model construct a standard structure type maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructRole {
    /// Paragraph (`/P`, `/Note`, `/Quote`, …).
    Paragraph,
    /// Heading at the given level 1..=6 (`/H1`..`/H6`, or a generic `/H` → 1).
    Heading(u8),
    /// List container (`/L`).
    List,
    /// List item (`/LI`, `/Lbl`, `/LBody`).
    ListItem,
    /// Table container (`/Table`).
    Table,
    /// Table row (`/TR`).
    TableRow,
    /// Table cell (`/TD`, `/TH`).
    TableCell,
    /// Figure / image (`/Figure`).
    Figure,
}

/// Map a standard structure-type tag to a [`StructRole`]. Unknown / grouping
/// tags (`/Document`, `/Part`, `/Sect`, `/Div`, `/Art`, `/Span`, …) return
/// `None` so the walker recurses through them without emitting a block.
pub fn structure_role(tag: &[u8]) -> Option<StructRole> {
    match tag {
        b"P" | b"Note" | b"Quote" | b"BlockQuote" | b"Caption" => Some(StructRole::Paragraph),
        b"H" => Some(StructRole::Heading(1)),
        b"H1" => Some(StructRole::Heading(1)),
        b"H2" => Some(StructRole::Heading(2)),
        b"H3" => Some(StructRole::Heading(3)),
        b"H4" => Some(StructRole::Heading(4)),
        b"H5" => Some(StructRole::Heading(5)),
        b"H6" => Some(StructRole::Heading(6)),
        b"L" => Some(StructRole::List),
        b"LI" | b"Lbl" | b"LBody" => Some(StructRole::ListItem),
        b"Table" => Some(StructRole::Table),
        b"TR" => Some(StructRole::TableRow),
        b"TD" | b"TH" => Some(StructRole::TableCell),
        b"Figure" | b"Formula" => Some(StructRole::Figure),
        _ => None,
    }
}

// ── structure-element attributes (`/A`, ISO 32000-1 §14.8.5.2) ───────────────
//
// A structure element's layout/table/list properties live in its `/A` entry: a
// single attribute dictionary, or an array of them (each optionally followed by
// a revision-number integer we ignore). Every attribute dict carries an `/O`
// (owner) name — `/Table` owns `/ColSpan`/`/RowSpan`, `/List` owns
// `/ListNumbering`, `/Layout` owns `/BBox`. These helpers are pure over a
// resolver closure so they can be unit-tested with hand-built dictionaries.

/// Yield each attribute dictionary in an element's `/A`, in order. Resolves an
/// indirect `/A`, flattens a single dict or an array (skipping the interleaved
/// revision-number integers of the `[dict rev dict rev …]` form).
fn attribute_dicts<'a, R>(elem: &'a Dictionary, resolve: &R) -> Vec<&'a Dictionary>
where
    R: Fn(&'a Object) -> &'a Object,
{
    let Some(a) = elem.get(b"A").map(resolve) else {
        return Vec::new();
    };
    match a {
        Object::Dictionary(d) => vec![d],
        Object::Array(items) => items
            .iter()
            .filter_map(|i| match resolve(i) {
                Object::Dictionary(d) => Some(d),
                _ => None, // revision-number integer (or anything else): skip
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The value of `key` in the first attribute dict owned by `owner` (its `/O`
/// name equals `owner`). A dict with no `/O` is treated as a wildcard match so
/// minimally-tagged producers (attributes without an explicit owner) still
/// resolve.
fn attribute_value<'a, R>(
    elem: &'a Dictionary,
    owner: &[u8],
    key: &[u8],
    resolve: &R,
) -> Option<&'a Object>
where
    R: Fn(&'a Object) -> &'a Object,
{
    for dict in attribute_dicts(elem, resolve) {
        let dict_owner = dict.get(b"O").map(resolve).and_then(Object::as_name);
        if matches!(dict_owner, Some(o) if o != owner) {
            continue; // explicit, non-matching owner
        }
        if let Some(v) = dict.get(key) {
            return Some(resolve(v));
        }
    }
    None
}

/// The `(col_span, row_span)` of a `/TD`/`/TH` cell element from its `/Table`
/// `/ColSpan`/`/RowSpan` attributes (default 1 each; clamped to ≥1).
fn cell_spans<'a, R>(elem: &'a Dictionary, resolve: &R) -> (u16, u16)
where
    R: Fn(&'a Object) -> &'a Object,
{
    let span = |key: &[u8]| -> u16 {
        attribute_value(elem, b"Table", key, resolve)
            .and_then(Object::as_i64)
            .filter(|n| *n >= 1)
            .map(|n| n.min(u16::MAX as i64) as u16)
            .unwrap_or(1)
    };
    (span(b"ColSpan"), span(b"RowSpan"))
}

/// The `(ordered, marker)` of an `/L` list element from its `/List`
/// `/ListNumbering` attribute. `None` when the attribute is absent, leaving the
/// caller's unordered-bullet default. `/None` maps to an unordered list; the
/// glyph kinds (`/Disc`/`/Circle`/`/Square`) map to bullet markers; the numeric
/// kinds map to the corresponding ordered [`ListMarker`].
fn list_numbering<'a, R>(elem: &'a Dictionary, resolve: &R) -> Option<(bool, ListMarker)>
where
    R: Fn(&'a Object) -> &'a Object,
{
    let kind = attribute_value(elem, b"List", b"ListNumbering", resolve)?.as_name()?;
    Some(match kind {
        b"Decimal" => (true, ListMarker::Decimal),
        b"UpperRoman" => (true, ListMarker::UpperRoman),
        b"LowerRoman" => (true, ListMarker::LowerRoman),
        b"UpperAlpha" => (true, ListMarker::UpperAlpha),
        b"LowerAlpha" => (true, ListMarker::LowerAlpha),
        b"Disc" => (false, ListMarker::Bullet('•')),
        b"Circle" => (false, ListMarker::Bullet('◦')),
        b"Square" => (false, ListMarker::Bullet('▪')),
        // `/None` (and any non-standard kind) → unordered, default bullet.
        _ => (false, ListMarker::default()),
    })
}

/// The element's bounding box from its `/Layout` `/BBox` attribute
/// (`[llx lly urx ury]` in default user space) as a [`Rect`] (lower-left +
/// width/height). `None` when absent or malformed.
fn layout_bbox<'a, R>(elem: &'a Dictionary, resolve: &R) -> Option<Rect>
where
    R: Fn(&'a Object) -> &'a Object,
{
    let arr = attribute_value(elem, b"Layout", b"BBox", resolve)?.as_array()?;
    if arr.len() != 4 {
        return None;
    }
    let n = |i: usize| resolve(&arr[i]).as_f64();
    let (llx, lly, urx, ury) = (n(0)?, n(1)?, n(2)?, n(3)?);
    Some(Rect::new(
        llx.min(urx),
        lly.min(ury),
        (urx - llx).abs(),
        (ury - lly).abs(),
    ))
}

/// The 0-based page index a structure element's `/Pg` points at: the position
/// of its referenced page object in the document's page-id list. `None` when
/// `/Pg` is absent, not an indirect reference, or doesn't match a page.
fn pg_page_index(elem: &Dictionary, page_ids: &[crate::object::ObjectId]) -> Option<usize> {
    let Some(Object::Reference(id)) = elem.get(b"Pg") else {
        return None;
    };
    page_ids.iter().position(|p| p == id)
}

/// Per-`(page index, MCID)` accumulated text from a page's marked content.
type MarkedText = BTreeMap<(usize, i64), String>;

/// Per-`(page index, MCID)` the resource name of the **first** image XObject
/// `Do`'d inside that marked-content range — used to recover a `/Figure` leaf's
/// raster (the image bytes are decoded lazily, on demand, at Figure time).
type MarkedImages = BTreeMap<(usize, i64), Vec<u8>>;

/// Walk the document's `/StructTreeRoot` into model blocks. `None` when the
/// document is not tagged, or the tree produced no text-bearing blocks (so the
/// caller uses the heuristic reconstruction instead).
///
/// The blocks are returned in document reading order, **flattened** across all
/// pages. The per-block page each one belongs to (resolved from the element's
/// `/Pg`) is available via [`reconstruct_from_struct_tree_paged`]; this entry
/// point drops it for callers that place the whole list on one page.
pub fn reconstruct_from_struct_tree(doc: &crate::Document, ids: &mut IdGen) -> Option<Vec<Block>> {
    let paged = reconstruct_from_struct_tree_paged(doc, ids)?;
    Some(paged.into_iter().map(|(_page, block)| block).collect())
}

/// Like [`reconstruct_from_struct_tree`] but pairs each top-level block with the
/// **0-based page index** it belongs to, resolved from the structure element's
/// `/Pg` (or the first `/Pg` found among its descendants). `None` when the page
/// is not declared/resolvable — the tag tree often omits `/Pg` on grouping
/// elements, so a `None` here means "page unknown from the structure", not page
/// 0. Callers distribute the blocks onto model pages from this hint.
///
/// The model-reconstruction driver (`Document::reconstruct_model` in
/// `document.rs`) **consumes** this hint: it groups the blocks by page index and
/// places each one on its real model page (a `None` page falls back to the first
/// page). This entry point is the one that path uses.
pub fn reconstruct_from_struct_tree_paged(
    doc: &crate::Document,
    ids: &mut IdGen,
) -> Option<Vec<(Option<usize>, Block)>> {
    let root = struct_tree_root(doc)?;
    // Collect marked-content text + image bindings for every page once.
    let (marked, images) = collect_all_marked_content(doc);
    if marked.is_empty() && images.is_empty() {
        return None;
    }
    // The author may tag with non-standard roles mapped to standard ones by the
    // tree's `/RoleMap` (ISO 32000-1 §14.7.4); resolve `/S` through it.
    let role_map = struct_role_map(doc, root);

    let mut walker = Walker {
        doc,
        marked: &marked,
        images: &images,
        role_map: &role_map,
        ids,
    };
    // The root's `/K` are the top-level structure elements. `visit` tags every
    // emitted block with the page of the *element that produced it* (its `/Pg`,
    // or the nearest enclosing one), so blocks land on their real page even when
    // a single grouping element (`/Document`) spans several pages.
    let mut paged: Vec<(Option<usize>, Block)> = Vec::new();
    for kid in kids_of(doc, root) {
        walker.visit(&kid, &mut paged, None);
    }
    // If nothing fruitful came out (no text- or image-bearing block), the tree
    // was unhelpful — fall back to the heuristic pipeline.
    let fruitful = paged.iter().any(|(_, b)| block_is_fruitful(b));
    fruitful.then_some(paged)
}

/// The `/StructTreeRoot` dictionary, if the catalog references one.
fn struct_tree_root(doc: &crate::Document) -> Option<&Dictionary> {
    let catalog = doc.catalog().ok()?;
    let root = catalog.get(b"StructTreeRoot")?;
    doc.resolve(root).as_dict()
}

/// The structure tree's `/RoleMap` (ISO 32000-1 §14.7.4) as `custom → standard`
/// tag-name pairs. The map lets a producer use a non-standard structure type
/// (`/ChapterTitle`) by declaring how it maps onto a standard one (`/H1`).
/// Returns an owned copy (a flat `Vec` is plenty — role maps are tiny); empty
/// when the tree has no `/RoleMap`.
fn struct_role_map(doc: &crate::Document, root: &Dictionary) -> Vec<(Vec<u8>, Vec<u8>)> {
    let Some(map) = root
        .get(b"RoleMap")
        .map(|o| doc.resolve(o))
        .and_then(Object::as_dict)
    else {
        return Vec::new();
    };
    map.0
        .iter()
        .filter_map(|(custom, mapped)| {
            doc.resolve(mapped)
                .as_name()
                .map(|std| (custom.clone(), std.to_vec()))
        })
        .collect()
}

/// The `/K` children of a structure element as a flat list of resolved objects
/// (a single kid, an array, or absent → empty).
fn kids_of(doc: &crate::Document, elem: &Dictionary) -> Vec<Object> {
    match elem.get(b"K").map(|k| doc.resolve(k)) {
        Some(Object::Array(items)) => items.iter().map(|i| doc.resolve(i).clone()).collect(),
        Some(other) => vec![other.clone()],
        None => Vec::new(),
    }
}

struct Walker<'a> {
    doc: &'a crate::Document,
    marked: &'a MarkedText,
    /// `(page, MCID) → image XObject name` bound to each marked-content range,
    /// used to recover a `/Figure` leaf's raster.
    images: &'a MarkedImages,
    /// `/RoleMap` (`custom → standard` tag-name pairs); empty when none.
    role_map: &'a [(Vec<u8>, Vec<u8>)],
    ids: &'a mut IdGen,
}

impl Walker<'_> {
    /// Visit a `/K` entry. Structure-element dictionaries map to a role; MCID
    /// integers and marked-content reference dicts are leaves bound to text via
    /// `page_hint` (the page index of the nearest enclosing `/Pg`). Each emitted
    /// block is tagged with that resolved page so the caller can place it on the
    /// right model page.
    fn visit(
        &mut self,
        node: &Object,
        out: &mut Vec<(Option<usize>, Block)>,
        page_hint: Option<usize>,
    ) {
        let Some(dict) = node.as_dict() else {
            return; // bare MCID int with no element context — handled by parents
        };
        // The element's page context (`/Pg`) overrides the inherited hint.
        let page = self.page_of(dict).or(page_hint);

        let tag = dict
            .get(b"S")
            .and_then(|o| self.doc.resolve(o).as_name())
            .map(|n| n.to_vec());
        // Resolve `/S` to a standard type through the tree's `/RoleMap` first, so
        // a custom tag (`/ChapterTitle` → `/H1`) takes its mapped role.
        let role = tag
            .as_deref()
            .map(|t| self.resolve_role_tag(t))
            .and_then(|t| structure_role(&t));

        match role {
            Some(StructRole::Heading(level)) => {
                let text = self.collect_text(dict, page);
                if !text.is_empty() {
                    let frame = self.element_bbox(dict);
                    out.push((page, self.heading_block(level, &text, frame)));
                }
            }
            Some(StructRole::Paragraph) => {
                let text = self.collect_text(dict, page);
                if !text.is_empty() {
                    let frame = self.element_bbox(dict);
                    out.push((page, self.paragraph_block(&text, frame)));
                }
            }
            Some(StructRole::List) => {
                if let Some(block) = self.list_block(dict, page) {
                    out.push((page, block));
                }
            }
            Some(StructRole::Table) => {
                if let Some(block) = self.table_block(dict, page) {
                    out.push((page, block));
                }
            }
            Some(StructRole::Figure) => {
                // A `/Figure` binds (via its `/K` MCID) the image XObject `Do`'d
                // in that marked-content range; emit it as an `Image` block whose
                // child text (`/Alt` or the figure's marked text) becomes alt
                // text. When no raster is bound (a vector-only figure) we keep the
                // old behaviour and recurse for caption text.
                let alt = self.figure_alt(dict, page);
                if let Some(block) = self.figure_image_block(dict, page, alt.as_deref()) {
                    out.push((page, block));
                } else {
                    for kid in kids_of(self.doc, dict) {
                        self.visit(&kid, out, page);
                    }
                }
            }
            // Grouping element (Document/Sect/Div/…) or a stray cell/row/item
            // outside its container: recurse.
            _ => {
                for kid in kids_of(self.doc, dict) {
                    self.visit(&kid, out, page);
                }
            }
        }
    }

    /// Resolve a structure element's `/Pg` to a page index (0-based).
    fn page_of(&self, elem: &Dictionary) -> Option<usize> {
        let page_ids = self.doc.page_ids().ok()?;
        pg_page_index(elem, &page_ids)
    }

    /// Map a structure-type tag to its standard equivalent through the tree's
    /// `/RoleMap`, following the chain (`/ChapterTitle → /Subtitle → /H2`) until a
    /// standard type is reached or a fixed point / cycle is hit. A tag that is
    /// already standard, or unmapped, is returned unchanged. Bounded to the map
    /// length so a self-referential `/RoleMap` can't loop forever.
    fn resolve_role_tag(&self, tag: &[u8]) -> Vec<u8> {
        let mut current = tag.to_vec();
        for _ in 0..=self.role_map.len() {
            // Stop as soon as the current tag is a standard, recognised type.
            if structure_role(&current).is_some() {
                return current;
            }
            match self
                .role_map
                .iter()
                .find(|(custom, _)| custom.as_slice() == current.as_slice())
            {
                Some((_, mapped)) if mapped != &current => current = mapped.clone(),
                _ => break, // unmapped or self-referential: stop
            }
        }
        current
    }

    /// Gather all text bound to a leaf element (and its descendants) by walking
    /// its `/K` MCID references against the marked-content map.
    fn collect_text(&self, elem: &Dictionary, page: Option<usize>) -> String {
        let mut s = String::new();
        self.gather_text(elem, page, &mut s);
        s.trim().to_string()
    }

    fn gather_text(&self, elem: &Dictionary, page: Option<usize>, out: &mut String) {
        let page = self.page_of(elem).or(page);
        match elem.get(b"K").map(|k| self.doc.resolve(k)) {
            Some(Object::Integer(mcid)) => self.push_mcid(page, *mcid, out),
            Some(Object::Array(items)) => {
                for item in items {
                    match self.doc.resolve(item) {
                        Object::Integer(mcid) => self.push_mcid(page, *mcid, out),
                        Object::Dictionary(d) => self.gather_text(d, page, out),
                        _ => {}
                    }
                }
            }
            Some(Object::Dictionary(d)) => self.gather_text(d, page, out),
            _ => {}
        }
    }

    fn push_mcid(&self, page: Option<usize>, mcid: i64, out: &mut String) {
        let Some(page) = page else { return };
        if let Some(text) = self.marked.get(&(page, mcid)) {
            let t = text.trim();
            if t.is_empty() {
                return;
            }
            if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                out.push(' ');
            }
            out.push_str(t);
        }
    }

    /// Collect every `(page, MCID)` an element binds (its `/K` ints, recursively
    /// through descendant elements), each paired with the page it sits on. Used to
    /// find a `/Figure`'s bound image XObject.
    fn gather_mcids(&self, elem: &Dictionary, page: Option<usize>, out: &mut Vec<(usize, i64)>) {
        let page = self.page_of(elem).or(page);
        match elem.get(b"K").map(|k| self.doc.resolve(k)) {
            Some(Object::Integer(mcid)) => {
                if let Some(p) = page {
                    out.push((p, *mcid));
                }
            }
            Some(Object::Array(items)) => {
                for item in items {
                    match self.doc.resolve(item) {
                        Object::Integer(mcid) => {
                            if let Some(p) = page {
                                out.push((p, *mcid));
                            }
                        }
                        Object::Dictionary(d) => self.gather_mcids(d, page, out),
                        _ => {}
                    }
                }
            }
            Some(Object::Dictionary(d)) => self.gather_mcids(d, page, out),
            _ => {}
        }
    }

    /// The alt text for a `/Figure`: its `/Alt` (a PDF text string, ISO 32000-1
    /// §14.9.3) when present, else the marked text bound to its descendants (the
    /// caption a producer wraps in the figure). Empty → `None`.
    fn figure_alt(&self, elem: &Dictionary, page: Option<usize>) -> Option<String> {
        if let Some(alt) = elem
            .get(b"Alt")
            .map(|o| self.doc.resolve(o))
            .and_then(|o| match o {
                Object::String(bytes, _) => Some(crate::font::decode_pdf_text(bytes)),
                _ => None,
            })
        {
            let alt = alt.trim();
            if !alt.is_empty() {
                return Some(alt.to_string());
            }
        }
        let text = self.collect_text(elem, page);
        (!text.is_empty()).then_some(text)
    }

    /// Build an [`Image`](BlockKind::Image) block for a `/Figure` from the raster
    /// XObject bound to one of its MCIDs. The image bytes are re-encoded to PNG and
    /// content-hashed (FNV-1a) into the same `resource` key the per-page
    /// reconstruction interns the blob under, so the model's `ResourceTable` (built
    /// by `reconstruct_model`'s per-page pass) resolves it. `None` when no raster is
    /// bound (a vector-only / empty figure) — the caller then recurses for caption
    /// text instead.
    fn figure_image_block(
        &mut self,
        elem: &Dictionary,
        page: Option<usize>,
        alt: Option<&str>,
    ) -> Option<Block> {
        let mut mcids = Vec::new();
        self.gather_mcids(elem, page, &mut mcids);
        // The first MCID that names an image XObject we can decode wins.
        for (page, mcid) in mcids {
            let Some(name) = self.images.get(&(page, mcid)) else {
                continue;
            };
            let page_images = self.doc.page_images((page + 1) as u32);
            let Some(img) = page_images.get(name) else {
                continue;
            };
            let png = crate::raster::png::encode_png(img.width, img.height, &img.rgba);
            let resource = fnv1a(&png);
            let frame = self.element_bbox(elem);
            return Some(Block {
                id: self.ids.mint(),
                frame,
                rotation: Rotation::D0,
                kind: BlockKind::Image(ImageRef {
                    resource,
                    alt: alt.map(str::to_string),
                }),
            });
        }
        None
    }

    /// The element's placement box from its `/Layout` `/BBox` attribute, if any.
    fn element_bbox(&self, elem: &Dictionary) -> Option<Rect> {
        layout_bbox(elem, &|o| self.doc.resolve(o))
    }

    fn paragraph_block(&mut self, text: &str, frame: Option<Rect>) -> Block {
        Block {
            id: self.ids.mint(),
            frame,
            rotation: Rotation::D0,
            kind: BlockKind::Paragraph(text_paragraph(text)),
        }
    }

    fn heading_block(&mut self, level: u8, text: &str, frame: Option<Rect>) -> Block {
        Block {
            id: self.ids.mint(),
            frame,
            rotation: Rotation::D0,
            kind: BlockKind::Heading(Heading {
                level: level.clamp(1, 6),
                para: text_paragraph(text),
            }),
        }
    }

    /// Build a [`List`] from an `/L` element: each `/LI` (or its `/LBody`)
    /// becomes a [`ListItem`].
    fn list_block(&mut self, elem: &Dictionary, page: Option<usize>) -> Option<Block> {
        let page = self.page_of(elem).or(page);
        let mut items: Vec<ListItem> = Vec::new();
        for kid in kids_of(self.doc, elem) {
            let Some(d) = kid.as_dict() else { continue };
            let tag = d.get(b"S").and_then(|o| self.doc.resolve(o).as_name());
            if tag == Some(b"LI".as_slice()) {
                let text = self.collect_text(d, page);
                if text.is_empty() {
                    continue;
                }
                items.push(ListItem {
                    blocks: vec![Block {
                        id: self.ids.mint(),
                        frame: None,
                        rotation: Rotation::D0,
                        kind: BlockKind::Paragraph(text_paragraph(&text)),
                    }],
                    level: 0,
                });
            }
        }
        if items.is_empty() {
            return None;
        }
        // `/ListNumbering` (ISO 32000-1 §14.8.5.2, `/List` owner) decides ordered
        // vs unordered and the marker glyph/format; absent ⇒ unordered default.
        let (ordered, marker) = list_numbering(elem, &|o| self.doc.resolve(o))
            .unwrap_or((false, ListMarker::default()));
        let frame = self.element_bbox(elem);
        Some(Block {
            id: self.ids.mint(),
            frame,
            rotation: Rotation::D0,
            kind: BlockKind::List(List {
                ordered,
                marker,
                items,
            
            ..Default::default()
}),
        })
    }

    /// Build a [`Table`] from a `/Table` element: each `/TR` is a row, each
    /// `/TD`/`/TH` a cell. `/THead`/`/TBody`/`/TFoot` row groups (ISO 32000-1
    /// §14.8.4.3.4.2) are descended into; a `/TR` inside a `/THead` group — or a
    /// `/TR` whose cells are all `/TH` header cells — yields a header
    /// [`Row::is_header`].
    fn table_block(&mut self, elem: &Dictionary, page: Option<usize>) -> Option<Block> {
        let page = self.page_of(elem).or(page);
        let mut rows: Vec<Row> = Vec::new();
        let mut max_cols = 0usize;
        for child in kids_of(self.doc, elem) {
            let Some(child_dict) = child.as_dict() else {
                continue;
            };
            match self.doc.resolve_name(child_dict, b"S").as_deref() {
                Some(b"TR") => {
                    self.push_table_row(child_dict, page, false, &mut rows, &mut max_cols);
                }
                // A row group: every `/TR` directly under a `/THead` is a header
                // row; `/TBody`/`/TFoot` rows are body rows.
                Some(group @ (b"THead" | b"TBody" | b"TFoot")) => {
                    let in_thead = group == b"THead";
                    for tr in kids_of(self.doc, child_dict) {
                        let Some(tr_dict) = tr.as_dict() else {
                            continue;
                        };
                        if self.doc.resolve_name(tr_dict, b"S").as_deref() == Some(b"TR") {
                            self.push_table_row(tr_dict, page, in_thead, &mut rows, &mut max_cols);
                        }
                    }
                }
                _ => {}
            }
        }
        if rows.is_empty() {
            return None;
        }
        let frame = self.element_bbox(elem);
        Some(Block {
            id: self.ids.mint(),
            frame,
            rotation: Rotation::D0,
            kind: BlockKind::Table(Table {
                rows,
                col_widths: vec![0.0; max_cols],
                border: crate::model::BorderStyle {
                    width: 1.0,
                    color: [0.0, 0.0, 0.0],
                },
            }),
        })
    }

    /// Lower one `/TR` dictionary into a model [`Row`], pushing it onto `rows` and
    /// updating `max_cols`. `in_thead` forces the header flag (the `/TR` came from
    /// a `/THead` group); otherwise the row is a header iff it has cells and every
    /// cell is a `/TH`.
    fn push_table_row(
        &mut self,
        tr_dict: &Dictionary,
        page: Option<usize>,
        in_thead: bool,
        rows: &mut Vec<Row>,
        max_cols: &mut usize,
    ) {
        let mut cells: Vec<Cell> = Vec::new();
        // Tracks whether every emitted cell is a `/TH` (an all-`/TH` row is a
        // header row even without a `/THead` wrapper). Stays `true` only while at
        // least one cell exists and none is a `/TD`.
        let mut all_th = true;
        for td in kids_of(self.doc, tr_dict) {
            let Some(td_dict) = td.as_dict() else {
                continue;
            };
            let tag = self.doc.resolve_name(td_dict, b"S");
            let is_th = tag.as_deref() == Some(b"TH");
            if tag.as_deref() != Some(b"TD") && !is_th {
                continue;
            }
            if !is_th {
                all_th = false;
            }
            let text = self.collect_text(td_dict, page);
            // `/ColSpan`/`/RowSpan` (`/Table` owner) — without them tagged
            // tables come out as ragged 0-width grids.
            let (col_span, row_span) = cell_spans(td_dict, &|o| self.doc.resolve(o));
            let cell_frame = self.element_bbox(td_dict);
            cells.push(Cell {
                blocks: vec![Block {
                    id: self.ids.mint(),
                    frame: cell_frame,
                    rotation: Rotation::D0,
                    kind: BlockKind::Paragraph(text_paragraph(&text)),
                }],
                col_span,
                row_span,
                shading: None,
                vertical_align: None,
            });
        }
        if cells.is_empty() {
            return;
        }
        // Logical column count honours horizontal spans (Σ col_span), so a row of
        // two cells where one spans 2 columns is a 3-wide grid.
        let row_cols: usize = cells.iter().map(|c| c.col_span.max(1) as usize).sum();
        *max_cols = (*max_cols).max(row_cols);
        rows.push(Row {
            cells,
            height: None,
            is_header: in_thead || all_th,
        });
    }
}

/// A small extension on [`Document`](crate::Document) for tag-tree walking.
trait ResolveName {
    fn resolve_name(&self, dict: &Dictionary, key: &[u8]) -> Option<Vec<u8>>;
}
impl ResolveName for crate::Document {
    fn resolve_name(&self, dict: &Dictionary, key: &[u8]) -> Option<Vec<u8>> {
        dict.get(key)
            .map(|o| self.resolve(o))
            .and_then(Object::as_name)
            .map(|n| n.to_vec())
    }
}

/// Build a single-run paragraph from plain text (default char style).
fn text_paragraph(text: &str) -> Paragraph {
    let runs = if text.trim().is_empty() {
        Vec::new()
    } else {
        vec![Inline::Run(InlineRun {
            text: text.trim().to_string(),
            style: char_style(&TextStyle::default(), 12.0),
            source_index: None,
        })]
    };
    Paragraph {
        style: ParagraphStyle::default(),
        style_ref: None,
        runs,
    }
}

/// Whether a block (recursively) carries any text — used to decide if the
/// struct-tree walk was fruitful.
fn block_has_text(block: &Block) -> bool {
    match &block.kind {
        BlockKind::Paragraph(p) => paragraph_has_text(p),
        BlockKind::Heading(h) => paragraph_has_text(&h.para),
        BlockKind::List(l) => l.items.iter().any(|i| i.blocks.iter().any(block_has_text)),
        BlockKind::Table(t) => t
            .rows
            .iter()
            .any(|r| r.cells.iter().any(|c| c.blocks.iter().any(block_has_text))),
        _ => false,
    }
}

/// Whether a block makes the struct-tree walk worth trusting: any text **or** a
/// recovered `/Figure` image. A figure-only tagged document (an image with no
/// prose) is still reconstructed from its tag tree, not the heuristics.
fn block_is_fruitful(block: &Block) -> bool {
    matches!(block.kind, BlockKind::Image(_)) || block_has_text(block)
}

fn paragraph_has_text(p: &Paragraph) -> bool {
    p.runs.iter().any(|r| match r {
        Inline::Run(run) => !run.text.trim().is_empty(),
        _ => false,
    })
}

// ── marked-content text collection ──────────────────────────────────────────

/// Collect `(page_index, MCID) → text` for every page by scanning each page's
/// content stream for `BDC … EMC` ranges that carry an `/MCID`.
fn collect_all_marked_content(doc: &crate::Document) -> (MarkedText, MarkedImages) {
    let mut text = MarkedText::new();
    let mut images = MarkedImages::new();
    for page_no in 1..=doc.page_count() as u32 {
        let Ok(content) = doc.page_content(page_no) else {
            continue;
        };
        let decoders = doc.page_font_decoders(page_no);
        collect_page_marked_content(
            (page_no - 1) as usize,
            &content,
            &decoders,
            &mut text,
            &mut images,
        );
    }
    (text, images)
}

/// Scan one page's content stream, accumulating per active `/MCID`: the shown
/// text, and the resource name of any image XObject drawn (`Do`) inside the
/// range. The `BDC`/`BMC` … `EMC` nesting is tracked on a stack; `Tj`/`TJ`/`'`/`"`
/// text and `Do` images are bound to the innermost MCID range in effect (the
/// standard "current marked-content sequence"). `Tf` selects the active font
/// decoder. A range whose `BDC` property dict carries `/ActualText` (ISO 32000-1
/// §14.9.4) takes that text **verbatim** and its glyph shows are suppressed.
fn collect_page_marked_content(
    page: usize,
    content: &[u8],
    decoders: &FontDecoders,
    text_out: &mut MarkedText,
    image_out: &mut MarkedImages,
) {
    let Ok(ops) = parse_content(content) else {
        return;
    };
    let mut mc_stack: Vec<Option<i64>> = Vec::new();
    // MCIDs whose text was fixed by `/ActualText`; their glyph shows are skipped.
    let mut frozen: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    let mut font: Option<&TextDecoder> = None;
    let default = TextDecoder::winansi();

    for op in &ops {
        match op.operator.as_slice() {
            b"BDC" => {
                let mcid = mcid_of(op);
                // `/ActualText` on the same BDC as the MCID stands in for the
                // decoded glyphs of the whole sequence.
                if let Some(mcid) = mcid {
                    if let Some(actual) = actual_text_of(op) {
                        text_out.insert((page, mcid), actual);
                        frozen.insert(mcid);
                    }
                }
                mc_stack.push(mcid);
            }
            b"BMC" => mc_stack.push(None),
            b"EMC" => {
                mc_stack.pop();
            }
            b"Tf" => {
                font = op
                    .operands
                    .first()
                    .and_then(Object::as_name)
                    .and_then(|name| decoders.get(name));
            }
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                let Some(mcid) = mc_stack.iter().rev().find_map(|m| *m) else {
                    continue;
                };
                if frozen.contains(&mcid) {
                    continue; // text fixed by /ActualText — ignore the glyphs
                }
                let decoder = font.unwrap_or(&default);
                let text = decode_show(&op.operands, decoder);
                if text.is_empty() {
                    continue;
                }
                text_out.entry((page, mcid)).or_default().push_str(&text);
            }
            b"Do" => {
                let Some(mcid) = mc_stack.iter().rev().find_map(|m| *m) else {
                    continue;
                };
                if let Some(Object::Name(name)) = op.operands.first() {
                    // Keep the first image bound to this range (a `/Figure` draws
                    // one raster); later `Do`s in the same range don't overwrite.
                    image_out
                        .entry((page, mcid))
                        .or_insert_with(|| name.clone());
                }
            }
            _ => {}
        }
    }
}

/// The `/MCID` integer of a `BDC` operation (`/Tag <<… /MCID n …>> BDC` or
/// `/Tag /Props BDC` with a named property dict we can't resolve → `None`).
fn mcid_of(op: &crate::content::Operation) -> Option<i64> {
    // Operands: `/Tag` then either a property dict or a name. We want the MCID
    // from an inline dict.
    op.operands.iter().find_map(|o| match o {
        Object::Dictionary(d) => d.get(b"MCID").and_then(Object::as_i64),
        _ => None,
    })
}

/// The `/ActualText` of a `BDC` operation's inline property dict (ISO 32000-1
/// §14.9.4), decoded as a PDF text string (UTF-16BE BOM or PDFDocEncoding).
/// `None` when the operand is a named property dict (not inline) or carries no
/// `/ActualText`.
fn actual_text_of(op: &crate::content::Operation) -> Option<String> {
    op.operands.iter().find_map(|o| match o {
        Object::Dictionary(d) => d.get(b"ActualText").and_then(|v| match v {
            Object::String(bytes, _) => Some(crate::font::decode_pdf_text(bytes)),
            _ => None,
        }),
        _ => None,
    })
}

/// FNV-1a 64-bit hash — the codebase's zero-dependency content key for image
/// blobs (matches `Document::fnv1a`, so a `/Figure`'s `ImageRef.resource` lines
/// up with the blob the per-page reconstruction interns into the model's
/// `ResourceTable`).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Decode the text operands of a show operator with the active font decoder.
fn decode_show(operands: &[Object], decoder: &TextDecoder) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_standard_structure_types() {
        assert_eq!(structure_role(b"P"), Some(StructRole::Paragraph));
        assert_eq!(structure_role(b"H1"), Some(StructRole::Heading(1)));
        assert_eq!(structure_role(b"H3"), Some(StructRole::Heading(3)));
        assert_eq!(structure_role(b"H"), Some(StructRole::Heading(1)));
        assert_eq!(structure_role(b"L"), Some(StructRole::List));
        assert_eq!(structure_role(b"LI"), Some(StructRole::ListItem));
        assert_eq!(structure_role(b"Table"), Some(StructRole::Table));
        assert_eq!(structure_role(b"TR"), Some(StructRole::TableRow));
        assert_eq!(structure_role(b"TD"), Some(StructRole::TableCell));
        assert_eq!(structure_role(b"TH"), Some(StructRole::TableCell));
        assert_eq!(structure_role(b"Figure"), Some(StructRole::Figure));
    }

    #[test]
    fn grouping_and_unknown_tags_have_no_role() {
        assert_eq!(structure_role(b"Document"), None);
        assert_eq!(structure_role(b"Sect"), None);
        assert_eq!(structure_role(b"Div"), None);
        assert_eq!(structure_role(b"Span"), None);
        assert_eq!(structure_role(b"Bogus"), None);
    }

    /// Scan a page's marked content for text only (discarding image bindings),
    /// the common shape the text tests assert against.
    fn marked_text(page: usize, content: &[u8], decoders: &FontDecoders) -> MarkedText {
        let mut text = MarkedText::new();
        let mut images = MarkedImages::new();
        collect_page_marked_content(page, content, decoders, &mut text, &mut images);
        text
    }

    #[test]
    fn mcid_text_accumulates_per_marked_range() {
        // `/P <</MCID 0>> BDC (Hello) Tj EMC  /P <</MCID 1>> BDC (World) Tj EMC`
        let content =
            b"/P <</MCID 0>> BDC BT (Hello) Tj ET EMC /P <</MCID 1>> BDC BT (World) Tj ET EMC";
        let out = marked_text(0, content, &FontDecoders::new());
        assert_eq!(out.get(&(0, 0)).map(String::as_str), Some("Hello"));
        assert_eq!(out.get(&(0, 1)).map(String::as_str), Some("World"));
    }

    #[test]
    fn text_outside_any_mcid_is_ignored() {
        let content = b"BT (loose) Tj ET";
        let out = marked_text(0, content, &FontDecoders::new());
        assert!(out.is_empty());
    }

    #[test]
    fn actual_text_overrides_decoded_glyphs() {
        // A `/Span <</MCID 0 /ActualText (ligature: fi)>> BDC (xY) Tj EMC`: the
        // shown glyphs `xY` are replaced by the `/ActualText`. MCID 1 has no
        // ActualText, so its glyphs come through unchanged.
        let content = b"/Span <</MCID 0 /ActualText (real text)>> BDC BT (xY) Tj ET EMC \
                        /Span <</MCID 1>> BDC BT (plain) Tj ET EMC";
        let out = marked_text(0, content, &FontDecoders::new());
        assert_eq!(
            out.get(&(0, 0)).map(String::as_str),
            Some("real text"),
            "/ActualText stands in for the decoded glyphs"
        );
        assert_eq!(
            out.get(&(0, 1)).map(String::as_str),
            Some("plain"),
            "an MCID without /ActualText keeps its glyphs"
        );
    }

    #[test]
    fn actual_text_decodes_utf16be_bom() {
        // `/ActualText <FEFF0041>` (UTF-16BE 'A') decodes to "A".
        let content = b"/Span <</MCID 0 /ActualText <FEFF0041>>> BDC BT (z) Tj ET EMC";
        let out = marked_text(0, content, &FontDecoders::new());
        assert_eq!(out.get(&(0, 0)).map(String::as_str), Some("A"));
    }

    #[test]
    fn image_do_binds_to_enclosing_mcid() {
        // `/Figure <</MCID 0>> BDC /Im0 Do EMC` records the image XObject name.
        let content = b"/Figure <</MCID 0>> BDC /Im0 Do EMC";
        let mut text = MarkedText::new();
        let mut images = MarkedImages::new();
        collect_page_marked_content(0, content, &FontDecoders::new(), &mut text, &mut images);
        assert_eq!(
            images.get(&(0, 0)).map(Vec::as_slice),
            Some(b"Im0".as_slice())
        );
        // First `Do` wins: a second image in the same range doesn't overwrite.
        let content2 = b"/Figure <</MCID 0>> BDC /Im0 Do /Im1 Do EMC";
        let mut images2 = MarkedImages::new();
        collect_page_marked_content(
            0,
            content2,
            &FontDecoders::new(),
            &mut MarkedText::new(),
            &mut images2,
        );
        assert_eq!(
            images2.get(&(0, 0)).map(Vec::as_slice),
            Some(b"Im0".as_slice())
        );
    }

    #[test]
    fn image_do_outside_any_mcid_is_ignored() {
        let content = b"/Im0 Do";
        let mut images = MarkedImages::new();
        collect_page_marked_content(
            0,
            content,
            &FontDecoders::new(),
            &mut MarkedText::new(),
            &mut images,
        );
        assert!(images.is_empty());
    }

    #[test]
    fn text_paragraph_is_empty_for_blank() {
        assert!(text_paragraph("   ").runs.is_empty());
        assert_eq!(text_paragraph("hi").runs.len(), 1);
    }

    // ── structure-element attribute lowering (`/A`) ─────────────────────────

    /// Identity resolver: the hand-built dictionaries below hold no indirect
    /// references, so resolution is a no-op.
    fn ident(o: &Object) -> &Object {
        o
    }

    fn name(s: &str) -> Object {
        Object::Name(s.as_bytes().to_vec())
    }

    /// A structure element with a single attribute dict `<</O owner …entries>>`
    /// in its `/A`.
    fn elem_with_attr(owner: &str, entries: &[(&str, Object)]) -> Dictionary {
        let mut attr = Dictionary::new();
        attr.set(b"O".to_vec(), name(owner));
        for (k, v) in entries {
            attr.set(k.as_bytes().to_vec(), v.clone());
        }
        let mut elem = Dictionary::new();
        elem.set(b"A".to_vec(), Object::Dictionary(attr));
        elem
    }

    #[test]
    fn cell_spans_read_colspan_and_rowspan() {
        // A `/TD` carrying `/ColSpan 2 /RowSpan 3` (the `/Table` owner).
        let td = elem_with_attr(
            "Table",
            &[
                ("ColSpan", Object::Integer(2)),
                ("RowSpan", Object::Integer(3)),
            ],
        );
        assert_eq!(cell_spans(&td, &ident), (2, 3));
    }

    #[test]
    fn cell_spans_default_to_one_when_absent_or_invalid() {
        // No `/A` at all.
        assert_eq!(cell_spans(&Dictionary::new(), &ident), (1, 1));
        // Present but zero / negative ⇒ clamp to the 1 default.
        let td = elem_with_attr(
            "Table",
            &[
                ("ColSpan", Object::Integer(0)),
                ("RowSpan", Object::Integer(-4)),
            ],
        );
        assert_eq!(cell_spans(&td, &ident), (1, 1));
    }

    #[test]
    fn attribute_value_honours_owner_array_and_wildcard() {
        // `/A [ <</O /Layout /BBox …>> <</O /Table /ColSpan 2>> ]` — the value
        // must come from the dict whose `/O` matches the requested owner.
        let mut layout = Dictionary::new();
        layout.set(b"O".to_vec(), name("Layout"));
        layout.set(
            b"BBox".to_vec(),
            Object::Array(vec![
                Object::Integer(1),
                Object::Integer(2),
                Object::Integer(3),
                Object::Integer(4),
            ]),
        );
        let mut table = Dictionary::new();
        table.set(b"O".to_vec(), name("Table"));
        table.set(b"ColSpan".to_vec(), Object::Integer(2));
        let mut elem = Dictionary::new();
        elem.set(
            b"A".to_vec(),
            Object::Array(vec![Object::Dictionary(layout), Object::Dictionary(table)]),
        );
        assert_eq!(
            attribute_value(&elem, b"Table", b"ColSpan", &ident).and_then(Object::as_i64),
            Some(2)
        );
        // A `/Layout`-owned `/BBox` is *not* returned for a `/Table` query.
        assert!(attribute_value(&elem, b"Table", b"BBox", &ident).is_none());
        // An owner-less dict acts as a wildcard.
        let mut bare = Dictionary::new();
        bare.set(b"ColSpan".to_vec(), Object::Integer(5));
        let mut e2 = Dictionary::new();
        e2.set(b"A".to_vec(), Object::Dictionary(bare));
        assert_eq!(
            attribute_value(&e2, b"Table", b"ColSpan", &ident).and_then(Object::as_i64),
            Some(5)
        );
    }

    #[test]
    fn list_numbering_maps_kinds() {
        let l = |kind: &str| elem_with_attr("List", &[("ListNumbering", name(kind))]);
        assert_eq!(
            list_numbering(&l("Decimal"), &ident),
            Some((true, ListMarker::Decimal))
        );
        assert_eq!(
            list_numbering(&l("UpperRoman"), &ident),
            Some((true, ListMarker::UpperRoman))
        );
        assert_eq!(
            list_numbering(&l("LowerAlpha"), &ident),
            Some((true, ListMarker::LowerAlpha))
        );
        assert_eq!(
            list_numbering(&l("Disc"), &ident),
            Some((false, ListMarker::Bullet('•')))
        );
        assert_eq!(
            list_numbering(&l("None"), &ident),
            Some((false, ListMarker::default()))
        );
        // Absent attribute ⇒ no override.
        assert_eq!(list_numbering(&Dictionary::new(), &ident), None);
    }

    #[test]
    fn layout_bbox_reads_rect_and_normalizes() {
        // `[llx lly urx ury]` ⇒ lower-left + width/height.
        let e = elem_with_attr(
            "Layout",
            &[(
                "BBox",
                Object::Array(vec![
                    Object::Real(72.0),
                    Object::Real(700.0),
                    Object::Real(300.0),
                    Object::Real(750.0),
                ]),
            )],
        );
        let r = layout_bbox(&e, &ident).expect("bbox");
        assert_eq!((r.x, r.y, r.w, r.h), (72.0, 700.0, 228.0, 50.0));
        // Reversed corners normalize to a positive-extent rect.
        let e2 = elem_with_attr(
            "Layout",
            &[(
                "BBox",
                Object::Array(vec![
                    Object::Integer(300),
                    Object::Integer(750),
                    Object::Integer(72),
                    Object::Integer(700),
                ]),
            )],
        );
        let r2 = layout_bbox(&e2, &ident).expect("bbox");
        assert_eq!((r2.x, r2.y, r2.w, r2.h), (72.0, 700.0, 228.0, 50.0));
        // Wrong arity ⇒ None.
        let e3 = elem_with_attr(
            "Layout",
            &[("BBox", Object::Array(vec![Object::Integer(1)]))],
        );
        assert!(layout_bbox(&e3, &ident).is_none());
    }

    // ── end-to-end: a real tagged document through the walker ───────────────

    /// Assemble a raw PDF from `(obj number, body)` parts. `Document::open`
    /// locates objects by scanning and the catalog via `trailer /Root`, so the
    /// dummy xref offsets need not be accurate (mirrors `document.rs`'s helper).
    fn raw_pdf(objects: &[(u32, String)]) -> Vec<u8> {
        let mut out = String::from("%PDF-1.7\n");
        for (num, body) in objects {
            out.push_str(&format!("{num} 0 obj\n{body}\nendobj\n"));
        }
        out.push_str(
            "xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Root 1 0 R >>\nstartxref\n0\n%%EOF",
        );
        out.into_bytes()
    }

    /// A 2-page tagged document. Page 1 (obj 8) carries a `/Table` with a single
    /// `/TR` of two `/TD`s — the first spanning 2 columns (`/ColSpan 2`) and
    /// carrying a `/Layout /BBox` — plus an ordered `/L` (`/ListNumbering
    /// /Decimal`). Page 2 (obj 9) carries a single `/P` whose `/Pg` points at it.
    fn tagged_doc() -> crate::Document {
        // Page-1 content: MCID 0/1 = the two table cells, MCID 2 = the list item.
        let c1 = "BT /F1 12 Tf \
                  /TD <</MCID 0>> BDC (A) Tj EMC \
                  /TD <</MCID 1>> BDC (B) Tj EMC \
                  /LI <</MCID 2>> BDC (one) Tj EMC ET";
        // Page-2 content: MCID 0 = the paragraph text.
        let c2 = "BT /F1 12 Tf /P <</MCID 0>> BDC (second page) Tj EMC ET";
        let pdf = raw_pdf(&[
            (
                1,
                "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 10 0 R >>".into(),
            ),
            (2, "<< /Type /Pages /Kids [8 0 R 9 0 R] /Count 2 >>".into()),
            (
                6,
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            ),
            (
                8,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /Font << /F1 6 0 R >> >> /Contents 18 0 R >>"
                    .into(),
            ),
            (
                9,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /Font << /F1 6 0 R >> >> /Contents 19 0 R >>"
                    .into(),
            ),
            (
                18,
                format!("<< /Length {} >> stream\n{c1}\nendstream", c1.len()),
            ),
            (
                19,
                format!("<< /Length {} >> stream\n{c2}\nendstream", c2.len()),
            ),
            // StructTreeRoot → /Document (obj 11).
            (10, "<< /Type /StructTreeRoot /K 11 0 R >>".into()),
            (
                11,
                "<< /Type /StructElem /S /Document /K [12 0 R 16 0 R 20 0 R] >>".into(),
            ),
            // Table (obj 12) → TR (13) → TD col-span-2 (14, with /Pg page 1) + TD (15).
            (12, "<< /S /Table /Pg 8 0 R /K 13 0 R >>".into()),
            (13, "<< /S /TR /K [14 0 R 15 0 R] >>".into()),
            (
                14,
                "<< /S /TD /Pg 8 0 R /A << /O /Table /ColSpan 2 >> /K 0 >>".into(),
            ),
            (15, "<< /S /TD /Pg 8 0 R /K 1 >>".into()),
            // List (obj 16, ordered Decimal) → LI (17).
            (
                16,
                "<< /S /L /Pg 8 0 R /A << /O /List /ListNumbering /Decimal >> /K 17 0 R >>".into(),
            ),
            (17, "<< /S /LI /Pg 8 0 R /K 2 >>".into()),
            // Paragraph on page 2 (obj 20), with a /Layout /BBox.
            (
                20,
                "<< /S /P /Pg 9 0 R /A << /O /Layout /BBox [72 700 300 750] >> /K 0 >>".into(),
            ),
        ]);
        crate::Document::open(&pdf).expect("valid tagged PDF")
    }

    #[test]
    fn tagged_table_cell_colspan_is_lowered_and_grid_aligned() {
        let doc = tagged_doc();
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let table = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("a table block");
        let row = &table.rows[0];
        assert_eq!(row.cells.len(), 2, "two physical cells");
        assert_eq!(row.cells[0].col_span, 2, "first cell spans two columns");
        assert_eq!(row.cells[1].col_span, 1, "second cell is single");
        // Logical grid width honours the span: 2 + 1 = 3 columns.
        assert_eq!(table.col_widths.len(), 3, "grid is 3 columns wide");
    }

    /// A `/Table` whose header row lives in a `/THead` group lowers to a header
    /// [`Row`] (`is_header`); a `/TR` placed directly under the `/Table` (with
    /// `/TD` cells) stays a body row.
    #[test]
    fn tagged_thead_group_marks_header_row() {
        // MCID 0/1 = the two header cells, MCID 2/3 = the two body cells.
        let c1 = "BT /F1 12 Tf \
                  /TH <</MCID 0>> BDC (H1) Tj EMC \
                  /TH <</MCID 1>> BDC (H2) Tj EMC \
                  /TD <</MCID 2>> BDC (D1) Tj EMC \
                  /TD <</MCID 3>> BDC (D2) Tj EMC ET";
        let pdf = raw_pdf(&[
            (
                1,
                "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 10 0 R >>".into(),
            ),
            (2, "<< /Type /Pages /Kids [8 0 R] /Count 1 >>".into()),
            (
                6,
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            ),
            (
                8,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /Font << /F1 6 0 R >> >> /Contents 18 0 R >>"
                    .into(),
            ),
            (
                18,
                format!("<< /Length {} >> stream\n{c1}\nendstream", c1.len()),
            ),
            (10, "<< /Type /StructTreeRoot /K 11 0 R >>".into()),
            // Table (12) → THead (13) → header TR (14) → TH (15)+TH (16);
            //            → body TR (17) → TD (18)+TD (19).
            (11, "<< /S /Document /K 12 0 R >>".into()),
            (12, "<< /S /Table /Pg 8 0 R /K [13 0 R 17 0 R] >>".into()),
            (13, "<< /S /THead /Pg 8 0 R /K 14 0 R >>".into()),
            (14, "<< /S /TR /K [15 0 R 16 0 R] >>".into()),
            (15, "<< /S /TH /Pg 8 0 R /K 0 >>".into()),
            (16, "<< /S /TH /Pg 8 0 R /K 1 >>".into()),
            (17, "<< /S /TR /K [21 0 R 22 0 R] >>".into()),
            (21, "<< /S /TD /Pg 8 0 R /K 2 >>".into()),
            (22, "<< /S /TD /Pg 8 0 R /K 3 >>".into()),
        ]);
        let doc = crate::Document::open(&pdf).expect("valid tagged PDF");
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let table = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("a table block");
        assert_eq!(table.rows.len(), 2, "header row + body row");
        assert!(table.rows[0].is_header, "/THead row → header");
        assert!(!table.rows[1].is_header, "body /TR → not header");
    }

    #[test]
    fn tagged_list_decimal_numbering_is_ordered() {
        let doc = tagged_doc();
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let list = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::List(l) => Some(l),
                _ => None,
            })
            .expect("a list block");
        assert!(list.ordered, "/ListNumbering /Decimal ⇒ ordered list");
        assert_eq!(list.marker, ListMarker::Decimal);
    }

    #[test]
    fn tagged_paragraph_bbox_sets_block_frame() {
        let doc = tagged_doc();
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let para = blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Paragraph(_)))
            .expect("a paragraph block");
        let frame = para.frame.expect("paragraph frame from /BBox");
        assert_eq!(
            (frame.x, frame.y, frame.w, frame.h),
            (72.0, 700.0, 228.0, 50.0)
        );
    }

    #[test]
    fn tagged_block_page_assignment_follows_pg() {
        let doc = tagged_doc();
        let mut ids = IdGen::default();
        let paged =
            reconstruct_from_struct_tree_paged(&doc, &mut ids).expect("paged tag tree blocks");
        // The table + list elements declare `/Pg 8 0 R` (page index 0); the
        // paragraph declares `/Pg 9 0 R` (page index 1).
        let para_page = paged
            .iter()
            .find(|(_, b)| matches!(b.kind, BlockKind::Paragraph(_)))
            .map(|(p, _)| *p)
            .expect("a paragraph entry");
        assert_eq!(
            para_page,
            Some(1),
            "paragraph is on the 2nd page, not page 1"
        );
        let table_page = paged
            .iter()
            .find(|(_, b)| matches!(b.kind, BlockKind::Table(_)))
            .map(|(p, _)| *p)
            .expect("a table entry");
        assert_eq!(table_page, Some(0), "table is on the 1st page");
    }

    /// A one-page tagged PDF whose single `/Figure` (element obj 12) binds, via
    /// MCID 0, an image XObject `/Im0` (obj 7) drawn in the content. The figure's
    /// child marked text "Chart caption" (MCID 1) is its caption. A `roles` extra
    /// string lets a test inject a `/StructTreeRoot /RoleMap` and use a custom
    /// `/S` on the figure.
    fn tagged_figure_pdf(figure_tag: &str, roles: &str) -> Vec<u8> {
        // `/Im0 Do` draws the 1×1 image inside the figure's MCID range; MCID 1 is
        // the caption text. "ABC" = the 3 RGB sample bytes of the 1×1 image
        // (valid UTF-8, so the whole PDF stays a plain string).
        let content = "q /Figure <</MCID 0>> BDC /Im0 Do EMC Q \
                       BT /F1 12 Tf /Caption <</MCID 1>> BDC (Chart caption) Tj EMC ET";
        let img = "ABC";
        raw_pdf(&[
            (
                1,
                "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 10 0 R >>".into(),
            ),
            (2, "<< /Type /Pages /Kids [8 0 R] /Count 1 >>".into()),
            (
                6,
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            ),
            (
                7,
                format!(
                    "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 \
                     /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length {} >> stream\n{img}\nendstream",
                    img.len()
                ),
            ),
            (
                8,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /Font << /F1 6 0 R >> /XObject << /Im0 7 0 R >> >> \
                 /Contents 18 0 R >>"
                    .into(),
            ),
            (
                18,
                format!("<< /Length {} >> stream\n{content}\nendstream", content.len()),
            ),
            (10, format!("<< /Type /StructTreeRoot /K 11 0 R{roles} >>")),
            (
                11,
                "<< /S /Document /K 12 0 R >>".into(),
            ),
            // The figure binds the image (MCID 0) and the caption (MCID 1).
            (
                12,
                format!("<< /S {figure_tag} /Pg 8 0 R /K [0 1] >>"),
            ),
        ])
    }

    /// #169: a `/Figure` whose marked content draws a raster XObject lowers to an
    /// `Image` block carrying the figure's caption as alt text — not dropped, and
    /// not just the caption left as a paragraph.
    #[test]
    fn tagged_figure_emits_image_with_alt() {
        let doc = crate::Document::open(&tagged_figure_pdf("/Figure", "")).expect("valid PDF");
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let img = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Image(i) => Some(i),
                _ => None,
            })
            .expect("a /Figure image block");
        assert_ne!(img.resource, 0, "the image carries a content-hash resource");
        assert_eq!(
            img.alt.as_deref(),
            Some("Chart caption"),
            "the figure's child text becomes the image alt text"
        );
        // No standalone caption paragraph is left behind.
        assert!(
            !blocks
                .iter()
                .any(|b| matches!(&b.kind, BlockKind::Paragraph(_))),
            "the caption is the image's alt text, not a loose paragraph"
        );
    }

    /// A `/Figure` whose `/Alt` entry is set uses that (an explicit alt string)
    /// over the child marked text.
    #[test]
    fn tagged_figure_prefers_explicit_alt_entry() {
        // Add an `/Alt` to the figure element (obj 12).
        let content = "q /Figure <</MCID 0>> BDC /Im0 Do EMC Q";
        let img = "ABC";
        let pdf = raw_pdf(&[
            (
                1,
                "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 10 0 R >>".into(),
            ),
            (2, "<< /Type /Pages /Kids [8 0 R] /Count 1 >>".into()),
            (
                7,
                format!(
                    "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 \
                     /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length {} >> stream\n{img}\nendstream",
                    img.len()
                ),
            ),
            (
                8,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /XObject << /Im0 7 0 R >> >> /Contents 18 0 R >>"
                    .into(),
            ),
            (
                18,
                format!("<< /Length {} >> stream\n{content}\nendstream", content.len()),
            ),
            (10, "<< /Type /StructTreeRoot /K 11 0 R >>".into()),
            (11, "<< /S /Document /K 12 0 R >>".into()),
            (
                12,
                "<< /S /Figure /Pg 8 0 R /Alt (a bar chart) /K 0 >>".into(),
            ),
        ]);
        let doc = crate::Document::open(&pdf).expect("valid PDF");
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let img = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Image(i) => Some(i),
                _ => None,
            })
            .expect("a /Figure image block");
        assert_eq!(img.alt.as_deref(), Some("a bar chart"));
    }

    /// #170: a custom `/S /ChapterTitle` declared in the `/StructTreeRoot`
    /// `/RoleMap` as mapping to `/H1` lowers to a level-1 heading.
    #[test]
    fn tagged_rolemap_resolves_custom_tag_to_heading() {
        // A one-page doc whose only element is `/S /ChapterTitle` → `/H1` via the
        // role map, with its text bound by MCID 0.
        let content = "BT /F1 12 Tf /ChapterTitle <</MCID 0>> BDC (Intro) Tj EMC ET";
        let pdf = raw_pdf(&[
            (
                1,
                "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 10 0 R >>".into(),
            ),
            (2, "<< /Type /Pages /Kids [8 0 R] /Count 1 >>".into()),
            (
                6,
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            ),
            (
                8,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /Font << /F1 6 0 R >> >> /Contents 18 0 R >>"
                    .into(),
            ),
            (
                18,
                format!(
                    "<< /Length {} >> stream\n{content}\nendstream",
                    content.len()
                ),
            ),
            // RoleMap maps the custom tag to the standard `/H1`.
            (
                10,
                "<< /Type /StructTreeRoot /K 11 0 R /RoleMap << /ChapterTitle /H1 >> >>".into(),
            ),
            (11, "<< /S /Document /K 12 0 R >>".into()),
            (12, "<< /S /ChapterTitle /Pg 8 0 R /K 0 >>".into()),
        ]);
        let doc = crate::Document::open(&pdf).expect("valid PDF");
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let heading = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Heading(h) => Some(h),
                _ => None,
            })
            .expect("the custom tag lowered to a heading via /RoleMap");
        assert_eq!(heading.level, 1, "/ChapterTitle → /H1 → level-1 heading");
        let text: String = heading
            .para
            .runs
            .iter()
            .filter_map(|r| match r {
                Inline::Run(run) => Some(run.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Intro");
    }

    /// #170: a `/RoleMap` chain (`/ChapterTitle → /Subtitle → /H2`) resolves
    /// transitively to the standard type at the end of the chain.
    #[test]
    fn tagged_rolemap_resolves_chain() {
        let content = "BT /F1 12 Tf /ChapterTitle <</MCID 0>> BDC (Deep) Tj EMC ET";
        let pdf = raw_pdf(&[
            (
                1,
                "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 10 0 R >>".into(),
            ),
            (2, "<< /Type /Pages /Kids [8 0 R] /Count 1 >>".into()),
            (
                6,
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            ),
            (
                8,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /Font << /F1 6 0 R >> >> /Contents 18 0 R >>"
                    .into(),
            ),
            (
                18,
                format!(
                    "<< /Length {} >> stream\n{content}\nendstream",
                    content.len()
                ),
            ),
            (
                10,
                "<< /Type /StructTreeRoot /K 11 0 R \
                 /RoleMap << /ChapterTitle /Subtitle /Subtitle /H2 >> >>"
                    .into(),
            ),
            (11, "<< /S /Document /K 12 0 R >>".into()),
            (12, "<< /S /ChapterTitle /Pg 8 0 R /K 0 >>".into()),
        ]);
        let doc = crate::Document::open(&pdf).expect("valid PDF");
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let heading = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Heading(h) => Some(h),
                _ => None,
            })
            .expect("chain resolves to a heading");
        assert_eq!(
            heading.level, 2,
            "/ChapterTitle → /Subtitle → /H2 → level 2"
        );
    }

    /// #171: a paragraph whose marked content carries `/ActualText` reconstructs
    /// with that text (not the decoded glyphs) end-to-end through the walker.
    #[test]
    fn tagged_actualtext_used_through_walker() {
        // The shown glyphs are "fi" (a ligature stand-in); `/ActualText (fish)`
        // is the real text the paragraph should carry.
        let content = "BT /F1 12 Tf /P <</MCID 0 /ActualText (fish)>> BDC (fi) Tj EMC ET";
        let pdf = raw_pdf(&[
            (
                1,
                "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 10 0 R >>".into(),
            ),
            (2, "<< /Type /Pages /Kids [8 0 R] /Count 1 >>".into()),
            (
                6,
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            ),
            (
                8,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 400 800] \
                 /Resources << /Font << /F1 6 0 R >> >> /Contents 18 0 R >>"
                    .into(),
            ),
            (
                18,
                format!(
                    "<< /Length {} >> stream\n{content}\nendstream",
                    content.len()
                ),
            ),
            (10, "<< /Type /StructTreeRoot /K 11 0 R >>".into()),
            (11, "<< /S /Document /K 12 0 R >>".into()),
            (12, "<< /S /P /Pg 8 0 R /K 0 >>".into()),
        ]);
        let doc = crate::Document::open(&pdf).expect("valid PDF");
        let mut ids = IdGen::default();
        let blocks = reconstruct_from_struct_tree(&doc, &mut ids).expect("tag tree blocks");
        let para = blocks
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::Paragraph(p) => Some(p),
                _ => None,
            })
            .expect("a paragraph block");
        let text: String = para
            .runs
            .iter()
            .filter_map(|r| match r {
                Inline::Run(run) => Some(run.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "fish", "/ActualText replaces the decoded glyphs");
    }

    #[test]
    fn pg_resolves_to_zero_based_page_index() {
        // Page object ids in document order; `/Pg 7 0 R` is the 2nd page → idx 1.
        let page_ids: Vec<crate::object::ObjectId> = vec![(4, 0), (7, 0), (9, 0)];
        let mut elem = Dictionary::new();
        elem.set(b"Pg".to_vec(), Object::Reference((7, 0)));
        assert_eq!(pg_page_index(&elem, &page_ids), Some(1));
        // `/Pg` pointing at the third page → idx 2.
        let mut e2 = Dictionary::new();
        e2.set(b"Pg".to_vec(), Object::Reference((9, 0)));
        assert_eq!(pg_page_index(&e2, &page_ids), Some(2));
        // Absent / non-matching `/Pg` ⇒ None (page unknown, not page 0).
        assert_eq!(pg_page_index(&Dictionary::new(), &page_ids), None);
        let mut e3 = Dictionary::new();
        e3.set(b"Pg".to_vec(), Object::Reference((99, 0)));
        assert_eq!(pg_page_index(&e3, &page_ids), None);
    }
}
