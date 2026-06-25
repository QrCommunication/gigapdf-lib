//! JPEG 2000 tier-2 packet decoding (ISO/IEC 15444-1 Annex B).
//!
//! Builds the per-tile-component subband / precinct / code-block geometry, then
//! walks the packets in the coding style's progression order, parsing each
//! packet header (tag-trees for inclusion and zero-bit-planes, new-pass counts,
//! code-block contribution lengths) and gathering the coded byte segments into
//! the code-blocks. Tier-1 ([`super::t1`]) and the inverse DWT ([`super::dwt`])
//! then consume the populated geometry.

use super::markers::{CodingStyle, Progression, Quantization};
use crate::error::Result;

/// One subband orientation. `Ll` exists only at resolution 0; `Hl`/`Lh`/`Hh` at
/// resolutions ≥ 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Ll,
    Hl,
    Lh,
    Hh,
}

impl Orientation {
    /// Context-formation orientation for tier-1 zero-coding (Table D.1
    /// grouping): LL/LH → 0, HL → 1, HH → 2.
    pub fn t1_kind(self) -> u8 {
        match self {
            Orientation::Ll | Orientation::Lh => 0,
            Orientation::Hl => 1,
            Orientation::Hh => 2,
        }
    }

    fn x_off(self) -> u32 {
        matches!(self, Orientation::Hl | Orientation::Hh) as u32
    }

    fn y_off(self) -> u32 {
        matches!(self, Orientation::Lh | Orientation::Hh) as u32
    }
}

/// A code-block within a subband: pixel bounds, evolving tier-2 state and the
/// gathered coded bytes for tier-1.
#[derive(Debug, Default)]
pub struct CodeBlock {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    /// Set once the block first appears in a packet.
    pub included: bool,
    /// Number of most-significant bit-planes that are entirely zero.
    pub zero_bit_planes: u32,
    /// Total coding passes accumulated across layers.
    pub num_passes: u32,
    /// `Lblock` length-signalling state.
    pub lblock: u32,
    /// Concatenated coded bytes (all layers' contributions).
    pub data: Vec<u8>,
}

impl CodeBlock {
    pub fn width(&self) -> usize {
        (self.x1 - self.x0) as usize
    }

    pub fn height(&self) -> usize {
        (self.y1 - self.y0) as usize
    }
}

/// A precinct: a grid of code-blocks for one subband, plus its two tag-trees.
#[derive(Debug)]
pub struct Precinct {
    pub cb_across: u32,
    pub blocks: Vec<CodeBlock>,
    inclusion: TagTree,
    zero_planes: TagTree,
}

/// One subband (LL/HL/LH/HH) at a resolution level.
#[derive(Debug)]
pub struct Subband {
    pub orientation: Orientation,
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    /// Quantisation `(exponent, mantissa)`.
    pub step: (u32, u32),
    /// Log2 gain of the band (LL=0, HL/LH=1, HH=2).
    pub gain: u32,
    pub precincts: Vec<Precinct>,
}

impl Subband {
    pub fn width(&self) -> usize {
        (self.x1 - self.x0) as usize
    }

    pub fn height(&self) -> usize {
        (self.y1 - self.y0) as usize
    }
}

/// One resolution level: its bounds, precinct grid and subbands.
#[derive(Debug)]
pub struct Resolution {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
    pub prec_across: u32,
    pub prec_down: u32,
    pub subbands: Vec<Subband>,
}

impl Resolution {
    fn num_precincts(&self) -> usize {
        (self.prec_across * self.prec_down) as usize
    }
}

/// A tile-component: the resolution pyramid plus coding/quant parameters.
#[derive(Debug)]
pub struct TileComponent {
    pub cod: CodingStyle,
    pub quant: Quantization,
    pub roi_shift: u8,
    /// Component bit depth (`Ssiz`), used for the irreversible dequant gain.
    pub bit_depth: u32,
    pub width: usize,
    pub height: usize,
    /// Resolution levels, low (0) → high (NL).
    pub resolutions: Vec<Resolution>,
}

impl TileComponent {
    /// Build the subband/precinct/code-block structure for a tile-component over
    /// the sample area `[x0,x1)×[y0,y1)` of the tile-component grid.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cod: CodingStyle,
        quant: Quantization,
        roi_shift: u8,
        bit_depth: u32,
        x0: u32,
        y0: u32,
        x1: u32,
        y1: u32,
    ) -> Self {
        let nl = cod.levels;
        let numres = nl + 1;
        let mut resolutions = Vec::with_capacity(numres as usize);
        for r in 0..numres {
            let nb = nl - r; // decomposition levels below this resolution.
            let rx0 = ceil_div(x0, 1 << nb);
            let ry0 = ceil_div(y0, 1 << nb);
            let rx1 = ceil_div(x1, 1 << nb);
            let ry1 = ceil_div(y1, 1 << nb);
            let ppx = cod.ppx(r as usize);
            let ppy = cod.ppy(r as usize);
            let (prec_across, prec_down) = precinct_grid(rx0, ry0, rx1, ry1, ppx, ppy);
            let mut subbands = Vec::new();
            if r == 0 {
                subbands.push(build_subband(
                    Orientation::Ll,
                    nl,
                    x0,
                    y0,
                    x1,
                    y1,
                    &cod,
                    &quant,
                    r,
                    rx0,
                    ry0,
                    ppx,
                    ppy,
                    prec_across,
                    prec_down,
                ));
            } else {
                let level = nl - r + 1;
                for orient in [Orientation::Hl, Orientation::Lh, Orientation::Hh] {
                    subbands.push(build_subband(
                        orient,
                        level,
                        x0,
                        y0,
                        x1,
                        y1,
                        &cod,
                        &quant,
                        r,
                        rx0,
                        ry0,
                        ppx,
                        ppy,
                        prec_across,
                        prec_down,
                    ));
                }
            }
            resolutions.push(Resolution {
                x0: rx0,
                y0: ry0,
                x1: rx1,
                y1: ry1,
                prec_across,
                prec_down,
                subbands,
            });
        }
        TileComponent {
            cod,
            quant,
            roi_shift,
            bit_depth,
            width: (x1 - x0) as usize,
            height: (y1 - y0) as usize,
            resolutions,
        }
    }
}

/// Build one subband: compute its bounds, partition it into code-blocks grouped
/// by the parent resolution's precincts, and allocate its tag-trees.
#[allow(clippy::too_many_arguments)]
fn build_subband(
    orient: Orientation,
    level: u32,
    tcx0: u32,
    tcy0: u32,
    tcx1: u32,
    tcy1: u32,
    cod: &CodingStyle,
    quant: &Quantization,
    res: u32,
    rx0: u32,
    ry0: u32,
    ppx: u32,
    ppy: u32,
    prec_across: u32,
    prec_down: u32,
) -> Subband {
    // Subband bounds (Annex B.5): shift tile-component bounds by the orientation
    // offset before dividing by 2^level. The LL band at resolution 0 (level 0)
    // has no orientation offset, so `half` is only used for detail bands.
    let div = 1u32 << level;
    let half = 1u32 << level.saturating_sub(1);
    let xob = orient.x_off() * half;
    let yob = orient.y_off() * half;
    let bx0 = ceil_div(tcx0.saturating_sub(xob), div);
    let by0 = ceil_div(tcy0.saturating_sub(yob), div);
    let bx1 = ceil_div(tcx1.saturating_sub(xob), div);
    let by1 = ceil_div(tcy1.saturating_sub(yob), div);

    // Precinct dimensions on the subband grid: at resolutions ≥ 1 the resolution
    // precinct is split by 2 between the resolution and its subbands.
    let pp_shift = if res == 0 { 0 } else { 1 };
    let pdx = ppx.saturating_sub(pp_shift);
    let pdy = ppy.saturating_sub(pp_shift);
    // Code-block size, capped by the precinct size on the subband grid.
    let xcb = cod.cb_width_exp.min(pdx);
    let ycb = cod.cb_height_exp.min(pdy);
    let cbw = 1u32 << xcb;
    let cbh = 1u32 << ycb;

    // Subband-grid precinct origin (resolution origin halved at r>0).
    let sb_px0 = rx0 >> pp_shift;
    let sb_py0 = ry0 >> pp_shift;
    let mut precincts = Vec::with_capacity((prec_across * prec_down) as usize);
    for py in 0..prec_down {
        for px in 0..prec_across {
            let prx0 = (align_down(sb_px0, pdx) + px * (1 << pdx))
                .max(bx0)
                .min(bx1);
            let pry0 = (align_down(sb_py0, pdy) + py * (1 << pdy))
                .max(by0)
                .min(by1);
            let prx1 = (align_down(sb_px0, pdx) + (px + 1) * (1 << pdx))
                .min(bx1)
                .max(prx0);
            let pry1 = (align_down(sb_py0, pdy) + (py + 1) * (1 << pdy))
                .min(by1)
                .max(pry0);
            precincts.push(build_precinct(prx0, pry0, prx1, pry1, cbw, cbh, bx0, by0));
        }
    }

    let step = subband_step(quant, orient, level, cod);
    let gain = match orient {
        Orientation::Ll => 0,
        Orientation::Hl | Orientation::Lh => 1,
        Orientation::Hh => 2,
    };
    Subband {
        orientation: orient,
        x0: bx0,
        y0: by0,
        x1: bx1,
        y1: by1,
        step,
        gain,
        precincts,
    }
}

/// Partition a precinct's area of a subband into code-blocks aligned to the
/// code-block grid (anchored at the subband origin), and size its tag-trees.
#[allow(clippy::too_many_arguments)]
fn build_precinct(
    px0: u32,
    py0: u32,
    px1: u32,
    py1: u32,
    cbw: u32,
    cbh: u32,
    sb_x0: u32,
    sb_y0: u32,
) -> Precinct {
    if px1 <= px0 || py1 <= py0 {
        return Precinct {
            cb_across: 0,
            blocks: Vec::new(),
            inclusion: TagTree::new(0, 0),
            zero_planes: TagTree::new(0, 0),
        };
    }
    // Code-block grid indices, anchored on the subband origin.
    let first_cx = px0.saturating_sub(sb_x0) / cbw;
    let first_cy = py0.saturating_sub(sb_y0) / cbh;
    let last_cx = (px1 - 1 - sb_x0) / cbw;
    let last_cy = (py1 - 1 - sb_y0) / cbh;
    let cb_across = last_cx - first_cx + 1;
    let cb_down = last_cy - first_cy + 1;
    let mut blocks = Vec::with_capacity((cb_across * cb_down) as usize);
    for cy in first_cy..=last_cy {
        for cx in first_cx..=last_cx {
            let bx0 = (sb_x0 + cx * cbw).max(px0);
            let by0 = (sb_y0 + cy * cbh).max(py0);
            let bx1 = (sb_x0 + (cx + 1) * cbw).min(px1);
            let by1 = (sb_y0 + (cy + 1) * cbh).min(py1);
            blocks.push(CodeBlock {
                x0: bx0,
                y0: by0,
                x1: bx1,
                y1: by1,
                lblock: 3,
                ..Default::default()
            });
        }
    }
    Precinct {
        cb_across,
        blocks,
        inclusion: TagTree::new(cb_across, cb_down),
        zero_planes: TagTree::new(cb_across, cb_down),
    }
}

/// The quantisation `(exponent, mantissa)` for a subband.
fn subband_step(
    quant: &Quantization,
    orient: Orientation,
    level: u32,
    cod: &CodingStyle,
) -> (u32, u32) {
    let nl = cod.levels;
    if quant.style == 1 {
        // Derived (Annex E.1.1): ε_b = ε_0 − NL + n_b where n_b is the subband's
        // decomposition level.
        let (e0, m) = quant.steps.first().copied().unwrap_or((0, 0));
        let e = e0 as i32 - nl as i32 + level as i32;
        (e.max(0) as u32, m)
    } else {
        let idx = subband_quant_index(orient, level, nl);
        quant.steps.get(idx).copied().unwrap_or((0, 0))
    }
}

/// Index into the QCD/QCC step list. LL=0; then for decomposition level from
/// `NL` (lowest resolution) down to 1, the triplet (HL, LH, HH).
fn subband_quant_index(orient: Orientation, level: u32, nl: u32) -> usize {
    if matches!(orient, Orientation::Ll) {
        return 0;
    }
    let band_group = nl - level; // 0 for the lowest-resolution detail bands.
    let within = match orient {
        Orientation::Hl => 0,
        Orientation::Lh => 1,
        Orientation::Hh => 2,
        Orientation::Ll => 0,
    };
    1 + band_group as usize * 3 + within
}

/// Decode a whole tile: walk packets in progression order, filling code-blocks
/// in `comps`. `packed_headers`, when non-empty, holds the tile's `PPM`/`PPT`
/// packed packet headers (Annex A.7.4/A.7.5): headers are read from it while
/// `body` supplies only the packet bodies.
pub fn decode_tile(comps: &mut [TileComponent], body: &[u8], packed_headers: &[u8]) -> Result<()> {
    if comps.is_empty() {
        return Ok(());
    }
    let progression = comps[0].cod.progression;
    let num_layers = comps[0].cod.num_layers as u32;
    let use_sop = comps[0].cod.use_sop;
    let use_eph = comps[0].cod.use_eph;
    let max_res = comps.iter().map(|c| c.resolutions.len()).max().unwrap_or(0) as u32;
    let max_prec = comps
        .iter()
        .flat_map(|c| c.resolutions.iter())
        .map(Resolution::num_precincts)
        .max()
        .unwrap_or(1)
        .max(1) as u32;

    let mut rd = if packed_headers.is_empty() {
        PacketReader::new(body)
    } else {
        PacketReader::with_packed_headers(body, packed_headers)
    };
    for step in ProgressionIter::new(
        progression,
        num_layers,
        max_res,
        comps.len() as u32,
        max_prec,
    ) {
        if step.comp >= comps.len() {
            continue;
        }
        // Validate against the actual component geometry.
        {
            let comp = &comps[step.comp];
            if step.res as usize >= comp.resolutions.len()
                || step.layer >= comp.cod.num_layers as u32
            {
                continue;
            }
            if step.precinct >= comp.resolutions[step.res as usize].num_precincts() {
                continue;
            }
        }
        if rd.exhausted() {
            break;
        }
        read_packet(
            &mut rd,
            &mut comps[step.comp],
            step.res as usize,
            step.precinct,
            step.layer,
            use_sop,
            use_eph,
        )?;
    }
    Ok(())
}

/// A single (layer, resolution, component, precinct) packet coordinate.
#[derive(Debug, Clone, Copy)]
struct PacketStep {
    layer: u32,
    res: u32,
    comp: usize,
    precinct: usize,
}

/// Enumerate packet coordinates in the coding style's progression order
/// (Annex B.12). Implemented as four nested counters whose role depends on the
/// order.
struct ProgressionIter {
    order: Progression,
    limits: [u32; 4],
    counter: [u32; 4],
    done: bool,
}

impl ProgressionIter {
    fn new(order: Progression, layers: u32, res: u32, comps: u32, prec: u32) -> Self {
        // limits[0] = outermost loop … limits[3] = innermost.
        let limits = match order {
            Progression::Lrcp => [layers, res, comps, prec],
            Progression::Rlcp => [res, layers, comps, prec],
            Progression::Rpcl => [res, prec, comps, layers],
            Progression::Pcrl => [prec, comps, res, layers],
            Progression::Cprl => [comps, prec, res, layers],
        };
        ProgressionIter {
            order,
            limits,
            counter: [0; 4],
            done: limits.contains(&0),
        }
    }

    fn current(&self) -> PacketStep {
        let [a, b, c, d] = self.counter;
        match self.order {
            Progression::Lrcp => PacketStep {
                layer: a,
                res: b,
                comp: c as usize,
                precinct: d as usize,
            },
            Progression::Rlcp => PacketStep {
                res: a,
                layer: b,
                comp: c as usize,
                precinct: d as usize,
            },
            Progression::Rpcl => PacketStep {
                res: a,
                precinct: b as usize,
                comp: c as usize,
                layer: d,
            },
            Progression::Pcrl => PacketStep {
                precinct: a as usize,
                comp: b as usize,
                res: c,
                layer: d,
            },
            Progression::Cprl => PacketStep {
                comp: a as usize,
                precinct: b as usize,
                res: c,
                layer: d,
            },
        }
    }
}

impl Iterator for ProgressionIter {
    type Item = PacketStep;

    fn next(&mut self) -> Option<PacketStep> {
        if self.done {
            return None;
        }
        let step = self.current();
        // Increment the odometer (innermost first).
        for i in (0..4).rev() {
            self.counter[i] += 1;
            if self.counter[i] < self.limits[i] {
                return Some(step);
            }
            self.counter[i] = 0;
        }
        self.done = true;
        Some(step)
    }
}

/// Read one packet (header + body) for `(component, resolution, precinct,
/// layer)` and fold its contributions into the code-blocks.
#[allow(clippy::too_many_arguments)]
fn read_packet(
    rd: &mut PacketReader,
    comp: &mut TileComponent,
    res: usize,
    precinct: usize,
    layer: u32,
    use_sop: bool,
    use_eph: bool,
) -> Result<()> {
    if use_sop {
        rd.consume_sop();
    }
    rd.start_header();
    let non_empty = rd.bit()? == 1;

    // (subband index, block index within precinct, new passes, byte length)
    let mut contributions: Vec<(usize, usize, usize)> = Vec::new();
    if non_empty {
        let resolution = &mut comp.resolutions[res];
        for (sbi, sb) in resolution.subbands.iter_mut().enumerate() {
            let Some(prec) = sb.precincts.get_mut(precinct) else {
                continue;
            };
            let cb_across = prec.cb_across.max(1);
            let nblocks = prec.blocks.len();
            for cbi in 0..nblocks {
                let cx = cbi as u32 % cb_across;
                let cy = cbi as u32 / cb_across;
                let included;
                if !prec.blocks[cbi].included {
                    let val = prec.inclusion.decode(rd, cx, cy, layer + 1)?;
                    included = val <= layer;
                    if included {
                        prec.blocks[cbi].included = true;
                        let z = prec.zero_planes.decode(rd, cx, cy, u32::MAX)?;
                        prec.blocks[cbi].zero_bit_planes = z;
                    }
                } else {
                    included = rd.bit()? == 1;
                }
                if !included {
                    continue;
                }
                let passes = rd.read_num_passes()?;
                let mut lblock = prec.blocks[cbi].lblock;
                while rd.bit()? == 1 {
                    lblock += 1;
                }
                prec.blocks[cbi].lblock = lblock;
                let len_bits = lblock + floor_log2(passes.max(1));
                let len = rd.read_bits(len_bits)? as usize;
                prec.blocks[cbi].num_passes += passes;
                contributions.push((sbi, cbi, len));
            }
        }
    }
    rd.finish_header(use_eph);

    // Packet body: read each contributing code-block's coded bytes in order.
    let resolution = &mut comp.resolutions[res];
    for (sbi, cbi, len) in contributions {
        let bytes = rd.read_body_bytes(len)?.to_vec();
        if let Some(sb) = resolution.subbands.get_mut(sbi) {
            if let Some(prec) = sb.precincts.get_mut(precinct) {
                if let Some(block) = prec.blocks.get_mut(cbi) {
                    block.data.extend_from_slice(&bytes);
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tag-tree (Annex B.10.2): a hierarchical "minimum value" coder used for both
// code-block inclusion and zero-bit-plane signalling.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct TagTree {
    /// Per-level node arrays (level 0 = leaves, last = the single root). Each
    /// level stores its width and the per-node `(value, known)` state.
    levels: Vec<TagLevel>,
}

#[derive(Debug, Default)]
struct TagLevel {
    w: u32,
    value: Vec<u32>,
    known: Vec<bool>,
}

impl TagTree {
    fn new(w: u32, h: u32) -> Self {
        if w == 0 || h == 0 {
            return TagTree { levels: Vec::new() };
        }
        let mut levels = Vec::new();
        let (mut lw, mut lh) = (w, h);
        loop {
            levels.push(TagLevel {
                w: lw,
                value: vec![0; (lw * lh) as usize],
                known: vec![false; (lw * lh) as usize],
            });
            if lw == 1 && lh == 1 {
                break;
            }
            lw = lw.div_ceil(2);
            lh = lh.div_ceil(2);
        }
        TagTree { levels }
    }

    /// Decode the value at leaf `(x, y)` against threshold `t` (Annex B.10.2):
    /// returns the node's value once it is ≤ `t`, or `t` if it is known to
    /// exceed it. Reads bits from `rd`; node state persists across calls.
    fn decode(&mut self, rd: &mut PacketReader, x: u32, y: u32, t: u32) -> Result<u32> {
        if self.levels.is_empty() {
            return Ok(0);
        }
        let depth = self.levels.len();
        // Node coordinate at each level for this leaf.
        let mut coords = [(0u32, 0u32); 32];
        let (mut cx, mut cy) = (x, y);
        for c in coords.iter_mut().take(depth) {
            *c = (cx, cy);
            cx /= 2;
            cy /= 2;
        }
        // Propagate the running minimum from the root down to the leaf, reading
        // bits to raise each node's value until it reaches `t` or is finalised.
        let mut min_val = 0u32;
        for lvl in (0..depth).rev() {
            let (nx, ny) = coords[lvl];
            let idx = (ny * self.levels[lvl].w + nx) as usize;
            if self.levels[lvl].value[idx] < min_val {
                self.levels[lvl].value[idx] = min_val;
            }
            while self.levels[lvl].value[idx] < t && !self.levels[lvl].known[idx] {
                if rd.bit()? == 1 {
                    self.levels[lvl].known[idx] = true;
                } else {
                    self.levels[lvl].value[idx] += 1;
                }
            }
            min_val = self.levels[lvl].value[idx];
        }
        Ok(min_val)
    }
}

// ---------------------------------------------------------------------------
// Bit-stuffed packet-header / packet-body reader (Annex B.10.1).
// ---------------------------------------------------------------------------

/// FF-stuffed bit cursor over one byte buffer (Annex B.10.1): after a `0xFF` byte
/// the next byte yields only 7 data bits (its top bit is a stuffed 0).
struct BitCursor<'a> {
    data: &'a [u8],
    pos: usize,
    cur: u8,
    bits_left: u8,
    last_ff: bool,
}

impl<'a> BitCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitCursor {
            data,
            pos: 0,
            cur: 0,
            bits_left: 0,
            last_ff: false,
        }
    }

    /// Reset the bit accumulator to a byte boundary (start of a packet header).
    fn align(&mut self) {
        self.cur = 0;
        self.bits_left = 0;
        self.last_ff = false;
    }

    fn bit(&mut self) -> u32 {
        if self.bits_left == 0 {
            let byte = self.data.get(self.pos).copied().unwrap_or(0xFF);
            self.pos += 1;
            if self.last_ff {
                self.cur = byte & 0x7F;
                self.bits_left = 7;
            } else {
                self.cur = byte;
                self.bits_left = 8;
            }
            self.last_ff = byte == 0xFF;
        }
        self.bits_left -= 1;
        ((self.cur >> self.bits_left) & 1) as u32
    }
}

/// Bit-stuffed packet reader. Packet headers and packet bodies normally share one
/// in-bitstream buffer, but when `PPM`/`PPT` packed packet headers are present
/// (Annex A.7.4/A.7.5) the headers live in a *separate* buffer while the bodies
/// stay in the tile bitstream. `header` selects the source of header bits; `body`
/// is always the tile bitstream and supplies code-block bytes + `SOP` markers.
struct PacketReader<'a> {
    /// Tile bitstream: packet bodies (and inline headers when not packed).
    body: BitCursor<'a>,
    /// Packed packet-header stream (`PPM`/`PPT`); `None` ⇒ headers are inline and
    /// read from `body`.
    packed: Option<BitCursor<'a>>,
}

impl<'a> PacketReader<'a> {
    /// Reader with inline packet headers (no `PPM`/`PPT`).
    fn new(data: &'a [u8]) -> Self {
        PacketReader {
            body: BitCursor::new(data),
            packed: None,
        }
    }

    /// Reader whose packet headers come from a separate packed-header buffer
    /// (`PPM`/`PPT`), bodies still from `body`.
    fn with_packed_headers(body: &'a [u8], headers: &'a [u8]) -> Self {
        PacketReader {
            body: BitCursor::new(body),
            packed: Some(BitCursor::new(headers)),
        }
    }

    /// Whether there is no more packet data to read (drives the packet loop's
    /// early-out). With packed headers, packets may have empty bodies, so the
    /// loop must keep going while header bits remain; we only stop once *both*
    /// the header source and the body bitstream are exhausted.
    fn exhausted(&self) -> bool {
        let body_done = self.body.pos >= self.body.data.len();
        match self.packed {
            Some(ref p) => body_done && p.pos >= p.data.len(),
            None => body_done,
        }
    }

    /// The cursor that header bits are read from.
    fn header_cursor(&mut self) -> &mut BitCursor<'a> {
        match self.packed {
            Some(ref mut p) => p,
            None => &mut self.body,
        }
    }

    /// Begin reading a packet header (aligns the header bit accumulator).
    fn start_header(&mut self) {
        self.header_cursor().align();
    }

    /// Read one header bit (Annex B.10.1 FF-stuffing) from the header source.
    fn bit(&mut self) -> Result<u32> {
        Ok(self.header_cursor().bit())
    }

    fn read_bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    /// Decode the number of newly included coding passes (Annex B.10.3 Table).
    fn read_num_passes(&mut self) -> Result<u32> {
        if self.bit()? == 0 {
            return Ok(1);
        }
        if self.bit()? == 0 {
            return Ok(2);
        }
        let two = self.read_bits(2)?;
        if two < 3 {
            return Ok(3 + two);
        }
        let five = self.read_bits(5)?;
        if five < 31 {
            return Ok(6 + five);
        }
        let seven = self.read_bits(7)?;
        Ok(37 + seven)
    }

    /// Finish a packet header: drop to the next byte boundary in the header
    /// source and consume an `EPH` marker (0xFF92) if present. With packed headers
    /// the `EPH`, if used, lives in the packed-header buffer (Annex A.7.4/A.7.5).
    fn finish_header(&mut self, use_eph: bool) {
        let c = self.header_cursor();
        c.bits_left = 0;
        c.last_ff = false;
        if use_eph && c.pos + 1 < c.data.len() && c.data[c.pos] == 0xFF && c.data[c.pos + 1] == 0x92
        {
            c.pos += 2;
        }
    }

    /// Read `len` raw body bytes (a code-block contribution) from the bitstream.
    fn read_body_bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = (self.body.pos + len).min(self.body.data.len());
        let s = &self.body.data[self.body.pos..end];
        self.body.pos = end;
        Ok(s)
    }

    /// Consume an `SOP` marker segment (0xFF91 + Lsop + Nsop) from the bitstream
    /// if present (`SOP` is never packed into `PPM`/`PPT`).
    fn consume_sop(&mut self) {
        let b = &mut self.body;
        if b.pos + 1 < b.data.len() && b.data[b.pos] == 0xFF && b.data[b.pos + 1] == 0x91 {
            b.pos = (b.pos + 6).min(b.data.len());
        }
    }
}

// ---------------------------------------------------------------------------
// Small integer helpers.
// ---------------------------------------------------------------------------

fn ceil_div(a: u32, b: u32) -> u32 {
    if b == 0 {
        0
    } else {
        a.div_ceil(b)
    }
}

fn align_down(v: u32, exp: u32) -> u32 {
    (v >> exp) << exp
}

/// Precinct grid of a resolution: the number of precincts across and down.
fn precinct_grid(rx0: u32, ry0: u32, rx1: u32, ry1: u32, ppx: u32, ppy: u32) -> (u32, u32) {
    if rx1 <= rx0 || ry1 <= ry0 {
        return (0, 0);
    }
    let across = (rx1.div_ceil(1 << ppx)).saturating_sub(rx0 >> ppx).max(1);
    let down = (ry1.div_ceil(1 << ppy)).saturating_sub(ry0 >> ppy).max(1);
    (across, down)
}

fn floor_log2(v: u32) -> u32 {
    if v == 0 {
        0
    } else {
        31 - v.leading_zeros()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagtree_single_node_threshold() {
        // A 1×1 tag-tree: the leaf value is decoded by reading "increment" 0-bits
        // until a 1-bit finalises it. Encode value 2 as bits 0,0,1.
        let bytes = [0b0010_0000u8]; // 0,0,1 then padding
        let mut rd = PacketReader::new(&bytes);
        rd.start_header();
        let mut tree = TagTree::new(1, 1);
        assert_eq!(tree.decode(&mut rd, 0, 0, u32::MAX).unwrap(), 2);
    }

    #[test]
    fn progression_iter_lrcp_counts() {
        // 2 layers × 2 res × 1 comp × 1 prec = 4 packets, layer outermost.
        let steps: Vec<_> = ProgressionIter::new(Progression::Lrcp, 2, 2, 1, 1).collect();
        assert_eq!(steps.len(), 4);
        assert_eq!((steps[0].layer, steps[0].res), (0, 0));
        assert_eq!((steps[1].layer, steps[1].res), (0, 1));
        assert_eq!((steps[2].layer, steps[2].res), (1, 0));
        assert_eq!((steps[3].layer, steps[3].res), (1, 1));
    }

    #[test]
    fn progression_iter_rlcp_counts() {
        // Resolution outermost.
        let steps: Vec<_> = ProgressionIter::new(Progression::Rlcp, 2, 2, 1, 1).collect();
        assert_eq!((steps[0].res, steps[0].layer), (0, 0));
        assert_eq!((steps[1].res, steps[1].layer), (0, 1));
        assert_eq!((steps[2].res, steps[2].layer), (1, 0));
    }

    #[test]
    fn packet_reader_ff_stuffing() {
        // After a 0xFF byte only 7 bits are read from the next byte.
        let bytes = [0xFF, 0xFF];
        let mut rd = PacketReader::new(&bytes);
        rd.start_header();
        // First byte 0xFF = 8 ones.
        for _ in 0..8 {
            assert_eq!(rd.bit().unwrap(), 1);
        }
        // Next byte is stuffed: top bit dropped, 7 ones follow.
        for _ in 0..7 {
            assert_eq!(rd.bit().unwrap(), 1);
        }
    }

    #[test]
    fn packet_reader_packed_headers_split_sources() {
        // With PPM/PPT packed headers, header bits come from the packed buffer and
        // body bytes from the bitstream. Header buffer = 0b1010_1100; body =
        // distinct marker bytes. Reading 4 header bits then body bytes must draw
        // from the two buffers independently.
        let header = [0b1010_1100u8];
        let body = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut rd = PacketReader::with_packed_headers(&body, &header);
        rd.start_header();
        assert_eq!(rd.read_bits(4).unwrap(), 0b1010);
        // Body bytes are untouched by header reads.
        rd.finish_header(false);
        assert_eq!(rd.read_body_bytes(2).unwrap(), &[0xDE, 0xAD]);
        assert_eq!(rd.read_body_bytes(2).unwrap(), &[0xBE, 0xEF]);
    }

    #[test]
    fn packet_reader_packed_headers_eph_in_header_stream() {
        // `finish_header` must consume an EPH (0xFF92) from the *packed* header
        // stream (Annex A.7.4/A.7.5), not from the bitstream. Header = 1 data byte
        // + EPH; the next header read after finish_header starts past the EPH.
        let header = [0b1000_0000u8, 0xFF, 0x92, 0b0100_0000];
        let body = [0x01, 0x02];
        let mut rd = PacketReader::with_packed_headers(&body, &header);
        rd.start_header();
        // First bit of byte 0.
        assert_eq!(rd.bit().unwrap(), 1);
        // finish_header aligns and consumes the EPH that follows in the packed
        // stream; the next header read then comes from byte 3 (0b0100_0000).
        rd.finish_header(true);
        rd.start_header();
        assert_eq!(rd.bit().unwrap(), 0);
        assert_eq!(rd.bit().unwrap(), 1);
        // Body remains fully available.
        assert_eq!(rd.read_body_bytes(2).unwrap(), &[0x01, 0x02]);
    }

    #[test]
    fn packet_reader_exhausted_considers_both_sources() {
        // With packed headers, the reader is exhausted only when both the body and
        // the header buffer are consumed (empty bodies are valid).
        let header = [0xAAu8];
        let body: [u8; 0] = [];
        let mut rd = PacketReader::with_packed_headers(&body, &header);
        // Body empty but header has bits to read → not exhausted yet.
        assert!(!rd.exhausted());
        rd.start_header();
        let _ = rd.read_bits(8).unwrap();
        assert!(rd.exhausted());
    }
}
