//! JPEG 2000 codestream marker parsing and tile decode orchestration
//! (ISO/IEC 15444-1 Annex A). Pure `std`.
//!
//! Parses the main header (`SIZ`/`COD`/`QCD`/…) and each tile-part header
//! (`SOT`/`COD`/`QCD`/`RGN`/`SOD`), gathers the packet bytes, and drives the
//! per-tile decode: tier-2 packet parsing ([`super::packet`]), tier-1 EBCOT
//! ([`super::t1`]), dequantisation + inverse DWT/MCT ([`super::dwt`]).

use super::dwt;
use super::packet::{self, TileComponent};
use super::{Image, Reader};
use crate::error::{EngineError, Result};

// Delimiting / fixed-information / header markers (ISO/IEC 15444-1 Table A.2).
const SOC: u16 = 0xFF4F;
const SOT: u16 = 0xFF90;
const SOD: u16 = 0xFF93;
const EOC: u16 = 0xFFD9;
const SIZ: u16 = 0xFF51;
const COD: u16 = 0xFF52;
const COC: u16 = 0xFF53;
const RGN: u16 = 0xFF5E;
const QCD: u16 = 0xFF5C;
const QCC: u16 = 0xFF5D;
const POC: u16 = 0xFF5F;
const TLM: u16 = 0xFF55;
const PLM: u16 = 0xFF57;
const PLT: u16 = 0xFF58;
const PPM: u16 = 0xFF60;
const PPT: u16 = 0xFF61;
const CRG: u16 = 0xFF63;
const COM: u16 = 0xFF64;

/// Image and tile geometry from the `SIZ` marker (Annex A.5.1).
#[derive(Debug, Clone)]
pub struct Siz {
    pub xsiz: u32,
    pub ysiz: u32,
    pub xosiz: u32,
    pub yosiz: u32,
    pub xtsiz: u32,
    pub ytsiz: u32,
    pub xtosiz: u32,
    pub ytosiz: u32,
    pub components: Vec<Component>,
}

/// Per-component parameters from `SIZ`: bit depth, signedness, sub-sampling.
#[derive(Debug, Clone)]
pub struct Component {
    pub bit_depth: u32,
    pub signed: bool,
    pub xr: u32,
    pub yr: u32,
}

impl Siz {
    /// Number of tiles across (`numXtiles`) and down (`numYtiles`).
    pub fn num_tiles(&self) -> (u32, u32) {
        let nx = (self.xsiz - self.xtosiz).div_ceil(self.xtsiz.max(1));
        let ny = (self.ysiz - self.ytosiz).div_ceil(self.ytsiz.max(1));
        (nx.max(1), ny.max(1))
    }
}

/// The wavelet transform kernel selected by `COD`/`COC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// 5/3 reversible (integer lifting) — lossless.
    Reversible,
    /// 9/7 irreversible (floating-point lifting) — lossy.
    Irreversible,
}

/// Progression order (Annex A.6.1, `SGcod`/`Ppoc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progression {
    Lrcp,
    Rlcp,
    Rpcl,
    Pcrl,
    Cprl,
}

/// Coding style for a component (`COD` default, overridden per-component by
/// `COC`) — Annex A.6.1.
#[derive(Debug, Clone)]
pub struct CodingStyle {
    pub progression: Progression,
    pub num_layers: u16,
    pub mct: bool,
    /// Number of decomposition (DWT) levels `NL`. Resolution levels = `NL + 1`.
    pub levels: u32,
    /// Code-block width/height as exponents (actual size = `1 << exp`).
    pub cb_width_exp: u32,
    pub cb_height_exp: u32,
    /// Code-block style bit-flags (`SPcod`/`SPcoc` cbstyle byte).
    pub cb_style: u8,
    pub transform: Transform,
    /// `true` when `SOP`/`EPH` packet markers are present (from `Scod`).
    pub use_sop: bool,
    pub use_eph: bool,
    /// Precinct width/height exponents per resolution level (low→high). Empty
    /// means the default maximum precinct (`15`,`15`) at every level.
    pub precincts: Vec<(u32, u32)>,
}

impl CodingStyle {
    /// Precinct width exponent at resolution level `r` (default 15 = no
    /// partition, i.e. one precinct covers the whole resolution).
    pub fn ppx(&self, r: usize) -> u32 {
        self.precincts.get(r).map(|p| p.0).unwrap_or(15)
    }

    pub fn ppy(&self, r: usize) -> u32 {
        self.precincts.get(r).map(|p| p.1).unwrap_or(15)
    }
}

/// Quantisation parameters for a component (`QCD` default, `QCC` override) —
/// Annex A.6.4.
#[derive(Debug, Clone)]
pub struct Quantization {
    /// 0 = none (reversible), 1 = scalar derived, 2 = scalar expounded.
    pub style: u8,
    pub guard_bits: u32,
    /// `(exponent, mantissa)` per subband, in subband order (LL, then per level
    /// HL, LH, HH from the lowest resolution up). For "derived", only the first
    /// entry is meaningful and the rest are computed.
    pub steps: Vec<(u32, u32)>,
}

/// A parsed codestream ready to decode.
#[derive(Debug)]
pub struct Codestream<'a> {
    data: &'a [u8],
    siz: Siz,
    /// Default coding style (from main-header `COD`).
    cod: CodingStyle,
    /// Per-component coding-style overrides (main-header `COC`).
    coc: Vec<Option<CodingStyle>>,
    /// Default quantisation (main-header `QCD`).
    qcd: Quantization,
    /// Per-component quantisation overrides (main-header `QCC`).
    qcc: Vec<Option<Quantization>>,
    /// ROI shift per component from `RGN` (0 = none).
    rgn: Vec<u8>,
    /// Concatenated `PPM` packed packet-header payloads from the main header
    /// (Annex A.7.4): the `Ippm` bytes only, in stream order. Empty when no `PPM`.
    /// Distributed to tile-parts (that lack their own `PPT`) by the `Nppm` lengths
    /// embedded in this stream.
    ppm: Vec<u8>,
    /// Byte offset of the first marker after the main header (`SOT`/`EOC`).
    body_start: usize,
}

impl<'a> Codestream<'a> {
    /// Parse the main header up to (but not consuming) the first tile-part.
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        if r.u16()? != SOC {
            return Err(EngineError::Filter("jpx: missing SOC marker".into()));
        }
        let mut siz: Option<Siz> = None;
        let mut cod: Option<CodingStyle> = None;
        let mut qcd: Option<Quantization> = None;
        let mut coc: Vec<Option<CodingStyle>> = Vec::new();
        let mut qcc: Vec<Option<Quantization>> = Vec::new();
        let mut rgn: Vec<u8> = Vec::new();
        let mut ppm: Vec<u8> = Vec::new();
        loop {
            let marker = r.u16()?;
            match marker {
                SOT | EOC => {
                    r.pos -= 2; // leave the marker for tile iteration.
                    break;
                }
                SIZ => {
                    let s = parse_siz(&mut r)?;
                    coc = vec![None; s.components.len()];
                    qcc = vec![None; s.components.len()];
                    rgn = vec![0; s.components.len()];
                    siz = Some(s);
                }
                COD => cod = Some(parse_cod(&mut r)?),
                COC => {
                    let s = siz
                        .as_ref()
                        .ok_or_else(|| EngineError::Filter("jpx: COC before SIZ".into()))?;
                    let base = cod.clone().unwrap_or_else(default_cod);
                    let (idx, style) = parse_coc(&mut r, s.components.len(), &base)?;
                    if let Some(slot) = coc.get_mut(idx) {
                        *slot = Some(style);
                    }
                }
                QCD => qcd = Some(parse_qcd(&mut r)?),
                QCC => {
                    let s = siz
                        .as_ref()
                        .ok_or_else(|| EngineError::Filter("jpx: QCC before SIZ".into()))?;
                    let (idx, q) = parse_qcc(&mut r, s.components.len())?;
                    if let Some(slot) = qcc.get_mut(idx) {
                        *slot = Some(q);
                    }
                }
                RGN => {
                    let s = siz
                        .as_ref()
                        .ok_or_else(|| EngineError::Filter("jpx: RGN before SIZ".into()))?;
                    let (idx, shift) = parse_rgn(&mut r, s.components.len())?;
                    if let Some(slot) = rgn.get_mut(idx) {
                        *slot = shift;
                    }
                }
                // PPM (Annex A.7.4): packed packet headers for the whole image.
                // Append the marker's payload (after the 1-byte `Zppm` index) to
                // the running PPM stream; it is split per tile-part later.
                PPM => parse_ppm(&mut r, &mut ppm)?,
                // Other informational / length markers in the main header: read
                // their length and skip the segment.
                POC | TLM | PLM | CRG | COM => skip_segment(&mut r)?,
                other if (0xFF30..=0xFF3F).contains(&other) => {
                    // Markers with no segment body (reserved/no-data).
                }
                other => {
                    return Err(EngineError::Filter(format!(
                        "jpx: unexpected main-header marker 0x{other:04X}"
                    )));
                }
            }
        }
        let siz = siz.ok_or_else(|| EngineError::Filter("jpx: no SIZ marker".into()))?;
        let cod = cod.ok_or_else(|| EngineError::Filter("jpx: no COD marker".into()))?;
        let qcd = qcd.ok_or_else(|| EngineError::Filter("jpx: no QCD marker".into()))?;
        let body_start = r.pos;
        Ok(Codestream {
            data,
            siz,
            cod,
            coc,
            qcd,
            qcc,
            rgn,
            ppm,
            body_start,
        })
    }

    /// Decode every tile and assemble the component planes.
    pub fn decode(&mut self) -> Result<Image> {
        let w = (self.siz.xsiz - self.siz.xosiz) as usize;
        let h = (self.siz.ysiz - self.siz.yosiz) as usize;
        if w == 0 || h == 0 || w > 1 << 20 || h > 1 << 20 {
            return Err(EngineError::Filter(
                "jpx: implausible image dimensions".into(),
            ));
        }
        let ncomp = self.siz.components.len();
        if ncomp == 0 {
            return Err(EngineError::Filter("jpx: zero components".into()));
        }
        let mut planes: Vec<Vec<i32>> = vec![vec![0i32; w * h]; ncomp];

        let tiles = self.collect_tiles()?;
        let (ntx, _nty) = self.siz.num_tiles();
        for tile in &tiles {
            self.decode_tile(tile, ntx, w, h, &mut planes)?;
        }

        let bit_depths = self
            .siz
            .components
            .iter()
            .map(|c| c.bit_depth)
            .collect::<Vec<_>>();
        Ok(Image {
            width: w,
            height: h,
            planes,
            bit_depths,
        })
    }

    /// Gather all tile-parts, concatenating multi-part tiles' packet bytes.
    fn collect_tiles(&self) -> Result<Vec<TilePart>> {
        let mut r = Reader::new(self.data);
        r.pos = self.body_start;
        let mut by_index: Vec<TilePart> = Vec::new();
        // Cursor into the main-header `PPM` stream: each tile-part lacking its own
        // `PPT` consumes the next `Nppm`-delimited chunk (Annex A.7.4).
        let mut ppm_cursor = 0usize;
        loop {
            if r.remaining() < 2 {
                break;
            }
            let marker = r.u16()?;
            if marker == EOC {
                break;
            }
            if marker != SOT {
                // Skip any stray informational markers between tile-parts.
                if marker & 0xFF00 == 0xFF00 && r.remaining() >= 2 {
                    let len = r.u16()? as usize;
                    if len >= 2 {
                        r.skip(len - 2)?;
                    }
                    continue;
                }
                break;
            }
            // SOT segment: Lsot(16)=10, Isot(16) tile index, Psot(32) tile-part
            // length (from SOT marker to end of tile-part data), TPsot(8),
            // TNsot(8).
            let _lsot = r.u16()?;
            let isot = r.u16()? as usize;
            let psot = r.u32()? as usize;
            let _tpsot = r.u8()?;
            let _tnsot = r.u8()?;
            // Read tile-part header markers until SOD, then the packet bytes.
            let mut tile_cod: Option<CodingStyle> = None;
            let mut tile_qcd: Option<Quantization> = None;
            let mut tile_coc: Vec<(usize, CodingStyle)> = Vec::new();
            let mut tile_qcc: Vec<(usize, Quantization)> = Vec::new();
            // Concatenated `PPT` `Ippt` bytes for this tile-part (Annex A.7.5).
            let mut tile_ppt: Vec<u8> = Vec::new();
            // The SOT marker began 12 bytes back: SOT(2) Lsot(2) Isot(2) Psot(4)
            // TPsot(1) TNsot(1). `Psot` is measured from that marker.
            let sot_pos = r.pos - 12;
            loop {
                let m = r.u16()?;
                match m {
                    SOD => break,
                    COD => tile_cod = Some(parse_cod(&mut r)?),
                    COC => {
                        let base = tile_cod.clone().unwrap_or_else(|| self.cod.clone());
                        let (idx, style) = parse_coc(&mut r, self.siz.components.len(), &base)?;
                        tile_coc.push((idx, style));
                    }
                    QCD => tile_qcd = Some(parse_qcd(&mut r)?),
                    QCC => {
                        let (idx, q) = parse_qcc(&mut r, self.siz.components.len())?;
                        tile_qcc.push((idx, q));
                    }
                    // PPT (Annex A.7.5): append this segment's `Ippt` payload (after
                    // the 1-byte `Zppt` index) to the tile-part's packed headers.
                    PPT => parse_ppt(&mut r, &mut tile_ppt)?,
                    RGN | POC | PLT | COM => skip_segment(&mut r)?,
                    other => {
                        return Err(EngineError::Filter(format!(
                            "jpx: unexpected tile-part marker 0x{other:04X}"
                        )));
                    }
                }
            }
            // Packet data runs from just after SOD to the end of this tile-part.
            // `Psot` counts from the SOT marker; 0 means "rest of codestream".
            let data_start = r.pos;
            let data_end = if psot == 0 {
                self.data.len()
            } else {
                (sot_pos + psot).min(self.data.len())
            };
            let body = self
                .data
                .get(data_start..data_end.max(data_start))
                .unwrap_or(&[])
                .to_vec();
            r.pos = data_end.max(data_start);

            // Packed packet headers for this tile-part: its own `PPT` bytes take
            // precedence; otherwise consume the next `PPM` chunk (if any).
            let part_headers = if !tile_ppt.is_empty() {
                tile_ppt
            } else {
                next_ppm_chunk(&self.ppm, &mut ppm_cursor)
            };

            // Merge into the per-tile aggregate (multi-part tiles append bytes).
            if let Some(tp) = by_index.iter_mut().find(|t| t.index == isot) {
                tp.packets.extend_from_slice(&body);
                tp.packed_headers.extend_from_slice(&part_headers);
            } else {
                by_index.push(TilePart {
                    index: isot,
                    cod: tile_cod,
                    qcd: tile_qcd,
                    coc: tile_coc,
                    qcc: tile_qcc,
                    packets: body,
                    packed_headers: part_headers,
                });
            }
        }
        if by_index.is_empty() {
            return Err(EngineError::Filter("jpx: no tile-parts".into()));
        }
        Ok(by_index)
    }

    /// Decode a single tile and blend its samples into the output planes.
    fn decode_tile(
        &self,
        tp: &TilePart,
        ntx: u32,
        img_w: usize,
        img_h: usize,
        planes: &mut [Vec<i32>],
    ) -> Result<()> {
        let ti = tp.index as u32;
        let (tx, ty) = (ti % ntx, ti / ntx);
        // Tile bounds on the reference grid (Annex B.3).
        let tx0 = (self.siz.xtosiz + tx * self.siz.xtsiz).max(self.siz.xosiz);
        let ty0 = (self.siz.ytosiz + ty * self.siz.ytsiz).max(self.siz.yosiz);
        let tx1 = (self.siz.xtosiz + (tx + 1) * self.siz.xtsiz).min(self.siz.xsiz);
        let ty1 = (self.siz.ytosiz + (ty + 1) * self.siz.ytsiz).min(self.siz.ysiz);

        let ncomp = self.siz.components.len();
        // Per-component coding/quant for this tile (tile overrides main header).
        let mut comps: Vec<TileComponent> = Vec::with_capacity(ncomp);
        for c in 0..ncomp {
            let cod = tile_style_for(c, &self.cod, tp, &self.coc);
            let quant = tile_quant_for(c, &self.qcd, tp, &self.qcc);
            let comp = &self.siz.components[c];
            // Component sample area: subsample the tile bounds by XRsiz/YRsiz.
            let cx0 = tx0.div_ceil(comp.xr);
            let cy0 = ty0.div_ceil(comp.yr);
            let cx1 = tx1.div_ceil(comp.xr);
            let cy1 = ty1.div_ceil(comp.yr);
            comps.push(TileComponent::new(
                cod,
                quant,
                self.rgn[c],
                comp.bit_depth,
                cx0,
                cy0,
                cx1,
                cy1,
            ));
        }

        // Tier-2: parse packets into code-block coded segments, then tier-1 +
        // dequant + inverse DWT to recover the spatial samples per component.
        // `packed_headers` (PPM/PPT), when present, supplies the packet headers.
        packet::decode_tile(&mut comps, &tp.packets, &tp.packed_headers)?;
        let mut spatial: Vec<Vec<i32>> = Vec::with_capacity(ncomp);
        for tc in &comps {
            spatial.push(dwt::reconstruct(tc)?);
        }

        // Inverse multi-component transform (RCT/ICT) when MCT is on and there
        // are ≥3 components. Operates on the first three components in place.
        let cs0 = tile_style_for(0, &self.cod, tp, &self.coc);
        if cs0.mct && ncomp >= 3 {
            dwt::inverse_mct(&mut spatial, cs0.transform);
        }

        // DC level-shift (unsigned components only) and clamp, then scatter into
        // the image planes honouring sub-sampling.
        for c in 0..ncomp {
            let comp = &self.siz.components[c];
            let shift = if comp.signed {
                0
            } else {
                1i32 << (comp.bit_depth - 1)
            };
            let max = (1i64 << comp.bit_depth) as i32 - 1;
            let cx0 = tx0.div_ceil(comp.xr) as usize;
            let cy0 = ty0.div_ceil(comp.yr) as usize;
            let cw = comps[c].width;
            let ch = comps[c].height;
            let plane = &mut planes[c];
            for sy in 0..ch {
                for sx in 0..cw {
                    let v = spatial[c][sy * cw + sx] + shift;
                    let v = v.clamp(0, max);
                    // Map component sample (cx0+sx, cy0+sy) up to image pixels by
                    // replicating across the sub-sampling factor.
                    let px0 = (cx0 + sx) * comp.xr as usize - self.siz.xosiz as usize;
                    let py0 = (cy0 + sy) * comp.yr as usize - self.siz.yosiz as usize;
                    for dy in 0..comp.yr as usize {
                        let py = py0 + dy;
                        if py >= img_h {
                            continue;
                        }
                        for dx in 0..comp.xr as usize {
                            let px = px0 + dx;
                            if px >= img_w {
                                continue;
                            }
                            plane[py * img_w + px] = v;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// One tile-part's aggregated header overrides and packet bytes.
#[derive(Debug)]
struct TilePart {
    index: usize,
    cod: Option<CodingStyle>,
    qcd: Option<Quantization>,
    coc: Vec<(usize, CodingStyle)>,
    qcc: Vec<(usize, Quantization)>,
    packets: Vec<u8>,
    /// Packed packet headers for this tile (concatenated `PPT` `Ippt` bytes and/or
    /// the `PPM` chunk(s) assigned to its tile-parts). Empty ⇒ inline headers.
    packed_headers: Vec<u8>,
}

/// Resolve component `c`'s coding style for a tile: tile `COC` > tile `COD` >
/// main `COC` > main `COD`.
fn tile_style_for(
    c: usize,
    main_cod: &CodingStyle,
    tp: &TilePart,
    main_coc: &[Option<CodingStyle>],
) -> CodingStyle {
    if let Some((_, s)) = tp.coc.iter().find(|(i, _)| *i == c) {
        return s.clone();
    }
    if let Some(s) = &tp.cod {
        return s.clone();
    }
    if let Some(Some(s)) = main_coc.get(c) {
        return s.clone();
    }
    main_cod.clone()
}

/// Resolve component `c`'s quantisation for a tile (same precedence as styles).
fn tile_quant_for(
    c: usize,
    main_qcd: &Quantization,
    tp: &TilePart,
    main_qcc: &[Option<Quantization>],
) -> Quantization {
    if let Some((_, q)) = tp.qcc.iter().find(|(i, _)| *i == c) {
        return q.clone();
    }
    if let Some(q) = &tp.qcd {
        return q.clone();
    }
    if let Some(Some(q)) = main_qcc.get(c) {
        return q.clone();
    }
    main_qcd.clone()
}

fn parse_siz(r: &mut Reader) -> Result<Siz> {
    let lsiz = r.u16()? as usize;
    let _rsiz = r.u16()?; // capabilities (profile) — informational here.
    let xsiz = r.u32()?;
    let ysiz = r.u32()?;
    let xosiz = r.u32()?;
    let yosiz = r.u32()?;
    let xtsiz = r.u32()?;
    let ytsiz = r.u32()?;
    let xtosiz = r.u32()?;
    let ytosiz = r.u32()?;
    let csiz = r.u16()? as usize;
    if csiz == 0 || csiz > 16384 {
        return Err(EngineError::Filter("jpx: bad component count".into()));
    }
    // Lsiz = 38 + 3*Csiz; sanity-bound the segment.
    if lsiz != 38 + 3 * csiz {
        return Err(EngineError::Filter("jpx: bad SIZ length".into()));
    }
    let mut components = Vec::with_capacity(csiz);
    for _ in 0..csiz {
        let ssiz = r.u8()?;
        let xr = r.u8()? as u32;
        let yr = r.u8()? as u32;
        components.push(Component {
            bit_depth: (ssiz & 0x7F) as u32 + 1,
            signed: ssiz & 0x80 != 0,
            xr: xr.max(1),
            yr: yr.max(1),
        });
    }
    if xsiz <= xosiz || ysiz <= yosiz || xtsiz == 0 || ytsiz == 0 {
        return Err(EngineError::Filter("jpx: degenerate SIZ geometry".into()));
    }
    Ok(Siz {
        xsiz,
        ysiz,
        xosiz,
        yosiz,
        xtsiz,
        ytsiz,
        xtosiz,
        ytosiz,
        components,
    })
}

fn default_cod() -> CodingStyle {
    CodingStyle {
        progression: Progression::Lrcp,
        num_layers: 1,
        mct: false,
        levels: 5,
        cb_width_exp: 6,
        cb_height_exp: 6,
        cb_style: 0,
        transform: Transform::Reversible,
        use_sop: false,
        use_eph: false,
        precincts: Vec::new(),
    }
}

/// Parse `COD` (Annex A.6.1): `Scod`, `SGcod` (progression, #layers, MCT) and
/// `SPcod` (levels, code-block size, style, transform, optional precincts).
fn parse_cod(r: &mut Reader) -> Result<CodingStyle> {
    let lcod = r.u16()? as usize;
    let scod = r.u8()?;
    let prog = progression(r.u8()?)?;
    let num_layers = r.u16()?;
    let mct = r.u8()? != 0;
    let levels = r.u8()? as u32;
    let cb_w = (r.u8()? as u32 & 0x0F) + 2;
    let cb_h = (r.u8()? as u32 & 0x0F) + 2;
    let cb_style = r.u8()?;
    let transform = match r.u8()? {
        0 => Transform::Irreversible,
        1 => Transform::Reversible,
        other => {
            return Err(EngineError::Filter(format!(
                "jpx: unknown transform {other}"
            )))
        }
    };
    let use_sop = scod & 0x02 != 0;
    let use_eph = scod & 0x04 != 0;
    let mut precincts = Vec::new();
    if scod & 0x01 != 0 {
        // SPcod has (levels+1) precinct-size bytes.
        for _ in 0..=levels {
            let b = r.u8()?;
            precincts.push(((b & 0x0F) as u32, ((b >> 4) & 0x0F) as u32));
        }
    } else {
        // No defined precincts: the segment length already consumed everything.
        let _ = lcod;
    }
    if levels > 32 {
        return Err(EngineError::Filter(
            "jpx: too many decomposition levels".into(),
        ));
    }
    Ok(CodingStyle {
        progression: prog,
        num_layers,
        mct,
        levels,
        cb_width_exp: cb_w,
        cb_height_exp: cb_h,
        cb_style,
        transform,
        use_sop,
        use_eph,
        precincts,
    })
}

/// Parse `COC` (Annex A.6.2): a per-component coding-style override. Inherits
/// progression/layers/MCT from `base` (those live only in `COD`).
fn parse_coc(r: &mut Reader, ncomp: usize, base: &CodingStyle) -> Result<(usize, CodingStyle)> {
    let _lcoc = r.u16()?;
    let idx = if ncomp <= 256 {
        r.u8()? as usize
    } else {
        r.u16()? as usize
    };
    let scoc = r.u8()?;
    let levels = r.u8()? as u32;
    let cb_w = (r.u8()? as u32 & 0x0F) + 2;
    let cb_h = (r.u8()? as u32 & 0x0F) + 2;
    let cb_style = r.u8()?;
    let transform = match r.u8()? {
        0 => Transform::Irreversible,
        1 => Transform::Reversible,
        other => {
            return Err(EngineError::Filter(format!(
                "jpx: unknown transform {other}"
            )))
        }
    };
    let mut precincts = Vec::new();
    if scoc & 0x01 != 0 {
        for _ in 0..=levels {
            let b = r.u8()?;
            precincts.push(((b & 0x0F) as u32, ((b >> 4) & 0x0F) as u32));
        }
    }
    if levels > 32 {
        return Err(EngineError::Filter(
            "jpx: too many decomposition levels".into(),
        ));
    }
    Ok((
        idx,
        CodingStyle {
            progression: base.progression,
            num_layers: base.num_layers,
            mct: base.mct,
            levels,
            cb_width_exp: cb_w,
            cb_height_exp: cb_h,
            cb_style,
            transform,
            use_sop: base.use_sop,
            use_eph: base.use_eph,
            precincts,
        },
    ))
}

/// Parse `QCD`/`QCC` quantisation steps from the `SQcd`/`SQcc` byte and the
/// per-subband `SPqcd` words.
fn parse_quant(r: &mut Reader, lqc: usize, header_bytes: usize) -> Result<Quantization> {
    let sq = r.u8()?;
    let style = sq & 0x1F;
    let guard_bits = (sq >> 5) as u32;
    let body_len = lqc.saturating_sub(header_bytes + 1);
    let mut steps = Vec::new();
    match style {
        0 => {
            // No quantisation: one 8-bit exponent per subband (Annex A.6.4).
            for _ in 0..body_len {
                let b = r.u8()?;
                steps.push(((b >> 3) as u32, 0));
            }
        }
        1 | 2 => {
            // Scalar derived (1) carries a single 16-bit word; scalar expounded
            // (2) carries one 16-bit word per subband.
            let count = body_len / 2;
            for _ in 0..count {
                let w = r.u16()?;
                steps.push(((w >> 11) as u32, (w & 0x07FF) as u32));
            }
        }
        other => {
            return Err(EngineError::Filter(format!(
                "jpx: unknown quantisation style {other}"
            )))
        }
    }
    Ok(Quantization {
        style,
        guard_bits,
        steps,
    })
}

fn parse_qcd(r: &mut Reader) -> Result<Quantization> {
    let lqcd = r.u16()? as usize;
    parse_quant(r, lqcd, 2)
}

fn parse_qcc(r: &mut Reader, ncomp: usize) -> Result<(usize, Quantization)> {
    let lqcc = r.u16()? as usize;
    let (idx, header) = if ncomp <= 256 {
        (r.u8()? as usize, 3) // Lqcc(2) + Cqcc(1)
    } else {
        (r.u16()? as usize, 4) // Lqcc(2) + Cqcc(2)
    };
    let q = parse_quant(r, lqcc, header)?;
    Ok((idx, q))
}

/// Parse `RGN` (Annex A.6.3): component index + ROI style + implicit shift.
fn parse_rgn(r: &mut Reader, ncomp: usize) -> Result<(usize, u8)> {
    let _lrgn = r.u16()?;
    let idx = if ncomp <= 256 {
        r.u8()? as usize
    } else {
        r.u16()? as usize
    };
    let _srgn = r.u8()?; // 0 = implicit (max-shift)
    let shift = r.u8()?;
    Ok((idx, shift))
}

/// Skip a marker segment whose length word (`Lxxx`) follows the marker.
fn skip_segment(r: &mut Reader) -> Result<()> {
    let len = r.u16()? as usize;
    if len < 2 {
        return Err(EngineError::Filter("jpx: bad marker-segment length".into()));
    }
    r.skip(len - 2)
}

/// Parse a `PPM` marker segment (Annex A.7.4): `Lppm`(2) `Zppm`(1) then the
/// `Nppm`/`Ippm` packed-header data. The data (everything after `Zppm`) is
/// appended verbatim to `ppm`; the `Nppm` lengths embedded in it split the stream
/// per tile-part when it is later consumed by [`next_ppm_chunk`].
fn parse_ppm(r: &mut Reader, ppm: &mut Vec<u8>) -> Result<()> {
    let len = r.u16()? as usize;
    if len < 3 {
        return Err(EngineError::Filter("jpx: bad PPM segment length".into()));
    }
    let _zppm = r.u8()?; // segment index (ordering only; segments are contiguous)
    let payload = r.bytes(len - 3)?;
    ppm.extend_from_slice(payload);
    Ok(())
}

/// Parse a `PPT` marker segment (Annex A.7.5): `Lppt`(2) `Zppt`(1) `Ippt[…]`. The
/// `Ippt` packet-header bytes (everything after `Zppt`) are appended to `out`.
fn parse_ppt(r: &mut Reader, out: &mut Vec<u8>) -> Result<()> {
    let len = r.u16()? as usize;
    if len < 3 {
        return Err(EngineError::Filter("jpx: bad PPT segment length".into()));
    }
    let _zppt = r.u8()?; // segment index within the tile-part
    let payload = r.bytes(len - 3)?;
    out.extend_from_slice(payload);
    Ok(())
}

/// Consume the next tile-part's packet-header chunk from a concatenated `PPM`
/// stream (Annex A.7.4): a 4-byte `Nppm` length followed by `Nppm` bytes of
/// `Ippm` packet headers. Advances `cursor`. Returns an empty buffer once the
/// stream is exhausted or malformed (so tile-parts beyond the `PPM` data fall
/// back to inline headers).
fn next_ppm_chunk(ppm: &[u8], cursor: &mut usize) -> Vec<u8> {
    let start = *cursor;
    if start + 4 > ppm.len() {
        *cursor = ppm.len();
        return Vec::new();
    }
    let nppm =
        u32::from_be_bytes([ppm[start], ppm[start + 1], ppm[start + 2], ppm[start + 3]]) as usize;
    let data_start = start + 4;
    let data_end = data_start.saturating_add(nppm).min(ppm.len());
    *cursor = data_end;
    ppm[data_start..data_end].to_vec()
}

fn progression(b: u8) -> Result<Progression> {
    Ok(match b {
        0 => Progression::Lrcp,
        1 => Progression::Rlcp,
        2 => Progression::Rpcl,
        3 => Progression::Pcrl,
        4 => Progression::Cprl,
        other => {
            return Err(EngineError::Filter(format!(
                "jpx: unknown progression order {other}"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ppt_appends_ippt_after_zppt() {
        // PPT segment: Lppt(2)=7, Zppt(1)=0, Ippt(4)=AA BB CC DD. The reader is
        // positioned just after the 0xFF61 marker (as in collect_tiles).
        let seg = [0x00, 0x07, 0x00, 0xAA, 0xBB, 0xCC, 0xDD];
        let mut r = Reader::new(&seg);
        let mut out = Vec::new();
        parse_ppt(&mut r, &mut out).unwrap();
        assert_eq!(out, vec![0xAA, 0xBB, 0xCC, 0xDD]);
        // A second PPT segment concatenates.
        let seg2 = [0x00, 0x05, 0x01, 0xEE, 0xFF];
        let mut r2 = Reader::new(&seg2);
        parse_ppt(&mut r2, &mut out).unwrap();
        assert_eq!(out, vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[test]
    fn parse_ppm_appends_payload_after_zppm() {
        // PPM segment: Lppm(2)=10, Zppm(1)=0, then Nppm(4)=3 + Ippm(3)=11 22 33.
        // parse_ppm keeps the raw Nppm/Ippm payload verbatim for later splitting.
        let seg = [0x00, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x03, 0x11, 0x22, 0x33];
        let mut r = Reader::new(&seg);
        let mut ppm = Vec::new();
        parse_ppm(&mut r, &mut ppm).unwrap();
        assert_eq!(ppm, vec![0x00, 0x00, 0x00, 0x03, 0x11, 0x22, 0x33]);
    }

    #[test]
    fn next_ppm_chunk_splits_by_nppm() {
        // Two tile-part chunks: Nppm=2 [A1 A2], then Nppm=3 [B1 B2 B3].
        let ppm = [
            0x00, 0x00, 0x00, 0x02, 0xA1, 0xA2, // chunk 0
            0x00, 0x00, 0x00, 0x03, 0xB1, 0xB2, 0xB3, // chunk 1
        ];
        let mut cursor = 0usize;
        assert_eq!(next_ppm_chunk(&ppm, &mut cursor), vec![0xA1, 0xA2]);
        assert_eq!(next_ppm_chunk(&ppm, &mut cursor), vec![0xB1, 0xB2, 0xB3]);
        // Past the end → empty (tile-parts fall back to inline headers).
        assert_eq!(next_ppm_chunk(&ppm, &mut cursor), Vec::<u8>::new());
    }

    #[test]
    fn next_ppm_chunk_clamps_truncated_nppm() {
        // Nppm claims 8 bytes but only 2 are present → clamp to the available data.
        let ppm = [0x00, 0x00, 0x00, 0x08, 0xC1, 0xC2];
        let mut cursor = 0usize;
        assert_eq!(next_ppm_chunk(&ppm, &mut cursor), vec![0xC1, 0xC2]);
        assert_eq!(cursor, ppm.len());
    }
}
