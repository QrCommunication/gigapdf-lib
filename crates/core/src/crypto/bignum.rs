//! Minimal big-unsigned-integer arithmetic for RSA — zero dependencies.
//!
//! Just enough for RSA key generation and signing: add/sub/mul, division with
//! remainder, modular exponentiation (square-and-multiply), modular inverse
//! (extended Euclid) and a Miller-Rabin primality test. Numbers are stored as
//! little-endian `u32` limbs. This is correctness-first, not constant-time — it
//! is used only for offline document signing, never to guard secrets at runtime.

use std::cmp::Ordering;

/// An arbitrary-precision non-negative integer (little-endian `u32` limbs, no
/// trailing zero limbs except for the value zero, represented as empty).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BigUint {
    limbs: Vec<u32>,
}

impl BigUint {
    /// Zero.
    pub fn zero() -> BigUint {
        BigUint { limbs: Vec::new() }
    }

    /// From a small value.
    pub fn from_u32(v: u32) -> BigUint {
        let mut b = BigUint { limbs: vec![v] };
        b.normalize();
        b
    }

    /// From big-endian bytes (as PDF/ASN.1 integers are written).
    pub fn from_bytes_be(bytes: &[u8]) -> BigUint {
        let mut limbs = Vec::new();
        let mut i = bytes.len();
        while i > 0 {
            let start = i.saturating_sub(4);
            let mut limb = 0u32;
            for &b in &bytes[start..i] {
                limb = (limb << 8) | b as u32;
            }
            limbs.push(limb);
            i = start;
        }
        let mut b = BigUint { limbs };
        b.normalize();
        b
    }

    /// Big-endian byte representation (no leading zeros; empty for zero).
    pub fn to_bytes_be(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for &limb in self.limbs.iter().rev() {
            out.extend_from_slice(&limb.to_be_bytes());
        }
        let first = out.iter().position(|&b| b != 0).unwrap_or(out.len());
        out.drain(..first);
        out
    }

    fn normalize(&mut self) {
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    /// Whether this is zero.
    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    fn is_odd(&self) -> bool {
        self.limbs.first().map(|l| l & 1 == 1).unwrap_or(false)
    }

    fn bit_len(&self) -> usize {
        match self.limbs.last() {
            Some(&top) => (self.limbs.len() - 1) * 32 + (32 - top.leading_zeros() as usize),
            None => 0,
        }
    }

    fn cmp_ref(&self, other: &BigUint) -> Ordering {
        if self.limbs.len() != other.limbs.len() {
            return self.limbs.len().cmp(&other.limbs.len());
        }
        for i in (0..self.limbs.len()).rev() {
            if self.limbs[i] != other.limbs[i] {
                return self.limbs[i].cmp(&other.limbs[i]);
            }
        }
        Ordering::Equal
    }

    /// Sum `self + other`.
    pub fn add(&self, other: &BigUint) -> BigUint {
        let mut out = Vec::with_capacity(self.limbs.len().max(other.limbs.len()) + 1);
        let mut carry = 0u64;
        for i in 0..self.limbs.len().max(other.limbs.len()) {
            let a = *self.limbs.get(i).unwrap_or(&0) as u64;
            let b = *other.limbs.get(i).unwrap_or(&0) as u64;
            let s = a + b + carry;
            out.push(s as u32);
            carry = s >> 32;
        }
        if carry != 0 {
            out.push(carry as u32);
        }
        let mut r = BigUint { limbs: out };
        r.normalize();
        r
    }

    /// `self - other`, assuming `self >= other`.
    pub fn sub(&self, other: &BigUint) -> BigUint {
        let mut out = Vec::with_capacity(self.limbs.len());
        let mut borrow = 0i64;
        for i in 0..self.limbs.len() {
            let a = self.limbs[i] as i64;
            let b = *other.limbs.get(i).unwrap_or(&0) as i64;
            let mut d = a - b - borrow;
            if d < 0 {
                d += 1 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            out.push(d as u32);
        }
        let mut r = BigUint { limbs: out };
        r.normalize();
        r
    }

    /// Product `self · other`.
    pub fn mul(&self, other: &BigUint) -> BigUint {
        if self.is_zero() || other.is_zero() {
            return BigUint::zero();
        }
        let mut out = vec![0u32; self.limbs.len() + other.limbs.len()];
        for (i, &a) in self.limbs.iter().enumerate() {
            let mut carry = 0u64;
            for (j, &b) in other.limbs.iter().enumerate() {
                let cur = out[i + j] as u64 + a as u64 * b as u64 + carry;
                out[i + j] = cur as u32;
                carry = cur >> 32;
            }
            out[i + other.limbs.len()] += carry as u32;
        }
        let mut r = BigUint { limbs: out };
        r.normalize();
        r
    }

    fn shl1(&self) -> BigUint {
        let mut out = Vec::with_capacity(self.limbs.len() + 1);
        let mut carry = 0u32;
        for &limb in &self.limbs {
            out.push((limb << 1) | carry);
            carry = limb >> 31;
        }
        if carry != 0 {
            out.push(carry);
        }
        let mut r = BigUint { limbs: out };
        r.normalize();
        r
    }

    fn set_bit(&mut self, bit: usize) {
        let limb = bit / 32;
        while self.limbs.len() <= limb {
            self.limbs.push(0);
        }
        self.limbs[limb] |= 1 << (bit % 32);
        self.normalize();
    }

    fn test_bit(&self, bit: usize) -> bool {
        let limb = bit / 32;
        self.limbs.get(limb).map(|l| l >> (bit % 32) & 1 == 1).unwrap_or(false)
    }

    /// Long division: returns `(quotient, remainder)`.
    fn divmod(&self, divisor: &BigUint) -> (BigUint, BigUint) {
        if divisor.is_zero() {
            return (BigUint::zero(), BigUint::zero());
        }
        if self.cmp_ref(divisor) == Ordering::Less {
            return (BigUint::zero(), self.clone());
        }
        let mut quotient = BigUint::zero();
        let mut remainder = BigUint::zero();
        for bit in (0..self.bit_len()).rev() {
            remainder = remainder.shl1();
            if self.test_bit(bit) {
                remainder.set_bit(0);
            }
            if remainder.cmp_ref(divisor) != Ordering::Less {
                remainder = remainder.sub(divisor);
                quotient.set_bit(bit);
            }
        }
        quotient.normalize();
        remainder.normalize();
        (quotient, remainder)
    }

    /// `self mod m`.
    pub fn rem(&self, m: &BigUint) -> BigUint {
        self.divmod(m).1
    }

    fn mod_mul(&self, other: &BigUint, m: &BigUint) -> BigUint {
        self.mul(other).rem(m)
    }

    /// Modular exponentiation `self^exp mod m` (square-and-multiply). Uses
    /// Montgomery multiplication for an odd modulus (the RSA/Miller-Rabin hot
    /// path), and the simple reduction otherwise.
    pub fn mod_pow(&self, exp: &BigUint, m: &BigUint) -> BigUint {
        if m.is_zero() {
            return BigUint::zero();
        }
        if m.is_odd() && m.limbs.len() > 1 {
            return self.mont_pow(exp, m);
        }
        self.mod_pow_simple(exp, m)
    }

    fn mod_pow_simple(&self, exp: &BigUint, m: &BigUint) -> BigUint {
        let mut result = BigUint::from_u32(1).rem(m);
        let mut acc = self.rem(m);
        for bit in 0..exp.bit_len() {
            if exp.test_bit(bit) {
                result = result.mod_mul(&acc, m);
            }
            acc = acc.mod_mul(&acc, m);
        }
        result
    }

    fn padded_limbs(&self, k: usize) -> Vec<u32> {
        let mut v = self.limbs.clone();
        v.resize(k, 0);
        v
    }

    /// Montgomery modular exponentiation for an odd modulus `m`.
    fn mont_pow(&self, exp: &BigUint, m: &BigUint) -> BigUint {
        let k = m.limbs.len();
        let n = m.limbs.clone();
        let np = mont_n_prime(n[0]);

        // r2 = R² mod m, with R = 2^(32k).
        let mut r2 = BigUint::zero();
        r2.set_bit(2 * 32 * k);
        let r2 = r2.rem(m).padded_limbs(k);

        let one = {
            let mut v = vec![0u32; k];
            v[0] = 1;
            v
        };
        let base = self.rem(m).padded_limbs(k);
        let mut acc = mont_mul(&base, &r2, &n, np); // base·R mod m (Montgomery base)
        let mut result = mont_mul(&one, &r2, &n, np); // 1·R mod m (Montgomery 1)

        // Right-to-left square-and-multiply: square the base each step, multiply
        // the result in when the exponent bit is set (LSB → MSB).
        for bit in 0..exp.bit_len() {
            if exp.test_bit(bit) {
                result = mont_mul(&result, &acc, &n, np);
            }
            acc = mont_mul(&acc, &acc, &n, np);
        }
        // Convert out of Montgomery form: result·1·R⁻¹ = result.
        let normal = mont_mul(&result, &one, &n, np);
        let mut out = BigUint { limbs: normal };
        out.normalize();
        out
    }

    /// Modular inverse `self^-1 mod m` via the extended Euclidean algorithm,
    /// or `None` when not invertible.
    pub fn mod_inverse(&self, m: &BigUint) -> Option<BigUint> {
        // Work with signed big integers via (value, negative) pairs.
        let (mut old_r, mut r) = (m.clone(), self.rem(m));
        let (mut old_s, mut s) = (Signed::zero(), Signed::one());
        while !r.is_zero() {
            let (q, rem) = old_r.divmod(&r);
            old_r = r;
            r = rem;
            let next = old_s.sub(&s.mul_uint(&q));
            old_s = s;
            s = next;
        }
        if old_r != BigUint::from_u32(1) {
            return None; // gcd != 1
        }
        Some(old_s.reduce(m))
    }
}

/// A small signed wrapper used only for the extended-Euclid coefficients.
#[derive(Clone)]
struct Signed {
    mag: BigUint,
    neg: bool,
}

impl Signed {
    fn zero() -> Signed {
        Signed { mag: BigUint::zero(), neg: false }
    }
    fn one() -> Signed {
        Signed { mag: BigUint::from_u32(1), neg: false }
    }
    fn mul_uint(&self, v: &BigUint) -> Signed {
        Signed { mag: self.mag.mul(v), neg: self.neg && !self.mag.is_zero() }
    }
    fn sub(&self, other: &Signed) -> Signed {
        // self - other
        if self.neg == other.neg {
            match self.mag.cmp_ref(&other.mag) {
                Ordering::Less => Signed { mag: other.mag.sub(&self.mag), neg: !self.neg },
                _ => Signed { mag: self.mag.sub(&other.mag), neg: self.neg },
            }
        } else {
            Signed { mag: self.mag.add(&other.mag), neg: self.neg }
        }
    }
    fn reduce(&self, m: &BigUint) -> BigUint {
        let r = self.mag.rem(m);
        if self.neg && !r.is_zero() {
            m.sub(&r)
        } else {
            r
        }
    }
}

/// `-n0⁻¹ mod 2³²`, the Montgomery constant (`n0` must be odd). Newton's method
/// doubles the number of correct bits each step: 5 steps cover 32 bits.
fn mont_n_prime(n0: u32) -> u32 {
    let mut inv = 1u32;
    for _ in 0..5 {
        inv = inv.wrapping_mul(2u32.wrapping_sub(n0.wrapping_mul(inv)));
    }
    inv.wrapping_neg()
}

fn cmp_limbs(a: &[u32], b: &[u32]) -> Ordering {
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

fn sub_limbs(a: &mut [u32], b: &[u32]) {
    let mut borrow = 0i64;
    for (i, ai) in a.iter_mut().enumerate() {
        let d = *ai as i64 - *b.get(i).unwrap_or(&0) as i64 - borrow;
        if d < 0 {
            *ai = (d + (1 << 32)) as u32;
            borrow = 1;
        } else {
            *ai = d as u32;
            borrow = 0;
        }
    }
}

/// Montgomery product `a·b·R⁻¹ mod n` (CIOS), with `a, b, n` all `s` limbs and
/// `np = -n⁻¹ mod 2³²`. Result is `< n`, `s` limbs.
fn mont_mul(a: &[u32], b: &[u32], n: &[u32], np: u32) -> Vec<u32> {
    let s = n.len();
    let mut t = vec![0u32; s + 2];
    for &bi in b.iter().take(s) {
        let bi = bi as u64;
        // t += a * bi
        let mut c: u64 = 0;
        for j in 0..s {
            let v = t[j] as u64 + a[j] as u64 * bi + c;
            t[j] = v as u32;
            c = v >> 32;
        }
        let v = t[s] as u64 + c;
        t[s] = v as u32;
        t[s + 1] = (v >> 32) as u32;
        // m = t[0]·np mod 2³²; t = (t + m·n) >> 32 (low limb cancels)
        let m = (t[0] as u64 * np as u64) as u32 as u64;
        let v = t[0] as u64 + m * n[0] as u64;
        c = v >> 32;
        for j in 1..s {
            let v = t[j] as u64 + m * n[j] as u64 + c;
            t[j - 1] = v as u32;
            c = v >> 32;
        }
        let v = t[s] as u64 + c;
        t[s - 1] = v as u32;
        c = v >> 32;
        t[s] = t[s + 1] + c as u32;
        t[s + 1] = 0;
    }
    // t is s+1 limbs and < 2n; conditional final subtraction.
    let mut res = t[0..s].to_vec();
    if t[s] != 0 || cmp_limbs(&res, n) != Ordering::Less {
        sub_limbs(&mut res, n);
    }
    res
}

/// Miller-Rabin primality test with the given deterministic witnesses.
pub fn is_probable_prime(n: &BigUint, witnesses: &[u32]) -> bool {
    let one = BigUint::from_u32(1);
    let two = BigUint::from_u32(2);
    if n.cmp_ref(&two) == Ordering::Less {
        return false;
    }
    if !n.is_odd() {
        return *n == two;
    }
    // n - 1 = 2^r · d
    let n_minus_1 = n.sub(&one);
    let mut d = n_minus_1.clone();
    let mut r = 0usize;
    while !d.is_odd() {
        d = d.divmod(&two).0;
        r += 1;
    }
    'witness: for &w in witnesses {
        let a = BigUint::from_u32(w);
        if a.cmp_ref(n) != Ordering::Less || a.is_zero() {
            continue;
        }
        let mut x = a.mod_pow(&d, n);
        if x == one || x == n_minus_1 {
            continue;
        }
        for _ in 0..r.saturating_sub(1) {
            x = x.mod_mul(&x, n);
            if x == n_minus_1 {
                continue 'witness;
            }
        }
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_round_trip() {
        let b = BigUint::from_bytes_be(&[0x01, 0x23, 0x45, 0x67, 0x89]);
        assert_eq!(b.to_bytes_be(), vec![0x01, 0x23, 0x45, 0x67, 0x89]);
    }

    #[test]
    fn mul_and_divmod() {
        let a = BigUint::from_u32(123456789);
        let b = BigUint::from_u32(987654321);
        let p = a.mul(&b);
        let (q, rem) = p.divmod(&b);
        assert_eq!(q, a);
        assert!(rem.is_zero());
    }

    #[test]
    fn mod_pow_small() {
        // 7^13 mod 11 = 2.
        let r = BigUint::from_u32(7).mod_pow(&BigUint::from_u32(13), &BigUint::from_u32(11));
        assert_eq!(r, BigUint::from_u32(2));
    }

    #[test]
    fn montgomery_matches_simple_path() {
        // A large odd modulus exercises the Montgomery path; cross-check it
        // against the verified simple reduction on several bases/exponents.
        let m = BigUint::from_bytes_be(&[
            0xC0, 0x4F, 0x9A, 0x33, 0x71, 0x8E, 0xEF, 0x2B, 0x15, 0x77, 0xD3, 0x09, 0xBB, 0x42,
            0x6D, 0x01, 0xA5, 0xF8, 0x3C, 0x7E, 0x91, 0x2A, 0x6B, 0xCD, 0xEF, 0x13, 0x57, 0x99,
            0x01, 0x23, 0x45, 0x6B,
        ]);
        assert!(m.is_odd() && m.limbs.len() > 1);
        for base in [3u32, 65537, 0x1234_5678, 0xDEAD_BEEF] {
            for exp in [17u32, 65537, 0x0010_0001] {
                let b = BigUint::from_u32(base);
                let e = BigUint::from_u32(exp);
                assert_eq!(
                    b.mont_pow(&e, &m),
                    b.mod_pow_simple(&e, &m),
                    "base={base} exp={exp}"
                );
            }
        }
    }

    #[test]
    fn mod_inverse_small() {
        // 3^-1 mod 11 = 4 (3*4 = 12 ≡ 1).
        let inv = BigUint::from_u32(3).mod_inverse(&BigUint::from_u32(11)).unwrap();
        assert_eq!(inv, BigUint::from_u32(4));
    }

    #[test]
    fn montgomery_primality_path() {
        // 2³²+15 = 4294967311 is prime (2 limbs → Montgomery mod_pow path).
        let p = BigUint::from_bytes_be(&[0x01, 0x00, 0x00, 0x00, 0x0F]);
        assert!(p.is_odd() && p.limbs.len() > 1);
        assert!(is_probable_prime(&p, &[2, 3, 5, 7, 11, 13]), "known prime");
        // 2³²+9 ends in 5 → divisible by 5 → composite.
        let c = BigUint::from_bytes_be(&[0x01, 0x00, 0x00, 0x00, 0x09]);
        assert!(!is_probable_prime(&c, &[2, 3, 5, 7, 11, 13]), "known composite");
    }

    #[test]
    fn montgomery_large_exponent() {
        let m = BigUint::from_bytes_be(&[
            0xC0, 0x4F, 0x9A, 0x33, 0x71, 0x8E, 0xEF, 0x2B, 0x15, 0x77, 0xD3, 0x09, 0xBB, 0x42,
            0x6D, 0x01,
        ]);
        let base = BigUint::from_u32(0xDEAD_BEEF);
        let exp = m.sub(&BigUint::from_u32(1)); // ~128-bit exponent
        assert_eq!(base.mont_pow(&exp, &m), base.mod_pow_simple(&exp, &m));
    }

    #[test]
    fn primality() {
        let witnesses = [2u32, 3, 5, 7, 11, 13];
        assert!(is_probable_prime(&BigUint::from_u32(97), &witnesses));
        assert!(is_probable_prime(&BigUint::from_u32(7919), &witnesses));
        assert!(!is_probable_prime(&BigUint::from_u32(99), &witnesses));
        assert!(!is_probable_prime(&BigUint::from_u32(7917), &witnesses));
    }
}
