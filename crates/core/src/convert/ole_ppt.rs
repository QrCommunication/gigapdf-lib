//! A from-scratch reader for **legacy PowerPoint 97–2003 (`.ppt`)** — the
//! Microsoft binary presentation format ([MS-PPT]). Zero dependencies; it reads
//! real records, not a text-scrape heuristic.
//!
//! A `.ppt` is an OLE2 / CFB compound file (parsed by [`crate::convert::cfb`])
//! holding two streams of interest: **`Current User`** (a `CurrentUserAtom`
//! whose `offsetToCurrentEdit` points at the most recent edit) and **`PowerPoint
//! Document`** (the record tree itself). Resolution proceeds exactly as the spec
//! prescribes:
//!
//! 1. **Persist directory.** From `Current User` read `offsetToCurrentEdit`, seek
//!    there in `PowerPoint Document` to the newest `UserEditAtom`, then follow its
//!    `offsetLastEdit` backwards through the whole edit chain. Each `UserEditAtom`
//!    references a `PersistDirectoryAtom` (at `offsetPersistDirectory`) which maps
//!    **persist-object IDs → byte offsets** within the stream. Newer edits win on
//!    a duplicate id.
//! 2. **Document container.** The newest `UserEditAtom.docPersistIdRef` resolves
//!    (through the persist map) to the `DocumentContainer`.
//! 3. **Slides.** Each `SlidePersistAtom` in the document's `SlideListWithText`
//!    gives a slide's persist id; the persist map turns that into the offset of a
//!    `SlideContainer`. Its `OfficeArtDg` drawing holds the text: each
//!    `OfficeArtClientTextbox` pairs a `TextHeaderAtom` (placeholder kind:
//!    title/body/notes/…) with a `TextCharsAtom` (UTF-16LE) or `TextBytesAtom`
//!    (CP1252). Paragraph breaks are `\r`; vertical-tabs are soft line breaks.
//! 4. **Notes.** The `NotesPersistAtom`s in `SlideListWithText` (instance "notes")
//!    map note persist ids to `NotesContainer`s; a note's body text becomes the
//!    owning slide's [`Slide::notes`].
//!
//! Optional run/paragraph formatting (`StyleTextPropAtom`) is lowered onto the
//! resulting [`CharStyle`]/[`Align`] as far as the structure allows. Tables,
//! charts and animations are out of scope.
//!
//! **Robustness.** The bytes are untrusted. Every record walk is bounded (capped
//! by the stream length and an explicit iteration limit), the `UserEditAtom`
//! chain is cycle-guarded by a visited-offset set, all field reads are
//! length-checked, and any truncated/garbage input yields `None`/empty rather
//! than a panic or an infinite loop.
//!
//! [MS-PPT]: https://learn.microsoft.com/openspecs/office_file_formats/ms-ppt/

use crate::convert::cfb::Cfb;
use crate::model::{
    Align, Block, BlockKind, CharStyle, Document, Inline, InlineRun, Page, PageGeometry, Paragraph,
    ParagraphStyle, Placeholder, PlaceholderRole, Section, Slide, SlideBlock, TextBox,
};
use std::collections::BTreeMap;

// ───────────────────────────── record type codes ─────────────────────────────
//
// Subset of the `RecordType` enumeration (MS-PPT §2.13.24) needed to navigate
// from the persist directory down to per-slide text. Container records carry
// `recVer == 0xF`; the rest are atoms.

const RT_DOCUMENT: u16 = 0x03E8; // DocumentContainer
const RT_SLIDE: u16 = 0x03EE; // SlideContainer
const RT_NOTES: u16 = 0x03F0; // NotesContainer

const RT_SLIDE_LIST_WITH_TEXT: u16 = 0x0FF0; // SlideListWithTextContainer
const RT_SLIDE_PERSIST_ATOM: u16 = 0x03F3; // SlidePersistAtom
const RT_NOTES_ATOM: u16 = 0x03F1; // NotesAtom (notes → owning slide id)

const RT_PPDRAWING: u16 = 0x040C; // PPDrawingContainer
const RT_TEXT_HEADER_ATOM: u16 = 0x0F9F; // TextHeaderAtom (placeholder kind)
const RT_TEXT_CHARS_ATOM: u16 = 0x0FA0; // TextCharsAtom (UTF-16LE)
const RT_TEXT_BYTES_ATOM: u16 = 0x0FA8; // TextBytesAtom (CP1252)
const RT_STYLE_TEXT_PROP_ATOM: u16 = 0x0FA1; // StyleTextPropAtom (run/para props)

const RT_PERSIST_DIRECTORY_ATOM: u16 = 0x1772; // PersistDirectoryAtom
const RT_USER_EDIT_ATOM: u16 = 0x0FF5; // UserEditAtom

// Office-Art (escher) drawing records (MS-ODRAW). Containers have `recVer 0xF`.
const RT_DG_CONTAINER: u16 = 0xF002; // OfficeArtDgContainer
const RT_SPGR_CONTAINER: u16 = 0xF003; // OfficeArtSpgrContainer (group)
const RT_SP_CONTAINER: u16 = 0xF004; // OfficeArtSpContainer (one shape)
const RT_CLIENT_TEXTBOX: u16 = 0xF00D; // OfficeArtClientTextbox (holds text records)

/// Container records have `recVer == 0xF` (MS-PPT §2.3.1 record header).
const REC_VER_CONTAINER: u16 = 0xF;

/// Hard cap on records visited in any single container walk — a defence against
/// a crafted file that would otherwise make the walker spin.
const MAX_RECORDS: usize = 1 << 18;

// ───────────────────────────── public entry point ────────────────────────────

/// Parse legacy `.ppt` (MS-PPT) `bytes` into a [`Document`] whose single
/// [`BlockKind::Slide`] holds one [`Slide`] per slide, each carrying its
/// placeholder paragraphs (title first, then body) and any speaker notes.
///
/// Returns `None` when the bytes are not a Compound File, lack a `PowerPoint
/// Document` stream, or yield no resolvable slide text. Never panics or loops.
pub fn ppt_to_model(bytes: &[u8]) -> Option<Document> {
    let cfb = Cfb::open(bytes)?;
    let doc_stream = cfb.read_stream("PowerPoint Document")?;
    if doc_stream.is_empty() {
        return None;
    }
    // `Current User` gives the entry offset; if absent or unusable, fall back to
    // scanning for the newest `UserEditAtom` directly in the document stream.
    let current_user = cfb.read_stream("Current User").unwrap_or_default();
    let entry = current_edit_offset(&current_user).unwrap_or(0);

    let persist = PersistDirectory::resolve(&doc_stream, entry);
    let slides = build_slides(&doc_stream, &persist);
    if slides.is_empty() {
        return None;
    }

    Some(slide_document(slides))
}

// ───────────────────────────── record primitives ─────────────────────────────

/// One parsed record header (MS-PPT §2.3.1): version nibble, 12-bit instance,
/// type, and the byte range of the record's data (following the 8-byte header).
#[derive(Debug, Clone, Copy)]
struct RecHeader {
    rec_ver: u16,
    rec_instance: u16,
    rec_type: u16,
    /// Offset of the record's *data* within the stream (header end).
    data_start: usize,
    /// Offset one past the record's data.
    data_end: usize,
}

impl RecHeader {
    fn is_container(&self) -> bool {
        self.rec_ver == REC_VER_CONTAINER
    }

    /// The record's data slice within `stream`, length-checked.
    fn data<'a>(&self, stream: &'a [u8]) -> Option<&'a [u8]> {
        stream.get(self.data_start..self.data_end)
    }
}

/// Read the 8-byte record header at `off`. The combined `recVer`/`recInstance`
/// `u16` packs `recVer` in the low 4 bits and `recInstance` in the high 12.
/// Returns `None` if the header — or the data length it declares — runs past the
/// stream end.
fn read_header(stream: &[u8], off: usize) -> Option<RecHeader> {
    let ver_inst = read_u16(stream, off)?;
    let rec_type = read_u16(stream, off + 2)?;
    let rec_len = read_u32(stream, off + 4)? as usize;
    let data_start = off.checked_add(8)?;
    let data_end = data_start.checked_add(rec_len)?;
    if data_end > stream.len() {
        return None;
    }
    Some(RecHeader {
        rec_ver: ver_inst & 0x000F,
        rec_instance: ver_inst >> 4,
        rec_type,
        data_start,
        data_end,
    })
}

/// Iterate the child records contained directly within `[start, end)` of
/// `stream`, invoking `f` on each header. Bounded by [`MAX_RECORDS`] and by the
/// fact that every step advances past a non-empty 8-byte header, so it always
/// terminates. A malformed length that would not advance the cursor stops the
/// walk cleanly.
fn for_each_child<F: FnMut(&RecHeader)>(stream: &[u8], start: usize, end: usize, mut f: F) {
    let end = end.min(stream.len());
    let mut off = start;
    let mut steps = 0usize;
    while off + 8 <= end && steps < MAX_RECORDS {
        let Some(h) = read_header(stream, off) else {
            break;
        };
        // The next record begins right after this one's data.
        let next = h.data_end;
        if next <= off {
            break; // no forward progress ⇒ corruption; stop
        }
        f(&h);
        off = next;
        steps += 1;
    }
}

/// Find the first direct child of `[start, end)` whose type is `rec_type`.
fn find_child(stream: &[u8], start: usize, end: usize, rec_type: u16) -> Option<RecHeader> {
    let mut found = None;
    for_each_child(stream, start, end, |h| {
        if found.is_none() && h.rec_type == rec_type {
            found = Some(*h);
        }
    });
    found
}

// ───────────────────────────── persist resolution ────────────────────────────

/// The resolved persist directory: a map from persist-object id to the byte
/// offset of its record in the `PowerPoint Document` stream, plus the document
/// container's persist id taken from the newest edit.
#[derive(Debug, Default)]
struct PersistDirectory {
    /// persist id → stream offset of the referenced record's header.
    offsets: BTreeMap<u32, usize>,
    /// `docPersistIdRef` of the most recent `UserEditAtom`.
    doc_persist_id: Option<u32>,
}

impl PersistDirectory {
    /// Resolve the persist directory by walking the `UserEditAtom` chain starting
    /// at `entry` (the `Current User`'s `offsetToCurrentEdit`). Newer edits are
    /// visited first, so their persist entries take precedence; the chain is
    /// cycle-guarded and bounded.
    fn resolve(stream: &[u8], entry: usize) -> PersistDirectory {
        let mut dir = PersistDirectory::default();
        let mut visited: Vec<usize> = Vec::new();
        let mut off = entry;
        let mut steps = 0usize;
        // The chain can be at most one edit per record; cap generously by the
        // stream length (every edit consumes ≥ a header) and a hard ceiling.
        let cap = (stream.len() / 8).saturating_add(1).min(MAX_RECORDS);

        while steps < cap {
            // Stop on a re-visited offset (cycle) or an out-of-range pointer.
            if visited.contains(&off) {
                break;
            }
            visited.push(off);

            let Some(edit) = read_user_edit_atom(stream, off) else {
                break;
            };
            // The most recent edit (visited first) owns the document reference.
            if dir.doc_persist_id.is_none() && edit.doc_persist_id_ref != 0 {
                dir.doc_persist_id = Some(edit.doc_persist_id_ref);
            }
            // Merge this edit's persist directory; do NOT overwrite ids already
            // set by a newer edit (insert-if-absent).
            dir.merge_persist_dir(stream, edit.offset_persist_directory);

            if edit.offset_last_edit == 0 || edit.offset_last_edit >= stream.len() {
                break; // start of the chain (or invalid) ⇒ done
            }
            off = edit.offset_last_edit;
            steps += 1;
        }

        // If `Current User` was missing/garbage, `entry == 0` may not point at a
        // UserEditAtom. As a fallback, scan the whole stream for the LAST
        // UserEditAtom (highest offset = newest) and re-resolve from there.
        if dir.offsets.is_empty() || dir.doc_persist_id.is_none() {
            if let Some(scan) = scan_last_user_edit(stream) {
                if scan != entry {
                    return PersistDirectory::resolve(stream, scan);
                }
            }
        }
        dir
    }

    /// Read the `PersistDirectoryAtom` at `off` and fold its (persist id →
    /// offset) entries into `self`, without clobbering ids already recorded.
    fn merge_persist_dir(&mut self, stream: &[u8], off: usize) {
        if off == 0 || off >= stream.len() {
            return;
        }
        let Some(h) = read_header(stream, off) else {
            return;
        };
        if h.rec_type != RT_PERSIST_DIRECTORY_ATOM {
            return;
        }
        let Some(data) = h.data(stream) else {
            return;
        };
        for (id, offset) in parse_persist_directory(data) {
            // Offset must address a real record header within the stream.
            if (offset as usize) + 8 <= stream.len() {
                self.offsets.entry(id).or_insert(offset as usize);
            }
        }
    }

    /// Resolve a persist id to the offset of the record it references.
    fn offset_of(&self, id: u32) -> Option<usize> {
        self.offsets.get(&id).copied()
    }
}

/// One `UserEditAtom`'s fields we need (MS-PPT §2.3.3): the previous edit's
/// offset, this edit's persist-directory offset, and the document persist id.
#[derive(Debug, Clone, Copy)]
struct UserEditAtom {
    offset_last_edit: usize,
    offset_persist_directory: usize,
    doc_persist_id_ref: u32,
}

/// Read a `UserEditAtom` whose header sits at `off`. The atom's data layout is:
/// `lastSlideIdRef`(4) `version`(2) `minorVersion`(1) `majorVersion`(1)
/// `offsetLastEdit`(4) `offsetPersistDirectory`(4) `docPersistIdRef`(4) … —
/// only the three offsets/ids above are consumed. `None` if the header at `off`
/// is not a `UserEditAtom` or the data is too short.
fn read_user_edit_atom(stream: &[u8], off: usize) -> Option<UserEditAtom> {
    let h = read_header(stream, off)?;
    if h.rec_type != RT_USER_EDIT_ATOM {
        return None;
    }
    let d = h.data(stream)?;
    // Need at least up to docPersistIdRef: 4+2+1+1+4+4+4 = 20 bytes.
    if d.len() < 20 {
        return None;
    }
    let offset_last_edit = u32_at(d, 8)? as usize;
    let offset_persist_directory = u32_at(d, 12)? as usize;
    let doc_persist_id_ref = u32_at(d, 16)?;
    Some(UserEditAtom {
        offset_last_edit,
        offset_persist_directory,
        doc_persist_id_ref,
    })
}

/// Scan the entire stream for the highest-offset `UserEditAtom` (the newest
/// edit). Used only as a fallback when `Current User` did not give a usable
/// entry point. Bounded by the stream length.
fn scan_last_user_edit(stream: &[u8]) -> Option<usize> {
    let mut last = None;
    let mut off = 0usize;
    let mut steps = 0usize;
    while off + 8 <= stream.len() && steps < MAX_RECORDS {
        let Some(h) = read_header(stream, off) else {
            break;
        };
        let next = h.data_end;
        if next <= off {
            break;
        }
        if !h.is_container() && h.rec_type == RT_USER_EDIT_ATOM {
            last = Some(off);
        }
        off = next;
        steps += 1;
    }
    last
}

/// Parse a `PersistDirectoryAtom`'s data into `(persistId, offset)` pairs
/// (MS-PPT §2.3.4 — `rgPersistDirEntry`). The body is a sequence of
/// `PersistDirectoryEntry`s, each a `u32` bit-field (`persistId` in the low 20
/// bits, `cPersist` count in the high 12) followed by `cPersist` little-endian
/// `u32` offsets, which apply to consecutive ids `persistId, persistId+1, …`.
/// Bounded by the data length.
fn parse_persist_directory(data: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut steps = 0usize;
    while i + 4 <= data.len() && steps < MAX_RECORDS {
        let field = u32_at(data, i).unwrap_or(0);
        i += 4;
        let persist_id = field & 0x000F_FFFF; // low 20 bits
        let count = (field >> 20) & 0x0000_0FFF; // high 12 bits
        if count == 0 {
            // A zero count is malformed; without progress on offsets we would
            // spin reading the same field — advance was already +4, continue.
            steps += 1;
            continue;
        }
        for k in 0..count {
            let Some(offset) = u32_at(data, i) else {
                return out; // truncated entry list ⇒ stop with what we have
            };
            i += 4;
            out.push((persist_id.wrapping_add(k), offset));
        }
        steps += 1;
    }
    out
}

/// Read the `offsetToCurrentEdit` from a `Current User` stream's
/// `CurrentUserAtom` (MS-PPT §2.3.2). The atom starts at offset 0: `size`(4)
/// `headerToken`(4) `offsetToCurrentEdit`(4) … Returns `None` if the stream is
/// too short or the header token is not one of the documented values.
fn current_edit_offset(current_user: &[u8]) -> Option<usize> {
    if current_user.len() < 12 {
        return None;
    }
    // headerToken: 0xE391C05F (not encrypted) or 0xF3D1C4DF (encrypted). We only
    // read the offset; either token is acceptable as a sanity check.
    let token = u32_at(current_user, 4)?;
    if token != 0xE391_C05F && token != 0xF3D1_C4DF {
        return None;
    }
    Some(u32_at(current_user, 8)? as usize)
}

// ──────────────────────────────── slide build ────────────────────────────────

/// The placeholder kind of a `TextHeaderAtom` (MS-PPT §2.13.31 — `TextTypeEnum`).
/// Only the distinctions we map to a [`PlaceholderRole`] matter; everything else
/// is treated as body text.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TextType {
    Title,
    Body,
    Notes,
    Other,
}

impl TextType {
    fn from_code(code: u32) -> TextType {
        match code {
            0 => TextType::Title, // Title
            1 => TextType::Body,  // Body
            2 => TextType::Notes, // Notes
            6 => TextType::Title, // CenterTitle
            5 => TextType::Body,  // CenterBody
            7 => TextType::Body,  // HalfBody
            8 => TextType::Body,  // QuarterBody
            _ => TextType::Other, // Other / number / date / footer / header
        }
    }
}

/// One block of text recovered from a `OfficeArtClientTextbox`: its placeholder
/// kind plus the decoded paragraphs (each a run of [`Inline`]s, already split on
/// `\r`).
#[derive(Debug, Default)]
struct TextRun {
    kind: Option<TextType>,
    paragraphs: Vec<ModelPara>,
}

/// A decoded paragraph: its inline content and resolved paragraph alignment.
#[derive(Debug, Default)]
struct ModelPara {
    runs: Vec<Inline>,
    align: Align,
}

/// Walk the resolved persist directory and build the model slides. Each slide's
/// persist id (from the document's `SlideListWithText`) resolves to a
/// `SlideContainer`; notes ids resolve to `NotesContainer`s and attach to the
/// owning slide.
fn build_slides(stream: &[u8], persist: &PersistDirectory) -> Vec<Slide> {
    // Locate the DocumentContainer via the newest edit's docPersistIdRef.
    let Some(doc) = locate_document(stream, persist) else {
        return Vec::new();
    };

    // Slide + notes persist ids in document order, from SlideListWithText.
    let SlideListing { slides, notes } = read_slide_listing(stream, doc);

    // Map "slide persist id" → decoded notes paragraphs (resolved from the notes
    // containers, keyed by the slide id each note references).
    let mut notes_by_slide: BTreeMap<u32, Vec<ModelPara>> = BTreeMap::new();
    for note in &notes {
        let Some(off) = persist.offset_of(note.persist_id) else {
            continue;
        };
        let Some(h) = read_header(stream, off) else {
            continue;
        };
        if h.rec_type != RT_NOTES {
            continue;
        }
        let owner = note
            .slide_id
            .or_else(|| notes_owner_slide(stream, &h))
            .unwrap_or(0);
        let mut paras = Vec::new();
        collect_drawing_text(stream, &h, &mut |tr| {
            for p in tr.paragraphs {
                if !p.runs.is_empty() {
                    paras.push(p);
                }
            }
        });
        if !paras.is_empty() {
            notes_by_slide.entry(owner).or_default().extend(paras);
        }
    }

    let mut out = Vec::new();
    for sp in &slides {
        let Some(off) = persist.offset_of(sp.persist_id) else {
            continue;
        };
        let Some(h) = read_header(stream, off) else {
            continue;
        };
        if h.rec_type != RT_SLIDE {
            continue;
        }
        let mut placeholders: Vec<Placeholder> = Vec::new();
        let mut title_first: Vec<Placeholder> = Vec::new();

        collect_drawing_text(stream, &h, &mut |tr| {
            let role = role_of(tr.kind);
            let blocks = paras_to_blocks(&tr.paragraphs);
            if blocks.is_empty() {
                return;
            }
            let ph = Placeholder {
                role: role.clone(),
                block: textbox_block(blocks),
            };
            if role == PlaceholderRole::Title {
                title_first.push(ph);
            } else {
                placeholders.push(ph);
            }
        });

        // Title placeholders lead, then the body/other placeholders in order.
        let mut all = title_first;
        all.append(&mut placeholders);

        // Notes for this slide (matched by the slide's own persist id).
        let notes_blocks = notes_by_slide
            .get(&sp.slide_number_id)
            .or_else(|| notes_by_slide.get(&sp.persist_id))
            .map(|ps| paras_to_blocks(ps));

        // Emit a slide even if it carries no text placeholders but has notes, so
        // a notes-only slide still appears; skip a truly empty slide.
        if all.is_empty() && notes_blocks.as_ref().map(|b| b.is_empty()).unwrap_or(true) {
            continue;
        }

        out.push(Slide {
            placeholders: all,
            notes: notes_blocks.filter(|b| !b.is_empty()),
            ..Slide::default()
        });
    }
    out
}

/// Resolve and return the `DocumentContainer`'s header. Tries the persist map
/// first (newest edit's `docPersistIdRef`); falls back to scanning the stream
/// for a top-level `RT_DOCUMENT` record.
fn locate_document(stream: &[u8], persist: &PersistDirectory) -> Option<RecHeader> {
    if let Some(id) = persist.doc_persist_id {
        if let Some(off) = persist.offset_of(id) {
            if let Some(h) = read_header(stream, off) {
                if h.rec_type == RT_DOCUMENT {
                    return Some(h);
                }
            }
        }
    }
    // Fallback: any persist offset that lands on a DocumentContainer.
    for &off in persist.offsets.values() {
        if let Some(h) = read_header(stream, off) {
            if h.rec_type == RT_DOCUMENT {
                return Some(h);
            }
        }
    }
    // Last resort: scan the whole stream for a DocumentContainer record.
    scan_for_record(stream, RT_DOCUMENT)
}

/// Scan the whole stream for the first record of `rec_type` at a record
/// boundary. Bounded; used only as a recovery path.
fn scan_for_record(stream: &[u8], rec_type: u16) -> Option<RecHeader> {
    let mut off = 0usize;
    let mut steps = 0usize;
    while off + 8 <= stream.len() && steps < MAX_RECORDS {
        let Some(h) = read_header(stream, off) else {
            break;
        };
        if h.rec_type == rec_type {
            return Some(h);
        }
        let next = h.data_end;
        if next <= off {
            break;
        }
        off = next;
        steps += 1;
    }
    None
}

/// A slide/notes persist reference recovered from `SlideListWithText`.
#[derive(Debug, Clone, Copy)]
struct PersistRef {
    persist_id: u32,
    /// For a `SlidePersistAtom`, the slide's own `slideId`. For a notes ref this
    /// is unused.
    slide_number_id: u32,
    /// For a notes ref, the `slideId` the notes belong to (from `NotesAtom`).
    slide_id: Option<u32>,
}

/// The document's slide and notes listings (persist references), in order.
#[derive(Debug, Default)]
struct SlideListing {
    slides: Vec<PersistRef>,
    notes: Vec<PersistRef>,
}

/// Read the `DocumentContainer`'s `SlideListWithText` children into slide and
/// notes persist references. The document holds up to three `SlideListWithText`
/// containers distinguished by `recInstance` (0 = slides, 1 = master, 2 =
/// notes); we read the slide list (instance 0) for slides and the notes list
/// (instance 2) for notes, plus any `NotesPersistAtom`s found.
fn read_slide_listing(stream: &[u8], doc: RecHeader) -> SlideListing {
    let mut listing = SlideListing::default();
    for_each_child(stream, doc.data_start, doc.data_end, |h| {
        if h.rec_type != RT_SLIDE_LIST_WITH_TEXT {
            return;
        }
        // instance 2 ⇒ notes list; anything else with SlidePersistAtoms ⇒ slides.
        let is_notes_list = h.rec_instance == 2;
        for_each_child(stream, h.data_start, h.data_end, |c| {
            if c.rec_type == RT_SLIDE_PERSIST_ATOM {
                if let Some(r) = read_slide_persist_atom(stream, *c) {
                    if is_notes_list {
                        listing.notes.push(r);
                    } else {
                        listing.slides.push(r);
                    }
                }
            }
        });
    });
    listing
}

/// Parse a `SlidePersistAtom` (MS-PPT §2.5.4): `persistIdRef`(4) flags(4)
/// `cTexts`(4) `slideId`(4) `reserved`(4). For a notes-list entry the same
/// record shape is used by `NotesPersistAtom`; both expose a `persistIdRef`.
fn read_slide_persist_atom(stream: &[u8], h: RecHeader) -> Option<PersistRef> {
    let d = h.data(stream)?;
    if d.len() < 4 {
        return None;
    }
    let persist_id = u32_at(d, 0)?;
    let slide_number_id = u32_at(d, 12).unwrap_or(0);
    Some(PersistRef {
        persist_id,
        slide_number_id,
        slide_id: None,
    })
}

/// Read the `slideId` a `NotesContainer` belongs to, from its `NotesAtom`
/// (MS-PPT §2.5.6 — first field `slideIdRef`(4)).
fn notes_owner_slide(stream: &[u8], notes: &RecHeader) -> Option<u32> {
    let atom = find_child(stream, notes.data_start, notes.data_end, RT_NOTES_ATOM)?;
    let d = atom.data(stream)?;
    u32_at(d, 0)
}

// ──────────────────────────── drawing / text walk ────────────────────────────

/// Walk a slide/notes container's drawing for client-textbox text, invoking `f`
/// once per recovered [`TextRun`]. The path is `…Container → PPDrawing →
/// OfficeArtDgContainer → (spgr/sp)* → OfficeArtClientTextbox → text atoms`. The
/// walk recurses through escher group containers with a depth bound.
fn collect_drawing_text<F: FnMut(TextRun)>(stream: &[u8], container: &RecHeader, f: &mut F) {
    // Find the PPDrawing inside the slide/notes container.
    let Some(ppd) = find_child(
        stream,
        container.data_start,
        container.data_end,
        RT_PPDRAWING,
    ) else {
        return;
    };
    // PPDrawing wraps an OfficeArtDgContainer.
    let Some(dg) = find_child(stream, ppd.data_start, ppd.data_end, RT_DG_CONTAINER) else {
        return;
    };
    walk_escher(stream, dg.data_start, dg.data_end, 0, f);
}

/// Recursively walk an escher container range, descending into shape-group
/// (`SpgrContainer`) and shape (`SpContainer`) containers, and decoding any
/// `OfficeArtClientTextbox` into a [`TextRun`]. `depth` is bounded to stop a
/// pathological nesting.
fn walk_escher<F: FnMut(TextRun)>(
    stream: &[u8],
    start: usize,
    end: usize,
    depth: usize,
    f: &mut F,
) {
    if depth > 64 {
        return;
    }
    for_each_child(stream, start, end, |h| match h.rec_type {
        RT_SPGR_CONTAINER | RT_DG_CONTAINER => {
            walk_escher(stream, h.data_start, h.data_end, depth + 1, f);
        }
        RT_SP_CONTAINER => {
            // A shape carries at most one client-textbox as a direct child; decode
            // it. We do NOT recurse into the sp container — a shape does not nest
            // further shapes, and recursing would re-find and double-count the same
            // client-textbox.
            if let Some(tb) = find_child(stream, h.data_start, h.data_end, RT_CLIENT_TEXTBOX) {
                if let Some(tr) = decode_client_textbox(stream, &tb) {
                    f(tr);
                }
            }
        }
        RT_CLIENT_TEXTBOX => {
            if let Some(tr) = decode_client_textbox(stream, h) {
                f(tr);
            }
        }
        _ => {}
    });
}

/// Decode one `OfficeArtClientTextbox`'s text records into a [`TextRun`]: its
/// `TextHeaderAtom` (placeholder kind), the `TextCharsAtom`/`TextBytesAtom`
/// payload, and an optional `StyleTextPropAtom` for per-run formatting. Returns
/// `None` if it holds no text atom.
fn decode_client_textbox(stream: &[u8], tb: &RecHeader) -> Option<TextRun> {
    let mut kind: Option<TextType> = None;
    let mut raw_text: Option<String> = None;
    let mut style_data: Option<Vec<u8>> = None;

    for_each_child(stream, tb.data_start, tb.data_end, |h| match h.rec_type {
        RT_TEXT_HEADER_ATOM => {
            if let Some(d) = h.data(stream) {
                let code = u32_at(d, 0).unwrap_or(99);
                kind = Some(TextType::from_code(code));
            }
        }
        RT_TEXT_CHARS_ATOM => {
            if raw_text.is_none() {
                if let Some(d) = h.data(stream) {
                    raw_text = Some(decode_utf16le(d));
                }
            }
        }
        RT_TEXT_BYTES_ATOM => {
            if raw_text.is_none() {
                if let Some(d) = h.data(stream) {
                    raw_text = Some(decode_cp1252(d));
                }
            }
        }
        RT_STYLE_TEXT_PROP_ATOM => {
            if style_data.is_none() {
                if let Some(d) = h.data(stream) {
                    style_data = Some(d.to_vec());
                }
            }
        }
        _ => {}
    });

    let text = raw_text?;
    if text.is_empty() {
        return Some(TextRun {
            kind,
            paragraphs: Vec::new(),
        });
    }
    let paragraphs = build_paragraphs(&text, style_data.as_deref());
    Some(TextRun { kind, paragraphs })
}

// ─────────────────────────── text → paragraphs/runs ──────────────────────────

/// Split a text-atom string into model paragraphs and apply (best-effort)
/// `StyleTextPropAtom` formatting. Paragraph breaks are `\r` (0x0D); a
/// vertical-tab (0x0B) becomes a soft line break within the paragraph. Runs are
/// styled from the style atom's character-run table when present.
fn build_paragraphs(text: &str, style: Option<&[u8]>) -> Vec<ModelPara> {
    // Character-run styling: a flat list of `(run_char_count, CharStyle)` and a
    // flat list of `(para_char_count, Align)`, both covering the text in order.
    let parsed = style.map(|d| parse_style_text_prop(d, char_count(text)));

    let mut paras: Vec<ModelPara> = Vec::new();
    let mut cur_runs: Vec<Inline> = Vec::new();
    let mut cur = String::new();
    // Running character index (in chars, matching the style table units).
    let mut char_idx = 0usize;
    let mut cur_style = style_at(&parsed, char_idx);

    // Flush the accumulated text span into a styled run.
    let push_span = |runs: &mut Vec<Inline>, buf: &mut String, st: &CharStyle| {
        if !buf.is_empty() {
            runs.push(Inline::Run(InlineRun {
                text: std::mem::take(buf),
                style: st.clone(),
                source_index: None,
            }));
        }
    };

    for ch in text.chars() {
        match ch {
            '\r' => {
                // End of paragraph.
                push_span(&mut cur_runs, &mut cur, &cur_style);
                let align = para_align_at(&parsed, char_idx);
                paras.push(ModelPara {
                    runs: std::mem::take(&mut cur_runs),
                    align,
                });
            }
            '\u{000B}' => {
                // Vertical tab ⇒ soft line break.
                push_span(&mut cur_runs, &mut cur, &cur_style);
                cur_runs.push(Inline::LineBreak);
            }
            '\n' => {} // PPT uses \r; ignore stray \n
            _ => {
                // Style may change at this character boundary; flush first.
                let st = style_at(&parsed, char_idx);
                if st != cur_style {
                    push_span(&mut cur_runs, &mut cur, &cur_style);
                    cur_style = st;
                }
                cur.push(ch);
            }
        }
        char_idx += 1;
    }
    // Trailing paragraph (text not terminated by a final \r).
    push_span(&mut cur_runs, &mut cur, &cur_style);
    if !cur_runs.is_empty() {
        let align = para_align_at(&parsed, char_idx.saturating_sub(1));
        paras.push(ModelPara {
            runs: cur_runs,
            align,
        });
    }
    paras
}

/// Look up the [`CharStyle`] covering character index `idx` in the parsed style
/// table, or the default style when there is none.
fn style_at(parsed: &Option<ParsedStyle>, idx: usize) -> CharStyle {
    match parsed {
        Some(p) => p.char_style_at(idx),
        None => CharStyle::default(),
    }
}

/// Look up the paragraph [`Align`] covering character index `idx`.
fn para_align_at(parsed: &Option<ParsedStyle>, idx: usize) -> Align {
    match parsed {
        Some(p) => p.align_at(idx),
        None => Align::default(),
    }
}

/// A parsed `StyleTextPropAtom`: run-length-coded character and paragraph
/// property spans over the text (in character units).
#[derive(Debug, Default)]
struct ParsedStyle {
    /// `(cumulative_end_char, style)` character-run spans.
    char_runs: Vec<(usize, CharStyle)>,
    /// `(cumulative_end_char, align)` paragraph spans.
    para_runs: Vec<(usize, Align)>,
}

impl ParsedStyle {
    fn char_style_at(&self, idx: usize) -> CharStyle {
        for (end, st) in &self.char_runs {
            if idx < *end {
                return st.clone();
            }
        }
        CharStyle::default()
    }

    fn align_at(&self, idx: usize) -> Align {
        for (end, al) in &self.para_runs {
            if idx < *end {
                return *al;
            }
        }
        Align::default()
    }
}

/// Parse a `StyleTextPropAtom` (MS-PPT §2.9.43). The body is a paragraph-run
/// table followed by a character-run table, each entry: `count`(u32) +
/// (paragraph: `paragraphStyleMask`(u32) + masked fields) / (character:
/// `characterStyleMask`(u32) + masked fields). Run counts sum to `text_len + 1`
/// (the trailing paragraph mark). We decode alignment from the paragraph mask
/// and bold/italic/underline/size/colour from the character mask; unknown masks
/// are length-skipped so the table stays in sync. Best-effort and fully bounded.
fn parse_style_text_prop(data: &[u8], text_len: usize) -> ParsedStyle {
    let mut out = ParsedStyle::default();
    let mut pos = 0usize;

    // The paragraph-run table covers `text_len + 1` characters total.
    let target = text_len.saturating_add(1);

    // --- paragraph-run table ---
    let mut covered = 0usize;
    let mut steps = 0usize;
    while covered < target && pos + 8 <= data.len() && steps < MAX_RECORDS {
        let count = u32_at(data, pos).unwrap_or(0) as usize;
        pos += 4;
        let mask = u32_at(data, pos).unwrap_or(0);
        pos += 4;
        let (align, consumed) = decode_paragraph_props(data, pos, mask);
        pos += consumed;
        let run = count.max(1);
        covered += run;
        out.para_runs.push((covered.min(target), align));
        steps += 1;
        if count == 0 {
            break; // avoid a zero-length spin
        }
    }

    // --- character-run table ---
    let mut covered = 0usize;
    let mut steps = 0usize;
    while covered < target && pos + 8 <= data.len() && steps < MAX_RECORDS {
        let count = u32_at(data, pos).unwrap_or(0) as usize;
        pos += 4;
        let mask = u32_at(data, pos).unwrap_or(0);
        pos += 4;
        let (style, consumed) = decode_character_props(data, pos, mask);
        pos += consumed;
        let run = count.max(1);
        covered += run;
        out.char_runs.push((covered.min(target), style));
        steps += 1;
        if count == 0 {
            break;
        }
    }

    out
}

/// Decode the alignment from a paragraph property mask, returning `(align,
/// bytes_consumed)`. Only the fields preceding/at the alignment bit are sized;
/// the rest are skipped by their known widths. Conservative: an unrecognised
/// layout falls back to `Left` and consumes only what it can account for.
fn decode_paragraph_props(data: &[u8], start: usize, mask: u32) -> (Align, usize) {
    // PPF (paragraph property flags) field widths, in mask-bit order (MS-PPT
    // §2.9.20 TextPFException). We only need `textAlignment` (bit 3, u16).
    let mut pos = start;
    let mut align = Align::default();

    // hasBullet flags occupy bit 0..=2 collectively as a single u16 when any set.
    if mask & 0x0000_000F != 0 {
        pos = skip_u16(pos);
    }
    if mask & 0x0000_0010 != 0 {
        // bulletChar (u16)
        pos = skip_u16(pos);
    }
    if mask & 0x0000_0020 != 0 {
        // bulletFontRef (u16)
        pos = skip_u16(pos);
    }
    if mask & 0x0000_0040 != 0 {
        // bulletSize (u16)
        pos = skip_u16(pos);
    }
    if mask & 0x0000_0080 != 0 {
        // bulletColor (u32)
        pos = skip_u32(pos);
    }
    if mask & 0x0000_0100 != 0 {
        // textAlignment (u16)
        if let Some(v) = u16_at(data, pos) {
            align = align_from_code(v);
        }
        pos = skip_u16(pos);
    }
    (align, pos.saturating_sub(start))
}

/// Decode bold/italic/underline/size/colour from a character property mask,
/// returning `(style, bytes_consumed)`. Field order follows the
/// `CharFormatFlags`/`TextCFException` layout (MS-PPT §2.9.13/§2.9.17). Unknown
/// trailing fields are skipped by width so the run table stays aligned.
fn decode_character_props(data: &[u8], start: usize, mask: u32) -> (CharStyle, usize) {
    let mut st = CharStyle::default();
    let mut pos = start;

    // styleFlags (bits 0..=15) present iff any of bits 0..=15 set ⇒ a u16 of the
    // bold/italic/underline/… flags.
    if mask & 0x0000_FFFF != 0 {
        if let Some(flags) = u16_at(data, pos) {
            st.bold = flags & 0x0001 != 0;
            st.italic = flags & 0x0002 != 0;
            st.underline = flags & 0x0004 != 0;
            st.strike = flags & 0x0100 != 0;
        }
        pos = skip_u16(pos);
    }
    if mask & 0x0001_0000 != 0 {
        // typeface (font index, u16) — no family table resolved here.
        pos = skip_u16(pos);
    }
    if mask & 0x0002_0000 != 0 {
        // oldEAFontRef (u16)
        pos = skip_u16(pos);
    }
    if mask & 0x0004_0000 != 0 {
        // ansiFontRef (u16)
        pos = skip_u16(pos);
    }
    if mask & 0x0008_0000 != 0 {
        // symbolFontRef (u16)
        pos = skip_u16(pos);
    }
    if mask & 0x0010_0000 != 0 {
        // fontSize (u16, points)
        if let Some(sz) = u16_at(data, pos) {
            st.size_pt = sz as f64;
        }
        pos = skip_u16(pos);
    }
    if mask & 0x0020_0000 != 0 {
        // color (u32 — OfficeArtCOLORREF: r,g,b,flags)
        if let Some(c) = u32_at(data, pos) {
            st.color = colorref_to_rgb(c);
        }
        pos = skip_u32(pos);
    }
    if mask & 0x0040_0000 != 0 {
        // position (superscript/subscript, i16)
        pos = skip_u16(pos);
    }
    (st, pos.saturating_sub(start))
}

/// Map a PPT alignment code (MS-PPT §2.9.4 `TextAlignmentEnum`) to a model
/// [`Align`]. `0` = left, `1` = center, `2` = right, `3`/`4` = justify-like.
fn align_from_code(v: u16) -> Align {
    match v {
        1 => Align::Center,
        2 => Align::Right,
        3..=6 => Align::Justify, // justify / distributed / thai-distributed
        _ => Align::Left,
    }
}

/// Convert an `OfficeArtCOLORREF` (`r`,`g`,`b` in the low three bytes, flags in
/// the high byte) to RGB `0.0..=1.0`. A scheme-index colour (the high "is index"
/// flag set, bit 0x01000000) has no concrete RGB here, so it yields `None`.
fn colorref_to_rgb(c: u32) -> Option<[f64; 3]> {
    // Bit 0 of the flags byte (0x01 << 24) marks a colour-scheme index, not a
    // literal RGB. We can only honour literal RGB values.
    if c & 0x0100_0000 != 0 {
        return None;
    }
    let r = (c & 0xFF) as f64 / 255.0;
    let g = ((c >> 8) & 0xFF) as f64 / 255.0;
    let b = ((c >> 16) & 0xFF) as f64 / 255.0;
    Some([r, g, b])
}

// ───────────────────────────── model assembly ────────────────────────────────

/// Map a [`TextType`] to a model [`PlaceholderRole`].
fn role_of(kind: Option<TextType>) -> PlaceholderRole {
    match kind {
        Some(TextType::Title) => PlaceholderRole::Title,
        Some(TextType::Notes) => PlaceholderRole::Other("notes".to_string()),
        Some(TextType::Body) | Some(TextType::Other) | None => PlaceholderRole::Body,
    }
}

/// Build model [`Block`]s (one paragraph per [`ModelPara`]) from decoded text.
fn paras_to_blocks(paras: &[ModelPara]) -> Vec<Block> {
    let mut blocks = Vec::new();
    for p in paras {
        if p.runs.is_empty() {
            continue;
        }
        blocks.push(Block {
            kind: BlockKind::Paragraph(Paragraph {
                style: ParagraphStyle {
                    align: p.align,
                    ..ParagraphStyle::default()
                },
                runs: p.runs.clone(),
                ..Paragraph::default()
            }),
            ..Block::default()
        });
    }
    blocks
}

/// Wrap paragraph blocks in a [`TextBox`] [`Block`] (the placeholder body).
fn textbox_block(blocks: Vec<Block>) -> Block {
    Block {
        kind: BlockKind::TextBox(TextBox { blocks }),
        ..Block::default()
    }
}

/// Build the final [`Document`]: a single absolute page holding one
/// [`BlockKind::Slide`] block with all slides. Geometry is the 16:9 default
/// (960 × 540 pt) used by the rest of the slide pipeline. Constructed via
/// `..Default::default()` so concurrent additions to the model structs do not
/// break this call site.
fn slide_document(slides: Vec<Slide>) -> Document {
    let block = Block {
        kind: BlockKind::Slide(SlideBlock { slides }),
        ..Block::default()
    };
    Document {
        sections: vec![Section {
            geometry: slide_geometry(),
            pages: vec![Page {
                blocks: vec![block],
                absolute: false,
            }],
            ..Section::default()
        }],
        ..Document::default()
    }
}

/// The slide page geometry: 16:9 (960 × 540 pt = 10in × 7.5in at 96 dpi-ish),
/// matching the engine's slide fallback. Built from the model default with width
/// and height overridden.
fn slide_geometry() -> PageGeometry {
    PageGeometry {
        width: 960.0,
        height: 540.0,
        ..PageGeometry::default()
    }
}

// ──────────────────────────── decode / read helpers ──────────────────────────

/// Count the `char`s in `s` (the unit the style-prop run tables use). This
/// matches how we index the text while applying styles.
fn char_count(s: &str) -> usize {
    s.chars().count()
}

/// Decode a UTF-16LE byte slice into a `String`, dropping the trailing record
/// padding and any embedded NULs. Lone surrogates are skipped. Used for
/// `TextCharsAtom`.
fn decode_utf16le(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() / 2);
    let mut i = 0usize;
    while i + 1 < data.len() {
        let cu = u16::from_le_bytes([data[i], data[i + 1]]);
        i += 2;
        if cu == 0x0000 {
            continue; // skip NUL padding
        }
        // Handle a surrogate pair if present; otherwise map the BMP unit.
        if (0xD800..=0xDBFF).contains(&cu) {
            if i + 1 < data.len() {
                let lo = u16::from_le_bytes([data[i], data[i + 1]]);
                if (0xDC00..=0xDFFF).contains(&lo) {
                    i += 2;
                    let c = 0x1_0000 + (((cu as u32 - 0xD800) << 10) | (lo as u32 - 0xDC00));
                    if let Some(ch) = char::from_u32(c) {
                        out.push(ch);
                    }
                    continue;
                }
            }
            continue; // unpaired high surrogate ⇒ drop
        }
        if (0xDC00..=0xDFFF).contains(&cu) {
            continue; // unpaired low surrogate ⇒ drop
        }
        if let Some(ch) = char::from_u32(cu as u32) {
            out.push(ch);
        }
    }
    out
}

/// Decode a CP1252 (Windows-1252) byte slice into a `String`, mapping the
/// 0x80..=0x9F band through the standard Windows-1252 table. Used for
/// `TextBytesAtom`. NUL bytes are skipped.
fn decode_cp1252(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len());
    for &b in data {
        if b == 0 {
            continue;
        }
        let ch = if (0x80..=0x9F).contains(&b) {
            // The only band that differs from ISO-8859-1.
            CP1252_HIGH[(b - 0x80) as usize]
        } else {
            // ASCII and the upper Latin-1 band map 1:1 to U+0000..U+00FF.
            b as char
        };
        out.push(ch);
    }
    out
}

/// Windows-1252 mapping for the 0x80..=0x9F band (the only bytes that differ
/// from ISO-8859-1). `\u{FFFD}` marks the five undefined positions.
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}', '\u{017D}', '\u{FFFD}',
    '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
];

/// Read a little-endian `u16` at `off` in `bytes`, or `None` past the end.
fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    let end = off.checked_add(2)?;
    let s = bytes.get(off..end)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

/// Read a little-endian `u32` at `off` in `bytes`, or `None` past the end.
fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let s = bytes.get(off..end)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Read a little-endian `u16` at `off` within a record-data slice.
fn u16_at(data: &[u8], off: usize) -> Option<u16> {
    read_u16(data, off)
}

/// Read a little-endian `u32` at `off` within a record-data slice.
fn u32_at(data: &[u8], off: usize) -> Option<u32> {
    read_u32(data, off)
}

/// Advance a cursor past a `u16` field (saturating).
fn skip_u16(pos: usize) -> usize {
    pos.saturating_add(2)
}

/// Advance a cursor past a `u32` field (saturating).
fn skip_u32(pos: usize) -> usize {
    pos.saturating_add(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal-but-valid `.ppt` builder. It writes a CFB compound file holding a
    // `Current User` stream and a `PowerPoint Document` stream whose record tree
    // is: UserEditAtom → PersistDirectoryAtom → DocumentContainer (with a
    // SlideListWithText of SlidePersistAtoms) → SlideContainer(s) (each with a
    // PPDrawing → DgContainer → SpContainer → ClientTextbox → text atom).
    //
    // To keep the CFB layout trivial we make both streams ≥ the 4096 mini-cutoff
    // so they live in the regular FAT, and lay sectors out explicitly.

    /// Append an 8-byte record header for `(rec_ver, rec_instance, rec_type,
    /// data_len)` to `buf`.
    fn rec_header(buf: &mut Vec<u8>, rec_ver: u16, rec_instance: u16, rec_type: u16, len: u32) {
        let ver_inst = (rec_ver & 0x0F) | (rec_instance << 4);
        buf.extend_from_slice(&ver_inst.to_le_bytes());
        buf.extend_from_slice(&rec_type.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
    }

    /// Build a `TextBytesAtom` (CP1252) record for `text`.
    fn text_bytes_atom(text: &str) -> Vec<u8> {
        let body: Vec<u8> = text.bytes().collect();
        let mut r = Vec::new();
        rec_header(&mut r, 0, 0, RT_TEXT_BYTES_ATOM, body.len() as u32);
        r.extend_from_slice(&body);
        r
    }

    /// Build a `TextCharsAtom` (UTF-16LE) record for `text`.
    fn text_chars_atom(text: &str) -> Vec<u8> {
        let mut body = Vec::new();
        for u in text.encode_utf16() {
            body.extend_from_slice(&u.to_le_bytes());
        }
        let mut r = Vec::new();
        rec_header(&mut r, 0, 0, RT_TEXT_CHARS_ATOM, body.len() as u32);
        r.extend_from_slice(&body);
        r
    }

    /// Build a `TextHeaderAtom` carrying the placeholder kind `code`.
    fn text_header_atom(code: u32) -> Vec<u8> {
        let mut r = Vec::new();
        rec_header(&mut r, 0, 0, RT_TEXT_HEADER_ATOM, 4);
        r.extend_from_slice(&code.to_le_bytes());
        r
    }

    /// Wrap `inner` records in a container record of `rec_type`/`rec_instance`.
    fn container(rec_type: u16, rec_instance: u16, inner: Vec<u8>) -> Vec<u8> {
        let mut r = Vec::new();
        rec_header(
            &mut r,
            REC_VER_CONTAINER,
            rec_instance,
            rec_type,
            inner.len() as u32,
        );
        r.extend_from_slice(&inner);
        r
    }

    /// A ClientTextbox holding a text-header + a text atom (bytes or chars).
    fn client_textbox(kind: u32, atom: Vec<u8>) -> Vec<u8> {
        let mut inner = text_header_atom(kind);
        inner.extend_from_slice(&atom);
        container(RT_CLIENT_TEXTBOX, 0, inner)
    }

    /// An SpContainer wrapping a ClientTextbox.
    fn sp_with_text(kind: u32, atom: Vec<u8>) -> Vec<u8> {
        container(RT_SP_CONTAINER, 0, client_textbox(kind, atom))
    }

    /// A slide's drawing: PPDrawing → DgContainer → (sp containers).
    fn drawing(sp_records: Vec<u8>) -> Vec<u8> {
        let dg = container(RT_DG_CONTAINER, 0, sp_records);
        container(RT_PPDRAWING, 0, dg)
    }

    /// A SlideContainer holding a drawing with the given sp records.
    fn slide_container(sp_records: Vec<u8>) -> Vec<u8> {
        container(RT_SLIDE, 0, drawing(sp_records))
    }

    /// A NotesContainer with a NotesAtom (owning slide id) + a notes drawing.
    fn notes_container(owner_slide_id: u32, sp_records: Vec<u8>) -> Vec<u8> {
        let mut inner = Vec::new();
        // NotesAtom: slideIdRef(4) + flags(2) + reserved(2) — 8 bytes minimum.
        rec_header(&mut inner, 0, 0, RT_NOTES_ATOM, 8);
        inner.extend_from_slice(&owner_slide_id.to_le_bytes());
        inner.extend_from_slice(&0u32.to_le_bytes());
        inner.extend_from_slice(&drawing(sp_records));
        container(RT_NOTES, 0, inner)
    }

    /// A SlidePersistAtom: persistIdRef(4) flags(4) cTexts(4) slideId(4) res(4).
    fn slide_persist_atom(persist_id: u32, slide_id: u32) -> Vec<u8> {
        let mut r = Vec::new();
        rec_header(&mut r, 0, 0, RT_SLIDE_PERSIST_ATOM, 20);
        r.extend_from_slice(&persist_id.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes()); // flags
        r.extend_from_slice(&0u32.to_le_bytes()); // cTexts
        r.extend_from_slice(&slide_id.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes()); // reserved
        r
    }

    /// Assemble a `PowerPoint Document` stream. `slides` is a list of
    /// `(persist_id, slide_id, sp_records)`; `notes` a list of `(notes_persist_id,
    /// owner_slide_id, sp_records)`. Returns the stream bytes plus the byte
    /// offsets needed to wire up the persist directory and the UserEditAtom.
    struct DocStream {
        bytes: Vec<u8>,
        /// (persist_id → offset) for every container we placed.
        offsets: Vec<(u32, u32)>,
        doc_persist_id: u32,
        offset_persist_dir: u32,
        offset_user_edit: u32,
    }

    fn build_doc_stream(
        slides: &[(u32, u32, Vec<u8>)],
        notes: &[(u32, u32, Vec<u8>)],
    ) -> DocStream {
        let mut stream = Vec::new();
        let mut offsets: Vec<(u32, u32)> = Vec::new();

        // 1) Each SlideContainer first; record its offset under its persist id.
        for (pid, sid, sp) in slides {
            offsets.push((*pid, stream.len() as u32));
            let _ = sid;
            stream.extend_from_slice(&slide_container(sp.clone()));
        }
        // 2) Each NotesContainer.
        for (pid, owner, sp) in notes {
            offsets.push((*pid, stream.len() as u32));
            stream.extend_from_slice(&notes_container(*owner, sp.clone()));
        }

        // 3) DocumentContainer: holds a SlideListWithText (instance 0) of
        //    SlidePersistAtoms, and (if any notes) a notes list (instance 2).
        let doc_persist_id = 1000u32;
        let doc_offset;
        {
            // Slides list (instance 0).
            let mut slist_inner = Vec::new();
            for (pid, sid, _) in slides {
                slist_inner.extend_from_slice(&slide_persist_atom(*pid, *sid));
            }
            let slides_list = container(RT_SLIDE_LIST_WITH_TEXT, 0, slist_inner);

            let mut doc_inner = slides_list;

            if !notes.is_empty() {
                let mut nlist_inner = Vec::new();
                for (pid, _owner, _) in notes {
                    nlist_inner.extend_from_slice(&slide_persist_atom(*pid, 0));
                }
                let notes_list = container(RT_SLIDE_LIST_WITH_TEXT, 2, nlist_inner);
                doc_inner.extend_from_slice(&notes_list);
            }

            doc_offset = stream.len() as u32;
            offsets.push((doc_persist_id, doc_offset));
            stream.extend_from_slice(&container(RT_DOCUMENT, 0, doc_inner));
        }

        // 4) PersistDirectoryAtom mapping every persist id → offset. Build one
        //    PersistDirectoryEntry per id (cPersist = 1) for simplicity.
        let offset_persist_dir = stream.len() as u32;
        {
            let mut pd_body = Vec::new();
            for (id, off) in &offsets {
                let field = (*id & 0x000F_FFFF) | (1u32 << 20); // cPersist = 1
                pd_body.extend_from_slice(&field.to_le_bytes());
                pd_body.extend_from_slice(&off.to_le_bytes());
            }
            rec_header(
                &mut stream,
                0,
                0,
                RT_PERSIST_DIRECTORY_ATOM,
                pd_body.len() as u32,
            );
            stream.extend_from_slice(&pd_body);
        }

        // 5) UserEditAtom: lastSlideIdRef(4) version(2) minor(1) major(1)
        //    offsetLastEdit(4) offsetPersistDirectory(4) docPersistIdRef(4)
        //    persistIdSeed(4) … — 24 bytes is enough for the reader.
        let offset_user_edit = stream.len() as u32;
        {
            rec_header(&mut stream, 0, 0, RT_USER_EDIT_ATOM, 28);
            stream.extend_from_slice(&0u32.to_le_bytes()); // lastSlideIdRef
            stream.extend_from_slice(&0u16.to_le_bytes()); // version
            stream.push(0); // minorVersion
            stream.push(0); // majorVersion
            stream.extend_from_slice(&0u32.to_le_bytes()); // offsetLastEdit (none)
            stream.extend_from_slice(&offset_persist_dir.to_le_bytes());
            stream.extend_from_slice(&doc_persist_id.to_le_bytes());
            stream.extend_from_slice(&0u32.to_le_bytes()); // persistIdSeed
            stream.extend_from_slice(&0u32.to_le_bytes()); // padding
        }

        DocStream {
            bytes: stream,
            offsets,
            doc_persist_id,
            offset_persist_dir,
            offset_user_edit,
        }
    }

    /// Build a `Current User` stream pointing at `offset_user_edit`.
    fn current_user_stream(offset_user_edit: u32) -> Vec<u8> {
        let mut s = Vec::new();
        s.extend_from_slice(&0x14u32.to_le_bytes()); // size
        s.extend_from_slice(&0xE391_C05Fu32.to_le_bytes()); // headerToken (not enc)
        s.extend_from_slice(&offset_user_edit.to_le_bytes()); // offsetToCurrentEdit
        s.extend_from_slice(&0u16.to_le_bytes()); // lenUserName
        s.extend_from_slice(&0u16.to_le_bytes()); // docFileVersion
        s.push(0); // majorVersion
        s.push(0); // minorVersion
        s.extend_from_slice(&0u16.to_le_bytes()); // unused
        s
    }

    // ── Minimal CFB writer (v3, 512-byte sectors) ──
    //
    // Lays out: FAT (sector 0), directory (sector 1), then two big streams in
    // consecutive FAT-chained sectors. Both streams are ≥ 4096 bytes so they use
    // the regular FAT (no mini-stream needed).

    const SIG: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    const FREESECT: u32 = 0xFFFF_FFFF;
    const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
    const FATSECT: u32 = 0xFFFF_FFFD;

    fn put_u16(buf: &mut [u8], off: usize, v: u16) {
        buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    #[allow(clippy::too_many_arguments)]
    fn dir_entry(
        dir: &mut [u8],
        i: usize,
        name: &str,
        obj_type: u8,
        left: u32,
        right: u32,
        child: u32,
        start: u32,
        size: u64,
    ) {
        let base = i * 128;
        let mut nlen = 0usize;
        for (k, ch) in name.encode_utf16().enumerate() {
            put_u16(dir, base + k * 2, ch);
            nlen = (k + 1) * 2;
        }
        nlen += 2;
        put_u16(dir, base + 64, nlen as u16);
        dir[base + 66] = obj_type;
        dir[base + 67] = 1;
        put_u32(dir, base + 68, left);
        put_u32(dir, base + 72, right);
        put_u32(dir, base + 76, child);
        put_u32(dir, base + 116, start);
        put_u32(dir, base + 120, (size & 0xFFFF_FFFF) as u32);
        put_u32(dir, base + 124, (size >> 32) as u32);
    }

    /// Build a CFB holding `Current User` and `PowerPoint Document` streams.
    fn build_ppt_cfb(current_user: &[u8], pp_doc: &[u8]) -> Vec<u8> {
        // Pad each stream up to a whole number of 512-byte sectors and ensure
        // each is ≥ 4096 bytes (regular FAT).
        let pad = |data: &[u8]| -> Vec<u8> {
            let min = 4096usize;
            let want = data.len().max(min);
            let secs = want.div_ceil(512);
            let mut v = vec![0u8; secs * 512];
            v[..data.len()].copy_from_slice(data);
            v
        };
        let cu = pad(current_user);
        let pd = pad(pp_doc);
        let cu_sectors = cu.len() / 512;
        let pd_sectors = pd.len() / 512;

        // Sector layout:
        //   0           : FAT
        //   1           : directory
        //   2..         : Current User stream
        //   ..          : PowerPoint Document stream
        let cu_first = 2u32;
        let pd_first = cu_first + cu_sectors as u32;
        let total = 2 + cu_sectors + pd_sectors;

        let mut sectors = vec![[0u8; 512]; total];

        // FAT (sector 0).
        {
            let fat = &mut sectors[0];
            put_u32(fat, 0, FATSECT); // sector 0 = FAT
            put_u32(fat, 4, ENDOFCHAIN); // sector 1 = directory
            for k in 0..cu_sectors {
                let sec = cu_first as usize + k;
                let next = if k + 1 < cu_sectors {
                    sec as u32 + 1
                } else {
                    ENDOFCHAIN
                };
                put_u32(fat, sec * 4, next);
            }
            for k in 0..pd_sectors {
                let sec = pd_first as usize + k;
                let next = if k + 1 < pd_sectors {
                    sec as u32 + 1
                } else {
                    ENDOFCHAIN
                };
                put_u32(fat, sec * 4, next);
            }
            for sec in total..(512 / 4) {
                put_u32(fat, sec * 4, FREESECT);
            }
        }

        // Directory (sector 1): Root + the two streams as its children.
        {
            let dir = &mut sectors[1];
            // Root: child → slot 1 (stream tree root). Root needs a non-empty
            // mini-stream start; we have none, so start = ENDOFCHAIN, size 0.
            dir_entry(
                dir,
                0,
                "Root Entry",
                5,
                FREESECT,
                FREESECT,
                1,
                ENDOFCHAIN,
                0,
            );
            // Slot 1: "Current User" (right → slot 2). The declared size is the
            // padded length (≥ 4096) so the reader routes through the regular
            // FAT, not the (absent) mini-stream; trailing zero padding lies past
            // every real record and is harmless to the reader/parser.
            dir_entry(
                dir,
                1,
                "Current User",
                2,
                FREESECT,
                2,
                FREESECT,
                cu_first,
                cu.len() as u64,
            );
            // Slot 2: "PowerPoint Document" (same padded-size rationale).
            dir_entry(
                dir,
                2,
                "PowerPoint Document",
                2,
                FREESECT,
                FREESECT,
                FREESECT,
                pd_first,
                pd.len() as u64,
            );
        }

        // Stream payloads.
        for k in 0..cu_sectors {
            sectors[cu_first as usize + k].copy_from_slice(&cu[k * 512..(k + 1) * 512]);
        }
        for k in 0..pd_sectors {
            sectors[pd_first as usize + k].copy_from_slice(&pd[k * 512..(k + 1) * 512]);
        }

        // Assemble file: 512-byte header + sectors.
        let mut bytes = vec![0u8; 512 + total * 512];
        for (i, sec) in sectors.iter().enumerate() {
            bytes[512 + i * 512..512 + (i + 1) * 512].copy_from_slice(sec);
        }
        bytes[0..8].copy_from_slice(&SIG);
        put_u16(&mut bytes, 24, 0x0003); // major version 3
        put_u16(&mut bytes, 26, 0x003E);
        put_u16(&mut bytes, 28, 0xFFFE); // byte order
        put_u16(&mut bytes, 30, 0x0009); // 512-byte sectors
        put_u16(&mut bytes, 32, 0x0006); // 64-byte mini-sectors
        put_u32(&mut bytes, 44, 1); // num FAT sectors
        put_u32(&mut bytes, 48, 1); // dir start sector
        put_u32(&mut bytes, 56, 4096); // mini cutoff
        put_u32(&mut bytes, 60, ENDOFCHAIN); // mini-FAT start (none)
        put_u32(&mut bytes, 64, 0); // num mini-FAT
        put_u32(&mut bytes, 68, ENDOFCHAIN); // DIFAT start
        put_u32(&mut bytes, 72, 0); // num DIFAT
        put_u32(&mut bytes, 76, 0); // inline DIFAT[0] = FAT in sector 0
        for k in 1..109 {
            put_u32(&mut bytes, 76 + k * 4, FREESECT);
        }
        bytes
    }

    /// Build a complete `.ppt` for the given slides/notes specs.
    fn build_ppt(slides: &[(u32, u32, Vec<u8>)], notes: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        let doc = build_doc_stream(slides, notes);
        let cu = current_user_stream(doc.offset_user_edit);
        // Sanity: the persist directory carries every container offset.
        assert!(
            !doc.offsets.is_empty(),
            "persist directory must be populated"
        );
        assert!(doc.offset_persist_dir > 0, "persist dir offset must be set");
        assert_eq!(
            doc.doc_persist_id, 1000,
            "document persist id fixed in builder"
        );
        build_ppt_cfb(&cu, &doc.bytes)
    }

    #[test]
    fn single_slide_title_via_text_bytes() {
        let sp = sp_with_text(0, text_bytes_atom("Slide Title"));
        let bytes = build_ppt(&[(2, 256, sp)], &[]);
        let doc = ppt_to_model(&bytes).expect("one slide must parse");
        let slides = collect_slides(&doc);
        assert_eq!(slides.len(), 1, "exactly one slide");
        let text = slide_text(&slides[0]);
        assert!(
            text.contains("Slide Title"),
            "title text recovered: {text:?}"
        );
    }

    #[test]
    fn single_slide_title_via_text_chars() {
        // UTF-16 path with a non-ASCII char to prove decoding.
        let sp = sp_with_text(0, text_chars_atom("Résumé Title"));
        let bytes = build_ppt(&[(2, 256, sp)], &[]);
        let doc = ppt_to_model(&bytes).expect("one slide must parse");
        let slides = collect_slides(&doc);
        assert_eq!(slides.len(), 1, "exactly one slide");
        assert!(
            slide_text(&slides[0]).contains("Résumé Title"),
            "utf16 decoded"
        );
    }

    #[test]
    fn two_slide_deck_in_order() {
        let sp1 = sp_with_text(0, text_bytes_atom("First Slide"));
        let sp2 = sp_with_text(0, text_bytes_atom("Second Slide"));
        let bytes = build_ppt(&[(2, 256, sp1), (3, 257, sp2)], &[]);
        let doc = ppt_to_model(&bytes).expect("two slides must parse");
        let slides = collect_slides(&doc);
        assert_eq!(slides.len(), 2, "two slides in order");
        assert!(
            slide_text(&slides[0]).contains("First Slide"),
            "slide 1 text"
        );
        assert!(
            slide_text(&slides[1]).contains("Second Slide"),
            "slide 2 text"
        );
    }

    #[test]
    fn title_then_body_order() {
        // A body placeholder then a title in the drawing; the title must lead.
        let mut sp = sp_with_text(1, text_bytes_atom("Body Content"));
        sp.extend_from_slice(&sp_with_text(0, text_bytes_atom("The Title")));
        let bytes = build_ppt(&[(2, 256, sp)], &[]);
        let doc = ppt_to_model(&bytes).expect("slide must parse");
        let slides = collect_slides(&doc);
        assert_eq!(slides.len(), 1, "one slide");
        let roles: Vec<&PlaceholderRole> = slides[0].placeholders.iter().map(|p| &p.role).collect();
        assert_eq!(roles.first(), Some(&&PlaceholderRole::Title), "title leads");
        assert!(
            slide_text(&slides[0]).contains("Body Content"),
            "body present"
        );
    }

    #[test]
    fn paragraph_breaks_split_on_cr() {
        let sp = sp_with_text(1, text_bytes_atom("Line one\rLine two\rLine three"));
        let bytes = build_ppt(&[(2, 256, sp)], &[]);
        let doc = ppt_to_model(&bytes).expect("slide must parse");
        let slides = collect_slides(&doc);
        // The body placeholder's TextBox should hold three paragraph blocks.
        let para_count = body_paragraph_count(&slides[0]);
        assert_eq!(para_count, 3, "three paragraphs split on \\r");
    }

    #[test]
    fn notes_text_attaches_to_slide() {
        let slide_sp = sp_with_text(0, text_bytes_atom("Deck Title"));
        let notes_sp = sp_with_text(2, text_bytes_atom("Speaker note here"));
        // Notes owner slideId = 256 (matches the slide's slide_id).
        let bytes = build_ppt(&[(2, 256, slide_sp)], &[(5, 256, notes_sp)]);
        let doc = ppt_to_model(&bytes).expect("slide+notes must parse");
        let slides = collect_slides(&doc);
        assert_eq!(slides.len(), 1, "one slide");
        let notes = slides[0].notes.as_ref().expect("notes present");
        let text = blocks_text(notes);
        assert!(
            text.contains("Speaker note here"),
            "notes text recovered: {text:?}"
        );
    }

    #[test]
    fn garbage_is_none_no_panic() {
        assert!(ppt_to_model(b"").is_none(), "empty ⇒ None");
        assert!(
            ppt_to_model(b"not a ppt file at all").is_none(),
            "garbage ⇒ None"
        );
        // CFB signature but no real container.
        let mut sig = SIG.to_vec();
        sig.extend_from_slice(&[0u8; 600]);
        assert!(ppt_to_model(&sig).is_none(), "sig only ⇒ None");
    }

    #[test]
    fn truncated_ppt_is_none_or_safe() {
        let sp = sp_with_text(0, text_bytes_atom("Slide Title"));
        let bytes = build_ppt(&[(2, 256, sp)], &[]);
        // Truncating anywhere must never panic; result is None or a safe doc.
        for cut in [16, 64, 600, bytes.len() / 2, bytes.len().saturating_sub(8)] {
            let cut = cut.min(bytes.len());
            let _ = ppt_to_model(&bytes[..cut]);
        }
    }

    #[test]
    fn cyclic_user_edit_chain_terminates() {
        // Hand-build a document stream whose UserEditAtom points its
        // offsetLastEdit back at itself; resolution must not loop.
        let sp = sp_with_text(0, text_bytes_atom("Cycle"));
        let mut doc = build_doc_stream(&[(2, 256, sp)], &[]);
        // Patch offsetLastEdit (data byte 8 of the UserEditAtom) to point at the
        // UserEditAtom's own header — a self-cycle.
        let ue = doc.offset_user_edit as usize;
        let self_off = doc.offset_user_edit;
        // data starts 8 bytes after the header; offsetLastEdit is data[8..12].
        let field = ue + 8 + 8;
        doc.bytes[field..field + 4].copy_from_slice(&self_off.to_le_bytes());
        let cu = current_user_stream(doc.offset_user_edit);
        let bytes = build_ppt_cfb(&cu, &doc.bytes);
        // Must terminate and still recover the slide (the self-edit is visited once).
        let _ = ppt_to_model(&bytes);
    }

    // ── test helpers to read the produced model ──

    fn collect_slides(doc: &Document) -> Vec<Slide> {
        let mut out = Vec::new();
        for sec in &doc.sections {
            for page in &sec.pages {
                for block in &page.blocks {
                    if let BlockKind::Slide(sb) = &block.kind {
                        out.extend(sb.slides.clone());
                    }
                }
            }
        }
        out
    }

    fn slide_text(slide: &Slide) -> String {
        let mut s = String::new();
        for ph in &slide.placeholders {
            collect_block_text(&ph.block, &mut s);
        }
        s
    }

    fn body_paragraph_count(slide: &Slide) -> usize {
        let mut n = 0;
        for ph in &slide.placeholders {
            if let BlockKind::TextBox(tb) = &ph.block.kind {
                for b in &tb.blocks {
                    if matches!(b.kind, BlockKind::Paragraph(_)) {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    fn blocks_text(blocks: &[Block]) -> String {
        let mut s = String::new();
        for b in blocks {
            collect_block_text(b, &mut s);
        }
        s
    }

    fn collect_block_text(block: &Block, out: &mut String) {
        match &block.kind {
            BlockKind::Paragraph(p) => {
                for r in &p.runs {
                    if let Inline::Run(run) = r {
                        out.push_str(&run.text);
                        out.push(' ');
                    }
                }
            }
            BlockKind::TextBox(tb) => {
                for b in &tb.blocks {
                    collect_block_text(b, out);
                }
            }
            _ => {}
        }
    }
}
