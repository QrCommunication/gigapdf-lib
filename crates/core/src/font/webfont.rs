//! WOFF / WOFF2 → sfnt reconstruction for CSS `@font-face` with inline `src`.
//!
//! [`sfnt_from_web_font`] takes the bytes of any web font and returns a plain
//! sfnt (TrueType / OpenType) byte vector that the engine's
//! [`super::truetype`] / [`super::cff`] parsers can consume directly:
//!
//! * **raw sfnt** (`0x00010000` / `OTTO` / `true` / `ttcf`) is returned as-is;
//! * **WOFF1** (`wOFF`) per-table zlib (RFC 1950) is decompressed via the
//!   engine's own [`inflate`](crate::filters::inflate::inflate) and the sfnt
//!   table directory is rebuilt with correct offsets, `searchRange` and
//!   checksums;
//! * **WOFF2** (`wOF2`) is parsed (header + table directory with the known-tag
//!   table, flag byte and UIntBase128 lengths), the single Brotli stream is
//!   decompressed via [`super::brotli`], and the transformed `glyf`/`loca`
//!   (and, if present, `hmtx`) tables are fully reconstructed (W3C WOFF2 §5.1,
//!   §5.4) before the sfnt directory is assembled.
//!
//! Zero third-party dependencies: the Brotli and inflate stages are both
//! in-house.

use super::brotli;
use crate::filters::inflate::inflate;

const SFNT_TRUETYPE: u32 = 0x0001_0000;
const SFNT_OTTO: u32 = 0x4F54_544F; // 'OTTO'
const SFNT_TRUE: u32 = 0x7472_7565; // 'true'
const SFNT_TTCF: u32 = 0x7474_6366; // 'ttcf'
const WOFF1_SIG: u32 = 0x774F_4646; // 'wOFF'
const WOFF2_SIG: u32 = 0x774F_4632; // 'wOF2'

const GLYF_TAG: u32 = 0x676C_7966; // 'glyf'
const LOCA_TAG: u32 = 0x6C6F_6361; // 'loca'
const HEAD_TAG: u32 = 0x6865_6164; // 'head'
const HMTX_TAG: u32 = 0x686D_7478; // 'hmtx'
const HHEA_TAG: u32 = 0x6868_6561; // 'hhea'
const MAXP_TAG: u32 = 0x6D61_7870; // 'maxp'

/// Reconstruct a plain sfnt (ttf/otf) from any web-font byte stream.
/// Returns `None` on malformed input or an unsupported flavor.
pub fn sfnt_from_web_font(bytes: &[u8]) -> Option<Vec<u8>> {
    let sig = be32(bytes, 0)?;
    match sig {
        SFNT_TRUETYPE | SFNT_OTTO | SFNT_TRUE | SFNT_TTCF => Some(bytes.to_vec()),
        WOFF1_SIG => woff1_to_sfnt(bytes),
        WOFF2_SIG => woff2_to_sfnt(bytes),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Big-endian readers over a byte slice.
// ---------------------------------------------------------------------------

fn be16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(off)?, *b.get(off + 1)?]))
}
fn be32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(off)?,
        *b.get(off + 1)?,
        *b.get(off + 2)?,
        *b.get(off + 3)?,
    ]))
}

// ---------------------------------------------------------------------------
// sfnt assembly: build a valid sfnt from a list of (tag, table-bytes).
// ---------------------------------------------------------------------------

/// Build an sfnt file (header + directory + 4-byte-aligned tables, with table
/// and head checksums) from `flavor` and `(tag, data)` pairs.
fn build_sfnt(flavor: u32, mut tables: Vec<(u32, Vec<u8>)>) -> Vec<u8> {
    // Table directory entries must be sorted by tag (sfnt spec).
    tables.sort_by_key(|(tag, _)| *tag);
    let num_tables = tables.len() as u16;

    // searchRange = (largest power of 2 <= numTables) * 16.
    let mut max_pow2: u16 = 1;
    while max_pow2 * 2 <= num_tables.max(1) {
        max_pow2 *= 2;
    }
    let search_range = max_pow2 * 16;
    let entry_selector = (max_pow2 as f32).log2() as u16;
    let range_shift = num_tables * 16 - search_range;

    let header_len = 12;
    let dir_len = 16 * tables.len();
    let mut offset = header_len + dir_len;

    // Compute each table's offset (4-byte aligned) and padded length.
    let mut layout = Vec::with_capacity(tables.len());
    for (tag, data) in &tables {
        let off = offset;
        let len = data.len();
        let padded = (len + 3) & !3;
        layout.push((*tag, off, len, padded));
        offset += padded;
    }
    let total = offset;

    let mut out = vec![0u8; total];
    // Header.
    out[0..4].copy_from_slice(&flavor.to_be_bytes());
    out[4..6].copy_from_slice(&num_tables.to_be_bytes());
    out[6..8].copy_from_slice(&search_range.to_be_bytes());
    out[8..10].copy_from_slice(&entry_selector.to_be_bytes());
    out[10..12].copy_from_slice(&range_shift.to_be_bytes());

    // Directory + table bodies.
    for (i, ((_, data), (tag, off, len, _padded))) in tables.iter().zip(layout.iter()).enumerate() {
        out[*off..*off + *len].copy_from_slice(data);
        let checksum = table_checksum(&out[*off..*off + ((*len + 3) & !3)]);
        let de = header_len + i * 16;
        out[de..de + 4].copy_from_slice(&tag.to_be_bytes());
        out[de + 4..de + 8].copy_from_slice(&checksum.to_be_bytes());
        out[de + 8..de + 12].copy_from_slice(&(*off as u32).to_be_bytes());
        out[de + 12..de + 16].copy_from_slice(&(*len as u32).to_be_bytes());
    }

    // head.checkSumAdjustment: 0xB1B0AFBA - checksum(whole file with field=0).
    if let Some((_, head_off, head_len, _)) = layout.iter().find(|(t, ..)| *t == HEAD_TAG) {
        if *head_len >= 12 {
            let csa_pos = head_off + 8;
            out[csa_pos..csa_pos + 4].copy_from_slice(&[0, 0, 0, 0]);
            let file_sum = table_checksum(&out);
            let adj = 0xB1B0_AFBAu32.wrapping_sub(file_sum);
            out[csa_pos..csa_pos + 4].copy_from_slice(&adj.to_be_bytes());
        }
    }
    out
}

/// sfnt table checksum: sum of big-endian u32 words (slice is 4-byte padded).
fn table_checksum(data: &[u8]) -> u32 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 4 <= data.len() {
        sum = sum.wrapping_add(u32::from_be_bytes([
            data[i],
            data[i + 1],
            data[i + 2],
            data[i + 3],
        ]));
        i += 4;
    }
    // Trailing bytes (only when not 4-aligned; build_sfnt always pads, so this
    // path is unused there but keeps the helper correct in isolation).
    if i < data.len() {
        let mut last = [0u8; 4];
        last[..data.len() - i].copy_from_slice(&data[i..]);
        sum = sum.wrapping_add(u32::from_be_bytes(last));
    }
    sum
}

// ---------------------------------------------------------------------------
// WOFF1 (RFC: WOFF File Format 1.0) — per-table zlib.
// ---------------------------------------------------------------------------

fn woff1_to_sfnt(b: &[u8]) -> Option<Vec<u8>> {
    let flavor = be32(b, 4)?;
    let num_tables = be16(b, 12)? as usize;
    let mut tables = Vec::with_capacity(num_tables);
    // Directory starts at offset 44; each entry is 20 bytes.
    for i in 0..num_tables {
        let de = 44 + i * 20;
        let tag = be32(b, de)?;
        let offset = be32(b, de + 4)? as usize;
        let comp_len = be32(b, de + 8)? as usize;
        let orig_len = be32(b, de + 12)? as usize;
        let raw = b.get(offset..offset.checked_add(comp_len)?)?;
        let data = if comp_len == orig_len {
            // Stored uncompressed.
            raw.to_vec()
        } else {
            // zlib (RFC 1950): 2-byte header + raw DEFLATE + 4-byte adler.
            let deflate = raw.get(2..)?;
            let out = inflate(deflate).ok()?;
            if out.len() != orig_len {
                return None;
            }
            out
        };
        tables.push((tag, data));
    }
    Some(build_sfnt(flavor, tables))
}

// ---------------------------------------------------------------------------
// WOFF2 — single Brotli stream + transformed glyf/loca/hmtx.
// ---------------------------------------------------------------------------

/// Known table tags (W3C WOFF2 §5, kKnownTags), indexed by the 6-bit flags
/// value 0..=62. Index 63 means an explicit 4-byte tag follows.
const KNOWN_TAGS: [u32; 63] = [
    tag(b"cmap"),
    tag(b"head"),
    tag(b"hhea"),
    tag(b"hmtx"),
    tag(b"maxp"),
    tag(b"name"),
    tag(b"OS/2"),
    tag(b"post"),
    tag(b"cvt "),
    tag(b"fpgm"),
    tag(b"glyf"),
    tag(b"loca"),
    tag(b"prep"),
    tag(b"CFF "),
    tag(b"VORG"),
    tag(b"EBDT"),
    tag(b"EBLC"),
    tag(b"gasp"),
    tag(b"hdmx"),
    tag(b"kern"),
    tag(b"LTSH"),
    tag(b"PCLT"),
    tag(b"VDMX"),
    tag(b"vhea"),
    tag(b"vmtx"),
    tag(b"BASE"),
    tag(b"GDEF"),
    tag(b"GPOS"),
    tag(b"GSUB"),
    tag(b"EBSC"),
    tag(b"JSTF"),
    tag(b"MATH"),
    tag(b"CBDT"),
    tag(b"CBLC"),
    tag(b"COLR"),
    tag(b"CPAL"),
    tag(b"SVG "),
    tag(b"sbix"),
    tag(b"acnt"),
    tag(b"avar"),
    tag(b"bdat"),
    tag(b"bloc"),
    tag(b"bsln"),
    tag(b"cvar"),
    tag(b"fdsc"),
    tag(b"feat"),
    tag(b"fmtx"),
    tag(b"fvar"),
    tag(b"gvar"),
    tag(b"hsty"),
    tag(b"just"),
    tag(b"lcar"),
    tag(b"mort"),
    tag(b"morx"),
    tag(b"opbd"),
    tag(b"prop"),
    tag(b"trak"),
    tag(b"Zapf"),
    tag(b"Silf"),
    tag(b"Glat"),
    tag(b"Gloc"),
    tag(b"Feat"),
    tag(b"Sill"),
];

const fn tag(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// A decoded WOFF2 directory entry.
struct W2Entry {
    tag: u32,
    transform_version: u8,
    /// Length of this table's sub-stream inside the decompressed Brotli blob:
    /// `transformLength` for transformed tables, otherwise `origLength`.
    transform_length: u32,
}

fn woff2_to_sfnt(b: &[u8]) -> Option<Vec<u8>> {
    let flavor = be32(b, 4)?;
    let num_tables = be16(b, 12)? as usize;
    let total_compressed = be32(b, 20)? as usize;

    // Parse the table directory (starts at offset 48).
    let mut pos = 48usize;
    let mut entries = Vec::with_capacity(num_tables);
    for _ in 0..num_tables {
        let flags = *b.get(pos)?;
        pos += 1;
        let tag_index = (flags & 0x3f) as usize;
        let transform_version = (flags >> 6) & 0x03;
        let table_tag = if tag_index == 0x3f {
            let t = be32(b, pos)?;
            pos += 4;
            t
        } else {
            *KNOWN_TAGS.get(tag_index)?
        };
        let (orig_length, used) = read_uint_base128(b, pos)?;
        pos += used;

        // A transform is present unless the version is the "null" transform.
        // For glyf/loca the null transform is version 3; for every other table
        // the null transform is version 0 (any non-zero version is transformed,
        // but in practice only glyf/loca/hmtx define transforms).
        let is_transformed = match table_tag {
            GLYF_TAG | LOCA_TAG => transform_version != 3,
            _ => transform_version != 0,
        };
        let transform_length = if is_transformed {
            let (tl, used) = read_uint_base128(b, pos)?;
            pos += used;
            tl
        } else {
            orig_length
        };
        entries.push(W2Entry {
            tag: table_tag,
            transform_version,
            transform_length,
        });
    }

    // Decompress the single Brotli stream that follows the directory.
    let comp = b.get(pos..pos.checked_add(total_compressed)?)?;
    let decompressed = brotli::decompress(comp)?;

    // Slice the decompressed buffer into the per-table sub-streams, in
    // directory order, each `transform_length` bytes long.
    let mut cursor = 0usize;
    let mut raw_tables: Vec<(u32, &[u8], &W2Entry)> = Vec::with_capacity(entries.len());
    for e in &entries {
        let len = e.transform_length as usize;
        let slice = decompressed.get(cursor..cursor.checked_add(len)?)?;
        cursor += len;
        raw_tables.push((e.tag, slice, e));
    }

    // We must process glyf+loca together (the loca transform stream is empty;
    // loca is regenerated while decoding the transformed glyf). First locate
    // the relevant streams.
    let head_data = raw_tables
        .iter()
        .find(|(t, ..)| *t == HEAD_TAG)
        .map(|(_, s, _)| *s);
    let index_format = head_data.and_then(|h| be16(h, 50)).unwrap_or(0);

    let mut final_tables: Vec<(u32, Vec<u8>)> = Vec::with_capacity(entries.len());
    let mut reconstructed_glyf: Option<Vec<u8>> = None;
    let mut reconstructed_loca: Option<Vec<u8>> = None;
    let mut glyf_xmins: Vec<i16> = Vec::new();
    let mut num_glyphs_for_hmtx: u16 = 0;

    for (tag, slice, e) in &raw_tables {
        match *tag {
            GLYF_TAG if e.transform_version != 3 => {
                let rec = reconstruct_glyf(slice, index_format)?;
                glyf_xmins = rec.x_mins;
                num_glyphs_for_hmtx = rec.num_glyphs;
                reconstructed_loca = Some(rec.loca);
                reconstructed_glyf = Some(rec.glyf);
            }
            LOCA_TAG if e.transform_version != 3 => {
                // Reconstructed alongside glyf; the transform stream is empty.
            }
            _ => {}
        }
    }

    // Second pass: assemble all tables (handling hmtx transform last, since it
    // needs glyf xMins which the glyf pass produced).
    for (tag, slice, e) in &raw_tables {
        match *tag {
            GLYF_TAG if reconstructed_glyf.is_some() => {
                final_tables.push((GLYF_TAG, reconstructed_glyf.clone().unwrap()));
            }
            LOCA_TAG if reconstructed_loca.is_some() => {
                final_tables.push((LOCA_TAG, reconstructed_loca.clone().unwrap()));
            }
            HMTX_TAG if e.transform_version != 0 => {
                let num_hmetrics = raw_tables
                    .iter()
                    .find(|(t, ..)| *t == HHEA_TAG)
                    .and_then(|(_, s, _)| be16(s, 34))
                    .unwrap_or(num_glyphs_for_hmtx);
                let total_glyphs = raw_tables
                    .iter()
                    .find(|(t, ..)| *t == MAXP_TAG)
                    .and_then(|(_, s, _)| be16(s, 4))
                    .unwrap_or(num_glyphs_for_hmtx);
                let hmtx = reconstruct_hmtx(slice, num_hmetrics, total_glyphs, &glyf_xmins)?;
                final_tables.push((HMTX_TAG, hmtx));
            }
            _ => {
                final_tables.push((*tag, slice.to_vec()));
            }
        }
    }

    Some(build_sfnt(flavor, final_tables))
}

/// Read a UIntBase128 value (W3C WOFF2). Returns `(value, bytes_consumed)`.
fn read_uint_base128(b: &[u8], pos: usize) -> Option<(u32, usize)> {
    let mut accum: u32 = 0;
    for i in 0..5 {
        let byte = *b.get(pos + i)?;
        // Leading 0x80 (a leading zero) is invalid.
        if i == 0 && byte == 0x80 {
            return None;
        }
        // Overflow guard: top 7 bits already set.
        if accum & 0xFE00_0000 != 0 {
            return None;
        }
        accum = (accum << 7) | (byte & 0x7f) as u32;
        if byte & 0x80 == 0 {
            return Some((accum, i + 1));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// WOFF2 transformed glyf → standard glyf + regenerated loca (W3C §5.1).
// ---------------------------------------------------------------------------

struct GlyfReconstruction {
    glyf: Vec<u8>,
    loca: Vec<u8>,
    num_glyphs: u16,
    /// xMin of each glyph (for the hmtx lsb reconstruction).
    x_mins: Vec<i16>,
}

/// A cursor over one sub-stream of the transformed glyf table.
struct Stream<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Stream<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let v = be16(self.data, self.pos)?;
        self.pos += 2;
        Some(v)
    }
    fn i16(&mut self) -> Option<i16> {
        Some(self.u16()? as i16)
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let v = self.data.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(v)
    }
    /// 255UInt16 (W3C WOFF2 §6.1.1).
    fn read_255_u16(&mut self) -> Option<u16> {
        let code = self.u8()?;
        match code {
            255 => Some(self.u8()? as u16 + 253),
            254 => Some(self.u8()? as u16 + 506),
            253 => self.u16(),
            _ => Some(code as u16),
        }
    }
}

fn reconstruct_glyf(data: &[u8], head_index_format: u16) -> Option<GlyfReconstruction> {
    // Transformed glyf header (36 bytes).
    let _reserved = be16(data, 0)?;
    let option_flags = be16(data, 2)?;
    let num_glyphs = be16(data, 4)?;
    let index_format = be16(data, 6)?;
    let n_contour_size = be32(data, 8)? as usize;
    let n_points_size = be32(data, 12)? as usize;
    let flag_size = be32(data, 16)? as usize;
    let glyph_size = be32(data, 20)? as usize;
    let composite_size = be32(data, 24)? as usize;
    let bbox_size = be32(data, 28)? as usize;
    let instruction_size = be32(data, 32)? as usize;

    let _ = head_index_format;

    // Sub-streams in order after the 36-byte header.
    let mut off = 36usize;
    let n_contour = data.get(off..off.checked_add(n_contour_size)?)?;
    off += n_contour_size;
    let n_points = data.get(off..off.checked_add(n_points_size)?)?;
    off += n_points_size;
    let flag_stream = data.get(off..off.checked_add(flag_size)?)?;
    off += flag_size;
    let glyph_stream = data.get(off..off.checked_add(glyph_size)?)?;
    off += glyph_size;
    let composite_stream = data.get(off..off.checked_add(composite_size)?)?;
    off += composite_size;
    let bbox_block = data.get(off..off.checked_add(bbox_size)?)?;
    off += bbox_size;
    let instruction_stream = data.get(off..off.checked_add(instruction_size)?)?;
    off += instruction_size;

    // bbox bitmap is the leading `4 * ceil(numGlyphs/32)` bytes of bbox_block.
    let bbox_bitmap_len = 4 * num_glyphs.div_ceil(32) as usize;
    let bbox_bitmap = bbox_block.get(..bbox_bitmap_len)?;
    let mut bbox_stream = Stream::new(bbox_block.get(bbox_bitmap_len..)?);

    // Optional overlapSimpleBitmap follows the instruction stream.
    let overlap_bitmap = if option_flags & 0x0001 != 0 {
        let len = num_glyphs.div_ceil(8) as usize;
        data.get(off..off.checked_add(len)?)
    } else {
        None
    };

    let mut n_contour_s = Stream::new(n_contour);
    let mut n_points_s = Stream::new(n_points);
    let mut flag_s = Stream::new(flag_stream);
    let mut glyph_s = Stream::new(glyph_stream);
    let mut composite_s = Stream::new(composite_stream);
    let mut instr_s = Stream::new(instruction_stream);

    let mut glyf = Vec::new();
    let mut loca_offsets: Vec<u32> = Vec::with_capacity(num_glyphs as usize + 1);
    let mut x_mins: Vec<i16> = Vec::with_capacity(num_glyphs as usize);
    loca_offsets.push(0);

    for gid in 0..num_glyphs {
        let n_cont = n_contour_s.i16()?;
        let glyph_start = glyf.len();

        if n_cont == 0 {
            // Empty glyph: no data, loca repeats.
            x_mins.push(0);
            loca_offsets.push(glyf.len() as u32);
            continue;
        }

        let has_bbox = bbox_bit_set(bbox_bitmap, gid);

        if n_cont > 0 {
            // --- Simple glyph ---
            let n_cont = n_cont as usize;
            // Contour endpoints from nPointsStream.
            let mut end_pts = Vec::with_capacity(n_cont);
            let mut total_points = 0u32;
            for _ in 0..n_cont {
                let np = n_points_s.read_255_u16()? as u32;
                total_points += np;
                end_pts.push(total_points.wrapping_sub(1) as u16);
            }
            let n_points_total = total_points as usize;

            // Per-point flags.
            let raw_flags = flag_s.bytes(n_points_total)?;
            // Coordinates via triplet decode from glyphStream.
            let points = triplet_decode(raw_flags, &mut glyph_s, n_points_total)?;

            // Instruction length + bytes.
            let instr_len = glyph_s.read_255_u16()? as usize;
            let instructions = instr_s.bytes(instr_len)?;

            // Bounding box (explicit or computed).
            let (x_min, y_min, x_max, y_max) = if has_bbox {
                (
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                )
            } else {
                compute_bbox(&points)
            };
            x_mins.push(x_min);

            // Emit a standard simple-glyph record.
            emit_simple_glyph(
                &mut glyf,
                n_cont as u16,
                x_min,
                y_min,
                x_max,
                y_max,
                &end_pts,
                &points,
                instructions,
                overlap_bitmap
                    .map(|bm| bit_set_msb_first(bm, gid))
                    .unwrap_or(false),
            );
        } else {
            // --- Composite glyph (n_cont == -1) ---
            // Composite glyphs MUST carry an explicit bbox.
            let (x_min, y_min, x_max, y_max) = if has_bbox {
                (
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                    bbox_stream.i16()?,
                )
            } else {
                return None;
            };
            x_mins.push(x_min);

            // numberOfContours = -1.
            glyf.extend_from_slice(&(-1i16).to_be_bytes());
            glyf.extend_from_slice(&x_min.to_be_bytes());
            glyf.extend_from_slice(&y_min.to_be_bytes());
            glyf.extend_from_slice(&x_max.to_be_bytes());
            glyf.extend_from_slice(&y_max.to_be_bytes());

            // Copy components verbatim from compositeStream, tracking whether
            // any component declares WE_HAVE_INSTRUCTIONS.
            let mut have_instructions = false;
            loop {
                let flags = composite_s.u16()?;
                let _glyph_index = composite_s.u16()?;
                glyf.extend_from_slice(&flags.to_be_bytes());
                glyf.extend_from_slice(&_glyph_index.to_be_bytes());

                // Argument bytes: 2 each (words) or 1 each (bytes).
                let arg_bytes = if flags & 0x0001 != 0 { 4 } else { 2 };
                glyf.extend_from_slice(composite_s.bytes(arg_bytes)?);

                // Transform bytes.
                if flags & 0x0008 != 0 {
                    // WE_HAVE_A_SCALE: 1 F2Dot14.
                    glyf.extend_from_slice(composite_s.bytes(2)?);
                } else if flags & 0x0040 != 0 {
                    // WE_HAVE_AN_X_AND_Y_SCALE: 2 F2Dot14.
                    glyf.extend_from_slice(composite_s.bytes(4)?);
                } else if flags & 0x0080 != 0 {
                    // WE_HAVE_A_TWO_BY_TWO: 4 F2Dot14.
                    glyf.extend_from_slice(composite_s.bytes(8)?);
                }

                if flags & 0x0100 != 0 {
                    have_instructions = true;
                }
                if flags & 0x0020 == 0 {
                    // No MORE_COMPONENTS.
                    break;
                }
            }

            if have_instructions {
                let instr_len = glyph_s.read_255_u16()? as usize;
                let instructions = instr_s.bytes(instr_len)?;
                glyf.extend_from_slice(&(instr_len as u16).to_be_bytes());
                glyf.extend_from_slice(instructions);
            }
        }

        // Pad each glyph record to an even length (loca uses 2-byte units in
        // the short format and the spec recommends 2-byte alignment regardless).
        if (glyf.len() - glyph_start) & 1 == 1 {
            glyf.push(0);
        }
        loca_offsets.push(glyf.len() as u32);
    }

    // Build the loca table in the requested index format.
    let loca = build_loca(&loca_offsets, index_format);
    let _ = index_format;

    Some(GlyfReconstruction {
        glyf,
        loca,
        num_glyphs,
        x_mins,
    })
}

/// Is the bbox bit set for glyph `gid` (MSB-first within each byte)?
fn bbox_bit_set(bitmap: &[u8], gid: u16) -> bool {
    bit_set_msb_first(bitmap, gid)
}

fn bit_set_msb_first(bitmap: &[u8], index: u16) -> bool {
    let byte = (index / 8) as usize;
    let bit = 7 - (index % 8);
    bitmap
        .get(byte)
        .map(|b| (b >> bit) & 1 == 1)
        .unwrap_or(false)
}

/// WOFF2 §5.2 triplet decoding: rebuild absolute (x, y, on_curve) points.
fn triplet_decode(flags: &[u8], glyph_stream: &mut Stream, n_points: usize) -> Option<Vec<Point>> {
    let mut x = 0i32;
    let mut y = 0i32;
    let mut points = Vec::with_capacity(n_points);
    for &flag_byte in flags.iter().take(n_points) {
        let on_curve = (flag_byte >> 7) == 0;
        let flag = (flag_byte & 0x7f) as i32;
        let n_data = if flag < 84 {
            1
        } else if flag < 120 {
            2
        } else if flag < 124 {
            3
        } else {
            4
        };
        let data = glyph_stream.bytes(n_data)?;
        let (dx, dy) = if flag < 10 {
            (0, with_sign(flag, ((flag & 14) << 7) + data[0] as i32))
        } else if flag < 20 {
            (
                with_sign(flag, (((flag - 10) & 14) << 7) + data[0] as i32),
                0,
            )
        } else if flag < 84 {
            let b0 = flag - 20;
            let b1 = data[0] as i32;
            (
                with_sign(flag, 1 + (b0 & 0x30) + (b1 >> 4)),
                with_sign(flag >> 1, 1 + ((b0 & 0x0c) << 2) + (b1 & 0x0f)),
            )
        } else if flag < 120 {
            let b0 = flag - 84;
            (
                with_sign(flag, 1 + ((b0 / 12) << 8) + data[0] as i32),
                with_sign(flag >> 1, 1 + (((b0 % 12) >> 2) << 8) + data[1] as i32),
            )
        } else if flag < 124 {
            let b2 = data[1] as i32;
            (
                with_sign(flag, ((data[0] as i32) << 4) + (b2 >> 4)),
                with_sign(flag >> 1, ((b2 & 0x0f) << 8) + data[2] as i32),
            )
        } else {
            (
                with_sign(flag, ((data[0] as i32) << 8) + data[1] as i32),
                with_sign(flag >> 1, ((data[2] as i32) << 8) + data[3] as i32),
            )
        };
        x = x.wrapping_add(dx);
        y = y.wrapping_add(dy);
        points.push(Point { x, y, on_curve });
    }
    Some(points)
}

#[inline]
fn with_sign(flag: i32, base: i32) -> i32 {
    if flag & 1 != 0 {
        base
    } else {
        -base
    }
}

#[derive(Clone, Copy)]
struct Point {
    x: i32,
    y: i32,
    on_curve: bool,
}

fn compute_bbox(points: &[Point]) -> (i16, i16, i16, i16) {
    if points.is_empty() {
        return (0, 0, 0, 0);
    }
    let mut x_min = i32::MAX;
    let mut y_min = i32::MAX;
    let mut x_max = i32::MIN;
    let mut y_max = i32::MIN;
    for p in points {
        x_min = x_min.min(p.x);
        y_min = y_min.min(p.y);
        x_max = x_max.max(p.x);
        y_max = y_max.max(p.y);
    }
    (
        clamp_i16(x_min),
        clamp_i16(y_min),
        clamp_i16(x_max),
        clamp_i16(y_max),
    )
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// Emit a standard TrueType simple-glyph record into `out`.
#[allow(clippy::too_many_arguments)]
fn emit_simple_glyph(
    out: &mut Vec<u8>,
    n_contours: u16,
    x_min: i16,
    y_min: i16,
    x_max: i16,
    y_max: i16,
    end_pts: &[u16],
    points: &[Point],
    instructions: &[u8],
    first_overlap: bool,
) {
    out.extend_from_slice(&n_contours.to_be_bytes());
    out.extend_from_slice(&x_min.to_be_bytes());
    out.extend_from_slice(&y_min.to_be_bytes());
    out.extend_from_slice(&x_max.to_be_bytes());
    out.extend_from_slice(&y_max.to_be_bytes());
    for &e in end_pts {
        out.extend_from_slice(&e.to_be_bytes());
    }
    out.extend_from_slice(&(instructions.len() as u16).to_be_bytes());
    out.extend_from_slice(instructions);

    // Flags + delta-encoded X and Y, as the standard glyf format expects.
    const ON_CURVE: u8 = 0x01;
    const X_SHORT: u8 = 0x02;
    const Y_SHORT: u8 = 0x04;
    const X_SAME_OR_POS: u8 = 0x10;
    const Y_SAME_OR_POS: u8 = 0x20;
    const OVERLAP_SIMPLE: u8 = 0x40;

    let mut prev_x = 0i32;
    let mut prev_y = 0i32;
    let mut flags = Vec::with_capacity(points.len());
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for (i, p) in points.iter().enumerate() {
        let mut f = 0u8;
        if p.on_curve {
            f |= ON_CURVE;
        }
        if i == 0 && first_overlap {
            f |= OVERLAP_SIMPLE;
        }
        let dx = p.x - prev_x;
        let dy = p.y - prev_y;
        prev_x = p.x;
        prev_y = p.y;

        if dx == 0 {
            f |= X_SAME_OR_POS;
        } else if (-255..=255).contains(&dx) {
            f |= X_SHORT;
            if dx > 0 {
                f |= X_SAME_OR_POS;
            }
            xs.push(dx.unsigned_abs() as u8);
        } else {
            xs.extend_from_slice(&(dx as i16).to_be_bytes());
        }

        if dy == 0 {
            f |= Y_SAME_OR_POS;
        } else if (-255..=255).contains(&dy) {
            f |= Y_SHORT;
            if dy > 0 {
                f |= Y_SAME_OR_POS;
            }
            ys.push(dy.unsigned_abs() as u8);
        } else {
            ys.extend_from_slice(&(dy as i16).to_be_bytes());
        }
        flags.push(f);
    }
    out.extend_from_slice(&flags);
    out.extend_from_slice(&xs);
    out.extend_from_slice(&ys);
}

/// Build a loca table (`numGlyphs + 1` offsets) in the requested index format.
fn build_loca(offsets: &[u32], index_format: u16) -> Vec<u8> {
    let mut loca = Vec::new();
    if index_format == 0 {
        // Short format: offsets / 2 as u16.
        for &o in offsets {
            loca.extend_from_slice(&((o / 2) as u16).to_be_bytes());
        }
    } else {
        for &o in offsets {
            loca.extend_from_slice(&o.to_be_bytes());
        }
    }
    loca
}

// ---------------------------------------------------------------------------
// WOFF2 transformed hmtx → standard hmtx (W3C §5.4).
// ---------------------------------------------------------------------------

fn reconstruct_hmtx(
    data: &[u8],
    num_hmetrics: u16,
    num_glyphs: u16,
    glyf_xmins: &[i16],
) -> Option<Vec<u8>> {
    let mut s = Stream::new(data);
    let flags = s.u8()?;
    let lsb_absent = flags & 0x01 != 0; // bit 0: lsb[] reconstructed from xMin
    let _rsb_absent = flags & 0x02 != 0; // bit 1: leftSideBearing[] reconstructed

    let num_hmetrics = num_hmetrics as usize;
    let num_glyphs = num_glyphs as usize;

    // advanceWidth[numberOfHMetrics] is always present.
    let mut advances = Vec::with_capacity(num_hmetrics);
    for _ in 0..num_hmetrics {
        advances.push(s.u16()?);
    }

    // lsb[numberOfHMetrics]: explicit unless reconstructed from xMin.
    let mut lsbs = Vec::with_capacity(num_hmetrics);
    if lsb_absent {
        for i in 0..num_hmetrics {
            lsbs.push(*glyf_xmins.get(i).unwrap_or(&0));
        }
    } else {
        for _ in 0..num_hmetrics {
            lsbs.push(s.i16()?);
        }
    }

    // leftSideBearing[numGlyphs - numberOfHMetrics] for the remaining glyphs.
    let extra = num_glyphs.saturating_sub(num_hmetrics);
    let mut extra_lsbs = Vec::with_capacity(extra);
    if _rsb_absent {
        for i in 0..extra {
            extra_lsbs.push(*glyf_xmins.get(num_hmetrics + i).unwrap_or(&0));
        }
    } else {
        for _ in 0..extra {
            extra_lsbs.push(s.i16()?);
        }
    }

    let mut out = Vec::with_capacity(num_hmetrics * 4 + extra * 2);
    for i in 0..num_hmetrics {
        out.extend_from_slice(&advances[i].to_be_bytes());
        out.extend_from_slice(&lsbs[i].to_be_bytes());
    }
    for v in extra_lsbs {
        out.extend_from_slice(&v.to_be_bytes());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::truetype::TrueTypeFont;

    const TINY_WOFF2: [u8; 2252] = [
        0x77, 0x4f, 0x46, 0x32, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x08, 0xcc, 0x00, 0x0b, 0x00,
        0x00, 0x00, 0x00, 0x12, 0x3c, 0x00, 0x00, 0x08, 0x7d, 0x00, 0x02, 0x4d, 0xd3, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x06, 0x60, 0x00, 0x34, 0x08, 0x81, 0x28, 0x09, 0x9c, 0x0c, 0x0a, 0x81,
        0x54, 0x81, 0x51, 0x01, 0x36, 0x02, 0x24, 0x03, 0x06, 0x0b, 0x06, 0x00, 0x04, 0x20, 0x0c,
        0x81, 0x56, 0x1b, 0x6e, 0x11, 0x51, 0x94, 0x6b, 0x52, 0x0d, 0xf0, 0xc5, 0x81, 0x79, 0x3e,
        0xd3, 0x59, 0xd3, 0xd9, 0xa8, 0xac, 0x83, 0x45, 0x73, 0xb0, 0x7e, 0x1c, 0x3f, 0xf2, 0xa1,
        0x2c, 0xa5, 0x54, 0x94, 0xdd, 0xcd, 0xff, 0x7f, 0x9d, 0xe5, 0x7d, 0xef, 0x83, 0xc0, 0xc0,
        0x1a, 0x20, 0x79, 0x00, 0x35, 0x48, 0xb2, 0x8c, 0x8b, 0xe4, 0x99, 0xdd, 0x0d, 0x20, 0x97,
        0x84, 0x45, 0x95, 0x93, 0x2a, 0x95, 0x1c, 0x42, 0xa7, 0x4a, 0x7a, 0xe6, 0xa2, 0xc9, 0x56,
        0x39, 0x2b, 0x83, 0xe6, 0x76, 0xb7, 0x81, 0xa7, 0x40, 0x68, 0xd6, 0xc6, 0x47, 0x54, 0xb5,
        0x57, 0x87, 0xff, 0x47, 0x5b, 0xaf, 0xc6, 0xa2, 0x02, 0x92, 0x93, 0x5c, 0xe5, 0x05, 0x48,
        0x13, 0x20, 0xc3, 0x84, 0xde, 0x52, 0xc2, 0xeb, 0x1f, 0x92, 0x82, 0x92, 0x77, 0xbc, 0x45,
        0xfd, 0x8b, 0x7a, 0xba, 0xea, 0x68, 0xcc, 0xa6, 0x8a, 0xcd, 0xa6, 0xb4, 0x6a, 0xc9, 0x72,
        0x28, 0xbe, 0x65, 0xe2, 0x28, 0xad, 0x24, 0xff, 0xa0, 0x1d, 0x96, 0x91, 0xdc, 0x2b, 0xc4,
        0xf7, 0xff, 0x5d, 0xeb, 0xb5, 0xef, 0xbe, 0xe4, 0x13, 0xba, 0x6a, 0x02, 0xe1, 0x01, 0x6d,
        0x9d, 0xdb, 0x66, 0x96, 0x92, 0x9d, 0xc2, 0xec, 0x7e, 0x04, 0x52, 0x05, 0x3e, 0x9e, 0x50,
        0x02, 0xb1, 0x23, 0x32, 0xba, 0xf5, 0xb5, 0xae, 0xd6, 0xa3, 0x8d, 0xe1, 0x7a, 0x97, 0xa1,
        0x78, 0x16, 0xb0, 0xfa, 0xa7, 0x80, 0xe4, 0x26, 0x3d, 0x0f, 0x05, 0xf0, 0x93, 0xdc, 0x04,
        0x70, 0x60, 0x5b, 0x80, 0x2e, 0xc1, 0x02, 0x05, 0x75, 0x0c, 0x58, 0xd8, 0xd6, 0x25, 0xdc,
        0x74, 0x7f, 0x05, 0x40, 0x1b, 0x65, 0x94, 0xc5, 0xff, 0x17, 0xbf, 0x04, 0x6c, 0x69, 0xe4,
        0xe1, 0x66, 0xf1, 0x28, 0x7f, 0x9f, 0xe4, 0x42, 0x6b, 0x5e, 0x20, 0x74, 0xde, 0x16, 0xa0,
        0x3e, 0x49, 0xc2, 0x85, 0x04, 0xe5, 0x94, 0xa3, 0xc8, 0x1c, 0xd3, 0xa8, 0x0d, 0xc2, 0x41,
        0xef, 0xb6, 0x74, 0x54, 0xa5, 0x79, 0x13, 0x1e, 0x05, 0xa2, 0x83, 0xd2, 0x9f, 0xd1, 0x05,
        0x69, 0x2e, 0xda, 0x35, 0xd8, 0x7f, 0xe2, 0xf7, 0xfd, 0x7e, 0xff, 0xc9, 0x33, 0x7d, 0xaa,
        0x6c, 0xc5, 0xa1, 0xf7, 0x1c, 0x29, 0xe8, 0x43, 0x9b, 0x11, 0x06, 0xd1, 0x3d, 0xb4, 0xdf,
        0xb6, 0xf0, 0xe9, 0x11, 0x71, 0xf5, 0xd5, 0xb3, 0xf7, 0x03, 0x4f, 0xe0, 0x54, 0xde, 0x14,
        0xe9, 0x87, 0xe7, 0x48, 0xde, 0xfc, 0x80, 0xbe, 0xfb, 0x2b, 0xb3, 0xb4, 0xca, 0xf8, 0x4b,
        0x3d, 0x6a, 0x3e, 0xf5, 0x0c, 0x47, 0x7b, 0x61, 0x8a, 0x29, 0x57, 0x1e, 0xec, 0x41, 0x88,
        0x11, 0xec, 0x80, 0xaa, 0xad, 0x17, 0x90, 0x54, 0xab, 0x27, 0x08, 0xb8, 0xf5, 0x98, 0x60,
        0x58, 0xb4, 0x98, 0x10, 0xd0, 0xf4, 0x48, 0xcc, 0x5d, 0x39, 0xe3, 0x2b, 0x72, 0x54, 0xad,
        0xde, 0x79, 0x6e, 0xe5, 0x7d, 0xa4, 0x97, 0x30, 0x7f, 0x3b, 0x15, 0x71, 0x87, 0xb9, 0x6a,
        0x13, 0xf2, 0xfa, 0x74, 0xe1, 0x9e, 0x1e, 0x56, 0xa7, 0xf8, 0x73, 0x2b, 0xc2, 0x5c, 0x2d,
        0xc9, 0xf9, 0xb3, 0x7b, 0xb7, 0x04, 0xd3, 0x7c, 0xc9, 0xe9, 0xec, 0xaa, 0xba, 0x81, 0xa3,
        0xf9, 0x61, 0x59, 0x1c, 0xed, 0x49, 0x94, 0x64, 0x13, 0x91, 0x14, 0x98, 0x9b, 0x52, 0x1c,
        0xae, 0x40, 0xaf, 0x1a, 0x34, 0x89, 0xf6, 0x35, 0xb2, 0xfc, 0xb5, 0x8c, 0xeb, 0x38, 0x37,
        0x87, 0x0d, 0xfa, 0x20, 0x47, 0x44, 0x94, 0x93, 0xbf, 0x73, 0x80, 0xc6, 0x61, 0x6e, 0x1a,
        0x47, 0xaf, 0xc9, 0x3a, 0x98, 0x83, 0xac, 0xb9, 0x6a, 0xd6, 0x5e, 0x8a, 0xba, 0x29, 0x6c,
        0x13, 0xe3, 0xb5, 0x87, 0xe2, 0xc2, 0xf0, 0x9c, 0x99, 0x43, 0x53, 0xbb, 0x81, 0x8e, 0x9a,
        0x9f, 0x6c, 0x4e, 0xb2, 0x75, 0xa5, 0x1a, 0xd7, 0xdf, 0xd8, 0x2e, 0x5e, 0xa1, 0x95, 0x8c,
        0x5a, 0x0a, 0xfc, 0x36, 0x70, 0x10, 0xe7, 0x01, 0x1c, 0xcf, 0xe0, 0x40, 0xaf, 0x23, 0xcd,
        0x1f, 0x6a, 0xee, 0x6f, 0xda, 0x7f, 0xd6, 0x9e, 0xa7, 0xe5, 0x80, 0xba, 0xc7, 0x73, 0x25,
        0x57, 0x94, 0x32, 0xb5, 0x02, 0x65, 0x29, 0xa5, 0xa0, 0xf2, 0x0e, 0x71, 0x68, 0x5e, 0x12,
        0x0a, 0xda, 0x08, 0x19, 0x6d, 0x12, 0x06, 0xf7, 0x19, 0xc3, 0x1d, 0xd3, 0x3d, 0xc4, 0x58,
        0xa0, 0x9a, 0xd0, 0x2c, 0x77, 0x35, 0x94, 0x55, 0xdd, 0xe8, 0xb8, 0x00, 0xaa, 0xc8, 0x76,
        0x28, 0x82, 0xb2, 0x65, 0xa0, 0xea, 0x58, 0x88, 0xda, 0x8d, 0x80, 0x8f, 0x46, 0xcd, 0xee,
        0x7f, 0xa8, 0xd9, 0xe7, 0xfa, 0x03, 0x59, 0xee, 0xe7, 0xba, 0x0f, 0xed, 0xa4, 0x64, 0x2b,
        0xb1, 0x88, 0xe0, 0x2e, 0x43, 0x19, 0x22, 0x41, 0x76, 0xae, 0x96, 0x30, 0x80, 0xc5, 0x77,
        0x17, 0xb0, 0xa8, 0xc9, 0x7c, 0x6b, 0x96, 0xbb, 0x09, 0x0c, 0x79, 0x83, 0x3b, 0x47, 0xa8,
        0x0b, 0x55, 0x8c, 0xdd, 0x84, 0x05, 0x69, 0x23, 0x92, 0x3c, 0x3c, 0x97, 0x72, 0x85, 0x62,
        0x29, 0x22, 0xa8, 0x05, 0x16, 0xaa, 0xeb, 0x1c, 0xdf, 0xc8, 0xc6, 0x62, 0x4d, 0x48, 0x75,
        0xe3, 0xef, 0x44, 0xe5, 0xa1, 0xd8, 0x60, 0x65, 0xb2, 0xe3, 0x31, 0xdd, 0x9b, 0xd2, 0x40,
        0x15, 0x19, 0x55, 0x9b, 0x79, 0xa8, 0x69, 0x62, 0x78, 0x10, 0x3d, 0x42, 0xa9, 0xc7, 0x5d,
        0x25, 0x60, 0x5a, 0x06, 0x22, 0x50, 0xf6, 0x3a, 0x99, 0x99, 0x24, 0xa2, 0x70, 0x18, 0x0e,
        0x09, 0x0c, 0xdb, 0x50, 0x52, 0x15, 0xea, 0xab, 0x3c, 0x4a, 0x8c, 0x89, 0x38, 0xd8, 0xe7,
        0x18, 0x2c, 0x96, 0x77, 0x62, 0x22, 0x85, 0xce, 0x3d, 0x07, 0x5c, 0xd5, 0xd6, 0x89, 0x34,
        0xa0, 0x2a, 0x91, 0xe2, 0xf7, 0xe0, 0x68, 0x16, 0x29, 0xf7, 0x42, 0x9a, 0x82, 0xe3, 0x48,
        0x17, 0x26, 0x32, 0x70, 0x59, 0x8c, 0x79, 0x8d, 0x7a, 0x2e, 0xe5, 0xca, 0x79, 0xa8, 0x91,
        0x61, 0xd5, 0x7c, 0x99, 0x91, 0x71, 0xb7, 0x44, 0x16, 0x3f, 0xfc, 0x96, 0xbb, 0xfc, 0x5e,
        0x97, 0xd7, 0x48, 0xfc, 0xd8, 0x69, 0x22, 0x47, 0x2d, 0xb9, 0x9f, 0x7f, 0xf8, 0x75, 0x74,
        0x42, 0x1c, 0x45, 0x6e, 0xc1, 0x3c, 0x3a, 0xb2, 0xab, 0xdf, 0xfd, 0x59, 0xdb, 0xed, 0xa1,
        0xe6, 0x64, 0xb2, 0x80, 0x2a, 0x1c, 0xde, 0x53, 0x0d, 0xd2, 0x8f, 0x09, 0x2d, 0x98, 0x10,
        0xb1, 0xcf, 0x9d, 0xa4, 0x5a, 0x91, 0xe1, 0xf3, 0x93, 0x96, 0x74, 0xdb, 0xf4, 0x6e, 0x34,
        0xa8, 0xb9, 0xed, 0x80, 0x21, 0xde, 0x70, 0xb6, 0x96, 0x86, 0x85, 0x14, 0x77, 0xa4, 0xcd,
        0x5e, 0x2a, 0x2c, 0xb0, 0x91, 0x88, 0xf1, 0x8e, 0x1d, 0xc8, 0xd9, 0x1a, 0x22, 0xf0, 0x17,
        0x52, 0x9d, 0xdf, 0x5a, 0x01, 0x67, 0x3a, 0xf8, 0xe0, 0xb7, 0x4c, 0xd6, 0x5d, 0x1b, 0x92,
        0xee, 0x24, 0x1c, 0x55, 0x5d, 0xf5, 0x59, 0xe7, 0xd2, 0x59, 0xc9, 0x9c, 0xb9, 0x74, 0xc6,
        0xdd, 0xb9, 0x40, 0xf9, 0x87, 0x86, 0x5a, 0xcf, 0x0f, 0x25, 0x4f, 0xe5, 0xea, 0xab, 0x25,
        0xe1, 0xa0, 0xa3, 0xf0, 0x6e, 0xa2, 0xc3, 0xce, 0x44, 0x9d, 0xcc, 0xd4, 0xc5, 0x82, 0xba,
        0x59, 0x52, 0x0f, 0x2b, 0xea, 0x65, 0x4d, 0x7d, 0x6c, 0x50, 0x3f, 0x9b, 0x34, 0xc0, 0x56,
        0xde, 0x14, 0x4c, 0x06, 0x97, 0x45, 0xd7, 0x1f, 0x7e, 0xcb, 0xa6, 0x01, 0xfa, 0xb8, 0x62,
        0x76, 0x4f, 0x78, 0x80, 0x08, 0x47, 0xfe, 0x2e, 0x3b, 0x1d, 0x31, 0x2a, 0xff, 0xce, 0x98,
        0x8b, 0x33, 0xa4, 0xb2, 0xac, 0x25, 0x50, 0x7b, 0x32, 0x2a, 0x33, 0xa8, 0x8c, 0x28, 0xc2,
        0xc5, 0x20, 0x0c, 0x43, 0x30, 0x0c, 0xc3, 0x30, 0x02, 0xc3, 0x28, 0x0c, 0x63, 0x30, 0x8c,
        0xc3, 0x30, 0x01, 0xc3, 0x24, 0xac, 0x78, 0x06, 0x1a, 0x95, 0x96, 0x66, 0x1b, 0x57, 0x3a,
        0xab, 0x56, 0x46, 0x0a, 0xac, 0xc4, 0x5a, 0x44, 0x95, 0x19, 0xbd, 0xad, 0x98, 0x03, 0xb3,
        0x79, 0x3a, 0x3b, 0xbb, 0x89, 0x79, 0x48, 0x3b, 0x49, 0xb4, 0xc8, 0xda, 0x6d, 0xbb, 0xb9,
        0x8f, 0x1c, 0x65, 0x02, 0x07, 0x68, 0xc1, 0x3d, 0x14, 0xf5, 0xe4, 0xc0, 0x81, 0xd6, 0x5d,
        0x89, 0xf9, 0xdd, 0xae, 0x74, 0x7e, 0x93, 0x3f, 0x28, 0x25, 0xb5, 0x17, 0x59, 0x34, 0x72,
        0x5c, 0xf1, 0x12, 0xf4, 0x69, 0xee, 0x73, 0x97, 0x71, 0xc6, 0x4c, 0x73, 0x90, 0xee, 0xbe,
        0xa1, 0xcc, 0x8f, 0xf2, 0x2b, 0x82, 0xf3, 0x18, 0x87, 0x2e, 0xb5, 0x2f, 0xed, 0x76, 0x5f,
        0xd2, 0xf9, 0xf2, 0x9e, 0x91, 0x15, 0x68, 0xd4, 0xae, 0x0c, 0x19, 0xa0, 0xda, 0x7f, 0x47,
        0x89, 0x55, 0x0c, 0xd7, 0xc6, 0xc4, 0x1a, 0xbc, 0x10, 0xe2, 0x21, 0xc8, 0x37, 0x5d, 0x47,
        0x67, 0x71, 0x5e, 0xd7, 0xa0, 0x1d, 0x18, 0x72, 0x38, 0xbe, 0x9b, 0x9f, 0x3c, 0xf2, 0x9d,
        0x35, 0xe5, 0x07, 0x21, 0x16, 0xc8, 0x7d, 0x59, 0x53, 0x9d, 0xbf, 0xfc, 0x4e, 0x62, 0x03,
        0xc0, 0xa1, 0xc3, 0x1d, 0x19, 0xc8, 0x80, 0xde, 0xbc, 0xc7, 0xa5, 0x42, 0x8c, 0xef, 0x3e,
        0x9c, 0x67, 0x5c, 0x1a, 0xe7, 0xb5, 0xdf, 0x0a, 0x36, 0x43, 0x1a, 0xfa, 0x3a, 0xa1, 0xdd,
        0x43, 0xdc, 0x54, 0x5c, 0x76, 0x9d, 0x45, 0x70, 0x93, 0x1f, 0x49, 0xbb, 0x68, 0xc1, 0x76,
        0x87, 0x72, 0x71, 0xa5, 0xb8, 0x9b, 0xb0, 0xa9, 0x79, 0x28, 0xd4, 0x29, 0xda, 0xb6, 0xa2,
        0x79, 0xc9, 0xd6, 0x54, 0x41, 0xf9, 0x64, 0xba, 0xa4, 0x64, 0x9d, 0x0c, 0xc5, 0x36, 0xed,
        0x33, 0xc1, 0x69, 0xa1, 0x31, 0xf5, 0x6e, 0x37, 0x64, 0xb8, 0x47, 0x6a, 0x26, 0x21, 0x8b,
        0x6d, 0xdb, 0xed, 0x60, 0xd8, 0xdc, 0xc2, 0x61, 0xb5, 0x52, 0xda, 0x66, 0x55, 0x63, 0xcc,
        0x68, 0xf3, 0x35, 0x55, 0x20, 0x87, 0x56, 0xd1, 0x50, 0x31, 0x17, 0x22, 0x4b, 0x6b, 0x4d,
        0x64, 0x44, 0x0b, 0x9b, 0xe7, 0x6e, 0x3a, 0x20, 0xbd, 0xb2, 0x92, 0xd8, 0xe2, 0xc7, 0x52,
        0x92, 0x50, 0x25, 0xc3, 0xc6, 0xdb, 0xde, 0x11, 0xdb, 0x21, 0x6c, 0x8c, 0x7f, 0x6b, 0x54,
        0xc2, 0xfe, 0xe6, 0x1f, 0x8f, 0x6f, 0x15, 0xc8, 0xcd, 0xed, 0x1b, 0x82, 0x93, 0x79, 0xd0,
        0x68, 0x31, 0xc0, 0x1e, 0xf9, 0x42, 0x89, 0x23, 0x13, 0x1d, 0x94, 0x49, 0xdf, 0xbd, 0xbf,
        0xb8, 0x05, 0x39, 0xac, 0xc6, 0x7b, 0x24, 0x66, 0x5c, 0xed, 0x27, 0x96, 0x72, 0x93, 0x4a,
        0x89, 0xec, 0x90, 0x15, 0xb7, 0x1e, 0x8b, 0xed, 0x15, 0x73, 0x55, 0x00, 0xd3, 0xc0, 0xc9,
        0x9f, 0xbc, 0xa5, 0x83, 0x79, 0x1f, 0x4e, 0x4a, 0xd6, 0x58, 0x86, 0x3e, 0xa7, 0x73, 0xef,
        0x26, 0xca, 0x88, 0xf5, 0x4f, 0xc2, 0x7e, 0xfa, 0x4a, 0xf8, 0x11, 0x87, 0x3a, 0xe2, 0x2a,
        0xd8, 0xc8, 0x44, 0x1b, 0xd6, 0xc0, 0x66, 0xce, 0x2e, 0x55, 0xe9, 0xd9, 0x76, 0xce, 0x6b,
        0xf0, 0x46, 0x68, 0x0e, 0xcc, 0x85, 0xa2, 0x7a, 0x08, 0x8e, 0x8e, 0xda, 0x85, 0xfb, 0x35,
        0x8d, 0xb8, 0x7b, 0xf1, 0x95, 0x92, 0xb6, 0x36, 0x8d, 0x1a, 0xec, 0x76, 0x1a, 0x3f, 0xf0,
        0x06, 0xba, 0xdd, 0x17, 0x0e, 0x1c, 0xb5, 0xfe, 0x25, 0x76, 0x9a, 0xac, 0xca, 0xa5, 0xee,
        0x65, 0x07, 0x8e, 0x76, 0x3b, 0x44, 0x9d, 0x49, 0x8d, 0x25, 0xaf, 0xb4, 0x27, 0x0f, 0x1b,
        0xef, 0x72, 0x46, 0xef, 0xa5, 0xb8, 0xe6, 0xd4, 0x68, 0xed, 0x56, 0xec, 0x36, 0x1b, 0xb0,
        0xcf, 0x2e, 0x5c, 0xfa, 0xc1, 0x45, 0x56, 0xb8, 0xca, 0xfd, 0x39, 0xbf, 0x48, 0xf3, 0xe4,
        0xaa, 0x38, 0x2a, 0x9c, 0x5e, 0x0b, 0x44, 0x70, 0x42, 0x4b, 0x53, 0xe8, 0xf2, 0x42, 0x9b,
        0x95, 0xdc, 0xd7, 0x83, 0x53, 0x1f, 0x72, 0x67, 0xdc, 0xc6, 0x93, 0x43, 0x97, 0x73, 0x2c,
        0xe8, 0xf1, 0x94, 0xda, 0x09, 0x0f, 0x39, 0x1b, 0x79, 0xbd, 0xd7, 0x81, 0x46, 0x07, 0x1f,
        0x13, 0xf9, 0xa3, 0x09, 0x04, 0x98, 0x4c, 0x10, 0x87, 0x3d, 0x1c, 0x52, 0x86, 0xc2, 0xd1,
        0x04, 0x22, 0x4c, 0x8d, 0xa3, 0xde, 0x1b, 0x00, 0xaa, 0x2d, 0x62, 0x82, 0x28, 0x1e, 0x43,
        0x20, 0x21, 0x88, 0x92, 0xc9, 0x00, 0x21, 0x25, 0x88, 0xd2, 0x31, 0x04, 0x32, 0x82, 0x4c,
        0x36, 0x5d, 0xd4, 0x39, 0x87, 0x3a, 0xe7, 0x51, 0xe7, 0x02, 0xea, 0xa6, 0x58, 0x63, 0x2f,
        0x97, 0x54, 0x4d, 0xe5, 0x68, 0x02, 0x15, 0xa6, 0x40, 0x35, 0x6f, 0xa8, 0x89, 0x1a, 0x6a,
        0xa2, 0x8e, 0x9a, 0x68, 0xa0, 0x26, 0x9a, 0xa8, 0x89, 0x16, 0x6a, 0xa2, 0x8d, 0x9a, 0xe8,
        0xa0, 0x56, 0xdf, 0xf5, 0x5a, 0xd8, 0x12, 0x62, 0xa0, 0x47, 0x71, 0x42, 0x65, 0x90, 0x7d,
        0xd8, 0xcd, 0x80, 0xf1, 0x16, 0x05, 0xa2, 0x74, 0xe8, 0xb5, 0x88, 0xa3, 0x51, 0x18, 0xc5,
        0x38, 0x8c, 0x34, 0x49, 0xc7, 0x76, 0xcc, 0x98, 0xca, 0x6d, 0xcc, 0xe4, 0xb6, 0x3d, 0x77,
        0x42, 0xd0, 0x02, 0x84, 0x22, 0x97, 0x6a, 0xb3, 0x57, 0x06, 0x42, 0xaf, 0xb7, 0x08, 0x1f,
        0x1b, 0xe9, 0x63, 0x2b, 0x7d, 0xec, 0xa4, 0x8f, 0xbd, 0xf4, 0x71, 0x90, 0x3e, 0x8e, 0xd2,
        0xc7, 0x29, 0xa2, 0x84, 0xb3, 0x2c, 0xe1, 0x22, 0x4b, 0xb8, 0xca, 0x12, 0x6e, 0xb2, 0x84,
        0xbb, 0x2c, 0xe1, 0x21, 0x4b, 0xc5, 0x4f, 0xef, 0x75, 0xe3, 0xdb, 0x6d, 0x83, 0x50, 0xf7,
        0xbc, 0xae, 0x62, 0xd7, 0xfd, 0x9b, 0x43, 0xc5, 0x9e, 0xe2, 0xce, 0x4e, 0x65, 0x2a, 0xd4,
        0x5d, 0x1d, 0x8a, 0xe1, 0xa3, 0xf7, 0x3d, 0xb0, 0x83, 0xd3, 0x38, 0x44, 0x33, 0x22, 0x25,
        0x22, 0x15, 0x22, 0x7b, 0x90, 0x3f, 0x0e, 0xd0, 0x3f, 0xef, 0x79, 0x08, 0xb0, 0x14, 0xc2,
        0xb4, 0x62, 0x2a, 0x21, 0xaf, 0xb8, 0x31, 0x7e, 0x3e, 0xa1, 0x62, 0x97, 0xc6, 0xce, 0xdb,
        0xa7, 0xed, 0x93, 0x67, 0x63, 0xa7, 0x5f, 0xf9, 0xda, 0x79, 0xf9, 0xe5, 0xee, 0x7e, 0xff,
        0xea, 0xf7, 0xb7, 0xdf, 0x34, 0xb5, 0xe1, 0xe5, 0xee, 0xf6, 0x36, 0xe7, 0x17, 0x3e, 0xfa,
        0xdf, 0xff, 0xe1, 0x77, 0x67, 0x67, 0x3f, 0xfd, 0x69, 0xbd, 0x48, 0x39, 0xf1, 0xee, 0xea,
        0xaf, 0x51, 0xe6, 0x54, 0x6f, 0x6f, 0xfb, 0xff, 0xb8, 0x08, 0x00, 0x08, 0x66, 0x2c, 0xc9,
        0x82, 0xe9, 0x20, 0x00, 0x16, 0xe0, 0x6b, 0x21, 0x88, 0xc4, 0x19, 0x08, 0x41, 0x97, 0x4a,
        0x22, 0x41, 0x47, 0x00, 0xd3, 0xd0, 0x0a, 0x02, 0x22, 0xa3, 0x74, 0xc7, 0x54, 0x30, 0x91,
        0x20, 0x09, 0x3a, 0x08, 0x09, 0x26, 0xc9, 0xd7, 0x01, 0x58, 0x3b, 0xa4, 0x04, 0x13, 0xad,
        0xd3, 0x61, 0xc0, 0x32, 0x61, 0xc0, 0xc8, 0x64, 0x32, 0xda, 0xea, 0x9a, 0x02, 0x01, 0xe0,
        0x43, 0x5f, 0x94, 0x3f, 0x7c, 0xad, 0x7e, 0x69, 0x6a, 0xeb, 0x3f, 0x33, 0x25, 0xfe, 0x05,
        0x80, 0x5f, 0xfe, 0x3d, 0xde, 0x0f, 0x00, 0x7f, 0x5f, 0xb1, 0xf8, 0x19, 0x16, 0x3e, 0xf7,
        0x37, 0x6a, 0x06, 0x10, 0xb9, 0xff, 0x12, 0x70, 0xb8, 0x89, 0xa0, 0x85, 0xa1, 0x3f, 0xeb,
        0x35, 0xf9, 0xd2, 0xae, 0xd5, 0xcd, 0x66, 0x64, 0x20, 0xae, 0xcc, 0xfc, 0xb2, 0xb8, 0x80,
        0x67, 0x00, 0x75, 0x59, 0x32, 0x4e, 0xbd, 0xfa, 0x81, 0x77, 0xf1, 0xf9, 0xf1, 0x66, 0x8b,
        0xe8, 0xd9, 0x9d, 0x90, 0x9e, 0x45, 0xb7, 0xb9, 0xad, 0x05, 0xa3, 0xf2, 0x36, 0x50, 0x9e,
        0x2f, 0x2a, 0x4c, 0x12, 0xee, 0x11, 0xc4, 0x0f, 0xcb, 0x86, 0x5c, 0x31, 0x47, 0xcd, 0x4e,
        0x43, 0xd9, 0xe4, 0x2b, 0x1c, 0xe7, 0xfd, 0xec, 0xeb, 0x19, 0x3d, 0xa0, 0x7e, 0xb7, 0x8e,
        0x57, 0x3e, 0xce, 0xb5, 0x33, 0xed, 0x44, 0xdb, 0x6a, 0xeb, 0x00, 0x0a, 0xb1, 0x96, 0x32,
        0xe9, 0x08, 0x8e, 0xca, 0xdc, 0xf6, 0x3d, 0x08, 0xa0, 0xd6, 0x1a, 0xa6, 0xc7, 0x8f, 0x35,
        0xc3, 0xe0, 0xf1, 0x26, 0x8d, 0xae, 0xe2, 0xf1, 0x9c, 0x77, 0x4d, 0xb4, 0x4d, 0x50, 0xdb,
        0xe9, 0x69, 0x99, 0xfa, 0x06, 0xef, 0x9b, 0x8f, 0x81, 0x24, 0x36, 0xed, 0xca, 0x93, 0x9e,
        0xea, 0xf0, 0xd0, 0x41, 0xfc, 0x28, 0xef, 0xe5, 0x35, 0x3d, 0xae, 0xbb, 0x95, 0x4a, 0x4e,
        0xbe, 0xcb, 0x28, 0xe4, 0x33, 0x2d, 0x86, 0x34, 0x7d, 0x43, 0x5d, 0xa5, 0x51, 0xab, 0x01,
        0x00, 0x00,
    ];

    /// A real (fontTools-produced) WOFF2 of a 2-glyph subset (.notdef + 'A')
    /// of JetBrains Mono. Exercises the full glyf/loca transform pipeline.
    #[test]
    fn woff2_tiny_subset_reconstructs() {
        let sfnt = sfnt_from_web_font(&TINY_WOFF2).expect("reconstruct sfnt from woff2");
        // The reconstructed bytes must be a parseable sfnt.
        let font = TrueTypeFont::parse(&sfnt).expect("parse reconstructed sfnt");
        assert_eq!(font.num_glyphs(), 2, "glyph count");
        // cmap: 'A' maps to glyph 1.
        assert_eq!(font.gid_for_unicode('A' as u32), Some(1));
        // The 'A' outline reconstructed from the triplet streams is non-empty.
        assert!(!font.glyph_polygons(1).is_empty(), "glyph A has an outline");
    }

    const HAND_WOFF2: [u8; 94] = [
        0x77, 0x4f, 0x46, 0x32, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x24, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x01, 0x36, 0x04, 0x06, 0x3f, 0x74, 0x65, 0x73, 0x74, 0x04, 0x1b, 0x3f,
        0x00, 0xf8, 0x8f, 0x94, 0xab, 0x6d, 0xbc, 0x13, 0xd1, 0xe4, 0xee, 0xf7, 0xf3, 0x58, 0x03,
        0x0b, 0x5c, 0x2b, 0x9e, 0xfa, 0x12, 0xa5, 0xaa, 0x42, 0x20, 0xa5, 0xab, 0x00, 0x70, 0x23,
        0x7d, 0x2c, 0x7e, 0x18,
    ];

    /// Hand-built minimal WOFF2 (head + maxp + a private 'test' table, all with
    /// the null transform) — checks the header, the table directory with both a
    /// known-tag flag byte and an explicit-tag (0x3f) entry, UIntBase128 length
    /// decoding, the single Brotli stream, and raw (untransformed) table copy.
    #[test]
    fn woff2_hand_built_minimal_directory() {
        let sfnt = sfnt_from_web_font(&HAND_WOFF2).expect("reconstruct minimal woff2");
        // sfnt header: TrueType flavor, 3 tables.
        assert_eq!(&sfnt[0..4], &[0x00, 0x01, 0x00, 0x00]);
        assert_eq!(u16::from_be_bytes([sfnt[4], sfnt[5]]), 3, "numTables");
        // The three tables are present and the directory is tag-sorted.
        let mut tags = Vec::new();
        for i in 0..3 {
            let de = 12 + i * 16;
            tags.push([sfnt[de], sfnt[de + 1], sfnt[de + 2], sfnt[de + 3]]);
        }
        assert_eq!(tags[0], *b"head");
        assert_eq!(tags[1], *b"maxp");
        assert_eq!(tags[2], *b"test");
        // unitsPerEm survived in the head table.
        let head_off = {
            let de = 12; // first entry == head
            u32::from_be_bytes([sfnt[de + 8], sfnt[de + 9], sfnt[de + 10], sfnt[de + 11]]) as usize
        };
        let upm = u16::from_be_bytes([sfnt[head_off + 18], sfnt[head_off + 19]]);
        assert_eq!(upm, 1000);
    }

    #[test]
    fn raw_sfnt_passes_through() {
        // A buffer already starting with the TrueType magic is returned verbatim.
        let raw = vec![0x00, 0x01, 0x00, 0x00, 0xDE, 0xAD, 0xBE, 0xEF];
        assert_eq!(sfnt_from_web_font(&raw), Some(raw.clone()));
        // OTTO (CFF OpenType) is likewise passed through.
        let otto = vec![b'O', b'T', b'T', b'O', 0x01, 0x02, 0x03, 0x04];
        assert_eq!(sfnt_from_web_font(&otto), Some(otto));
    }

    #[test]
    fn unsupported_or_truncated_returns_none() {
        assert_eq!(sfnt_from_web_font(b""), None);
        assert_eq!(sfnt_from_web_font(b"\x00\x01\x00"), None); // < 4 bytes
        assert_eq!(sfnt_from_web_font(b"RIFFxxxx"), None); // unknown signature
                                                           // A 'wOF2' signature with a truncated body must fail gracefully.
        assert_eq!(sfnt_from_web_font(b"wOF2\x00\x01\x00\x00"), None);
    }

    #[test]
    fn uint_base128_decoding() {
        // 0x82, 0x2c == 300 (the spec's worked example).
        assert_eq!(read_uint_base128(&[0x82, 0x2c], 0), Some((300, 2)));
        // Single-byte values.
        assert_eq!(read_uint_base128(&[0x00], 0), Some((0, 1)));
        assert_eq!(read_uint_base128(&[0x7f], 0), Some((127, 1)));
        // Leading 0x80 is rejected.
        assert_eq!(read_uint_base128(&[0x80, 0x01], 0), None);
        // A sequence longer than 5 bytes is rejected.
        assert_eq!(
            read_uint_base128(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00], 0),
            None
        );
        // Truncated (fewer bytes than the continuation flags demand).
        assert_eq!(read_uint_base128(&[0x82], 0), None);
        // Overflow guard: five continuation bytes whose accumulated value
        // would exceed 32 bits is rejected.
        assert_eq!(read_uint_base128(&[0xff, 0xff, 0xff, 0xff, 0x7f], 0), None);
        // Offset into the slice is honoured.
        assert_eq!(read_uint_base128(&[0xaa, 0x82, 0x2c], 1), Some((300, 2)));
    }

    // -- Big-endian readers --------------------------------------------------

    #[test]
    fn be_readers_bounds() {
        let b = [0x12, 0x34, 0x56, 0x78];
        assert_eq!(be16(&b, 0), Some(0x1234));
        assert_eq!(be16(&b, 2), Some(0x5678));
        assert_eq!(be16(&b, 3), None); // would read past the end
        assert_eq!(be32(&b, 0), Some(0x1234_5678));
        assert_eq!(be32(&b, 1), None);
        assert_eq!(be16(&[], 0), None);
        assert_eq!(be32(&[0, 0, 0], 0), None);
    }

    // -- tag / known-tag constant -------------------------------------------

    #[test]
    fn tag_packs_four_ascii_bytes() {
        assert_eq!(tag(b"head"), HEAD_TAG);
        assert_eq!(tag(b"glyf"), GLYF_TAG);
        assert_eq!(tag(b"loca"), LOCA_TAG);
        assert_eq!(tag(b"cmap"), KNOWN_TAGS[0]);
        // The known-tags table starts with cmap/head/hhea/hmtx.
        assert_eq!(KNOWN_TAGS[1], HEAD_TAG);
        assert_eq!(KNOWN_TAGS[3], HMTX_TAG);
    }

    // -- table_checksum ------------------------------------------------------

    #[test]
    fn table_checksum_sums_be_words_with_wrap() {
        // Two aligned big-endian words.
        let data = [0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02];
        assert_eq!(table_checksum(&data), 3);
        // Empty input → 0.
        assert_eq!(table_checksum(&[]), 0);
        // Trailing bytes (unaligned tail) are zero-extended into a final word.
        // [0xAB] → 0xAB00_0000.
        assert_eq!(table_checksum(&[0xAB]), 0xAB00_0000);
        assert_eq!(
            table_checksum(&[0x00, 0x00, 0x00, 0x01, 0xFF]),
            1 + 0xFF00_0000
        );
        // Wrapping addition overflows cleanly.
        let big = [0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x02];
        assert_eq!(table_checksum(&big), 1); // 0xFFFFFFFF + 2 wraps to 1
    }

    // -- build_sfnt ----------------------------------------------------------

    #[test]
    fn build_sfnt_lays_out_sorted_tables_with_head_adjustment() {
        // A 'head' table (>= 12 bytes so the checkSumAdjustment path runs) and a
        // 'maxp' table; pass them out of tag order to exercise the sort.
        let head = vec![0u8; 16];
        let maxp = vec![1u8, 2, 3, 4, 5]; // 5 bytes → padded to 8
        let out = build_sfnt(SFNT_TRUETYPE, vec![(MAXP_TAG, maxp), (HEAD_TAG, head)]);

        // Header.
        assert_eq!(&out[0..4], &SFNT_TRUETYPE.to_be_bytes());
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 2, "numTables");
        // searchRange = max_pow2(2)*16 = 32; entrySelector = log2(2) = 1.
        assert_eq!(u16::from_be_bytes([out[6], out[7]]), 32);
        assert_eq!(u16::from_be_bytes([out[8], out[9]]), 1);
        // rangeShift = 2*16 - 32 = 0.
        assert_eq!(u16::from_be_bytes([out[10], out[11]]), 0);

        // Directory is tag-sorted: head (0x68..) < maxp (0x6D..).
        assert_eq!(&out[12..16], &HEAD_TAG.to_be_bytes());
        assert_eq!(&out[28..32], &MAXP_TAG.to_be_bytes());

        // head's checkSumAdjustment was written (offset 8 within the head table).
        let head_off = u32::from_be_bytes([out[20], out[21], out[22], out[23]]) as usize;
        let csa = u32::from_be_bytes([
            out[head_off + 8],
            out[head_off + 9],
            out[head_off + 10],
            out[head_off + 11],
        ]);
        // The file checksum with the field zeroed plus the adjustment must equal
        // the magic 0xB1B0AFBA.
        let mut zeroed = out.clone();
        zeroed[head_off + 8..head_off + 12].copy_from_slice(&[0, 0, 0, 0]);
        let file_sum = table_checksum(&zeroed);
        assert_eq!(file_sum.wrapping_add(csa), 0xB1B0_AFBA);
    }

    #[test]
    fn build_sfnt_single_table_search_params() {
        let out = build_sfnt(SFNT_TRUETYPE, vec![(MAXP_TAG, vec![0u8; 4])]);
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 1, "numTables");
        // max_pow2 = 1 → searchRange 16, entrySelector 0, rangeShift 0.
        assert_eq!(u16::from_be_bytes([out[6], out[7]]), 16);
        assert_eq!(u16::from_be_bytes([out[8], out[9]]), 0);
        assert_eq!(u16::from_be_bytes([out[10], out[11]]), 0);
    }

    #[test]
    fn build_sfnt_short_head_skips_adjustment() {
        // A 'head' table shorter than 12 bytes must not trigger the
        // checkSumAdjustment write (no panic / out-of-bounds).
        let out = build_sfnt(SFNT_TRUETYPE, vec![(HEAD_TAG, vec![0u8; 4])]);
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 1);
    }

    // -- 255UInt16 (Stream::read_255_u16) -----------------------------------

    #[test]
    fn read_255_u16_all_branches() {
        // Direct one-byte value (< 253).
        let mut s = Stream::new(&[100]);
        assert_eq!(s.read_255_u16(), Some(100));
        // code 253 → next big-endian u16 verbatim.
        let mut s = Stream::new(&[253, 0x01, 0x02]);
        assert_eq!(s.read_255_u16(), Some(0x0102));
        // code 254 → next byte + 506.
        let mut s = Stream::new(&[254, 10]);
        assert_eq!(s.read_255_u16(), Some(516));
        // code 255 → next byte + 253.
        let mut s = Stream::new(&[255, 10]);
        assert_eq!(s.read_255_u16(), Some(263));
        // Truncated: code present but the trailing data is missing.
        let mut s = Stream::new(&[255]);
        assert_eq!(s.read_255_u16(), None);
    }

    #[test]
    fn stream_primitive_readers() {
        let mut s = Stream::new(&[0x01, 0x02, 0x03, 0xFF, 0xFE, 0xAA, 0xBB]);
        assert_eq!(s.u8(), Some(0x01));
        assert_eq!(s.u16(), Some(0x0203));
        assert_eq!(s.i16(), Some(-2)); // 0xFFFE
        assert_eq!(s.bytes(2), Some(&[0xAA, 0xBB][..]));
        // Exhausted.
        assert_eq!(s.u8(), None);
        assert_eq!(s.u16(), None);
        assert_eq!(s.bytes(1), None);
        // Overflow on the length arithmetic is handled.
        let mut s = Stream::new(&[0; 4]);
        assert_eq!(s.bytes(usize::MAX), None);
    }

    // -- with_sign -----------------------------------------------------------

    #[test]
    fn with_sign_uses_low_bit_of_flag() {
        assert_eq!(with_sign(1, 5), 5); // odd flag → positive
        assert_eq!(with_sign(3, 5), 5);
        assert_eq!(with_sign(0, 5), -5); // even flag → negative
        assert_eq!(with_sign(2, 5), -5);
        assert_eq!(with_sign(0, 0), 0);
    }

    // -- compute_bbox / clamp_i16 -------------------------------------------

    #[test]
    fn compute_bbox_empty_is_zero() {
        assert_eq!(compute_bbox(&[]), (0, 0, 0, 0));
    }

    #[test]
    fn compute_bbox_min_max_and_clamp() {
        let pts = [
            Point {
                x: -3,
                y: 10,
                on_curve: true,
            },
            Point {
                x: 40,
                y: -7,
                on_curve: false,
            },
            Point {
                x: 5,
                y: 5,
                on_curve: true,
            },
        ];
        assert_eq!(compute_bbox(&pts), (-3, -7, 40, 10));
        // Values beyond i16 range are clamped.
        let huge = [Point {
            x: 100_000,
            y: -100_000,
            on_curve: true,
        }];
        assert_eq!(compute_bbox(&huge), (32767, -32768, 32767, -32768));
    }

    #[test]
    fn clamp_i16_saturates() {
        assert_eq!(clamp_i16(0), 0);
        assert_eq!(clamp_i16(40_000), i16::MAX);
        assert_eq!(clamp_i16(-40_000), i16::MIN);
        assert_eq!(clamp_i16(123), 123);
    }

    // -- bbox bitmap (MSB-first) --------------------------------------------

    #[test]
    fn bit_set_msb_first_and_bbox_bit_set() {
        // 0b1000_0001 → bits 0 and 7 set (MSB-first indexing).
        let bm = [0b1000_0001u8];
        assert!(bit_set_msb_first(&bm, 0));
        assert!(!bit_set_msb_first(&bm, 1));
        assert!(bit_set_msb_first(&bm, 7));
        // Second byte: 0b0100_0000 → index 9 set.
        let bm2 = [0x00, 0b0100_0000];
        assert!(bit_set_msb_first(&bm2, 9));
        assert!(!bit_set_msb_first(&bm2, 8));
        // Out-of-range index returns false rather than panicking.
        assert!(!bit_set_msb_first(&bm, 99));
        // bbox_bit_set delegates to the same logic.
        assert!(bbox_bit_set(&bm, 0));
        assert!(!bbox_bit_set(&bm, 99));
    }

    // -- build_loca ----------------------------------------------------------

    #[test]
    fn build_loca_short_and_long_formats() {
        let offsets = [0u32, 4, 12, 20];
        // Short format (index_format 0): each offset / 2 as u16.
        let short = build_loca(&offsets, 0);
        assert_eq!(short.len(), offsets.len() * 2);
        assert_eq!(u16::from_be_bytes([short[0], short[1]]), 0);
        assert_eq!(u16::from_be_bytes([short[2], short[3]]), 2); // 4/2
        assert_eq!(u16::from_be_bytes([short[4], short[5]]), 6); // 12/2
        assert_eq!(u16::from_be_bytes([short[6], short[7]]), 10); // 20/2
                                                                  // Long format (index_format 1): each offset as u32.
        let long = build_loca(&offsets, 1);
        assert_eq!(long.len(), offsets.len() * 4);
        assert_eq!(u32::from_be_bytes([long[4], long[5], long[6], long[7]]), 4);
    }

    // -- WOFF1 round-trip (stored + zlib) -----------------------------------

    /// Build a minimal but valid WOFF1 file from `(tag, data, compress)` table
    /// specs and return the bytes.
    fn make_woff1(flavor: u32, tables: &[(u32, Vec<u8>, bool)]) -> Vec<u8> {
        use crate::filters::deflate::deflate;
        let num = tables.len();
        let dir_start = 44usize;
        let mut body_off = dir_start + num * 20;
        // Build directory + body.
        let mut dir = Vec::new();
        let mut body = Vec::new();
        for (tag, data, compress) in tables {
            let orig_len = data.len();
            let stored: Vec<u8> = if *compress {
                // zlib wrapper: 2-byte header + raw DEFLATE + 4-byte adler
                // (the reader skips the 2-byte header and ignores the adler).
                let mut z = vec![0x78, 0x9c];
                z.extend_from_slice(&deflate(data));
                z.extend_from_slice(&[0, 0, 0, 0]);
                z
            } else {
                data.clone()
            };
            let comp_len = stored.len();
            dir.extend_from_slice(&tag.to_be_bytes());
            dir.extend_from_slice(&(body_off as u32).to_be_bytes());
            dir.extend_from_slice(&(comp_len as u32).to_be_bytes());
            dir.extend_from_slice(&(orig_len as u32).to_be_bytes());
            dir.extend_from_slice(&0u32.to_be_bytes()); // origChecksum (unused by reader)
            body.extend_from_slice(&stored);
            body_off += comp_len;
        }
        let mut out = vec![0u8; 44];
        out[0..4].copy_from_slice(&WOFF1_SIG.to_be_bytes());
        out[4..8].copy_from_slice(&flavor.to_be_bytes());
        out[12..14].copy_from_slice(&(num as u16).to_be_bytes());
        out.extend_from_slice(&dir);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn woff1_stored_table_passthrough() {
        // A single uncompressed 'maxp'-like table (comp_len == orig_len).
        let data = vec![0x00, 0x05, 0x00, 0x00, 0x00, 0x02]; // version + numGlyphs
        let woff = make_woff1(SFNT_TRUETYPE, &[(MAXP_TAG, data.clone(), false)]);
        let sfnt = sfnt_from_web_font(&woff).expect("woff1 stored reconstructs");
        assert_eq!(&sfnt[0..4], &SFNT_TRUETYPE.to_be_bytes());
        assert_eq!(u16::from_be_bytes([sfnt[4], sfnt[5]]), 1, "numTables");
        // The reconstructed table data matches the original.
        let off = u32::from_be_bytes([sfnt[20], sfnt[21], sfnt[22], sfnt[23]]) as usize;
        let len = u32::from_be_bytes([sfnt[24], sfnt[25], sfnt[26], sfnt[27]]) as usize;
        assert_eq!(&sfnt[off..off + len], &data[..]);
    }

    #[test]
    fn woff1_zlib_table_inflates() {
        // A compressible table (repeated bytes) stored zlib-compressed.
        let data = vec![0x41u8; 64];
        let woff = make_woff1(SFNT_OTTO, &[(MAXP_TAG, data.clone(), true)]);
        let sfnt = sfnt_from_web_font(&woff).expect("woff1 zlib reconstructs");
        assert_eq!(&sfnt[0..4], &SFNT_OTTO.to_be_bytes());
        let off = u32::from_be_bytes([sfnt[20], sfnt[21], sfnt[22], sfnt[23]]) as usize;
        let len = u32::from_be_bytes([sfnt[24], sfnt[25], sfnt[26], sfnt[27]]) as usize;
        assert_eq!(len, data.len());
        assert_eq!(&sfnt[off..off + len], &data[..]);
    }

    #[test]
    fn woff1_truncated_directory_fails() {
        // Claims one table but the directory entry runs past the buffer end.
        let mut woff = vec![0u8; 44];
        woff[0..4].copy_from_slice(&WOFF1_SIG.to_be_bytes());
        woff[4..8].copy_from_slice(&SFNT_TRUETYPE.to_be_bytes());
        woff[12..14].copy_from_slice(&1u16.to_be_bytes());
        // No 20-byte directory entry appended.
        assert_eq!(sfnt_from_web_font(&woff), None);
    }

    #[test]
    fn woff1_table_offset_out_of_bounds_fails() {
        // A directory entry whose data offset/length points past the buffer.
        let mut woff = vec![0u8; 44 + 20];
        woff[0..4].copy_from_slice(&WOFF1_SIG.to_be_bytes());
        woff[4..8].copy_from_slice(&SFNT_TRUETYPE.to_be_bytes());
        woff[12..14].copy_from_slice(&1u16.to_be_bytes());
        let de = 44;
        woff[de..de + 4].copy_from_slice(&MAXP_TAG.to_be_bytes());
        woff[de + 4..de + 8].copy_from_slice(&9999u32.to_be_bytes()); // bad offset
        woff[de + 8..de + 12].copy_from_slice(&4u32.to_be_bytes());
        woff[de + 12..de + 16].copy_from_slice(&4u32.to_be_bytes());
        assert_eq!(sfnt_from_web_font(&woff), None);
    }

    #[test]
    fn woff1_zlib_wrong_orig_len_fails() {
        use crate::filters::deflate::deflate;
        // Build a zlib table but lie about orig_len so the length check trips.
        let data = vec![0x42u8; 32];
        let mut z = vec![0x78, 0x9c];
        z.extend_from_slice(&deflate(&data));
        z.extend_from_slice(&[0, 0, 0, 0]);
        let mut woff = vec![0u8; 44];
        woff[0..4].copy_from_slice(&WOFF1_SIG.to_be_bytes());
        woff[4..8].copy_from_slice(&SFNT_TRUETYPE.to_be_bytes());
        woff[12..14].copy_from_slice(&1u16.to_be_bytes());
        let body_off = 44 + 20;
        woff.extend_from_slice(&MAXP_TAG.to_be_bytes());
        woff.extend_from_slice(&(body_off as u32).to_be_bytes());
        woff.extend_from_slice(&(z.len() as u32).to_be_bytes());
        woff.extend_from_slice(&999u32.to_be_bytes()); // wrong orig_len
        woff.extend_from_slice(&0u32.to_be_bytes()); // origChecksum
        woff.extend_from_slice(&z);
        assert_eq!(sfnt_from_web_font(&woff), None);
    }
}
