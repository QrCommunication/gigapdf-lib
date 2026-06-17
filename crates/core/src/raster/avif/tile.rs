//! AV1 tile decode — superblock partition recursion (geometry layer).
//!
//! This module owns the *geometry* of the block partition tree (AV1 spec
//! §5.11.4 `decode_partition`): how a superblock recursively splits into coding
//! blocks, which sub-block lands where, and how frame edges force splits. The
//! block-size and partition tables are transcribed from dav1d `src/tables.c`
//! (`dav1d_block_dimensions`, `dav1d_block_sizes`, `dav1d_partition_type_count`;
//! BSD-2-Clause). The actual partition-symbol *reading* (CDF context + the
//! gather-probability edge decisions, driven by `Msac`) is layered on top via
//! the `read` callback so the geometry can be validated independently.

#![allow(dead_code)]

/// Block-level indices (largest → smallest square superblock subdivision).
pub(crate) const BL_128X128: u8 = 0;
pub(crate) const BL_64X64: u8 = 1;
pub(crate) const BL_32X32: u8 = 2;
pub(crate) const BL_16X16: u8 = 3;
pub(crate) const BL_8X8: u8 = 4;

/// Partition types (AV1 spec order, matches dav1d `BlockPartition`).
pub(crate) mod part {
    pub const NONE: u8 = 0;
    pub const H: u8 = 1; // horizontal split (top / bottom)
    pub const V: u8 = 2; // vertical split (left / right)
    pub const SPLIT: u8 = 3;
    pub const T_TOP: u8 = 4; // HORZ_A
    pub const T_BOTTOM: u8 = 5; // HORZ_B
    pub const T_LEFT: u8 = 6; // VERT_A
    pub const T_RIGHT: u8 = 7; // VERT_B
    pub const H4: u8 = 8;
    pub const V4: u8 = 9;
}

/// Block-size indices (dav1d `BlockSize` enum order).
pub(crate) mod bs {
    pub const BS_128X128: u8 = 0;
    pub const BS_128X64: u8 = 1;
    pub const BS_64X128: u8 = 2;
    pub const BS_64X64: u8 = 3;
    pub const BS_64X32: u8 = 4;
    pub const BS_64X16: u8 = 5;
    pub const BS_32X64: u8 = 6;
    pub const BS_32X32: u8 = 7;
    pub const BS_32X16: u8 = 8;
    pub const BS_32X8: u8 = 9;
    pub const BS_16X64: u8 = 10;
    pub const BS_16X32: u8 = 11;
    pub const BS_16X16: u8 = 12;
    pub const BS_16X8: u8 = 13;
    pub const BS_16X4: u8 = 14;
    pub const BS_8X32: u8 = 15;
    pub const BS_8X16: u8 = 16;
    pub const BS_8X8: u8 = 17;
    pub const BS_8X4: u8 = 18;
    pub const BS_4X16: u8 = 19;
    pub const BS_4X8: u8 = 20;
    pub const BS_4X4: u8 = 21;
}

/// `dav1d_block_dimensions[bs] = { w4, h4, w_log2, h_log2 }` (size in 4×4 units).
pub(crate) static BLOCK_DIMENSIONS: [[u8; 4]; 22] = [
    [32, 32, 5, 5], // 128x128
    [32, 16, 5, 4], // 128x64
    [16, 32, 4, 5], // 64x128
    [16, 16, 4, 4], // 64x64
    [16, 8, 4, 3],  // 64x32
    [16, 4, 4, 2],  // 64x16
    [8, 16, 3, 4],  // 32x64
    [8, 8, 3, 3],   // 32x32
    [8, 4, 3, 2],   // 32x16
    [8, 2, 3, 1],   // 32x8
    [4, 16, 2, 4],  // 16x64
    [4, 8, 2, 3],   // 16x32
    [4, 4, 2, 2],   // 16x16
    [4, 2, 2, 1],   // 16x8
    [4, 1, 2, 0],   // 16x4
    [2, 8, 1, 3],   // 8x32
    [2, 4, 1, 2],   // 8x16
    [2, 2, 1, 1],   // 8x8
    [2, 1, 1, 0],   // 8x4
    [1, 4, 0, 2],   // 4x16
    [1, 2, 0, 1],   // 4x8
    [1, 1, 0, 0],   // 4x4
];

use bs::*;

/// `dav1d_block_sizes[bl][partition] = { sub0, sub1 }` — the coding-block size(s)
/// produced by each partition at each block level. Entries for partitions a
/// level can't emit (SPLIT above 8×8 recurses instead; H4/V4 above 64×64) are
/// `BS_128X128` placeholders and never read.
pub(crate) static BLOCK_SIZES: [[[u8; 2]; 10]; 5] = [
    // BL_128X128
    [
        [BS_128X128, 0],
        [BS_128X64, 0],
        [BS_64X128, 0],
        [BS_128X128, 0], // SPLIT (recurses)
        [BS_64X64, BS_128X64],
        [BS_128X64, BS_64X64],
        [BS_64X64, BS_64X128],
        [BS_64X128, BS_64X64],
        [BS_128X128, 0], // H4 (n/a)
        [BS_128X128, 0], // V4 (n/a)
    ],
    // BL_64X64
    [
        [BS_64X64, 0],
        [BS_64X32, 0],
        [BS_32X64, 0],
        [BS_128X128, 0], // SPLIT
        [BS_32X32, BS_64X32],
        [BS_64X32, BS_32X32],
        [BS_32X32, BS_32X64],
        [BS_32X64, BS_32X32],
        [BS_64X16, 0],
        [BS_16X64, 0],
    ],
    // BL_32X32
    [
        [BS_32X32, 0],
        [BS_32X16, 0],
        [BS_16X32, 0],
        [BS_128X128, 0], // SPLIT
        [BS_16X16, BS_32X16],
        [BS_32X16, BS_16X16],
        [BS_16X16, BS_16X32],
        [BS_16X32, BS_16X16],
        [BS_32X8, 0],
        [BS_8X32, 0],
    ],
    // BL_16X16
    [
        [BS_16X16, 0],
        [BS_16X8, 0],
        [BS_8X16, 0],
        [BS_128X128, 0], // SPLIT
        [BS_8X8, BS_16X8],
        [BS_16X8, BS_8X8],
        [BS_8X8, BS_8X16],
        [BS_8X16, BS_8X8],
        [BS_16X4, 0],
        [BS_4X16, 0],
    ],
    // BL_8X8 — only NONE/H/V/SPLIT; SPLIT → four 4×4 leaves.
    [
        [BS_8X8, 0],
        [BS_8X4, 0],
        [BS_4X8, 0],
        [BS_4X4, 0],
        [BS_128X128, 0],
        [BS_128X128, 0],
        [BS_128X128, 0],
        [BS_128X128, 0],
        [BS_128X128, 0],
        [BS_128X128, 0],
    ],
];

/// `dav1d_partition_type_count[bl]` = the `n_symbols` arg for the partition CDF
/// (alphabet = count + 1). 128×128 omits H4/V4; 8×8 only NONE/H/V/SPLIT.
pub(crate) static PARTITION_TYPE_COUNT: [usize; 5] = [7, 9, 9, 9, 3];

/// Frame geometry in 4×4 (mi) units.
pub(crate) struct TileGeom {
    pub mi_cols: u32,
    pub mi_rows: u32,
}

/// Which sub-blocks of a partition fall inside the frame. At a frame edge AV1
/// restricts the partition to `{SPLIT, H}` (cols only) or `{SPLIT, V}` (rows
/// only), or forces `SPLIT` (neither) — the `read` callback gets the case so it
/// can pick the right syntax (full symbol vs. the gather-probability binary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PartCase {
    Both,
    ColsOnly,
    RowsOnly,
    Neither,
}

/// Recurse the partition tree from `(mi_row, mi_col)` at block level `bl`,
/// emitting each coding block through `leaf(mi_row, mi_col, block_size)`. The
/// `read` callback supplies the partition type for a node (full symbol when
/// `Both`, the SPLIT-or-H/V binary at edges); `Neither` is forced to SPLIT
/// without a read. Faithful to dav1d `decode_sb` geometry (4:2:0 / 4:4:4 / 4:0:0;
/// the 4:2:2 V-partition restriction is not modelled — AVIF stills are not 422).
pub(crate) fn decode_partition(
    geom: &TileGeom,
    mi_row: u32,
    mi_col: u32,
    bl: u8,
    read: &mut dyn FnMut(u8, u32, u32, PartCase) -> u8,
    leaf: &mut dyn FnMut(u32, u32, u8),
) {
    if mi_row >= geom.mi_rows || mi_col >= geom.mi_cols {
        return;
    }
    let hbs = 16u32 >> bl; // half block, in 4×4 units
    let has_rows = mi_row + hbs < geom.mi_rows;
    let has_cols = mi_col + hbs < geom.mi_cols;
    let case = match (has_rows, has_cols) {
        (true, true) => PartCase::Both,
        (true, false) => PartCase::RowsOnly,
        (false, true) => PartCase::ColsOnly,
        (false, false) => PartCase::Neither,
    };
    let partition = match case {
        PartCase::Neither => part::SPLIT,
        _ => read(bl, mi_row, mi_col, case),
    };
    let sub = &BLOCK_SIZES[bl as usize][partition as usize];
    let (r, c) = (mi_row, mi_col);
    match partition {
        part::NONE => leaf(r, c, sub[0]),
        part::H => {
            leaf(r, c, sub[0]);
            if has_rows {
                leaf(r + hbs, c, sub[0]);
            }
        }
        part::V => {
            leaf(r, c, sub[0]);
            if has_cols {
                leaf(r, c + hbs, sub[0]);
            }
        }
        part::SPLIT => {
            if bl == BL_8X8 {
                leaf(r, c, BS_4X4);
                if c + 1 < geom.mi_cols {
                    leaf(r, c + 1, BS_4X4);
                }
                if r + 1 < geom.mi_rows {
                    leaf(r + 1, c, BS_4X4);
                }
                if r + 1 < geom.mi_rows && c + 1 < geom.mi_cols {
                    leaf(r + 1, c + 1, BS_4X4);
                }
            } else {
                decode_partition(geom, r, c, bl + 1, read, leaf);
                decode_partition(geom, r, c + hbs, bl + 1, read, leaf);
                decode_partition(geom, r + hbs, c, bl + 1, read, leaf);
                decode_partition(geom, r + hbs, c + hbs, bl + 1, read, leaf);
            }
        }
        part::T_TOP => {
            leaf(r, c, sub[0]);
            leaf(r, c + hbs, sub[0]);
            leaf(r + hbs, c, sub[1]);
        }
        part::T_BOTTOM => {
            leaf(r, c, sub[0]);
            leaf(r + hbs, c, sub[1]);
            leaf(r + hbs, c + hbs, sub[1]);
        }
        part::T_LEFT => {
            leaf(r, c, sub[0]);
            leaf(r + hbs, c, sub[0]);
            leaf(r, c + hbs, sub[1]);
        }
        part::T_RIGHT => {
            leaf(r, c, sub[0]);
            leaf(r, c + hbs, sub[1]);
            leaf(r + hbs, c + hbs, sub[1]);
        }
        part::H4 => {
            let q = hbs >> 1;
            leaf(r, c, sub[0]);
            leaf(r + q, c, sub[0]);
            leaf(r + 2 * q, c, sub[0]);
            if r + 3 * q < geom.mi_rows {
                leaf(r + 3 * q, c, sub[0]);
            }
        }
        part::V4 => {
            let q = hbs >> 1;
            leaf(r, c, sub[0]);
            leaf(r, c + q, sub[0]);
            leaf(r, c + 2 * q, sub[0]);
            if c + 3 * q < geom.mi_cols {
                leaf(r, c + 3 * q, sub[0]);
            }
        }
        _ => {}
    }
}

// ── Partition CDF context + edge gather probabilities (dav1d src/env.h) ───────

/// `dav1d_al_part_ctx[above/left][bl][partition]` — the value written into the
/// above/left partition-context arrays after a (non-pure-split) node, encoding
/// which block levels split at that position. `0xFF` = unused (`-1` in dav1d).
pub(crate) static AL_PART_CTX: [[[u8; 10]; 5]; 2] = [
    [
        // above
        [0x00, 0x00, 0x10, 0xFF, 0x00, 0x10, 0x10, 0x10, 0xFF, 0xFF],
        [0x10, 0x10, 0x18, 0xFF, 0x10, 0x18, 0x18, 0x18, 0x10, 0x1c],
        [0x18, 0x18, 0x1c, 0xFF, 0x18, 0x1c, 0x1c, 0x1c, 0x18, 0x1e],
        [0x1c, 0x1c, 0x1e, 0xFF, 0x1c, 0x1e, 0x1e, 0x1e, 0x1c, 0x1f],
        [0x1e, 0x1e, 0x1f, 0x1f, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    ],
    [
        // left
        [0x00, 0x10, 0x00, 0xFF, 0x10, 0x10, 0x00, 0x10, 0xFF, 0xFF],
        [0x10, 0x18, 0x10, 0xFF, 0x18, 0x18, 0x10, 0x18, 0x1c, 0x10],
        [0x18, 0x1c, 0x18, 0xFF, 0x1c, 0x1c, 0x18, 0x1c, 0x1e, 0x18],
        [0x1c, 0x1e, 0x1c, 0xFF, 0x1e, 0x1e, 0x1c, 0x1e, 0x1f, 0x1c],
        [0x1e, 0x1f, 0x1e, 0x1f, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    ],
];

/// CDF context for the partition symbol (AV1: above split-bit + left split-bit
/// at this level). `above` is frame-wide (8×8 columns), `left` is the current
/// superblock row's 128-px window (8×8 rows). dav1d `get_partition_ctx`.
fn get_partition_ctx(above: &[u8], left: &[u8; 16], bl: u8, mi_row: u32, mi_col: u32) -> usize {
    let xb8 = (mi_col >> 1) as usize;
    let yb8 = ((mi_row & 31) >> 1) as usize;
    let a = (above[xb8] >> (4 - bl)) & 1;
    let l = (left[yb8] >> (4 - bl)) & 1;
    (a + (l << 1)) as usize
}

/// Edge binary "split-or-horz" probability when only columns are in-frame.
/// dav1d `gather_top_partition_prob` (sums adjacent inverse-CDF entries).
fn gather_top_partition_prob(c: &[u16], bl: u8) -> u32 {
    // PARTITION indices: V-1=1, T_TOP=4, T_LEFT-1=5, V4-1=8, T_RIGHT=7.
    let mut out = c[1] as i32 - c[4] as i32;
    out += c[5] as i32;
    if bl != BL_128X128 {
        out += c[8] as i32 - c[7] as i32;
    }
    out as u32
}

/// Edge binary "split-or-vert" probability when only rows are in-frame.
/// dav1d `gather_left_partition_prob`.
fn gather_left_partition_prob(c: &[u16], bl: u8) -> u32 {
    // PARTITION indices: H-1=0, H=1, SPLIT-1=2, T_LEFT=6, H4-1=7, H4=8.
    let mut out = c[0] as i32 - c[1] as i32;
    out += c[2] as i32 - c[6] as i32;
    if bl != BL_128X128 {
        out += c[7] as i32 - c[8] as i32;
    }
    out as u32
}

// ── Tile decoder: the partition tree driven by the entropy decoder ────────────

use super::cdf;
use super::itx;
use super::msac::Msac;
use super::predict;
use super::scan::SCANS;

// ── Intra mode decode (decode_b block-info: dav1d src/decode.c) ───────────────

/// Intra prediction modes (AV1 `IntraPredMode`). Directional modes span
/// `VERT_PRED..=VERT_LEFT_PRED` (carry an angle delta). `CFL_PRED`/`FILTER_PRED`
/// share the value `N_INTRA_PRED_MODES`.
pub(crate) mod mode {
    pub const DC_PRED: u8 = 0;
    pub const VERT_PRED: u8 = 1;
    pub const HOR_PRED: u8 = 2;
    pub const VERT_LEFT_PRED: u8 = 8;
    pub const SMOOTH_PRED: u8 = 9;
    pub const SMOOTH_V_PRED: u8 = 10;
    pub const SMOOTH_H_PRED: u8 = 11;
    pub const PAETH_PRED: u8 = 12;
    pub const N_INTRA_PRED_MODES: u8 = 13;
    pub const CFL_PRED: u8 = 13;
    pub const N_UV_INTRA_PRED_MODES: u8 = 14;
    pub const FILTER_PRED: u8 = 13;
}

/// `dav1d_av1_mode_to_angle_map` — nominal angle (degrees) for the directional
/// modes `VERT_PRED..=VERT_LEFT_PRED`, before adding `3 * angle_delta`.
pub(crate) static AV1_MODE_TO_ANGLE_MAP: [i32; 8] = [90, 180, 45, 135, 113, 157, 203, 67];

/// `dav1d_intra_mode_context[mode]` — maps a neighbour's intra mode to one of 5
/// contexts for the keyframe Y-mode CDF (`cdf::KF_Y_MODE[above_ctx][left_ctx]`).
pub(crate) static INTRA_MODE_CONTEXT: [u8; 13] =
    [0, 1, 2, 3, 4, 4, 4, 4, 3, 0, 1, 2, 0];

/// `dav1d_txfm_dimensions[tx] = [lw, lh, max, sub, min, ctx]` (19 square+rect
/// sizes; `lw`/`lh` = log2 width/height in 4×4, `max`/`min` = larger/smaller log2
/// dim, `sub` = next-smaller tx for the size-split descent, `ctx` = coef-skip CDF
/// index). `TX_4X4`=0..`TX_64X64`=4, then rectangular `RTX_*` from 5.
pub(crate) static TXFM_DIMENSIONS: [[u8; 6]; 19] = [
    [0, 0, 0, 0, 0, 0],   // TX_4X4
    [1, 1, 1, 0, 1, 1],   // TX_8X8
    [2, 2, 2, 1, 2, 2],   // TX_16X16
    [3, 3, 3, 2, 3, 3],   // TX_32X32
    [4, 4, 4, 3, 4, 4],   // TX_64X64
    [0, 1, 1, 0, 0, 1],   // RTX_4X8
    [1, 0, 1, 0, 0, 1],   // RTX_8X4
    [1, 2, 2, 1, 1, 2],   // RTX_8X16
    [2, 1, 2, 1, 1, 2],   // RTX_16X8
    [2, 3, 3, 2, 2, 3],   // RTX_16X32
    [3, 2, 3, 2, 2, 3],   // RTX_32X16
    [3, 4, 4, 3, 3, 4],   // RTX_32X64
    [4, 3, 4, 3, 3, 4],   // RTX_64X32
    [0, 2, 2, 5, 0, 1],   // RTX_4X16
    [2, 0, 2, 6, 0, 1],   // RTX_16X4
    [1, 3, 3, 7, 1, 2],   // RTX_8X32
    [3, 1, 3, 8, 1, 2],   // RTX_32X8
    [2, 4, 4, 9, 2, 3],   // RTX_16X64
    [4, 2, 4, 10, 2, 3],  // RTX_64X16
];

/// `dav1d_skip_ctx[la][ll]` — coefficient all-zero (txb_skip) context for luma.
pub(crate) static SKIP_CTX: [[u8; 5]; 5] = [
    [1, 2, 2, 2, 3],
    [2, 4, 4, 4, 5],
    [2, 4, 4, 4, 5],
    [2, 4, 4, 4, 5],
    [3, 5, 5, 5, 6],
];

/// `dav1d_tx_types_per_set` — Intra2 set at [0..5], Intra1 at [5..12]
/// (TxfmType values; only the intra slices are used by the keyframe decoder).
pub(crate) static TX_TYPES_PER_SET: [u8; 12] = [
    // Intra2: IDTX, DCT_DCT, ADST_ADST, ADST_DCT, DCT_ADST
    9, 0, 3, 1, 2, // Intra1: IDTX, DCT_DCT, V_DCT, H_DCT, ADST_ADST, ADST_DCT, DCT_ADST
    9, 0, 10, 11, 3, 1, 2,
];

/// `dav1d_txtp_from_uvmode[uv_mode]` — implicit chroma tx type for intra (index
/// 13 = CFL_PRED defaults to DCT_DCT).
pub(crate) static TXTP_FROM_UVMODE: [u8; 14] = [0, 1, 2, 0, 3, 1, 2, 2, 1, 3, 1, 2, 3, 0];

/// `dav1d_filter_mode_to_y_mode` — FILTER_PRED's effective Y mode by `y_angle`.
pub(crate) static FILTER_MODE_TO_Y_MODE: [u8; 5] = [0, 1, 2, 6, 0];

/// TxfmType values used by the intra decoder.
pub(crate) mod txtp {
    pub const DCT_DCT: u8 = 0;
    pub const WHT_WHT: u8 = 16;
}

/// `dav1d_tx_type_class[txtp]` — 0 = 2D, 1 = horizontal (H), 2 = vertical (V).
/// Drives the eob CDF's `is_1d` index and the coefficient scan class.
pub(crate) static TX_TYPE_CLASS: [u8; 17] =
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 1, 2, 1, 2, 1, 0];
const TX_CLASS_2D: u8 = 0;

/// `dav1d_lo_ctx_offsets[shape][y][x]` — base context offset for `coeff_base`,
/// by block shape (0: w==h, 1: w>h, 2: w<h) and the coefficient's (x,y).
pub(crate) static LO_CTX_OFFSETS: [[[u8; 5]; 5]; 3] = [
    [
        [0, 1, 6, 6, 21],
        [1, 6, 6, 21, 21],
        [6, 6, 21, 21, 21],
        [6, 21, 21, 21, 21],
        [21, 21, 21, 21, 21],
    ],
    [
        [0, 16, 6, 6, 21],
        [16, 16, 6, 21, 21],
        [16, 16, 21, 21, 21],
        [16, 16, 21, 21, 21],
        [16, 16, 21, 21, 21],
    ],
    [
        [0, 11, 11, 11, 11],
        [11, 11, 11, 11, 11],
        [6, 6, 21, 21, 21],
        [6, 21, 21, 21, 21],
        [21, 21, 21, 21, 21],
    ],
];

/// `get_lo_ctx` (dav1d recon_tmpl.c): the `coeff_base` CDF context for a
/// coefficient at `(x, y)`, from the neighbouring magnitudes in the `levels`
/// grid (sliced at the coefficient's position). Returns `(ctx, hi_mag)` — the
/// `hi_mag` is reused for the base-range context. Bit-exact matters: a wrong ctx
/// can change a `tok==3` outcome and thus the symbol count (desync).
fn get_lo_ctx(
    levels: &[u8],
    tx_class: u8,
    x: usize,
    y: usize,
    stride: usize,
    ctx_offsets: &[[u8; 5]; 5],
) -> (usize, u32) {
    let g = |o: usize| levels.get(o).copied().unwrap_or(0) as u32;
    let mut mag = g(1) + g(stride);
    let hi_mag;
    let offset;
    if tx_class == TX_CLASS_2D {
        mag += g(stride + 1);
        hi_mag = mag;
        mag += g(2) + g(2 * stride);
        offset = ctx_offsets[y.min(4)][x.min(4)] as u32;
    } else {
        mag += g(2);
        hi_mag = mag;
        mag += g(3) + g(4);
        offset = 26 + if y > 1 { 10 } else { (y * 5) as u32 };
    }
    let ctx = offset + if mag > 512 { 4 } else { (mag + 64) >> 7 };
    (ctx as usize, hi_mag)
}

/// Map a scan index `i` to `(x, y, rc)` for a coefficient. 2D reads the scan
/// table; the 1-D classes transpose coordinates (dav1d DECODE_COEFS_CLASS).
fn coef_xyrc(
    tx: usize,
    tx_class: u8,
    i: usize,
    shift: usize,
    shift2: usize,
    mask: usize,
) -> (usize, usize, usize) {
    if tx_class == TX_CLASS_2D {
        let rc = SCANS[tx][i] as usize;
        (rc >> shift, rc & mask, rc)
    } else if tx_class == 1 {
        (i & mask, i >> shift, i) // TX_CLASS_H
    } else {
        let (x, y) = (i & mask, i >> shift); // TX_CLASS_V
        (x, y, (x << shift2) | y)
    }
}

/// `get_dc_sign_ctx` (dav1d recon_tmpl.c): the DC-sign CDF context for a
/// transform block. dav1d's SIMD masks/multiplies reduce to a scalar fold: each
/// neighbour byte's bits 6-7 encode the prior DC sign (`0x40>>6=1` none,
/// `0x80>>6=2` positive, `0x00>>6=0` negative), so `s = Σ(byte>>6) - (w+h)` is
/// `#positive - #negative` over the `1<<lw` above / `1<<lh` left bytes the tx
/// spans. Context = `(s != 0) + (s > 0)` → {neg:1, zero:0, pos:2}.
fn get_dc_sign_ctx(tx: usize, a: &[u8], l: &[u8]) -> usize {
    let tdim = TXFM_DIMENSIONS[tx];
    let wa = 1usize << tdim[0];
    let wl = 1usize << tdim[1];
    let mut s: i32 = 0;
    for &b in a.iter().take(wa) {
        s += (b >> 6) as i32;
    }
    for &b in l.iter().take(wl) {
        s += (b >> 6) as i32;
    }
    s -= (wa + wl) as i32;
    (s != 0) as usize + (s > 0) as usize
}

/// dav1d `dav1d_dq_tbl[0]` (8-bit): [qindex][dc=0, ac=1] dequant factors.
/// Transcribed from dav1d `src/dequant_tables.c` (BSD-2-Clause).
pub(crate) static DQ_TBL_8BIT: [[u16; 2]; 256] = [
    [4, 4], [8, 8], [8, 9], [9, 10], [10, 11], [11, 12], [12, 13], [12, 14],
    [13, 15], [14, 16], [15, 17], [16, 18], [17, 19], [18, 20], [19, 21], [19, 22],
    [20, 23], [21, 24], [22, 25], [23, 26], [24, 27], [25, 28], [26, 29], [26, 30],
    [27, 31], [28, 32], [29, 33], [30, 34], [31, 35], [32, 36], [32, 37], [33, 38],
    [34, 39], [35, 40], [36, 41], [37, 42], [38, 43], [38, 44], [39, 45], [40, 46],
    [41, 47], [42, 48], [43, 49], [43, 50], [44, 51], [45, 52], [46, 53], [47, 54],
    [48, 55], [48, 56], [49, 57], [50, 58], [51, 59], [52, 60], [53, 61], [53, 62],
    [54, 63], [55, 64], [56, 65], [57, 66], [57, 67], [58, 68], [59, 69], [60, 70],
    [61, 71], [62, 72], [62, 73], [63, 74], [64, 75], [65, 76], [66, 77], [66, 78],
    [67, 79], [68, 80], [69, 81], [70, 82], [70, 83], [71, 84], [72, 85], [73, 86],
    [74, 87], [74, 88], [75, 89], [76, 90], [77, 91], [78, 92], [78, 93], [79, 94],
    [80, 95], [81, 96], [81, 97], [82, 98], [83, 99], [84, 100], [85, 101], [85, 102],
    [87, 104], [88, 106], [90, 108], [92, 110], [93, 112], [95, 114], [96, 116], [98, 118],
    [99, 120], [101, 122], [102, 124], [104, 126], [105, 128], [107, 130], [108, 132], [110, 134],
    [111, 136], [113, 138], [114, 140], [116, 142], [117, 144], [118, 146], [120, 148], [121, 150],
    [123, 152], [125, 155], [127, 158], [129, 161], [131, 164], [134, 167], [136, 170], [138, 173],
    [140, 176], [142, 179], [144, 182], [146, 185], [148, 188], [150, 191], [152, 194], [154, 197],
    [156, 200], [158, 203], [161, 207], [164, 211], [166, 215], [169, 219], [172, 223], [174, 227],
    [177, 231], [180, 235], [182, 239], [185, 243], [187, 247], [190, 251], [192, 255], [195, 260],
    [199, 265], [202, 270], [205, 275], [208, 280], [211, 285], [214, 290], [217, 295], [220, 300],
    [223, 305], [226, 311], [230, 317], [233, 323], [237, 329], [240, 335], [243, 341], [247, 347],
    [250, 353], [253, 359], [257, 366], [261, 373], [265, 380], [269, 387], [272, 394], [276, 401],
    [280, 408], [284, 416], [288, 424], [292, 432], [296, 440], [300, 448], [304, 456], [309, 465],
    [313, 474], [317, 483], [322, 492], [326, 501], [330, 510], [335, 520], [340, 530], [344, 540],
    [349, 550], [354, 560], [359, 571], [364, 582], [369, 593], [374, 604], [379, 615], [384, 627],
    [389, 639], [395, 651], [400, 663], [406, 676], [411, 689], [417, 702], [423, 715], [429, 729],
    [435, 743], [441, 757], [447, 771], [454, 786], [461, 801], [467, 816], [475, 832], [482, 848],
    [489, 864], [497, 881], [505, 898], [513, 915], [522, 933], [530, 951], [539, 969], [549, 988],
    [559, 1007], [569, 1026], [579, 1046], [590, 1066], [602, 1087], [614, 1108], [626, 1129], [640, 1151],
    [654, 1173], [668, 1196], [684, 1219], [700, 1243], [717, 1267], [736, 1292], [755, 1317], [775, 1343],
    [796, 1369], [819, 1396], [843, 1423], [869, 1451], [896, 1479], [925, 1508], [955, 1537], [988, 1567],
    [1022, 1597], [1058, 1628], [1098, 1660], [1139, 1692], [1184, 1725], [1232, 1759], [1282, 1793], [1336, 1828],
];

/// `init_quant_tables` (dav1d decode.c): per-segment, per-plane `[dc, ac]`
/// dequant factors. Luma AC is the base qindex; every other component offsets it
/// by its frame-header delta (and, per segment, the segmentation Q delta). 8-bit
/// only (`dav1d_dq_tbl[0]`); all indices clamped to `0..=255`.
fn init_dq(fh: &super::FrameHeader) -> [[[u16; 2]; 3]; 8] {
    let mut dq = [[[0u16; 2]; 3]; 8];
    let nseg = if fh.segmentation_enabled { 8 } else { 1 };
    let base = fh.base_q_idx as i32;
    for (i, seg) in dq.iter_mut().enumerate().take(nseg) {
        let yac = if fh.segmentation_enabled {
            (base + fh.feature_data[i][0]).clamp(0, 255)
        } else {
            base
        };
        let idx = |delta: i32| (yac + delta).clamp(0, 255) as usize;
        let ydc = idx(fh.delta_q_y_dc);
        let uac = idx(fh.delta_q_u_ac);
        let udc = idx(fh.delta_q_u_dc);
        let vac = idx(fh.delta_q_v_ac);
        let vdc = idx(fh.delta_q_v_dc);
        seg[0] = [DQ_TBL_8BIT[ydc][0], DQ_TBL_8BIT[yac as usize][1]];
        seg[1] = [DQ_TBL_8BIT[udc][0], DQ_TBL_8BIT[uac][1]];
        seg[2] = [DQ_TBL_8BIT[vdc][0], DQ_TBL_8BIT[vac][1]];
    }
    dq
}

/// `get_skip_ctx` (dav1d recon_tmpl.c): the txb_skip (all-zero) CDF context for a
/// transform block, from the above/left coefficient-level neighbour bytes
/// (`0x40` = "no token"). dav1d's SIMD byte-pattern reads reduce to an OR-fold
/// over the bytes the tx spans (`1<<lw` above, `1<<lh` left).
fn get_skip_ctx(
    bs: u8,
    tx: usize,
    chroma: bool,
    ss_h: bool,
    ss_v: bool,
    above: &[u8],
    left: &[u8],
) -> usize {
    let bdim = BLOCK_DIMENSIONS[bs as usize];
    let (blw, blh) = (bdim[2], bdim[3]);
    let (tlw, tlh) = (TXFM_DIMENSIONS[tx][0], TXFM_DIMENSIONS[tx][1]);
    let aw = 1usize << tlw;
    let lhh = 1usize << tlh;
    if chroma {
        let nob = (blw - (blw != 0 && ss_h) as u8) > tlw || (blh - (blh != 0 && ss_v) as u8) > tlh;
        let ca = above.iter().take(aw).any(|&b| b != 0x40) as usize;
        let cl = left.iter().take(lhh).any(|&b| b != 0x40) as usize;
        7 + nob as usize * 3 + ca + cl
    } else if blw == tlw && blh == tlh {
        0
    } else {
        let la = (above.iter().take(aw).fold(0u8, |a, &b| a | b) & 0x3F) as usize;
        let ll = (left.iter().take(lhh).fold(0u8, |a, &b| a | b) & 0x3F) as usize;
        SKIP_CTX[la.min(4)][ll.min(4)] as usize
    }
}

/// `dav1d_max_txfm_size_for_bs[bs][0]` — the luma transform size for a block,
/// the starting point of the intra tx-size descent.
pub(crate) static MAX_YTX: [u8; 22] = [
    4, 4, 4, 4, 12, 18, 11, 3, 10, 16, 17, 9, 2, 8, 14, 15, 7, 1, 6, 13, 5, 0,
];

/// `dav1d_max_txfm_size_for_bs[bs][layout]` — max transform size per block
/// size and chroma layout (0=luma/I400, 1=I420, 2=I422, 3=I444). Column 0
/// equals `MAX_YTX`; the others give `uvtx`. From dav1d `src/tables.c`.
pub(crate) static MAX_TXFM_SIZE_FOR_BS: [[u8; 4]; 22] = [
    [4, 3, 3, 3], // BS_128x128
    [4, 3, 3, 3], // BS_128x64
    [4, 3, 0, 3], // BS_64x128
    [4, 3, 3, 3], // BS_64x64
    [12, 10, 3, 3], // BS_64x32
    [18, 16, 10, 10], // BS_64x16
    [11, 9, 0, 3], // BS_32x64
    [3, 2, 9, 3], // BS_32x32
    [10, 8, 2, 10], // BS_32x16
    [16, 14, 8, 16], // BS_32x8
    [17, 15, 0, 9], // BS_16x64
    [9, 7, 0, 9], // BS_16x32
    [2, 1, 7, 2], // BS_16x16
    [8, 6, 1, 8], // BS_16x8
    [14, 6, 6, 14], // BS_16x4
    [15, 13, 0, 15], // BS_8x32
    [7, 5, 0, 7], // BS_8x16
    [1, 0, 5, 1], // BS_8x8
    [6, 0, 0, 6], // BS_8x4
    [13, 5, 0, 13], // BS_4x16
    [5, 0, 0, 5], // BS_4x8
    [0, 0, 0, 0], // BS_4x4
];

/// The adapting CDF state a frame's intra tile decode mutates. Seeded from the
/// `cdf::` defaults; `symbol_adapt`/`bool_adapt` update entries in place (unless
/// the frame set `disable_cdf_update`).
pub(crate) struct Cdf {
    pub partition: [[[u16; 16]; 4]; 5],
    pub skip: [[u16; 2]; 3],
    pub kfym: [[[u16; 16]; 5]; 5],
    pub angle_delta: [[u16; 8]; 8],
    pub uv_mode: [[[u16; 16]; 13]; 2],
    pub cfl_sign: [u16; 8],
    pub cfl_alpha: [[u16; 16]; 6],
    pub use_filter_intra: [[u16; 2]; 22],
    pub filter_intra: [u16; 8],
    pub txsz: [[[u16; 4]; 3]; 4],
    pub pal_y: [[[u16; 2]; 3]; 7],
    pub pal_uv: [[u16; 2]; 2],
    // Intra transform-type CDFs (frame-global, not qcat-indexed).
    pub txtp_intra1: [[[u16; 8]; 13]; 2],
    pub txtp_intra2: [[[u16; 8]; 13]; 3],
    // Coefficient CDFs — the `qcat`-selected slice of the `cdf::*_Q[4]` tables.
    pub coef_skip: [[[u16; 2]; 13]; 5],
    pub eob_bin_16: [[[u16; 8]; 2]; 2],
    pub eob_bin_32: [[[u16; 8]; 2]; 2],
    pub eob_bin_64: [[[u16; 8]; 2]; 2],
    pub eob_bin_128: [[[u16; 8]; 2]; 2],
    pub eob_bin_256: [[[u16; 16]; 2]; 2],
    pub eob_bin_512: [[u16; 16]; 2],
    pub eob_bin_1024: [[u16; 16]; 2],
    pub eob_hi_bit: [[[[u16; 2]; 9]; 2]; 5],
    pub coeff_base_eob: [[[[u16; 4]; 4]; 2]; 5],
    pub coeff_base: [[[[u16; 4]; 41]; 2]; 5],
    pub coeff_br: [[[[u16; 4]; 21]; 2]; 4],
    pub dc_sign: [[[u16; 2]; 3]; 2],
}

impl Cdf {
    /// Seed all tables from the defaults; the coefficient CDFs are taken from the
    /// `qcat` (quantizer-category) slice — `qcat = (q>20)+(q>60)+(q>120)`.
    pub fn new(qcat: usize) -> Self {
        Cdf {
            partition: cdf::PARTITION,
            skip: cdf::SKIP,
            kfym: cdf::KF_Y_MODE,
            angle_delta: cdf::ANGLE_DELTA,
            uv_mode: cdf::UV_MODE,
            cfl_sign: cdf::CFL_SIGN,
            cfl_alpha: cdf::CFL_ALPHA,
            use_filter_intra: cdf::USE_FILTER_INTRA,
            filter_intra: cdf::FILTER_INTRA_MODE,
            txsz: cdf::TX_SIZE,
            pal_y: cdf::PAL_Y_MODE,
            pal_uv: cdf::PAL_UV_MODE,
            txtp_intra1: cdf::TXTP_INTRA1,
            txtp_intra2: cdf::TXTP_INTRA2,
            coef_skip: cdf::COEF_SKIP_Q[qcat],
            eob_bin_16: cdf::EOB_BIN_16_Q[qcat],
            eob_bin_32: cdf::EOB_BIN_32_Q[qcat],
            eob_bin_64: cdf::EOB_BIN_64_Q[qcat],
            eob_bin_128: cdf::EOB_BIN_128_Q[qcat],
            eob_bin_256: cdf::EOB_BIN_256_Q[qcat],
            eob_bin_512: cdf::EOB_BIN_512_Q[qcat],
            eob_bin_1024: cdf::EOB_BIN_1024_Q[qcat],
            eob_hi_bit: cdf::EOB_HI_BIT_Q[qcat],
            coeff_base_eob: cdf::EOB_BASE_TOK_Q[qcat],
            coeff_base: cdf::COEFF_BASE_Q[qcat],
            coeff_br: cdf::COEFF_BR_Q[qcat],
            dc_sign: cdf::DC_SIGN_Q[qcat],
        }
    }
}

/// `qcat = (q>20)+(q>60)+(q>120)` — selects the coefficient CDF set (AV1 §).
pub(crate) fn qcat_for(base_q_idx: u32) -> usize {
    (base_q_idx > 20) as usize + (base_q_idx > 60) as usize + (base_q_idx > 120) as usize
}

/// Byte offset of the tile data inside an `OBU_FRAME` payload: the frame header
/// is byte-aligned (`frame_obu` → `byte_alignment`), and a single-tile
/// `tile_group_obu` adds no bytes before the coded tile.
pub(crate) fn tile_data_offset(header_bits: usize) -> usize {
    header_bits.div_ceil(8)
}

/// AV1 intra tile decoder: walks the superblock partition tree, decoding each
/// node's partition symbol from the `Msac` (full symbol for interior nodes, the
/// gather-probability binary at frame edges) and recursing to coding blocks. The
/// per-block leaf decode (intra mode, coefficients, reconstruction) is layered
/// on in follow-up iterations; for now `decode_block` is a counting stub, so the
/// recursion + entropy wiring are exercised but pixels are not yet produced.
pub(crate) struct Av1Tile<'a> {
    pub msac: Msac<'a>,
    pub geom: TileGeom,
    /// Superblock level: `BL_128X128` or `BL_64X64`.
    pub sb_bl: u8,
    /// Mutable, adapting CDF state.
    cdf: Cdf,
    /// Per-segment, per-plane `[dc, ac]` dequant factors (`init_quant_tables`).
    dq: [[[u16; 2]; 3]; 8],
    /// Per-segment lossless flag (drives WHT_WHT + skips dequant rounding).
    lossless: [bool; 8],
    /// Reconstructed 8-bit pixel planes (Y, U, V); chroma sized by subsampling.
    planes: [Vec<u8>; 3],
    plane_w: [usize; 3],
    plane_h: [usize; 3],
    /// Chroma subsampling (`monochrome`/`420`/`444`), drives `has_chroma`.
    mono_chrome: bool,
    subsampling_x: bool,
    subsampling_y: bool,
    enable_filter_intra: bool,
    /// `seq_hdr.enable_intra_edge_filter` — gates directional edge low-pass +
    /// upsampling (`angle | enable<<10`) and the Z2 corner 3-tap filter.
    intra_edge_filter: bool,
    tx_mode_select: bool,
    reduced_tx_set: bool,
    allow_screen_content_tools: bool,
    above_partition: Vec<u8>,
    left_partition: [u8; 16],
    /// Above/left neighbour Y-mode (per 4×4) for the keyframe Y-mode context.
    above_mode: Vec<u8>,
    left_mode: [u8; 32],
    /// Above/left neighbour UV-mode (per 4×4, luma-indexed) for the chroma
    /// smooth-neighbour flag (`sm_uv_flag`) used by directional edge filtering.
    above_uvmode: Vec<u8>,
    left_uvmode: [u8; 32],
    above_skip: Vec<u8>,
    left_skip: [u8; 32],
    /// Above/left neighbour tx log2-dim for the tx-size context (`-1` = unset).
    above_tx: Vec<i8>,
    left_tx: [i8; 32],
    above_pal: Vec<u8>,
    left_pal: [u8; 32],
    /// Coefficient-level neighbour context (luma + 2 chroma planes), `0x40` =
    /// "no token yet" — consumed by the coefficient decoder's skip/level contexts.
    above_lcoef: Vec<u8>,
    left_lcoef: [u8; 32],
    above_ccoef: [Vec<u8>; 2],
    left_ccoef: [[u8; 32]; 2],
    pub blocks_visited: u32,
}

impl<'a> Av1Tile<'a> {
    /// Build a tile decoder over `tile_bytes` (the coded tile data) for a frame
    /// described by its sequence + frame headers.
    pub fn new(
        tile_bytes: &'a [u8],
        mi_cols: u32,
        mi_rows: u32,
        seq: &super::SequenceHeader,
        fh: &super::FrameHeader,
    ) -> Self {
        let cols = mi_cols as usize;
        Av1Tile {
            msac: Msac::new(tile_bytes, fh.disable_cdf_update),
            geom: TileGeom { mi_cols, mi_rows },
            sb_bl: if seq.use_128x128_superblock {
                BL_128X128
            } else {
                BL_64X64
            },
            cdf: Cdf::new(qcat_for(fh.base_q_idx)),
            dq: init_dq(fh),
            lossless: fh.lossless,
            planes: {
                let yw = cols * 4;
                let yh = mi_rows as usize * 4;
                if seq.mono_chrome {
                    [vec![0u8; yw * yh], Vec::new(), Vec::new()]
                } else {
                    let cw = yw >> (seq.subsampling_x != 0) as usize;
                    let ch = yh >> (seq.subsampling_y != 0) as usize;
                    [vec![0u8; yw * yh], vec![0u8; cw * ch], vec![0u8; cw * ch]]
                }
            },
            plane_w: {
                let yw = cols * 4;
                if seq.mono_chrome {
                    [yw, 0, 0]
                } else {
                    let cw = yw >> (seq.subsampling_x != 0) as usize;
                    [yw, cw, cw]
                }
            },
            plane_h: {
                let yh = mi_rows as usize * 4;
                if seq.mono_chrome {
                    [yh, 0, 0]
                } else {
                    let ch = yh >> (seq.subsampling_y != 0) as usize;
                    [yh, ch, ch]
                }
            },
            mono_chrome: seq.mono_chrome,
            subsampling_x: seq.subsampling_x != 0,
            subsampling_y: seq.subsampling_y != 0,
            enable_filter_intra: seq.enable_filter_intra,
            intra_edge_filter: seq.enable_intra_edge_filter,
            tx_mode_select: fh.tx_mode_select,
            reduced_tx_set: fh.reduced_tx_set,
            allow_screen_content_tools: fh.allow_screen_content_tools,
            above_partition: vec![0u8; (cols >> 1) + 32],
            left_partition: [0u8; 16],
            above_mode: vec![mode::DC_PRED; cols + 32],
            left_mode: [mode::DC_PRED; 32],
            above_uvmode: vec![mode::DC_PRED; cols + 32],
            left_uvmode: [mode::DC_PRED; 32],
            above_skip: vec![0u8; cols + 32],
            left_skip: [0u8; 32],
            above_tx: vec![-1i8; cols + 32],
            left_tx: [-1i8; 32],
            above_pal: vec![0u8; cols + 32],
            left_pal: [0u8; 32],
            above_lcoef: vec![0x40u8; cols + 32],
            left_lcoef: [0x40u8; 32],
            above_ccoef: [vec![0x40u8; cols + 32], vec![0x40u8; cols + 32]],
            left_ccoef: [[0x40u8; 32]; 2],
            blocks_visited: 0,
        }
    }

    /// Decode the whole tile by walking its superblock grid.
    pub fn decode(&mut self) {
        let sb4: u32 = if self.sb_bl == BL_128X128 { 32 } else { 16 };
        let mut row = 0;
        while row < self.geom.mi_rows {
            // Reset the left neighbour contexts at the start of each SB row.
            self.left_partition = [0u8; 16];
            self.left_mode = [mode::DC_PRED; 32];
            self.left_uvmode = [mode::DC_PRED; 32];
            self.left_skip = [0u8; 32];
            self.left_tx = [-1i8; 32];
            self.left_pal = [0u8; 32];
            self.left_lcoef = [0x40u8; 32];
            self.left_ccoef = [[0x40u8; 32]; 2];
            let mut col = 0;
            while col < self.geom.mi_cols {
                self.decode_sb(row, col, self.sb_bl);
                col += sb4;
            }
            row += sb4;
        }
    }

    fn read_partition(&mut self, bl: u8, mi_row: u32, mi_col: u32, case: PartCase) -> u8 {
        let ctx = get_partition_ctx(&self.above_partition, &self.left_partition, bl, mi_row, mi_col);
        let bl_i = bl as usize;
        match case {
            PartCase::Both => {
                let n = PARTITION_TYPE_COUNT[bl_i];
                self.msac.symbol_adapt(&mut self.cdf.partition[bl_i][ctx], n) as u8
            }
            PartCase::ColsOnly => {
                let prob = gather_top_partition_prob(&self.cdf.partition[bl_i][ctx], bl);
                if self.msac.bool_p(prob) != 0 {
                    part::SPLIT
                } else {
                    part::H
                }
            }
            PartCase::RowsOnly => {
                let prob = gather_left_partition_prob(&self.cdf.partition[bl_i][ctx], bl);
                if self.msac.bool_p(prob) != 0 {
                    part::SPLIT
                } else {
                    part::V
                }
            }
            PartCase::Neither => part::SPLIT,
        }
    }

    fn update_partition_ctx(&mut self, bl: u8, mi_row: u32, mi_col: u32, bp: u8) {
        let run = (16u32 >> bl) as usize; // 8×8 columns/rows the node spans
        let a_val = AL_PART_CTX[0][bl as usize][bp as usize];
        let l_val = AL_PART_CTX[1][bl as usize][bp as usize];
        let acol = (mi_col >> 1) as usize;
        for i in 0..run {
            if acol + i < self.above_partition.len() {
                self.above_partition[acol + i] = a_val;
            }
        }
        let lrow = ((mi_row & 31) >> 1) as usize;
        for i in 0..run {
            if lrow + i < 16 {
                self.left_partition[lrow + i] = l_val;
            }
        }
    }

    /// Decode an intra-keyframe block's mode info (skip, Y/UV intra mode, angle,
    /// CfL, filter-intra), keeping the entropy stream synchronised through these
    /// symbols. Palette, transform-size descent, segmentation/delta-q reads and
    /// the coefficient decode + reconstruction (pixels) are layered on next.
    fn decode_block(&mut self, mi_row: u32, mi_col: u32, bs: u8) {
        let dim = BLOCK_DIMENSIONS[bs as usize];
        let (bw4, bh4) = (dim[0] as u32, dim[1] as u32);
        let (lw, lh) = (dim[2], dim[3]);
        let acol = mi_col as usize;
        let by4 = (mi_row & 31) as usize;

        // skip flag
        let sctx = (self.above_skip[acol] + self.left_skip[by4]) as usize;
        let skip = self.msac.bool_adapt(&mut self.cdf.skip[sctx]);

        // Keyframe ⇒ intra (intrabc handled when allow_screen_content_tools lands).

        // Y intra mode: keyframe Y-mode CDF, indexed by neighbour mode contexts.
        let a_ctx = INTRA_MODE_CONTEXT[self.above_mode[acol] as usize] as usize;
        let l_ctx = INTRA_MODE_CONTEXT[self.left_mode[by4] as usize] as usize;
        let n_y = (mode::N_INTRA_PRED_MODES - 1) as usize;
        let mut y_mode = self.msac.symbol_adapt(&mut self.cdf.kfym[a_ctx][l_ctx], n_y) as u8;

        // Smooth-neighbour flags for directional edge filtering (`sm_flag`), sampled
        // from the neighbour modes BEFORE this block overwrites them.
        let is_smooth = |m: u8| {
            m == mode::SMOOTH_PRED || m == mode::SMOOTH_V_PRED || m == mode::SMOOTH_H_PRED
        };
        let is_sm_y = is_smooth(self.above_mode[acol]) || is_smooth(self.left_mode[by4]);
        let is_sm_uv = is_smooth(self.above_uvmode[acol]) || is_smooth(self.left_uvmode[by4]);

        // Y angle delta (directional modes, blocks ≥ 8×8): the directional angle is
        // `base + 3 * delta`, with delta in [-3, 3] (symbol 0..6 minus 3).
        let mut y_angle_delta = 0i32;
        if lw + lh >= 2 && (mode::VERT_PRED..=mode::VERT_LEFT_PRED).contains(&y_mode) {
            let idx = (y_mode - mode::VERT_PRED) as usize;
            y_angle_delta = self.msac.symbol_adapt(&mut self.cdf.angle_delta[idx], 6) as i32 - 3;
        }

        // Chroma intra mode (+ CfL) when this block carries chroma samples.
        let ss_h = self.subsampling_x as u32;
        let ss_v = self.subsampling_y as u32;
        let has_chroma = !self.mono_chrome
            && (bw4 > ss_h || (mi_col & 1) == 1)
            && (bh4 > ss_v || (mi_row & 1) == 1);
        let mut uv_mode = mode::DC_PRED;
        // CfL alpha [u, v] (signed magnitude, decoded when uv_mode == CFL_PRED).
        let mut cfl_alpha = [0i32; 2];
        let mut uv_angle_delta = 0i32;
        if has_chroma {
            let cfl_allowed = bw4 <= 8 && bh4 <= 8;
            let n_uv = (mode::N_UV_INTRA_PRED_MODES - 1) as usize - (!cfl_allowed as usize);
            uv_mode = self
                .msac
                .symbol_adapt(&mut self.cdf.uv_mode[cfl_allowed as usize][y_mode as usize], n_uv)
                as u8;
            if uv_mode == mode::CFL_PRED {
                let sign = self.msac.symbol_adapt(&mut self.cdf.cfl_sign, 7) + 1;
                let sign_u = (sign * 0x56) >> 8; // = sign / 3
                let sign_v = sign - sign_u * 3;
                if sign_u != 0 {
                    let ctx = (sign_u == 2) as usize * 3 + sign_v;
                    let mag = self.msac.symbol_adapt(&mut self.cdf.cfl_alpha[ctx], 15) as i32 + 1;
                    cfl_alpha[0] = if sign_u == 1 { -mag } else { mag };
                }
                if sign_v != 0 {
                    let ctx = (sign_v == 2) as usize * 3 + sign_u;
                    let mag = self.msac.symbol_adapt(&mut self.cdf.cfl_alpha[ctx], 15) as i32 + 1;
                    cfl_alpha[1] = if sign_v == 1 { -mag } else { mag };
                }
            } else if lw + lh >= 2 && (mode::VERT_PRED..=mode::VERT_LEFT_PRED).contains(&uv_mode) {
                let idx = (uv_mode - mode::VERT_PRED) as usize;
                uv_angle_delta = self.msac.symbol_adapt(&mut self.cdf.angle_delta[idx], 6) as i32 - 3;
            }
        }

        // Palette (screen-content frames only). The Y/UV palette-mode flags must
        // be read to stay in sync; reading the palette itself (`read_pal_indices`)
        // is not yet implemented, but photographic AVIF encoders disable
        // screen-content tools so the flags read 0.
        if self.allow_screen_content_tools && bw4.max(bh4) <= 16 && bw4 + bh4 >= 4 {
            let sz_ctx = (lw + lh - 2) as usize;
            if y_mode == mode::DC_PRED {
                let pal_ctx =
                    (self.above_pal[acol] > 0) as usize + (self.left_pal[by4] > 0) as usize;
                let use_y_pal = self.msac.bool_adapt(&mut self.cdf.pal_y[sz_ctx][pal_ctx]);
                debug_assert!(use_y_pal == 0, "TODO(palette): read_pal_indices");
            }
            if has_chroma && uv_mode == mode::DC_PRED {
                let use_uv_pal = self.msac.bool_adapt(&mut self.cdf.pal_uv[0]);
                debug_assert!(use_uv_pal == 0, "TODO(palette): read_pal_uv");
            }
        }

        // filter-intra: DC_PRED, blocks ≤ 8×8. The chosen filter index becomes
        // `y_angle`, used to derive the luma tx-type (`FILTER_MODE_TO_Y_MODE`).
        let mut y_angle = 0u8;
        if y_mode == mode::DC_PRED && lw.max(lh) <= 3 && self.enable_filter_intra {
            let is_filter = self.msac.bool_adapt(&mut self.cdf.use_filter_intra[bs as usize]);
            if is_filter != 0 {
                y_angle = self.msac.symbol_adapt(&mut self.cdf.filter_intra, 4) as u8;
                y_mode = mode::FILTER_PRED;
            }
        }

        // Transform size (intra): start at the block's max luma tx, then descend
        // via the size-split symbol when the frame uses switchable transforms.
        let mut tx = MAX_YTX[bs as usize] as usize;
        let tmax = TXFM_DIMENSIONS[tx][2];
        if self.tx_mode_select && tmax > 0 {
            let max_lw = TXFM_DIMENSIONS[tx][0] as i8;
            let max_lh = TXFM_DIMENSIONS[tx][1] as i8;
            let tctx =
                (self.left_tx[by4] >= max_lh) as usize + (self.above_tx[acol] >= max_lw) as usize;
            let n = (tmax as usize).min(2);
            let mut depth = self.msac.symbol_adapt(&mut self.cdf.txsz[(tmax - 1) as usize][tctx], n);
            while depth > 0 {
                tx = TXFM_DIMENSIONS[tx][3] as usize;
                depth -= 1;
            }
        }
        let (tlw, tlh) = (TXFM_DIMENSIONS[tx][0] as i8, TXFM_DIMENSIONS[tx][1] as i8);
        for i in 0..bw4 as usize {
            if acol + i < self.above_tx.len() {
                self.above_tx[acol + i] = tlw;
            }
        }
        for i in 0..bh4 as usize {
            if by4 + i < 32 {
                self.left_tx[by4 + i] = tlh;
            }
        }

        // Coefficient decode: walk the block's transform grid (luma then chroma),
        // reading each transform block's residual. A skipped block leaves every
        // coefficient context at 0x40 across its span.
        let layout = if self.mono_chrome {
            0 // I400
        } else if !self.subsampling_x {
            3 // I444
        } else if !self.subsampling_y {
            2 // I422
        } else {
            1 // I420
        };
        let uvtx = MAX_TXFM_SIZE_FOR_BS[bs as usize][layout] as usize;
        if skip != 0 {
            for i in 0..bw4 as usize {
                if acol + i < self.above_lcoef.len() {
                    self.above_lcoef[acol + i] = 0x40;
                }
            }
            for i in 0..bh4 as usize {
                if by4 + i < 32 {
                    self.left_lcoef[by4 + i] = 0x40;
                }
            }
            if has_chroma {
                let cbw4 = ((bw4 + ss_h) >> ss_h) as usize;
                let cbh4 = ((bh4 + ss_v) >> ss_v) as usize;
                let cacol = acol >> ss_h;
                let cby4 = by4 >> ss_v;
                for pl in 0..2 {
                    for i in 0..cbw4 {
                        if cacol + i < self.above_ccoef[pl].len() {
                            self.above_ccoef[pl][cacol + i] = 0x40;
                        }
                    }
                    for i in 0..cbh4 {
                        if cby4 + i < 32 {
                            self.left_ccoef[pl][cby4 + i] = 0x40;
                        }
                    }
                }
            }
        } else {
            self.decode_residuals(
                mi_row, mi_col, bs, tx, uvtx, y_mode, y_angle, uv_mode, cfl_alpha, has_chroma,
                y_angle_delta, uv_angle_delta, is_sm_y, is_sm_uv,
            );
        }

        // Propagate this block's mode/skip into the neighbour contexts.
        let y_nofilt = if y_mode == mode::FILTER_PRED {
            mode::DC_PRED
        } else {
            y_mode
        };
        for i in 0..bw4 as usize {
            if acol + i < self.above_mode.len() {
                self.above_mode[acol + i] = y_nofilt;
                self.above_uvmode[acol + i] = uv_mode;
                self.above_skip[acol + i] = skip as u8;
            }
        }
        for i in 0..bh4 as usize {
            if by4 + i < 32 {
                self.left_mode[by4 + i] = y_nofilt;
                self.left_uvmode[by4 + i] = uv_mode;
                self.left_skip[by4 + i] = skip as u8;
            }
        }
        self.blocks_visited += 1;
    }

    /// Decode a transform block's header: txb_skip (all-zero) and, when not
    /// skipped, the intra transform type. Returns `(all_skip, txtp)`. The eob +
    /// coefficient levels and the tx-grid wiring land in 7c/7d.
    #[allow(clippy::too_many_arguments)]
    fn decode_txb_header(
        &mut self,
        tx: usize,
        bs: u8,
        plane: usize,
        y_mode: u8,
        y_angle: u8,
        uv_mode: u8,
        lossless: bool,
        reduced_txtp: bool,
        a_off: usize,
        l_off: usize,
    ) -> (bool, u8) {
        let tdim = TXFM_DIMENSIONS[tx];
        let tctx = tdim[5] as usize;
        let chroma = plane != 0;
        let sctx = {
            let (above, left): (&[u8], &[u8]) = if chroma {
                (
                    &self.above_ccoef[plane - 1][a_off..],
                    &self.left_ccoef[plane - 1][l_off..],
                )
            } else {
                (&self.above_lcoef[a_off..], &self.left_lcoef[l_off..])
            };
            get_skip_ctx(
                bs,
                tx,
                chroma,
                self.subsampling_x,
                self.subsampling_y,
                above,
                left,
            )
        };
        let all_skip = self.msac.bool_adapt(&mut self.cdf.coef_skip[tctx][sctx]) != 0;
        if all_skip {
            return (true, if lossless { txtp::WHT_WHT } else { txtp::DCT_DCT });
        }
        let (tmax, tmin) = (tdim[2], tdim[4] as usize);
        let ty = if lossless {
            txtp::WHT_WHT
        } else if tmax + 1 >= 4 {
            // t_dim.max + intra(1) >= TX_64X64
            txtp::DCT_DCT
        } else if chroma {
            TXTP_FROM_UVMODE[uv_mode as usize]
        } else {
            let y_nofilt = if y_mode == mode::FILTER_PRED {
                FILTER_MODE_TO_Y_MODE[y_angle as usize]
            } else {
                y_mode
            };
            if reduced_txtp || tmin == 2 {
                let idx = self.msac.symbol_adapt(&mut self.cdf.txtp_intra2[tmin][y_nofilt as usize], 4);
                TX_TYPES_PER_SET[idx]
            } else {
                let idx = self.msac.symbol_adapt(&mut self.cdf.txtp_intra1[tmin][y_nofilt as usize], 6);
                TX_TYPES_PER_SET[idx + 5]
            }
        };
        (false, ty)
    }

    /// Decode the end-of-block position (count of coefficients) for a transform
    /// block of type `txtp`. dav1d decode_coefs eob section: an exp-Golomb-style
    /// `eob_pt` symbol (table chosen by tx area, indexed by chroma + is_1d) plus,
    /// when `eob_pt>1`, an adaptive hi bit and `eob_pt-2` equiprobable bits.
    fn decode_eob(&mut self, tx: usize, chroma: bool, txtp: u8) -> i32 {
        let tdim = TXFM_DIMENSIONS[tx];
        let tx2dszctx = (tdim[0] as usize).min(3) + (tdim[1] as usize).min(3);
        let is_1d = (TX_TYPE_CLASS[txtp as usize] != TX_CLASS_2D) as usize;
        let c = chroma as usize;
        let n = 4 + tx2dszctx;
        let mut eob = match tx2dszctx {
            0 => self.msac.symbol_adapt(&mut self.cdf.eob_bin_16[c][is_1d], n),
            1 => self.msac.symbol_adapt(&mut self.cdf.eob_bin_32[c][is_1d], n),
            2 => self.msac.symbol_adapt(&mut self.cdf.eob_bin_64[c][is_1d], n),
            3 => self.msac.symbol_adapt(&mut self.cdf.eob_bin_128[c][is_1d], n),
            4 => self.msac.symbol_adapt(&mut self.cdf.eob_bin_256[c][is_1d], n),
            5 => self.msac.symbol_adapt(&mut self.cdf.eob_bin_512[c], n),
            _ => self.msac.symbol_adapt(&mut self.cdf.eob_bin_1024[c], n),
        } as i32;
        if eob > 1 {
            let eob_bin = (eob - 2) as u32;
            let hi = self
                .msac
                .bool_adapt(&mut self.cdf.eob_hi_bit[tdim[5] as usize][c][(eob - 2) as usize])
                as i32;
            eob = ((hi | 2) << eob_bin) | self.msac.bools(eob_bin) as i32;
        }
        eob
    }

    /// Decode the coefficient magnitude tokens of a transform block in reverse
    /// scan order (eob → DC), returning per-position tokens `cf` (sized to the
    /// tx's coefficient count). Faithful to dav1d `DECODE_COEFS_CLASS`. Signs,
    /// golomb extension and dequant are applied in 7d-3. `eob` is the last
    /// non-zero scan index from `decode_eob`.
    fn decode_coef_levels(&mut self, tx: usize, chroma: bool, txtp: u8, eob: i32) -> Vec<i32> {
        let tdim = TXFM_DIMENSIONS[tx];
        let tctx = tdim[5] as usize;
        let br = tctx.min(3);
        let c = chroma as usize;
        let tx_class = TX_TYPE_CLASS[txtp as usize];
        let slw = (tdim[0] as usize).min(3);
        let slh = (tdim[1] as usize).min(3);
        let tx2dszctx = slw + slh;
        let n_coef = SCANS[tx].len();
        let mut cf = vec![0i32; n_coef];

        if eob <= 0 {
            // dc-only block.
            let tok_br = self.msac.symbol_adapt(&mut self.cdf.coeff_base_eob[tctx][c][0], 2);
            cf[0] = if tok_br == 2 {
                self.msac.decode_hi_tok(&mut self.cdf.coeff_br[br][c][0]) as i32
            } else {
                1 + tok_br as i32
            };
            return cf;
        }

        let (stride, shift, shift2, mask, shape) = match tx_class {
            TX_CLASS_2D => {
                let nonsquare = (tx >= 5) as usize; // tx >= RTX_4X8
                (
                    4usize << slh,
                    slh + 2,
                    0usize,
                    (4usize << slh) - 1,
                    nonsquare + (tx & nonsquare),
                )
            }
            1 => (16usize, slh + 2, 0usize, (4usize << slh) - 1, 0), // TX_CLASS_H
            _ => (16usize, slw + 2, slh + 2, (4usize << slw) - 1, 0), // TX_CLASS_V
        };
        let offsets = &LO_CTX_OFFSETS[shape];
        let mut levels = vec![0u8; stride * ((4 << slw.max(slh)) + 2) + 2 * stride + 8];

        // eob coefficient.
        let eob_ctx =
            1 + (eob > (2 << tx2dszctx)) as usize + (eob > (4 << tx2dszctx)) as usize;
        let eob_tok = self.msac.symbol_adapt(&mut self.cdf.coeff_base_eob[tctx][c][eob_ctx], 2);
        let (x, y, rc) = coef_xyrc(tx, tx_class, eob as usize, shift, shift2, mask);
        let (tok, lvl) = if eob_tok == 2 {
            let big = if tx_class == TX_CLASS_2D {
                (x | y) > 1
            } else {
                y != 0
            };
            let hctx = if big { 14 } else { 7 };
            let t = self.msac.decode_hi_tok(&mut self.cdf.coeff_br[br][c][hctx]) as i32;
            (t, (t + (3 << 6)) as u8)
        } else {
            let t = eob_tok as i32 + 1;
            (t, (t * 0x41) as u8)
        };
        cf[rc] = tok;
        levels[if tx_class == TX_CLASS_2D { rc } else { x * stride + y }] = lvl;

        // AC coefficients, reverse scan order.
        for i in (1..eob as usize).rev() {
            let (xi, yi, rci) = coef_xyrc(tx, tx_class, i, shift, shift2, mask);
            let lpos = if tx_class == TX_CLASS_2D { rci } else { xi * stride + yi };
            let (lo_ctx, mag) = get_lo_ctx(&levels[lpos..], tx_class, xi, yi, stride, offsets);
            let yy = if tx_class == TX_CLASS_2D { yi | xi } else { yi };
            let t0 = self.msac.symbol_adapt(&mut self.cdf.coeff_base[tctx][c][lo_ctx], 3);
            let (tok_i, lvl_i) = if t0 == 3 {
                let m = mag & 63;
                let base = if yy > (tx_class == TX_CLASS_2D) as usize { 14 } else { 7 };
                let hctx = base + if m > 12 { 6 } else { ((m + 1) >> 1) as usize };
                let t = self.msac.decode_hi_tok(&mut self.cdf.coeff_br[br][c][hctx]) as i32;
                (t, (t + (3 << 6)) as u8)
            } else {
                (t0 as i32, (t0 as i32 * 0x41) as u8)
            };
            cf[rci] = tok_i;
            levels[lpos] = lvl_i;
        }

        // DC coefficient.
        let (dc_ctx, mut dc_mag) = if tx_class == TX_CLASS_2D {
            (0usize, 0u32)
        } else {
            get_lo_ctx(&levels[0..], tx_class, 0, 0, stride, offsets)
        };
        let dt = self.msac.symbol_adapt(&mut self.cdf.coeff_base[tctx][c][dc_ctx], 3);
        cf[0] = if dt == 3 {
            if tx_class == TX_CLASS_2D {
                dc_mag = levels[1] as u32 + levels[stride] as u32 + levels[stride + 1] as u32;
            }
            dc_mag &= 63;
            let hctx = if dc_mag > 12 { 6 } else { ((dc_mag + 1) >> 1) as usize };
            self.msac.decode_hi_tok(&mut self.cdf.coeff_br[br][c][hctx]) as i32
        } else {
            dt as i32
        };
        cf
    }

    /// Sign + Golomb-residual + dequant pass over the raw tokens from
    /// `decode_coef_levels`, in forward scan order with the DC first. Mirrors
    /// dav1d `decode_coefs` L595-726, non-quant-matrix path (AVIF intra still
    /// images don't apply a qm here). dav1d walks a packed `tok<<11 | next_rc`
    /// chain that visits non-zero AC coefficients in increasing scan index; the
    /// forward scan walk below (skipping zero tokens) reproduces that exact MSAC
    /// read order without the packing. Returns the dequantized signed
    /// coefficients (by raster position) and the packed neighbour `res_ctx`
    /// (`min(cul_level,63) | dc_sign_level`).
    #[allow(clippy::too_many_arguments)]
    fn decode_coef_signs(
        &mut self,
        tx: usize,
        chroma: bool,
        txtp: u8,
        eob: i32,
        plane: usize,
        seg_id: usize,
        raw: &[i32],
        a: &[u8],
        l: &[u8],
    ) -> (Vec<i32>, u8) {
        let mut cf = vec![0i32; raw.len()];
        let tx_class = TX_TYPE_CLASS[txtp as usize];
        let tdim = TXFM_DIMENSIONS[tx];
        let slw = (tdim[0] as usize).min(3);
        let slh = (tdim[1] as usize).min(3);
        let tctx = tdim[5] as usize;
        let dq_tbl = self.dq[seg_id][plane];
        let dq_shift = (tctx as i32 - 2).max(0) as u32;
        // 8-bit asymmetric coefficient clamp: ~(~127 << 8) == 32767.
        let cf_max = (!(!127u32 << 8)) as i32;
        let (shift, shift2, mask) = match tx_class {
            TX_CLASS_2D | 1 => (slh + 2, 0usize, (4usize << slh) - 1),
            _ => (slw + 2, slh + 2, (4usize << slw) - 1), // TX_CLASS_V
        };

        // DC coefficient.
        let dc_tok = raw[0];
        let mut cul_level: u32;
        let dc_sign_level: u32;
        if dc_tok == 0 {
            cul_level = 0;
            dc_sign_level = 1 << 6; // 0x40 = "no DC coefficient"
        } else {
            let dc_sign_ctx = get_dc_sign_ctx(tx, a, l);
            let dc_sign = self
                .msac
                .bool_adapt(&mut self.cdf.dc_sign[chroma as usize][dc_sign_ctx]);
            // 0x80 positive, 0x00 negative.
            dc_sign_level = ((dc_sign as i32 - 1) & (2 << 6)) as u32;
            let dc_full = if dc_tok >= 15 {
                (self.msac.golomb() as i32 + 15) & 0xf_ffff
            } else {
                dc_tok
            };
            let prod = dq_tbl[0] as i64 * dc_full as i64;
            let dq = if dc_tok >= 15 {
                (((prod & 0xff_ffff) >> dq_shift) as i32).min(cf_max + dc_sign as i32)
            } else {
                (prod >> dq_shift) as i32
            };
            cul_level = dc_full as u32;
            cf[0] = if dc_sign != 0 { -dq } else { dq };
        }

        // AC coefficients, forward scan order over the non-zero tokens.
        let ac_dq = dq_tbl[1] as i64;
        for i in 1..=eob.max(0) as usize {
            let (_x, _y, rc) = coef_xyrc(tx, tx_class, i, shift, shift2, mask);
            let tok = raw[rc];
            if tok == 0 {
                continue;
            }
            let sign = self.msac.bool_equi();
            let tok_full = if tok >= 15 {
                (self.msac.golomb() as i32 + 15) & 0xf_ffff
            } else {
                tok
            };
            let prod = ac_dq * tok_full as i64;
            let dq = if tok >= 15 {
                (((prod & 0xff_ffff) >> dq_shift) as i32).min(cf_max + sign as i32)
            } else {
                (prod >> dq_shift) as i32
            };
            cul_level += tok_full as u32;
            cf[rc] = if sign != 0 { -dq } else { dq };
        }

        let res_ctx = (cul_level.min(63) | dc_sign_level) as u8;
        (cf, res_ctx)
    }

    /// Decode one transform block end-to-end: txb header (skip + tx_type) →, if
    /// not skipped, eob → levels → signs/dequant. Returns the dequantized signed
    /// coefficients (for reconstruction, layer 8) and the neighbour `res_ctx`
    /// the caller writes back across the tx's above/left context span. Mirrors a
    /// single `decode_coefs` invocation in dav1d's `read_coef_blocks` walk.
    /// `a_off`/`l_off` are the neighbour-context offsets (absolute column above,
    /// within-superblock row left). Segment 0 only for now (fixture has
    /// segmentation off, matching `decode_block`).
    #[allow(clippy::too_many_arguments)]
    fn decode_tx_block(
        &mut self,
        tx: usize,
        bs: u8,
        plane: usize,
        y_mode: u8,
        y_angle: u8,
        uv_mode: u8,
        a_off: usize,
        l_off: usize,
    ) -> (Vec<i32>, u8, u8, i32) {
        let chroma = plane != 0;
        let lossless = self.lossless[0];
        // Snapshot the dc-sign neighbour bytes before this block overwrites
        // them; `decode_coef_signs` needs the pre-update context (same bytes
        // `get_skip_ctx` reads inside `decode_txb_header`).
        let tdim = TXFM_DIMENSIONS[tx];
        let wa = 1usize << tdim[0];
        let wl = 1usize << tdim[1];
        let mut a_ctx = [0u8; 16];
        let mut l_ctx = [0u8; 16];
        {
            let (above, left): (&[u8], &[u8]) = if chroma {
                (
                    &self.above_ccoef[plane - 1][a_off..],
                    &self.left_ccoef[plane - 1][l_off..],
                )
            } else {
                (&self.above_lcoef[a_off..], &self.left_lcoef[l_off..])
            };
            let na = wa.min(above.len());
            let nl = wl.min(left.len());
            a_ctx[..na].copy_from_slice(&above[..na]);
            l_ctx[..nl].copy_from_slice(&left[..nl]);
        }
        let (all_skip, txtp) = self.decode_txb_header(
            tx,
            bs,
            plane,
            y_mode,
            y_angle,
            uv_mode,
            lossless,
            self.reduced_tx_set,
            a_off,
            l_off,
        );
        if all_skip {
            return (Vec::new(), 0x40, txtp, -1);
        }
        let eob = self.decode_eob(tx, chroma, txtp);
        let raw = self.decode_coef_levels(tx, chroma, txtp, eob);
        let (cf, res_ctx) =
            self.decode_coef_signs(tx, chroma, txtp, eob, plane, 0, &raw, &a_ctx, &l_ctx);
        (cf, res_ctx, txtp, eob)
    }

    /// CfL luma AC for a chroma block at chroma-pixel `(px,py)` size `(bw,bh)`:
    /// subsample the reconstructed luma footprint (2×2 average for I420, scaled)
    /// then subtract the block mean. dav1d `cfl_ac_c`. Common (mi-aligned) case;
    /// no edge padding.
    fn cfl_ac(&self, px: usize, py: usize, bw: usize, bh: usize) -> Vec<i32> {
        let ss_h = self.subsampling_x as usize;
        let ss_v = self.subsampling_y as usize;
        let lpw = self.plane_w[0];
        let luma = &self.planes[0];
        let (lx0, ly0) = (px << ss_h, py << ss_v);
        let shift = 1 + (ss_v == 0) as i32 + (ss_h == 0) as i32;
        let mut ac = vec![0i32; bw * bh];
        for y in 0..bh {
            let ly = ly0 + (y << ss_v);
            for x in 0..bw {
                let base = ly * lpw + lx0 + (x << ss_h);
                let mut s = luma[base] as i32;
                if ss_h == 1 {
                    s += luma[base + 1] as i32;
                }
                if ss_v == 1 {
                    s += luma[base + lpw] as i32;
                    if ss_h == 1 {
                        s += luma[base + lpw + 1] as i32;
                    }
                }
                ac[y * bw + x] = s << shift;
            }
        }
        let log2sz = bw.trailing_zeros() + bh.trailing_zeros();
        let mut sum: i64 = (1i64 << log2sz) >> 1;
        for &a in &ac {
            sum += a as i64;
        }
        let dc = (sum >> log2sz) as i32;
        for a in &mut ac {
            *a -= dc;
        }
        ac
    }

    /// Reconstruct one transform block into a pixel plane: intra-predict from the
    /// already-reconstructed top/left neighbours, inverse-transform the residual,
    /// add and clip. Covers DC/V/H/Paeth/Smooth, filter-intra (luma), CfL (chroma)
    /// and the directional Z1/Z2/Z3 modes (resolved from `angle_delta` via the
    /// `prepare_intra_edges` Z-path; topright/bottomleft availability is the one
    /// remaining gap — currently repeat-last). `(px, py)`/`(bw, bh)` in plane pixels.
    #[allow(clippy::too_many_arguments)]
    fn reconstruct_tx(
        &mut self,
        plane: usize,
        px: usize,
        py: usize,
        bw: usize,
        bh: usize,
        mode: u8,
        filt_idx: usize,
        cfl_alpha: i32,
        cf: &mut [i32],
        tx: usize,
        txtp: u8,
        eob: i32,
        angle_delta: i32,
        is_sm: bool,
    ) {
        let pw = self.plane_w[plane];
        let ph = self.plane_h[plane];
        let have_top = py > 0;
        let have_left = px > 0;
        // Assemble top/left/topleft edges with dav1d's availability fills
        // (`prepare_intra_edges`): extend past the frame edge with the last
        // in-frame sample; substitute the neighbouring row/col or 127/129/128
        // when an edge is unavailable.
        let buf = &self.planes[plane];
        let mut top = vec![0i32; bw];
        let mut left = vec![0i32; bh];
        if have_top {
            let avail = bw.min(pw - px);
            for (i, t) in top.iter_mut().enumerate() {
                *t = buf[(py - 1) * pw + px + i.min(avail - 1)] as i32;
            }
        } else {
            let fill = if have_left { buf[py * pw + px - 1] as i32 } else { 127 };
            top.fill(fill);
        }
        if have_left {
            let avail = bh.min(ph - py);
            for (i, l) in left.iter_mut().enumerate() {
                *l = buf[(py + i.min(avail - 1)) * pw + px - 1] as i32;
            }
        } else {
            let fill = if have_top { buf[(py - 1) * pw + px] as i32 } else { 129 };
            left.fill(fill);
        }
        let topleft = if have_left {
            if have_top {
                buf[(py - 1) * pw + px - 1] as i32
            } else {
                buf[py * pw + px - 1] as i32
            }
        } else if have_top {
            buf[(py - 1) * pw + px] as i32
        } else {
            128
        };

        // Luma filter-intra, chroma CfL, or the standard / directional predictors.
        let pred = if plane == 0 && mode == predict::FILTER_PRED {
            predict::filter(bw, bh, &top, &left, topleft, filt_idx)
        } else if plane != 0 && mode == predict::CFL_PRED {
            let dc = predict::dc_value(bw, bh, &top, &left, have_top, have_left);
            let ac = self.cfl_ac(px, py, bw, bh);
            predict::cfl_apply(dc, &ac, cfl_alpha)
        } else {
            // Resolve directional modes (`VERT_PRED..=VERT_LEFT_PRED`) into Z1/Z2/Z3
            // or fall back to VERT/HOR, mirroring `dav1d_prepare_intra_edges`.
            let mut m = mode;
            let mut z = 0u8; // 0 = none, 1 = Z1, 2 = Z2, 3 = Z3
            let mut z_angle = 0i32;
            if (mode::VERT_PRED..=mode::VERT_LEFT_PRED).contains(&mode) {
                let angle = AV1_MODE_TO_ANGLE_MAP[(mode - mode::VERT_PRED) as usize] + 3 * angle_delta;
                if angle <= 90 {
                    if angle < 90 && have_top {
                        z = 1;
                        z_angle = angle;
                    } else {
                        m = mode::VERT_PRED;
                    }
                } else if angle < 180 {
                    z = 2;
                    z_angle = angle;
                } else if angle > 180 && have_left {
                    z = 3;
                    z_angle = angle;
                } else {
                    m = mode::HOR_PRED;
                }
            }
            if z != 0 {
                // Unified directional edge buffer (predict.rs convention): corner at
                // `corner`, top+topright at `corner+1+i`, left+bottomleft at
                // `corner-1-i`. Topright/bottomleft are treated as unavailable here
                // (origin & the common case) → repeat the last in-block sample, as
                // dav1d does when the EDGE_*_HAS_* flag is absent. (Threading the real
                // availability from `decode_sb` is the next refinement.)
                let span = bw + bh;
                let corner = span;
                let mut edge = vec![0i32; 2 * span + 1];
                for i in 0..span {
                    edge[corner + 1 + i] = if i < bw { top[i] } else { top[bw - 1] };
                    edge[corner - 1 - i] = if i < bh { left[i] } else { left[bh - 1] };
                }
                // Z2 corner gets the 3-tap intra-edge filter for tw+th >= 6.
                let mut corner_v = topleft;
                if z == 2 && (bw >> 2) + (bh >> 2) >= 6 && self.intra_edge_filter {
                    corner_v = ((left[0] + top[0]) * 5 + topleft * 6 + 8) >> 4;
                }
                edge[corner] = corner_v;
                let angle_full =
                    z_angle | (is_sm as i32) << 9 | (self.intra_edge_filter as i32) << 10;
                match z {
                    1 => predict::z1(bw, bh, angle_full, &edge, corner),
                    2 => predict::z2(bw, bh, angle_full, &edge, corner, pw - px, ph - py),
                    _ => predict::z3(bw, bh, angle_full, &edge, corner),
                }
            } else {
                predict::predict(m, have_top, have_left, bw, bh, &top, &left, topleft)
            }
        };

        // Residual (skipped block → none).
        let residual = if eob >= 0 {
            if txtp == txtp::WHT_WHT {
                itx::inv_wht4x4_residual(cf)
            } else {
                itx::inv_txfm_residual(cf, tx, txtp, eob)
            }
        } else {
            Vec::new()
        };
        let buf = &mut self.planes[plane];
        for yy in 0..bh {
            for xx in 0..bw {
                let r = residual.get(yy * bw + xx).copied().unwrap_or(0);
                buf[(py + yy) * pw + px + xx] = (pred[yy * bw + xx] + r).clamp(0, 255) as u8;
            }
        }
    }

    /// A reconstructed plane (`0=Y, 1=U, 2=V`) as `(pixels, width, height)`.
    pub fn plane(&self, p: usize) -> (&[u8], usize, usize) {
        (&self.planes[p], self.plane_w[p], self.plane_h[p])
    }

    /// Walk a coding block's transform grid (luma first, then both chroma planes),
    /// decoding each transform block's residual and propagating its `res_ctx`
    /// across the spanned above/left coefficient-context bytes. Faithful to dav1d
    /// `read_coef_blocks` (intra path): the 16×16-mi outer tiling, then the
    /// `t_dim`-step inner loops, in exactly this order — the MSAC read sequence
    /// every subsequent symbol depends on. Reconstruction (predict + inverse
    /// transform from the returned `cf`) is layer 8.
    #[allow(clippy::too_many_arguments)]
    fn decode_residuals(
        &mut self,
        mi_row: u32,
        mi_col: u32,
        bs: u8,
        tx: usize,
        uvtx: usize,
        y_mode: u8,
        y_angle: u8,
        uv_mode: u8,
        cfl_alpha: [i32; 2],
        has_chroma: bool,
        y_angle_delta: i32,
        uv_angle_delta: i32,
        is_sm_y: bool,
        is_sm_uv: bool,
    ) {
        let dim = BLOCK_DIMENSIONS[bs as usize];
        let (bw4, bh4) = (dim[0] as i32, dim[1] as i32);
        let acol = mi_col as usize;
        let by4 = (mi_row & 31) as usize;
        let ss_h = self.subsampling_x as i32;
        let ss_v = self.subsampling_y as i32;
        let mic = self.geom.mi_cols as i32;
        let mir = self.geom.mi_rows as i32;
        let w4 = bw4.min(mic - mi_col as i32);
        let h4 = bh4.min(mir - mi_row as i32);
        let cw4 = (w4 + ss_h) >> ss_h;
        let ch4 = (h4 + ss_v) >> ss_v;
        let (tw, th) = (1i32 << TXFM_DIMENSIONS[tx][0], 1i32 << TXFM_DIMENSIONS[tx][1]);
        let (uvw, uvh) = (1i32 << TXFM_DIMENSIONS[uvtx][0], 1i32 << TXFM_DIMENSIONS[uvtx][1]);
        let cacol = acol >> ss_h;
        let cby4 = by4 >> ss_v;

        let mut init_y = 0i32;
        while init_y < h4 {
            let sub_h4 = h4.min(16 + init_y);
            let mut init_x = 0i32;
            while init_x < w4 {
                let sub_w4 = w4.min(init_x + 16);
                // Luma transform blocks.
                let mut y = init_y;
                while y < sub_h4 {
                    let mut x = init_x;
                    while x < sub_w4 {
                        let a_off = acol + x as usize;
                        let l_off = by4 + y as usize;
                        let (mut cf, res_ctx, txtp, eob) =
                            self.decode_tx_block(tx, bs, 0, y_mode, y_angle, uv_mode, a_off, l_off);
                        let px = (mi_col as usize + x as usize) * 4;
                        let py = (mi_row as usize + y as usize) * 4;
                        self.reconstruct_tx(
                            0,
                            px,
                            py,
                            (tw * 4) as usize,
                            (th * 4) as usize,
                            y_mode,
                            y_angle as usize,
                            0,
                            &mut cf,
                            tx,
                            txtp,
                            eob,
                            y_angle_delta,
                            is_sm_y,
                        );
                        let cw = tw.min(mic - (mi_col as i32 + x)).max(0) as usize;
                        let cht = th.min(mir - (mi_row as i32 + y)).max(0) as usize;
                        for i in 0..cw {
                            if a_off + i < self.above_lcoef.len() {
                                self.above_lcoef[a_off + i] = res_ctx;
                            }
                        }
                        for i in 0..cht {
                            if l_off + i < 32 {
                                self.left_lcoef[l_off + i] = res_ctx;
                            }
                        }
                        x += tw;
                    }
                    y += th;
                }
                if has_chroma {
                    let sub_ch4 = ch4.min((init_y + 16) >> ss_v);
                    let sub_cw4 = cw4.min((init_x + 16) >> ss_h);
                    #[allow(clippy::needless_range_loop)]
                    for pl in 0..2usize {
                        let mut y = init_y >> ss_v;
                        while y < sub_ch4 {
                            let mut x = init_x >> ss_h;
                            while x < sub_cw4 {
                                let a_off = cacol + x as usize;
                                let l_off = cby4 + y as usize;
                                let (mut cf, res_ctx, txtp, eob) = self.decode_tx_block(
                                    uvtx,
                                    bs,
                                    1 + pl,
                                    y_mode,
                                    y_angle,
                                    uv_mode,
                                    a_off,
                                    l_off,
                                );
                                let cpx = ((mi_col as usize >> ss_h) + x as usize) * 4;
                                let cpy = ((mi_row as usize >> ss_v) + y as usize) * 4;
                                self.reconstruct_tx(
                                    1 + pl,
                                    cpx,
                                    cpy,
                                    (uvw * 4) as usize,
                                    (uvh * 4) as usize,
                                    uv_mode,
                                    0,
                                    cfl_alpha[pl],
                                    &mut cf,
                                    uvtx,
                                    txtp,
                                    eob,
                                    uv_angle_delta,
                                    is_sm_uv,
                                );
                                // Luma-space position of this chroma block for the edge clamp.
                                let lx = mi_col as i32 + (x << ss_h);
                                let ly = mi_row as i32 + (y << ss_v);
                                let ctw = uvw.min((mic - lx + ss_h) >> ss_h).max(0) as usize;
                                let cth = uvh.min((mir - ly + ss_v) >> ss_v).max(0) as usize;
                                for i in 0..ctw {
                                    if a_off + i < self.above_ccoef[pl].len() {
                                        self.above_ccoef[pl][a_off + i] = res_ctx;
                                    }
                                }
                                for i in 0..cth {
                                    if l_off + i < 32 {
                                        self.left_ccoef[pl][l_off + i] = res_ctx;
                                    }
                                }
                                x += uvw;
                            }
                            y += uvh;
                        }
                    }
                }
                init_x += 16;
            }
            init_y += 16;
        }
    }

    fn decode_sb(&mut self, mi_row: u32, mi_col: u32, bl: u8) {
        if mi_row >= self.geom.mi_rows || mi_col >= self.geom.mi_cols {
            return;
        }
        let hbs = 16u32 >> bl;
        let has_rows = mi_row + hbs < self.geom.mi_rows;
        let has_cols = mi_col + hbs < self.geom.mi_cols;
        let case = match (has_rows, has_cols) {
            (true, true) => PartCase::Both,
            (true, false) => PartCase::RowsOnly,
            (false, true) => PartCase::ColsOnly,
            (false, false) => PartCase::Neither,
        };
        let partition = match case {
            PartCase::Neither => part::SPLIT,
            _ => self.read_partition(bl, mi_row, mi_col, case),
        };
        let sub = BLOCK_SIZES[bl as usize][partition as usize];
        let (r, c) = (mi_row, mi_col);
        match partition {
            part::NONE => self.decode_block(r, c, sub[0]),
            part::H => {
                self.decode_block(r, c, sub[0]);
                if has_rows {
                    self.decode_block(r + hbs, c, sub[0]);
                }
            }
            part::V => {
                self.decode_block(r, c, sub[0]);
                if has_cols {
                    self.decode_block(r, c + hbs, sub[0]);
                }
            }
            part::SPLIT => {
                if bl == BL_8X8 {
                    self.decode_block(r, c, bs::BS_4X4);
                    if c + 1 < self.geom.mi_cols {
                        self.decode_block(r, c + 1, bs::BS_4X4);
                    }
                    if r + 1 < self.geom.mi_rows {
                        self.decode_block(r + 1, c, bs::BS_4X4);
                    }
                    if r + 1 < self.geom.mi_rows && c + 1 < self.geom.mi_cols {
                        self.decode_block(r + 1, c + 1, bs::BS_4X4);
                    }
                } else {
                    self.decode_sb(r, c, bl + 1);
                    self.decode_sb(r, c + hbs, bl + 1);
                    self.decode_sb(r + hbs, c, bl + 1);
                    self.decode_sb(r + hbs, c + hbs, bl + 1);
                }
            }
            part::T_TOP => {
                self.decode_block(r, c, sub[0]);
                self.decode_block(r, c + hbs, sub[0]);
                self.decode_block(r + hbs, c, sub[1]);
            }
            part::T_BOTTOM => {
                self.decode_block(r, c, sub[0]);
                self.decode_block(r + hbs, c, sub[1]);
                self.decode_block(r + hbs, c + hbs, sub[1]);
            }
            part::T_LEFT => {
                self.decode_block(r, c, sub[0]);
                self.decode_block(r + hbs, c, sub[0]);
                self.decode_block(r, c + hbs, sub[1]);
            }
            part::T_RIGHT => {
                self.decode_block(r, c, sub[0]);
                self.decode_block(r, c + hbs, sub[1]);
                self.decode_block(r + hbs, c + hbs, sub[1]);
            }
            part::H4 => {
                let q = hbs >> 1;
                self.decode_block(r, c, sub[0]);
                self.decode_block(r + q, c, sub[0]);
                self.decode_block(r + 2 * q, c, sub[0]);
                if r + 3 * q < self.geom.mi_rows {
                    self.decode_block(r + 3 * q, c, sub[0]);
                }
            }
            part::V4 => {
                let q = hbs >> 1;
                self.decode_block(r, c, sub[0]);
                self.decode_block(r, c + q, sub[0]);
                self.decode_block(r, c + 2 * q, sub[0]);
                if c + 3 * q < self.geom.mi_cols {
                    self.decode_block(r, c + 3 * q, sub[0]);
                }
            }
            _ => {}
        }
        // Update the partition context (skipped for pure splits above 8×8).
        if partition != part::SPLIT || bl == BL_8X8 {
            self.update_partition_ctx(bl, mi_row, mi_col, partition);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode with a mock partition reader and assert every mi cell of the frame
    /// is covered by exactly one leaf (tiles the frame, no gaps, no overlap).
    fn assert_tiles_exactly(
        mi_cols: u32,
        mi_rows: u32,
        top_bl: u8,
        sb4: u32,
        read: &mut dyn FnMut(u8, u32, u32, PartCase) -> u8,
    ) {
        let geom = TileGeom { mi_cols, mi_rows };
        let mut cover = vec![0u32; (mi_cols * mi_rows) as usize];
        let mut leaf = |r: u32, c: u32, b: u8| {
            let w = BLOCK_DIMENSIONS[b as usize][0] as u32;
            let h = BLOCK_DIMENSIONS[b as usize][1] as u32;
            for y in r..(r + h).min(mi_rows) {
                for x in c..(c + w).min(mi_cols) {
                    cover[(y * mi_cols + x) as usize] += 1;
                }
            }
        };
        // Superblock grid walk.
        let mut row = 0;
        while row < mi_rows {
            let mut col = 0;
            while col < mi_cols {
                decode_partition(&geom, row, col, top_bl, read, &mut leaf);
                col += sb4;
            }
            row += sb4;
        }
        for (i, &n) in cover.iter().enumerate() {
            assert_eq!(n, 1, "cell {} covered {n} times (want 1)", i);
        }
    }

    #[test]
    fn partition_none_covers_single_block() {
        // 64×64 frame (16×16 mi), one 64×64 SB, NONE everywhere → 1 leaf.
        assert_tiles_exactly(16, 16, BL_64X64, 16, &mut |_, _, _, _| part::NONE);
    }

    #[test]
    fn partition_split_then_none_tiles_quadrants() {
        // SPLIT at 64×64 → four 32×32 (NONE). Exact coverage.
        assert_tiles_exactly(16, 16, BL_64X64, 16, &mut |bl, _, _, _| {
            if bl == BL_64X64 {
                part::SPLIT
            } else {
                part::NONE
            }
        });
    }

    #[test]
    fn partition_h_and_v_tile_halves() {
        assert_tiles_exactly(16, 16, BL_64X64, 16, &mut |_, _, _, _| part::H);
        assert_tiles_exactly(16, 16, BL_64X64, 16, &mut |_, _, _, _| part::V);
    }

    #[test]
    fn partition_t_shapes_tile_exactly() {
        for p in [part::T_TOP, part::T_BOTTOM, part::T_LEFT, part::T_RIGHT] {
            assert_tiles_exactly(16, 16, BL_64X64, 16, &mut move |_, _, _, _| p);
        }
    }

    #[test]
    fn partition_h4_v4_tile_exactly() {
        assert_tiles_exactly(16, 16, BL_64X64, 16, &mut |_, _, _, _| part::H4);
        assert_tiles_exactly(16, 16, BL_64X64, 16, &mut |_, _, _, _| part::V4);
    }

    #[test]
    fn edge_partitions_force_split_and_tile() {
        // 48×48 frame (12×12 mi): the SB right/bottom edges fall mid-block, so
        // the recursion must hit ColsOnly/RowsOnly/Neither and still tile exactly.
        // Reader: SPLIT for full nodes, and the edge binary defaults to SPLIT
        // (valid for both ColsOnly and RowsOnly), bottoming out at NONE leaves.
        assert_tiles_exactly(12, 12, BL_64X64, 16, &mut |bl, _, _, case| match case {
            PartCase::Both if bl == BL_8X8 => part::NONE,
            PartCase::Both => part::SPLIT,
            _ => part::SPLIT,
        });
    }

    #[test]
    fn the_32x32_fixture_tiles() {
        // The av1test fixture: 32×32 px = 8×8 mi, one 64×64 SB (mostly off-frame).
        // NONE at the in-frame 32×32 quadrant; the SB forces SPLIT to reach it.
        assert_tiles_exactly(8, 8, BL_64X64, 16, &mut |bl, _, _, _| {
            if bl == BL_32X32 {
                part::NONE
            } else {
                part::SPLIT
            }
        });
    }

    #[test]
    fn gather_probs_match_formula() {
        // Synthetic decreasing inverse-CDF; hand-computed against the dav1d
        // index combinations validates the (error-prone) edge-probability math.
        let c = [100u16, 90, 80, 70, 60, 50, 40, 30, 20, 0, 0, 0, 0, 0, 0, 0];
        // top (bl≠128): (c[1]-c[4]) + c[5] + (c[8]-c[7]) = 30 + 50 + (20-30=-10) = 70.
        assert_eq!(gather_top_partition_prob(&c, BL_64X64), 70);
        // left (bl≠128): (c[0]-c[1]) + (c[2]-c[6]) + (c[7]-c[8]) = 10 + 40 + 10 = 60.
        assert_eq!(gather_left_partition_prob(&c, BL_64X64), 60);
        // 128×128 drops the H4/V4 term.
        assert_eq!(gather_top_partition_prob(&c, BL_128X128), 30 + 50);
        assert_eq!(gather_left_partition_prob(&c, BL_128X128), 10 + 40);
    }

    #[test]
    fn partition_ctx_combines_above_left() {
        let mut above = vec![0u8; 8];
        let mut left = [0u8; 16];
        // Bit (4-bl) of the context byte selects "did this level split here".
        above[0] = 1 << (4 - BL_64X64); // above split bit set at col 0
        left[0] = 1 << (4 - BL_64X64); // left split bit set at row 0
        assert_eq!(get_partition_ctx(&above, &left, BL_64X64, 0, 0), 3); // a=1, l=1 → 1+2
        above[0] = 0;
        assert_eq!(get_partition_ctx(&above, &left, BL_64X64, 0, 0), 2); // a=0, l=1
        left[0] = 0;
        assert_eq!(get_partition_ctx(&above, &left, BL_64X64, 0, 0), 0);
    }

    #[test]
    fn coef_cdf_selected_by_qcat() {
        assert_eq!(
            [
                qcat_for(0),
                qcat_for(20),
                qcat_for(21),
                qcat_for(60),
                qcat_for(61),
                qcat_for(120),
                qcat_for(121),
                qcat_for(255),
            ],
            [0, 0, 1, 1, 2, 2, 3, 3]
        );
        // Cdf::new copies the coefficient tables from the matching qcat slice.
        for q in 0..4 {
            let c = Cdf::new(q);
            assert_eq!(c.coef_skip, cdf::COEF_SKIP_Q[q]);
            assert_eq!(c.coeff_base, cdf::COEFF_BASE_Q[q]);
            assert_eq!(c.dc_sign, cdf::DC_SIGN_Q[q]);
        }
    }

    #[test]
    fn skip_ctx_matches_dav1d_formula() {
        let z = [0x40u8; 16];
        // tx == block size (16×16) → context 0.
        assert_eq!(get_skip_ctx(bs::BS_16X16, 2, false, true, true, &z, &z), 0);
        // larger block (32×32) with a 16×16 tx, empty neighbours → SKIP_CTX[0][0]=1.
        assert_eq!(get_skip_ctx(bs::BS_32X32, 2, false, true, true, &z, &z), 1);
        // a non-zero above level folds in (0x41 & 0x3F = 1) → SKIP_CTX[1][0]=2.
        let mut a = [0x40u8; 16];
        a[0] = 0x41;
        assert_eq!(get_skip_ctx(bs::BS_32X32, 2, false, true, true, &a, &z), 2);
    }

    #[test]
    fn tx_type_tables_well_formed() {
        assert!(TX_TYPES_PER_SET.iter().all(|&t| t < 16));
        assert!(TXTP_FROM_UVMODE.iter().all(|&t| t < 16));
        assert_eq!(TXFM_DIMENSIONS.len(), 19);
        // ctx/min columns: TX_64X64 has ctx 4 / min 4; TX_4X4 both 0.
        assert_eq!(TXFM_DIMENSIONS[4][5], 4);
        assert_eq!(TXFM_DIMENSIONS[0][4], 0);
    }

    #[test]
    fn txb_header_runs_without_panic() {
        // Exercises get_skip_ctx + txb_skip + tx_type on a fresh tile (not yet
        // wired into the tx-grid walk, so the bytes are partition data — this
        // only guards range + no-panic; sync validation comes with 7c/7d).
        let bytes = vec![0x12u8, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];
        let seq = super::super::SequenceHeader {
            subsampling_x: 1,
            subsampling_y: 1,
            ..Default::default()
        };
        let fh = super::super::FrameHeader {
            base_q_idx: 100,
            ..Default::default()
        };
        let mut tile = Av1Tile::new(&bytes, 16, 16, &seq, &fh);
        let (_skip, ty) =
            tile.decode_txb_header(0, bs::BS_4X4, 0, mode::DC_PRED, 0, mode::DC_PRED, false, false, 0, 0);
        assert!(ty <= 16, "txtp out of range: {ty}");
    }

    #[test]
    fn lo_ctx_matches_dav1d() {
        // 2D at (0,0), stride 8, only the right neighbour set (level 100).
        // mag = lv[1]+lv[8]+lv[9] = 100 (hi_mag), +lv[2]+lv[16] = 100;
        // offset = LO_CTX_OFFSETS[0][0][0] = 0; ctx = 0 + ((100+64)>>7) = 1.
        let mut lv = [0u8; 64];
        lv[1] = 100;
        assert_eq!(get_lo_ctx(&lv, TX_CLASS_2D, 0, 0, 8, &LO_CTX_OFFSETS[0]), (1, 100));
        // mag > 512 saturates the magnitude term to 4; offset[1][2] = 6 → ctx 10.
        let mut lv2 = [0u8; 64];
        lv2[1] = 255;
        lv2[8] = 255;
        lv2[9] = 200;
        let (ctx2, hi2) = get_lo_ctx(&lv2, TX_CLASS_2D, 2, 1, 8, &LO_CTX_OFFSETS[0]);
        assert_eq!((ctx2, hi2), (10, 710));
    }

    #[test]
    fn eob_decode_runs_in_range() {
        let bytes = vec![0x5au8, 0xa5, 0x3c, 0xc3, 0x0f, 0xf0, 0x77, 0x88, 0x12, 0x34];
        let seq = super::super::SequenceHeader {
            subsampling_x: 1,
            subsampling_y: 1,
            ..Default::default()
        };
        let fh = super::super::FrameHeader {
            base_q_idx: 80,
            ..Default::default()
        };
        let mut tile = Av1Tile::new(&bytes, 16, 16, &seq, &fh);
        // A 4×4 tx has ≤16 coefficients; the eob must be a sane position.
        for &tx in &[0usize /*TX_4X4*/, 1 /*TX_8X8*/, 4 /*TX_64X64*/] {
            let eob = tile.decode_eob(tx, false, txtp::DCT_DCT);
            assert!((0..=1024).contains(&eob), "eob out of range: {eob} (tx={tx})");
        }
    }

    #[test]
    fn coef_levels_loop_runs() {
        let bytes = vec![0xa1u8, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c];
        let seq = super::super::SequenceHeader {
            subsampling_x: 1,
            subsampling_y: 1,
            ..Default::default()
        };
        let fh = super::super::FrameHeader {
            base_q_idx: 90,
            ..Default::default()
        };
        let mut tile = Av1Tile::new(&bytes, 16, 16, &seq, &fh);
        // 8×8 2D tx (TX_8X8=1, DCT_DCT) with a moderate eob → tokens in [0,15].
        let cf = tile.decode_coef_levels(1, false, txtp::DCT_DCT, 5);
        assert_eq!(cf.len(), 64);
        assert!(cf.iter().all(|&t| (0..=15).contains(&t)), "token out of range");
        // dc-only path (eob = 0).
        let cf2 = tile.decode_coef_levels(0, false, txtp::DCT_DCT, 0);
        assert_eq!(cf2.len(), 16);
        assert!(cf2.iter().all(|&t| (0..=15).contains(&t)));
    }

    #[test]
    fn coef_signs_dequant_runs() {
        let bytes = vec![0x3c, 0xd2, 0x91, 0x4e, 0xa7, 0x60, 0xf1, 0x28, 0xbb, 0x05, 0x9d, 0x46];
        let seq = super::super::SequenceHeader {
            subsampling_x: 1,
            subsampling_y: 1,
            ..Default::default()
        };
        let fh = super::super::FrameHeader {
            base_q_idx: 110,
            ..Default::default()
        };
        let mut tile = Av1Tile::new(&bytes, 16, 16, &seq, &fh);
        // base_q_idx 110 → qcat 2; dq[0][luma] uses dav1d_dq_tbl[110].
        assert_eq!(tile.dq[0][0], [DQ_TBL_8BIT[110][0], DQ_TBL_8BIT[110][1]]);
        // Decode 8×8 levels, then signs + Golomb + dequant with default (0x40)
        // "no neighbour" sign context. The result must be the dequantized
        // signed coefficients; res_ctx packs cul_level (0-63) | dc_sign bits.
        let raw = tile.decode_coef_levels(1, false, txtp::DCT_DCT, 6);
        let ctx = [0x40u8; 8];
        let (cf, res_ctx) = tile.decode_coef_signs(1, false, txtp::DCT_DCT, 6, 0, 0, &raw, &ctx, &ctx);
        assert_eq!(cf.len(), 64);
        // dc_sign_level occupies bits 6-7; cul_level the low 6 bits.
        assert!((res_ctx & 0x3f) <= 63);
        assert!(matches!(res_ctx & 0xc0, 0x00 | 0x40 | 0x80));
        // Every non-zero raw token yields a (possibly clamped) signed coef.
        for (rc, &t) in raw.iter().enumerate() {
            if t == 0 {
                assert_eq!(cf[rc], 0, "zero token must stay zero at rc={rc}");
            }
        }
        assert!(cf.iter().all(|&v| v.abs() <= 32767 + 1), "coef exceeds clamp");
    }

    #[test]
    fn tx_block_glue_runs() {
        let bytes = vec![0x71, 0x2c, 0x9e, 0x53, 0x80, 0xd6, 0x1b, 0xf4, 0x4a, 0xa9, 0x37, 0x62];
        let seq = super::super::SequenceHeader {
            subsampling_x: 1,
            subsampling_y: 1,
            ..Default::default()
        };
        let fh = super::super::FrameHeader {
            base_q_idx: 128,
            ..Default::default()
        };
        let mut tile = Av1Tile::new(&bytes, 16, 16, &seq, &fh);
        // One luma 8×8 tx block at the SB origin (a_off=l_off=0, DC_PRED).
        let (cf, res_ctx, _txtp, _eob) =
            tile.decode_tx_block(1, bs::BS_8X8, 0, mode::DC_PRED, 0, mode::DC_PRED, 0, 0);
        // Either skipped (empty cf, res_ctx=0x40) or decoded (64 coefs, valid ctx).
        if res_ctx == 0x40 && cf.is_empty() {
            // all-zero transform block — fine.
        } else {
            assert_eq!(cf.len(), 64);
            assert!((res_ctx & 0x3f) <= 63);
            assert!(matches!(res_ctx & 0xc0, 0x00 | 0x40 | 0x80));
        }
        // A chroma block (plane 1, uvtx) right after must also stay in sync.
        let (_cfu, ctxu, _t, _e) =
            tile.decode_tx_block(0, bs::BS_8X8, 1, mode::DC_PRED, 0, mode::DC_PRED, 0, 0);
        assert!(matches!(ctxu & 0xc0, 0x00 | 0x40 | 0x80));
    }

    #[test]
    fn tile_decodes_fixture_without_panic() {
        use super::super::{
            extract_av1_stream, parse_frame_header, parse_sequence_header, split_obus,
            OBU_FRAME, OBU_FRAME_HEADER, OBU_SEQUENCE_HEADER,
        };
        let avif = include_bytes!("../fixtures/av1test.avif");
        let stream = extract_av1_stream(avif).unwrap();
        let obus = split_obus(&stream).unwrap();
        let seq_obu = obus.iter().find(|o| o.kind == OBU_SEQUENCE_HEADER).unwrap();
        let seq = parse_sequence_header(seq_obu.data).unwrap();
        let frame = obus
            .iter()
            .find(|o| o.kind == OBU_FRAME || o.kind == OBU_FRAME_HEADER)
            .unwrap();
        let fh = parse_frame_header(&seq, frame.data).unwrap();
        let off = tile_data_offset(fh.header_bits);
        assert!(off < frame.data.len(), "tile offset {off} >= {}", frame.data.len());
        let mi_cols = 2 * ((fh.frame_width + 7) >> 3);
        let mi_rows = 2 * ((fh.frame_height + 7) >> 3);
        let tile_len = frame.data[off..].len();
        let mut tile = Av1Tile::new(&frame.data[off..], mi_cols, mi_rows, &seq, &fh);
        // Full intra coefficient decode is now wired end-to-end. A correctly
        // synchronised decode consumes ~all of the tile bytes (the msac reads
        // ahead into its window, so `pos` reaches `buf_len`) and leaves the
        // range renormalised in `[0x8000, 0xFFFF]`.
        tile.decode();
        let (pos, blen, rng) = (tile.msac.pos(), tile.msac.buf_len(), tile.msac.rng());
        eprintln!("[fixture] blocks={} msac pos={pos}/{blen} (tile_len={tile_len}) rng={rng:#06x}", tile.blocks_visited);
        assert!(tile.blocks_visited > 0, "no blocks visited");
        assert!((0x8000..=0xffff).contains(&rng), "msac rng out of range: {rng:#x}");
        assert!(pos >= blen.saturating_sub(8), "msac consumed only {pos}/{blen} bytes");
    }

    #[test]
    fn reconstructs_fixture_pixels() {
        use super::super::{
            extract_av1_stream, parse_frame_header, parse_sequence_header, split_obus, OBU_FRAME,
            OBU_FRAME_HEADER, OBU_SEQUENCE_HEADER,
        };
        let avif = include_bytes!("../fixtures/av1test.avif");
        let reference = include_bytes!("../fixtures/av1test_ref.yuv");
        let stream = extract_av1_stream(avif).unwrap();
        let obus = split_obus(&stream).unwrap();
        let seq = parse_sequence_header(
            obus.iter().find(|o| o.kind == OBU_SEQUENCE_HEADER).unwrap().data,
        )
        .unwrap();
        let frame = obus
            .iter()
            .find(|o| o.kind == OBU_FRAME || o.kind == OBU_FRAME_HEADER)
            .unwrap();
        let fh = parse_frame_header(&seq, frame.data).unwrap();
        let off = tile_data_offset(fh.header_bits);
        let mi_cols = 2 * ((fh.frame_width + 7) >> 3);
        let mi_rows = 2 * ((fh.frame_height + 7) >> 3);
        let mut tile = Av1Tile::new(&frame.data[off..], mi_cols, mi_rows, &seq, &fh);
        tile.decode();

        // Reference is I420 planar 32×32: Y(1024) + U(256) + V(256) = 1536 bytes.
        let mut refoff = 0usize;
        let mut maxdiff = [0i32; 3];
        for p in 0..3 {
            let (buf, pw, ph) = tile.plane(p);
            let n = pw * ph;
            for i in 0..n {
                let d = (buf[i] as i32 - reference[refoff + i] as i32).abs();
                maxdiff[p] = maxdiff[p].max(d);
            }
            refoff += n;
        }
        eprintln!(
            "[recon] maxdiff Y={} U={} V={}",
            maxdiff[0], maxdiff[1], maxdiff[2]
        );
        // First-pixels milestone: the DC_PRED single-block fixture must
        // reconstruct bit-exactly against dav1d's reference YUV.
        assert_eq!(maxdiff, [0, 0, 0], "pixel mismatch vs dav1d reference");
    }
}
