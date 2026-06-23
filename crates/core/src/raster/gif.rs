//! GIF (87a/89a) decoder → RGBA — pure std, zero dependency.
//!
//! Decodes the first image frame (sufficient for converting a GIF to a static
//! raster for PDF embedding, the native replacement for a third-party image
//! library's GIF path). Handles the global/local colour table, the LZW image
//! data, interlacing, and a graphic-control transparency index. Animation beyond
//! the first frame is ignored.

fn le16(d: &[u8], o: usize) -> usize {
    (*d.get(o).unwrap_or(&0) as usize) | ((*d.get(o + 1).unwrap_or(&0) as usize) << 8)
}

/// Decode a GIF into `(width, height, rgba)` (the first frame). `None` on a
/// malformed/truncated stream.
pub fn decode_gif(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
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

    let mut transparent: Option<u8> = None;
    while pos < data.len() {
        match data[pos] {
            0x21 => {
                // Extension. 0xF9 = Graphic Control (transparency).
                let label = *data.get(pos + 1)?;
                pos += 2;
                if label == 0xF9 {
                    // Block: size(1)=4, packed(1), delay(2), tindex(1), term(0).
                    let block_packed = *data.get(pos + 1)?;
                    if block_packed & 0x01 != 0 {
                        transparent = Some(*data.get(pos + 4)?);
                    }
                }
                pos = skip_sub_blocks(data, pos)?;
            }
            0x2C => {
                // Image descriptor.
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
                    return None;
                }
                let min_code = *data.get(pos)?;
                pos += 1;
                let (lzw, _) = collect_sub_blocks(data, pos)?;
                let indices = lzw_decode(&lzw, min_code, iw * ih)?;
                return Some(assemble(
                    screen_w.max(iw),
                    screen_h.max(ih),
                    iw,
                    ih,
                    interlaced,
                    &indices,
                    &palette,
                    transparent,
                ));
            }
            0x3B => break, // trailer
            _ => pos += 1,
        }
    }
    None
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

/// Map palette indices to an RGBA buffer the size of the logical screen,
/// honouring interlacing and the transparency index.
#[allow(clippy::too_many_arguments)]
fn assemble(
    w: usize,
    h: usize,
    iw: usize,
    ih: usize,
    interlaced: bool,
    indices: &[u8],
    palette: &[[u8; 3]],
    transparent: Option<u8>,
) -> (u32, u32, Vec<u8>) {
    let mut rgba = vec![0u8; w * h * 4];
    // Interlace pass row order.
    let rows: Vec<usize> = if interlaced {
        let mut r = Vec::with_capacity(ih);
        for (start, step) in [(0usize, 8usize), (4, 8), (2, 4), (1, 2)] {
            let mut y = start;
            while y < ih {
                r.push(y);
                y += step;
            }
        }
        r
    } else {
        (0..ih).collect()
    };
    for (src_row, &dst_y) in rows.iter().enumerate() {
        for x in 0..iw {
            let idx = indices[src_row * iw + x];
            if Some(idx) == transparent {
                continue; // leave RGBA (0,0,0,0)
            }
            let c = palette.get(idx as usize).copied().unwrap_or([0, 0, 0]);
            if dst_y < h && x < w {
                let p = (dst_y * w + x) * 4;
                rgba[p] = c[0];
                rgba[p + 1] = c[1];
                rgba[p + 2] = c[2];
                rgba[p + 3] = 255;
            }
        }
    }
    (w as u32, h as u32, rgba)
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
}
