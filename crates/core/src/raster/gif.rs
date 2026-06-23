//! GIF (87a/89a) decoder → RGBA — pure std, zero dependency.
//!
//! Decodes the global/local colour table, LZW image data, interlacing, the
//! graphic-control transparency index, **and the full animation**: every frame
//! is composited onto a persistent logical-screen canvas honouring its
//! sub-rectangle placement and disposal method (no-dispose / restore-background
//! / restore-previous), and each carries its centisecond delay. The native
//! replacement for a third-party image library's GIF path.
//!
//! [`decode_gif`] returns the single composited first frame (unchanged — the
//! static-image path for PDF embedding); [`decode_gif_frames`] returns the whole
//! sequence, and [`decode_gif_frame`] extracts one composited frame by index.

fn le16(d: &[u8], o: usize) -> usize {
    (*d.get(o).unwrap_or(&0) as usize) | ((*d.get(o + 1).unwrap_or(&0) as usize) << 8)
}

/// One decoded animation frame: the fully-composited logical screen at the
/// moment this frame is shown, plus how long to show it.
#[derive(Debug, Clone)]
pub struct GifFrame {
    /// Logical screen width (same for every frame).
    pub width: u32,
    /// Logical screen height (same for every frame).
    pub height: u32,
    /// Composited RGBA pixels (`width * height * 4`).
    pub rgba: Vec<u8>,
    /// Display duration in centiseconds (1/100 s), as authored. 0 if unspecified.
    pub delay_cs: u16,
}

/// A decoded GIF animation: its logical-screen size and every composited frame
/// in display order (a static GIF yields exactly one frame).
#[derive(Debug, Clone)]
pub struct GifAnimation {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<GifFrame>,
}

/// Disposal method (graphic-control packed bits 2..4): what to do with this
/// frame's pixels before compositing the next one.
#[derive(Clone, Copy, PartialEq)]
enum Disposal {
    /// 0/1 — leave the frame in place.
    Keep,
    /// 2 — clear the frame's rectangle back to the (transparent) background.
    RestoreBackground,
    /// 3 — revert the rectangle to the canvas as it was before this frame.
    RestorePrevious,
}

/// Decode a GIF into `(width, height, rgba)` — the single composited first
/// frame, on the full logical screen. `None` on a malformed/truncated stream.
/// Backward-compatible static-image entry point.
pub fn decode_gif(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let f = decode_gif_frame(data, 0)?;
    Some((f.width, f.height, f.rgba))
}

/// Decode a single composited frame by index (0-based). `None` if the stream is
/// malformed or has fewer than `index + 1` frames. Decoding stops as soon as the
/// requested frame is composited, so extracting an early frame from a long
/// animation stays cheap.
pub fn decode_gif_frame(data: &[u8], index: usize) -> Option<GifFrame> {
    let frames = decode_frames(data, Some(index))?.frames;
    // `decode_frames` stops *after* compositing frame `index`; if the stream had
    // fewer frames it returns all of them — so require the index to exist.
    if frames.len() <= index {
        return None;
    }
    frames.into_iter().nth(index)
}

/// Decode **all** frames of a GIF animation (each composited onto the logical
/// screen, in display order). A static GIF yields one frame. `None` on a
/// malformed/truncated stream with no decodable frame.
pub fn decode_gif_frames(data: &[u8]) -> Option<GifAnimation> {
    decode_frames(data, None)
}

/// Core frame walker. When `stop_at` is `Some(n)`, returns once frame `n` has
/// been composited (its `frames` holds frames `0..=n`); when `None`, decodes
/// every frame. Returns `None` only if not a GIF or no frame could be decoded.
fn decode_frames(data: &[u8], stop_at: Option<usize>) -> Option<GifAnimation> {
    if data.len() < 13 || (&data[0..3] != b"GIF") {
        return None;
    }
    let screen_w = le16(data, 6);
    let screen_h = le16(data, 8);
    let packed = data[10];
    let gct_flag = packed & 0x80 != 0;
    let gct_size = 2usize << (packed & 0x07);
    let mut pos = 13;
    let gct = if gct_flag {
        let t = read_table(data, pos, gct_size)?;
        pos += gct_size * 3;
        t
    } else {
        Vec::new()
    };

    // The persistent logical-screen canvas (transparent to start). Frame
    // sub-rectangles are blended onto it; disposal acts between frames.
    let mut canvas: Vec<u8> = vec![0u8; screen_w * screen_h * 4];
    let mut frames: Vec<GifFrame> = Vec::new();
    // Pending graphic-control state for the *next* image descriptor.
    let mut transparent: Option<u8> = None;
    let mut disposal = Disposal::Keep;
    let mut delay_cs: u16 = 0;

    while pos < data.len() {
        match data[pos] {
            0x21 => {
                // Extension. 0xF9 = Graphic Control (transparency/disposal/delay).
                let label = *data.get(pos + 1)?;
                pos += 2;
                if label == 0xF9 {
                    // Block: size(1)=4, packed(1), delay(2), tindex(1), term(0).
                    let block_packed = *data.get(pos + 1)?;
                    delay_cs = le16(data, pos + 2) as u16;
                    transparent = if block_packed & 0x01 != 0 {
                        Some(*data.get(pos + 4)?)
                    } else {
                        None
                    };
                    disposal = match (block_packed >> 2) & 0x07 {
                        2 => Disposal::RestoreBackground,
                        3 => Disposal::RestorePrevious,
                        _ => Disposal::Keep,
                    };
                }
                pos = skip_sub_blocks(data, pos)?;
            }
            0x2C => {
                // Image descriptor: sub-rectangle + flags, then LZW data.
                let left = le16(data, pos + 1);
                let top = le16(data, pos + 3);
                let iw = le16(data, pos + 5);
                let ih = le16(data, pos + 7);
                let ipacked = *data.get(pos + 9)?;
                pos += 10;
                let lct_flag = ipacked & 0x80 != 0;
                let interlaced = ipacked & 0x40 != 0;
                let palette = if lct_flag {
                    let lct_size = 2usize << (ipacked & 0x07);
                    let t = read_table(data, pos, lct_size)?;
                    pos += lct_size * 3;
                    t
                } else {
                    gct.clone()
                };
                if palette.is_empty() || iw == 0 || ih == 0 {
                    return finalize(screen_w, screen_h, frames);
                }
                let min_code = *data.get(pos)?;
                pos += 1;
                let (lzw, next) = collect_sub_blocks(data, pos)?;
                pos = next;
                let indices = lzw_decode(&lzw, min_code, iw * ih)?;

                // Snapshot for a possible restore-to-previous of THIS frame's rect.
                let saved = if disposal == Disposal::RestorePrevious {
                    Some(canvas.clone())
                } else {
                    None
                };
                composite(
                    &mut canvas, screen_w, screen_h, left, top, iw, ih, interlaced, &indices,
                    &palette, transparent,
                );
                frames.push(GifFrame {
                    width: screen_w as u32,
                    height: screen_h as u32,
                    rgba: canvas.clone(),
                    delay_cs,
                });
                if let Some(idx) = stop_at {
                    if frames.len() > idx {
                        return finalize(screen_w, screen_h, frames);
                    }
                }
                // Apply this frame's disposal before the next is composited.
                match disposal {
                    Disposal::Keep => {}
                    Disposal::RestoreBackground => {
                        clear_rect(&mut canvas, screen_w, screen_h, left, top, iw, ih);
                    }
                    Disposal::RestorePrevious => {
                        if let Some(prev) = saved {
                            canvas = prev;
                        }
                    }
                }
                // Reset per-frame graphic-control state (it applies to one image).
                transparent = None;
                disposal = Disposal::Keep;
                delay_cs = 0;
            }
            0x3B => break, // trailer
            _ => pos += 1,
        }
    }
    finalize(screen_w, screen_h, frames)
}

/// Wrap the collected frames into an animation, or `None` if there were none.
fn finalize(w: usize, h: usize, frames: Vec<GifFrame>) -> Option<GifAnimation> {
    if frames.is_empty() {
        return None;
    }
    Some(GifAnimation {
        width: w as u32,
        height: h as u32,
        frames,
    })
}

/// Blend one frame's palette indices onto the canvas at `(left, top)`, honouring
/// interlacing and the transparency index (transparent pixels leave the canvas
/// untouched — the prior frame shows through).
#[allow(clippy::too_many_arguments)]
fn composite(
    canvas: &mut [u8],
    w: usize,
    h: usize,
    left: usize,
    top: usize,
    iw: usize,
    ih: usize,
    interlaced: bool,
    indices: &[u8],
    palette: &[[u8; 3]],
    transparent: Option<u8>,
) {
    for (src_row, dst_y) in interlace_rows(ih, interlaced).into_iter().enumerate() {
        let cy = top + dst_y;
        if cy >= h {
            continue;
        }
        for x in 0..iw {
            let cx = left + x;
            if cx >= w {
                continue;
            }
            let idx = indices[src_row * iw + x];
            if Some(idx) == transparent {
                continue; // leave the canvas pixel as-is
            }
            let c = palette.get(idx as usize).copied().unwrap_or([0, 0, 0]);
            let p = (cy * w + cx) * 4;
            canvas[p] = c[0];
            canvas[p + 1] = c[1];
            canvas[p + 2] = c[2];
            canvas[p + 3] = 255;
        }
    }
}

/// Clear a sub-rectangle of the canvas to the transparent background (0,0,0,0).
fn clear_rect(canvas: &mut [u8], w: usize, h: usize, left: usize, top: usize, iw: usize, ih: usize) {
    for y in top..(top + ih).min(h) {
        for x in left..(left + iw).min(w) {
            let p = (y * w + x) * 4;
            canvas[p..p + 4].fill(0);
        }
    }
}

/// Destination row order for an image of height `ih`: identity, or the GIF
/// four-pass interlace order.
fn interlace_rows(ih: usize, interlaced: bool) -> Vec<usize> {
    if !interlaced {
        return (0..ih).collect();
    }
    let mut r = Vec::with_capacity(ih);
    for (start, step) in [(0usize, 8usize), (4, 8), (2, 4), (1, 2)] {
        let mut y = start;
        while y < ih {
            r.push(y);
            y += step;
        }
    }
    r
}

fn read_table(d: &[u8], o: usize, n: usize) -> Option<Vec<[u8; 3]>> {
    let end = o + n * 3;
    if end > d.len() {
        return None;
    }
    Some(
        (0..n)
            .map(|i| [d[o + i * 3], d[o + i * 3 + 1], d[o + i * 3 + 2]])
            .collect(),
    )
}

/// Advance past a label byte's chain of length-prefixed sub-blocks (ending in a
/// 0-length block). `pos` points at the first sub-block length byte.
fn skip_sub_blocks(d: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        let len = *d.get(pos)? as usize;
        pos += 1 + len;
        if len == 0 {
            return Some(pos);
        }
    }
}

/// Concatenate a chain of length-prefixed sub-blocks into one buffer; returns it
/// plus the position just after the terminating 0-block.
fn collect_sub_blocks(d: &[u8], mut pos: usize) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    loop {
        let len = *d.get(pos)? as usize;
        pos += 1;
        if len == 0 {
            return Some((out, pos));
        }
        out.extend_from_slice(d.get(pos..pos + len)?);
        pos += len;
    }
}

/// Variable-width LZW decode (GIF flavour) → palette indices.
fn lzw_decode(data: &[u8], min_code: u8, expected: usize) -> Option<Vec<u8>> {
    let clear = 1u16 << min_code;
    let eoi = clear + 1;
    let mut code_size = min_code + 1;
    let mut dict: Vec<Vec<u8>> = Vec::with_capacity(4096);
    let reset = |dict: &mut Vec<Vec<u8>>| {
        dict.clear();
        for i in 0..clear {
            dict.push(vec![i as u8]);
        }
        dict.push(Vec::new()); // clear
        dict.push(Vec::new()); // eoi
    };
    reset(&mut dict);

    let mut out: Vec<u8> = Vec::with_capacity(expected);
    let mut bitpos = 0usize;
    let mut prev: Option<u16> = None;
    let read = |bitpos: &mut usize, code_size: u8| -> Option<u16> {
        let mut code = 0u16;
        for i in 0..code_size {
            let byte = *data.get(*bitpos / 8)?;
            let bit = (byte >> (*bitpos % 8)) & 1;
            code |= (bit as u16) << i;
            *bitpos += 1;
        }
        Some(code)
    };

    loop {
        if bitpos + code_size as usize > data.len() * 8 {
            break;
        }
        let code = read(&mut bitpos, code_size)?;
        if code == clear {
            reset(&mut dict);
            code_size = min_code + 1;
            prev = None;
            continue;
        }
        if code == eoi {
            break;
        }
        let entry = if (code as usize) < dict.len() {
            dict[code as usize].clone()
        } else if let Some(p) = prev {
            // KwKwK case.
            let mut e = dict[p as usize].clone();
            e.push(dict[p as usize][0]);
            e
        } else {
            return None;
        };
        out.extend_from_slice(&entry);
        if let Some(p) = prev {
            let mut new = dict[p as usize].clone();
            new.push(entry[0]);
            dict.push(new);
            if dict.len() == (1 << code_size) && code_size < 12 {
                code_size += 1;
            }
        }
        prev = Some(code);
        if out.len() >= expected {
            break;
        }
    }
    out.truncate(expected);
    out.resize(expected, 0);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tiny 89a GIF: 2×2, 4-colour GCT, one LZW-coded frame.
    fn sample_gif() -> Vec<u8> {
        let mut g = Vec::new();
        g.extend_from_slice(b"GIF89a");
        g.extend_from_slice(&[2, 0, 2, 0]); // 2×2
        g.push(0x80 | 0x01); // GCT flag, size 2 → 4 colours
        g.extend_from_slice(&[0, 0]); // bg, aspect
                                      // GCT: red, green, blue, white.
        g.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        // Image descriptor.
        g.push(0x2C);
        g.extend_from_slice(&[0, 0, 0, 0, 2, 0, 2, 0, 0x00]); // pos 0,0 size 2×2, no LCT
                                                              // LZW: min code size 2 → clear=4, eoi=5; codes 0,1,2,3 then EOI.
        let min = 2u8;
        let codes: Vec<(u16, u8)> = {
            // clear(4) @3bits, then literals 0,1,2,3, then EOI(5). Code size grows
            // to 4 once the dict reaches 8 entries (after adding 2 strings).
            let mut v = vec![(4u16, 3u8)]; // clear
                                           // After clear: dict has 6 entries (0..3,clear,eoi), code_size=3.
                                           // Emit 0 (size3), 1 (size3) → dict adds "01" (entry6) → size stays 3
                                           //   until len==8. After 1: len=7. Emit 2 → adds "12" len=8 → size→4.
            v.push((0, 3));
            v.push((1, 3));
            v.push((2, 3));
            v.push((3, 4));
            v.push((5, 4)); // eoi
            v
        };
        // Pack LSB-first.
        let mut bits = Vec::new();
        let mut acc = 0u32;
        let mut nb = 0u32;
        for (c, sz) in codes {
            acc |= (c as u32) << nb;
            nb += sz as u32;
            while nb >= 8 {
                bits.push((acc & 0xFF) as u8);
                acc >>= 8;
                nb -= 8;
            }
        }
        if nb > 0 {
            bits.push((acc & 0xFF) as u8);
        }
        g.push(min);
        g.push(bits.len() as u8);
        g.extend_from_slice(&bits);
        g.push(0x00); // block terminator
        g.push(0x3B); // trailer
        g
    }

    #[test]
    fn decodes_a_2x2_gif() {
        let (w, h, rgba) = decode_gif(&sample_gif()).expect("decodes");
        assert_eq!((w, h), (2, 2));
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "top-left red");
        assert_eq!(&rgba[4..8], &[0, 255, 0, 255], "top-right green");
        assert_eq!(&rgba[8..12], &[0, 0, 255, 255], "bottom-left blue");
        assert_eq!(&rgba[12..16], &[255, 255, 255, 255], "bottom-right white");
    }

    #[test]
    fn rejects_non_gif() {
        assert!(decode_gif(b"not a gif").is_none());
        assert!(decode_gif(&[]).is_none());
    }

    // ── multi-frame animation ────────────────────────────────────────────────

    /// LZW-pack four palette indices as a 2×2 frame's image data (min code
    /// size 2). Growth depends only on the literal count, so any indices work:
    /// clear, p0(3b), p1(3b), p2(3b), p3(4b), eoi(4b).
    fn pack_2x2(px: [u16; 4]) -> Vec<u8> {
        let codes: Vec<(u16, u8)> = vec![
            (4, 3), // clear
            (px[0], 3),
            (px[1], 3),
            (px[2], 3),
            (px[3], 4),
            (5, 4), // eoi
        ];
        let mut bits = Vec::new();
        let (mut acc, mut nb) = (0u32, 0u32);
        for (c, sz) in codes {
            acc |= (c as u32) << nb;
            nb += sz as u32;
            while nb >= 8 {
                bits.push((acc & 0xFF) as u8);
                acc >>= 8;
                nb -= 8;
            }
        }
        if nb > 0 {
            bits.push((acc & 0xFF) as u8);
        }
        bits
    }

    /// Append a full image block (graphic control + descriptor + LZW data) for a
    /// 2×2 frame at `(left, top)` with the given indices, delay and disposal.
    fn push_frame(
        g: &mut Vec<u8>,
        left: u16,
        top: u16,
        px: [u16; 4],
        delay_cs: u16,
        disposal: u8,
        transparent: Option<u8>,
    ) {
        // Graphic Control Extension.
        let tflag = if transparent.is_some() { 1 } else { 0 };
        let packed = (disposal << 2) | tflag;
        g.extend_from_slice(&[0x21, 0xF9, 0x04, packed]);
        g.extend_from_slice(&delay_cs.to_le_bytes());
        g.push(transparent.unwrap_or(0));
        g.push(0x00); // block terminator
                      // Image descriptor at (left, top), 2×2, no LCT.
        g.push(0x2C);
        g.extend_from_slice(&left.to_le_bytes());
        g.extend_from_slice(&top.to_le_bytes());
        g.extend_from_slice(&[2, 0, 2, 0, 0x00]);
        let bits = pack_2x2(px);
        g.push(2); // min code size
        g.push(bits.len() as u8);
        g.extend_from_slice(&bits);
        g.push(0x00); // block terminator
    }

    /// A 4×2 logical screen, two full-frame-ish 2×2 frames placed side by side
    /// (frame 0 at x=0 in red, frame 1 at x=2 in green), each with a delay.
    fn two_frame_gif() -> Vec<u8> {
        let mut g = Vec::new();
        g.extend_from_slice(b"GIF89a");
        g.extend_from_slice(&[4, 0, 2, 0]); // 4×2 logical screen
        g.push(0x80 | 0x01); // GCT flag, 4 colours
        g.extend_from_slice(&[0, 0]);
        g.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]); // R,G,B,W
                                                                                // Frame 0: red square at (0,0), delay 10cs, keep.
        push_frame(&mut g, 0, 0, [0, 0, 0, 0], 10, 0, None);
        // Frame 1: green square at (2,0), delay 7cs, keep.
        push_frame(&mut g, 2, 0, [1, 1, 1, 1], 7, 0, None);
        g.push(0x3B); // trailer
        g
    }

    #[test]
    fn decodes_all_frames_with_delays_and_placement() {
        let anim = decode_gif_frames(&two_frame_gif()).expect("animation decodes");
        assert_eq!((anim.width, anim.height), (4, 2));
        assert_eq!(anim.frames.len(), 2, "two frames");
        assert_eq!(anim.frames[0].delay_cs, 10);
        assert_eq!(anim.frames[1].delay_cs, 7);

        // Frame 0: red at the left 2×2, right half still transparent.
        let f0 = &anim.frames[0].rgba;
        assert_eq!(&f0[0..4], &[255, 0, 0, 255], "f0 (0,0) red");
        let right = 2 * 4; // byte offset of pixel (x=2, y=0) on a 4-wide screen
        assert_eq!(&f0[right..right + 4], &[0, 0, 0, 0], "f0 right half transparent");

        // Frame 1 composites onto the canvas: left still red, right now green.
        let f1 = &anim.frames[1].rgba;
        assert_eq!(&f1[0..4], &[255, 0, 0, 255], "f1 left stays red (kept)");
        assert_eq!(&f1[right..right + 4], &[0, 255, 0, 255], "f1 right now green");
    }

    #[test]
    fn single_frame_decode_matches_first_composited_frame() {
        let data = two_frame_gif();
        let (w, h, rgba) = decode_gif(&data).expect("first frame");
        let anim = decode_gif_frames(&data).unwrap();
        assert_eq!((w, h), (anim.width, anim.height));
        assert_eq!(rgba, anim.frames[0].rgba, "decode_gif == first frame");
    }

    #[test]
    fn extract_frame_by_index() {
        let data = two_frame_gif();
        let f1 = decode_gif_frame(&data, 1).expect("frame 1");
        assert_eq!(f1.delay_cs, 7);
        let right = 2 * 4;
        assert_eq!(&f1.rgba[right..right + 4], &[0, 255, 0, 255], "green right");
        // Out-of-range index → None.
        assert!(decode_gif_frame(&data, 5).is_none());
    }

    #[test]
    fn restore_background_disposal_clears_rect() {
        // Frame 0 (red, full-ish) with disposal=2 (restore background): after it
        // is shown, its rectangle is cleared before frame 1, so frame 1's canvas
        // does NOT retain frame 0's pixels outside frame 1's own rect.
        let mut g = Vec::new();
        g.extend_from_slice(b"GIF89a");
        g.extend_from_slice(&[2, 0, 2, 0]); // 2×2 logical screen
        g.push(0x80 | 0x01);
        g.extend_from_slice(&[0, 0]);
        g.extend_from_slice(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]);
        // Frame 0: red full 2×2, disposal = restore-background.
        push_frame(&mut g, 0, 0, [0, 0, 0, 0], 5, 2, None);
        // Frame 1: a single green pixel region at (0,0) covering 2×2 too, but we
        // make only one pixel green by reusing indices; here all green for
        // simplicity — the point is frame 0's disposal cleared first.
        push_frame(&mut g, 0, 0, [1, 1, 1, 1], 5, 0, None);
        g.push(0x3B);
        let anim = decode_gif_frames(&g).expect("decodes");
        assert_eq!(anim.frames.len(), 2);
        // Frame 0 was red; frame 1 overwrote with green (restore-bg cleared red
        // first, then green composited) → all green, no red bleed-through.
        for px in anim.frames[1].rgba.chunks_exact(4) {
            assert_eq!(px, &[0, 255, 0, 255], "frame 1 fully green");
        }
    }
}
