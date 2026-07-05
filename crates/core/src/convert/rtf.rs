//! Stateful RTF → styled HTML parser (feeds the [`crate::html`] engine).
//!
//! Unlike [`super::reverse::rtf_to_paragraphs`] — a plain-text extractor where
//! every style control word is dropped — this parser tracks a **character-format
//! stack across RTF groups** (`{` pushes the current state, `}` pops it), reads
//! the `{\fonttbl}` / `{\colortbl}` resource tables, and recovers paragraph
//! alignment and indents. The result is emitted as styled HTML (`<p style=…>`
//! with `<span style=…>` runs and real `<table>`s) so the in-house HTML/CSS
//! layout engine renders bold/italic/underline/strike, colours, font sizes,
//! families, alignment and tables — none of which the text-only path preserves.
//!
//! Mirrors the inverse exporter [`super::reverse::rtf_from_model`], which already
//! writes `\b \i \ul \strike \fs \cf`; this closes the import side of that loop.
//!
//! Zero-dependency: hand-written byte scanner, no regex / no external crates.

/// Recovered character formatting. Toggled by `\b \i \ul \strike \super \sub`
/// (with `\b0`-style "off" forms) and the indexed `\cf \fs \f` controls; cloned
/// onto a stack at every `{` and restored at the matching `}`.
#[derive(Debug, Clone, Default, PartialEq)]
struct CharState {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    superscript: bool,
    subscript: bool,
    /// `\\cf` colour-table index (0 = auto / inherit).
    color_idx: usize,
    /// `\\cb` / `\\highlight` colour-table index for the run background
    /// (0 = auto / inherit → no highlight).
    highlight_idx: usize,
    /// `\\f` font-table index.
    font_idx: usize,
    /// `\fs` value in half-points (0 = unset → engine default).
    half_points: u32,
}

/// Recovered paragraph formatting, reset by `\pard` and `\par`.
#[derive(Debug, Clone, Default, PartialEq)]
struct ParaState {
    align: Align,
    /// `\li` left indent (twips).
    indent_left: i32,
    /// `\ri` right indent (twips).
    indent_right: i32,
    /// `\fi` first-line indent (twips, may be negative for hanging indents).
    first_line: i32,
    /// `\sb` space before (twips).
    space_before: i32,
    /// `\sa` space after (twips).
    space_after: i32,
    /// `\sl` line spacing (twips when `\slmult` is 0/absent, 240ths of a line when `\slmult` is 1).
    line_spacing: i32,
    /// `\slmult` — 1 = multiple, 0/absent = exact (twips).
    line_spacing_mult: bool,
    /// List membership of the paragraph being built: `Some(true)` = numbered
    /// item, `Some(false)` = bulleted item, `None` = plain paragraph. Set by
    /// `\pnlvlbody` / `\pnlvlblt` (the `{\*\pn …}` numbering definition) or
    /// classified from the pre-rendered `{\listtext …}` / `{\pntext …}` marker;
    /// reset by `\pard` like every other paragraph property.
    list_ordered: Option<bool>,
    /// `\ilvl` list nesting level (0-based, clamped to 0..=8).
    ilvl: u8,
    /// `\rtlpar` — right-to-left paragraph flow (`\ltrpar` clears it); reset by
    /// `\pard` like every other paragraph property.
    rtl: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Align {
    #[default]
    Left,
    Center,
    Right,
    Justify,
}

/// One styled span of text within a paragraph.
#[derive(Debug, Clone)]
struct Run {
    text: String,
    style: CharState,
    /// Hyperlink target if this run is the (visible) result of a
    /// `{\field{\*\fldinst{HYPERLINK "url"}}{\fldrslt …}}`. `None` for ordinary
    /// runs. Consecutive runs sharing the same `Some(url)` form one link.
    link: Option<String>,
    /// `Some(id)` marks a zero-width comment anchor (from `\chatn` /
    /// `{\*\annotation …}`): the run carries no text and lowers to
    /// [`Inline::CommentRef`] joined by `id` to a recovered comment body.
    comment_ref: Option<String>,
    /// `Some(id)` tags a run synthesized by a `{\footnote …}` group (the
    /// superscript ordinal marker and the inlined body copy). The HTML path
    /// renders these runs verbatim (unchanged output); the **model** path
    /// lowers the first run of an id to [`Inline::FootnoteRef`] and drops the
    /// rest — the note body lives in [`Document::footnotes`] instead.
    footnote_ref: Option<String>,
}

/// A recovered paragraph: alignment + indents + spacing + its styled runs.
#[derive(Debug, Clone, Default)]
struct Para {
    align: Align,
    indent_left: i32,
    indent_right: i32,
    first_line: i32,
    space_before: i32,
    space_after: i32,
    line_spacing: i32,
    line_spacing_mult: bool,
    /// `Some((ordered, level))` when the paragraph is a list item (recovered
    /// from `{\*\pn …}` / `{\listtext …}`); consecutive tagged paragraphs are
    /// grouped into one list on serialization.
    list: Option<(bool, u8)>,
    /// `\rtlpar` — right-to-left paragraph flow.
    rtl: bool,
    runs: Vec<Run>,
}

/// A top-level recovered block: a flowing paragraph, a table, an image, or a hard page break.
#[derive(Debug, Clone)]
enum RtfBlock {
    Para(Para),
    /// Rows of cells; each cell is its own list of paragraphs.
    Table(Vec<Vec<Vec<Para>>>),
    /// `\page`: hard page boundary. In the model path this starts a new page;
    /// in the HTML/PDF path it becomes a CSS page break.
    PageBreak,
    /// An embedded picture extracted from a `{\pict …}` group: the decoded raw
    /// image bytes (PNG or JPEG), the IANA subtype ("png" / "jpeg"), and the
    /// display size in CSS points (from `\picwgoal`/`\pichgoal`, else `\picw`/
    /// `\pich`). Web-native blips (`\pngblip`/`\jpegblip`) are kept verbatim;
    /// `\wmetafile`/`\emfblip` (WMF/EMF) and `\dibitmap`/`\wbitmap` (DIB/BMP)
    /// are decoded to RGBA by the in-house metafile/DIB decoders and re-encoded
    /// to PNG. Serialized as an `<img src="data:image/…;base64,…">` so the HTML
    /// engine's existing image-embed path renders it.
    Image(RtfPicture),
}

/// A recovered `\pict` image ready to emit as an HTML `data:` URI.
#[derive(Debug, Clone)]
struct RtfPicture {
    /// Decoded raw image bytes (PNG or JPEG file content).
    data: Vec<u8>,
    /// IANA image subtype for the data URI: "png" or "jpeg".
    subtype: &'static str,
    /// Display width in CSS points.
    width_pt: f64,
    /// Display height in CSS points.
    height_pt: f64,
}

/// How a `\pict` payload must be decoded, selected by its blip control word.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PictKind {
    /// No recognized blip control word yet → skip the picture.
    Unknown,
    /// `\pngblip` — PNG bytes, embedded verbatim.
    Png,
    /// `\jpegblip` — JPEG bytes, embedded verbatim.
    Jpeg,
    /// `\wmetafile` — Windows Metafile, rasterized then re-encoded to PNG.
    Wmf,
    /// `\emfblip` — Enhanced Metafile, rasterized then re-encoded to PNG.
    Emf,
    /// `\dibitmap` / `\wbitmap` — packed DIB, decoded then re-encoded to PNG.
    Dib,
}

/// A review comment recovered from `{\*\atnid}{\*\atnauthor}…{\*\annotation …}`.
/// `id` is the parser's own 1-based counter (RTF's `\atnref` is optional and
/// writer-dependent, so a deterministic counter keeps anchor/body pairing
/// stable); it matches the [`Run::comment_ref`] anchor emitted at `\chatn`.
/// `date` is left empty: RTF's `\atndate` packs a binary DTTM integer, not the
/// verbatim ISO text the model convention stores.
#[derive(Debug, Clone, Default)]
struct RtfComment {
    id: String,
    author: String,
    text: String,
}

/// One `{\stylesheet}` entry's *base* formatting, applied inline when the body
/// references it via `\sN` (paragraph styles) or `\csN` (character styles).
///
/// Deliberately **inline resolution**: the properties are merged into the live
/// character/paragraph state at the `\sN` reference — later explicit toggles
/// override them, exactly as RTF layering works — instead of building a named
/// [`StyleTable`](crate::model::StyleTable). That keeps the recovery local to
/// this parser (no cross-module model plumbing) while restoring what the styles
/// actually *look like*; a named-style table can layer on later without
/// changing this read.
#[derive(Debug, Clone, Copy, Default)]
struct StyleDef {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    /// `\fs` half-points (0 = unset).
    half_points: u32,
    /// `\cf` colour-table index (0 = unset/auto).
    color_idx: usize,
    /// `\f` font-table index.
    font_idx: Option<usize>,
    /// Paragraph alignment (`\ql\qc\qr\qj`), meaningful for `\s` styles only.
    align: Option<Align>,
}

/// A font-table entry: family name + a generic CSS family bucket.
#[derive(Debug, Clone, Default)]
struct FontEntry {
    name: String,
    /// One of "serif" / "sans-serif" / "monospace" / "cursive" — from `\froman`
    /// `\fswiss` `\fmodern` `\fscript`; empty if unspecified.
    generic: &'static str,
    /// `\fcharset2` (or a Symbol/Wingdings family name): the font's `\'xx`
    /// bytes are **symbol codes**, not CP1252 — Word writes its list bullets as
    /// `{\pntext\f1\'b7}` where `0xB7` in Symbol is `•`, not a middle dot.
    symbol: bool,
}

// ────────────────────────── CP1252 high-byte table ─────────────────────────

/// Windows-1252 mapping for bytes `0x80..=0x9F` (the range where CP1252 differs
/// from Latin-1). `\'80` is the Euro sign, `\'93`/`\'94` curly quotes, etc.
/// `0` marks the five undefined CP1252 code points (kept as U+FFFD on use).
const CP1252_HIGH: [u16; 32] = [
    0x20AC, 0x0000, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, // 80–87
    0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x0000, 0x017D, 0x0000, // 88–8F
    0x0000, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, // 90–97
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x0000, 0x017E, 0x0178, // 98–9F
];

/// Decode a single RTF `\'xx` byte to a Unicode `char` using CP1252 semantics.
fn cp1252_byte(b: u8) -> char {
    if (0x80..=0x9F).contains(&b) {
        let cp = CP1252_HIGH[(b - 0x80) as usize];
        if cp == 0 {
            '\u{FFFD}'
        } else {
            char::from_u32(cp as u32).unwrap_or('\u{FFFD}')
        }
    } else {
        // 0x00–0x7F identical to ASCII; 0xA0–0xFF identical to Latin-1.
        b as char
    }
}

// ─────────────────────────────── the parser ────────────────────────────────

/// Per-group snapshot pushed at `{` and restored at `}`.
#[derive(Clone)]
struct Group {
    chr: CharState,
    /// Destination this group is skipped as (text discarded), e.g. `\fonttbl`.
    skip: bool,
    /// Active hyperlink target, restored when the `\fldrslt` group closes.
    link: Option<String>,
}

struct Parser<'a> {
    bytes: &'a [u8],
    src: &'a str,
    i: usize,
    /// Open-group stack of saved states.
    stack: Vec<Group>,
    /// Live character state.
    chr: CharState,
    /// Live paragraph state.
    par: ParaState,
    /// Whether the current group's text is being discarded.
    skip: bool,
    /// `\ucN`: count of fallback bytes to skip after each `\uN`.
    uc: i64,
    /// `\binN`: count of raw payload bytes the scanner must skip *verbatim* after
    /// the control word (they may embed `{`/`}`/`\`). Set by [`Self::apply_control`],
    /// consumed by [`Self::control`] right after it advances past the word.
    bin_skip: usize,

    fonts: Vec<FontEntry>,
    colors: Vec<[u8; 3]>,

    /// Finished blocks.
    blocks: Vec<RtfBlock>,
    /// The paragraph currently being accumulated.
    cur: Para,
    /// Whether `cur` has had its para-format captured from `par` yet.
    cur_started: bool,

    // Table assembly.
    in_row: bool,
    row_cells: Vec<Vec<Para>>,
    cell_paras: Vec<Para>,

    // Hyperlink (`\field`) assembly.
    /// While inside a `\fldinst` instruction, decoded text is collected here so
    /// we can extract the `HYPERLINK "url"` target (instead of dropping it like
    /// an ordinary ignorable destination). `Some` iff capture is active.
    fldinst_capture: Option<String>,
    /// Open-group depth ([`Self::stack`] length) at which the active `\fldinst`
    /// capture began; capture ends — and the URL is mined — once a `}` brings
    /// the stack back below it. Survives inner `{…}` groups in the instruction.
    fldinst_depth: Option<usize>,
    /// The URL of the hyperlink whose `\fldrslt` result is currently being
    /// emitted, tagged onto every run produced inside it.
    cur_link: Option<String>,
    /// URL mined from a just-closed `\fldinst`, awaiting its sibling `\fldrslt`.
    pending_link: Option<String>,

    /// Absolute scanner position to jump to after the current control word —
    /// set by destination readers ([`Self::read_header_footer`], …) to the
    /// group's closing `}` so the payload is *never* re-scanned (unlike the
    /// legacy `skip` re-scan, this also keeps the destination's `\pard`/`\par`
    /// from perturbing the live paragraph state). Consumed by [`Self::control`].
    jump_to: Option<usize>,
    /// Nesting depth of [`Self::parse_fragment`] sub-parsers; bounds recursion
    /// on pathological inputs (`{\header {\header …}}`).
    frag_depth: u8,

    /// Best recovered running header / footer: `(preference_rank, blocks)`;
    /// lower rank wins (default `\header` 0 > first `\headerf` 1 > left
    /// `\headerl` 2 > right `\headerr` 3 — the model carries one header, so
    /// the page-variant forms fold onto it by that preference).
    header: Option<(u8, Vec<RtfBlock>)>,
    footer: Option<(u8, Vec<RtfBlock>)>,

    /// Running `\footnote` counter (1-based), mirroring RTF's auto-numbered
    /// `\chftn` marker; used as the inlined note's superscript ordinal and as
    /// the typed note id.
    footnote_n: u32,
    /// Foot-note bodies recovered from `{\footnote …}` groups, in document
    /// order (`(id, body blocks)`); anchored in the runs via
    /// [`Run::footnote_ref`] and lowered to [`Document::footnotes`] by the
    /// model path (the HTML path keeps the historical inlined rendering).
    footnotes: Vec<(String, Vec<RtfBlock>)>,

    /// Review comments recovered from `{\*\annotation …}` groups, in document
    /// order; anchored in the runs via [`Run::comment_ref`].
    comments: Vec<RtfComment>,
    /// `{\*\atnid …}` initials seen since the last annotation (author fallback).
    pending_atnid: Option<String>,
    /// `{\*\atnauthor …}` full name seen since the last annotation.
    pending_atnauthor: Option<String>,

    /// `{\stylesheet}` paragraph styles by `\s` index.
    pstyles: BTreeMap<usize, StyleDef>,
    /// `{\stylesheet}` character styles by `\*\cs` index.
    cstyles: BTreeMap<usize, StyleDef>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Parser {
            bytes: src.as_bytes(),
            src,
            i: 0,
            stack: Vec::new(),
            chr: CharState::default(),
            par: ParaState::default(),
            skip: false,
            uc: 1,
            bin_skip: 0,
            fonts: Vec::new(),
            colors: Vec::new(),
            blocks: Vec::new(),
            cur: Para::default(),
            cur_started: false,
            in_row: false,
            row_cells: Vec::new(),
            cell_paras: Vec::new(),
            fldinst_capture: None,
            fldinst_depth: None,
            cur_link: None,
            pending_link: None,
            jump_to: None,
            frag_depth: 0,
            header: None,
            footer: None,
            footnote_n: 0,
            footnotes: Vec::new(),
            comments: Vec::new(),
            pending_atnid: None,
            pending_atnauthor: None,
            pstyles: BTreeMap::new(),
            cstyles: BTreeMap::new(),
        }
    }

    /// Capture the live paragraph format onto the paragraph being built, once.
    fn start_para(&mut self) {
        if self.cur_started {
            return;
        }
        self.cur.align = self.par.align;
        self.cur.indent_left = self.par.indent_left;
        self.cur.indent_right = self.par.indent_right;
        self.cur.first_line = self.par.first_line;
        self.cur.space_before = self.par.space_before;
        self.cur.space_after = self.par.space_after;
        self.cur.line_spacing = self.par.line_spacing;
        self.cur.line_spacing_mult = self.par.line_spacing_mult;
        self.cur.list = self.par.list_ordered.map(|o| (o, self.par.ilvl));
        self.cur.rtl = self.par.rtl;
        self.cur_started = true;
    }

    /// Append a decoded character to the active run (creating/extending one with
    /// the current style), unless the current group is a skipped destination.
    fn push_char(&mut self, ch: char) {
        // Inside a `\fldinst` instruction, collect text to mine the hyperlink
        // target rather than emitting it as visible body content.
        if let Some(buf) = self.fldinst_capture.as_mut() {
            buf.push(ch);
            return;
        }
        if self.skip {
            return;
        }
        self.start_para();
        match self.cur.runs.last_mut() {
            // Never grow a comment anchor (zero-width) or a footnote-tagged run
            // (the model path drops tagged bodies — merged text would vanish).
            Some(r)
                if r.style == self.chr
                    && r.link == self.cur_link
                    && r.comment_ref.is_none()
                    && r.footnote_ref.is_none() =>
            {
                r.text.push(ch)
            }
            _ => self.cur.runs.push(Run {
                text: ch.to_string(),
                style: self.chr.clone(),
                link: self.cur_link.clone(),
                comment_ref: None,
                footnote_ref: None,
            }),
        }
    }

    /// Append a pre-styled run (no merging) to the paragraph being built,
    /// honouring the same skip / field-capture bookkeeping as [`push_char`].
    /// Used for synthesized content (inlined footnote bodies, comment anchors).
    fn push_run(
        &mut self,
        text: String,
        style: CharState,
        comment_ref: Option<String>,
        footnote_ref: Option<String>,
    ) {
        if self.skip || self.fldinst_capture.is_some() || (text.is_empty() && comment_ref.is_none())
        {
            return;
        }
        self.start_para();
        self.cur.runs.push(Run {
            text,
            style,
            link: self.cur_link.clone(),
            comment_ref,
            footnote_ref,
        });
    }

    /// Whether the *enclosing* group was already a skipped destination when the
    /// current group opened — i.e. skipping inherited from outside, as opposed
    /// to the `\*` marker of the current group itself. Destination readers that
    /// know how to import a `\*`-prefixed group (annotations, …) consult this:
    /// inherited skip ⇒ genuinely discarded context, do not import.
    fn inherited_skip(&self) -> bool {
        self.stack.last().is_some_and(|g| g.skip)
    }

    /// End the current paragraph (on `\par` / `\cell` / `\row` / EOF) and route
    /// it to the open table cell or the top-level block list.
    fn flush_para(&mut self) {
        if !self.cur_started && self.cur.runs.is_empty() {
            // Still emit blank paragraphs between content as spacing, but only
            // when we already have output (avoid leading blank noise).
            if self.in_row {
                return;
            }
            if matches!(self.blocks.last(), Some(RtfBlock::Para(_))) {
                self.blocks.push(RtfBlock::Para(Para {
                    align: self.par.align,
                    indent_left: self.par.indent_left,
                    indent_right: self.par.indent_right,
                    first_line: self.par.first_line,
                    space_before: self.par.space_before,
                    space_after: self.par.space_after,
                    line_spacing: self.par.line_spacing,
                    line_spacing_mult: self.par.line_spacing_mult,
                    list: None,
                    ..Para::default()
                }));
            }
            return;
        }
        let para = std::mem::take(&mut self.cur);
        self.cur_started = false;
        if self.in_row {
            self.cell_paras.push(para);
        } else {
            self.blocks.push(RtfBlock::Para(para));
        }
    }

    /// `\cell`: finish the current paragraph and close the table cell.
    fn end_cell(&mut self) {
        // Push the in-progress paragraph into the cell.
        if self.cur_started || !self.cur.runs.is_empty() {
            let para = std::mem::take(&mut self.cur);
            self.cur_started = false;
            self.cell_paras.push(para);
        }
        let cell = std::mem::take(&mut self.cell_paras);
        self.row_cells.push(cell);
    }

    /// `\row`: close the table row, merging into the trailing table block (so
    /// consecutive `\row`s build one `<table>`).
    fn end_row(&mut self) {
        // Any text after the last \cell but before \row is dropped per RTF.
        let cells = std::mem::take(&mut self.row_cells);
        self.cell_paras.clear();
        self.in_row = false;
        if cells.is_empty() {
            return;
        }
        if let Some(RtfBlock::Table(rows)) = self.blocks.last_mut() {
            rows.push(cells);
        } else {
            self.blocks.push(RtfBlock::Table(vec![cells]));
        }
    }

    /// Drive the byte scanner, accumulating blocks and populating the resource
    /// tables. Consumes the bytes but leaves `self` alive so the caller can read
    /// `fonts` / `colors` back during HTML serialization. Returns the blocks.
    fn drive(&mut self) -> Vec<RtfBlock> {
        while self.i < self.bytes.len() {
            match self.bytes[self.i] {
                b'{' => {
                    self.stack.push(Group {
                        chr: self.chr.clone(),
                        skip: self.skip,
                        link: self.cur_link.clone(),
                    });
                    self.i += 1;
                }
                b'}' => {
                    if let Some(g) = self.stack.pop() {
                        // Leaving the group that opened a `\fldinst` capture
                        // (back below its start depth): mine the collected text
                        // for the `HYPERLINK "url"` target, then end capture.
                        if let Some(depth) = self.fldinst_depth {
                            if self.stack.len() < depth {
                                if let Some(url) =
                                    extract_hyperlink(self.fldinst_capture.as_deref())
                                {
                                    self.pending_link = Some(url);
                                }
                                self.fldinst_capture = None;
                                self.fldinst_depth = None;
                            }
                        }
                        self.chr = g.chr;
                        self.skip = g.skip;
                        self.cur_link = g.link;
                    }
                    self.i += 1;
                }
                b'\\' => self.control(),
                b'\r' | b'\n' => self.i += 1,
                c => {
                    self.push_char(cp1252_byte(c));
                    self.i += 1;
                }
            }
        }
        // Final paragraph / open row.
        self.flush_para();
        if self.in_row && !self.row_cells.is_empty() {
            self.end_row();
        }
        std::mem::take(&mut self.blocks)
    }

    /// Handle a backslash: either an escaped literal / symbol, or a control word.
    fn control(&mut self) {
        let b = self.bytes;
        // `\<non-alnum>`: escaped char, `\'xx`, or a control symbol.
        if self.i + 1 < b.len() && !b[self.i + 1].is_ascii_alphanumeric() {
            match b[self.i + 1] {
                b'\'' if self.i + 3 < b.len() => {
                    let hex = &self.src[self.i + 2..self.i + 4];
                    if let Ok(byte) = u8::from_str_radix(hex, 16) {
                        let ch = self.decode_byte(byte);
                        self.push_char(ch);
                    }
                    self.i += 4;
                }
                b'\\' | b'{' | b'}' => {
                    self.push_char(b[self.i + 1] as char);
                    self.i += 2;
                }
                b'~' => {
                    self.push_char('\u{00A0}'); // non-breaking space
                    self.i += 2;
                }
                b'-' => self.i += 2, // optional hyphen → drop
                b'_' => {
                    self.push_char('\u{2011}'); // non-breaking hyphen
                    self.i += 2;
                }
                b'*' => {
                    // `\*` → next destination is an ignorable one: skip group.
                    self.skip = true;
                    self.i += 2;
                }
                b'\r' | b'\n' => {
                    // `\` line continuation == `\par`.
                    self.flush_para();
                    self.i += 2;
                }
                _ => self.i += 2,
            }
            return;
        }

        // Alphabetic control word + optional signed numeric parameter.
        let start = self.i + 1;
        let mut j = start;
        while j < b.len() && b[j].is_ascii_alphabetic() {
            j += 1;
        }
        let word = &self.src[start..j];
        let mut k = j;
        let mut neg = false;
        if k < b.len() && b[k] == b'-' {
            neg = true;
            k += 1;
        }
        let num_start = k;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        let param: Option<i64> = self.src[num_start..k]
            .parse()
            .ok()
            .map(|n: i64| if neg { -n } else { n });

        let mut fallback_skip = 0i64;
        self.apply_control(word, param, &mut fallback_skip);

        // A single trailing space delimits the control word — consume it.
        if k < b.len() && b[k] == b' ' {
            k += 1;
        }
        self.i = k;

        // A destination reader asked to jump straight to the group's closing
        // `}` (position is absolute): the payload is consumed exactly once by
        // the reader, never re-scanned, and the `}` still pops the group stack.
        if let Some(j) = self.jump_to.take() {
            self.i = j.max(self.i).min(b.len());
        }

        // `\binN`: jump the scanner past the N raw payload bytes verbatim so an
        // arbitrary `{`/`}`/`\` inside them is never mis-read as RTF structure.
        if self.bin_skip > 0 {
            self.i = self.i.saturating_add(self.bin_skip).min(b.len());
            self.bin_skip = 0;
        }

        // Skip `\ucN` fallback bytes that follow a `\uN`.
        for _ in 0..fallback_skip {
            if self.i >= b.len() {
                break;
            }
            // Skip one source char; handle a stray `\'xx` / `\x` escape as one.
            if b[self.i] == b'\\' && self.i + 1 < b.len() {
                if b[self.i + 1] == b'\'' {
                    self.i += 4.min(b.len() - self.i);
                } else {
                    self.i += 2;
                }
            } else {
                let mut adv = 1;
                while self.i + adv < b.len() && (b[self.i + adv] & 0xC0) == 0x80 {
                    adv += 1;
                }
                self.i += adv;
            }
        }
    }

    fn apply_control(&mut self, word: &str, param: Option<i64>, fallback_skip: &mut i64) {
        let on = param != Some(0); // toggles: absent or non-zero ⇒ on, `0` ⇒ off
        match word {
            // ── character toggles ──
            "b" => self.chr.bold = on,
            "i" => self.chr.italic = on,
            "ul" => self.chr.underline = on,
            "ulnone" => self.chr.underline = false,
            "strike" => self.chr.strike = on,
            "super" => {
                self.chr.superscript = on;
                if on {
                    self.chr.subscript = false;
                }
            }
            "sub" => {
                self.chr.subscript = on;
                if on {
                    self.chr.superscript = false;
                }
            }
            "nosupersub" => {
                self.chr.superscript = false;
                self.chr.subscript = false;
            }
            "plain" => {
                let (cf, f) = (self.chr.color_idx, self.chr.font_idx);
                self.chr = CharState {
                    color_idx: cf,
                    font_idx: f,
                    ..CharState::default()
                };
            }
            "cf" => self.chr.color_idx = param.unwrap_or(0).max(0) as usize,
            "cb" | "highlight" => self.chr.highlight_idx = param.unwrap_or(0).max(0) as usize,
            "fs" => self.chr.half_points = param.unwrap_or(0).max(0) as u32,
            "f" => {
                // `\f` inside `\fonttbl` selects the entry being defined; in body
                // it selects the run font. We only need the body meaning here.
                if !self.skip {
                    self.chr.font_idx = param.unwrap_or(0).max(0) as usize;
                }
            }

            // ── paragraph format ──
            "pard" => {
                self.par = ParaState::default();
            }
            "ql" => self.par.align = Align::Left,
            "qc" => self.par.align = Align::Center,
            "qr" => self.par.align = Align::Right,
            "qj" => self.par.align = Align::Justify,
            // `\rtlpar` / `\ltrpar`: paragraph writing direction.
            "rtlpar" => self.par.rtl = true,
            "ltrpar" => self.par.rtl = false,
            "li" => self.par.indent_left = param.unwrap_or(0) as i32,
            "ri" => self.par.indent_right = param.unwrap_or(0) as i32,
            "fi" => self.par.first_line = param.unwrap_or(0) as i32,
            "sb" => self.par.space_before = param.unwrap_or(0) as i32,
            "sa" => self.par.space_after = param.unwrap_or(0) as i32,
            "sl" => self.par.line_spacing = param.unwrap_or(0) as i32,
            "slmult" => self.par.line_spacing_mult = param.unwrap_or(0) != 0,

            // ── breaks ──
            "par" | "sect" => self.flush_para(),
            "page" => {
                self.flush_para();
                self.blocks.push(RtfBlock::PageBreak);
            }
            "line" => self.push_char('\n'),
            "tab" => self.push_char('\t'),

            // ── tables ──
            "trowd" => {
                // Begin a table row definition.
                self.in_row = true;
                self.row_cells.clear();
                self.cell_paras.clear();
                // A row often follows body text; flush any pending paragraph.
                if self.cur_started || !self.cur.runs.is_empty() {
                    let para = std::mem::take(&mut self.cur);
                    self.cur_started = false;
                    self.blocks.push(RtfBlock::Para(para));
                }
            }
            "cell" => self.end_cell(),
            "row" => self.end_row(),
            "nestcell" => self.end_cell(),
            "nestrow" => self.end_row(),

            // ── Unicode ──
            "uc" => {
                if let Some(n) = param {
                    self.uc = n.max(0);
                }
            }
            "u" => {
                if let Some(n) = param {
                    let code = if n < 0 {
                        (n + 0x10000) as u32
                    } else {
                        n as u32
                    };
                    if let Some(ch) = char::from_u32(code) {
                        self.push_char(ch);
                    }
                    *fallback_skip = self.uc;
                }
            }

            // ── resource tables / ignorable destinations: skip their text ──
            "fonttbl" => {
                self.skip = true;
                self.read_fonttbl();
            }
            "colortbl" => {
                self.skip = true;
                self.read_colortbl();
            }
            "pict" => {
                // Suppress the hex/binary picture data from leaking as body text
                // (the main scanner re-reads these bytes with `skip` on), and
                // extract the image — appended as an `RtfBlock::Image`.
                self.skip = true;
                self.read_pict();
            }
            "bin" => {
                // `\binN`: the next N bytes are a raw binary blob (picture data,
                // object data, …), NOT control text. Signal [`Self::control`] to
                // jump the scanner past them verbatim — their bytes may otherwise
                // be mis-read as `{`/`}`/`\` structure. `read_pict` has already
                // captured them for any picture; here we only skip them.
                self.bin_skip = param.unwrap_or(0).max(0) as usize;
            }
            // ── hyperlinks (`\field`) ──
            "field" => {} // container; its \fldinst / \fldrslt sub-groups carry the data
            "fldinst" => {
                // Capture the instruction text (e.g. `HYPERLINK "https://…"`)
                // instead of dropping it, so the target can be recovered. The
                // capture buffer suppresses it from visible body output, and
                // accumulates across any inner `{…}` groups until the group that
                // holds `\fldinst` closes.
                self.fldinst_capture.get_or_insert_with(String::new);
                self.fldinst_depth.get_or_insert(self.stack.len());
            }
            "fldrslt" => {
                // The visible result of the field: tag its runs with the URL
                // mined from the preceding `\fldinst`.
                if let Some(url) = self.pending_link.take() {
                    self.cur_link = Some(url);
                }
            }

            // ── running headers / footers (destinations) ──
            "header" | "headerl" | "headerr" | "headerf" => self.read_header_footer(word, true),
            "footer" | "footerl" | "footerr" | "footerf" => self.read_header_footer(word, false),

            // ── footnotes: inline the note body at the reference point ──
            "footnote" => self.read_footnote(),

            // ── review comments (annotations) ──
            "atnid" => self.read_atn_meta(true),
            "atnauthor" => self.read_atn_meta(false),
            "annotation" => self.read_annotation(),

            // ── stylesheet: read the definitions, applied on `\sN` / `\csN` ──
            "stylesheet" => {
                self.skip = true;
                self.read_stylesheet();
            }
            "s" => {
                // Body reference to a paragraph style (the stylesheet's own
                // `\s` never reaches here: its group is jumped, and any copy in
                // a skipped destination is fenced by the `skip` guard).
                if !self.skip {
                    if let Some(def) = param
                        .and_then(|n| usize::try_from(n).ok())
                        .and_then(|idx| self.pstyles.get(&idx).copied())
                    {
                        self.apply_style_def(&def, true);
                    }
                }
            }
            "cs" => {
                if !self.skip {
                    if let Some(def) = param
                        .and_then(|n| usize::try_from(n).ok())
                        .and_then(|idx| self.cstyles.get(&idx).copied())
                    {
                        self.apply_style_def(&def, false);
                    }
                }
            }

            // ── lists: strip the pre-rendered marker, keep the semantics ──
            // `{\listtext …}` / `{\pntext …}` carry the marker as literal text;
            // emitting it would duplicate the list marker the model renders, so
            // it is consumed — after classifying bullet vs number when the
            // `{\*\pn …}` definition hasn't already said.
            "listtext" | "pntext" => self.read_listtext(),
            // `{\*\pn …}` numbering definition: level type. The group is `\*`
            // (skip) so only these control words matter; guard on *inherited*
            // skip so a `\pn` inside a genuinely discarded destination (e.g.
            // the raw `\listtable`) cannot tag body paragraphs.
            "pnlvlblt" => {
                if !self.inherited_skip() {
                    self.par.list_ordered = Some(false);
                }
            }
            "pnlvlbody" => {
                if !self.inherited_skip() {
                    self.par.list_ordered = Some(true);
                }
            }
            "ilvl" => {
                if !self.skip {
                    self.par.ilvl = param.unwrap_or(0).clamp(0, 8) as u8;
                }
            }

            // Ignorable metadata / raw resource destinations: drop their text.
            // (`\ls` list-table references stay unmapped: without the skipped
            // `\listtable` there is no marker info; the `\listtext` fallback
            // above recovers those lists instead.)
            "info" | "object" | "xmlns" | "themedata" | "colorschememapping" | "datastore"
            | "latentstyles" | "listtable" | "listoverridetable" | "generator" | "revtbl"
            | "rsidtbl" | "atndate" | "atnref" | "atnicn" | "atnparent" | "ftnsep" | "ftnsepc"
            | "ftncn" | "aftnsep" | "aftnsepc" | "aftncn" => {
                self.skip = true;
            }

            _ => {}
        }
    }

    /// Merge a stylesheet definition into the live state at its `\sN` / `\csN`
    /// reference. Only properties the definition carries are applied, so
    /// explicit toggles that follow (or preceded, for unset ones) still win —
    /// the inline-resolution contract documented on [`StyleDef`].
    fn apply_style_def(&mut self, d: &StyleDef, para: bool) {
        if d.bold {
            self.chr.bold = true;
        }
        if d.italic {
            self.chr.italic = true;
        }
        if d.underline {
            self.chr.underline = true;
        }
        if d.strike {
            self.chr.strike = true;
        }
        if d.half_points > 0 {
            self.chr.half_points = d.half_points;
        }
        if d.color_idx > 0 {
            self.chr.color_idx = d.color_idx;
        }
        if let Some(f) = d.font_idx {
            self.chr.font_idx = f;
        }
        if para {
            if let Some(a) = d.align {
                self.par.align = a;
            }
        }
    }

    /// Parse `{\fonttbl …}` into [`Self::fonts`]. Called right after the `\fonttbl`
    /// control word; reads ahead to the group close. The main loop still scans the
    /// same bytes (text suppressed via `skip`), so we only record metadata here.
    fn read_fonttbl(&mut self) {
        let b = self.bytes;
        let mut p = self.i; // points just after "fonttbl"
        let mut depth = 0i32; // relative depth within the fonttbl group
        let mut cur = FontEntry::default();
        let mut cur_idx: Option<usize> = None;
        let mut name = String::new();

        while p < b.len() {
            match b[p] {
                b'{' => {
                    depth += 1;
                    p += 1;
                }
                b'}' => {
                    if depth == 0 {
                        // Close of the fonttbl group itself.
                        break;
                    }
                    // Close of one font sub-group: commit it.
                    if let Some(idx) = cur_idx.take() {
                        cur.name = name.trim().trim_end_matches(';').trim().to_string();
                        self.set_font(idx, cur.clone());
                    }
                    cur = FontEntry::default();
                    name.clear();
                    depth -= 1;
                    p += 1;
                }
                b'\\' => {
                    let s = p + 1;
                    let mut e = s;
                    while e < b.len() && b[e].is_ascii_alphabetic() {
                        e += 1;
                    }
                    let w = &self.src[s..e];
                    let mut q = e;
                    while q < b.len() && (b[q].is_ascii_digit() || b[q] == b'-') {
                        q += 1;
                    }
                    let np: Option<usize> = self.src[e..q].parse().ok();
                    match w {
                        "f" => cur_idx = np,
                        // Charset 2 marks a symbol font (Symbol, Wingdings, …).
                        "fcharset" => cur.symbol = np == Some(2),
                        "froman" => cur.generic = "serif",
                        "fswiss" => cur.generic = "sans-serif",
                        "fmodern" => cur.generic = "monospace",
                        "fscript" => cur.generic = "cursive",
                        "fdecor" | "ftech" | "fbidi" | "fnil" => {}
                        _ => {}
                    }
                    if q < b.len() && b[q] == b' ' {
                        q += 1;
                    }
                    p = q;
                }
                c => {
                    // Font name text (between the control words and the `;`).
                    if depth >= 1 {
                        name.push(c as char);
                    }
                    p += 1;
                }
            }
        }
        // Commit a trailing entry that closed at the group boundary.
        if let Some(idx) = cur_idx.take() {
            cur.name = name.trim().trim_end_matches(';').trim().to_string();
            self.set_font(idx, cur);
        }
    }

    fn set_font(&mut self, idx: usize, mut entry: FontEntry) {
        // Belt-and-braces: a Symbol/Wingdings family is a symbol font even when
        // the writer omitted `\fcharset2`.
        if !entry.symbol {
            let lc = entry.name.to_ascii_lowercase();
            entry.symbol = lc.contains("symbol") || lc.contains("wingding");
        }
        if self.fonts.len() <= idx {
            self.fonts.resize(idx + 1, FontEntry::default());
        }
        self.fonts[idx] = entry;
    }

    /// Decode a `\'xx` byte through the **active font**: a symbol-charset font
    /// (Symbol / Wingdings, `\fcharset2`) resolves through its own encoding —
    /// Word writes list bullets as `{\pntext\f1\'b7\tab}` where `0xB7` in
    /// Symbol is the bullet `•`, not the CP1252 middle dot `·`. Codes the
    /// symbol tables don't cover (and every non-symbol font) fall back to the
    /// plain CP1252 reading.
    fn decode_byte(&self, byte: u8) -> char {
        if let Some(font) = self.fonts.get(self.chr.font_idx) {
            if font.symbol {
                let mapped = if font.name.to_ascii_lowercase().contains("wingding") {
                    super::office_import::wingdings_to_char(byte)
                } else {
                    crate::font::encoding::symbol_to_char(byte)
                };
                if let Some(c) = mapped {
                    return c;
                }
            }
        }
        cp1252_byte(byte)
    }

    /// Parse `{\colortbl …}` into [`Self::colors`]. Each `;` terminates one entry;
    /// the first entry (often empty) is the "auto" colour (index 0).
    fn read_colortbl(&mut self) {
        let b = self.bytes;
        let mut p = self.i;
        let (mut r, mut g, mut bl) = (0u8, 0u8, 0u8);
        let mut seen = false; // any \red/\green/\blue for this entry?

        while p < b.len() {
            match b[p] {
                b'}' => break, // end of colortbl group
                b'\\' => {
                    let s = p + 1;
                    let mut e = s;
                    while e < b.len() && b[e].is_ascii_alphabetic() {
                        e += 1;
                    }
                    let w = &self.src[s..e];
                    let mut q = e;
                    while q < b.len() && b[q].is_ascii_digit() {
                        q += 1;
                    }
                    let np: u16 = self.src[e..q].parse().unwrap_or(0);
                    match w {
                        "red" => {
                            r = np.min(255) as u8;
                            seen = true;
                        }
                        "green" => {
                            g = np.min(255) as u8;
                            seen = true;
                        }
                        "blue" => {
                            bl = np.min(255) as u8;
                            seen = true;
                        }
                        _ => {}
                    }
                    if q < b.len() && b[q] == b' ' {
                        q += 1;
                    }
                    p = q;
                }
                b';' => {
                    // Commit one colour entry (auto entry if nothing seen).
                    self.colors.push(if seen { [r, g, bl] } else { [0, 0, 0] });
                    r = 0;
                    g = 0;
                    bl = 0;
                    seen = false;
                    p += 1;
                }
                _ => p += 1,
            }
        }
    }

    /// Parse a `{\pict …}` group and, for every decodable blip, push an
    /// [`RtfBlock::Image`]. Called right after the `\pict` control word; scans
    /// ahead from `self.i` to the group close **without** moving `self.i` — the
    /// main loop re-scans the same bytes with `skip` on, suppressing the payload
    /// from the body text (mirrors [`Self::read_fonttbl`]). The `\bin<N>` binary
    /// form is made safe by [`Self::apply_control`] skipping its N raw bytes on
    /// that re-scan (the bytes may embed `{`/`}`/`\` that would otherwise be
    /// mis-read as structure); here we only capture them for decoding.
    ///
    /// Supported source encodings:
    /// * **hex** (RTF default): pairs of hex digits in the group text.
    /// * **`\bin<N>`**: the *N raw bytes* immediately after the delimiter are the
    ///   payload (any blip type may use it). Captured verbatim.
    ///
    /// Supported formats — all decoded to real images:
    /// * `\pngblip` / `\jpegblip` — kept verbatim (already web-native).
    /// * `\wmetafile` / `\emfblip` — WMF/EMF, rasterized by the in-house
    ///   [`metafile`](super::metafile) decoder and re-encoded to PNG.
    /// * `\dibitmap` / `\wbitmap` — packed DIB, decoded by [`decode_dib`] and
    ///   re-encoded to PNG.
    ///
    /// Genuinely-undecodable payloads (unknown blip, corrupt/truncated bytes) are
    /// skipped cleanly: no `<img>`, no panic, no leaked payload bytes.
    fn read_pict(&mut self) {
        let b = self.bytes;
        let mut p = self.i; // at the `\` of `\pict`
        let mut depth = 0i32; // relative depth within the \pict group

        // Picture metadata, gathered from the control words preceding the data.
        let mut subtype: Option<&'static str> = None; // None until a known blip
        let mut kind = PictKind::Unknown; // how to decode the payload
        let (mut picw, mut pich) = (0i64, 0i64); // \picw / \pich (source units)
        let (mut goalw, mut goalh) = (0i64, 0i64); // \picwgoal / \pichgoal (twips)
        let (mut scalex, mut scaley) = (100i64, 100i64); // \picscalex / \picscaley (%)
        let mut hex = String::new(); // collected hex digits of the payload
        let mut bin: Option<Vec<u8>> = None; // raw bytes from a `\bin<N>` payload

        while p < b.len() {
            match b[p] {
                b'{' => {
                    depth += 1;
                    p += 1;
                }
                b'}' => {
                    if depth == 0 {
                        break; // close of the \pict group itself
                    }
                    depth -= 1;
                    p += 1;
                }
                b'\\' => {
                    let s = p + 1;
                    let mut e = s;
                    while e < b.len() && b[e].is_ascii_alphabetic() {
                        e += 1;
                    }
                    let w = &self.src[s..e];
                    let mut neg = false;
                    let mut q = e;
                    if q < b.len() && b[q] == b'-' {
                        neg = true;
                        q += 1;
                    }
                    let ns = q;
                    while q < b.len() && b[q].is_ascii_digit() {
                        q += 1;
                    }
                    let np: Option<i64> =
                        self.src[ns..q]
                            .parse()
                            .ok()
                            .map(|n: i64| if neg { -n } else { n });

                    match w {
                        "pngblip" => {
                            subtype = Some("png");
                            kind = PictKind::Png;
                        }
                        "jpegblip" => {
                            subtype = Some("jpeg");
                            kind = PictKind::Jpeg;
                        }
                        "dibitmap" | "wbitmap" => kind = PictKind::Dib,
                        "wmetafile" => kind = PictKind::Wmf,
                        "emfblip" => kind = PictKind::Emf,
                        "picw" => picw = np.unwrap_or(0),
                        "pich" => pich = np.unwrap_or(0),
                        "picwgoal" => goalw = np.unwrap_or(0),
                        "pichgoal" => goalh = np.unwrap_or(0),
                        "picscalex" => scalex = np.unwrap_or(100),
                        "picscaley" => scaley = np.unwrap_or(100),
                        _ => {}
                    }
                    // A single trailing space delimits the control word.
                    if q < b.len() && b[q] == b' ' {
                        q += 1;
                    }
                    // `\bin<N>`: the next N bytes are the raw (non-hex) payload.
                    // Capture them and jump past, so their arbitrary bytes never
                    // reach the structural scanner.
                    if w == "bin" {
                        let n = np.unwrap_or(0).max(0) as usize;
                        let end = q.saturating_add(n).min(b.len());
                        bin = Some(b[q..end].to_vec());
                        q = end;
                    }
                    p = q;
                }
                c => {
                    // Picture data: hex digit pairs (the RTF default encoding).
                    if depth == 0 && c.is_ascii_hexdigit() {
                        hex.push(c as char);
                    }
                    p += 1;
                }
            }
        }

        // The encoded payload bytes: `\bin` raw form takes precedence over hex.
        let raw: Option<Vec<u8>> = match bin {
            Some(bytes) if !bytes.is_empty() => Some(bytes),
            _ => decode_hex(&hex),
        };
        let Some(raw) = raw else {
            return;
        };

        // Decode to final image bytes (re-encoding vector/bitmap forms to PNG).
        let (data, subtype): (Vec<u8>, &'static str) = match kind {
            PictKind::Png => {
                if !raw.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
                    return;
                }
                (raw, "png")
            }
            PictKind::Jpeg => {
                if !raw.starts_with(&[0xFF, 0xD8, 0xFF]) {
                    return;
                }
                (raw, "jpeg")
            }
            PictKind::Wmf => match metafile_to_png(super::metafile::decode_wmf(&raw)) {
                Some(png) => (png, "png"),
                None => return,
            },
            PictKind::Emf => match metafile_to_png(super::metafile::decode_emf(&raw)) {
                Some(png) => (png, "png"),
                None => return,
            },
            PictKind::Dib => match decode_dib(&raw) {
                Some((w, h, rgba)) => (crate::raster::encode_png(w, h, &rgba), "png"),
                None => return,
            },
            PictKind::Unknown => {
                let _ = subtype; // no recognized blip control word
                return;
            }
        };

        // Display size: prefer the goal (twips → pt); else the source dimensions
        // (also taken as twips) scaled by \picscale; else a sane default. RTF
        // bitmap source units are nominally twips for metafiles and pixels for
        // bitmaps, but goal is what the document author asked to display.
        let scale_w = (scalex.max(1)) as f64 / 100.0;
        let scale_h = (scaley.max(1)) as f64 / 100.0;
        let width_pt = if goalw > 0 {
            twips_to_pt(goalw as i32) * scale_w
        } else if picw > 0 {
            twips_to_pt(picw as i32) * scale_w
        } else {
            96.0
        };
        let height_pt = if goalh > 0 {
            twips_to_pt(goalh as i32) * scale_h
        } else if pich > 0 {
            twips_to_pt(pich as i32) * scale_h
        } else {
            96.0
        };

        // Pictures sit inline in the source flow; flush any pending paragraph so
        // the image lands in document order, then push it as its own block.
        if self.cur_started || !self.cur.runs.is_empty() {
            let para = std::mem::take(&mut self.cur);
            self.cur_started = false;
            self.blocks.push(RtfBlock::Para(para));
        }
        self.blocks.push(RtfBlock::Image(RtfPicture {
            data,
            subtype,
            width_pt: width_pt.max(1.0),
            height_pt: height_pt.max(1.0),
        }));
    }

    /// Extent of the current destination group: `(content_start, closing_brace)`.
    /// Called with `self.i` on the `\` of the destination's control word; skips
    /// that word (+ optional signed parameter + one delimiter space), then
    /// balances braces up to the `}` that closes the group, honouring control
    /// symbols (`\{ \} \\ \~ …`), `\'xx` hex escapes and `\binN` raw payloads.
    /// Truncated input yields `(start, len)`. Both bounds land on ASCII
    /// structure bytes, so they are always valid `str` boundaries.
    fn group_extent(&self) -> (usize, usize) {
        let b = self.bytes;
        let mut p = (self.i + 1).min(b.len());
        while p < b.len() && b[p].is_ascii_alphabetic() {
            p += 1;
        }
        if p < b.len() && b[p] == b'-' {
            p += 1;
        }
        while p < b.len() && b[p].is_ascii_digit() {
            p += 1;
        }
        if p < b.len() && b[p] == b' ' {
            p += 1;
        }
        let start = p;
        let mut depth = 0i32;
        while p < b.len() {
            match b[p] {
                b'{' => {
                    depth += 1;
                    p += 1;
                }
                b'}' => {
                    if depth == 0 {
                        return (start, p);
                    }
                    depth -= 1;
                    p += 1;
                }
                b'\\' => {
                    if p + 1 >= b.len() {
                        return (start, b.len());
                    }
                    let c = b[p + 1];
                    if c == b'\'' {
                        p = (p + 4).min(b.len());
                    } else if !c.is_ascii_alphabetic() {
                        // Control symbol (incl. escaped `\{ \} \\`): two bytes.
                        p += 2;
                    } else {
                        // Control word; `\binN` announces N raw payload bytes.
                        let s = p + 1;
                        let mut e = s;
                        while e < b.len() && b[e].is_ascii_alphabetic() {
                            e += 1;
                        }
                        let word = &self.src[s..e];
                        let mut q = e;
                        let neg = q < b.len() && b[q] == b'-';
                        if neg {
                            q += 1;
                        }
                        let ns = q;
                        while q < b.len() && b[q].is_ascii_digit() {
                            q += 1;
                        }
                        let n: usize = if neg {
                            0
                        } else {
                            self.src[ns..q].parse().unwrap_or(0)
                        };
                        if q < b.len() && b[q] == b' ' {
                            q += 1;
                        }
                        if word == "bin" {
                            q = q.saturating_add(n).min(b.len());
                        }
                        p = q;
                    }
                }
                _ => p += 1,
            }
        }
        (start, b.len())
    }

    /// Parse `src[start..end]` — the payload of a destination group — as an
    /// embedded RTF fragment with this parser's resource tables (fonts /
    /// colours / stylesheet live in the document header, before any such
    /// destination) and live character state inherited. Returns the fragment's
    /// recovered blocks; recursion is bounded by [`MAX_FRAG_DEPTH`].
    fn parse_fragment(&self, start: usize, end: usize) -> Vec<RtfBlock> {
        if start >= end || end > self.src.len() {
            return Vec::new();
        }
        let mut sub = Parser::new(&self.src[start..end]);
        sub.fonts = self.fonts.clone();
        sub.colors = self.colors.clone();
        sub.pstyles = self.pstyles.clone();
        sub.cstyles = self.cstyles.clone();
        sub.chr = self.chr.clone();
        sub.uc = self.uc;
        sub.frag_depth = self.frag_depth + 1;
        sub.drive()
    }

    /// `{\header(l|r|f)? …}` / `{\footer(l|r|f)? …}`: recover the running
    /// header/footer as blocks (same shape the DOCX importer stores in
    /// [`Section::header`]). The group is parsed once by a sub-parser and the
    /// main scanner jumps to its closing `}`, so nothing leaks into the body —
    /// including the `\pard`/`\par` state churn the old skip re-scan let
    /// through. The left/right/first page variants fold onto the model's
    /// single header/footer with preference default > first > left > right.
    fn read_header_footer(&mut self, word: &str, is_header: bool) {
        let (start, close) = self.group_extent();
        self.jump_to = Some(close);
        self.skip = true; // belt-and-braces; the jump already skips the payload
        if self.inherited_skip() || self.frag_depth >= MAX_FRAG_DEPTH {
            return;
        }
        let rank = match word {
            "header" | "footer" => 0u8,
            "headerf" | "footerf" => 1,
            "headerl" | "footerl" => 2,
            _ => 3, // headerr / footerr
        };
        let slot_rank = if is_header {
            self.header.as_ref().map(|(r, _)| *r)
        } else {
            self.footer.as_ref().map(|(r, _)| *r)
        };
        if slot_rank.is_some_and(|r| r <= rank) {
            return; // an equal-or-better variant is already in place
        }
        let blocks = self.parse_fragment(start, close);
        // An all-blank group (Word writes empty `{\header \pard\par}` stubs)
        // imports as *absent*, not as an empty Section.header.
        let keep = !fragment_text(&blocks).is_empty()
            || blocks.iter().any(|b| matches!(b, RtfBlock::Image(_)));
        if !keep {
            return;
        }
        if is_header {
            self.header = Some((rank, blocks));
        } else {
            self.footer = Some((rank, blocks));
        }
    }

    /// `{\footnote …}`: recover the note as a **typed** footnote — the runs
    /// pushed at the reference point (a superscript ordinal standing in for
    /// the auto-numbered `\chftn` marker, plus the inlined body copy the HTML
    /// path still renders) are tagged with the note id
    /// ([`Run::footnote_ref`]), and the body blocks are recorded on
    /// [`Self::footnotes`] so the model path lowers the anchor to
    /// [`Inline::FootnoteRef`] + [`Document::footnotes`] — the same shape the
    /// DOCX importer produces from `w:footnoteReference`. Inside a sub-parsed
    /// fragment (`frag_depth > 0`: a note in a header/another note) the
    /// document-level list is out of reach, so it degrades to the historical
    /// untagged inline rendering.
    fn read_footnote(&mut self) {
        let (start, close) = self.group_extent();
        self.jump_to = Some(close);
        let import = !self.inherited_skip()
            && self.fldinst_capture.is_none()
            && self.frag_depth < MAX_FRAG_DEPTH;
        if import {
            // A `\*` on this very group set `skip`; the note is understood, so
            // it must not mute the inlined body (the `}` restores the state).
            self.skip = false;
            let blocks = self.parse_fragment(start, close);
            let body = fragment_text(&blocks);
            self.footnote_n += 1;
            // `frag_depth == 0`: footnotes live on the *document*; a
            // sub-parser's list is discarded, so tagging there would leave a
            // dangling anchor. Degrade to the untagged inline rendering.
            let id = (self.frag_depth == 0).then(|| self.footnote_n.to_string());
            let marker = CharState {
                superscript: true,
                ..CharState::default()
            };
            self.push_run(self.footnote_n.to_string(), marker, None, id.clone());
            if !body.is_empty() {
                self.push_run(format!(" {body}"), CharState::default(), None, id.clone());
            }
            if let Some(id) = id {
                self.footnotes.push((id, blocks));
            }
        }
        self.skip = true;
    }

    /// `{\*\atnid …}` (reviewer initials, `id == true`) / `{\*\atnauthor …}`
    /// (full name): small text destinations remembered for the next
    /// `{\*\annotation …}` group.
    fn read_atn_meta(&mut self, id: bool) {
        let (start, close) = self.group_extent();
        self.jump_to = Some(close);
        if !self.inherited_skip() && self.frag_depth < MAX_FRAG_DEPTH {
            let text = fragment_text(&self.parse_fragment(start, close));
            if !text.is_empty() {
                if id {
                    self.pending_atnid = Some(text);
                } else {
                    self.pending_atnauthor = Some(text);
                }
            }
        }
        self.skip = true;
    }

    /// `{\*\annotation …}`: recover the review-comment body and anchor it in
    /// the run flow, paired with the `{\*\atnid}` / `{\*\atnauthor}` metadata
    /// that preceded the `\chatn` reference. Lowered to [`Document::comments`]
    /// plus an [`Inline::CommentRef`] — the shape the DOCX importer produces
    /// from `w:commentReference` and `word/comments.xml`. The inner
    /// `{\*\atnref}` / `{\*\atndate}` groups are ignorable destinations to the
    /// sub-parser, so the body text comes out clean.
    fn read_annotation(&mut self) {
        let (start, close) = self.group_extent();
        self.jump_to = Some(close);
        let author = self.pending_atnauthor.take();
        let initials = self.pending_atnid.take();
        // `frag_depth == 0`: comments live on the *document*; a sub-parser's
        // comment list is discarded, so importing one inside a header/footnote
        // fragment would leave an orphaned anchor. Degrade to skipping there.
        let import =
            !self.inherited_skip() && self.fldinst_capture.is_none() && self.frag_depth == 0;
        if import {
            self.skip = false; // the group's own `\*` must not mute the anchor
            let text = fragment_text(&self.parse_fragment(start, close));
            let id = (self.comments.len() + 1).to_string();
            self.comments.push(RtfComment {
                id: id.clone(),
                author: author.or(initials).unwrap_or_default(),
                text,
            });
            self.push_run(String::new(), CharState::default(), Some(id), None);
        }
        self.skip = true;
    }

    /// `{\listtext …}` / `{\pntext …}`: the pre-rendered list marker. Emitting
    /// it would duplicate the marker the recovered list renders, so the group
    /// is consumed — after classifying the paragraph as numbered vs bulleted
    /// (see [`marker_is_ordered`]), when the `{\*\pn …}` definition hasn't
    /// already decided.
    fn read_listtext(&mut self) {
        let (start, close) = self.group_extent();
        self.jump_to = Some(close);
        if !self.skip
            && !self.inherited_skip()
            && self.frag_depth < MAX_FRAG_DEPTH
            && self.par.list_ordered.is_none()
        {
            let marker = fragment_text(&self.parse_fragment(start, close));
            self.par.list_ordered = Some(marker_is_ordered(&marker));
        }
        self.skip = true;
    }

    /// Parse `{\stylesheet{\ql\fs22 Normal;}{\s1\b\fs32 heading 1;}{\*\cs10\ul
    /// Hyperlink;}…}` into [`Self::pstyles`] / [`Self::cstyles`]. Scans ahead
    /// from the control word (the main scanner jumps past the group, so names
    /// never leak as body text). Only the base formatting is kept — the
    /// `\sbasedon`/`\snext` chains and style names are dropped, per the
    /// inline-resolution contract on [`StyleDef`]. A leading definition with no
    /// `\s`/`\cs` keyword is the implicit paragraph style 0 ("Normal").
    fn read_stylesheet(&mut self) {
        let (_, close) = self.group_extent();
        self.jump_to = Some(close);
        let b = self.bytes;
        let mut p = self.i; // on the `\` of `\stylesheet`
        let mut depth = 0i32;
        let mut def = StyleDef::default();
        let mut para_idx: Option<usize> = None; // `\s N`
        let mut char_idx: Option<usize> = None; // `\*\cs N`
        let mut first_entry = true;
        while p < b.len() {
            match b[p] {
                b'{' => {
                    depth += 1;
                    p += 1;
                }
                b'}' => {
                    if depth == 0 {
                        break; // close of the stylesheet group itself
                    }
                    if depth == 1 {
                        // Close of one style definition: commit it.
                        if let Some(idx) = para_idx.take() {
                            self.pstyles.insert(idx, def);
                        } else if let Some(idx) = char_idx.take() {
                            self.cstyles.insert(idx, def);
                        } else if first_entry {
                            self.pstyles.insert(0, def);
                        }
                        first_entry = false;
                        def = StyleDef::default();
                    }
                    depth -= 1;
                    p += 1;
                }
                b'\\' => {
                    let s = p + 1;
                    if s >= b.len() {
                        break;
                    }
                    if !b[s].is_ascii_alphanumeric() {
                        // `\*` markers / `\'xx` escapes inside names: skip.
                        p = if b[s] == b'\'' {
                            (p + 4).min(b.len())
                        } else {
                            p + 2
                        };
                        continue;
                    }
                    let mut e = s;
                    while e < b.len() && b[e].is_ascii_alphabetic() {
                        e += 1;
                    }
                    let w = &self.src[s..e];
                    let mut q = e;
                    let neg = q < b.len() && b[q] == b'-';
                    if neg {
                        q += 1;
                    }
                    let ns = q;
                    while q < b.len() && b[q].is_ascii_digit() {
                        q += 1;
                    }
                    let np: Option<i64> = self.src[ns..q]
                        .parse()
                        .ok()
                        .map(|n: i64| if neg { -n } else { n });
                    if q < b.len() && b[q] == b' ' {
                        q += 1;
                    }
                    // Only depth-1 words describe the entry itself (deeper
                    // groups are `\keycode`-style riders).
                    if depth == 1 {
                        match w {
                            "s" => para_idx = np.and_then(|n| usize::try_from(n).ok()),
                            "cs" => char_idx = np.and_then(|n| usize::try_from(n).ok()),
                            "b" => def.bold = np != Some(0),
                            "i" => def.italic = np != Some(0),
                            "ul" => def.underline = np != Some(0),
                            "ulnone" => def.underline = false,
                            "strike" => def.strike = np != Some(0),
                            "fs" => def.half_points = np.unwrap_or(0).max(0) as u32,
                            "cf" => def.color_idx = np.unwrap_or(0).max(0) as usize,
                            "f" => def.font_idx = np.and_then(|n| usize::try_from(n).ok()),
                            "ql" => def.align = Some(Align::Left),
                            "qc" => def.align = Some(Align::Center),
                            "qr" => def.align = Some(Align::Right),
                            "qj" => def.align = Some(Align::Justify),
                            _ => {}
                        }
                    }
                    p = q;
                }
                _ => p += 1, // style names / `;` terminators: not recorded
            }
        }
    }

    fn color_hex(&self, idx: usize) -> Option<String> {
        // Index 0 is the "auto" colour → inherit (no explicit colour).
        if idx == 0 {
            return None;
        }
        self.colors
            .get(idx)
            .map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}"))
    }

    /// Resolve a `\cf` index to normalized RGB (`0.0..=1.0`) for the model
    /// [`CharStyle`](crate::model::CharStyle). Index 0 is "auto" → `None`.
    fn color_rgb(&self, idx: usize) -> Option<[f64; 3]> {
        if idx == 0 {
            return None;
        }
        self.colors
            .get(idx)
            .map(|[r, g, b]| [*r as f64 / 255.0, *g as f64 / 255.0, *b as f64 / 255.0])
    }
}

/// Maximum nesting of destination sub-parsers ([`Parser::parse_fragment`]):
/// real documents nest a footnote inside a header at most; anything deeper is
/// pathological input and degrades to plain skipping.
const MAX_FRAG_DEPTH: u8 = 4;

/// Classify a pre-rendered list-marker text (`{\listtext …}` / `{\pntext …}`)
/// as numbered (`true`) vs bulleted (`false`). Any digit ⇒ numbered (`1.`,
/// `12)`), and so does a lone letter followed by `.`/`)` — Word's alpha/roman
/// sub-levels (`a.`, `i.`) — so they stay in the same ordered list. A bare
/// letter without ordinal punctuation stays a bullet (Word draws its level-2
/// bullet as a Courier `o`).
fn marker_is_ordered(marker: &str) -> bool {
    let t = marker.trim();
    if t.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    let mut ch = t.chars();
    matches!(
        (ch.next(), ch.next()),
        (Some(a), Some(p)) if a.is_ascii_alphabetic() && (p == '.' || p == ')')
    )
}

/// Flatten a fragment's recovered blocks to normalized plain text (runs
/// concatenated, paragraphs/cells separated by single spaces, whitespace
/// collapsed) — the same shape the DOCX importer's `inline_blocks_text`
/// produces when it inlines a note body.
fn fragment_text(blocks: &[RtfBlock]) -> String {
    fn para(p: &Para, out: &mut String) {
        for r in &p.runs {
            out.push_str(&r.text);
        }
        out.push(' ');
    }
    let mut out = String::new();
    for b in blocks {
        match b {
            RtfBlock::Para(p) => para(p, &mut out),
            RtfBlock::Table(rows) => {
                for row in rows {
                    for cell in row {
                        for p in cell {
                            para(p, &mut out);
                        }
                    }
                }
            }
            RtfBlock::Image(_) | RtfBlock::PageBreak => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Mine a `\fldinst` instruction string for a `HYPERLINK "target"` field and
/// return the target URL. Word/RTF emit the canonical quoted form
/// (`HYPERLINK "https://example.com"`); the unquoted form is treated as having
/// no recoverable target (returns `None`). Switches such as `\l` (bookmark) are
/// ignored — only the first quoted argument is taken.
fn extract_hyperlink(instr: Option<&str>) -> Option<String> {
    let instr = instr?;
    // Find the HYPERLINK keyword (case-insensitive), then the first
    // double-quoted argument that follows it.
    let upper = instr.to_ascii_uppercase();
    let kw = upper.find("HYPERLINK")?;
    let rest = &instr[kw + "HYPERLINK".len()..];
    let open = rest.find('"')?;
    let after = &rest[open + 1..];
    let close = after.find('"')?;
    let url = after[..close].trim();
    if url.is_empty() {
        None
    } else {
        Some(url.to_string())
    }
}

// ──────────────────────────── HTML serialization ───────────────────────────

fn esc_html(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#160;&#160;&#160;&#160;"),
            '\n' => out.push_str("<br>"),
            c => out.push(c),
        }
    }
}

/// Build the CSS `style="…"` body for a run, given the resolved colour/font.
fn run_style(p: &Parser, s: &CharState) -> String {
    let mut css = String::new();
    if s.bold {
        css.push_str("font-weight:bold;");
    }
    if s.italic {
        css.push_str("font-style:italic;");
    }
    if s.underline && s.strike {
        css.push_str("text-decoration:underline line-through;");
    } else if s.underline {
        css.push_str("text-decoration:underline;");
    } else if s.strike {
        css.push_str("text-decoration:line-through;");
    }
    if let Some(c) = p.color_hex(s.color_idx) {
        css.push_str(&format!("color:{c};"));
    }
    // `\fs` is in half-points; super/sub render smaller (engine has no
    // vertical-align, so size is the honest approximation).
    if s.half_points > 0 {
        let mut pt = s.half_points as f64 / 2.0;
        if s.superscript || s.subscript {
            pt *= 0.66;
        }
        css.push_str(&format!("font-size:{pt:.1}pt;"));
    } else if s.superscript || s.subscript {
        css.push_str("font-size:66%;");
    }
    if let Some(font) = p.fonts.get(s.font_idx) {
        if !font.name.is_empty() {
            let fam = font.name.replace(['"', ';'], "");
            if font.generic.is_empty() {
                css.push_str(&format!("font-family:'{fam}';"));
            } else {
                css.push_str(&format!("font-family:'{fam}',{};", font.generic));
            }
        } else if !font.generic.is_empty() {
            css.push_str(&format!("font-family:{};", font.generic));
        }
    }
    css
}

fn align_css(a: Align) -> &'static str {
    match a {
        Align::Left => "",
        Align::Center => "text-align:center;",
        Align::Right => "text-align:right;",
        Align::Justify => "text-align:justify;",
    }
}

/// twips (1/1440") → CSS pt (1/72").
fn twips_to_pt(t: i32) -> f64 {
    t as f64 / 20.0
}

/// Decode a string of hex digit pairs (a `\pict` payload) into raw bytes.
/// Non-hex characters must already be filtered out by the caller; an odd final
/// nibble is dropped. Returns `None` only when there is no data.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    let digits = hex.as_bytes();
    if digits.len() < 2 {
        return None;
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    let mut iter = digits.chunks_exact(2);
    for pair in &mut iter {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Re-encode a decoded metafile raster (from the in-house WMF/EMF decoder) to a
/// PNG, or `None` if decoding failed or produced an empty raster.
fn metafile_to_png(raster: Option<super::metafile::MetafileRaster>) -> Option<Vec<u8>> {
    let r = raster?;
    if r.width == 0 || r.height == 0 || r.rgba.len() < (r.width as usize) * (r.height as usize) * 4
    {
        return None;
    }
    Some(crate::raster::encode_png(r.width, r.height, &r.rgba))
}

/// Decode a **packed DIB** — a `BITMAPINFOHEADER` (≥40 bytes) followed by its
/// palette (for ≤8 bpp) and pixel bits — to top-down RGBA8 `(width, height,
/// rgba)`. This is exactly the payload an RTF `\dibitmap` (and the older
/// `\wbitmap`, in practice a packed DIB) carries. Supports the common
/// uncompressed `BI_RGB` depths (1/4/8/24/32 bpp); compressed (RLE/bitfields)
/// or malformed input returns `None`. Self-contained; never panics.
fn decode_dib(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let rd_u16 =
        |o: usize| -> Option<u16> { data.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])) };
    let rd_u32 = |o: usize| -> Option<u32> {
        data.get(o..o + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };
    let rd_i32 = |o: usize| -> Option<i32> {
        data.get(o..o + 4)
            .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    };

    let header_size = rd_u32(0)? as usize;
    // Modern BITMAPINFOHEADER family only (≥40); reject the rare 12-byte core form.
    if header_size < 40 || header_size > data.len() {
        return None;
    }
    let width = rd_i32(4)?;
    let height_raw = rd_i32(8)?;
    let bit_count = rd_u16(14)? as u32;
    let compression = rd_u32(16)?;
    // Only uncompressed BI_RGB; bound dimensions to keep allocation sane.
    if compression != 0 || width <= 0 || width > (1 << 16) {
        return None;
    }
    let top_down = height_raw < 0;
    let height = height_raw.unsigned_abs();
    if height == 0 || height > (1 << 16) {
        return None;
    }
    let (w, h) = (width as u32, height);

    // Palette (BGRA quads) for indexed depths.
    let mut clr_used = rd_u32(32).unwrap_or(0);
    if clr_used == 0 && bit_count <= 8 {
        clr_used = 1u32 << bit_count;
    }
    let palette_len = if bit_count <= 8 { clr_used as usize } else { 0 };
    let mut palette: Vec<[u8; 3]> = Vec::with_capacity(palette_len);
    for i in 0..palette_len {
        let o = header_size + i * 4;
        // Truncated palette → pad black so indices stay valid.
        match data.get(o..o + 4) {
            Some(q) => palette.push([q[2], q[1], q[0]]),
            None => palette.push([0, 0, 0]),
        }
    }

    let bits_off = header_size + palette_len * 4;
    let bits = data.get(bits_off..)?;

    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    let mut store = |x: u32, row: u32, rgb: [u8; 3]| {
        // Rows are bottom-up unless `top_down`; flip to top-down storage.
        let y = if top_down { row } else { h - 1 - row };
        if x < w && y < h {
            let i = ((y * w + x) * 4) as usize;
            out[i] = rgb[0];
            out[i + 1] = rgb[1];
            out[i + 2] = rgb[2];
            out[i + 3] = 255;
        }
    };

    // Each row is padded up to a 4-byte boundary.
    let row_bytes = ((w as usize) * bit_count as usize).div_ceil(32) * 4;
    match bit_count {
        1 | 4 | 8 => {
            for row in 0..h {
                let ro = (row as usize) * row_bytes;
                let line = match bits.get(ro..ro + row_bytes) {
                    Some(l) => l,
                    None => break,
                };
                for x in 0..w {
                    let bit_pos = (x as usize) * bit_count as usize;
                    let idx = match bit_count {
                        8 => line[bit_pos / 8] as usize,
                        4 => {
                            let byte = line[bit_pos / 8];
                            if x & 1 == 0 {
                                (byte >> 4) as usize
                            } else {
                                (byte & 0x0F) as usize
                            }
                        }
                        _ => {
                            let byte = line[bit_pos / 8];
                            ((byte >> (7 - (bit_pos & 7))) & 1) as usize
                        }
                    };
                    store(x, row, *palette.get(idx).unwrap_or(&[0, 0, 0]));
                }
            }
        }
        24 => {
            for row in 0..h {
                let ro = (row as usize) * row_bytes;
                if bits.get(ro..ro + row_bytes).is_none() {
                    break;
                }
                for x in 0..w {
                    let p = ro + (x as usize) * 3;
                    store(x, row, [bits[p + 2], bits[p + 1], bits[p]]);
                }
            }
        }
        32 => {
            for row in 0..h {
                let ro = (row as usize) * row_bytes;
                if bits.get(ro..ro + row_bytes).is_none() {
                    break;
                }
                for x in 0..w {
                    let p = ro + (x as usize) * 4;
                    store(x, row, [bits[p + 2], bits[p + 1], bits[p]]);
                }
            }
        }
        _ => return None,
    }
    Some((w, h, out))
}

fn para_html(p: &Parser, para: &Para, out: &mut String) {
    let mut style = String::from(align_css(para.align));
    if para.indent_left > 0 {
        style.push_str(&format!(
            "margin-left:{:.1}pt;",
            twips_to_pt(para.indent_left)
        ));
    }
    if para.first_line != 0 {
        style.push_str(&format!(
            "text-indent:{:.1}pt;",
            twips_to_pt(para.first_line)
        ));
    }
    if style.is_empty() {
        out.push_str("<p>");
    } else {
        out.push_str(&format!("<p style=\"{style}\">"));
    }
    if para.runs.iter().all(|r| r.text.trim().is_empty()) {
        // Empty paragraph: keep vertical space.
        out.push_str("&#160;");
    }
    for run in &para.runs {
        if run.text.is_empty() {
            continue;
        }
        // A hyperlink run is wrapped in an `<a href>` so the link survives into
        // HTML (and, via `html_to_model`, into the model as an `Inline::Link`).
        if let Some(url) = &run.link {
            out.push_str("<a href=\"");
            esc_attr(url, out);
            out.push_str("\">");
        }
        let css = run_style(p, &run.style);
        if css.is_empty() {
            esc_html(&run.text, out);
        } else {
            out.push_str(&format!("<span style=\"{css}\">"));
            esc_html(&run.text, out);
            out.push_str("</span>");
        }
        if run.link.is_some() {
            out.push_str("</a>");
        }
    }
    out.push_str("</p>");
}

/// Escape a string for use inside a double-quoted HTML attribute value.
fn esc_attr(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

fn table_html(p: &Parser, rows: &[Vec<Vec<Para>>], out: &mut String) {
    out.push_str(
        "<table style=\"border-collapse:collapse;\" border=\"1\" cellpadding=\"4\"><tbody>",
    );
    for row in rows {
        out.push_str("<tr>");
        for cell in row {
            out.push_str("<td style=\"border:1px solid #000;padding:4pt;\">");
            for para in cell {
                para_html(p, para, out);
            }
            out.push_str("</td>");
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
}

/// Serialize a recovered `\pict` image as an `<img>` with a base64 `data:` URI.
/// The HTML engine decodes the data URI and embeds the PNG/JPEG into the PDF;
/// `width`/`height` (HTML attributes, in points) drive its layout box.
fn image_html(pic: &RtfPicture, out: &mut String) {
    out.push_str(&format!(
        "<p><img src=\"data:image/{};base64,{}\" width=\"{:.1}\" height=\"{:.1}\"></p>",
        pic.subtype,
        super::base64(&pic.data),
        pic.width_pt,
        pic.height_pt,
    ));
}

/// Parse RTF and serialize it to styled HTML for the [`crate::html`] engine.
pub fn rtf_to_html(rtf: &str) -> String {
    let mut parser = Parser::new(rtf);
    // `drive` returns the blocks but leaves the parser (and its now-populated
    // `fonts` / `colors` tables) alive, so we can borrow it for serialization.
    let blocks = parser.drive();

    let mut html = String::from("<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head><body>");
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            // Consecutive list-tagged paragraphs of one kind render as a real
            // `<ul>`/`<ol>` (nesting conveyed by a per-level margin), so the
            // marker recovered from `{\*\pn}`/`{\listtext}` is drawn by the
            // list — never duplicated as literal text.
            RtfBlock::Para(para) if para.list.is_some() => {
                let ordered = para.list.map(|(o, _)| o).unwrap_or(false);
                let tag = if ordered { "ol" } else { "ul" };
                html.push_str(&format!("<{tag}>"));
                while i < blocks.len() {
                    let RtfBlock::Para(q) = &blocks[i] else { break };
                    let Some((o, lvl)) = q.list else { break };
                    if o != ordered {
                        break;
                    }
                    if lvl > 0 {
                        html.push_str(&format!(
                            "<li style=\"margin-left:{:.1}pt\">",
                            lvl as f64 * 18.0
                        ));
                    } else {
                        html.push_str("<li>");
                    }
                    para_html(&parser, q, &mut html);
                    html.push_str("</li>");
                    i += 1;
                }
                html.push_str(&format!("</{tag}>"));
                continue;
            }
            RtfBlock::Para(para) => para_html(&parser, para, &mut html),
            RtfBlock::Table(rows) => table_html(&parser, rows, &mut html),
            RtfBlock::Image(pic) => image_html(pic, &mut html),
            RtfBlock::PageBreak => {
                html.push_str("<div style=\"page-break-before:always;break-before:page\"></div>");
            }
        }
        i += 1;
    }
    html.push_str("</body></html>");
    html
}

// ─────────────────────── unified Document model lowering ────────────────────

use crate::convert::style::Generic;
use crate::model::{
    Block, BlockKind, Cell, CharStyle, Comment, Document, ImageRef, ImageResource, Inline,
    InlineRun, LineHeight, LinkTarget, List, ListItem, ListMarker, Page, Paragraph,
    ParagraphStyle, Row, Section, Table,
};
use std::collections::BTreeMap;

/// Parse RTF and lower it **directly** into the unified editable
/// [`Document`](crate::model::Document) model — the rich counterpart of the
/// plain-text path. This reuses the same stateful parser as [`rtf_to_html`]
/// (no second RTF tokenizer): the recovered [`RtfBlock`]s are lowered to model
/// blocks with run-level [`CharStyle`] (bold/italic/underline/strike, colour,
/// size, font family), tables ([`BlockKind::Table`]), `\pict` images
/// (bytes **interned** into [`Document::resources`]), and `\field` hyperlinks
/// ([`Inline::Link`]).
///
/// One section / one page of *flow* blocks (A4 default geometry), mirroring
/// [`crate::html::to_model`].
pub fn rtf_to_model(rtf: &str) -> Document {
    let mut parser = Parser::new(rtf);
    let rtf_blocks = parser.drive();

    let mut images: BTreeMap<u64, ImageResource> = BTreeMap::new();
    let mut pages: Vec<Page> = Vec::new();
    let mut seg: Vec<RtfBlock> = Vec::new();
    for rb in rtf_blocks {
        match rb {
            RtfBlock::PageBreak => {
                pages.push(Page {
                    blocks: lower_blocks(&parser, &seg, &mut images),
                    absolute: false,
                });
                seg.clear();
            }
            other => seg.push(other),
        }
    }
    pages.push(Page {
        blocks: lower_blocks(&parser, &seg, &mut images),
        absolute: false,
    });

    // Running header / footer recovered from `{\header…}` / `{\footer…}` (page
    // variants already folded by preference inside the parser) — same
    // block-list shape the DOCX importer stores.
    let header_blocks = parser.header.take();
    let footer_blocks = parser.footer.take();
    let header = header_blocks
        .map(|(_, blocks)| lower_blocks(&parser, &blocks, &mut images))
        .filter(|v| !v.is_empty());
    let footer = footer_blocks
        .map(|(_, blocks)| lower_blocks(&parser, &blocks, &mut images))
        .filter(|v| !v.is_empty());
    let comments = std::mem::take(&mut parser.comments);
    // Typed foot-notes: lower each recovered body to model blocks; the anchors
    // were tagged on the runs (`Run::footnote_ref`) and become
    // `Inline::FootnoteRef` in `para_to_model`.
    let footnotes = std::mem::take(&mut parser.footnotes);
    let footnotes: Vec<crate::model::Footnote> = footnotes
        .into_iter()
        .map(|(id, blocks)| crate::model::Footnote {
            id,
            blocks: lower_blocks(&parser, &blocks, &mut images),
        })
        .collect();

    let mut doc = Document {
        sections: vec![Section {
            geometry: crate::model::PageGeometry::default(),
            header,
            footer,
            pages,
            ..Default::default()
        }],
        ..Document::default()
    };
    doc.comments = comments
        .into_iter()
        .map(|c| Comment {
            id: c.id,
            author: c.author,
            date: String::new(),
            text: c.text,
        })
        .collect();
    doc.footnotes = footnotes;
    doc.resources.images = images;
    doc
}

/// Lower a flat run of recovered blocks to model blocks: consecutive
/// list-tagged paragraphs of one kind group into a single [`BlockKind::List`]
/// (items keep their nesting level), `\pict` bytes intern into the shared
/// image table, and page breaks — only meaningful at body level — are ignored.
fn lower_blocks(
    p: &Parser,
    blocks: &[RtfBlock],
    images: &mut BTreeMap<u64, ImageResource>,
) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            RtfBlock::Para(para) if para.list.is_some() => {
                let ordered = para.list.map(|(o, _)| o).unwrap_or(false);
                let mut items: Vec<ListItem> = Vec::new();
                while i < blocks.len() {
                    let RtfBlock::Para(q) = &blocks[i] else { break };
                    let Some((o, lvl)) = q.list else { break };
                    if o != ordered {
                        break;
                    }
                    items.push(ListItem {
                        blocks: vec![model_block(BlockKind::Paragraph(para_to_model(p, q)))],
                        level: lvl,
                        label: None,
                    });
                    i += 1;
                }
                out.push(model_block(BlockKind::List(List {
                    ordered,
                    marker: if ordered {
                        ListMarker::Decimal
                    } else {
                        ListMarker::Bullet('•')
                    },
                    items,
                    ..List::default()
                })));
            }
            RtfBlock::Para(para) => {
                out.push(model_block(BlockKind::Paragraph(para_to_model(p, para))));
                i += 1;
            }
            RtfBlock::Table(rows) => {
                out.push(model_block(BlockKind::Table(table_to_model(p, rows))));
                i += 1;
            }
            RtfBlock::Image(pic) => {
                let key = fnv1a(&pic.data);
                images.entry(key).or_insert_with(|| ImageResource {
                    bytes: pic.data.clone(),
                    format: pic.subtype.to_string(),
                });
                out.push(model_block(BlockKind::Image(ImageRef {
                    resource: key,
                    ..Default::default()
                })));
                i += 1;
            }
            RtfBlock::PageBreak => i += 1,
        }
    }
    out
}

/// A default-framed (flow) [`Block`] carrying `kind`.
fn model_block(kind: BlockKind) -> Block {
    Block {
        kind,
        ..Block::default()
    }
}

/// Lower a recovered [`Para`] to a model [`Paragraph`]: paragraph alignment +
/// indents, and inline content where consecutive runs sharing one hyperlink
/// target collapse into a single [`Inline::Link`]. Footnote-tagged runs
/// ([`Run::footnote_ref`]) lower to one [`Inline::FootnoteRef`] per note id
/// (the tagged marker/body copies exist for the HTML path only — the model
/// body lives in [`Document::footnotes`]).
fn para_to_model(p: &Parser, para: &Para) -> Paragraph {
    let mut runs: Vec<Inline> = Vec::new();
    // Note ids already anchored in this paragraph: the tagged marker + the
    // inlined-body copies share one id and must yield a single `FootnoteRef`
    // (the copies exist for the HTML path; the model body lives in
    // `Document::footnotes`).
    let mut seen_notes: Vec<String> = Vec::new();
    // The model inline(s) for one parser run: a first-seen footnote tag → the
    // typed anchor, a repeated tag → nothing (dropped body copy), untagged →
    // the ordinary lowering.
    let mut lower = |run: &Run| -> Option<Inline> {
        match &run.footnote_ref {
            Some(id) if seen_notes.contains(id) => None,
            Some(id) => {
                seen_notes.push(id.clone());
                Some(Inline::FootnoteRef { id: id.clone() })
            }
            None => Some(inline_to_model(p, run)),
        }
    };
    let mut i = 0;
    while i < para.runs.len() {
        let run = &para.runs[i];
        match &run.link {
            Some(url) => {
                // Coalesce the contiguous run of identically-linked spans.
                let mut children: Vec<Inline> = Vec::new();
                while i < para.runs.len() && para.runs[i].link.as_deref() == Some(url.as_str()) {
                    children.extend(lower(&para.runs[i]));
                    i += 1;
                }
                runs.push(Inline::Link {
                    href: LinkTarget::Url(url.clone()),
                    children,
                });
            }
            None => {
                runs.extend(lower(run));
                i += 1;
            }
        }
    }

    Paragraph {
        style: ParagraphStyle {
            align: align_model(para.align),
            indent_left_pt: if para.indent_left > 0 {
                twips_to_pt(para.indent_left)
            } else {
                0.0
            },
            indent_right_pt: if para.indent_right > 0 {
                twips_to_pt(para.indent_right)
            } else {
                0.0
            },
            first_line_pt: twips_to_pt(para.first_line),
            space_before_pt: if para.space_before > 0 {
                twips_to_pt(para.space_before)
            } else {
                0.0
            },
            space_after_pt: if para.space_after > 0 {
                twips_to_pt(para.space_after)
            } else {
                0.0
            },
            line_height: if para.line_spacing_mult {
                LineHeight::Multiple(para.line_spacing as f64 / 240.0)
            } else if para.line_spacing > 0 {
                LineHeight::Points(twips_to_pt(para.line_spacing))
            } else {
                LineHeight::Normal
            },
            // `\rtlpar` → the model's typed writing direction.
            direction: para.rtl.then_some(crate::model::Direction::Rtl),
            ..ParagraphStyle::default()
        },
        runs,
        ..Paragraph::default()
    }
}

/// Lower one parser [`Run`] to its model inline: a comment anchor becomes the
/// zero-width [`Inline::CommentRef`], everything else an [`Inline::Run`].
fn inline_to_model(p: &Parser, run: &Run) -> Inline {
    match &run.comment_ref {
        Some(id) => Inline::CommentRef { id: id.clone() },
        None => Inline::Run(run_to_model(p, run)),
    }
}

/// Lower a single styled [`Run`] to a model [`InlineRun`].
fn run_to_model(p: &Parser, run: &Run) -> InlineRun {
    InlineRun {
        text: run.text.clone(),
        style: char_state_to_model(p, &run.style),
        source_index: None,
    }
}

/// Lower a parser [`CharState`] to a model [`CharStyle`].
fn char_state_to_model(p: &Parser, s: &CharState) -> CharStyle {
    let (family, generic) = match p.fonts.get(s.font_idx) {
        Some(f) if !f.name.is_empty() => (f.name.clone(), generic_class(f.generic)),
        Some(f) => (String::new(), generic_class(f.generic)),
        None => (String::new(), Generic::default()),
    };
    CharStyle {
        family,
        generic,
        size_pt: if s.half_points > 0 {
            s.half_points as f64 / 2.0
        } else {
            0.0
        },
        bold: s.bold,
        italic: s.italic,
        underline: s.underline,
        strike: s.strike,
        color: p.color_rgb(s.color_idx),
        background: p.color_rgb(s.highlight_idx),
        vertical_align: if s.superscript {
            crate::model::VAlign::Super
        } else if s.subscript {
            crate::model::VAlign::Sub
        } else {
            crate::model::VAlign::Baseline
        },
        ..Default::default()
    }
}

/// Lower the table model `Vec<rows of cells of paragraphs>` to a [`Table`].
fn table_to_model(p: &Parser, rows: &[Vec<Vec<Para>>]) -> Table {
    Table {
        rows: rows
            .iter()
            .map(|cells| Row {
                cells: cells
                    .iter()
                    .map(|cell| Cell {
                        blocks: cell
                            .iter()
                            .map(|para| model_block(BlockKind::Paragraph(para_to_model(p, para))))
                            .collect(),
                        ..Cell::default()
                    })
                    .collect(),
                height: None,
                // RTF has no table-header-row concept; rows import as body rows.
                is_header: false,
            })
            .collect(),
        ..Table::default()
    }
}

/// Map the parser's CSS-keyword generic bucket to the model [`Generic`] class.
fn generic_class(g: &str) -> Generic {
    match g {
        "serif" => Generic::Serif,
        "monospace" => Generic::Mono,
        // "sans-serif", "cursive" (no cursive bucket), or empty → sans default.
        _ => Generic::Sans,
    }
}

/// Map the parser's [`Align`] to the model [`Align`](crate::model::Align).
fn align_model(a: Align) -> crate::model::Align {
    match a {
        Align::Left => crate::model::Align::Left,
        Align::Center => crate::model::Align::Center,
        Align::Right => crate::model::Align::Right,
        Align::Justify => crate::model::Align::Justify,
    }
}

/// 64-bit FNV-1a content hash — a stable, dependency-free resource key (matches
/// the convention used by the image / HTML importers).
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_italic_runs_become_styled_spans() {
        let rtf = r"{\rtf1\ansi {\b gras}{\i italique} normal\par}";
        let html = rtf_to_html(rtf);
        assert!(html.contains("font-weight:bold"), "bold span: {html}");
        assert!(html.contains("gras"));
        assert!(html.contains("font-style:italic"), "italic span: {html}");
        assert!(html.contains("italique"));
        assert!(html.contains("normal"));
    }

    #[test]
    fn bold_toggle_off_with_b0() {
        let rtf = r"{\rtf1\ansi \b bold\b0  plain\par}";
        let html = rtf_to_html(rtf);
        // "bold" is inside a bold span; "plain" is not.
        let bold_pos = html.find("bold").unwrap();
        let plain_pos = html.find("plain").unwrap();
        let span_close = html[bold_pos..].find("</span>").map(|o| bold_pos + o);
        assert!(
            span_close.map(|c| c < plain_pos).unwrap_or(false),
            "the bold span must close before 'plain': {html}"
        );
    }

    #[test]
    fn color_from_colortbl_applies_hex() {
        // colortbl: index0 auto, index1 = red(255,0,0).
        let rtf = r"{\rtf1\ansi{\colortbl ;\red255\green0\blue0;}\cf1 rouge\par}";
        let html = rtf_to_html(rtf);
        assert!(html.contains("color:#ff0000"), "red color: {html}");
        assert!(html.contains("rouge"));
        // The colortbl group's text must NOT leak into the body.
        assert!(!html.contains("red255"), "colortbl leaked: {html}");
    }

    #[test]
    fn underline_and_strike() {
        let rtf = r"{\rtf1\ansi \ul souligne\ulnone  \strike barre\par}";
        let html = rtf_to_html(rtf);
        assert!(html.contains("text-decoration:underline"), "{html}");
        assert!(html.contains("souligne"));
        assert!(html.contains("text-decoration:line-through"), "{html}");
        assert!(html.contains("barre"));
    }

    #[test]
    fn font_size_from_fs() {
        let rtf = r"{\rtf1\ansi \fs48 grand\par}";
        let html = rtf_to_html(rtf);
        // 48 half-points = 24pt.
        assert!(html.contains("font-size:24.0pt"), "{html}");
    }

    #[test]
    fn paragraph_alignment_center() {
        let rtf = r"{\rtf1\ansi \qc centre\par}";
        let html = rtf_to_html(rtf);
        assert!(html.contains("text-align:center"), "{html}");
        assert!(html.contains("centre"));
    }

    #[test]
    fn cp1252_high_bytes_decode_correctly() {
        // \'80 = €, \'93/\'94 = curly double quotes, \'85 = ellipsis, \'97 = em dash.
        let rtf = r"{\rtf1\ansi \'80 \'93x\'94 \'85 \'97\par}";
        let html = rtf_to_html(rtf);
        assert!(html.contains('€'), "euro from \\'80: {html}");
        assert!(
            html.contains('\u{201C}') && html.contains('\u{201D}'),
            "curly quotes: {html}"
        );
        assert!(html.contains('…'), "ellipsis from \\'85: {html}");
        assert!(html.contains('—'), "em dash from \\'97: {html}");
    }

    #[test]
    fn cp1252_low_high_bytes_are_latin1() {
        // \'e9 = é (identical in CP1252 and Latin-1).
        let rtf = r"{\rtf1\ansi caf\'e9\par}";
        let html = rtf_to_html(rtf);
        assert!(html.contains("café"), "{html}");
    }

    #[test]
    fn table_rows_become_html_table() {
        let rtf = r"{\rtf1\ansi\trowd \cell A1\cell A2\row \trowd \cell B1\cell B2\row}";
        let html = rtf_to_html(rtf);
        assert!(html.contains("<table"), "table present: {html}");
        assert!(
            html.contains("<tr>") && html.contains("<td"),
            "rows/cells: {html}"
        );
        assert!(
            html.contains("A1") && html.contains("B2"),
            "cell text: {html}"
        );
        // Two rows → two <tr>.
        assert_eq!(html.matches("<tr>").count(), 2, "two rows: {html}");
    }

    #[test]
    fn group_stack_restores_style_on_close() {
        // Bold turned on inside a group must NOT survive the closing brace.
        let rtf = r"{\rtf1\ansi pre {\b inside} post\par}";
        let html = rtf_to_html(rtf);
        let pre = html.find("pre").unwrap();
        let post = html.find("post").unwrap();
        let bold = html.find("font-weight:bold").unwrap();
        assert!(pre < bold && bold < post, "bold scoped to group: {html}");
        // "post" should be plain text, not inside a bold span.
        let post_segment = &html[post..];
        assert!(
            !post_segment.starts_with("</span>")
                || html[..post].matches("<span").count() == html[..post].matches("</span>").count(),
            "post is not bold: {html}"
        );
    }

    #[test]
    fn fonttbl_and_unicode_escape() {
        let rtf = r"{\rtf1\ansi{\fonttbl{\f0\froman Times;}}\f0 \u233?cole\par}";
        let html = rtf_to_html(rtf);
        // \u233 = é ; the trailing '?' is the \uc1 fallback and must be skipped.
        assert!(html.contains("école"), "unicode + uc fallback: {html}");
        // Font family recovered with generic fallback.
        assert!(html.contains("Times"), "font family: {html}");
        assert!(!html.contains("Times;"), "trailing ; stripped: {html}");
    }

    #[test]
    fn star_destination_is_skipped() {
        let rtf = r"{\rtf1\ansi {\*\fldinst HYPERLINK secret}visible\par}";
        let html = rtf_to_html(rtf);
        assert!(html.contains("visible"), "{html}");
        assert!(!html.contains("secret"), "ignorable dest leaked: {html}");
    }

    // ───────────────────────── \pict image tests ───────────────────────────

    /// Hex-encode bytes (uppercase, no separators) for an RTF `\pict` payload.
    fn hex_encode(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02X}"));
        }
        s
    }

    #[test]
    fn pngblip_pict_becomes_data_uri_img() {
        // A real 2×2 PNG produced by the crate's own encoder.
        let rgba = vec![
            0u8, 0, 255, 255, /*…*/ 255, 0, 0, 255, 0, 255, 0, 255, 255, 255, 0, 255,
        ];
        let png = crate::raster::encode_png(2, 2, &rgba);
        let expected_b64 = super::super::base64(&png);
        // \picwgoal 1440 twips = 72pt ; \pichgoal 720 twips = 36pt.
        let rtf = format!(
            r"{{\rtf1\ansi before {{\pict\pngblip\picw100\pich100\picwgoal1440\pichgoal720 {}}}after\par}}",
            hex_encode(&png)
        );
        let html = rtf_to_html(&rtf);

        assert!(
            html.contains("<img src=\"data:image/png;base64,"),
            "img data URI emitted: {html}"
        );
        assert!(
            html.contains(&expected_b64),
            "exact PNG base64 round-trips into the data URI"
        );
        assert!(
            html.contains("width=\"72.0\"") && html.contains("height=\"36.0\""),
            "goal dimensions (twips→pt) applied: {html}"
        );
        // Surrounding text is preserved and in order around the image.
        let before = html.find("before").expect("before text");
        let img = html.find("<img").expect("img tag");
        let after = html.find("after").expect("after text");
        assert!(
            before < img && img < after,
            "image lands in flow order: {html}"
        );
        // The raw hex payload must NOT leak as visible body text.
        assert!(
            !html.contains(&hex_encode(&png)),
            "hex payload leaked into body: {html}"
        );
    }

    #[test]
    fn jpegblip_pict_becomes_jpeg_data_uri() {
        // Minimal bytes carrying the JPEG SOI magic (the parser only checks the
        // magic before emitting; the engine embeds JPEG verbatim downstream).
        let jpeg = [0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let rtf = format!(
            r"{{\rtf1\ansi {{\pict\jpegblip\picwgoal960\pichgoal960 {}}}\par}}",
            hex_encode(&jpeg)
        );
        let html = rtf_to_html(&rtf);
        assert!(
            html.contains("<img src=\"data:image/jpeg;base64,"),
            "jpeg data URI emitted: {html}"
        );
        assert!(
            html.contains(&super::super::base64(&jpeg)),
            "jpeg bytes base64-encoded into the URI"
        );
    }

    #[test]
    fn pict_falls_back_to_picw_pich_when_no_goal() {
        let rgba = vec![10u8; 4];
        let png = crate::raster::encode_png(1, 1, &rgba);
        // No \picwgoal/\pichgoal → fall back to \picw/\pich (taken as twips):
        // 2880 twips = 144pt.
        let rtf = format!(
            r"{{\rtf1\ansi {{\pict\pngblip\picw2880\pich2880 {}}}\par}}",
            hex_encode(&png)
        );
        let html = rtf_to_html(&rtf);
        assert!(
            html.contains("width=\"144.0\"") && html.contains("height=\"144.0\""),
            "picw/pich fallback dimensions: {html}"
        );
    }

    #[test]
    fn undecodable_metafile_and_bitmap_pictures_are_skipped() {
        // WMF/EMF and DIB/BMP blips ARE decoded now — but a payload the decoder
        // rejects (here: random bytes that are neither a metafile nor a DIB)
        // yields no <img>, no leaked hex, and leaves surrounding text intact.
        for blip in ["wmetafile8", "emfblip", "dibitmap", "wbitmap"] {
            let payload = "DEADBEEFCAFE0102";
            let rtf = format!(
                r"{{\rtf1\ansi keep {{\pict\{blip}\picwgoal500\pichgoal500 {payload}}}done\par}}"
            );
            let html = rtf_to_html(&rtf);
            assert!(
                !html.contains("<img"),
                "{blip}: must not emit <img>: {html}"
            );
            assert!(
                !html.contains(payload),
                "{blip}: hex payload leaked: {html}"
            );
            assert!(
                html.contains("keep") && html.contains("done"),
                "{blip}: surrounding text preserved: {html}"
            );
        }
    }

    #[test]
    fn pict_with_bin_payload_carrying_invalid_magic_is_skipped() {
        // A `\bin<N>` payload is now read (its N raw bytes are captured), but a
        // `\pngblip` whose bytes lack the PNG magic is dropped — and the binary
        // bytes are skipped by the scanner so surrounding text survives.
        let rtf = r"{\rtf1\ansi text {\pict\pngblip\bin4 ....}more\par}";
        let html = rtf_to_html(rtf);
        assert!(
            !html.contains("<img"),
            "invalid-magic binary pict skipped: {html}"
        );
        assert!(html.contains("text") && html.contains("more"), "{html}");
    }

    #[test]
    fn corrupt_png_pict_is_dropped() {
        // Hex that decodes but lacks the PNG magic → dropped, no <img>.
        let rtf = r"{\rtf1\ansi {\pict\pngblip\picwgoal100\pichgoal100 0011223344}\par}";
        let html = rtf_to_html(rtf);
        assert!(!html.contains("<img"), "magic-less payload dropped: {html}");
    }

    // ───────────── WMF / EMF / DIB / \bin picture decoding (#4) ──────────────

    /// Build a tiny **placeable WMF** that fills a blue polygon on a 100×100
    /// logical canvas (same record shapes the metafile module's own tests use).
    fn tiny_wmf() -> Vec<u8> {
        fn pu16(v: &mut Vec<u8>, x: u16) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        fn pu32(v: &mut Vec<u8>, x: u32) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        fn rec(out: &mut Vec<u8>, func: u16, params: &[u16]) {
            pu32(out, 3 + params.len() as u32);
            pu16(out, func);
            for p in params {
                pu16(out, *p);
            }
        }
        let blue = {
            let c = 255u32 << 16; // COLORREF 0x00bbggrr → blue
            [(c & 0xFFFF) as u16, (c >> 16) as u16]
        };
        let mut recs = Vec::new();
        rec(&mut recs, 0x020B, &[0, 0]); // SetWindowOrg
        rec(&mut recs, 0x020C, &[100, 100]); // SetWindowExt 100×100
        rec(&mut recs, 0x02FC, &[0, blue[0], blue[1], 0]); // CreateBrushIndirect (blue)
        rec(&mut recs, 0x012D, &[0]); // SelectObject(brush)
        rec(&mut recs, 0x0324, &[4, 20, 20, 80, 20, 80, 80, 20, 80]); // Polygon (square)
        rec(&mut recs, 0x0000, &[]); // EOF

        let mut v = Vec::new();
        pu32(&mut v, 0x9AC6_CDD7); // placeable key
        pu16(&mut v, 0); // hmf
        for c in [0i16, 0, 100, 100] {
            pu16(&mut v, c as u16); // bbox
        }
        pu16(&mut v, 100); // inch
        pu32(&mut v, 0); // reserved
        pu16(&mut v, 0); // checksum
        pu16(&mut v, 1); // mtType
        pu16(&mut v, 9); // mtHeaderSize
        pu16(&mut v, 0x0300);
        pu32(&mut v, 0);
        pu16(&mut v, 4);
        pu32(&mut v, 0);
        pu16(&mut v, 0);
        v.extend_from_slice(&recs);
        v
    }

    /// Build a tiny **EMF** that fills a blue ellipse on a 60×60 device canvas.
    fn tiny_emf() -> Vec<u8> {
        fn pu16(v: &mut Vec<u8>, x: u16) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        fn pu32(v: &mut Vec<u8>, x: u32) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        fn pi32(v: &mut Vec<u8>, x: i32) {
            v.extend_from_slice(&(x as u32).to_le_bytes());
        }
        fn emr(out: &mut Vec<u8>, itype: u32, body: &[u8]) {
            let mut body = body.to_vec();
            while !body.len().is_multiple_of(4) {
                body.push(0);
            }
            pu32(out, itype);
            pu32(out, 8 + body.len() as u32);
            out.extend_from_slice(&body);
        }
        let (w, h) = (60i32, 60i32);
        let mut recs = Vec::new();
        // EMR_CREATEBRUSHINDIRECT (handle 1, solid blue), then SelectObject(1).
        let mut br = Vec::new();
        pu32(&mut br, 1);
        pu32(&mut br, 0); // BS_SOLID
        pu32(&mut br, 0x00FF_0000); // COLORREF blue
        pu32(&mut br, 0);
        emr(&mut recs, 39, &br);
        let mut sel = Vec::new();
        pu32(&mut sel, 1);
        emr(&mut recs, 37, &sel);
        // EMR_ELLIPSE box (10,10)-(50,50).
        let mut el = Vec::new();
        for c in [10i32, 10, 50, 50] {
            pi32(&mut el, c);
        }
        emr(&mut recs, 42, &el);
        emr(&mut recs, 14, &[0, 0, 0, 0]); // EOF

        let mut header = Vec::new();
        for c in [0i32, 0, w - 1, h - 1] {
            pi32(&mut header, c); // rclBounds
        }
        for c in [0i32, 0, w * 26, h * 26] {
            pi32(&mut header, c); // rclFrame
        }
        pu32(&mut header, 0x464D_4520); // " EMF"
        pu32(&mut header, 0x0001_0000); // version
        pu32(&mut header, 0); // nBytes
        pu32(&mut header, 0); // nRecords
        pu16(&mut header, 0); // nHandles
        pu16(&mut header, 0); // sReserved
        pu32(&mut header, 0); // nDescription
        pu32(&mut header, 0); // offDescription
        pu32(&mut header, 0); // nPalEntries
        for c in [w, h, w, h] {
            pi32(&mut header, c); // szlDevice + szlMillimeters
        }
        let mut v = Vec::new();
        pu32(&mut v, 1); // EMR_HEADER
        pu32(&mut v, 8 + header.len() as u32);
        v.extend_from_slice(&header);
        v.extend_from_slice(&recs);
        v
    }

    /// Build a 2×2 24-bpp packed DIB (BITMAPINFOHEADER + bottom-up rows):
    /// TL red, TR green, BL blue, BR white.
    fn tiny_dib() -> Vec<u8> {
        fn pu16(v: &mut Vec<u8>, x: u16) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        fn pu32(v: &mut Vec<u8>, x: u32) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        fn pi32(v: &mut Vec<u8>, x: i32) {
            v.extend_from_slice(&(x as u32).to_le_bytes());
        }
        let mut d = Vec::new();
        pu32(&mut d, 40); // biSize
        pi32(&mut d, 2); // biWidth
        pi32(&mut d, 2); // biHeight (bottom-up)
        pu16(&mut d, 1); // biPlanes
        pu16(&mut d, 24); // biBitCount
        pu32(&mut d, 0); // BI_RGB
        pu32(&mut d, 0); // biSizeImage
        pi32(&mut d, 0);
        pi32(&mut d, 0);
        pu32(&mut d, 0);
        pu32(&mut d, 0);
        // Bottom row (BL blue, BR white), B,G,R, row padded to 4 bytes.
        d.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
        // Top row (TL red, TR green).
        d.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        d
    }

    /// Decode the PNG interned by `rtf_to_model` for the first image block and
    /// assert it carries at least one non-transparent pixel.
    fn assert_interned_png_has_pixels(doc: &Document) {
        let img = blocks(doc)
            .iter()
            .find_map(|blk| match &blk.kind {
                BlockKind::Image(i) => Some(i),
                _ => None,
            })
            .expect("a BlockKind::Image");
        let res = doc
            .resources
            .images
            .get(&img.resource)
            .expect("image bytes interned");
        assert_eq!(res.format, "png", "re-encoded to PNG");
        let decoded = crate::raster::decode_png(&res.bytes).expect("interned PNG decodes");
        assert!(decoded.width > 0 && decoded.height > 0, "non-empty raster");
        let painted = decoded.rgba.chunks_exact(4).any(|px| px[3] > 0);
        assert!(painted, "decoded PNG must have a painted (opaque) pixel");
    }

    #[test]
    fn wmetafile_pict_decodes_to_png_image() {
        let wmf = tiny_wmf();
        // Sanity: the metafile decoder itself produces a painted raster.
        let raster = super::super::metafile::decode_wmf(&wmf).expect("wmf decodes");
        assert!(
            raster.rgba.chunks_exact(4).any(|p| p[3] > 0),
            "wmf has pixels"
        );

        let rtf = format!(
            r"{{\rtf1\ansi a {{\pict\wmetafile8\picwgoal1440\pichgoal1440 {}}}b\par}}",
            hex_encode(&wmf)
        );
        let html = rtf_to_html(&rtf);
        assert!(
            html.contains("<img src=\"data:image/png;base64,"),
            "WMF re-encoded to a PNG data URI: {html}"
        );
        let (a, img, b) = (
            html.find('a').unwrap(),
            html.find("<img").expect("img"),
            html.rfind('b').unwrap(),
        );
        assert!(a < img && img < b, "image lands in flow order");
        assert_interned_png_has_pixels(&rtf_to_model(&rtf));
    }

    #[test]
    fn emfblip_pict_decodes_to_png_image() {
        let emf = tiny_emf();
        let rtf = format!(
            r"{{\rtf1\ansi {{\pict\emfblip\picwgoal960\pichgoal960 {}}}\par}}",
            hex_encode(&emf)
        );
        let html = rtf_to_html(&rtf);
        assert!(
            html.contains("<img src=\"data:image/png;base64,"),
            "EMF re-encoded to a PNG data URI: {html}"
        );
        assert_interned_png_has_pixels(&rtf_to_model(&rtf));
    }

    #[test]
    fn dibitmap_pict_decodes_to_png_image() {
        let dib = tiny_dib();
        let rtf = format!(
            r"{{\rtf1\ansi {{\pict\dibitmap0\picwgoal720\pichgoal720 {}}}\par}}",
            hex_encode(&dib)
        );
        let html = rtf_to_html(&rtf);
        assert!(
            html.contains("<img src=\"data:image/png;base64,"),
            "DIB re-encoded to a PNG data URI: {html}"
        );
        // The interned PNG must round-trip to the DIB's actual colours.
        let doc = rtf_to_model(&rtf);
        assert_interned_png_has_pixels(&doc);
        let img = blocks(&doc)
            .iter()
            .find_map(|blk| match &blk.kind {
                BlockKind::Image(i) => Some(i),
                _ => None,
            })
            .unwrap();
        let png = &doc.resources.images.get(&img.resource).unwrap().bytes;
        let dec = crate::raster::decode_png(png).expect("png decodes");
        assert_eq!((dec.width, dec.height), (2, 2), "2×2 DIB preserved");
        // Top-left pixel is red (DIB stored bottom-up; decoder flips to top-down).
        let tl = &dec.rgba[0..4];
        assert!(
            tl[0] > 200 && tl[1] < 80 && tl[2] < 80 && tl[3] == 255,
            "DIB top-left should be opaque red, got {tl:?}"
        );
    }

    #[test]
    fn pict_with_bin_form_blip_becomes_image() {
        // `\binN` carries the picture as N RAW bytes (not hex). The parser's API
        // is `&str` (the crate forbids unsafe), so the blob must be valid UTF-8;
        // we use a packed DIB whose every byte is < 0x80 AND that deliberately
        // embeds the structural bytes `{` (0x7B), `}` (0x7D), `\` (0x5C) plus NUL
        // — exactly what a naive scanner would mis-read. A correct `\bin` reader
        // captures the N bytes verbatim and jumps the scanner past them.
        //
        // 2×2 24-bpp DIB, bottom-up; pixel BGR triples chosen to contain the
        // structural bytes: BL=(0x7B,0x5C,0x7D), BR=(0x10,0x20,0x30),
        // TL=(0x40,0x50,0x60), TR=(0x01,0x02,0x03).
        let mut dib: Vec<u8> = Vec::new();
        // BITMAPINFOHEADER (all small ints / zeros → valid UTF-8).
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes()); // width
        dib.extend_from_slice(&2i32.to_le_bytes()); // height (bottom-up)
        dib.extend_from_slice(&1u16.to_le_bytes()); // planes
        dib.extend_from_slice(&24u16.to_le_bytes()); // bpp
        dib.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        dib.extend_from_slice(&[0u8; 20]); // remaining header fields (zeros)

        // Pixel rows are bottom-up; BGR triples, each row padded to 4 bytes.
        // Bottom row: BL then BR.
        dib.extend_from_slice(&[0x7B, 0x5C, 0x7D, 0x10, 0x20, 0x30, 0x00, 0x00]);
        // Top row: TL then TR.
        dib.extend_from_slice(&[0x40, 0x50, 0x60, 0x01, 0x02, 0x03, 0x00, 0x00]);
        assert!(dib.iter().all(|&b| b < 0x80), "DIB must be valid UTF-8");
        assert!(
            dib.contains(&b'{') && dib.contains(&b'}') && dib.contains(&b'\\'),
            "DIB must embed the structural bytes the scanner could mis-read"
        );

        // Assemble the RTF at the byte level, then validate it is real UTF-8.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(br"{\rtf1\ansi text {\pict\dibitmap0\bin");
        bytes.extend_from_slice(dib.len().to_string().as_bytes());
        bytes.push(b' '); // single delimiter space before the raw bytes
        bytes.extend_from_slice(&dib); // N raw bytes
        bytes.extend_from_slice(br"}more\par}");
        let rtf = String::from_utf8(bytes).expect("DIB blob keeps the RTF valid UTF-8");

        let html = rtf_to_html(&rtf);
        assert!(
            html.contains("<img src=\"data:image/png;base64,"),
            "bin-form DIB decodes + re-encodes to an <img>: {html}"
        );
        assert!(
            html.contains("text") && html.contains("more"),
            "surrounding text survives the raw `{{`/`}}`/`\\` bytes: {html}"
        );

        // The interned PNG round-trips to the DIB's top-left colour (0x60,0x50,0x40).
        let doc = rtf_to_model(&rtf);
        assert_interned_png_has_pixels(&doc);
        let img = blocks(&doc)
            .iter()
            .find_map(|blk| match &blk.kind {
                BlockKind::Image(i) => Some(i),
                _ => None,
            })
            .unwrap();
        let dec =
            crate::raster::decode_png(&doc.resources.images.get(&img.resource).unwrap().bytes)
                .expect("png decodes");
        assert_eq!((dec.width, dec.height), (2, 2));
        assert_eq!(
            &dec.rgba[0..4],
            &[0x60, 0x50, 0x40, 255],
            "TL pixel R,G,B,A"
        );
    }

    #[test]
    fn malformed_metafile_blip_is_skipped_without_panic() {
        // A `\wmetafile` blip whose payload is NOT a metafile: skipped cleanly,
        // no panic, and the rest of the document (text + a following table) is
        // intact.
        let rtf = concat!(
            r"{\rtf1\ansi keep ",
            r"{\pict\wmetafile8\picwgoal500\pichgoal500 00FF00FF00FF}",
            r"after\par",
            r"\trowd \cell X1\cell X2\row}",
        );
        let html = rtf_to_html(rtf);
        assert!(
            !html.contains("<img"),
            "undecodable metafile → no img: {html}"
        );
        assert!(
            html.contains("keep") && html.contains("after"),
            "text intact: {html}"
        );
        assert!(
            html.contains("X1") && html.contains("X2"),
            "table after still parses: {html}"
        );

        // The model path must also survive (no panic) and keep the table.
        let doc = rtf_to_model(rtf);
        assert!(
            blocks(&doc)
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Table(_))),
            "table block preserved in model"
        );
        assert!(
            !blocks(&doc)
                .iter()
                .any(|b| matches!(b.kind, BlockKind::Image(_))),
            "no image block from the undecodable metafile"
        );
    }

    // ──────────────────────── rich RTF → model (#4) ─────────────────────────

    use crate::model::{BlockKind, Document, Inline, LinkTarget};

    /// Flatten a paragraph's inline content (runs + link children) to a string.
    fn para_text(p: &crate::model::Paragraph) -> String {
        fn walk(inlines: &[Inline], out: &mut String) {
            for i in inlines {
                match i {
                    Inline::Run(r) => out.push_str(&r.text),
                    Inline::Link { children, .. } => walk(children, out),
                    _ => {}
                }
            }
        }
        let mut s = String::new();
        walk(&p.runs, &mut s);
        s.trim().to_string()
    }

    fn blocks(doc: &Document) -> &[crate::model::Block] {
        &doc.sections[0].pages[0].blocks
    }

    #[test]
    fn rtf_page_control_word_splits_model_pages_and_html() {
        let rtf = r"{\rtf1 before\page after}";
        let doc = rtf_to_model(rtf);
        assert_eq!(
            doc.sections[0].pages.len(),
            2,
            "hard page break creates a second page"
        );
        let p0 = match &doc.sections[0].pages[0].blocks[0].kind {
            BlockKind::Paragraph(p) => para_text(p),
            other => panic!("first page block not paragraph: {other:?}"),
        };
        let p1 = match &doc.sections[0].pages[1].blocks[0].kind {
            BlockKind::Paragraph(p) => para_text(p),
            other => panic!("second page block not paragraph: {other:?}"),
        };
        assert_eq!(p0, "before");
        assert_eq!(p1, "after");
        let html = rtf_to_html(rtf);
        assert!(
            html.contains("break-before:page"),
            "HTML carries a page break marker: {html}"
        );
    }

    /// The headline test: an RTF with a bold run, a coloured run, a 2-cell table
    /// and a hyperlink lowers to a model that carries the bold `CharStyle`, the
    /// colour, the `BlockKind::Table`, and the `Inline::Link` — never flat text.
    #[test]
    fn rtf_model_carries_bold_colour_table_and_link() {
        let rtf = concat!(
            r"{\rtf1\ansi{\colortbl ;\red255\green0\blue0;}",
            r"{\b gras}{\cf1 rouge} plain\par",
            r#"{\field{\*\fldinst{HYPERLINK "https://example.com"}}"#,
            r"{\fldrslt{\ul cliquez}}}\par",
            r"\trowd \cell A1\cell A2\row}",
        );
        let doc = rtf_to_model(rtf);
        let b = blocks(&doc);

        // First paragraph: a bold run carrying "gras" + a red run "rouge".
        let p0 = match &b[0].kind {
            BlockKind::Paragraph(p) => p,
            other => panic!("block 0 not a paragraph: {other:?}"),
        };
        assert!(
            p0.runs.iter().any(|i| matches!(
                i, Inline::Run(r) if r.style.bold && r.text.contains("gras"))),
            "bold CharStyle preserved: {:?}",
            p0.runs
        );
        assert!(
            p0.runs.iter().any(|i| matches!(
                i, Inline::Run(r) if r.style.color == Some([1.0, 0.0, 0.0]) && r.text.contains("rouge"))),
            "run colour (red) preserved: {:?}",
            p0.runs
        );

        // Second paragraph: the hyperlink lowered to an Inline::Link (not text).
        let p1 = match &b[1].kind {
            BlockKind::Paragraph(p) => p,
            other => panic!("block 1 not a paragraph: {other:?}"),
        };
        let link = p1
            .runs
            .iter()
            .find_map(|i| match i {
                Inline::Link { href, children } => Some((href, children)),
                _ => None,
            })
            .expect("an Inline::Link in the second paragraph");
        assert_eq!(
            link.0,
            &LinkTarget::Url("https://example.com".to_string()),
            "link target recovered"
        );
        assert_eq!(para_text(p1), "cliquez", "link wraps the visible text");

        // Third block: a 2-cell table row.
        let table = b
            .iter()
            .find_map(|blk| match &blk.kind {
                BlockKind::Table(t) => Some(t),
                _ => None,
            })
            .expect("a BlockKind::Table");
        assert_eq!(table.rows.len(), 1, "one row");
        assert_eq!(table.rows[0].cells.len(), 2, "two cells");
    }

    /// A `\pict` PNG lowers to a `BlockKind::Image` whose **bytes** are interned
    /// into the document resource table (the HTML→model path would only keep a
    /// reference — this is why RTF→model goes through the rich parser directly).
    #[test]
    fn rtf_model_interns_picture_bytes() {
        let rgba = vec![
            0u8, 0, 255, 255, 255, 0, 0, 255, 0, 255, 0, 255, 255, 255, 0, 255,
        ];
        let png = crate::raster::encode_png(2, 2, &rgba);
        let rtf = format!(
            r"{{\rtf1\ansi {{\pict\pngblip\picwgoal1440\pichgoal720 {}}}\par}}",
            hex_encode(&png)
        );
        let doc = rtf_to_model(&rtf);
        let img = blocks(&doc)
            .iter()
            .find_map(|blk| match &blk.kind {
                BlockKind::Image(i) => Some(i),
                _ => None,
            })
            .expect("a BlockKind::Image");
        let res = doc
            .resources
            .images
            .get(&img.resource)
            .expect("image bytes interned in the resource table");
        assert_eq!(res.bytes, png, "stored bytes match the source PNG");
        assert_eq!(res.format, "png");
    }

    /// A plain RTF (no styling) still imports as paragraphs of text.
    #[test]
    fn rtf_model_plain_text_still_imports() {
        let doc = rtf_to_model(r"{\rtf1\ansi Hello\par World\par}");
        let texts: Vec<String> = blocks(&doc)
            .iter()
            .filter_map(|blk| match &blk.kind {
                BlockKind::Paragraph(p) => Some(para_text(p)),
                _ => None,
            })
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(texts, vec!["Hello".to_string(), "World".to_string()]);
    }

    /// Malformed / truncated RTF must not panic — it lowers to *something*.
    #[test]
    fn rtf_model_malformed_does_not_panic() {
        for bad in [
            r"{\rtf1\ansi unclosed {\b bold",         // unbalanced braces
            r"}}}{{{\\\\",                            // brace/escape garbage
            r"{\rtf1{\field{\*\fldinst{HYPERLINK ",   // truncated hyperlink field
            r"{\rtf1\ansi {\pict\pngblip ZZZZ}\par}", // non-hex pict payload
            "",                                       // empty input
            r"{\rtf1\ansi\trowd \cell",               // open table row, no \row
            r"{\rtf1{\header truncated",              // truncated header dest
            r"{\rtf1 x{\footnote",                    // truncated footnote
            r"{\rtf1{\*\atnid jd}{\*\annotation",     // truncated annotation
            r"{\rtf1{\stylesheet{\s1\b",              // truncated stylesheet
            r"{\rtf1{\listtext",                      // truncated list marker
            r"{\rtf1{\header {\header {\header {\header {\header x}}}}}\par}", // nesting bomb
        ] {
            let _ = rtf_to_model(bad); // must return, not panic
        }
    }

    // ─────────── header / footer / footnote / annotation / styles ───────────

    /// Find the single paragraph-run text of a block list (helper for the
    /// header/footer assertions).
    fn blocks_text(blocks: &[crate::model::Block]) -> String {
        let mut out = String::new();
        for b in blocks {
            if let BlockKind::Paragraph(p) = &b.kind {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(&para_text(p));
            }
        }
        out.trim().to_string()
    }

    #[test]
    fn header_and_footer_populate_section_and_do_not_leak() {
        let rtf = r"{\rtf1\ansi{\header \pard\qc {\b Entete} societe\par}{\footer Page 1\par}Corps du texte\par}";
        let doc = rtf_to_model(rtf);

        let head = doc.sections[0].header.as_ref().expect("Section.header set");
        assert_eq!(blocks_text(head), "Entete societe");
        // Formatting survives: centred paragraph, bold lead run.
        let BlockKind::Paragraph(hp) = &head[0].kind else {
            panic!("header block is a paragraph");
        };
        assert_eq!(hp.style.align, crate::model::Align::Center);
        assert!(
            hp.runs
                .iter()
                .any(|i| matches!(i, Inline::Run(r) if r.style.bold && r.text.contains("Entete"))),
            "bold header run: {:?}",
            hp.runs
        );

        let foot = doc.sections[0].footer.as_ref().expect("Section.footer set");
        assert_eq!(blocks_text(foot), "Page 1");

        // The body carries ONLY the body text.
        let body: Vec<String> = blocks(&doc)
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Paragraph(p) => Some(para_text(p)),
                _ => None,
            })
            .filter(|t| !t.is_empty())
            .collect();
        assert_eq!(body, vec!["Corps du texte".to_string()]);

        // And the HTML path leaks neither of them.
        let html = rtf_to_html(rtf);
        assert!(!html.contains("Entete"), "header leaked into HTML: {html}");
        assert!(!html.contains("Page 1"), "footer leaked into HTML: {html}");
        assert!(html.contains("Corps du texte"), "{html}");
    }

    #[test]
    fn header_variants_fold_by_preference() {
        // default `\header` beats the left-page variant, whatever the order.
        let doc = rtf_to_model(r"{\rtf1{\headerl Gauche\par}{\header Principal\par}Body\par}");
        let head = doc.sections[0].header.as_ref().expect("header");
        assert_eq!(blocks_text(head), "Principal");

        let doc = rtf_to_model(r"{\rtf1{\header Principal\par}{\headerl Gauche\par}Body\par}");
        assert_eq!(
            blocks_text(doc.sections[0].header.as_ref().expect("header")),
            "Principal"
        );

        // With only a first-page variant, it is still recovered…
        let doc = rtf_to_model(r"{\rtf1{\headerf Premiere\par}Body\par}");
        assert_eq!(
            blocks_text(doc.sections[0].header.as_ref().expect("header")),
            "Premiere"
        );

        // …and first beats left beats right.
        let doc = rtf_to_model(
            r"{\rtf1{\footerr Droite\par}{\footerl Gauche\par}{\footerf Premiere\par}Body\par}",
        );
        assert_eq!(
            blocks_text(doc.sections[0].footer.as_ref().expect("footer")),
            "Premiere"
        );

        // An all-blank header imports as absent, not as an empty block list.
        let doc = rtf_to_model(r"{\rtf1{\header \pard\par}Body\par}");
        assert!(doc.sections[0].header.is_none(), "blank header dropped");
    }

    #[test]
    fn footnote_inlines_body_with_superscript_marker() {
        let rtf = concat!(
            r"{\rtf1\ansi Voir la note\chftn{\footnote \chftn Le corps de la note.} et la suite\par",
            "\n",
            r"Deux\chftn{\footnote \chftn Autre note.}\par}",
        );
        let doc = rtf_to_model(rtf);
        let b = blocks(&doc);

        // The model path is now TYPED: the reference point carries an
        // `Inline::FootnoteRef` (one per note — the tagged marker/body copies
        // collapse), the host text stays intact around it, and the note body
        // lives in `Document::footnotes` instead of being inlined.
        let BlockKind::Paragraph(p0) = &b[0].kind else {
            panic!("first block is a paragraph");
        };
        assert!(
            p0.runs
                .iter()
                .any(|i| matches!(i, Inline::FootnoteRef { id } if id == "1")),
            "typed anchor for note 1: {:?}",
            p0.runs
        );
        assert_eq!(
            para_text(p0),
            "Voir la note et la suite",
            "host text intact, body no longer inlined"
        );

        // Second note anchors with id 2.
        let BlockKind::Paragraph(p1) = &b[1].kind else {
            panic!("second block is a paragraph");
        };
        assert!(
            p1.runs
                .iter()
                .any(|i| matches!(i, Inline::FootnoteRef { id } if id == "2")),
            "typed anchor for note 2: {:?}",
            p1.runs
        );
        assert_eq!(para_text(p1), "Deux");

        // Both bodies are recovered as block lists on the document.
        assert_eq!(doc.footnotes.len(), 2, "two recovered notes");
        assert_eq!(doc.footnotes[0].id, "1");
        assert_eq!(doc.footnotes[1].id, "2");
        let body_text = |f: &crate::model::Footnote| {
            f.blocks
                .iter()
                .filter_map(|blk| match &blk.kind {
                    BlockKind::Paragraph(p) => Some(para_text(p)),
                    _ => None,
                })
                .collect::<String>()
        };
        assert!(
            body_text(&doc.footnotes[0]).contains("Le corps de la note."),
            "note 1 body recovered: {:?}",
            doc.footnotes[0]
        );
        assert!(
            body_text(&doc.footnotes[1]).contains("Autre note."),
            "note 2 body recovered"
        );

        // The HTML path still inlines the body (unchanged rendering — and
        // never the raw group).
        let html = rtf_to_html(rtf);
        assert!(html.contains("Le corps de la note."), "{html}");
    }

    #[test]
    fn rtlpar_sets_paragraph_direction() {
        // `\rtlpar` → `ParagraphStyle::direction = Some(Rtl)`; `\pard` resets
        // it like every other paragraph property; `\ltrpar` clears explicitly.
        let rtf = r"{\rtf1\ansi\rtlpar shalom\par\pard plain\par\rtlpar\ltrpar back\par}";
        let doc = rtf_to_model(rtf);
        // Only the content-bearing paragraphs (EOF may flush a trailing blank
        // spacing paragraph, direction-less by construction).
        let dirs: Vec<Option<crate::model::Direction>> = blocks(&doc)
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Paragraph(p) if !p.runs.is_empty() => Some(p.style.direction),
                _ => None,
            })
            .collect();
        assert_eq!(
            dirs,
            vec![Some(crate::model::Direction::Rtl), None, None],
            "rtlpar → Rtl; pard resets; ltrpar clears"
        );
    }

    #[test]
    fn annotation_maps_to_comment_and_anchor() {
        let rtf = concat!(
            r"{\rtf1\ansi Le contrat est signe.",
            r"{\*\atnid JD}{\*\atnauthor John Doe}\chatn",
            r"{\*\annotation{\*\atnref 1}\pard\plain A verifier avant envoi.\par}",
            r" Fin.\par}",
        );
        let doc = rtf_to_model(rtf);

        assert_eq!(doc.comments.len(), 1, "one recovered comment");
        let c = &doc.comments[0];
        assert_eq!(c.author, "John Doe");
        assert_eq!(c.text, "A verifier avant envoi.");
        assert!(!c.id.is_empty());
        assert_eq!(c.date, "", "RTF DTTM date is not verbatim text");

        // The anchor sits in the body flow and pairs by id.
        let BlockKind::Paragraph(p0) = &blocks(&doc)[0].kind else {
            panic!("paragraph");
        };
        let anchor = p0
            .runs
            .iter()
            .find_map(|i| match i {
                Inline::CommentRef { id } => Some(id.clone()),
                _ => None,
            })
            .expect("an Inline::CommentRef anchor");
        assert_eq!(anchor, c.id);

        // The comment body never leaks into the visible text (model or HTML).
        assert_eq!(para_text(p0), "Le contrat est signe. Fin.");
        let html = rtf_to_html(rtf);
        assert!(!html.contains("A verifier"), "body leaked: {html}");
        assert!(!html.contains("John Doe"), "author leaked: {html}");

        // Initials alone still name the author.
        let doc = rtf_to_model(
            r"{\rtf1 x{\*\atnid jd}\chatn{\*\annotation\pard Note interne.\par}y\par}",
        );
        assert_eq!(doc.comments.len(), 1);
        assert_eq!(doc.comments[0].author, "jd");
        assert_eq!(doc.comments[0].text, "Note interne.");
    }

    #[test]
    fn stylesheet_applies_paragraph_and_char_styles() {
        let rtf = concat!(
            r"{\rtf1\ansi{\colortbl ;\red255\green0\blue0;}",
            r"{\stylesheet{\ql\fs22 Normal;}{\s1\b\fs32\qc heading 1;}{\*\cs10\ul\cf1 Hyperlink;}}",
            r"\pard\s1 Titre Un\par",
            r"\pard\plain Corps normal {\cs10 lien rouge} fin\par}",
        );
        let doc = rtf_to_model(rtf);
        let b = blocks(&doc);

        // `\s1` pulls bold + 16pt (fs32) + centred from the stylesheet.
        let BlockKind::Paragraph(p0) = &b[0].kind else {
            panic!("paragraph 0");
        };
        assert_eq!(p0.style.align, crate::model::Align::Center);
        assert!(
            p0.runs.iter().any(|i| matches!(
                i,
                Inline::Run(r) if r.text.contains("Titre Un")
                    && r.style.bold
                    && (r.style.size_pt - 16.0).abs() < 0.01
            )),
            "styled heading run: {:?}",
            p0.runs
        );

        // `\cs10` pulls underline + the red colour; surrounding text is plain.
        let BlockKind::Paragraph(p1) = &b[1].kind else {
            panic!("paragraph 1");
        };
        assert!(
            p1.runs.iter().any(|i| matches!(
                i,
                Inline::Run(r) if r.text.contains("lien rouge")
                    && r.style.underline
                    && r.style.color == Some([1.0, 0.0, 0.0])
            )),
            "char-styled run: {:?}",
            p1.runs
        );
        assert!(
            p1.runs.iter().any(|i| matches!(
                i,
                Inline::Run(r) if r.text.contains("Corps normal") && !r.style.bold && !r.style.underline
            )),
            "plain run stays plain: {:?}",
            p1.runs
        );

        // Style names never leak as body text.
        let html = rtf_to_html(rtf);
        assert!(!html.contains("heading 1"), "stylesheet leaked: {html}");
        assert!(!html.contains("Hyperlink"), "stylesheet leaked: {html}");
        assert!(html.contains("Titre Un") && html.contains("lien rouge"));
    }

    // ─────────────────────────── list recovery ──────────────────────────────

    #[test]
    fn pn_bullet_list_maps_to_model_list_and_strips_marker() {
        // The classic Word `\pn` shape: `{\*\pn\pnlvlblt…}` definition plus the
        // pre-rendered `{\pntext \'b7\tab}` marker (0xB7 = • in Symbol).
        let rtf = concat!(
            r"{\rtf1\ansi{\fonttbl{\f0 Helvetica;}{\f1\fcharset2 Symbol;}}",
            r"\pard{\*\pn\pnlvlblt\pnf1\pnindent0{\pntxtb\'b7}}{\pntext\f1\'b7\tab}Premier element\par",
            r"\pard{\*\pn\pnlvlblt\pnf1\pnindent0{\pntxtb\'b7}}{\pntext\f1\'b7\tab}Second element\par",
            r"\pard Apres la liste\par}",
        );
        let doc = rtf_to_model(rtf);
        let list = blocks(&doc)
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::List(l) => Some(l),
                _ => None,
            })
            .expect("a BlockKind::List");
        assert!(!list.ordered, "bullet list");
        assert_eq!(list.items.len(), 2, "two items");
        let item_text = |i: usize| {
            let BlockKind::Paragraph(p) = &list.items[i].blocks[0].kind else {
                panic!("item paragraph");
            };
            para_text(p)
        };
        // The pre-rendered marker is stripped: the item text carries no bullet.
        assert_eq!(item_text(0), "Premier element");
        assert_eq!(item_text(1), "Second element");

        // The trailing paragraph is NOT swallowed by the list.
        assert!(
            blocks(&doc).iter().any(|b| matches!(
                &b.kind,
                BlockKind::Paragraph(p) if para_text(p) == "Apres la liste"
            )),
            "paragraph after the list stays a paragraph"
        );

        // HTML: a real <ul> with two <li>, and no literal bullet duplicate.
        let html = rtf_to_html(rtf);
        assert!(html.contains("<ul>"), "unordered list: {html}");
        assert_eq!(html.matches("<li").count(), 2, "two items: {html}");
        assert!(!html.contains('\u{2022}'), "marker not duplicated: {html}");
    }

    #[test]
    fn listtext_numbered_marker_maps_to_ordered_list() {
        // Modern Word lists reference the (skipped) \listtable via `\ls`; the
        // pre-rendered `{\listtext 1.\tab}` marker is what classifies them.
        let rtf = concat!(
            r"{\rtf1\ansi",
            r"\pard\ls1\ilvl0{\listtext 1.\tab}Un\par",
            r"\pard\ls1\ilvl0{\listtext 2.\tab}Deux\par",
            r"\pard\ls1\ilvl1{\listtext a.\tab}Sous\par}",
        );
        let doc = rtf_to_model(rtf);
        let list = blocks(&doc)
            .iter()
            .find_map(|b| match &b.kind {
                BlockKind::List(l) => Some(l),
                _ => None,
            })
            .expect("a BlockKind::List");
        assert!(list.ordered, "numbered list");
        // The alpha sub-level marker (`a.`) classifies as *ordered* too, so all
        // three items stay one list — the model's per-level ordered markers
        // re-render `a.` at level 1 on export.
        assert_eq!(list.items.len(), 3);
        assert_eq!(list.items[0].level, 0);
        assert_eq!(list.items[2].level, 1, "\\ilvl1 nesting level kept");
        let BlockKind::Paragraph(p) = &list.items[0].blocks[0].kind else {
            panic!("item paragraph");
        };
        assert_eq!(para_text(p), "Un", "marker text stripped");

        // The HTML path renders an <ol>.
        let html = rtf_to_html(rtf);
        assert!(html.contains("<ol>"), "ordered list: {html}");
        assert!(!html.contains("1.\t"), "marker not duplicated: {html}");
    }
}
