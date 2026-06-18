//! CFF / Type2 charstring outline extraction (Adobe TN #5176/#5177) — zero
//! dependencies. Renders the glyphs of `/FontFile3` (CFF, OpenType-CFF) fonts.
//!
//! CFF packs everything into compact INDEX and DICT structures; glyph outlines
//! are Type2 charstrings — a stack machine emitting cubic Béziers with local and
//! global subroutines. We parse the structures, run the charstring, and return
//! flattened contours in font units (matching [`super::truetype`]'s interface).

use std::collections::BTreeMap;

/// A parsed CFF font program.
#[derive(Debug, Clone)]
pub struct CffFont {
    charstrings: Vec<Vec<u8>>,
    gsubrs: Vec<Vec<u8>>,
    lsubrs: Vec<Vec<u8>>,
    fd_subrs: Vec<Vec<Vec<u8>>>,
    fd_select: Vec<u8>,
    is_cid: bool,
    units_per_em: f64,
    /// String INDEX (custom strings; SID >= 391). Kept to resolve glyph names
    /// for charset → name → Unicode mapping when wrapping bare CFF in OpenType.
    strings: Vec<Vec<u8>>,
    /// `charset[gid] = sid` (the glyph name SID, or CID when CID-keyed). GID 0 is
    /// always `.notdef` (SID 0). Empty falls back to the identity charset.
    charset: Vec<u16>,
}

fn read_index(data: &[u8], pos: usize) -> (Vec<Vec<u8>>, usize) {
    if pos + 2 > data.len() {
        return (Vec::new(), pos);
    }
    let count = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
    if count == 0 {
        return (Vec::new(), pos + 2);
    }
    let off_size = *data.get(pos + 2).unwrap_or(&1) as usize;
    let mut offsets = Vec::with_capacity(count + 1);
    let mut p = pos + 3;
    for _ in 0..=count {
        let mut v = 0usize;
        for _ in 0..off_size {
            v = (v << 8) | *data.get(p).unwrap_or(&0) as usize;
            p += 1;
        }
        offsets.push(v);
    }
    let base = p - 1; // offsets are 1-based from just before the data
    let mut items = Vec::with_capacity(count);
    for w in offsets.windows(2) {
        let start = base + w[0];
        let end = base + w[1];
        items.push(data.get(start..end).unwrap_or(&[]).to_vec());
    }
    (items, base + offsets[count])
}

fn parse_real(data: &[u8], mut i: usize) -> (f64, usize) {
    let mut s = String::new();
    'outer: while i < data.len() {
        let byte = data[i];
        i += 1;
        for nibble in [byte >> 4, byte & 0x0F] {
            match nibble {
                0..=9 => s.push((b'0' + nibble) as char),
                0x0a => s.push('.'),
                0x0b => s.push('E'),
                0x0c => s.push_str("E-"),
                0x0e => s.push('-'),
                0x0f => break 'outer,
                _ => {}
            }
        }
    }
    (s.parse().unwrap_or(0.0), i)
}

/// Parse a CFF DICT into `operator → operands` (two-byte operators are
/// `0x0c00 | b1`).
fn parse_dict(data: &[u8]) -> BTreeMap<u16, Vec<f64>> {
    let mut dict = BTreeMap::new();
    let mut operands: Vec<f64> = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        match b0 {
            0..=21 => {
                let op = if b0 == 12 {
                    i += 1;
                    0x0c00 | *data.get(i).unwrap_or(&0) as u16
                } else {
                    b0 as u16
                };
                dict.insert(op, std::mem::take(&mut operands));
                i += 1;
            }
            28 => {
                let v = i16::from_be_bytes([data[i + 1], data[i + 2]]) as f64;
                operands.push(v);
                i += 3;
            }
            29 => {
                let v = i32::from_be_bytes([data[i + 1], data[i + 2], data[i + 3], data[i + 4]]);
                operands.push(v as f64);
                i += 5;
            }
            30 => {
                let (v, ni) = parse_real(data, i + 1);
                operands.push(v);
                i = ni;
            }
            32..=246 => {
                operands.push(b0 as f64 - 139.0);
                i += 1;
            }
            247..=250 => {
                let b1 = *data.get(i + 1).unwrap_or(&0) as f64;
                operands.push((b0 as f64 - 247.0) * 256.0 + b1 + 108.0);
                i += 2;
            }
            251..=254 => {
                let b1 = *data.get(i + 1).unwrap_or(&0) as f64;
                operands.push(-(b0 as f64 - 251.0) * 256.0 - b1 - 108.0);
                i += 2;
            }
            _ => i += 1,
        }
    }
    dict
}

fn subr_bias(count: usize) -> i32 {
    if count < 1240 {
        107
    } else if count < 33900 {
        1131
    } else {
        32768
    }
}

/// Count of predefined CFF Standard Strings per the spec (Adobe TN #5176
/// Appendix A): SIDs `0..391` are predefined, `391..` index the font's own
/// String INDEX. Fixed at 391 — only the names with a meaningful Unicode value
/// (SID 0..=228) are tabulated below; the stylistic remainder (small caps,
/// old-style figures, version strings) resolve to `None`, which is correct
/// because they carry no base code point.
const N_STANDARD_STRINGS: usize = 391;

// The predefined names are stored as space-separated fragments and joined at
// compile time, so each fragment stays small.
const STD_A: &str = ".notdef space exclam quotedbl numbersign dollar percent ampersand quoteright parenleft parenright asterisk plus comma hyphen period slash zero one two three four five six seven eight nine colon semicolon less equal greater question at";
const STD_B: &str = "A B C D E F G H I J K L M N O P Q R S T U V W X Y Z bracketleft backslash bracketright asciicircum underscore quoteleft";
const STD_C: &str = "a b c d e f g h i j k l m n o p q r s t u v w x y z braceleft bar braceright asciitilde";
const STD_D: &str = "exclamdown cent sterling fraction yen florin section currency quotesingle quotedblleft guillemotleft guilsinglleft guilsinglright fi fl endash dagger daggerdbl periodcentered paragraph bullet quotesinglbase quotedblbase quotedblright guillemotright ellipsis perthousand questiondown grave acute circumflex tilde macron breve dotaccent dieresis ring cedilla hungarumlaut ogonek caron emdash";
const STD_E: &str = "AE ordfeminine Lslash Oslash OE ordmasculine ae dotlessi lslash oslash oe germandbls onesuperior logicalnot mu trademark Eth onehalf plusminus Thorn onequarter divide brokenbar degree thorn threequarters twosuperior registered minus eth multiply threesuperior copyright";
const STD_F: &str = "Aacute Acircumflex Adieresis Agrave Aring Atilde Ccedilla Eacute Ecircumflex Edieresis Egrave Iacute Icircumflex Idieresis Igrave Ntilde Oacute Ocircumflex Odieresis Ograve Otilde Scaron Uacute Ucircumflex Udieresis Ugrave Yacute Ydieresis Zcaron aacute acircumflex adieresis agrave aring atilde ccedilla eacute ecircumflex edieresis egrave iacute icircumflex idieresis igrave ntilde oacute ocircumflex odieresis ograve otilde scaron uacute ucircumflex udieresis ugrave yacute ydieresis zcaron";

const STD_FRAGMENTS: [&str; 6] = [STD_A, STD_B, STD_C, STD_D, STD_E, STD_F];

/// Resolve a predefined Standard String SID to its glyph name (`None` for the
/// untabulated stylistic SIDs 229..391, which carry no base code point).
fn standard_string(sid: usize) -> Option<&'static str> {
    let mut remaining = sid;
    for frag in STD_FRAGMENTS {
        let count = frag.split(' ').count();
        if remaining < count {
            return frag.split(' ').nth(remaining);
        }
        remaining -= count;
    }
    None
}

impl CffFont {
    /// Parse a CFF font program. Returns `None` if it is not valid CFF.
    pub fn parse(data: &[u8]) -> Option<CffFont> {
        if data.len() < 4 || data[0] != 1 {
            return None; // major version 1
        }
        let hdr_size = data[2] as usize;
        let (_names, p) = read_index(data, hdr_size);
        let (top_dicts, p) = read_index(data, p);
        let (strings, p) = read_index(data, p);
        let (gsubrs, _) = read_index(data, p);
        let top = parse_dict(top_dicts.first()?);

        let cs_off = *top.get(&17)?.first()? as usize;
        let (charstrings, _) = read_index(data, cs_off);
        let num_glyphs = charstrings.len();

        // charset (top DICT op 15): maps glyph id → SID (glyph name). Absent or a
        // predefined id (0 ISOAdobe / 1 Expert / 2 ExpertSubset) → identity SIDs.
        let charset = match top.get(&15).and_then(|v| v.first()).copied() {
            Some(off) if off > 2.0 => parse_charset(data, off as usize, num_glyphs),
            _ => Vec::new(),
        };

        let units_per_em = match top.get(&0x0c07) {
            Some(m) if m.first().copied().unwrap_or(0.0).abs() > 1e-9 => 1.0 / m[0],
            _ => 1000.0,
        };

        let is_cid = top.contains_key(&0x0c1e); // ROS
        let mut lsubrs = Vec::new();
        let mut fd_subrs = Vec::new();
        let mut fd_select = Vec::new();

        if is_cid {
            // FDArray: each font DICT carries its own Private → local subrs.
            if let Some(fda) = top.get(&0x0c24).and_then(|v| v.first()) {
                let (fd_dicts, _) = read_index(data, *fda as usize);
                for fd in &fd_dicts {
                    fd_subrs.push(local_subrs(data, &parse_dict(fd)));
                }
            }
            fd_select = parse_fd_select(data, &top, num_glyphs);
        } else {
            lsubrs = local_subrs(data, &top);
        }

        Some(CffFont {
            charstrings,
            gsubrs,
            lsubrs,
            fd_subrs,
            fd_select,
            is_cid,
            units_per_em,
            strings,
            charset,
        })
    }

    /// Font design units per em.
    pub fn units_per_em(&self) -> f64 {
        self.units_per_em
    }

    /// Number of glyphs.
    pub fn num_glyphs(&self) -> u16 {
        self.charstrings.len() as u16
    }

    /// Glyph advance width in font units (from the charstring, else half em).
    pub fn advance_width(&self, gid: u16) -> f64 {
        self.run(gid)
            .map(|g| g.width)
            .unwrap_or(self.units_per_em * 0.5)
    }

    /// Flattened glyph contours in font units.
    pub fn glyph_polygons(&self, gid: u16) -> Vec<Vec<(f64, f64)>> {
        self.run(gid).map(|g| g.contours).unwrap_or_default()
    }

    /// `true` when the CFF is CID-keyed (ROS present). For CID-keyed fonts the
    /// charset holds CIDs, not name SIDs, so glyph-name resolution is unavailable.
    pub fn is_cid(&self) -> bool {
        self.is_cid
    }

    /// Resolve a String ID to its name: a predefined Adobe Standard String for
    /// `sid < 391` (TN #5176 Appendix A), otherwise an entry from this font's
    /// String INDEX (`sid - 391`). `None` if out of range or not valid UTF-8.
    pub fn sid_name(&self, sid: u16) -> Option<&str> {
        let sid = sid as usize;
        if sid < N_STANDARD_STRINGS {
            return standard_string(sid);
        }
        self.strings
            .get(sid - N_STANDARD_STRINGS)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
    }

    /// Map a glyph id to its charset SID (the glyph-name SID for name-keyed CFF,
    /// or the CID for CID-keyed CFF). GID 0 is `.notdef` (SID 0). When the font
    /// carries no explicit charset, falls back to the identity (`sid = gid`).
    pub fn gid_to_sid(&self, gid: u16) -> u16 {
        if gid == 0 {
            return 0;
        }
        self.charset
            .get(gid as usize)
            .copied()
            .unwrap_or(gid)
    }

    fn local_for(&self, gid: u16) -> &[Vec<u8>] {
        if self.is_cid {
            let fd = self.fd_select.get(gid as usize).copied().unwrap_or(0) as usize;
            self.fd_subrs.get(fd).map(|s| s.as_slice()).unwrap_or(&[])
        } else {
            &self.lsubrs
        }
    }

    fn run(&self, gid: u16) -> Option<Glyph> {
        let charstring = self.charstrings.get(gid as usize)?;
        let mut interp = Interp {
            stack: Vec::new(),
            x: 0.0,
            y: 0.0,
            contours: Vec::new(),
            current: Vec::new(),
            n_stems: 0,
            width: None,
            have_width: false,
            gsubrs: &self.gsubrs,
            lsubrs: self.local_for(gid),
            default_width: self.units_per_em * 0.5,
        };
        interp.exec(charstring, 0);
        interp.finish_contour();
        Some(Glyph {
            contours: interp.contours,
            width: interp.width.unwrap_or(interp.default_width),
        })
    }
}

struct Glyph {
    contours: Vec<Vec<(f64, f64)>>,
    width: f64,
}

fn local_subrs(data: &[u8], dict: &BTreeMap<u16, Vec<f64>>) -> Vec<Vec<u8>> {
    // Private = [size, offset]; Subrs (op 19) is relative to the Private offset.
    let private = match dict.get(&18) {
        Some(v) if v.len() == 2 => v,
        _ => return Vec::new(),
    };
    let size = private[0] as usize;
    let offset = private[1] as usize;
    let priv_data = data.get(offset..offset + size).unwrap_or(&[]);
    let priv_dict = parse_dict(priv_data);
    match priv_dict.get(&19).and_then(|v| v.first()) {
        Some(&subrs_off) => read_index(data, offset + subrs_off as usize).0,
        None => Vec::new(),
    }
}

fn parse_fd_select(data: &[u8], top: &BTreeMap<u16, Vec<f64>>, num_glyphs: usize) -> Vec<u8> {
    let mut out = vec![0u8; num_glyphs];
    let Some(&off) = top.get(&0x0c25).and_then(|v| v.first()) else {
        return out;
    };
    let pos = off as usize;
    match data.get(pos) {
        Some(0) => {
            for (g, slot) in out.iter_mut().enumerate() {
                *slot = *data.get(pos + 1 + g).unwrap_or(&0);
            }
        }
        Some(3) => {
            let n_ranges = u16::from_be_bytes([data[pos + 1], data[pos + 2]]) as usize;
            let mut p = pos + 3;
            for _ in 0..n_ranges {
                let first = u16::from_be_bytes([data[p], data[p + 1]]) as usize;
                let fd = data[p + 2];
                let next = u16::from_be_bytes([data[p + 3], data[p + 4]]) as usize;
                for slot in out.iter_mut().take(next.min(num_glyphs)).skip(first) {
                    *slot = fd;
                }
                p += 3;
            }
        }
        _ => {}
    }
    out
}

/// Parse the charset (TN #5176 §13). `charset[gid] = sid`; GID 0 (`.notdef`) is
/// implicit (SID 0) and not stored in the table. Formats: 0 = flat SID list;
/// 1/2 = ranges of consecutive SIDs (1 has u8 nLeft, 2 has u16 nLeft).
fn parse_charset(data: &[u8], pos: usize, num_glyphs: usize) -> Vec<u16> {
    let mut out = vec![0u16; num_glyphs];
    let Some(&format) = data.get(pos) else {
        return out;
    };
    let mut p = pos + 1;
    let mut gid = 1usize; // GID 0 is .notdef, not encoded.
    match format {
        0 => {
            while gid < num_glyphs && p + 1 < data.len() {
                out[gid] = u16::from_be_bytes([data[p], data[p + 1]]);
                p += 2;
                gid += 1;
            }
        }
        1 | 2 => {
            while gid < num_glyphs && p + 2 < data.len() {
                let first = u16::from_be_bytes([data[p], data[p + 1]]);
                p += 2;
                let n_left = if format == 1 {
                    let v = *data.get(p).unwrap_or(&0) as usize;
                    p += 1;
                    v
                } else {
                    let v = u16::from_be_bytes([
                        *data.get(p).unwrap_or(&0),
                        *data.get(p + 1).unwrap_or(&0),
                    ]) as usize;
                    p += 2;
                    v
                };
                for k in 0..=n_left {
                    if gid >= num_glyphs {
                        break;
                    }
                    out[gid] = first.wrapping_add(k as u16);
                    gid += 1;
                }
            }
        }
        _ => {}
    }
    out
}

struct Interp<'a> {
    stack: Vec<f64>,
    x: f64,
    y: f64,
    contours: Vec<Vec<(f64, f64)>>,
    current: Vec<(f64, f64)>,
    n_stems: usize,
    width: Option<f64>,
    have_width: bool,
    gsubrs: &'a [Vec<u8>],
    lsubrs: &'a [Vec<u8>],
    default_width: f64,
}

impl Interp<'_> {
    fn finish_contour(&mut self) {
        if self.current.len() >= 2 {
            self.contours.push(std::mem::take(&mut self.current));
        } else {
            self.current.clear();
        }
    }

    fn moveto(&mut self, dx: f64, dy: f64) {
        self.finish_contour();
        self.x += dx;
        self.y += dy;
        self.current.push((self.x, self.y));
    }

    fn lineto(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        self.current.push((self.x, self.y));
    }

    fn curveto(&mut self, dx1: f64, dy1: f64, dx2: f64, dy2: f64, dx3: f64, dy3: f64) {
        let p0 = (self.x, self.y);
        let p1 = (self.x + dx1, self.y + dy1);
        let p2 = (p1.0 + dx2, p1.1 + dy2);
        let p3 = (p2.0 + dx3, p2.1 + dy3);
        const STEPS: usize = 8;
        for i in 1..=STEPS {
            let t = i as f64 / STEPS as f64;
            let mt = 1.0 - t;
            let x = mt * mt * mt * p0.0
                + 3.0 * mt * mt * t * p1.0
                + 3.0 * mt * t * t * p2.0
                + t * t * t * p3.0;
            let y = mt * mt * mt * p0.1
                + 3.0 * mt * mt * t * p1.1
                + 3.0 * mt * t * t * p2.1
                + t * t * t * p3.1;
            self.current.push((x, y));
        }
        self.x = p3.0;
        self.y = p3.1;
    }

    fn exec(&mut self, code: &[u8], depth: usize) -> bool {
        if depth > 10 {
            return true;
        }
        let mut i = 0;
        while i < code.len() {
            let b0 = code[i];
            i += 1;
            match b0 {
                1 | 3 | 18 | 23 => {
                    // h/v stem(hm): width on first, then stem pairs.
                    if !self.have_width && self.stack.len() % 2 == 1 {
                        self.width = Some(self.default_width + self.stack.remove(0));
                    }
                    self.have_width = true;
                    self.n_stems += self.stack.len() / 2;
                    self.stack.clear();
                }
                19 | 20 => {
                    // hintmask/cntrmask: same as stem, then skip mask bytes.
                    if !self.have_width && self.stack.len() % 2 == 1 {
                        self.width = Some(self.default_width + self.stack.remove(0));
                    }
                    self.have_width = true;
                    self.n_stems += self.stack.len() / 2;
                    self.stack.clear();
                    i += self.n_stems.div_ceil(8);
                }
                21 => {
                    // rmoveto
                    self.take_width_n(2);
                    let dy = self.stack.pop().unwrap_or(0.0);
                    let dx = self.stack.pop().unwrap_or(0.0);
                    self.moveto(dx, dy);
                    self.stack.clear();
                }
                22 => {
                    // hmoveto
                    self.take_width_n(1);
                    let dx = self.stack.pop().unwrap_or(0.0);
                    self.moveto(dx, 0.0);
                    self.stack.clear();
                }
                4 => {
                    // vmoveto
                    self.take_width_n(1);
                    let dy = self.stack.pop().unwrap_or(0.0);
                    self.moveto(0.0, dy);
                    self.stack.clear();
                }
                5 => {
                    // rlineto
                    let mut j = 0;
                    while j + 1 < self.stack.len() {
                        self.lineto(self.stack[j], self.stack[j + 1]);
                        j += 2;
                    }
                    self.stack.clear();
                }
                6 | 7 => {
                    // hlineto / vlineto: alternating
                    let mut horizontal = b0 == 6;
                    for k in 0..self.stack.len() {
                        let d = self.stack[k];
                        if horizontal {
                            self.lineto(d, 0.0);
                        } else {
                            self.lineto(0.0, d);
                        }
                        horizontal = !horizontal;
                    }
                    self.stack.clear();
                }
                8 => {
                    // rrcurveto
                    let mut j = 0;
                    while j + 5 < self.stack.len() {
                        let s = &self.stack[j..j + 6];
                        self.curveto(s[0], s[1], s[2], s[3], s[4], s[5]);
                        j += 6;
                    }
                    self.stack.clear();
                }
                24 => {
                    // rcurveline
                    let mut j = 0;
                    while j + 5 < self.stack.len().saturating_sub(2) {
                        let s = &self.stack[j..j + 6];
                        self.curveto(s[0], s[1], s[2], s[3], s[4], s[5]);
                        j += 6;
                    }
                    if j + 1 < self.stack.len() {
                        self.lineto(self.stack[j], self.stack[j + 1]);
                    }
                    self.stack.clear();
                }
                25 => {
                    // rlinecurve
                    let mut j = 0;
                    while j + 1 < self.stack.len().saturating_sub(6) {
                        self.lineto(self.stack[j], self.stack[j + 1]);
                        j += 2;
                    }
                    if j + 5 < self.stack.len() {
                        let s = &self.stack[j..j + 6];
                        self.curveto(s[0], s[1], s[2], s[3], s[4], s[5]);
                    }
                    self.stack.clear();
                }
                26 => {
                    // vvcurveto
                    let mut j = 0;
                    let dx1 = if self.stack.len() % 4 == 1 {
                        j = 1;
                        self.stack[0]
                    } else {
                        0.0
                    };
                    let mut first = true;
                    while j + 3 < self.stack.len() {
                        let s = &self.stack[j..j + 4];
                        let d1x = if first { dx1 } else { 0.0 };
                        self.curveto(d1x, s[0], s[1], s[2], 0.0, s[3]);
                        first = false;
                        j += 4;
                    }
                    self.stack.clear();
                }
                27 => {
                    // hhcurveto
                    let mut j = 0;
                    let dy1 = if self.stack.len() % 4 == 1 {
                        j = 1;
                        self.stack[0]
                    } else {
                        0.0
                    };
                    let mut first = true;
                    while j + 3 < self.stack.len() {
                        let s = &self.stack[j..j + 4];
                        let d1y = if first { dy1 } else { 0.0 };
                        self.curveto(s[0], d1y, s[1], s[2], s[3], 0.0);
                        first = false;
                        j += 4;
                    }
                    self.stack.clear();
                }
                30 | 31 => {
                    // vhcurveto / hvcurveto: alternating start direction
                    let mut horizontal = b0 == 31;
                    let mut j = 0;
                    let n = self.stack.len();
                    while j + 4 <= n {
                        let remain = n - j;
                        let last = remain == 5;
                        let s = &self.stack[j..j + 4];
                        let df = if last { self.stack[j + 4] } else { 0.0 };
                        if horizontal {
                            self.curveto(s[0], 0.0, s[1], s[2], df, s[3]);
                        } else {
                            self.curveto(0.0, s[0], s[1], s[2], s[3], df);
                        }
                        horizontal = !horizontal;
                        j += 4;
                    }
                    self.stack.clear();
                }
                10 => {
                    // callsubr
                    if let Some(idx) = self.stack.pop() {
                        let bias = subr_bias(self.lsubrs.len());
                        let n = (idx as i32 + bias) as usize;
                        if let Some(sub) = self.lsubrs.get(n).cloned() {
                            if self.exec(&sub, depth + 1) {
                                return true;
                            }
                        }
                    }
                }
                29 => {
                    // callgsubr
                    if let Some(idx) = self.stack.pop() {
                        let bias = subr_bias(self.gsubrs.len());
                        let n = (idx as i32 + bias) as usize;
                        if let Some(sub) = self.gsubrs.get(n).cloned() {
                            if self.exec(&sub, depth + 1) {
                                return true;
                            }
                        }
                    }
                }
                11 => return false, // return
                14 => {
                    // endchar
                    self.take_width_n(0);
                    return true;
                }
                28 => {
                    let v = i16::from_be_bytes([code[i], code[i + 1]]) as f64;
                    self.stack.push(v);
                    i += 2;
                }
                32..=246 => self.stack.push(b0 as f64 - 139.0),
                247..=250 => {
                    let b1 = *code.get(i).unwrap_or(&0) as f64;
                    self.stack.push((b0 as f64 - 247.0) * 256.0 + b1 + 108.0);
                    i += 1;
                }
                251..=254 => {
                    let b1 = *code.get(i).unwrap_or(&0) as f64;
                    self.stack.push(-(b0 as f64 - 251.0) * 256.0 - b1 - 108.0);
                    i += 1;
                }
                255 => {
                    let v = i32::from_be_bytes([code[i], code[i + 1], code[i + 2], code[i + 3]]);
                    self.stack.push(v as f64 / 65536.0);
                    i += 4;
                }
                12 => {
                    i += 1; // escape operators (flex etc.) — skip, clear operands
                    self.stack.clear();
                }
                _ => self.stack.clear(),
            }
        }
        false
    }

    /// Pull a leading width off the stack on the first stack-clearing operator
    /// that expects exactly `expected` arguments.
    fn take_width_n(&mut self, expected: usize) {
        if self.have_width {
            return;
        }
        self.have_width = true;
        if self.stack.len() > expected {
            let w = self.stack.remove(0);
            self.width = Some(self.default_width + w);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interprets_a_simple_charstring() {
        // width=10, rmoveto(100,100), rlineto(50,0), rlineto(0,50), endchar.
        // The 32..246 single-byte range encodes only -107..=107 (value = byte
        // - 139), so all operands stay within that range.
        let n = |v: i32| (v + 139) as u8;
        let cs = vec![
            n(10),
            n(100),
            n(100),
            21, // 10 100 100 rmoveto (width 10 stripped)
            n(50),
            n(0),
            5, // 50 0 rlineto
            n(0),
            n(50),
            5,  // 0 50 rlineto
            14, // endchar
        ];
        let mut interp = Interp {
            stack: Vec::new(),
            x: 0.0,
            y: 0.0,
            contours: Vec::new(),
            current: Vec::new(),
            n_stems: 0,
            width: None,
            have_width: false,
            gsubrs: &[],
            lsubrs: &[],
            default_width: 500.0,
        };
        interp.exec(&cs, 0);
        interp.finish_contour();
        assert_eq!(interp.width, Some(510.0), "width = default + 10");
        assert_eq!(interp.contours.len(), 1);
        let c = &interp.contours[0];
        assert_eq!(c[0], (100.0, 100.0), "moveto");
        assert_eq!(c[1], (150.0, 100.0), "rlineto +50x");
        assert_eq!(c[2], (150.0, 150.0), "rlineto +50y");
    }

    #[test]
    fn rejects_non_cff() {
        assert!(CffFont::parse(b"not a cff").is_none());
    }
}
