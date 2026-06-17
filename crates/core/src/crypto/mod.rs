//! Cryptographic primitives for the PDF standard security handler and digital
//! signatures — all implemented from scratch, zero dependencies.
//!
//! - [`md5`] / [`rc4`] — legacy RC4 handler (R2–R4) key derivation & cipher.
//! - [`aes`] — AES-128/256 CBC for the AESV2 (R4) and AESV3 (R5/R6) handlers.
//! - [`sha256`] — AES-256 key derivation and document hashing for signatures.

pub mod aes;
pub mod des;
pub mod hmac;
pub mod kdf;
pub mod md5;
pub mod rc2;
pub mod rc4;
pub mod rsa;
pub mod sha1;
pub mod sha256;
pub mod sha512;

pub use aes::{aes_cbc_decrypt, aes_cbc_encrypt};
pub use des::{des3_cbc_decrypt, des3_cbc_encrypt};
pub use hmac::{hmac_sha1, hmac_sha256};
pub use kdf::{
    bmp_string, pbkdf2_hmac_sha1, pbkdf2_hmac_sha256, pkcs12_kdf_sha1, pkcs12_kdf_sha256,
};
pub use md5::md5;
pub use rc4::rc4;
pub use sha1::sha1;
pub use sha256::sha256;
pub use sha512::{sha384, sha512};

#[cfg(test)]
mod foundation_smoke {
    //! Guards the audited-crypto + Boa dependency foundation: proves `sha2` +
    //! `boa_engine` run and `rsa` links — native, and (via `cargo wasm`) on
    //! wasm32 with the `wasm_js` getrandom backend. This is the base the crypto
    //! and JS migrations build on; the hand-rolled primitives are retired as
    //! each consumer (signing, the PDF security handler, inline-script HTML)
    //! moves over.
    #[test]
    fn audited_crypto_and_boa_are_available() {
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(b"abc");
        assert_eq!(h.len(), 32);
        assert_eq!(h[0], 0xba, "SHA-256(\"abc\") starts ba78…");

        let mut ctx = boa_engine::Context::default();
        let v = ctx.eval(boa_engine::Source::from_bytes(b"40 + 2")).unwrap();
        assert_eq!(v.as_number(), Some(42.0));

        // Link-check the RSA types (no slow keygen in the spike).
        let _ = core::mem::size_of::<rsa::RsaPrivateKey>();
    }
}
