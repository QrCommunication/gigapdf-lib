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
const STD_C: &str =
    "a b c d e f g h i j k l m n o p q r s t u v w x y z braceleft bar braceright asciitilde";
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
        self.charset.get(gid as usize).copied().unwrap_or(gid)
    }

    /// Build a glyph-**name** → glyph-id map from the charset, so a simple
    /// (name-keyed) CFF font's PDF `/Encoding` (`code → name`) can be resolved to
    /// outlines. Empty for CID-keyed fonts (their charset holds CIDs, not names).
    /// Lower glyph ids win on duplicate names (the canonical glyph for a name).
    pub fn name_to_gid_map(&self) -> BTreeMap<String, u16> {
        let mut map = BTreeMap::new();
        if self.is_cid {
            return map;
        }
        for gid in 0..self.num_glyphs() {
            let sid = self.gid_to_sid(gid);
            if let Some(name) = self.sid_name(sid) {
                map.entry(name.to_string()).or_insert(gid);
            }
        }
        map
    }

    /// Map a CID to its glyph id for a CID-keyed CFF font (the charset holds
    /// `gid → CID`, so this inverts it). Identity when the font carries no
    /// explicit charset. Returns `None` for name-keyed CFF or an unknown CID.
    pub fn gid_for_cid(&self, cid: u16) -> Option<u16> {
        if !self.is_cid {
            return None;
        }
        if self.charset.is_empty() {
            return (cid < self.num_glyphs()).then_some(cid);
        }
        self.charset
            .iter()
            .position(|&c| c == cid)
            .map(|g| g as u16)
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

    /// flex (12 35): 13 args (dx1 dy1 dx2 dy2 dx3 dy3 dx4 dy4 dx5 dy5 dx6 dy6
    /// fd) → two cubic curves. `fd` (flex depth) only affects hinting and is
    /// ignored for outline rendering.
    fn flex(&mut self) {
        let s = &self.stack;
        if s.len() < 12 {
            return;
        }
        let (a, b) = (s[0], s[1]);
        let (c, d) = (s[2], s[3]);
        let (e, f) = (s[4], s[5]);
        let (g, h) = (s[6], s[7]);
        let (k, l) = (s[8], s[9]);
        let (m, n) = (s[10], s[11]);
        self.curveto(a, b, c, d, e, f);
        self.curveto(g, h, k, l, m, n);
    }

    /// hflex (12 34): 7 args (dx1 dx2 dy2 dx3 dx4 dx5 dx6). The two curves keep
    /// the start/end on the same y; only the inner points carry the vertical
    /// excursion `dy2`, undone by `-dy2` on the second curve's mid point.
    fn hflex(&mut self) {
        let s = &self.stack;
        if s.len() < 7 {
            return;
        }
        let dx1 = s[0];
        let dx2 = s[1];
        let dy2 = s[2];
        let dx3 = s[3];
        let dx4 = s[4];
        let dx5 = s[5];
        let dx6 = s[6];
        self.curveto(dx1, 0.0, dx2, dy2, dx3, 0.0);
        self.curveto(dx4, 0.0, dx5, -dy2, dx6, 0.0);
    }

    /// hflex1 (12 36): 9 args (dx1 dy1 dx2 dy2 dx3 dx4 dx5 dy5 dx6). Start and
    /// end share the same y; the final dy closes the vertical loop:
    /// -(dy1 + dy2 + dy5).
    fn hflex1(&mut self) {
        let s = &self.stack;
        if s.len() < 9 {
            return;
        }
        let dx1 = s[0];
        let dy1 = s[1];
        let dx2 = s[2];
        let dy2 = s[3];
        let dx3 = s[4];
        let dx4 = s[5];
        let dx5 = s[6];
        let dy5 = s[7];
        let dx6 = s[8];
        self.curveto(dx1, dy1, dx2, dy2, dx3, 0.0);
        self.curveto(dx4, 0.0, dx5, dy5, dx6, -(dy1 + dy2 + dy5));
    }

    /// flex1 (12 37): 11 args (dx1 dy1 dx2 dy2 dx3 dy3 dx4 dy4 dx5 dy5 d6). The
    /// last point closes on the dominant axis: if the total |dx| > |dy| the
    /// final point is (d6, -dy_total), else (-dx_total, d6).
    fn flex1(&mut self) {
        let s = &self.stack;
        if s.len() < 11 {
            return;
        }
        let dx1 = s[0];
        let dy1 = s[1];
        let dx2 = s[2];
        let dy2 = s[3];
        let dx3 = s[4];
        let dy3 = s[5];
        let dx4 = s[6];
        let dy4 = s[7];
        let dx5 = s[8];
        let dy5 = s[9];
        let d6 = s[10];
        let dx = dx1 + dx2 + dx3 + dx4 + dx5;
        let dy = dy1 + dy2 + dy3 + dy4 + dy5;
        self.curveto(dx1, dy1, dx2, dy2, dx3, dy3);
        if dx.abs() > dy.abs() {
            self.curveto(dx4, dy4, dx5, dy5, d6, -dy);
        } else {
            self.curveto(dx4, dy4, dx5, dy5, -dx, d6);
        }
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
                    // Escape: the operator is a second byte (b1).
                    let b1 = *code.get(i).unwrap_or(&0);
                    i += 1;
                    match b1 {
                        34 => self.hflex(),
                        35 => self.flex(),
                        36 => self.hflex1(),
                        37 => self.flex1(),
                        _ => {} // other 12-x operators (arithmetic/deprecated): ignore.
                    }
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

/// Encode a CFF INDEX (count, offSize, offsets, data) for `items`. Test-only:
/// assumes each item and the running offset fit in a single byte.
#[cfg(test)]
pub(crate) fn test_index(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![(items.len() >> 8) as u8, items.len() as u8];
    if items.is_empty() {
        return out;
    }
    out.push(1); // offSize = 1 byte (small test data)
    let mut off = 1u8;
    out.push(off);
    for it in items {
        off += it.len() as u8;
        out.push(off);
    }
    for it in items {
        out.extend_from_slice(it);
    }
    out
}

/// Build a minimal name-keyed CFF: `.notdef` (empty) plus one glyph whose
/// charset SID is `sid_a` (e.g. SID 34 names it "A"), drawn as a square outline
/// so it produces real ink. The Top DICT offsets are back-patched once the
/// layout stabilises. Returns the CFF bytes. Test-only fixture builder, shared
/// with the document-level render test.
#[cfg(test)]
pub(crate) fn tiny_named_cff(sid_a: u16) -> Vec<u8> {
    // Type2 charstring number: single byte (-107..=107), else the 3-byte
    // `255 + 16.16 fixed` form is overkill — use the 2-byte 28/hi/lo short int.
    let cn = |v: i32, out: &mut Vec<u8>| {
        if (-107..=107).contains(&v) {
            out.push((v + 139) as u8);
        } else {
            out.push(28);
            out.push((v >> 8) as u8);
            out.push((v & 0xFF) as u8);
        }
    };
    let notdef = vec![14u8]; // .notdef: just endchar — no outline.
                             // 'A': an outer square with an inner counter wound the opposite way, so
                             // the non-zero fill leaves a hole — a genuinely non-uniform glyph (not a
                             // solid box). All moves/lines are relative (rmoveto=21, rlineto=5).
    let glyph_a = {
        let mut g = Vec::new();
        // Outer contour (CCW): (40,40)→(240,40)→(240,240)→(40,240).
        cn(40, &mut g);
        cn(40, &mut g);
        g.push(21); // rmoveto
        cn(200, &mut g);
        cn(0, &mut g);
        g.push(5); // rlineto →(240,40)
        cn(0, &mut g);
        cn(200, &mut g);
        g.push(5); // rlineto →(240,240)
        cn(-200, &mut g);
        cn(0, &mut g);
        g.push(5); // rlineto →(40,240)
                   // Inner counter (CW): move from (40,240) to (100,100), then a small square.
        cn(60, &mut g);
        cn(-140, &mut g);
        g.push(21); // rmoveto →(100,100)
        cn(0, &mut g);
        cn(80, &mut g);
        g.push(5); // rlineto →(100,180)
        cn(80, &mut g);
        cn(0, &mut g);
        g.push(5); // rlineto →(180,180)
        cn(0, &mut g);
        cn(-80, &mut g);
        g.push(5); // rlineto →(180,100)
        g.push(14); // endchar
        g
    };
    let charstrings = test_index(&[notdef, glyph_a]);

    // Top DICT integer operand encoding (TN #5176 §4): single byte for
    // -107..=107, two bytes for 108..=1131, else the 3-byte `28 hi lo` form.
    let enc_int = |v: i32| -> Vec<u8> {
        if (-107..=107).contains(&v) {
            vec![(v + 139) as u8]
        } else if (108..=1131).contains(&v) {
            let v = v - 108;
            vec![247 + (v >> 8) as u8, (v & 0xFF) as u8]
        } else {
            vec![28, (v >> 8) as u8, (v & 0xFF) as u8]
        }
    };

    let header = vec![1u8, 0, 4, 1];
    let names = test_index(&[b"F".to_vec()]);
    let strings = test_index(&[]); // no custom strings — SID is a Standard String
    let gsubrs = test_index(&[]);
    // charset format 0: one SID (for gid 1); gid 0 is implicitly .notdef.
    let charset = vec![0u8, (sid_a >> 8) as u8, sid_a as u8];
    // Minimal Private DICT: defaultWidthX(20)=0, nominalWidthX(21)=0.
    let private = [enc_int(0), vec![20u8], enc_int(0), vec![21u8]].concat();

    let build_top = |charset_off: i32, cs_off: i32, priv_off: i32| -> Vec<u8> {
        let mut d = Vec::new();
        d.extend(enc_int(charset_off));
        d.push(15); // charset
        d.extend(enc_int(cs_off));
        d.push(17); // CharStrings
        d.extend(enc_int(private.len() as i32));
        d.extend(enc_int(priv_off));
        d.push(18); // Private = [size offset]
        d
    };

    // The Top DICT INDEX size depends on the offsets it encodes, which depend on
    // its own size — iterate to a fixed point.
    let mut top_index = test_index(&[build_top(0, 0, 0)]);
    let mut prev_len = 0;
    loop {
        let base = header.len() + names.len() + top_index.len() + strings.len() + gsubrs.len();
        let charset_off = base as i32;
        let cs_off = (base + charset.len()) as i32;
        let priv_off = (base + charset.len() + charstrings.len()) as i32;
        top_index = test_index(&[build_top(charset_off, cs_off, priv_off)]);
        if top_index.len() == prev_len {
            break;
        }
        prev_len = top_index.len();
    }

    let mut out = Vec::new();
    out.extend(header);
    out.extend(names);
    out.extend(top_index);
    out.extend(strings);
    out.extend(gsubrs);
    out.extend(charset);
    out.extend(charstrings);
    out.extend(private);
    out
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

    /// Build a fresh interpreter for charstring-level unit tests.
    fn interp() -> Interp<'static> {
        Interp {
            stack: Vec::new(),
            x: 0.0,
            y: 0.0,
            contours: Vec::new(),
            current: Vec::new(),
            n_stems: 0,
            width: None,
            have_width: true, // skip width-stripping in flex unit tests
            gsubrs: &[],
            lsubrs: &[],
            default_width: 0.0,
        }
    }

    /// Run flex with explicit operands and return the emitted contour points
    /// (after the leading moveto vertex).
    fn run_flex(operands: &[f64], op12: u8) -> Vec<(f64, f64)> {
        let mut it = interp();
        // Seed a start point so the curve has somewhere to begin.
        it.current.push((0.0, 0.0));
        it.stack = operands.to_vec();
        it.exec(&[12, op12], 0);
        it.current
    }

    /// Reference cubic flattening matching Interp::curveto (8 steps), used to
    /// derive the EXPECTED points independently from absolute control points.
    fn flatten(start: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64)) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        const STEPS: usize = 8;
        for i in 1..=STEPS {
            let t = i as f64 / STEPS as f64;
            let mt = 1.0 - t;
            let x = mt * mt * mt * start.0
                + 3.0 * mt * mt * t * p1.0
                + 3.0 * mt * t * t * p2.0
                + t * t * t * p3.0;
            let y = mt * mt * mt * start.1
                + 3.0 * mt * mt * t * p1.1
                + 3.0 * mt * t * t * p2.1
                + t * t * t * p3.1;
            out.push((x, y));
        }
        out
    }

    fn approx_eq(a: &[(f64, f64)], b: &[(f64, f64)]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(p, q)| (p.0 - q.0).abs() < 1e-6 && (p.1 - q.1).abs() < 1e-6)
    }

    #[test]
    fn flex_emits_two_cubic_curves() {
        // flex (12 35): two curves with explicit relative deltas; fd ignored.
        let ops = [10.0, 0.0, 20.0, 30.0, 10.0, 0.0, 10.0, 0.0, 20.0, -30.0, 10.0, 0.0, 50.0];
        let pts = run_flex(&ops, 35);
        // Curve 1 absolute control points from start (0,0).
        let c1 = flatten((0.0, 0.0), (10.0, 0.0), (30.0, 30.0), (40.0, 30.0));
        // Curve 2 begins at end of curve 1 (40,30).
        let c2 = flatten((40.0, 30.0), (50.0, 30.0), (70.0, 0.0), (80.0, 0.0));
        let mut expected = vec![(0.0, 0.0)];
        expected.extend(c1);
        expected.extend(c2);
        assert!(approx_eq(&pts, &expected), "flex points\n got {pts:?}\n exp {expected:?}");
        // End point must be exactly (80, 0): start + total dx, y returned to 0.
        assert_eq!(*pts.last().unwrap(), (80.0, 0.0));
    }

    #[test]
    fn hflex_keeps_endpoints_on_baseline() {
        // hflex (12 34): dx1 dx2 dy2 dx3 dx4 dx5 dx6
        let ops = [10.0, 20.0, 15.0, 10.0, 10.0, 20.0, 10.0];
        let pts = run_flex(&ops, 34);
        let c1 = flatten((0.0, 0.0), (10.0, 0.0), (30.0, 15.0), (40.0, 15.0));
        let c2 = flatten((40.0, 15.0), (50.0, 15.0), (70.0, 0.0), (80.0, 0.0));
        let mut expected = vec![(0.0, 0.0)];
        expected.extend(c1);
        expected.extend(c2);
        assert!(approx_eq(&pts, &expected), "hflex points");
        // Endpoint returns to baseline y = 0.
        assert!((pts.last().unwrap().1).abs() < 1e-9, "hflex ends on y=0");
    }

    #[test]
    fn hflex1_closes_vertical_loop() {
        // hflex1 (12 36): dx1 dy1 dx2 dy2 dx3 dx4 dx5 dy5 dx6
        let ops = [10.0, 5.0, 20.0, 10.0, 10.0, 10.0, 20.0, -8.0, 10.0];
        let pts = run_flex(&ops, 36);
        // dy_close = -(5 + 10 + (-8)) = -7  → end y = 0.
        let c1 = flatten((0.0, 0.0), (10.0, 5.0), (30.0, 15.0), (40.0, 15.0));
        let c2 = flatten((40.0, 15.0), (50.0, 15.0), (70.0, 7.0), (80.0, 0.0));
        let mut expected = vec![(0.0, 0.0)];
        expected.extend(c1);
        expected.extend(c2);
        assert!(approx_eq(&pts, &expected), "hflex1 points\n got {pts:?}\n exp {expected:?}");
        assert!((pts.last().unwrap().1).abs() < 1e-9, "hflex1 ends on starting y");
    }

    #[test]
    fn flex1_closes_on_dominant_axis() {
        // flex1 (12 37): horizontal-dominant case → last point = (d6, -dy_total).
        // dx_total = 10+20+10+10+20 = 70, dy_total = 0+10+0+0+(-10) = 0 → |dx|>|dy|.
        let ops = [10.0, 0.0, 20.0, 10.0, 10.0, 0.0, 10.0, 0.0, 20.0, -10.0, 10.0];
        let pts = run_flex(&ops, 37);
        let c1 = flatten((0.0, 0.0), (10.0, 0.0), (30.0, 10.0), (40.0, 10.0));
        // last delta = (d6=10, -dy_total=0) → from (50,10) by (50,? ) ... compute:
        // p4 = (40+10,10+0)=(50,10); p5=(50+20,10-10)=(70,0); p6=(70+10,0+0)=(80,0)
        let c2 = flatten((40.0, 10.0), (50.0, 10.0), (70.0, 0.0), (80.0, 0.0));
        let mut expected = vec![(0.0, 0.0)];
        expected.extend(c1);
        expected.extend(c2);
        assert!(approx_eq(&pts, &expected), "flex1 points\n got {pts:?}\n exp {expected:?}");
        assert_eq!(*pts.last().unwrap(), (80.0, 0.0));
    }

    #[test]
    fn flex1_vertical_dominant_branch() {
        // Vertical-dominant: dx_total small, dy_total large → last = (-dx_total, d6).
        // dx_total = 0+5+0+0+(-5) = 0, dy_total = 10+20+10+10+20 = 70 → |dy|>|dx|.
        let ops = [0.0, 10.0, 5.0, 20.0, 0.0, 10.0, 0.0, 10.0, -5.0, 20.0, 12.0];
        let pts = run_flex(&ops, 37);
        let c1 = flatten((0.0, 0.0), (0.0, 10.0), (5.0, 30.0), (5.0, 40.0));
        // p4 = (5+dx4, 40+dy4) = (5, 50); p5 = (5+dx5, 50+dy5) = (0, 70);
        // last delta = (-dx_total=0, d6=12) → p6 = (0, 82).
        let c2 = flatten((5.0, 40.0), (5.0, 50.0), (0.0, 70.0), (0.0, 82.0));
        let mut expected = vec![(0.0, 0.0)];
        expected.extend(c1);
        expected.extend(c2);
        assert!(approx_eq(&pts, &expected), "flex1 vertical points\n got {pts:?}\n exp {expected:?}");
        assert_eq!(*pts.last().unwrap(), (0.0, 82.0));
    }

    #[test]
    fn non_flex_charstring_unaffected() {
        // Regression: a glyph with rrcurveto (no flex) must still flatten to the
        // same point count and endpoint as before the flex change.
        let n = |v: i32| (v + 139) as u8;
        let cs = vec![
            n(0),
            n(0),
            21, // rmoveto to (0,0)
            n(10),
            n(20),
            n(30),
            n(40),
            n(50),
            n(0),
            8,  // rrcurveto: one cubic
            14, // endchar
        ];
        let mut it = interp();
        it.exec(&cs, 0);
        it.finish_contour();
        assert_eq!(it.contours.len(), 1);
        let c = &it.contours[0];
        // 1 moveto vertex + 8 flattening steps.
        assert_eq!(c.len(), 9, "rrcurveto unaffected by flex addition");
        // Endpoint = (0+10+30+50, 0+20+40+0) = (90, 60).
        assert_eq!(*c.last().unwrap(), (90.0, 60.0));
    }

    #[test]
    fn rejects_non_cff() {
        assert!(CffFont::parse(b"not a cff").is_none());
    }

    #[test]
    fn tiny_cff_parses_and_resolves_names_and_unicode() {
        // SID 34 names "A" in this build's Standard Strings table.
        let bytes = tiny_named_cff(34);
        let cff = CffFont::parse(&bytes).expect("hand-built CFF must parse");
        assert_eq!(cff.num_glyphs(), 2, ".notdef + A");
        assert!(!cff.is_cid(), "name-keyed");
        assert_eq!(
            cff.sid_name(cff.gid_to_sid(1)),
            Some("A"),
            "gid 1 is named A"
        );

        // The fix's resolution maps: name "A" → gid 1, and Unicode U+0041 → gid 1.
        let n2g = cff.name_to_gid_map();
        assert_eq!(n2g.get("A").copied(), Some(1), "name→gid resolves A");
        let u2g = crate::font::cff_to_otf::cff_unicode_to_gid(&cff);
        assert_eq!(u2g.get(&0x41).copied(), Some(1), "unicode→gid resolves A");

        // The glyph has real outline ink (a square), not an empty/notdef shape.
        assert!(
            !cff.glyph_polygons(1).is_empty(),
            "glyph A produces contours"
        );
        assert!(cff.glyph_polygons(0).is_empty(), ".notdef is empty");
    }
}
