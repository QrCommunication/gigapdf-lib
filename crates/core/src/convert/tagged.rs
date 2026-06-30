//! Tagged-PDF construction for **PDF/A level A** (ISO 19005-1 §6.8 / 19005-2
//! §6.8 — the accessibility / logical-structure layer that lifts a `…b` file to
//! `…a`).
//!
//! [`Document::to_pdfa_level`](crate::Document::to_pdfa_level), when asked for a
//! level-A flavour ([`Pdfa1a`](super::pdfa::PdfaLevel::Pdfa1a) /
//! [`Pdfa2a`](super::pdfa::PdfaLevel::Pdfa2a)), calls [`build_struct_tree`] over
//! a working clone of the document's objects. That adds the three pieces a
//! Tagged PDF needs on top of the `…b` baseline:
//!
//! 1. **A `/StructTreeRoot`** (ISO 32000-1 §14.7) rooted at a `/Document`
//!    structure element, built from the **logical structure the engine already
//!    infers geometrically** — the very [`page_blocks`](crate::Document::page_blocks)
//!    reconstruction that drives the editor (paragraphs, headings, tables,
//!    lists, figures). Each leaf becomes a `/StructElem` of the matching standard
//!    role (`P`, `H1`..`H6`, `Table`→`TR`→`TH`/`TD`, `L`→`LI`→`LBody`, `Figure`).
//! 2. **Marked content** — every page's content stream is re-emitted so each
//!    text-show run (and tagged image) is wrapped in
//!    `/<role> <</MCID n>> BDC … EMC`, binding the glyphs to their `/StructElem`.
//!    Every MCID is **unique within its page** (ISO 32000-1 §14.7.4.3); a leaf
//!    that spans several show operators lists all their MCIDs in its `/K` array.
//!    **No real text is left unmarked**: any show operator not owned by a leaf is
//!    wrapped in `/Artifact BDC … EMC` (page furniture / decoration).
//! 3. **Catalog flags** — `/MarkInfo <</Marked true>>`, the `/StructTreeRoot`
//!    reference and a document `/Lang`.
//!
//! The rewrite is **rendering-neutral**: it only inserts balanced
//! `BDC`/`EMC` marked-content operators around existing operators — no geometry,
//! colour or text changes. The binding from a structure leaf back to the exact
//! content-stream operators reuses the editor's stable bridge: each model run
//! carries a [`source_index`](crate::model::InlineRun::source_index) that is the
//! page's unified [`ContentElement`](crate::content::ContentElement) index, whose
//! `op_start..=op_end` range pins the operators to wrap.

use std::collections::BTreeMap;

use crate::content::{self, ContentElement, ElementKind, Operation};
use crate::model::{Block, BlockKind, Inline, Paragraph};
use crate::object::{Dictionary, Object, ObjectId, Stream};

/// A standard structure role we emit, with its PDF tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Document,
    P,
    H(u8),
    L,
    LI,
    LBody,
    Table,
    TR,
    TH,
    TD,
    Figure,
}

impl Role {
    /// The structure-type name written to `/S`.
    fn tag(self) -> Vec<u8> {
        match self {
            Role::Document => b"Document".to_vec(),
            Role::P => b"P".to_vec(),
            Role::H(level) => format!("H{}", level.clamp(1, 6)).into_bytes(),
            Role::L => b"L".to_vec(),
            Role::LI => b"LI".to_vec(),
            Role::LBody => b"LBody".to_vec(),
            Role::Table => b"Table".to_vec(),
            Role::TR => b"TR".to_vec(),
            Role::TH => b"TH".to_vec(),
            Role::TD => b"TD".to_vec(),
            Role::Figure => b"Figure".to_vec(),
        }
    }
}

/// A per-document unique key identifying a content leaf, used to bind the
/// MCID(s) assigned during the content rewrite back to the structure node that
/// owns them.
type LeafId = u64;

/// A node of the structure tree being built. A *content leaf* (paragraph,
/// heading, cell text, list-body text, figure) owns one or more marked-content
/// sequences identified by [`LeafId`]; a *container* (Document, L, LI, Table,
/// TR, cell) holds child nodes. Object ids and the leaf's resolved
/// `(page, mcid)` pairs are filled in later passes.
struct StructNode {
    role: Role,
    /// Assigned object id (filled in [`assign_ids`]).
    id: ObjectId,
    /// For a content leaf: its binding key. `None` for containers.
    leaf_id: Option<LeafId>,
    /// 0-based page index this leaf's marked content lives on (for `/Pg`).
    /// Filled from the rewrite result.
    page: Option<usize>,
    /// The MCIDs this leaf owns on `page`, in content order (filled from the
    /// rewrite result). One per show operator the leaf covers.
    mcids: Vec<i64>,
    /// Alternate text (`/Alt`) to emit for a [`Role::Figure`] leaf (ISO 32000-1
    /// §14.9.3): the author-supplied string from
    /// [`crate::Document::set_figure_alt`] when one was set for this figure's
    /// document-global index, else a generic placeholder so the figure stays
    /// PDF/UA-valid. `None` for non-figure nodes (they emit no `/Alt`).
    alt: Option<Vec<u8>>,
    /// Child structure elements (containers only).
    kids: Vec<StructNode>,
}

impl StructNode {
    fn container(role: Role) -> Self {
        Self {
            role,
            id: (0, 0),
            leaf_id: None,
            page: None,
            mcids: Vec::new(),
            alt: None,
            kids: Vec::new(),
        }
    }

    fn leaf(role: Role, leaf_id: LeafId) -> Self {
        Self {
            role,
            id: (0, 0),
            leaf_id: Some(leaf_id),
            page: None,
            mcids: Vec::new(),
            alt: None,
            kids: Vec::new(),
        }
    }

    /// A [`Role::Figure`] leaf carrying its resolved `/Alt` bytes.
    fn figure(leaf_id: LeafId, alt: Vec<u8>) -> Self {
        let mut node = Self::leaf(Role::Figure, leaf_id);
        node.alt = Some(alt);
        node
    }
}

/// Per-page marked-content plan: which content operator maps to which leaf (so
/// the rewrite can assign that operator a unique MCID and tag it with the leaf's
/// role). Built while walking the page's blocks; consumed by the rewrite.
#[derive(Default)]
struct PagePlan {
    /// `ContentElement index → (leaf id, structure tag)` for every text run a
    /// structure leaf owns on this page.
    by_element: BTreeMap<usize, (LeafId, Vec<u8>)>,
    /// `image ordinal (0-based, element order) → (leaf id, tag)` for each image
    /// block tagged as a `/Figure`. Images carry no `source_index`, so they are
    /// bound positionally: the Nth image element ↔ the Nth image block.
    by_image_ordinal: BTreeMap<usize, (LeafId, Vec<u8>)>,
}

/// Per-page result of the content rewrite: the MCIDs each leaf was assigned (in
/// content order), and the `/ParentTree` row mapping `mcid → owning leaf`.
struct PageRewrite {
    /// `leaf id → MCIDs assigned to it on this page`, in content order.
    leaf_mcids: BTreeMap<LeafId, Vec<i64>>,
    /// `mcid (== index) → owning leaf id`. `None` slots are artifact MCIDs that
    /// belong to no structure element (we never emit those — artifacts carry no
    /// MCID — so the row only has structure MCIDs, densely indexed 0..n).
    mcid_owner: Vec<Option<LeafId>>,
}

/// The outcome of a tagging build: the struct-tree-root object id (to set on the
/// catalog) and the next free object number after every new structure object.
pub(crate) struct Built {
    pub struct_tree_root_id: ObjectId,
    pub next_free: u32,
}

/// Build the tagged-PDF structure over `objects` (a working clone of the
/// document) and return the catalog wiring. Mutates `objects`: page content
/// streams are re-emitted with marked content, and the `/StructTreeRoot`,
/// `/Document` element, every `/StructElem` and the number-tree `/ParentTree`
/// are inserted. `first_free` is the next free object number to allocate from.
///
/// Returns `None` when the document has no reconstructable text-bearing
/// structure at all (no leaf would be emitted) — the caller then ships the `…b`
/// baseline, which still validates as level B (never a regression).
pub(crate) fn build_struct_tree(
    doc: &crate::Document,
    objects: &mut BTreeMap<ObjectId, Object>,
    first_free: u32,
) -> Option<Built> {
    let page_ids = doc.page_ids().ok()?;
    let mut next = first_free;

    // Reserve the StructTreeRoot id up front so every element's `/P` chain can
    // point at it.
    let struct_tree_root_id = (next, 0u16);
    next += 1;

    // The single top-level /Document element and a monotonic leaf-id source.
    let mut document = StructNode::container(Role::Document);
    let mut leaf_seq: LeafId = 0;
    // Document-global figure counter (0-based, increments across pages in the same
    // page→content order the figures are minted). Keys `doc.figure_alt(index)` so
    // the Nth figure of the whole document gets the Nth author `/Alt`.
    let mut global_fig: usize = 0;

    // Accumulate, per leaf id, the `(page, mcids)` it owns (a leaf lives on one
    // page). Filled from each page's rewrite.
    let mut leaf_binding: BTreeMap<LeafId, (usize, Vec<i64>)> = BTreeMap::new();
    // Per page: the `/ParentTree` row of leaf ids by MCID (resolved to object
    // ids once every leaf has an id).
    let mut parent_tree_pages: Vec<Vec<Option<LeafId>>> = Vec::new();

    for (page_idx, page_id) in page_ids.iter().enumerate() {
        let page_no = (page_idx + 1) as u32;
        let blocks = doc.page_blocks(page_no);
        let elements = doc.page_elements(page_no).unwrap_or_default();

        // Walk blocks → structure nodes + a per-page plan keyed by leaf id.
        let mut fig_ordinal: usize = 0;
        let mut plan = PagePlan::default();
        let mut page_nodes: Vec<StructNode> = Vec::new();
        for block in &blocks {
            if let Some(node) = node_from_block(
                doc,
                block,
                &mut leaf_seq,
                &mut fig_ordinal,
                &mut global_fig,
                &mut plan,
            ) {
                page_nodes.push(node);
            }
        }

        // Re-emit this page's content stream with marked content, assigning a
        // unique MCID per tagged show/figure op. The content-stream object id is
        // taken from `next` (NOT `max(objects)+1`) so it can never collide with
        // the reserved struct ids that are not yet inserted.
        let content_id = (next, 0u16);
        next += 1;
        let rewrite = rewrite_page_content(doc, page_no, &elements, &plan, objects, content_id);

        // Record each leaf's `(page, mcids)` binding.
        for (leaf_id, mcids) in rewrite.leaf_mcids {
            leaf_binding.insert(leaf_id, (page_idx, mcids));
        }
        parent_tree_pages.push(rewrite.mcid_owner);

        document.kids.extend(page_nodes);

        // Tag the page dictionary with /StructParents = page index (its key into
        // the /ParentTree number tree).
        if let Some(Object::Dictionary(page)) = objects.get_mut(page_id) {
            page.set(b"StructParents", Object::Integer(page_idx as i64));
        }
    }

    // No structure at all → bail (caller ships the …b baseline).
    if document.kids.is_empty() {
        return None;
    }

    // Resolve each leaf's `(page, mcids)` from the binding gathered above, then
    // assign object ids depth-first.
    bind_leaves(&mut document.kids, &leaf_binding);
    document.id = (next, 0u16);
    next += 1;
    assign_ids(&mut document.kids, &mut next);

    // `leaf id → element object id`, for the ParentTree.
    let mut leaf_to_id: BTreeMap<LeafId, ObjectId> = BTreeMap::new();
    index_leaf_ids(&document, &mut leaf_to_id);

    // Resolve each page's ParentTree row (mcid → owning element id).
    let parent_tree_rows: Vec<(i64, Vec<ObjectId>)> = parent_tree_pages
        .iter()
        .enumerate()
        .map(|(page_idx, row)| {
            let ids: Vec<ObjectId> = row
                .iter()
                .map(|leaf| {
                    leaf.and_then(|l| leaf_to_id.get(&l).copied())
                        .unwrap_or((0, 0))
                })
                .collect();
            (page_idx as i64, ids)
        })
        .collect();

    // Emit the /Document element and all descendants as objects.
    emit_node(&document, struct_tree_root_id, objects);

    // /ParentTree number tree: maps each /StructParents key → array of elem refs.
    let parent_tree_id = (next, 0u16);
    next += 1;
    objects.insert(parent_tree_id, parent_tree_number_tree(&parent_tree_rows));

    // The /StructTreeRoot itself.
    let mut root = Dictionary::new();
    root.set(b"Type", crate::annot::name(b"StructTreeRoot"));
    root.set(b"K", Object::Reference(document.id));
    root.set(b"ParentTree", Object::Reference(parent_tree_id));
    root.set(b"ParentTreeNextKey", Object::Integer(page_ids.len() as i64));
    objects.insert(struct_tree_root_id, Object::Dictionary(root));

    Some(Built {
        struct_tree_root_id,
        next_free: next,
    })
}

/// Build a structure node (and its descendants) from a model [`Block`], minting
/// leaf ids and recording the `source_index`/image-ordinal → leaf bindings in
/// `plan`. Returns `None` for blocks with no taggable content (a shape, a rule).
///
/// `fig_ordinal` is the **per-page** image ordinal (for the positional
/// `by_image_ordinal` binding); `global_fig` is the **document-global** figure
/// index, used to look up the author's `/Alt` (`doc.figure_alt(index)`). Both
/// advance only on `BlockKind::Image`, in lock-step, exactly where a `/Figure`
/// is minted — including images nested in list items and table cells, so the
/// document-global index matches [`crate::Document::figure_count`].
fn node_from_block(
    doc: &crate::Document,
    block: &Block,
    leaf_seq: &mut LeafId,
    fig_ordinal: &mut usize,
    global_fig: &mut usize,
    plan: &mut PagePlan,
) -> Option<StructNode> {
    match &block.kind {
        BlockKind::Paragraph(p) => paragraph_node(Role::P, p, leaf_seq, plan),
        BlockKind::Heading(h) => {
            paragraph_node(Role::H(h.level.clamp(1, 6)), &h.para, leaf_seq, plan)
        }
        BlockKind::List(list) => {
            let mut l = StructNode::container(Role::L);
            for item in &list.items {
                let mut li = StructNode::container(Role::LI);
                let mut body = StructNode::container(Role::LBody);
                for b in &item.blocks {
                    if let Some(child) =
                        node_from_block(doc, b, leaf_seq, fig_ordinal, global_fig, plan)
                    {
                        body.kids.push(child);
                    }
                }
                if !body.kids.is_empty() {
                    li.kids.push(body);
                    l.kids.push(li);
                }
            }
            (!l.kids.is_empty()).then_some(l)
        }
        BlockKind::Table(table) => {
            let mut t = StructNode::container(Role::Table);
            for (r, row) in table.rows.iter().enumerate() {
                let mut tr = StructNode::container(Role::TR);
                for cell in &row.cells {
                    // First row's cells are headers (`TH`), the rest `TD`.
                    let role = if r == 0 { Role::TH } else { Role::TD };
                    let mut cell_node = StructNode::container(role);
                    for b in &cell.blocks {
                        if let Some(child) =
                            node_from_block(doc, b, leaf_seq, fig_ordinal, global_fig, plan)
                        {
                            cell_node.kids.push(child);
                        }
                    }
                    // A cell with no taggable content still needs a structure
                    // element (a TR's kids must be cells); give it an empty `/P`
                    // child so the cell is never an empty leaf.
                    if cell_node.kids.is_empty() {
                        cell_node.kids.push(StructNode::container(Role::P));
                    }
                    tr.kids.push(cell_node);
                }
                if !tr.kids.is_empty() {
                    t.kids.push(tr);
                }
            }
            (!t.kids.is_empty()).then_some(t)
        }
        BlockKind::Image(_) => {
            // A figure, bound positionally to the page's Nth image element
            // (images carry no `source_index`).
            let leaf_id = *leaf_seq;
            *leaf_seq += 1;
            let ordinal = *fig_ordinal;
            *fig_ordinal += 1;
            // Resolve this figure's `/Alt`: the author's text for this
            // document-global figure index when set, else a non-empty placeholder
            // (so the figure stays PDF/UA-valid). Advance the global index in
            // lock-step with the per-page ordinal.
            let alt = figure_alt_bytes(doc.figure_alt(*global_fig));
            *global_fig += 1;
            plan.by_image_ordinal
                .insert(ordinal, (leaf_id, Role::Figure.tag()));
            Some(StructNode::figure(leaf_id, alt))
        }
        _ => None,
    }
}

/// Encode a figure's `/Alt` string value: the author's alternate text when
/// supplied (UTF-16BE for non-ASCII, exactly as the editor encodes text), else a
/// generic non-empty placeholder so every `/Figure` carries an `/Alt` and the
/// output stays structurally PDF/UA-valid (ISO 32000-1 §14.9.3).
fn figure_alt_bytes(author_alt: Option<&str>) -> Vec<u8> {
    match author_alt {
        Some(text) if !text.is_empty() => crate::font::encode_pdf_text(text),
        _ => crate::font::encode_pdf_text("Figure"),
    }
}

/// Build a `P`/`H` leaf from a paragraph: mint a leaf id and bind each of its
/// runs' `source_index` to it. `None` if the paragraph has no source-bound runs
/// (e.g. all its text came from a form XObject — display-only, not addressable).
fn paragraph_node(
    role: Role,
    para: &Paragraph,
    leaf_seq: &mut LeafId,
    plan: &mut PagePlan,
) -> Option<StructNode> {
    let mut sources: Vec<usize> = Vec::new();
    collect_run_sources(&para.runs, &mut sources);
    if sources.is_empty() {
        return None;
    }
    let leaf_id = *leaf_seq;
    *leaf_seq += 1;
    let tag = role.tag();
    for src in sources {
        plan.by_element.entry(src).or_insert((leaf_id, tag.clone()));
    }
    Some(StructNode::leaf(role, leaf_id))
}

/// Collect every run's `source_index` (the page's unified `ContentElement`
/// index) from an inline sequence, descending into links.
fn collect_run_sources(runs: &[Inline], out: &mut Vec<usize>) {
    for inline in runs {
        match inline {
            Inline::Run(run) => {
                if let Some(src) = run.source_index {
                    out.push(src);
                }
            }
            Inline::Link { children, .. } => collect_run_sources(children, out),
            _ => {}
        }
    }
}

/// Re-emit `page_no`'s content stream with marked content per `plan`, writing the
/// rewritten stream into `objects` (a fresh content object at `content_id`).
/// Each tagged show/figure operator is wrapped in `/<role> <</MCID n>> BDC … EMC`
/// with a **page-unique** MCID assigned in content order; the matching leaf
/// collects that MCID. Untagged show operators are wrapped in `/Artifact BDC …
/// EMC` (carrying no MCID). Returns the per-leaf MCID lists and the ParentTree
/// row (`mcid → owning leaf`).
fn rewrite_page_content(
    doc: &crate::Document,
    page_no: u32,
    elements: &[ContentElement],
    plan: &PagePlan,
    objects: &mut BTreeMap<ObjectId, Object>,
    content_id: ObjectId,
) -> PageRewrite {
    let empty = PageRewrite {
        leaf_mcids: BTreeMap::new(),
        mcid_owner: Vec::new(),
    };
    let Ok(content) = doc.page_content(page_no) else {
        return empty;
    };
    let Ok(operations) = content::parse_content(&content) else {
        return empty;
    };

    // op position → (owning leaf id, tag) for every operator that must be tagged
    // (text shows owned by a leaf, image `Do`s bound to a figure).
    let mut op_owner: BTreeMap<usize, (LeafId, Vec<u8>)> = BTreeMap::new();
    let mut image_ordinal: usize = 0;
    for el in elements {
        match el.kind {
            ElementKind::Text => {
                if let Some((leaf, tag)) = plan.by_element.get(&el.index) {
                    for pos in el.op_start..=el.op_end {
                        if pos < operations.len() && is_text_show(&operations[pos].operator) {
                            op_owner.insert(pos, (*leaf, tag.clone()));
                        }
                    }
                }
            }
            ElementKind::Image => {
                if let Some((leaf, tag)) = plan.by_image_ordinal.get(&image_ordinal) {
                    for pos in el.op_start..=el.op_end {
                        if pos < operations.len() && operations[pos].operator == b"Do" {
                            op_owner.insert(pos, (*leaf, tag.clone()));
                        }
                    }
                }
                image_ordinal += 1;
            }
            ElementKind::Path => {}
        }
    }

    // Rewrite, assigning a page-unique MCID per tagged op in content order.
    let mut out: Vec<Operation> = Vec::with_capacity(operations.len() + op_owner.len() * 2);
    let mut leaf_mcids: BTreeMap<LeafId, Vec<i64>> = BTreeMap::new();
    let mut mcid_owner: Vec<Option<LeafId>> = Vec::new();
    let mut next_mcid: i64 = 0;
    for (pos, op) in operations.into_iter().enumerate() {
        match op_owner.get(&pos) {
            Some((leaf, tag)) => {
                let mcid = next_mcid;
                next_mcid += 1;
                out.push(bdc_op(tag, Some(mcid)));
                out.push(op);
                out.push(emc_op());
                leaf_mcids.entry(*leaf).or_default().push(mcid);
                mcid_owner.push(Some(*leaf));
            }
            None if is_text_show(&op.operator) => {
                // Real text with no structure leaf → mark as an artifact (no
                // MCID, so it does not consume an MCID slot).
                out.push(bdc_op(b"Artifact", None));
                out.push(op);
                out.push(emc_op());
            }
            None => out.push(op),
        }
    }

    let bytes = content::encode_content(&out);
    replace_page_contents(doc, page_no, bytes, objects, content_id);

    PageRewrite {
        leaf_mcids,
        mcid_owner,
    }
}

/// True for the four text-show operators (matches the engine's `is_text_show`).
fn is_text_show(operator: &[u8]) -> bool {
    matches!(operator, b"Tj" | b"TJ" | b"'" | b"\"")
}

/// A `/Tag <</MCID n>> BDC` operation (`/Tag <<>> BDC` when `mcid` is `None`,
/// e.g. an artifact). The empty property dict keeps `BDC` well-formed.
fn bdc_op(tag: &[u8], mcid: Option<i64>) -> Operation {
    let mut props = Dictionary::new();
    if let Some(n) = mcid {
        props.set(b"MCID", Object::Integer(n));
    }
    Operation {
        operator: b"BDC".to_vec(),
        operands: vec![Object::Name(tag.to_vec()), Object::Dictionary(props)],
    }
}

fn emc_op() -> Operation {
    Operation {
        operator: b"EMC".to_vec(),
        operands: Vec::new(),
    }
}

/// Replace `page_no`'s `/Contents` with a fresh stream holding `bytes`, in the
/// working `objects` clone, under the caller-supplied `content_id`.
fn replace_page_contents(
    doc: &crate::Document,
    page_no: u32,
    bytes: Vec<u8>,
    objects: &mut BTreeMap<ObjectId, Object>,
    content_id: ObjectId,
) {
    let Ok(page_ids) = doc.page_ids() else { return };
    let Some(&page_id) = page_ids.get((page_no - 1) as usize) else {
        return;
    };
    let mut dict = Dictionary::new();
    dict.set(b"Length", Object::Integer(bytes.len() as i64));
    objects.insert(content_id, Object::Stream(Stream::new(dict, bytes)));
    if let Some(Object::Dictionary(page)) = objects.get_mut(&page_id) {
        page.set(b"Contents", Object::Reference(content_id));
    }
}

/// Fill each leaf node's `(page, mcids)` from the binding gathered during the
/// page rewrites. A leaf with no binding (its show ops were never found —
/// unusual) is left with empty MCIDs and emits an empty `/K`.
fn bind_leaves(nodes: &mut [StructNode], binding: &BTreeMap<LeafId, (usize, Vec<i64>)>) {
    for node in nodes {
        if let Some(leaf_id) = node.leaf_id {
            if let Some((page, mcids)) = binding.get(&leaf_id) {
                node.page = Some(*page);
                node.mcids = mcids.clone();
            }
        }
        bind_leaves(&mut node.kids, binding);
    }
}

/// Assign object ids depth-first to a node list.
fn assign_ids(nodes: &mut [StructNode], next: &mut u32) {
    for node in nodes {
        node.id = (*next, 0u16);
        *next += 1;
        assign_ids(&mut node.kids, next);
    }
}

/// Index every content leaf `leaf id → element object id` for ParentTree
/// resolution.
fn index_leaf_ids(node: &StructNode, out: &mut BTreeMap<LeafId, ObjectId>) {
    if let Some(leaf_id) = node.leaf_id {
        out.insert(leaf_id, node.id);
    }
    for kid in &node.kids {
        index_leaf_ids(kid, out);
    }
}

/// Emit a structure node (and its descendants) as `/StructElem` objects, each
/// linking to `parent` via `/P`. A content leaf's `/K` is its MCID (an integer
/// for one, an array for several); a container's `/K` is its child references.
fn emit_node(node: &StructNode, parent: ObjectId, objects: &mut BTreeMap<ObjectId, Object>) {
    let mut dict = Dictionary::new();
    dict.set(b"Type", crate::annot::name(b"StructElem"));
    dict.set(b"S", Object::Name(node.role.tag()));
    dict.set(b"P", Object::Reference(parent));

    // A `/Figure` (level-A / PDF-UA) requires non-empty alternate text describing
    // the image (ISO 32000-1 §14.9.3): emit the resolved `/Alt` (author-supplied
    // when set via `Document::set_figure_alt`, else the generic placeholder).
    if let Some(alt) = &node.alt {
        dict.set(
            b"Alt",
            Object::String(alt.clone(), crate::object::StringKind::Literal),
        );
    }

    if node.leaf_id.is_some() {
        // A content leaf: `/Pg` + `/K` MCID(s).
        if let Some(page) = node.page {
            if let Some(page_id) = nth_page_id(objects, page) {
                dict.set(b"Pg", Object::Reference(page_id));
            }
        }
        match node.mcids.len() {
            0 => {}
            1 => dict.set(b"K", Object::Integer(node.mcids[0])),
            _ => {
                let kids = node.mcids.iter().map(|m| Object::Integer(*m)).collect();
                dict.set(b"K", Object::Array(kids));
            }
        }
    } else if !node.kids.is_empty() {
        // A container: `/K` is the child element reference(s).
        let kids: Vec<Object> = node.kids.iter().map(|k| Object::Reference(k.id)).collect();
        dict.set(
            b"K",
            if kids.len() == 1 {
                kids.into_iter().next().unwrap()
            } else {
                Object::Array(kids)
            },
        );
        for kid in &node.kids {
            emit_node(kid, node.id, objects);
        }
    }
    objects.insert(node.id, Object::Dictionary(dict));
}

/// The object id of the 0-based `page` index, read from the `/Pages` tree in the
/// working `objects` (the page order is stable across the clone).
fn nth_page_id(objects: &BTreeMap<ObjectId, Object>, page: usize) -> Option<ObjectId> {
    // Resolve the catalog → /Pages → flatten /Kids, mirroring `Document::page_ids`
    // but over the working clone (which the live `doc.page_ids()` also reflects,
    // since page objects keep their ids through the clone).
    let mut out: Vec<ObjectId> = Vec::new();
    let catalog = objects.values().find_map(|o| match o {
        Object::Dictionary(d) if d.get(b"Type").and_then(Object::as_name) == Some(b"Catalog") => {
            Some(d)
        }
        _ => None,
    })?;
    let pages_ref = catalog.get(b"Pages")?;
    if let Object::Reference(root) = pages_ref {
        flatten_pages(objects, *root, &mut out, 0);
    }
    out.get(page).copied()
}

/// Depth-first flatten a `/Pages` node into leaf page ids (bounded recursion).
fn flatten_pages(
    objects: &BTreeMap<ObjectId, Object>,
    id: ObjectId,
    out: &mut Vec<ObjectId>,
    depth: usize,
) {
    if depth > 50 {
        return;
    }
    let Some(Object::Dictionary(node)) = objects.get(&id) else {
        return;
    };
    match node.get(b"Type").and_then(Object::as_name) {
        Some(b"Page") => out.push(id),
        _ => {
            if let Some(Object::Array(kids)) = node.get(b"Kids") {
                for kid in kids {
                    if let Object::Reference(kid_id) = kid {
                        flatten_pages(objects, *kid_id, out, depth + 1);
                    }
                }
            } else {
                // A node without /Kids but referenced as a page — treat as a page.
                out.push(id);
            }
        }
    }
}

/// Build the `/ParentTree` as a number tree mapping each page's `/StructParents`
/// key to the array of structure-element references indexed by MCID.
fn parent_tree_number_tree(rows: &[(i64, Vec<ObjectId>)]) -> Object {
    let mut nums: Vec<Object> = Vec::with_capacity(rows.len() * 2);
    for (key, ids) in rows {
        nums.push(Object::Integer(*key));
        let refs: Vec<Object> = ids.iter().map(|id| Object::Reference(*id)).collect();
        nums.push(Object::Array(refs));
    }
    let mut dict = Dictionary::new();
    dict.set(b"Nums", Object::Array(nums));
    Object::Dictionary(dict)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_tags_are_standard() {
        assert_eq!(Role::P.tag(), b"P");
        assert_eq!(Role::H(2).tag(), b"H2");
        assert_eq!(Role::H(9).tag(), b"H6"); // clamped
        assert_eq!(Role::Table.tag(), b"Table");
        assert_eq!(Role::LBody.tag(), b"LBody");
        assert_eq!(Role::Figure.tag(), b"Figure");
    }

    #[test]
    fn bdc_marked_content_op_carries_mcid() {
        let op = bdc_op(b"P", Some(3));
        assert_eq!(op.operator, b"BDC");
        assert_eq!(op.operands.len(), 2);
        let bytes = content::encode_content(&[op]);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("/P"), "tag present: {s}");
        assert!(s.contains("/MCID 3"), "mcid present: {s}");
        assert!(s.contains("BDC"), "operator present: {s}");
    }

    #[test]
    fn artifact_bdc_has_empty_props_and_no_mcid() {
        let bytes = content::encode_content(&[bdc_op(b"Artifact", None)]);
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("/Artifact"), "{s}");
        assert!(s.contains("BDC"), "{s}");
        assert!(!s.contains("MCID"), "no mcid for artifact: {s}");
    }

    #[test]
    fn emc_closes_marked_content() {
        let bytes = content::encode_content(&[emc_op()]);
        assert_eq!(String::from_utf8_lossy(&bytes).trim(), "EMC");
    }

    /// A leaf spanning several show ops gets several **distinct** MCIDs, all
    /// listed in its `/K` array; MCIDs are unique and dense in content order.
    #[test]
    fn multi_run_leaf_gets_unique_mcids() {
        // Two text elements (op positions 0 and 1) both owned by leaf 7.
        let mut plan = PagePlan::default();
        plan.by_element.insert(0, (7, b"P".to_vec()));
        plan.by_element.insert(1, (7, b"P".to_vec()));
        let elements = vec![
            ContentElement {
                index: 0,
                kind: ElementKind::Text,
                label: "a".into(),
                op_start: 0,
                op_end: 0,
                bounds: None,
                font: None,
                font_style: None,
                color: None,
                font_size: None,
                rotation_deg: None,
                fill_alpha: None,
                nested: false,
            },
            ContentElement {
                index: 1,
                kind: ElementKind::Text,
                label: "b".into(),
                op_start: 1,
                op_end: 1,
                bounds: None,
                font: None,
                font_style: None,
                color: None,
                font_size: None,
                rotation_deg: None,
                fill_alpha: None,
                nested: false,
            },
        ];
        // Drive only the op-owner mapping + MCID assignment (no Document needed):
        // mirror `rewrite_page_content`'s assignment loop on two synthetic shows.
        let ops = [
            Operation {
                operator: b"Tj".to_vec(),
                operands: Vec::new(),
            },
            Operation {
                operator: b"Tj".to_vec(),
                operands: Vec::new(),
            },
        ];
        let mut op_owner: BTreeMap<usize, (LeafId, Vec<u8>)> = BTreeMap::new();
        for el in &elements {
            if let Some((leaf, tag)) = plan.by_element.get(&el.index) {
                for (pos, op) in ops.iter().enumerate().take(el.op_end + 1).skip(el.op_start) {
                    if is_text_show(&op.operator) {
                        op_owner.insert(pos, (*leaf, tag.clone()));
                    }
                }
            }
        }
        let mut leaf_mcids: BTreeMap<LeafId, Vec<i64>> = BTreeMap::new();
        let mut next_mcid = 0i64;
        for (pos, _) in ops.iter().enumerate() {
            if let Some((leaf, _)) = op_owner.get(&pos) {
                leaf_mcids.entry(*leaf).or_default().push(next_mcid);
                next_mcid += 1;
            }
        }
        assert_eq!(
            leaf_mcids.get(&7),
            Some(&vec![0, 1]),
            "leaf 7 owns MCIDs 0 and 1"
        );
    }

    /// End-to-end: tagging a real reconstructed document produces a catalog with
    /// `/StructTreeRoot` + `/MarkInfo<</Marked true>>` + `/Lang`, marked content
    /// (`BDC … /MCID … EMC`) in the page stream, and reopens cleanly. Exercises
    /// [`crate::Document::to_pdfa_level`] for the level-A flavour.
    #[test]
    fn to_pdfa_level_a_emits_struct_tree_and_marked_content() {
        let html = "<h1>Title</h1><p>First paragraph of body text.</p>\
                    <p>Second paragraph with more words to tag.</p>";
        let src = crate::convert::reverse::html_to_pdf(html);
        let doc = crate::Document::open(&src).expect("open reconstructed pdf");

        for level in [
            crate::convert::pdfa::PdfaLevel::Pdfa2a,
            crate::convert::pdfa::PdfaLevel::Pdfa1a,
        ] {
            let out = doc.to_pdfa_level(level);
            let s = String::from_utf8_lossy(&out);

            // Catalog flags for a Tagged PDF.
            assert!(
                s.contains("/StructTreeRoot"),
                "{level:?}: StructTreeRoot ref"
            );
            assert!(s.contains("/MarkInfo"), "{level:?}: MarkInfo present");
            assert!(s.contains("/Marked true"), "{level:?}: Marked true");
            assert!(s.contains("/Lang"), "{level:?}: document /Lang");

            // The structure tree and standard roles.
            assert!(
                s.contains("/Document"),
                "{level:?}: Document struct element"
            );
            assert!(s.contains("/StructElem"), "{level:?}: StructElem objects");
            assert!(
                s.contains("/ParentTree"),
                "{level:?}: ParentTree number tree"
            );

            // Marked content actually wraps text in the page stream. The content
            // stream is uncompressed here, so the operators appear verbatim.
            assert!(s.contains("BDC"), "{level:?}: BDC marked content");
            assert!(s.contains("/MCID"), "{level:?}: MCID in marked content");
            assert!(s.contains("EMC"), "{level:?}: EMC closes marked content");

            // The output is still a readable single-page PDF (tagging is additive).
            let reopened = crate::Document::open(&out).expect("reopen tagged pdf");
            assert!(reopened.page_count() >= 1, "{level:?}: reopens");
        }
    }

    /// A non-level-A flavour (2b) must **not** emit a struct tree or `/MarkInfo`
    /// — tagging is gated strictly on level A, so the `…b`/`…u` paths are
    /// untouched (non-regression guard for the existing levels).
    #[test]
    fn non_level_a_levels_are_not_tagged() {
        let src = crate::convert::reverse::html_to_pdf("<p>Plain body text here.</p>");
        let doc = crate::Document::open(&src).unwrap();
        for level in [
            crate::convert::pdfa::PdfaLevel::Pdfa1b,
            crate::convert::pdfa::PdfaLevel::Pdfa2b,
            crate::convert::pdfa::PdfaLevel::Pdfa2u,
            crate::convert::pdfa::PdfaLevel::Pdfa3b,
        ] {
            let out = doc.to_pdfa_level(level);
            let s = String::from_utf8_lossy(&out);
            assert!(
                !s.contains("/StructTreeRoot"),
                "{level:?}: no struct tree on a non-A level"
            );
            assert!(
                !s.contains("/MarkInfo"),
                "{level:?}: no MarkInfo on a non-A level"
            );
        }
    }

    /// A real 1×1 PNG (valid, decodable) so [`crate::Document::add_image`] embeds
    /// an image XObject the geometric reconstruction surfaces as a `BlockKind::Image`
    /// → a `/Figure` structure element when tagged.
    fn tiny_png() -> [u8; 70] {
        [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xDA, 0x63, 0xFC, 0xCF, 0xC0, 0x50, 0x0F, 0x00, 0x04, 0x85, 0x01, 0x80, 0x84, 0xA9,
            0x8C, 0x21, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    /// A one-page document carrying `count` real images (so the reconstruction
    /// yields `count` `/Figure`s when tagged).
    fn doc_with_images(count: usize) -> crate::Document {
        let src = crate::convert::reverse::txt_to_pdf("Body text page for figures.");
        let mut doc = crate::Document::open(&src).expect("open base pdf");
        let png = tiny_png();
        for i in 0..count {
            let x = 30.0 + (i as f64) * 90.0;
            doc.add_image(1, &png, x, 30.0, 80.0, 80.0, 1.0)
                .expect("embed image");
        }
        doc
    }

    /// Author-supplied alternate text set on figure 0 lands on that `/Figure`
    /// structure element's `/Alt` (ISO 32000-1 §14.9.3) in a level-A export,
    /// instead of the generic placeholder — exercised through both the
    /// PDF/A-2a/1a path and `to_tagged`.
    #[test]
    fn figure_alt_author_text_reaches_struct_elem() {
        let alt = "A bar chart of quarterly revenue";
        for tagged in tagged_outputs(|doc| {
            doc.set_figure_alt(0, alt).expect("set alt");
        }) {
            let s = String::from_utf8_lossy(&tagged.bytes);
            assert!(
                s.contains(alt),
                "{}: author /Alt present; got:\n{s}",
                tagged.label
            );
            // One figure ⇒ exactly one `/Alt`, and it is the author's (not the
            // placeholder). `/Alt` is emitted only on the `/Figure` StructElem, so
            // its count equals the figure count (unlike `/Figure`, which also
            // appears as the marked-content `BDC` tag in the page stream).
            assert!(s.contains("/Figure"), "{}: a figure element", tagged.label);
            assert_eq!(s.matches("/Alt").count(), 1, "{}: one /Alt", tagged.label);
            assert!(
                !s.contains("(Figure)"),
                "{}: author /Alt replaced the placeholder",
                tagged.label
            );
            // Reopens cleanly (tagging stays additive).
            assert!(
                crate::Document::open(&tagged.bytes).is_ok(),
                "{}: reopens",
                tagged.label
            );
        }
    }

    /// A figure with **no** author-supplied alt keeps the non-empty placeholder
    /// (`(Figure)`) so the output stays structurally PDF/UA-valid — existing
    /// callers are unaffected (no regression).
    #[test]
    fn figure_without_author_alt_keeps_placeholder() {
        for tagged in tagged_outputs(|_doc| {}) {
            let s = String::from_utf8_lossy(&tagged.bytes);
            assert!(
                s.contains("/Figure") && s.contains("/Alt"),
                "{}: figure still carries an /Alt",
                tagged.label
            );
            assert!(
                s.contains("(Figure)"),
                "{}: placeholder /Alt present",
                tagged.label
            );
        }
    }

    /// A second figure gets its **own** alt: each `/Figure` carries the alt set
    /// for its document-global index (0 → first, 1 → second), independently.
    #[test]
    fn second_figure_gets_its_own_alt() {
        let alt0 = "Company logo, a red square";
        let alt1 = "Signature of the director";
        let mut doc = doc_with_images(2);
        assert_eq!(doc.figure_count(), 2, "two figures reconstructed");
        doc.set_figure_alt(0, alt0).unwrap();
        doc.set_figure_alt(1, alt1).unwrap();
        // Both export paths must carry both author alts and exactly two `/Alt`s.
        let outputs = [
            ("to_tagged", doc.to_tagged(true)),
            (
                "pdfa-2a",
                doc.to_pdfa_level(crate::convert::pdfa::PdfaLevel::Pdfa2a),
            ),
        ];
        for (label, bytes) in outputs {
            let s = String::from_utf8_lossy(&bytes);
            assert!(s.contains(alt0), "{label}: figure 0 alt");
            assert!(s.contains(alt1), "{label}: figure 1 alt");
            // Two figures ⇒ two `/Alt` (one per `/Figure` StructElem), both authored.
            assert_eq!(s.matches("/Alt").count(), 2, "{label}: two /Alt");
            assert!(!s.contains("(Figure)"), "{label}: no placeholder left");
        }
    }

    /// Setting alt on only the second figure labels that one and leaves the first
    /// on the placeholder — the registry is keyed per figure, not all-or-nothing.
    #[test]
    fn partial_alt_labels_only_the_named_figure() {
        let alt1 = "Detail photo of the seal";
        let mut doc = doc_with_images(2);
        doc.set_figure_alt(1, alt1).unwrap();
        let out = doc.to_tagged(false);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains(alt1), "named figure carries its alt");
        assert!(s.contains("(Figure)"), "unnamed figure keeps placeholder");
        assert_eq!(s.matches("/Alt").count(), 2, "still one /Alt per figure");
    }

    /// `figure_alt` round-trips through the registry, and an empty alt is rejected
    /// (the placeholder is the "no alt" state — clearing is not via empty string).
    #[test]
    fn set_figure_alt_stores_and_validates() {
        let mut doc = doc_with_images(1);
        assert_eq!(doc.figure_alt(0), None, "no author alt initially");
        doc.set_figure_alt(0, "A photo of a bridge").unwrap();
        assert_eq!(doc.figure_alt(0), Some("A photo of a bridge"));
        // Replacing keeps the latest value.
        doc.set_figure_alt(0, "An updated caption").unwrap();
        assert_eq!(doc.figure_alt(0), Some("An updated caption"));
        // Empty alt is an invalid argument (no `/Alt` may be empty).
        assert!(doc.set_figure_alt(0, "").is_err(), "empty alt rejected");
        // The rejected call left the prior value intact.
        assert_eq!(doc.figure_alt(0), Some("An updated caption"));
    }

    /// A tagged output and the label naming which export path produced it.
    struct TaggedOut {
        label: &'static str,
        bytes: Vec<u8>,
    }

    /// Build the three level-A / tagged outputs (`pdfa-2a`, `pdfa-1a`, `to_tagged`)
    /// for a one-image document after applying `author` (which sets figure alts).
    fn tagged_outputs(author: impl Fn(&mut crate::Document)) -> Vec<TaggedOut> {
        let mut doc = doc_with_images(1);
        author(&mut doc);
        vec![
            TaggedOut {
                label: "pdfa-2a",
                bytes: doc.to_pdfa_level(crate::convert::pdfa::PdfaLevel::Pdfa2a),
            },
            TaggedOut {
                label: "pdfa-1a",
                bytes: doc.to_pdfa_level(crate::convert::pdfa::PdfaLevel::Pdfa1a),
            },
            TaggedOut {
                label: "to_tagged",
                bytes: doc.to_tagged(true),
            },
        ]
    }
}
