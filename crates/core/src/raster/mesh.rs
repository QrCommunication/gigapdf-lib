//! Mesh-shading decoder (ISO 32000-1 §8.7.4.5.5–8): turn the packed data stream of
//! a type 4–7 shading into a flat list of Gouraud [`MeshVertex`] triangles, ready
//! for the rasterizer to barycentric-fill.
//!
//! This module is **pure** — it never touches the PDF object graph. The caller
//! (the document) resolves the stream dictionary into a [`MeshParams`] and supplies
//! a `color` closure that maps the decoded per-vertex colour components to device
//! RGB (the closure owns the `/Function` evaluation and `/ColorSpace` conversion).
//! Free-form (4) and lattice (5) meshes emit their triangles directly; Coons (6)
//! and tensor (7) patches are tessellated into a `GRID × GRID` triangle mesh.
//!
//! Zero dependencies.

use super::render::MeshVertex;

/// Subdivision resolution for a Coons/tensor patch: each patch becomes a
/// `PATCH_GRID × PATCH_GRID` grid of quads (→ `2 · GRID²` triangles). Eight is a
/// good visual/perf trade-off for smooth bicubic surfaces at page resolution.
const PATCH_GRID: usize = 8;

/// Hard cap on emitted triangles, so a hostile or malformed stream (huge vertex
/// count / runaway patches) can't allocate without bound.
const MAX_TRIANGLES: usize = 4_000_000;

/// Resolved parameters of a mesh shading, extracted from its stream dictionary.
/// All colour handling is delegated to the caller's `color` closure; this struct
/// only describes the *geometry* packing.
#[derive(Debug, Clone)]
pub struct MeshParams {
    /// `/ShadingType`: 4 (free-form), 5 (lattice), 6 (Coons), 7 (tensor).
    pub shading_type: i64,
    /// `/BitsPerCoordinate` (1,2,4,8,12,16,24,32).
    pub bits_per_coord: u32,
    /// `/BitsPerComponent` (1,2,4,8,12,16).
    pub bits_per_comp: u32,
    /// `/BitsPerFlag` (2,4,8) — used by types 4, 6, 7. Ignored for type 5.
    pub bits_per_flag: u32,
    /// `/Decode`: `[xmin xmax ymin ymax c0min c0max …]`. Length must be at least
    /// `4 + 2 · n_color`.
    pub decode: Vec<f64>,
    /// Number of colour components packed per vertex: `1` when a `/Function` maps a
    /// single parametric value, otherwise the colour space's component count.
    pub n_color: usize,
    /// `/VerticesPerRow` (≥ 2) — type 5 only.
    pub vertices_per_row: usize,
}

/// A big-endian, MSB-first bit reader over the decoded stream bytes. Returns
/// `None` from `read` once the data is exhausted, which terminates decoding
/// cleanly (a truncated last record is simply dropped).
struct BitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    /// Total number of bits available.
    fn total_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// `true` once every bit has been consumed.
    fn at_end(&self) -> bool {
        self.bit >= self.total_bits()
    }

    /// Read `n` bits (`0 ≤ n ≤ 32`) MSB-first as an unsigned integer. `None` if
    /// fewer than `n` bits remain.
    fn read(&mut self, n: u32) -> Option<u32> {
        let n = n as usize;
        if n == 0 {
            return Some(0);
        }
        if self.bit + n > self.total_bits() {
            return None;
        }
        let mut value: u32 = 0;
        for _ in 0..n {
            let byte = self.data[self.bit >> 3];
            let shift = 7 - (self.bit & 7);
            let b = (byte >> shift) & 1;
            value = (value << 1) | b as u32;
            self.bit += 1;
        }
        Some(value)
    }

    /// Advance to the next byte boundary (discard the remaining bits of the current
    /// byte). Used before each Coons/tensor patch, per the spec's packing rule.
    fn align(&mut self) {
        let rem = self.bit & 7;
        if rem != 0 {
            self.bit += 8 - rem;
        }
    }
}

/// Decode a `value` of `bits` bits into the range `[dmin, dmax]`, the linear map
/// the spec applies to coordinates and colour components:
/// `dmin + value/(2^bits − 1) · (dmax − dmin)`.
fn decode_value(raw: u32, bits: u32, dmin: f64, dmax: f64) -> f64 {
    let max = if bits >= 32 {
        u32::MAX as f64
    } else {
        ((1u64 << bits) - 1) as f64
    };
    if max <= 0.0 {
        return dmin;
    }
    dmin + (raw as f64 / max) * (dmax - dmin)
}

/// A decoded vertex in shading space: position plus already-resolved RGB.
#[derive(Clone, Copy)]
struct Vert {
    x: f64,
    y: f64,
    color: [u8; 3],
}

/// Decode a mesh shading's stream `data` into a flat triangle list (multiple of 3
/// vertices). `color(&comps) -> rgb` resolves each vertex's colour. Returns an
/// empty vector on malformed input rather than erroring (the page still renders).
pub fn decode_mesh<F>(data: &[u8], params: &MeshParams, color: F) -> Vec<MeshVertex>
where
    F: Fn(&[f64]) -> [u8; 3],
{
    if params.decode.len() < 4 + 2 * params.n_color || params.bits_per_coord == 0 {
        return Vec::new();
    }
    let mut out: Vec<MeshVertex> = Vec::new();
    let mut reader = BitReader::new(data);
    match params.shading_type {
        4 => decode_free_form(&mut reader, params, &color, &mut out),
        5 => decode_lattice(&mut reader, params, &color, &mut out),
        6 => decode_patches(&mut reader, params, &color, &mut out, false),
        7 => decode_patches(&mut reader, params, &color, &mut out, true),
        _ => {}
    }
    out
}

/// Read one vertex's `(x, y, colour)` (no flag) from the bitstream. `None` at EOF.
fn read_vertex<F>(reader: &mut BitReader, p: &MeshParams, color: &F) -> Option<Vert>
where
    F: Fn(&[f64]) -> [u8; 3],
{
    let xr = reader.read(p.bits_per_coord)?;
    let yr = reader.read(p.bits_per_coord)?;
    let x = decode_value(xr, p.bits_per_coord, p.decode[0], p.decode[1]);
    let y = decode_value(yr, p.bits_per_coord, p.decode[2], p.decode[3]);
    let rgb = read_color(reader, p, color)?;
    Some(Vert { x, y, color: rgb })
}

/// Read one vertex's colour components from the bitstream and resolve them to RGB
/// via `color`. `None` at EOF.
fn read_color<F>(reader: &mut BitReader, p: &MeshParams, color: &F) -> Option<[u8; 3]>
where
    F: Fn(&[f64]) -> [u8; 3],
{
    let mut comps = [0.0f64; 8];
    let n = p.n_color.min(8);
    for (i, slot) in comps.iter_mut().enumerate().take(n) {
        let raw = reader.read(p.bits_per_comp)?;
        let dmin = p.decode[4 + 2 * i];
        let dmax = p.decode[5 + 2 * i];
        *slot = decode_value(raw, p.bits_per_comp, dmin, dmax);
    }
    Some(color(&comps[..n]))
}

/// Push a triangle (three decoded vertices) into the output list.
fn emit_triangle(out: &mut Vec<MeshVertex>, a: &Vert, b: &Vert, c: &Vert) {
    out.push(MeshVertex {
        x: a.x,
        y: a.y,
        color: a.color,
    });
    out.push(MeshVertex {
        x: b.x,
        y: b.y,
        color: b.color,
    });
    out.push(MeshVertex {
        x: c.x,
        y: c.y,
        color: c.color,
    });
}

/// Type 4 — free-form Gouraud-shaded triangles. Each vertex carries an edge flag:
/// `0` starts a new triangle (the next two `0`-flag vertices complete it); `1`/`2`
/// share an edge of the previous triangle, forming a strip/fan. Read continuously
/// (no byte alignment between vertices).
fn decode_free_form<F>(reader: &mut BitReader, p: &MeshParams, color: &F, out: &mut Vec<MeshVertex>)
where
    F: Fn(&[f64]) -> [u8; 3],
{
    // The two most-recently-emitted triangle vertices, kept to extend a strip/fan.
    let mut va: Option<Vert> = None;
    let mut vb: Option<Vert> = None;
    let mut vc: Option<Vert> = None;
    loop {
        if out.len() >= MAX_TRIANGLES * 3 {
            return;
        }
        let Some(flag) = reader.read(p.bits_per_flag) else {
            return;
        };
        let Some(v) = read_vertex(reader, p, color) else {
            return;
        };
        match flag {
            0 => {
                // Start a fresh triangle: this vertex plus the next two (which the
                // spec guarantees also carry flag 0). Read them now.
                let (Some(v2), Some(v3)) = (
                    reader
                        .read(p.bits_per_flag)
                        .and(read_vertex(reader, p, color)),
                    reader
                        .read(p.bits_per_flag)
                        .and(read_vertex(reader, p, color)),
                ) else {
                    return;
                };
                emit_triangle(out, &v, &v2, &v3);
                va = Some(v);
                vb = Some(v2);
                vc = Some(v3);
            }
            1 => {
                // Share the previous triangle's (vb, vc) edge: new triangle (vb, vc, v).
                if let (Some(b), Some(c)) = (vb, vc) {
                    emit_triangle(out, &b, &c, &v);
                    va = Some(b);
                    vb = Some(c);
                    vc = Some(v);
                } else {
                    return;
                }
            }
            2 => {
                // Share the previous triangle's (va, vc) edge: new triangle (va, vc, v).
                if let (Some(a), Some(c)) = (va, vc) {
                    emit_triangle(out, &a, &c, &v);
                    // va stays, vb becomes the shared vc, vc becomes v.
                    vb = Some(c);
                    vc = Some(v);
                } else {
                    return;
                }
            }
            _ => return, // invalid flag
        }
        if reader.at_end() {
            return;
        }
    }
}

/// Type 5 — lattice-form Gouraud mesh. No flags: a regular grid of
/// `VerticesPerRow` columns by however many rows the stream holds. Consecutive
/// rows are joined into quads, each split into two triangles. Read continuously.
fn decode_lattice<F>(reader: &mut BitReader, p: &MeshParams, color: &F, out: &mut Vec<MeshVertex>)
where
    F: Fn(&[f64]) -> [u8; 3],
{
    let vpr = p.vertices_per_row;
    if vpr < 2 {
        return;
    }
    let mut prev: Option<Vec<Vert>> = None;
    loop {
        // Read one full row; stop as soon as a row can't be completed.
        let mut row = Vec::with_capacity(vpr);
        for _ in 0..vpr {
            match read_vertex(reader, p, color) {
                Some(v) => row.push(v),
                None => return,
            }
        }
        if let Some(top) = &prev {
            for i in 0..(vpr - 1) {
                // Quad corners: top[i], top[i+1], row[i+1], row[i].
                emit_triangle(out, &top[i], &top[i + 1], &row[i + 1]);
                emit_triangle(out, &top[i], &row[i + 1], &row[i]);
                if out.len() >= MAX_TRIANGLES * 3 {
                    return;
                }
            }
        }
        prev = Some(row);
        if reader.at_end() {
            return;
        }
    }
}

/// A 2-D point in shading space (used during Coons/tensor surface evaluation).
type P2 = (f64, f64);

/// Type 6 (Coons) and type 7 (tensor) patch meshes. Each patch is read with a
/// leading edge flag (`0` = standalone, `1`/`2`/`3` = shares an edge with the
/// previous patch, reusing 4 boundary points + 2 corner colours). The 12 (Coons)
/// or 16 (tensor) control points and 4 corner colours are then tessellated into a
/// `PATCH_GRID²` quad grid. A byte alignment precedes each patch.
fn decode_patches<F>(
    reader: &mut BitReader,
    p: &MeshParams,
    color: &F,
    out: &mut Vec<MeshVertex>,
    tensor: bool,
) where
    F: Fn(&[f64]) -> [u8; 3],
{
    // Total control points per patch on the wire: 12 (Coons) or 16 (tensor).
    let total_pts = if tensor { 16 } else { 12 };
    // Previous patch's 12/16 control points and 4 corner colours, for edge sharing.
    let mut prev_pts: Vec<P2> = Vec::new();
    let mut prev_cols: [[u8; 3]; 4] = [[0; 3]; 4];
    let mut have_prev = false;

    loop {
        if out.len() >= MAX_TRIANGLES * 3 {
            return;
        }
        reader.align();
        if reader.at_end() {
            return;
        }
        let Some(flag) = reader.read(p.bits_per_flag) else {
            return;
        };
        let new_pts = if flag == 0 { total_pts } else { total_pts - 4 };
        let new_cols = if flag == 0 { 4 } else { 2 };

        // Read the new control points.
        let mut pts: Vec<P2> = Vec::with_capacity(new_pts);
        for _ in 0..new_pts {
            let Some(xr) = reader.read(p.bits_per_coord) else {
                return;
            };
            let Some(yr) = reader.read(p.bits_per_coord) else {
                return;
            };
            let x = decode_value(xr, p.bits_per_coord, p.decode[0], p.decode[1]);
            let y = decode_value(yr, p.bits_per_coord, p.decode[2], p.decode[3]);
            pts.push((x, y));
        }
        // Read the new corner colours.
        let mut cols: Vec<[u8; 3]> = Vec::with_capacity(new_cols);
        for _ in 0..new_cols {
            let Some(rgb) = read_color(reader, p, color) else {
                return;
            };
            cols.push(rgb);
        }

        // Assemble the full 12/16 control points and 4 corner colours, splicing in
        // the shared edge from the previous patch when `flag != 0`.
        let (full_pts, full_cols) = if flag == 0 {
            if !have_prev && pts.len() < total_pts {
                return;
            }
            let mut c = [[0u8; 3]; 4];
            for (i, col) in cols.iter().enumerate().take(4) {
                c[i] = *col;
            }
            (pts.clone(), c)
        } else {
            if !have_prev {
                return; // a sharing flag without a predecessor is malformed
            }
            match assemble_shared(&prev_pts, &prev_cols, flag, &pts, &cols, tensor) {
                Some(v) => v,
                None => return,
            }
        };

        if full_pts.len() >= 12 {
            tessellate_patch(&full_pts, &full_cols, tensor, out);
        }
        prev_pts = full_pts;
        prev_cols = full_cols;
        have_prev = true;
    }
}

/// Build the full control-point list and corner colours for a patch that shares an
/// edge (`flag` 1/2/3) with `prev`. The 4 reused boundary points and 2 reused
/// colours come from the previous patch's edge; the `new_pts`/`new_cols` provide
/// the rest. Point/colour indices follow ISO 32000-1 Table 85 (Coons) / 86 (tensor).
fn assemble_shared(
    prev_pts: &[P2],
    prev_cols: &[[u8; 3]; 4],
    flag: u32,
    new_pts: &[P2],
    new_cols: &[[u8; 3]],
    tensor: bool,
) -> Option<(Vec<P2>, [[u8; 3]; 4])> {
    // Reused boundary control points (4) by flag, in the order they become the new
    // patch's p1..p4 (first boundary curve). Indices are 1-based in the spec; here
    // 0-based into the previous patch's 12/16-point array.
    // Coons control-point numbering (boundary): p1..p12 as in Figure 47.
    let prev12 = prev_pts; // first 12 are the boundary in both Coons and tensor.
    let (shared_pts, shared_cols): ([P2; 4], [[u8; 3]; 2]) = match flag {
        1 => (
            [prev12[3], prev12[4], prev12[5], prev12[6]],
            [prev_cols[1], prev_cols[2]],
        ),
        2 => (
            [prev12[6], prev12[7], prev12[8], prev12[9]],
            [prev_cols[2], prev_cols[3]],
        ),
        3 => (
            [prev12[9], prev12[10], prev12[11], prev12[0]],
            [prev_cols[3], prev_cols[0]],
        ),
        _ => return None,
    };
    // The new patch boundary: shared 4 points become p1..p4, then the new points
    // fill p5..p12 (and the 4 internal tensor points for type 7).
    let total = if tensor { 16 } else { 12 };
    if new_pts.len() < total - 4 || new_cols.len() < 2 {
        return None;
    }
    let mut full: Vec<P2> = Vec::with_capacity(total);
    full.extend_from_slice(&shared_pts);
    full.extend_from_slice(new_pts);
    full.truncate(total);
    // Corner colours: shared 2 become c1,c2; new 2 become c3,c4.
    let cols = [shared_cols[0], shared_cols[1], new_cols[0], new_cols[1]];
    Some((full, cols))
}

/// Tessellate a 12/16-point patch into a `PATCH_GRID²` quad grid of Gouraud
/// triangles. The surface is evaluated on a `(u, v)` lattice (Coons interpolation
/// for type 6, full bicubic for type 7); corner colours are bilinearly blended.
fn tessellate_patch(pts: &[P2], corners: &[[u8; 3]; 4], tensor: bool, out: &mut Vec<MeshVertex>) {
    // Build the 4×4 control-point grid for surface evaluation.
    let grid = if tensor {
        tensor_grid(pts)
    } else {
        coons_grid(pts)
    };
    let n = PATCH_GRID;
    // Sample positions and colours at each lattice node.
    let mut nodes: Vec<Vert> = Vec::with_capacity((n + 1) * (n + 1));
    for j in 0..=n {
        let v = j as f64 / n as f64;
        for i in 0..=n {
            let u = i as f64 / n as f64;
            let (x, y) = bicubic_surface(&grid, u, v);
            let color = corner_color(corners, u, v);
            nodes.push(Vert { x, y, color });
        }
    }
    let idx = |i: usize, j: usize| j * (n + 1) + i;
    for j in 0..n {
        for i in 0..n {
            let a = nodes[idx(i, j)];
            let b = nodes[idx(i + 1, j)];
            let c = nodes[idx(i + 1, j + 1)];
            let d = nodes[idx(i, j + 1)];
            emit_triangle(out, &a, &b, &c);
            emit_triangle(out, &a, &c, &d);
        }
    }
}

/// Bilinearly interpolate the 4 patch corner colours at `(u, v)`.
///
/// Corner order per the spec: `c1` at `(u=0,v=0)`, `c2` at `(0,1)`, `c3` at
/// `(1,1)`, `c4` at `(1,0)` — i.e. the colours follow the boundary `D1, C2, D2,
/// C1` corners. We map `u` along the `C` (top/bottom) direction and `v` along the
/// `D` (side) direction consistently with [`coons_grid`].
fn corner_color(c: &[[u8; 3]; 4], u: f64, v: f64) -> [u8; 3] {
    // c[0]=(0,0) c[1]=(0,1) c[2]=(1,1) c[3]=(1,0).
    let mut rgb = [0u8; 3];
    for (k, slot) in rgb.iter_mut().enumerate() {
        let c00 = c[0][k] as f64;
        let c01 = c[1][k] as f64;
        let c11 = c[2][k] as f64;
        let c10 = c[3][k] as f64;
        let top = c00 * (1.0 - u) + c10 * u; // v = 0 edge
        let bot = c01 * (1.0 - u) + c11 * u; // v = 1 edge
        let val = top * (1.0 - v) + bot * v;
        *slot = val.round().clamp(0.0, 255.0) as u8;
    }
    rgb
}

/// Evaluate a tensor-product bicubic Bézier surface defined by a 4×4 control grid
/// at parameters `(u, v) ∈ [0,1]²`.
fn bicubic_surface(grid: &[[P2; 4]; 4], u: f64, v: f64) -> P2 {
    let bu = bernstein3(u);
    let bv = bernstein3(v);
    let mut x = 0.0;
    let mut y = 0.0;
    for (i, bui) in bu.iter().enumerate() {
        for (j, bvj) in bv.iter().enumerate() {
            let w = bui * bvj;
            x += w * grid[i][j].0;
            y += w * grid[i][j].1;
        }
    }
    (x, y)
}

/// The four cubic Bernstein basis weights at `t`.
fn bernstein3(t: f64) -> [f64; 4] {
    let mt = 1.0 - t;
    [mt * mt * mt, 3.0 * mt * mt * t, 3.0 * mt * t * t, t * t * t]
}

/// Build the 4×4 control grid for a **tensor** patch (type 7): the 16 wire points
/// map directly to the Bézier control net, in the spec's boundary-then-internal
/// numbering (Figure 48 / Table 86).
fn tensor_grid(p: &[P2]) -> [[P2; 4]; 4] {
    // Spec point numbering (1-based) → grid[row][col] (0-based, row=u, col=v):
    //   boundary p1..p12 trace the outline starting at corner (0,0);
    //   p13..p16 are the four internal points.
    // Mapping (0-based p indices):
    //   grid[0][0]=p0  grid[0][1]=p1  grid[0][2]=p2  grid[0][3]=p3
    //   grid[1][0]=p11 grid[1][1]=p12 grid[1][2]=p13 grid[1][3]=p4
    //   grid[2][0]=p10 grid[2][1]=p15 grid[2][2]=p14 grid[2][3]=p5
    //   grid[3][0]=p9  grid[3][1]=p8  grid[3][2]=p7  grid[3][3]=p6
    let g = |i: usize| p.get(i).copied().unwrap_or((0.0, 0.0));
    [
        [g(0), g(1), g(2), g(3)],
        [g(11), g(12), g(13), g(4)],
        [g(10), g(15), g(14), g(5)],
        [g(9), g(8), g(7), g(6)],
    ]
}

/// Build the 4×4 control grid for a **Coons** patch (type 6): the 12 boundary
/// points define the outline; the 4 internal control points are synthesised from
/// the boundary by the standard Coons-to-bicubic formula (ISO 32000-1 §8.7.4.5.7)
/// so the same bicubic evaluator can be reused. The boundary layout matches
/// [`tensor_grid`]'s outer ring.
fn coons_grid(p: &[P2]) -> [[P2; 4]; 4] {
    let g = |i: usize| p.get(i).copied().unwrap_or((0.0, 0.0));
    // Outer ring (boundary) placed at the same node positions as the tensor grid;
    // the four interior nodes (g[1][1], g[1][2], g[2][1], g[2][2]) are filled below.
    let mut m: [[P2; 4]; 4] = [
        [g(0), g(1), g(2), g(3)],
        [g(11), (0.0, 0.0), (0.0, 0.0), g(4)],
        [g(10), (0.0, 0.0), (0.0, 0.0), g(5)],
        [g(9), g(8), g(7), g(6)],
    ];
    // Linear combination of grid nodes with the given weights (point arithmetic).
    let comb = |terms: &[(f64, P2)]| -> P2 {
        let mut x = 0.0;
        let mut y = 0.0;
        for &(w, p) in terms {
            x += w * p.0;
            y += w * p.1;
        }
        (x / 9.0, y / 9.0)
    };
    // Corners and edge nodes (grid indices g[i][j]).
    let g00 = m[0][0];
    let g01 = m[0][1];
    let g02 = m[0][2];
    let g03 = m[0][3];
    let g10 = m[1][0];
    let g13 = m[1][3];
    let g20 = m[2][0];
    let g23 = m[2][3];
    let g30 = m[3][0];
    let g31 = m[3][1];
    let g32 = m[3][2];
    let g33 = m[3][3];
    // The four interior control points (spec formula; each ÷ 9 via `comb`).
    m[1][1] = comb(&[
        (-4.0, g00),
        (6.0, g01),
        (6.0, g10),
        (-2.0, g03),
        (-2.0, g30),
        (3.0, g31),
        (3.0, g13),
        (-1.0, g33),
    ]);
    m[1][2] = comb(&[
        (-4.0, g03),
        (6.0, g02),
        (6.0, g13),
        (-2.0, g00),
        (-2.0, g33),
        (3.0, g32),
        (3.0, g10),
        (-1.0, g30),
    ]);
    m[2][1] = comb(&[
        (-4.0, g30),
        (6.0, g31),
        (6.0, g20),
        (-2.0, g33),
        (-2.0, g00),
        (3.0, g01),
        (3.0, g23),
        (-1.0, g03),
    ]);
    m[2][2] = comb(&[
        (-4.0, g33),
        (6.0, g32),
        (6.0, g23),
        (-2.0, g30),
        (-2.0, g03),
        (3.0, g02),
        (3.0, g20),
        (-1.0, g00),
    ]);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bit writer mirroring [`BitReader`], for building test fixtures.
    struct BitWriter {
        bytes: Vec<u8>,
        bit: usize,
    }
    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bit: 0,
            }
        }
        fn write(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                let b = ((value >> i) & 1) as u8;
                let byte_idx = self.bit >> 3;
                if byte_idx >= self.bytes.len() {
                    self.bytes.push(0);
                }
                let shift = 7 - (self.bit & 7);
                self.bytes[byte_idx] |= b << shift;
                self.bit += 1;
            }
        }
        fn align(&mut self) {
            let rem = self.bit & 7;
            if rem != 0 {
                self.bit += 8 - rem;
                let need = self.bit >> 3;
                while self.bytes.len() < need {
                    self.bytes.push(0);
                }
            }
        }
    }

    /// Colour closure that treats the single decoded component as a grey level.
    fn grey(comps: &[f64]) -> [u8; 3] {
        let v = (comps.first().copied().unwrap_or(0.0).clamp(0.0, 1.0) * 255.0).round() as u8;
        [v, v, v]
    }

    #[test]
    fn bit_reader_reads_msb_first() {
        let data = [0b1011_0010u8, 0b1100_0000u8];
        let mut r = BitReader::new(&data);
        assert_eq!(r.read(3), Some(0b101));
        assert_eq!(r.read(5), Some(0b10010));
        assert_eq!(r.read(2), Some(0b11));
        // 6 bits remain (all zero except none) → readable, then EOF.
        assert_eq!(r.read(6), Some(0));
        assert!(r.at_end());
        assert_eq!(r.read(1), None);
    }

    #[test]
    fn decode_value_endpoints() {
        assert_eq!(decode_value(0, 8, 0.0, 1.0), 0.0);
        assert_eq!(decode_value(255, 8, 0.0, 1.0), 1.0);
        assert!((decode_value(128, 8, 0.0, 1.0) - 0.50196).abs() < 1e-4);
    }

    #[test]
    fn free_form_single_triangle() {
        // One flag-0 triangle, three vertices. 8-bit coords/components/flags.
        let p = MeshParams {
            shading_type: 4,
            bits_per_coord: 8,
            bits_per_comp: 8,
            bits_per_flag: 8,
            decode: vec![0.0, 100.0, 0.0, 100.0, 0.0, 1.0],
            n_color: 1,
            vertices_per_row: 0,
        };
        let mut w = BitWriter::new();
        // (flag, x, y, c) ×3. Coords 0..100 over 0..255 raw → raw = round(x/100*255).
        let raw = |val: f64| (val / 100.0 * 255.0).round() as u32;
        for (x, y, c) in [(0.0, 0.0, 0u32), (100.0, 0.0, 255), (0.0, 100.0, 128)] {
            w.write(0, 8); // flag
            w.write(raw(x), 8);
            w.write(raw(y), 8);
            w.write(c, 8);
        }
        let tris = decode_mesh(&w.bytes, &p, grey);
        assert_eq!(tris.len(), 3, "exactly one triangle");
        assert!((tris[0].x - 0.0).abs() < 0.5 && (tris[0].y - 0.0).abs() < 0.5);
        assert!((tris[1].x - 100.0).abs() < 0.5);
        assert!((tris[2].y - 100.0).abs() < 0.5);
        assert_eq!(tris[0].color, [0, 0, 0]);
        assert_eq!(tris[1].color, [255, 255, 255]);
    }

    #[test]
    fn lattice_two_by_two_makes_two_triangles() {
        // 2 rows × 2 columns → one quad → two triangles.
        let p = MeshParams {
            shading_type: 5,
            bits_per_coord: 8,
            bits_per_comp: 8,
            bits_per_flag: 0,
            decode: vec![0.0, 10.0, 0.0, 10.0, 0.0, 1.0],
            n_color: 1,
            vertices_per_row: 2,
        };
        let raw = |val: f64| (val / 10.0 * 255.0).round() as u32;
        let mut w = BitWriter::new();
        for (x, y) in [(0.0, 0.0), (10.0, 0.0), (0.0, 10.0), (10.0, 10.0)] {
            w.write(raw(x), 8);
            w.write(raw(y), 8);
            w.write(128, 8);
        }
        let tris = decode_mesh(&w.bytes, &p, grey);
        assert_eq!(tris.len(), 6, "one quad = two triangles");
    }

    #[test]
    fn coons_patch_tessellates_to_grid() {
        // One flag-0 Coons patch (12 points, 4 colours) → 2·GRID² triangles.
        let p = MeshParams {
            shading_type: 6,
            bits_per_coord: 8,
            bits_per_comp: 8,
            bits_per_flag: 8,
            decode: vec![0.0, 100.0, 0.0, 100.0, 0.0, 1.0],
            n_color: 1,
            vertices_per_row: 0,
        };
        let raw = |val: f64| (val / 100.0 * 255.0).round() as u32;
        let mut w = BitWriter::new();
        w.align();
        w.write(0, 8); // flag 0
                       // 12 boundary points around a 0..100 square (exact positions irrelevant
                       // to the triangle count; just exercise the reader/tessellator).
        let pts = [
            (0.0, 0.0),
            (0.0, 33.0),
            (0.0, 66.0),
            (0.0, 100.0),
            (33.0, 100.0),
            (66.0, 100.0),
            (100.0, 100.0),
            (100.0, 66.0),
            (100.0, 33.0),
            (100.0, 0.0),
            (66.0, 0.0),
            (33.0, 0.0),
        ];
        for (x, y) in pts {
            w.write(raw(x), 8);
            w.write(raw(y), 8);
        }
        for c in [0u32, 85, 170, 255] {
            w.write(c, 8);
        }
        let tris = decode_mesh(&w.bytes, &p, grey);
        assert_eq!(tris.len(), 2 * PATCH_GRID * PATCH_GRID * 3);
        // All emitted vertices must lie within the patch bounding square.
        for v in &tris {
            assert!((-1.0..=101.0).contains(&v.x));
            assert!((-1.0..=101.0).contains(&v.y));
        }
    }
}
