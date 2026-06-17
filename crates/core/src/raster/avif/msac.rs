//! AV1 multi-symbol arithmetic decoder (msac).
//!
//! Faithful translation of dav1d `src/msac.c` + `src/msac.h` (BSD-2-Clause,
//! © VideoLAN / Two Orioles). This is the entropy engine every AV1 tile symbol
//! flows through: a range coder over a 64-bit `dif` window with per-symbol
//! adaptive CDFs in Q15. No tables live here — CDFs are passed in by the caller.

#![allow(dead_code)]

const EC_PROB_SHIFT: u32 = 6;
const EC_MIN_PROB: u32 = 4; // must be <= (1<<EC_PROB_SHIFT)/16
const EC_WIN_SIZE: i32 = 64; // size_t window, in bits

/// Multi-symbol arithmetic decoder context over a byte slice.
pub(crate) struct Msac<'a> {
    buf: &'a [u8],
    pos: usize,
    dif: u64,
    rng: u32,
    cnt: i32,
    allow_update_cdf: bool,
}

impl<'a> Msac<'a> {
    /// `dav1d_msac_init`. `disable_cdf_update` freezes the adaptive CDFs.
    pub fn new(data: &'a [u8], disable_cdf_update: bool) -> Self {
        let mut s = Msac {
            buf: data,
            pos: 0,
            dif: 0,
            rng: 0x8000,
            cnt: -15,
            allow_update_cdf: !disable_cdf_update,
        };
        s.refill();
        s
    }

    /// Bytes pulled from the source so far (the decoder reads ahead into the
    /// `dif` window, so at end-of-stream this reaches `buf_len`).
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Total source length, for end-of-tile consumption checks.
    pub fn buf_len(&self) -> usize {
        self.buf.len()
    }

    /// Current range; must stay in `[0x8000, 0xFFFF]` for a healthy decode.
    pub fn rng(&self) -> u32 {
        self.rng
    }

    /// `ctx_refill`: pull bytes (inverted) into the high end of `dif`. On stream
    /// exhaustion the remaining low bits are set to 1.
    fn refill(&mut self) {
        let mut c: i32 = EC_WIN_SIZE - self.cnt - 24;
        let mut dif = self.dif;
        loop {
            if self.pos >= self.buf.len() {
                dif |= !(!0xffu64 << c);
                break;
            }
            dif |= ((self.buf[self.pos] ^ 0xff) as u64) << c;
            self.pos += 1;
            c -= 8;
            if c < 0 {
                break;
            }
        }
        self.dif = dif;
        self.cnt = EC_WIN_SIZE - c - 24;
    }

    /// `ctx_norm`: renormalize so `32768 <= rng < 65536`, refilling if needed.
    fn norm(&mut self, dif: u64, rng: u32) {
        let d = 15i32 ^ (31i32 ^ rng.leading_zeros() as i32);
        let cnt = self.cnt;
        self.dif = dif << d;
        self.rng = rng << d;
        self.cnt = cnt - d;
        // Unsigned compare avoids redundant refills at eob (cnt may be negative).
        if (cnt as u32) < (d as u32) {
            self.refill();
        }
    }

    /// Decode a single equiprobable (1/2) bit. `dav1d_msac_decode_bool_equi`.
    pub fn bool_equi(&mut self) -> u32 {
        let r = self.rng;
        let mut dif = self.dif;
        let mut v = ((r >> 8) << 7) + EC_MIN_PROB;
        let vw = (v as u64) << (EC_WIN_SIZE - 16);
        let ret = (dif >= vw) as u32;
        dif -= ret as u64 * vw;
        v = v.wrapping_add(ret.wrapping_mul(r.wrapping_sub(2u32.wrapping_mul(v))));
        self.norm(dif, v);
        1 - ret
    }

    /// Decode a bit with probability `f` (Q15) of being one. `dav1d_msac_decode_bool`.
    pub fn bool_p(&mut self, f: u32) -> u32 {
        let r = self.rng;
        let mut dif = self.dif;
        let mut v = (((r >> 8) * (f >> EC_PROB_SHIFT)) >> (7 - EC_PROB_SHIFT)) + EC_MIN_PROB;
        let vw = (v as u64) << (EC_WIN_SIZE - 16);
        let ret = (dif >= vw) as u32;
        dif -= ret as u64 * vw;
        v = v.wrapping_add(ret.wrapping_mul(r.wrapping_sub(2u32.wrapping_mul(v))));
        self.norm(dif, v);
        1 - ret
    }

    /// Decode an adaptive-CDF symbol. `cdf` is the inverse CDF in Q15 with the
    /// adaptation counter at `cdf[n_symbols]` (array length `n_symbols + 1`).
    /// Returns the symbol in `0..=n_symbols`. `dav1d_msac_decode_symbol_adapt_c`.
    pub fn symbol_adapt(&mut self, cdf: &mut [u16], n_symbols: usize) -> usize {
        let c = (self.dif >> (EC_WIN_SIZE - 16)) as u32;
        let r = self.rng >> 8;
        let mut v = self.rng;
        let mut u;
        let mut val = 0usize;
        loop {
            u = v;
            v = r * (cdf[val] as u32 >> EC_PROB_SHIFT);
            v >>= 7 - EC_PROB_SHIFT;
            v += EC_MIN_PROB * (n_symbols - val) as u32;
            if c >= v {
                break;
            }
            val += 1;
        }
        self.norm(self.dif - ((v as u64) << (EC_WIN_SIZE - 16)), u - v);

        if self.allow_update_cdf {
            let count = cdf[n_symbols] as u32;
            let rate = 4 + (count >> 4) + (n_symbols > 2) as u32;
            for c in cdf.iter_mut().take(val) {
                *c += ((32768 - *c as u32) >> rate) as u16;
            }
            for c in cdf.iter_mut().take(n_symbols).skip(val) {
                *c -= (*c as u32 >> rate) as u16;
            }
            cdf[n_symbols] = (count + (count < 32) as u32) as u16;
        }
        val
    }

    /// Decode an adaptive boolean (2-entry CDF: prob + counter).
    /// `dav1d_msac_decode_bool_adapt`.
    pub fn bool_adapt(&mut self, cdf: &mut [u16; 2]) -> u32 {
        let bit = self.bool_p(cdf[0] as u32);
        if self.allow_update_cdf {
            let count = cdf[1] as u32;
            let rate = 4 + (count >> 4);
            if bit != 0 {
                cdf[0] += ((32768 - cdf[0] as u32) >> rate) as u16;
            } else {
                cdf[0] -= (cdf[0] as u32 >> rate) as u16;
            }
            cdf[1] = (count + (count < 32) as u32) as u16;
        }
        bit
    }

    /// Read `n` equiprobable bits, MSB first. `dav1d_msac_decode_bools` / `L(n)`.
    pub fn bools(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bool_equi();
        }
        v
    }

    /// Decode a coefficient "hi token" (3..=15) via the 3-symbol base-range
    /// cascade on a shared CDF. `dav1d_msac_decode_hi_tok`.
    pub fn decode_hi_tok(&mut self, cdf: &mut [u16]) -> u32 {
        let mut tok_br = self.symbol_adapt(cdf, 3) as u32;
        let mut tok = 3 + tok_br;
        if tok_br == 3 {
            tok_br = self.symbol_adapt(cdf, 3) as u32;
            tok = 6 + tok_br;
            if tok_br == 3 {
                tok_br = self.symbol_adapt(cdf, 3) as u32;
                tok = 9 + tok_br;
                if tok_br == 3 {
                    tok = 12 + self.symbol_adapt(cdf, 3) as u32;
                }
            }
        }
        tok
    }

    /// Exp-Golomb residual extension. `read_golomb`: count leading zero
    /// equi-bits (capped at 32) to get the magnitude width, then read that many
    /// equi-bits as the mantissa. Used to extend a coefficient token of 15 to
    /// its full magnitude (AV1 spec §5.11.39 `coeffs`).
    pub fn golomb(&mut self) -> u32 {
        let mut len = 0u32;
        let mut val = 1u32;
        while self.bool_equi() == 0 && len < 32 {
            len += 1;
        }
        while len > 0 {
            len -= 1;
            val = (val << 1) + self.bool_equi();
        }
        val - 1
    }

    /// `NS(n)` non-symmetric uniform decode. `dav1d_msac_decode_uniform`.
    pub fn uniform(&mut self, n: u32) -> u32 {
        debug_assert!(n > 0);
        let l = n.ilog2() + 1; // ulog2(n) + 1
        let m = (1u32 << l) - n;
        let v = self.bools(l - 1);
        if v < m {
            v
        } else {
            (v << 1) - m + self.bool_equi()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msac_decodes_in_range_without_panic() {
        // Deterministic pseudo-data exercises refill/norm/bool/symbol paths. We
        // can't bit-validate without an AV1 encoder here (that happens
        // end-to-end vs the YUV reference); this guards range + shift-overflow.
        let data: Vec<u8> = (0..96u32)
            .map(|i| (i.wrapping_mul(73) ^ 0x5a) as u8)
            .collect();
        let mut s = Msac::new(&data, false);

        for _ in 0..40 {
            assert!(s.bool_equi() <= 1);
        }
        // 4-symbol adaptive CDF (inverse, Q15, decreasing) + counter at [3].
        let mut cdf = [28000u16, 18000, 9000, 0];
        for _ in 0..40 {
            let v = s.symbol_adapt(&mut cdf, 3);
            assert!(v <= 3, "symbol out of range: {v}");
        }
        for _ in 0..20 {
            assert!(s.bool_p(16384) <= 1);
        }
        for n in 2..12u32 {
            let v = s.uniform(n);
            assert!(v < n, "uniform {v} >= {n}");
        }
        // Adaptive boolean.
        let mut bcdf = [16384u16, 0];
        for _ in 0..20 {
            assert!(s.bool_adapt(&mut bcdf) <= 1);
        }
    }
}
