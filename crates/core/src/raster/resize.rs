//! Alpha-correct separable image resampling (RGBA8) — pure std.
//!
//! Two passes (horizontal then vertical). Each output sample is a normalized
//! weighted average of the source samples in its footprint, using a triangle
//! kernel whose support scales with the downscale factor — so shrinking averages
//! (box-like, no aliasing) and enlarging interpolates (bilinear). Alpha is
//! premultiplied during filtering so edges between transparent and coloured
//! pixels don't bleed a dark/bright fringe.

/// One output sample's contributions: `(source index, weight)`, weights summing
/// to 1. Triangle kernel; support = `max(1, src/dst)` source pixels.
fn axis_weights(src_n: u32, dst_n: u32) -> Vec<Vec<(usize, f32)>> {
    let scale = src_n as f32 / dst_n as f32;
    let support = scale.max(1.0);
    let mut all = Vec::with_capacity(dst_n as usize);
    for d in 0..dst_n {
        let center = (d as f32 + 0.5) * scale; // output centre in source space
        let left = (center - support).floor().max(0.0) as usize;
        let right = ((center + support).ceil() as usize).min(src_n as usize); // exclusive
        let mut ws: Vec<(usize, f32)> = Vec::new();
        let mut sum = 0.0f32;
        for s in left..right {
            let dist = ((s as f32 + 0.5) - center).abs() / support;
            let w = (1.0 - dist).max(0.0);
            if w > 0.0 {
                ws.push((s, w));
                sum += w;
            }
        }
        if sum > 0.0 {
            for x in &mut ws {
                x.1 /= sum;
            }
        } else {
            // Degenerate footprint — snap to the nearest source pixel.
            let s = (center as usize).min(src_n as usize - 1);
            ws.push((s, 1.0));
        }
        all.push(ws);
    }
    all
}

/// Resample `src` (`sw`×`sh` RGBA8, row-major, non-premultiplied) to `dw`×`dh`.
/// Returns the new RGBA buffer (`dw*dh*4`). Zero dims or a `src` length mismatch
/// yields an empty `Vec`.
pub fn resize_rgba(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() != (sw as usize * sh as usize * 4) {
        return Vec::new();
    }
    // 1. Premultiply alpha into f32 (RGB scaled by a/255, alpha kept 0..255).
    let n = sw as usize * sh as usize;
    let mut pm = vec![0f32; n * 4];
    for i in 0..n {
        let a = src[i * 4 + 3] as f32;
        let af = a / 255.0;
        pm[i * 4] = src[i * 4] as f32 * af;
        pm[i * 4 + 1] = src[i * 4 + 1] as f32 * af;
        pm[i * 4 + 2] = src[i * 4 + 2] as f32 * af;
        pm[i * 4 + 3] = a;
    }

    // 2. Horizontal pass: (sw × sh) → (dw × sh).
    let hw = axis_weights(sw, dw);
    let mut horiz = vec![0f32; dw as usize * sh as usize * 4];
    for y in 0..sh as usize {
        let src_row = y * sw as usize * 4;
        let dst_row = y * dw as usize * 4;
        for (dx, contribs) in hw.iter().enumerate() {
            let mut acc = [0f32; 4];
            for &(sx, w) in contribs {
                let p = src_row + sx * 4;
                acc[0] += pm[p] * w;
                acc[1] += pm[p + 1] * w;
                acc[2] += pm[p + 2] * w;
                acc[3] += pm[p + 3] * w;
            }
            let q = dst_row + dx * 4;
            horiz[q..q + 4].copy_from_slice(&acc);
        }
    }

    // 3. Vertical pass: (dw × sh) → (dw × dh).
    let vw = axis_weights(sh, dh);
    let mut out = vec![0u8; dw as usize * dh as usize * 4];
    for (dy, contribs) in vw.iter().enumerate() {
        let dst_row = dy * dw as usize * 4;
        for x in 0..dw as usize {
            let mut acc = [0f32; 4];
            for &(sy, w) in contribs {
                let p = (sy * dw as usize + x) * 4;
                acc[0] += horiz[p] * w;
                acc[1] += horiz[p + 1] * w;
                acc[2] += horiz[p + 2] * w;
                acc[3] += horiz[p + 3] * w;
            }
            // Unpremultiply + quantize.
            let a = acc[3];
            let inv = if a > 0.0 { 255.0 / a } else { 0.0 };
            let q = dst_row + x * 4;
            out[q] = (acc[0] * inv).round().clamp(0.0, 255.0) as u8;
            out[q + 1] = (acc[1] * inv).round().clamp(0.0, 255.0) as u8;
            out[q + 2] = (acc[2] * inv).round().clamp(0.0, 255.0) as u8;
            out[q + 3] = a.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_input() {
        assert!(resize_rgba(&[], 0, 0, 2, 2).is_empty());
        assert!(resize_rgba(&[0; 3], 1, 1, 2, 2).is_empty(), "length mismatch");
    }

    #[test]
    fn downscale_averages_a_2x2_to_1x1() {
        // Four opaque pixels: red, green, blue, white → average (191,191,127).
        let src = [
            255, 0, 0, 255, // R
            0, 255, 0, 255, // G
            0, 0, 255, 255, // B
            255, 255, 255, 255, // W
        ];
        let out = resize_rgba(&src, 2, 2, 1, 1);
        assert_eq!(out.len(), 4);
        // Mean R = (255+0+0+255)/4 = 127.5; G = 127.5; B = 127.5.
        for (c, &v) in out[..3].iter().enumerate() {
            assert!((v as i32 - 128).abs() <= 1, "channel {c} ≈ 128, got {v}");
        }
        assert_eq!(out[3], 255, "opaque");
    }

    #[test]
    fn upscale_preserves_dimensions_and_a_flat_colour() {
        let src = [10, 20, 30, 255]; // 1×1
        let out = resize_rgba(&src, 1, 1, 4, 3);
        assert_eq!(out.len(), 4 * 3 * 4);
        // A flat source stays flat after enlargement.
        for px in out.chunks_exact(4) {
            assert_eq!(px, [10, 20, 30, 255]);
        }
    }

    #[test]
    fn transparent_pixels_do_not_bleed_colour() {
        // One opaque red + one fully transparent (garbage RGB) → the resized
        // single pixel must stay red-ish, not muddied by the transparent RGB.
        let src = [255, 0, 0, 255, 0, 255, 0, 0]; // 2×1: red opaque, green transparent
        let out = resize_rgba(&src, 2, 1, 1, 1);
        assert!(out[0] > 200 && out[1] < 60, "stays red: {:?}", &out[..4]);
        assert_eq!(out[3], 128, "alpha averaged (255+0)/2");
    }
}
