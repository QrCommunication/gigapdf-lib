//! RSA key generation and RSASSA-PKCS#1 v1.5 (SHA-256) signing — backed by the
//! audited RustCrypto [`rsa`] crate (constant-time modular exponentiation with
//! base blinding), wrapped in the engine's [`RsaPrivateKey`] facade so the PDF
//! signing / PKCS#12 glue stays implementation-agnostic.
//!
//! Used for engine-managed (self-signed) and imported (PKCS#12) PDF signatures.
//! Key generation is seeded from host-supplied randomness; signing draws its
//! blinding entropy from the platform RNG (the wasm host's
//! `crypto.getRandomValues`, via getrandom).

use rand::rngs::{OsRng, StdRng};
use rand::SeedableRng;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding};
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, RsaPrivateKey as RustRsaPrivateKey};
use sha2::Sha256;

/// An RSA private key. The public modulus/exponent are exposed as big-endian
/// bytes for the certificate / `SignerInfo` encoders; the underlying audited key
/// is reachable via [`RsaPrivateKey::inner`] for the `x509-cert` / `cms` builders.
#[derive(Debug, Clone)]
pub struct RsaPrivateKey {
    inner: RustRsaPrivateKey,
    /// Modulus size in bytes.
    pub modulus_len: usize,
}

impl RsaPrivateKey {
    /// Generate a `bits`-bit RSA key, seeding a CSPRNG from host `rand` (needs
    /// ≥ 32 bytes). `None` if `rand` is too short or generation fails.
    pub fn generate(bits: usize, rand: &[u8]) -> Option<RsaPrivateKey> {
        if rand.len() < 32 {
            return None;
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&rand[..32]);
        let mut rng = StdRng::from_seed(seed);
        let inner = RustRsaPrivateKey::new(&mut rng, bits).ok()?;
        let modulus_len = inner.size();
        Some(RsaPrivateKey { inner, modulus_len })
    }

    /// Rebuild a key from a PKCS#1 `RSAPrivateKey` DER (the key bytes recovered
    /// from a `.p12`). Keeps the CRT factors for correct, fast signing.
    pub fn from_pkcs1_der(der: &[u8]) -> Option<RsaPrivateKey> {
        let inner = RustRsaPrivateKey::from_pkcs1_der(der).ok()?;
        let modulus_len = inner.size();
        Some(RsaPrivateKey { inner, modulus_len })
    }

    /// Rebuild from `(n, e, d)` big-endian magnitudes, recovering the CRT
    /// factors. `None` if the components are inconsistent.
    pub fn from_components(n_be: &[u8], e_be: &[u8], d_be: &[u8]) -> Option<RsaPrivateKey> {
        let n = BigUint::from_bytes_be(n_be);
        let e = BigUint::from_bytes_be(e_be);
        let d = BigUint::from_bytes_be(d_be);
        let inner = RustRsaPrivateKey::from_components(n, e, d, Vec::new()).ok()?;
        let modulus_len = inner.size();
        Some(RsaPrivateKey { inner, modulus_len })
    }

    /// Public modulus `n` as big-endian bytes.
    pub fn n_bytes_be(&self) -> Vec<u8> {
        self.inner.n().to_bytes_be()
    }

    /// Public exponent `e` as big-endian bytes.
    pub fn e_bytes_be(&self) -> Vec<u8> {
        self.inner.e().to_bytes_be()
    }

    /// The underlying RustCrypto key (for the `x509-cert` / `cms` builders).
    pub fn inner(&self) -> &RustRsaPrivateKey {
        &self.inner
    }

    /// RSASSA-PKCS#1 v1.5 (SHA-256) signature — `modulus_len` big-endian bytes,
    /// blinded against timing side-channels via the platform RNG.
    pub fn sign_sha256(&self, message: &[u8]) -> Vec<u8> {
        let signing_key = SigningKey::<Sha256>::new(self.inner.clone());
        signing_key.sign_with_rng(&mut OsRng, message).to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;

    #[test]
    fn small_key_signs_and_verifies() {
        // 512-bit keeps the test fast while exercising keygen + blinded signing.
        let rand: Vec<u8> = (0..64).map(|i| (i * 37 + 11) as u8).collect();
        let key = RsaPrivateKey::generate(512, &rand).expect("key");
        let msg = b"GigaPDF self-signed";
        let sig = key.sign_sha256(msg);
        assert_eq!(sig.len(), key.modulus_len);

        let vk = VerifyingKey::<Sha256>::new(key.inner.to_public_key());
        let signature = Signature::try_from(sig.as_slice()).expect("sig parses");
        vk.verify(msg, &signature).expect("RustCrypto verifies our signature");
    }

    #[test]
    fn rejects_short_randomness() {
        assert!(RsaPrivateKey::generate(512, &[0u8; 8]).is_none());
    }
}
