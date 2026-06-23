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
    /// `\cf` colour-table index (0 = auto / inherit).
    color_idx: usize,
    /// `\f` font-table index.
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
}

/// A recovered paragraph: alignment + indents + its styled runs.
#[derive(Debug, Clone, Default)]
struct Para {
    align: Align,
    indent_left: i32,
    first_line: i32,
    runs: Vec<Run>,
}

/// A top-level recovered block: a flowing paragraph, a table, or an image.
#[derive(Debug, Clone)]
enum RtfBlock {
    Para(Para),
    /// Rows of cells; each cell is its own list of paragraphs.
    Table(Vec<Vec<Vec<Para>>>),
    /// An embedded picture extracted from a `{\pict …}` group: the decoded raw
    /// image bytes (PNG or JPEG), the IANA subtype ("png" / "jpeg"), and the
    /// display size in CSS points (from `\picwgoal`/`\pichgoal`, else `\picw`/
    /// `\pich`). Serialized as an `<img src="data:image/…;base64,…">` so the
    /// HTML engine's existing image-embed path renders it.
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

/// A font-table entry: family name + a generic CSS family bucket.
#[derive(Debug, Clone, Default)]
struct FontEntry {
    name: String,
    /// One of "serif" / "sans-serif" / "monospace" / "cursive" — from `\froman`
    /// `\fswiss` `\fmodern` `\fscript`; empty if unspecified.
    generic: &'static str,
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
            fonts: Vec::new(),
            colors: Vec::new(),
            blocks: Vec::new(),
            cur: Para::default(),
            cur_started: false,
            in_row: false,
            row_cells: Vec::new(),
            cell_paras: Vec::new(),
        }
    }

    /// Append a decoded character to the active run (creating/extending one with
    /// the current style), unless the current group is a skipped destination.
    fn push_char(&mut self, ch: char) {
        if self.skip {
            return;
        }
        if !self.cur_started {
            self.cur.align = self.par.align;
            self.cur.indent_left = self.par.indent_left;
            self.cur.first_line = self.par.first_line;
            self.cur_started = true;
        }
        match self.cur.runs.last_mut() {
            Some(r) if r.style == self.chr => r.text.push(ch),
            _ => self.cur.runs.push(Run {
                text: ch.to_string(),
                style: self.chr.clone(),
            }),
        }
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
                    first_line: self.par.first_line,
                    runs: Vec::new(),
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
                    });
                    self.i += 1;
                }
                b'}' => {
                    if let Some(g) = self.stack.pop() {
                        self.chr = g.chr;
                        self.skip = g.skip;
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
                        self.push_char(cp1252_byte(byte));
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
            "li" => self.par.indent_left = param.unwrap_or(0) as i32,
            "ri" => self.par.indent_right = param.unwrap_or(0) as i32,
            "fi" => self.par.first_line = param.unwrap_or(0) as i32,

            // ── breaks ──
            "par" | "sect" => self.flush_para(),
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
            "stylesheet" | "info" | "object" | "header" | "footer" | "footnote"
            | "annotation" | "fldinst" | "xmlns" | "themedata" | "colorschememapping"
            | "datastore" | "latentstyles" | "listtable" | "listoverridetable" | "generator"
            | "revtbl" | "rsidtbl" => {
                self.skip = true;
            }

            _ => {}
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

    fn set_font(&mut self, idx: usize, entry: FontEntry) {
        if self.fonts.len() <= idx {
            self.fonts.resize(idx + 1, FontEntry::default());
        }
        self.fonts[idx] = entry;
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

    /// Parse a `{\pict …}` group and, for PNG/JPEG pictures, push an
    /// [`RtfBlock::Image`]. Called right after the `\pict` control word; reads
    /// ahead from `self.i` to the group close **without** advancing `self.i` —
    /// the main loop re-scans the same bytes with `skip` on, suppressing the
    /// hex payload from the body text (mirrors [`Self::read_fonttbl`]).
    ///
    /// Supported source encodings:
    /// * **hex** (RTF default): pairs of hex digits in the group text.
    /// * **`\bin<N>`**: not handled here — binary blobs can embed `{`/`}`/`\`
    ///   bytes the byte-scanner would mis-read, so such pictures are skipped
    ///   (the format is rare for PNG/JPEG, which ship hex-encoded).
    ///
    /// Supported formats: `\pngblip` (PNG) and `\jpegblip` (JPEG) are decoded
    /// and embedded. `\dibitmap`/`\wbitmap` (DIB/BMP) and `\wmetafile`/`\emfblip`
    /// (WMF/EMF) have no decoder/parser in this crate, so they are skipped.
    fn read_pict(&mut self) {
        let b = self.bytes;
        let mut p = self.i; // just after "pict"
        let mut depth = 0i32; // relative depth within the \pict group

        // Picture metadata, gathered from the control words preceding the data.
        let mut subtype: Option<&'static str> = None; // None until a known blip
        let mut is_metafile = false; // \wmetafile / \emfblip / default WMF
        let mut is_bitmap = false; // \dibitmap / \wbitmap (no decoder)
        let mut has_bin = false; // \bin<N> binary payload present → skip
        let (mut picw, mut pich) = (0i64, 0i64); // \picw / \pich (source units)
        let (mut goalw, mut goalh) = (0i64, 0i64); // \picwgoal / \pichgoal (twips)
        let (mut scalex, mut scaley) = (100i64, 100i64); // \picscalex / \picscaley (%)
        let mut hex = String::new(); // collected hex digits of the payload

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
                    let np: Option<i64> = self.src[ns..q]
                        .parse()
                        .ok()
                        .map(|n: i64| if neg { -n } else { n });

                    match w {
                        "pngblip" => subtype = Some("png"),
                        "jpegblip" => subtype = Some("jpeg"),
                        "dibitmap" | "wbitmap" => is_bitmap = true,
                        "wmetafile" | "emfblip" => is_metafile = true,
                        "bin" => has_bin = true,
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

        // No usable raster: unknown/unsupported format, or binary payload we
        // cannot safely slice from the scanned stream → drop the picture.
        let subtype = match subtype {
            Some(st) if !has_bin => st,
            _ => {
                // is_bitmap / is_metafile / has_bin are intentionally unhandled
                // (documented limits); reading the flags keeps intent explicit.
                let _ = (is_bitmap, is_metafile);
                return;
            }
        };

        let Some(data) = decode_hex(&hex) else {
            return;
        };
        // Defend against truncated/garbage payloads: require the format's magic.
        let ok = match subtype {
            "png" => data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
            "jpeg" => data.starts_with(&[0xFF, 0xD8, 0xFF]),
            _ => false,
        };
        if !ok {
            return;
        }

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

    fn color_hex(&self, idx: usize) -> Option<String> {
        // Index 0 is the "auto" colour → inherit (no explicit colour).
        if idx == 0 {
            return None;
        }
        self.colors
            .get(idx)
            .map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}"))
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
        let css = run_style(p, &run.style);
        if css.is_empty() {
            esc_html(&run.text, out);
        } else {
            out.push_str(&format!("<span style=\"{css}\">"));
            esc_html(&run.text, out);
            out.push_str("</span>");
        }
    }
    out.push_str("</p>");
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
    for block in &blocks {
        match block {
            RtfBlock::Para(para) => para_html(&parser, para, &mut html),
            RtfBlock::Table(rows) => table_html(&parser, rows, &mut html),
            RtfBlock::Image(pic) => image_html(pic, &mut html),
        }
    }
    html.push_str("</body></html>");
    html
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
        let rgba = vec![0u8, 0, 255, 255, /*…*/ 255, 0, 0, 255, 0, 255, 0, 255, 255, 255, 0, 255];
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
        assert!(before < img && img < after, "image lands in flow order: {html}");
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
    fn unsupported_metafile_and_bitmap_pictures_are_skipped() {
        // WMF/EMF (vector metafiles) and DIB/BMP have no decoder → no <img>, and
        // the hex must not leak. Surrounding text still renders.
        for blip in ["wmetafile8", "emfblip", "dibitmap", "wbitmap"] {
            let payload = "DEADBEEFCAFE0102";
            let rtf = format!(
                r"{{\rtf1\ansi keep {{\pict\{blip}\picwgoal500\pichgoal500 {payload}}}done\par}}"
            );
            let html = rtf_to_html(&rtf);
            assert!(!html.contains("<img"), "{blip}: must not emit <img>: {html}");
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
    fn pict_with_bin_payload_is_skipped() {
        // \bin<N> binary payloads are not sliced from the scanned stream → no img.
        let rtf = r"{\rtf1\ansi text {\pict\pngblip\bin4 ....}more\par}";
        let html = rtf_to_html(rtf);
        assert!(!html.contains("<img"), "binary pict skipped: {html}");
        assert!(html.contains("text") && html.contains("more"), "{html}");
    }

    #[test]
    fn corrupt_png_pict_is_dropped() {
        // Hex that decodes but lacks the PNG magic → dropped, no <img>.
        let rtf = r"{\rtf1\ansi {\pict\pngblip\picwgoal100\pichgoal100 0011223344}\par}";
        let html = rtf_to_html(rtf);
        assert!(!html.contains("<img"), "magic-less payload dropped: {html}");
    }
}
