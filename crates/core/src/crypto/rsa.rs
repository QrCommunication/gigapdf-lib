//! RSA key generation and RSASSA-PKCS#1 v1.5 signing over SHA-256 — zero
//! dependencies, built on [`super::bignum`].
//!
//! Used only for offline, engine-managed (self-signed) PDF signatures, never to
//! protect secrets at runtime. Randomness is supplied by the host (the WASM host
//! has `crypto.getRandomValues`); this module never invents entropy.

use super::bignum::{is_probable_prime, BigUint};
use super::sha256::sha256;

/// An RSA private key (and its public modulus/exponent).
#[derive(Debug, Clone)]
pub struct RsaPrivateKey {
    /// Public modulus `n = p·q`.
    pub n: BigUint,
    /// Public exponent (65537).
    pub e: BigUint,
    /// Private exponent `d = e^-1 mod lcm(p-1, q-1)`.
    pub d: BigUint,
    /// Modulus size in bytes.
    pub modulus_len: usize,
}

const MR_WITNESSES: [u32; 9] = [2, 3, 5, 7, 11, 13, 17, 19, 23];

/// Turn a stream of random bytes into an odd candidate prime of `bits` bits with
/// the two top bits set (so the product of two has the full modulus size).
fn candidate_from(bytes: &[u8], bits: usize) -> BigUint {
    let mut v = BigUint::from_bytes_be(bytes);
    // Force exact bit length: keep only the low `bits`, set top two + bottom bit.
    let byte_len = bits / 8;
    let mut raw = v.to_bytes_be();
    if raw.len() > byte_len {
        raw = raw[raw.len() - byte_len..].to_vec();
    } else {
        let mut padded = vec![0u8; byte_len - raw.len()];
        padded.extend_from_slice(&raw);
        raw = padded;
    }
    if let Some(first) = raw.first_mut() {
        *first |= 0b1100_0000; // top two bits
    }
    if let Some(last) = raw.last_mut() {
        *last |= 1; // odd
    }
    v = BigUint::from_bytes_be(&raw);
    v
}

fn next_prime(mut candidate: BigUint) -> BigUint {
    let two = BigUint::from_u32(2);
    loop {
        if is_probable_prime(&candidate, &MR_WITNESSES) {
            return candidate;
        }
        candidate = candidate.add(&two);
    }
}

impl RsaPrivateKey {
    /// Generate a `bits`-bit RSA key from `rand` (need ≈ `bits/4` random bytes;
    /// supply more for safety). Returns `None` if `rand` is too short.
    pub fn generate(bits: usize, rand: &[u8]) -> Option<RsaPrivateKey> {
        let half = bits / 2;
        let half_bytes = half / 8;
        if rand.len() < half_bytes * 2 {
            return None;
        }
        let e = BigUint::from_u32(65537);

        let one = BigUint::from_u32(1);
        let p = next_prime(candidate_from(&rand[..half_bytes], half));
        let q = next_prime(candidate_from(&rand[half_bytes..half_bytes * 2], half));

        let n = p.mul(&q);
        let phi = p.sub(&one).mul(&q.sub(&one));
        let d = e.mod_inverse(&phi)?;
        let modulus_len = n.to_bytes_be().len();

        Some(RsaPrivateKey {
            n,
            e,
            d,
            modulus_len,
        })
    }

    /// RSASSA-PKCS#1 v1.5 signature of `message` using SHA-256. Returns the
    /// `modulus_len`-byte big-endian signature.
    pub fn sign_sha256(&self, message: &[u8]) -> Vec<u8> {
        let digest = sha256(message);
        // DigestInfo prefix for SHA-256 (RFC 8017 §9.2).
        const PREFIX: [u8; 19] = [
            0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
            0x01, 0x05, 0x00, 0x04, 0x20,
        ];
        let mut t = PREFIX.to_vec();
        t.extend_from_slice(&digest);

        // EM = 0x00 || 0x01 || PS (0xff…) || 0x00 || T
        let k = self.modulus_len;
        let mut em = vec![0x00, 0x01];
        em.resize(k - t.len() - 1, 0xFF);
        em.push(0x00);
        em.extend_from_slice(&t);

        let m = BigUint::from_bytes_be(&em);
        let sig = m.mod_pow(&self.d, &self.n);
        let mut out = sig.to_bytes_be();
        while out.len() < k {
            out.insert(0, 0); // left-pad to modulus length
        }
        out
    }

    /// Verify a signature (used by tests) — `s^e mod n` should rebuild `EM`.
    pub fn public_recover(&self, signature: &[u8]) -> Vec<u8> {
        BigUint::from_bytes_be(signature)
            .mod_pow(&self.e, &self.n)
            .to_bytes_be()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_key_signs_and_verifies() {
        // A 512-bit key is plenty to exercise the math in a unit test.
        let rand: Vec<u8> = (0..256).map(|i| (i * 37 + 11) as u8).collect();
        let key = RsaPrivateKey::generate(512, &rand).expect("key");
        let sig = key.sign_sha256(b"GigaPDF self-signed");
        assert_eq!(sig.len(), key.modulus_len);

        // s^e mod n must rebuild the padded DigestInfo (ends with the digest).
        let recovered = key.public_recover(&sig);
        let digest = sha256(b"GigaPDF self-signed");
        assert!(
            recovered.ends_with(&digest),
            "recovered EM ends with the SHA-256 digest"
        );
    }
}
