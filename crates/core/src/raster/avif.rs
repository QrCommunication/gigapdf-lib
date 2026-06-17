//! AVIF decoder — ISOBMFF container + AV1 keyframe (intra) decode.
//!
//! AVIF wraps a single AV1 keyframe (the primary item) in an ISO-BMFF container.
//! This module is built in validated layers (the AV1 codec is large): **this
//! file** is the container + OBU layer — it parses the box tree, locates the
//! primary item's coded data via `iloc`, and splits the AV1 OBU stream. The AV1
//! pixel decoder (sequence/frame headers, the multi-symbol entropy decoder,
//! intra prediction, transforms, CDEF, loop restoration) lands in `avif/` and is
//! translated from the AV1 spec + dav1d (BSD), with tables sourced
//! deterministically — never fabricated.

// WIP: the OBU constants/accessors below are consumed by the AV1 pixel decoder
// (built incrementally in follow-up iterations); allow dead code until it lands.
#![allow(dead_code)]

pub(crate) mod cdef;
pub(crate) mod cdf;
pub(crate) mod deblock;
pub(crate) mod itx;
pub(crate) mod msac;
pub(crate) mod predict;
pub(crate) mod scan;
pub(crate) mod tile;

/// One parsed Open Bitstream Unit.
#[derive(Debug)]
pub(crate) struct Obu<'a> {
    pub kind: u8,
    pub data: &'a [u8],
}

// AV1 OBU types.
pub(crate) const OBU_SEQUENCE_HEADER: u8 = 1;
pub(crate) const OBU_FRAME_HEADER: u8 = 3;
pub(crate) const OBU_TILE_GROUP: u8 = 4;
pub(crate) const OBU_FRAME: u8 = 6;

/// Read an unsigned LEB128 from `d[*pos..]`, advancing `pos`. AV1 caps it at 8
/// bytes. Returns `None` on truncation.
fn leb128(d: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    for i in 0..8 {
        let b = *d.get(*pos)?;
        *pos += 1;
        value |= ((b & 0x7f) as u64) << (i * 7);
        if b & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Split a raw AV1 OBU stream into its OBUs (low-overhead bitstream format with
/// `obu_has_size_field` set, as written inside an AVIF `mdat`).
pub(crate) fn split_obus(stream: &[u8]) -> Option<Vec<Obu<'_>>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < stream.len() {
        let header = stream[pos];
        if header & 0x80 != 0 {
            return None; // forbidden bit set
        }
        let kind = (header >> 3) & 0x0f;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        pos += 1;
        if extension {
            pos += 1; // extension header byte
        }
        let size = if has_size {
            leb128(stream, &mut pos)? as usize
        } else {
            stream.len() - pos
        };
        let data = stream.get(pos..pos + size)?;
        out.push(Obu { kind, data });
        pos += size;
    }
    Some(out)
}

// ── ISOBMFF box walk ──────────────────────────────────────────────────────────

fn be32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_be_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

/// Read a sized field of `n` bytes (big-endian) at `*o`, advancing it.
fn be_n(d: &[u8], o: &mut usize, n: usize) -> Option<u64> {
    let mut v = 0u64;
    for _ in 0..n {
        v = (v << 8) | *d.get(*o)? as u64;
        *o += 1;
    }
    Some(v)
}

/// Find the byte range `[start, end)` of the first child box of `type tag`
/// within `[off, end)`. Returns the box *payload* range (after the 8/16-byte
/// header), and whether it is a FullBox-style container is left to the caller.
fn find_box(d: &[u8], mut off: usize, end: usize, tag: &[u8; 4]) -> Option<(usize, usize)> {
    while off + 8 <= end {
        let size = be32(d, off)? as usize;
        let typ = &d[off + 4..off + 8];
        let (hdr, box_end) = if size == 1 {
            let large = be_n(d, &mut { off + 8 }, 8)? as usize;
            (16, off + large)
        } else if size == 0 {
            (8, end)
        } else {
            (8, off + size)
        };
        if box_end > end || box_end <= off {
            return None;
        }
        if typ == tag {
            return Some((off + hdr, box_end));
        }
        off = box_end;
    }
    None
}

struct Item {
    id: u32,
    /// File offset + length of the item's coded data (single contiguous extent).
    offset: usize,
    length: usize,
}

/// Parse the `iloc` box (versions 0/1/2) into per-item file extents. Only the
/// single-extent, file-offset (`construction_method == 0`) case AVIF encoders
/// emit is supported; others yield an item with `length == 0`.
fn parse_iloc(d: &[u8], start: usize, _end: usize) -> Option<Vec<Item>> {
    let mut o = start;
    let version = *d.get(o)?;
    o += 4; // version (1) + flags (3)
    let sizes = *d.get(o)?;
    let offset_size = (sizes >> 4) as usize;
    let length_size = (sizes & 0x0f) as usize;
    o += 1;
    let bsizes = *d.get(o)?;
    let base_offset_size = (bsizes >> 4) as usize;
    let index_size = if version == 1 || version == 2 {
        (bsizes & 0x0f) as usize
    } else {
        0
    };
    o += 1;
    let item_count = if version < 2 {
        be_n(d, &mut o, 2)?
    } else {
        be_n(d, &mut o, 4)?
    };
    let mut items = Vec::new();
    for _ in 0..item_count {
        let id = if version < 2 {
            be_n(d, &mut o, 2)?
        } else {
            be_n(d, &mut o, 4)?
        } as u32;
        let construction_method = if version == 1 || version == 2 {
            be_n(d, &mut o, 2)? & 0x0f
        } else {
            0
        };
        o += 2; // data_reference_index
        let base_offset = be_n(d, &mut o, base_offset_size)? as usize;
        let extent_count = be_n(d, &mut o, 2)?;
        let mut item = Item {
            id,
            offset: 0,
            length: 0,
        };
        for e in 0..extent_count {
            if index_size > 0 {
                o += index_size; // extent_index
            }
            let ext_off = be_n(d, &mut o, offset_size)? as usize;
            let ext_len = be_n(d, &mut o, length_size)? as usize;
            // Only the first file-offset extent is taken (AVIF primary items are
            // single-extent, construction_method 0).
            if e == 0 && construction_method == 0 {
                item.offset = base_offset + ext_off;
                item.length = ext_len;
            }
        }
        items.push(item);
    }
    Some(items)
}

/// Extract the primary item's raw AV1 OBU byte stream from an AVIF file.
pub(crate) fn extract_av1_stream(avif: &[u8]) -> Option<Vec<u8>> {
    if avif.len() < 16 || &avif[4..8] != b"ftyp" {
        return None;
    }
    let (meta_start, meta_end) = find_box(avif, 0, avif.len(), b"meta")?;
    // meta is a FullBox: skip its 4-byte version/flags before the child boxes.
    let children = meta_start + 4;

    // primary item id
    let (pitm_s, _) = find_box(avif, children, meta_end, b"pitm")?;
    let pitm_ver = *avif.get(pitm_s)?;
    let mut po = pitm_s + 4;
    let primary_id = be_n(avif, &mut po, if pitm_ver == 0 { 2 } else { 4 })? as u32;

    // item locations
    let (iloc_s, iloc_e) = find_box(avif, children, meta_end, b"iloc")?;
    let items = parse_iloc(avif, iloc_s, iloc_e)?;
    let item = items.iter().find(|i| i.id == primary_id)?;
    if item.length == 0 {
        return None;
    }
    avif.get(item.offset..item.offset + item.length).map(|s| s.to_vec())
}

/// Decode an AVIF file to `(width, height, rgba)`. The container + OBU layer is
/// complete; the AV1 pixel decoder is built incrementally (returns `None` until
/// it lands).
pub fn decode_avif(avif: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let stream = extract_av1_stream(avif)?;
    let obus = split_obus(&stream)?;
    let seq = parse_sequence_header(obus.iter().find(|o| o.kind == OBU_SEQUENCE_HEADER)?.data)?;
    // The frame header opens the OBU_FRAME payload (header + tile group) or a
    // standalone OBU_FRAME_HEADER; the tile data follows the byte-aligned header.
    let frame = obus
        .iter()
        .find(|o| o.kind == OBU_FRAME || o.kind == OBU_FRAME_HEADER)?;
    let fh = parse_frame_header(&seq, frame.data)?;
    let off = tile::tile_data_offset(fh.header_bits);
    if off >= frame.data.len() {
        return None;
    }
    // MiCols/MiRows (AV1 §5.9.2): 4×4 units rounded up to the 8-pixel grid.
    let mi_cols = 2 * ((fh.frame_width + 7) >> 3);
    let mi_rows = 2 * ((fh.frame_height + 7) >> 3);
    let mut tile = tile::Av1Tile::new(&frame.data[off..], mi_cols, mi_rows, &seq, &fh);
    tile.decode();
    // NOTE: in-loop filters (deblock, CDEF, loop-restoration, film-grain) are not
    // yet applied — fine for the still-picture fixture (its YUV reference is the
    // raw intra reconstruction), a documented gap for arbitrary AVIFs.
    let (w, h) = (fh.frame_width as usize, fh.frame_height as usize);
    let rgba = yuv_to_rgba(&tile, &seq, w, h);
    Some((fh.frame_width, fh.frame_height, rgba))
}

/// Clamp a float colour component to a `u8` (round-to-nearest, saturating).
#[inline]
fn to_u8(v: f32) -> u8 {
    let r = v.round();
    if r <= 0.0 {
        0
    } else if r >= 255.0 {
        255
    } else {
        r as u8
    }
}

/// Convert the decoded YUV planes to a cropped `w*h` RGBA8888 buffer (8-bit).
/// Chroma is nearest-neighbour upsampled per the sequence subsampling; the matrix
/// (BT.601/709/2020-NCL/Identity) and range (limited/full) come from the sequence
/// header. ITU-R YCbCr→RGB constants (not codec tables).
fn yuv_to_rgba(tile: &tile::Av1Tile, seq: &SequenceHeader, w: usize, h: usize) -> Vec<u8> {
    let (y_buf, yw, _yh) = tile.plane(0);
    let full = seq.color_range != 0;
    let mut out = vec![0u8; w * h * 4];

    // Luma → full-range RGB level (shared by mono + the matrix path).
    let lift = |yv: f32| if full { yv } else { (yv - 16.0) * (255.0 / 219.0) };

    if seq.mono_chrome {
        for py in 0..h {
            for px in 0..w {
                let l = to_u8(lift(y_buf[py * yw + px] as f32));
                let o = (py * w + px) * 4;
                out[o] = l;
                out[o + 1] = l;
                out[o + 2] = l;
                out[o + 3] = 255;
            }
        }
        return out;
    }

    let (u_buf, uw, _) = tile.plane(1);
    let (v_buf, _vw, _) = tile.plane(2);

    // Identity matrix: the "YUV" planes carry G/B/R directly (lossless RGB AVIF).
    if seq.matrix_coefficients == 0 {
        for py in 0..h {
            for px in 0..w {
                let o = (py * w + px) * 4;
                out[o] = v_buf[py * uw + px]; // R
                out[o + 1] = y_buf[py * yw + px]; // G
                out[o + 2] = u_buf[py * uw + px]; // B
                out[o + 3] = 255;
            }
        }
        return out;
    }

    let (ss_h, ss_v) = (seq.subsampling_x as usize, seq.subsampling_y as usize);
    let (kr, kb): (f32, f32) = match seq.matrix_coefficients {
        1 => (0.2126, 0.0722), // BT.709
        9 => (0.2627, 0.0593), // BT.2020 non-constant-luminance
        _ => (0.299, 0.114),   // BT.601 (6) / unspecified default
    };
    let kg = 1.0 - kr - kb;
    let (cr_r, cb_b) = (2.0 * (1.0 - kr), 2.0 * (1.0 - kb));
    let (cr_g, cb_g) = (2.0 * kr * (1.0 - kr) / kg, 2.0 * kb * (1.0 - kb) / kg);
    let cscale = if full { 1.0 } else { 255.0 / 224.0 };

    for py in 0..h {
        let cy = py >> ss_v;
        for px in 0..w {
            let cx = px >> ss_h;
            let yl = lift(y_buf[py * yw + px] as f32);
            let u = (u_buf[cy * uw + cx] as f32 - 128.0) * cscale;
            let v = (v_buf[cy * uw + cx] as f32 - 128.0) * cscale;
            let o = (py * w + px) * 4;
            out[o] = to_u8(yl + cr_r * v);
            out[o + 1] = to_u8(yl - cb_g * u - cr_g * v);
            out[o + 2] = to_u8(yl + cb_b * u);
            out[o + 3] = 255;
        }
    }
    out
}

// ── AV1 bit reader (MSB-first, used for the uncompressed headers) ─────────────

pub(crate) struct BitReader<'a> {
    d: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(d: &'a [u8]) -> Self {
        BitReader { d, bit: 0 }
    }
    /// `f(n)` in the AV1 spec: read `n` bits big-endian (MSB first).
    pub fn f(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.bit >> 3;
            let off = 7 - (self.bit & 7);
            let b = self.d.get(byte).map_or(0, |x| (x >> off) & 1);
            v = (v << 1) | b as u32;
            self.bit += 1;
        }
        v
    }
    /// `su(n)`: `n`-bit two's-complement signed (AV1 spec §4.10.6).
    pub fn su(&mut self, n: u32) -> i32 {
        let v = self.f(n) as i32;
        let sign = 1i32 << (n - 1);
        if v & sign != 0 {
            v - (1 << n)
        } else {
            v
        }
    }
    /// Current bit position (for byte-alignment after the header).
    pub fn pos(&self) -> usize {
        self.bit
    }
}

/// Smallest `k` with `(blk << k) >= target` (AV1 `tile_log2`).
fn tile_log2(blk: u32, target: u32) -> u32 {
    let mut k = 0;
    while (blk << k) < target {
        k += 1;
    }
    k
}

/// The AV1 sequence header fields a still-picture (AVIF) decoder needs.
#[derive(Debug, Default)]
pub(crate) struct SequenceHeader {
    pub seq_profile: u32,
    pub bit_depth: u32,
    pub mono_chrome: bool,
    pub subsampling_x: u32,
    pub subsampling_y: u32,
    pub width: u32,
    pub height: u32,
    pub frame_width_bits: u32,
    pub frame_height_bits: u32,
    pub use_128x128_superblock: bool,
    pub enable_filter_intra: bool,
    pub enable_intra_edge_filter: bool,
    pub enable_superres: bool,
    pub enable_cdef: bool,
    pub enable_restoration: bool,
    pub color_range: u32,
    /// `matrix_coefficients` (AV1 color_config): 0=Identity(GBR), 1=BT.709,
    /// 6=BT.601, 9=BT.2020-NCL, 2=Unspecified (→ BT.601 default). Drives YUV→RGB.
    pub matrix_coefficients: u32,
    pub separate_uv_delta_q: bool,
    pub film_grain_params_present: bool,
}

/// Parse `sequence_header_obu` (AV1 spec §5.5). Only the
/// `reduced_still_picture_header` path — what AVIF still images use — is
/// supported; the full streaming path returns `None`.
pub(crate) fn parse_sequence_header(data: &[u8]) -> Option<SequenceHeader> {
    let mut r = BitReader::new(data);
    let mut s = SequenceHeader {
        seq_profile: r.f(3),
        ..Default::default()
    };
    let _still_picture = r.f(1);
    let reduced = r.f(1) != 0;
    if !reduced {
        return None;
    }
    let _seq_level_idx0 = r.f(5);
    s.frame_width_bits = r.f(4) + 1;
    s.frame_height_bits = r.f(4) + 1;
    s.width = r.f(s.frame_width_bits) + 1;
    s.height = r.f(s.frame_height_bits) + 1;
    s.use_128x128_superblock = r.f(1) != 0;
    s.enable_filter_intra = r.f(1) != 0;
    s.enable_intra_edge_filter = r.f(1) != 0;
    s.enable_superres = r.f(1) != 0;
    s.enable_cdef = r.f(1) != 0;
    s.enable_restoration = r.f(1) != 0;

    // color_config()
    let high_bitdepth = r.f(1) != 0;
    s.bit_depth = if s.seq_profile == 2 && high_bitdepth {
        if r.f(1) != 0 {
            12
        } else {
            10
        }
    } else if high_bitdepth {
        10
    } else {
        8
    };
    s.mono_chrome = if s.seq_profile == 1 { false } else { r.f(1) != 0 };
    let color_description_present = r.f(1) != 0;
    let (cp, tc, mc) = if color_description_present {
        (r.f(8), r.f(8), r.f(8))
    } else {
        (2, 2, 2) // CP/TC/MC_UNSPECIFIED
    };
    s.matrix_coefficients = mc;
    if s.mono_chrome {
        s.color_range = r.f(1);
        s.subsampling_x = 1;
        s.subsampling_y = 1;
    } else if cp == 1 && tc == 13 && mc == 0 {
        // BT.709 + sRGB + identity ⇒ full-range 4:4:4
        s.color_range = 1;
        s.subsampling_x = 0;
        s.subsampling_y = 0;
    } else {
        s.color_range = r.f(1);
        match s.seq_profile {
            0 => {
                s.subsampling_x = 1;
                s.subsampling_y = 1;
            }
            1 => {
                s.subsampling_x = 0;
                s.subsampling_y = 0;
            }
            _ => {
                if s.bit_depth == 12 {
                    s.subsampling_x = r.f(1);
                    s.subsampling_y = if s.subsampling_x != 0 { r.f(1) } else { 0 };
                } else {
                    s.subsampling_x = 1;
                    s.subsampling_y = 0;
                }
            }
        }
        if s.subsampling_x != 0 && s.subsampling_y != 0 {
            let _chroma_sample_position = r.f(2);
        }
    }
    if !s.mono_chrome {
        s.separate_uv_delta_q = r.f(1) != 0;
    }
    s.film_grain_params_present = r.f(1) != 0;
    Some(s)
}

// ── AV1 frame header (uncompressed_header, §5.9.2 — reduced still-picture/KEY) ──

/// `Segmentation_Feature_Bits` (AV1 §5.9.14).
const SEG_FEATURE_BITS: [u32; 8] = [8, 6, 6, 6, 6, 3, 0, 0];
/// `Segmentation_Feature_Signed`.
const SEG_FEATURE_SIGNED: [bool; 8] = [true, true, true, true, true, false, false, false];
/// `Segmentation_Feature_Max`.
const SEG_FEATURE_MAX: [i32; 8] = [255, 63, 63, 63, 63, 7, 0, 0];
/// Default loop-filter ref deltas (INTRA..ALTREF), AV1 §7.14 setup_past_independence.
const DEFAULT_LF_REF_DELTAS: [i32; 8] = [1, 0, 0, 0, -1, 0, -1, -1];
/// `Remap_Lr_Type` (AV1 §5.9.20): NONE, SWITCHABLE, WIENER, SGRPROJ.
const REMAP_LR_TYPE: [u8; 4] = [0, 3, 1, 2];

/// The AV1 frame-header fields a still-picture intra decoder needs downstream
/// (partition/intra/transform/CDEF/loop-restoration). Only the
/// reduced-still-picture KEY path is populated.
#[derive(Debug, Default)]
pub(crate) struct FrameHeader {
    pub allow_screen_content_tools: bool,
    pub allow_intrabc: bool,
    pub frame_width: u32,
    pub frame_height: u32,
    pub upscaled_width: u32,
    pub render_width: u32,
    pub render_height: u32,
    pub use_superres: bool,
    pub superres_denom: u32,
    pub tile_cols_log2: u32,
    pub tile_rows_log2: u32,
    pub base_q_idx: u32,
    pub delta_q_y_dc: i32,
    pub delta_q_u_dc: i32,
    pub delta_q_u_ac: i32,
    pub delta_q_v_dc: i32,
    pub delta_q_v_ac: i32,
    pub using_qmatrix: bool,
    pub qm_y: u32,
    pub qm_u: u32,
    pub qm_v: u32,
    pub segmentation_enabled: bool,
    pub feature_enabled: [[bool; 8]; 8],
    pub feature_data: [[i32; 8]; 8],
    pub delta_q_present: bool,
    pub delta_q_res: u32,
    pub delta_lf_present: bool,
    pub delta_lf_res: u32,
    pub delta_lf_multi: bool,
    pub coded_lossless: bool,
    pub all_lossless: bool,
    pub lossless: [bool; 8],
    pub loop_filter_level: [u32; 4],
    pub loop_filter_sharpness: u32,
    pub loop_filter_ref_deltas: [i32; 8],
    pub loop_filter_mode_deltas: [i32; 2],
    /// `loop_filter_delta_enabled` — applies the ref/mode deltas to the level.
    pub loop_filter_delta_enabled: bool,
    pub cdef_damping: u32,
    pub cdef_bits: u32,
    pub cdef_y_pri: [u32; 8],
    pub cdef_y_sec: [u32; 8],
    pub cdef_uv_pri: [u32; 8],
    pub cdef_uv_sec: [u32; 8],
    pub lr_type: [u8; 3],
    pub lr_unit_size: [u32; 3],
    pub tx_mode_select: bool,
    pub reduced_tx_set: bool,
    /// Freezes the adaptive CDFs for this frame (drives `Msac` cdf updates).
    pub disable_cdf_update: bool,
    /// Number of bits consumed by the header (the tile data follows, byte-aligned).
    pub header_bits: usize,
}

fn clip3(lo: i32, hi: i32, v: i32) -> i32 {
    v.clamp(lo, hi)
}

/// `read_delta_q()` (AV1 §5.9.13): a flag then an optional signed 7-bit value.
fn read_delta_q(r: &mut BitReader<'_>) -> i32 {
    if r.f(1) != 0 {
        r.su(1 + 6)
    } else {
        0
    }
}

/// `get_qindex` with `ignoreDeltaQ = 1` (AV1 §7.12.2) — used for the lossless
/// derivation (SEG_LVL_ALT_Q is segmentation feature index 0).
fn get_qindex(h: &FrameHeader, seg: usize) -> i32 {
    if h.segmentation_enabled && h.feature_enabled[seg][0] {
        clip3(0, 255, h.base_q_idx as i32 + h.feature_data[seg][0])
    } else {
        h.base_q_idx as i32
    }
}

/// `ns(n)` — non-symmetric unsigned encoding (AV1 §4.10.7).
fn ns(r: &mut BitReader<'_>, n: u32) -> u32 {
    if n <= 1 {
        return 0;
    }
    let w = 32 - (n - 1).leading_zeros(); // FloorLog2(n-1) + 1
    let m = (1u32 << w) - n;
    let v = r.f(w - 1);
    if v < m {
        v
    } else {
        (v << 1) - m + r.f(1)
    }
}

/// Consume `film_grain_params()` bits (AV1 §5.9.30) to keep the reader aligned.
/// AVIF stills rarely carry film grain; we parse-and-discard (no application).
fn parse_film_grain(r: &mut BitReader<'_>, seq: &SequenceHeader) {
    if r.f(1) == 0 {
        return; // apply_grain
    }
    let _grain_seed = r.f(16);
    // KEY_FRAME ⇒ update_grain = 1 (not coded).
    let num_y_points = r.f(4);
    for _ in 0..num_y_points {
        r.f(8); // point_y_value
        r.f(8); // point_y_scaling
    }
    let chroma_scaling_from_luma = if seq.mono_chrome { false } else { r.f(1) != 0 };
    let (num_cb_points, num_cr_points) = if seq.mono_chrome
        || chroma_scaling_from_luma
        || (seq.subsampling_x == 1 && seq.subsampling_y == 1 && num_y_points == 0)
    {
        (0, 0)
    } else {
        let cb = r.f(4);
        for _ in 0..cb {
            r.f(8);
            r.f(8);
        }
        let cr = r.f(4);
        for _ in 0..cr {
            r.f(8);
            r.f(8);
        }
        (cb, cr)
    };
    let _grain_scaling_minus_8 = r.f(2);
    let ar_coeff_lag = r.f(2);
    let num_pos_luma = 2 * ar_coeff_lag * (ar_coeff_lag + 1);
    let num_pos_chroma = if num_y_points > 0 {
        for _ in 0..num_pos_luma {
            r.f(8);
        }
        num_pos_luma + 1
    } else {
        num_pos_luma
    };
    if chroma_scaling_from_luma || num_cb_points > 0 {
        for _ in 0..num_pos_chroma {
            r.f(8);
        }
    }
    if chroma_scaling_from_luma || num_cr_points > 0 {
        for _ in 0..num_pos_chroma {
            r.f(8);
        }
    }
    let _ar_coeff_shift_minus_6 = r.f(2);
    let _grain_scale_shift = r.f(2);
    if num_cb_points > 0 {
        r.f(8); // cb_mult
        r.f(8); // cb_luma_mult
        r.f(9); // cb_offset
    }
    if num_cr_points > 0 {
        r.f(8);
        r.f(8);
        r.f(9);
    }
    let _overlap_flag = r.f(1);
    let _clip_to_restricted_range = r.f(1);
}

/// Parse the AV1 `uncompressed_header` for an AVIF still image. With
/// `reduced_still_picture_header` the frame is a shown KEY_FRAME (intra), which
/// elides every inter-frame syntax element. Translated from the AV1 spec §5.9
/// and dav1d `src/obu.c` (BSD). Consumes only the header; `header_bits` lets the
/// caller byte-align before the tile data.
#[allow(clippy::field_reassign_with_default)] // header is built incrementally
pub(crate) fn parse_frame_header(seq: &SequenceHeader, data: &[u8]) -> Option<FrameHeader> {
    let mut r = BitReader::new(data);
    let num_planes = if seq.mono_chrome { 1 } else { 3 };
    let mut h = FrameHeader::default();

    h.disable_cdf_update = r.f(1) != 0;
    // seq_force_screen_content_tools == SELECT (reduced) ⇒ read the flag.
    let allow_screen_content_tools = r.f(1) != 0;
    h.allow_screen_content_tools = allow_screen_content_tools;
    if allow_screen_content_tools {
        // seq_force_integer_mv == SELECT ⇒ read; intra forces it to 1 regardless.
        let _force_integer_mv = r.f(1);
    }
    // frame_size_override_flag=0, order_hint absent (enable_order_hint=0),
    // primary_ref_frame=NONE, refresh_frame_flags=0xff: none coded.

    // frame_size(): no override ⇒ sequence dimensions.
    h.frame_width = seq.width;
    h.frame_height = seq.height;
    // superres_params()
    h.use_superres = seq.enable_superres && r.f(1) != 0;
    h.superres_denom = if h.use_superres {
        r.f(3) + 9 // coded_denom + SUPERRES_DENOM_MIN
    } else {
        8 // SUPERRES_NUM
    };
    h.upscaled_width = h.frame_width;
    if h.use_superres {
        h.frame_width = (h.upscaled_width * 8 + h.superres_denom / 2) / h.superres_denom;
    }
    // render_size()
    if r.f(1) != 0 {
        h.render_width = r.f(16) + 1;
        h.render_height = r.f(16) + 1;
    } else {
        h.render_width = h.upscaled_width;
        h.render_height = h.frame_height;
    }
    if allow_screen_content_tools && h.upscaled_width == h.frame_width {
        h.allow_intrabc = r.f(1) != 0;
    }

    // tile_info() (AV1 §5.9.15)
    let mi_cols = 2 * ((h.frame_width + 7) >> 3);
    let mi_rows = 2 * ((h.frame_height + 7) >> 3);
    let (sb_cols, sb_rows, sb_size) = if seq.use_128x128_superblock {
        ((mi_cols + 31) >> 5, (mi_rows + 31) >> 5, 7u32) // sbShift 5 + 2
    } else {
        ((mi_cols + 15) >> 4, (mi_rows + 15) >> 4, 6u32) // sbShift 4 + 2
    };
    let max_tile_width_sb = 4096u32 >> sb_size;
    let max_tile_area_sb = (4096u32 * 2304) >> (2 * sb_size);
    let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
    let max_log2_tile_cols = tile_log2(1, sb_cols.min(64));
    let max_log2_tile_rows = tile_log2(1, sb_rows.min(64));
    let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
    let uniform = r.f(1) != 0;
    if uniform {
        h.tile_cols_log2 = min_log2_tile_cols;
        while h.tile_cols_log2 < max_log2_tile_cols {
            if r.f(1) != 0 {
                h.tile_cols_log2 += 1;
            } else {
                break;
            }
        }
        let min_log2_tile_rows = min_log2_tiles.saturating_sub(h.tile_cols_log2);
        h.tile_rows_log2 = min_log2_tile_rows;
        while h.tile_rows_log2 < max_log2_tile_rows {
            if r.f(1) != 0 {
                h.tile_rows_log2 += 1;
            } else {
                break;
            }
        }
    } else {
        // Non-uniform tiling (rare for AVIF): widths/heights coded as ns().
        let mut start_sb = 0u32;
        let mut cols = 0u32;
        while start_sb < sb_cols {
            let max_width = (sb_cols - start_sb).min(max_tile_width_sb);
            start_sb += ns(&mut r, max_width) + 1;
            cols += 1;
        }
        h.tile_cols_log2 = tile_log2(1, cols);
        let max_tile_height_sb = (max_tile_area_sb / start_sb.max(1)).max(1);
        let mut start_sb = 0u32;
        let mut rows = 0u32;
        while start_sb < sb_rows {
            let max_height = (sb_rows - start_sb).min(max_tile_height_sb);
            start_sb += ns(&mut r, max_height) + 1;
            rows += 1;
        }
        h.tile_rows_log2 = tile_log2(1, rows);
    }
    if h.tile_cols_log2 > 0 || h.tile_rows_log2 > 0 {
        let _context_update_tile_id = r.f(h.tile_rows_log2 + h.tile_cols_log2);
        let _tile_size_bytes_minus_1 = r.f(2);
    }

    // quantization_params() (AV1 §5.9.12)
    h.base_q_idx = r.f(8);
    h.delta_q_y_dc = read_delta_q(&mut r);
    if num_planes > 1 {
        let diff_uv_delta = seq.separate_uv_delta_q && r.f(1) != 0;
        h.delta_q_u_dc = read_delta_q(&mut r);
        h.delta_q_u_ac = read_delta_q(&mut r);
        if diff_uv_delta {
            h.delta_q_v_dc = read_delta_q(&mut r);
            h.delta_q_v_ac = read_delta_q(&mut r);
        } else {
            h.delta_q_v_dc = h.delta_q_u_dc;
            h.delta_q_v_ac = h.delta_q_u_ac;
        }
    }
    h.using_qmatrix = r.f(1) != 0;
    if h.using_qmatrix {
        h.qm_y = r.f(4);
        h.qm_u = r.f(4);
        h.qm_v = if seq.separate_uv_delta_q { r.f(4) } else { h.qm_u };
    }

    // segmentation_params() — primary_ref_frame == NONE ⇒ update_map/data = 1.
    h.segmentation_enabled = r.f(1) != 0;
    if h.segmentation_enabled {
        for i in 0..8 {
            for j in 0..8 {
                if r.f(1) != 0 {
                    h.feature_enabled[i][j] = true;
                    let bits = SEG_FEATURE_BITS[j];
                    let limit = SEG_FEATURE_MAX[j];
                    h.feature_data[i][j] = if SEG_FEATURE_SIGNED[j] {
                        clip3(-limit, limit, r.su(1 + bits))
                    } else {
                        clip3(0, limit, r.f(bits) as i32)
                    };
                }
            }
        }
    }

    // delta_q_params() / delta_lf_params()
    if h.base_q_idx > 0 {
        h.delta_q_present = r.f(1) != 0;
    }
    if h.delta_q_present {
        h.delta_q_res = r.f(2);
    }
    if h.delta_q_present && !h.allow_intrabc {
        h.delta_lf_present = r.f(1) != 0;
        if h.delta_lf_present {
            h.delta_lf_res = r.f(2);
            h.delta_lf_multi = r.f(1) != 0;
        }
    }

    // CodedLossless / AllLossless (derived, no bits).
    h.coded_lossless = true;
    for seg in 0..8 {
        let lossless = get_qindex(&h, seg) == 0
            && h.delta_q_y_dc == 0
            && h.delta_q_u_ac == 0
            && h.delta_q_u_dc == 0
            && h.delta_q_v_ac == 0
            && h.delta_q_v_dc == 0;
        h.lossless[seg] = lossless;
        if !lossless {
            h.coded_lossless = false;
        }
    }
    h.all_lossless = h.coded_lossless && h.frame_width == h.upscaled_width;

    // loop_filter_params() (AV1 §5.9.11)
    h.loop_filter_ref_deltas = DEFAULT_LF_REF_DELTAS;
    if !(h.coded_lossless || h.allow_intrabc) {
        h.loop_filter_level[0] = r.f(6);
        h.loop_filter_level[1] = r.f(6);
        if num_planes > 1 && (h.loop_filter_level[0] != 0 || h.loop_filter_level[1] != 0) {
            h.loop_filter_level[2] = r.f(6);
            h.loop_filter_level[3] = r.f(6);
        }
        h.loop_filter_sharpness = r.f(3);
        if r.f(1) != 0 {
            // loop_filter_delta_enabled
            h.loop_filter_delta_enabled = true;
            if r.f(1) != 0 {
                // loop_filter_delta_update
                for i in 0..8 {
                    if r.f(1) != 0 {
                        h.loop_filter_ref_deltas[i] = r.su(1 + 6);
                    }
                }
                for i in 0..2 {
                    if r.f(1) != 0 {
                        h.loop_filter_mode_deltas[i] = r.su(1 + 6);
                    }
                }
            }
        }
    }

    // cdef_params() (AV1 §5.9.19)
    h.cdef_damping = 3;
    if !(h.coded_lossless || h.allow_intrabc || !seq.enable_cdef) {
        h.cdef_damping = r.f(2) + 3;
        h.cdef_bits = r.f(2);
        for i in 0..(1usize << h.cdef_bits) {
            h.cdef_y_pri[i] = r.f(4);
            h.cdef_y_sec[i] = r.f(2);
            if h.cdef_y_sec[i] == 3 {
                h.cdef_y_sec[i] += 1;
            }
            if num_planes > 1 {
                h.cdef_uv_pri[i] = r.f(4);
                h.cdef_uv_sec[i] = r.f(2);
                if h.cdef_uv_sec[i] == 3 {
                    h.cdef_uv_sec[i] += 1;
                }
            }
        }
    }

    // lr_params() (AV1 §5.9.20)
    if !(h.all_lossless || h.allow_intrabc || !seq.enable_restoration) {
        let mut uses_lr = false;
        let mut uses_chroma_lr = false;
        for i in 0..num_planes {
            let t = REMAP_LR_TYPE[r.f(2) as usize];
            h.lr_type[i] = t;
            if t != 0 {
                uses_lr = true;
                if i > 0 {
                    uses_chroma_lr = true;
                }
            }
        }
        if uses_lr {
            let lr_unit_shift = if seq.use_128x128_superblock {
                r.f(1) + 1
            } else {
                let s = r.f(1);
                if s != 0 {
                    s + r.f(1)
                } else {
                    s
                }
            };
            let size0 = 256u32 >> (2 - lr_unit_shift);
            h.lr_unit_size[0] = size0;
            let lr_uv_shift = if seq.subsampling_x != 0 && seq.subsampling_y != 0 && uses_chroma_lr {
                r.f(1)
            } else {
                0
            };
            h.lr_unit_size[1] = size0 >> lr_uv_shift;
            h.lr_unit_size[2] = size0 >> lr_uv_shift;
        }
    }

    // read_tx_mode() (AV1 §5.9.21)
    if !h.coded_lossless {
        h.tx_mode_select = r.f(1) != 0;
    }

    // frame_reference_mode / skip_mode_params / allow_warped_motion: intra ⇒ no bits.
    h.reduced_tx_set = r.f(1) != 0;
    // global_motion_params(): intra ⇒ all identity, no bits.

    if seq.film_grain_params_present {
        parse_film_grain(&mut r, seq);
    }

    h.header_bits = r.pos();
    Some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avif_container_extracts_av1_obus() {
        let avif = include_bytes!("fixtures/av1test.avif");
        let stream = extract_av1_stream(avif).expect("extract AV1 stream");
        let obus = split_obus(&stream).expect("split OBUs");
        let kinds: Vec<u8> = obus.iter().map(|o| o.kind).collect();
        // libaom/ffmpeg emits a sequence header followed by a frame OBU.
        assert!(kinds.contains(&OBU_SEQUENCE_HEADER), "kinds={kinds:?}");
        assert!(
            kinds.contains(&OBU_FRAME)
                || (kinds.contains(&OBU_FRAME_HEADER) && kinds.contains(&OBU_TILE_GROUP)),
            "kinds={kinds:?}"
        );
        let seq = obus.iter().find(|o| o.kind == OBU_SEQUENCE_HEADER).unwrap();
        assert!(!seq.data.is_empty());
    }

    #[test]
    fn av1_sequence_header_parses_dimensions() {
        let avif = include_bytes!("fixtures/av1test.avif");
        let stream = extract_av1_stream(avif).unwrap();
        let obus = split_obus(&stream).unwrap();
        let seq = obus.iter().find(|o| o.kind == OBU_SEQUENCE_HEADER).unwrap();
        let sh = parse_sequence_header(seq.data).expect("parse sequence header");
        // Empirical validation against the known 32×32 4:2:0 fixture: confirms the
        // bit layout up to the frame dimensions is correct.
        assert_eq!((sh.width, sh.height), (32, 32), "{sh:?}");
        assert_eq!(sh.seq_profile, 0);
        assert_eq!(sh.bit_depth, 8);
        assert!(!sh.mono_chrome);
        assert_eq!((sh.subsampling_x, sh.subsampling_y), (1, 1));
    }

    #[test]
    fn av1_default_cdf_tables_are_well_formed() {
        // Spot-check against known dav1d values (inverse Q15: 32768 - cdf_c).
        // partition[0][0] starts CDF4(27899, ...) ⇒ 32768-27899 = 4869.
        assert_eq!(cdf::PARTITION[0][0][0], 4869);
        // kfym[0][0] starts CDF12(15588, ...) ⇒ 32768-15588 = 17180.
        assert_eq!(cdf::KF_Y_MODE[0][0][0], 17180);
        // The 4 quantizer-category coefficient sets must all be present.
        assert_eq!(cdf::COEFF_BASE_Q.len(), 4);
        assert_eq!(cdf::COEF_SKIP_Q.len(), 4);
        // Inverse-CDF rows are non-increasing until the zero padding/counter:
        // verify a representative row of the 13-symbol KF Y-mode CDF.
        let row = &cdf::KF_Y_MODE[2][3];
        let mut seen_zero = false;
        for w in row.windows(2) {
            if w[1] == 0 {
                seen_zero = true;
            }
            if !seen_zero {
                assert!(w[0] >= w[1], "kfym not monotone at {w:?}");
            }
        }
        assert!(seen_zero, "expected trailing counter/padding zeros");
    }

    #[test]
    fn av1_scan_tables_are_permutations() {
        // SCANS[TX_4X4] is dav1d's 4×4 zig-zag (DC first).
        assert_eq!(scan::SCANS[0][..4], [0, 4, 1, 2]);
        let lens = [
            16, 64, 256, 1024, 1024, 32, 32, 128, 128, 512, 512, 1024, 1024, 64, 64, 256, 256, 512,
            512,
        ];
        for (i, s) in scan::SCANS.iter().enumerate() {
            assert_eq!(s.len(), lens[i], "scan {i} length");
            // Each scan must be a permutation of 0..len (no dups, in range).
            let mut seen = vec![false; s.len()];
            for &p in s.iter() {
                let p = p as usize;
                assert!(p < s.len() && !seen[p], "scan {i} bad position {p}");
                seen[p] = true;
            }
        }
    }

    #[test]
    fn av1_frame_header_parses_quant() {
        let avif = include_bytes!("fixtures/av1test.avif");
        let stream = extract_av1_stream(avif).unwrap();
        let obus = split_obus(&stream).unwrap();
        let seq = obus.iter().find(|o| o.kind == OBU_SEQUENCE_HEADER).unwrap();
        let sh = parse_sequence_header(seq.data).unwrap();
        // The frame header is the start of the OBU_FRAME payload (header + tile
        // group), or a standalone OBU_FRAME_HEADER.
        let frame = obus
            .iter()
            .find(|o| o.kind == OBU_FRAME || o.kind == OBU_FRAME_HEADER)
            .expect("frame OBU present");
        let fh = parse_frame_header(&sh, frame.data).expect("parse frame header");
        assert_eq!((fh.frame_width, fh.frame_height), (32, 32), "{fh:?}");
        // The header precedes the tile data, so it must fit within the OBU payload
        // — a gross over-read (wrong syntax) would blow past this bound.
        assert!(
            fh.header_bits > 0 && fh.header_bits <= frame.data.len() * 8,
            "header_bits={} payload_bits={} {fh:?}",
            fh.header_bits,
            frame.data.len() * 8
        );
    }

    #[test]
    fn to_u8_rounds_and_saturates() {
        assert_eq!(super::to_u8(-5.0), 0);
        assert_eq!(super::to_u8(0.4), 0);
        assert_eq!(super::to_u8(0.5), 1);
        assert_eq!(super::to_u8(127.5), 128);
        assert_eq!(super::to_u8(254.6), 255);
        assert_eq!(super::to_u8(999.0), 255);
    }

    #[test]
    fn decode_avif_produces_rgba_pixels() {
        // THE pixel milestone: decode_avif drives parse → tile decode → YUV→RGBA
        // and emits a real image. The intra planes are bit-exact vs the YUV
        // reference (see tile::reconstructs_fixture_pixels), so the RGBA equals
        // the documented YCbCr→RGB conversion of that reference.
        let avif = include_bytes!("fixtures/av1test.avif");
        let reference = include_bytes!("fixtures/av1test_ref.yuv");
        let (w, h, rgba) = decode_avif(avif).expect("decode_avif returns pixels");
        assert_eq!((w, h), (32, 32));
        assert_eq!(rgba.len(), 32 * 32 * 4);
        assert!(rgba.iter().skip(3).step_by(4).all(|&a| a == 255), "alpha not opaque");
        assert!(rgba.chunks(4).any(|p| p[..3] != [0, 0, 0]), "image is all black");

        // Independent cross-check against the I420 reference (Y 1024 + U/V 256
        // each), using BT.601 limited-range constants (the fixture's matrix).
        let seq = parse_sequence_header(
            split_obus(&extract_av1_stream(avif).unwrap())
                .unwrap()
                .iter()
                .find(|o| o.kind == OBU_SEQUENCE_HEADER)
                .unwrap()
                .data,
        )
        .unwrap();
        eprintln!(
            "[avif] mc={} range={} mono={} ss=({},{})",
            seq.matrix_coefficients, seq.color_range, seq.mono_chrome,
            seq.subsampling_x, seq.subsampling_y
        );
        let full = seq.color_range != 0;
        let (kr, kb) = match seq.matrix_coefficients {
            1 => (0.2126f32, 0.0722f32),
            9 => (0.2627, 0.0593),
            _ => (0.299, 0.114),
        };
        let kg = 1.0 - kr - kb;
        let cscale = if full { 1.0 } else { 255.0 / 224.0 };
        let mut maxdiff = 0i32;
        for py in 0..32usize {
            for px in 0..32usize {
                let y = reference[py * 32 + px] as f32;
                let cx = px >> seq.subsampling_x;
                let cy = py >> seq.subsampling_y;
                let u = (reference[1024 + cy * 16 + cx] as f32 - 128.0) * cscale;
                let v = (reference[1024 + 256 + cy * 16 + cx] as f32 - 128.0) * cscale;
                let yl = if full { y } else { (y - 16.0) * (255.0 / 219.0) };
                let exp = [
                    super::to_u8(yl + 2.0 * (1.0 - kr) * v),
                    super::to_u8(yl - 2.0 * kb * (1.0 - kb) / kg * u - 2.0 * kr * (1.0 - kr) / kg * v),
                    super::to_u8(yl + 2.0 * (1.0 - kb) * u),
                ];
                let o = (py * 32 + px) * 4;
                for c in 0..3 {
                    maxdiff = maxdiff.max((rgba[o + c] as i32 - exp[c] as i32).abs());
                }
            }
        }
        assert_eq!(maxdiff, 0, "RGBA diverges from the reference conversion");
    }
}
