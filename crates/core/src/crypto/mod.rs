//! Cryptographic primitives for the PDF standard security handler and digital
//! signatures — all implemented from scratch, zero dependencies.
//!
//! - [`md5`] / [`rc4`] — legacy RC4 handler (R2–R4) key derivation & cipher.
//! - [`aes`] — AES-128/256 CBC for the AESV2 (R4) and AESV3 (R5/R6) handlers.
//! - [`sha256`] — AES-256 key derivation and document hashing for signatures.

pub mod aes;
pub mod bignum;
pub mod des;
pub mod hmac;
pub mod kdf;
pub mod md5;
pub mod rc4;
pub mod rsa;
pub mod sha1;
pub mod sha256;
pub mod sha512;

pub use aes::{aes_cbc_decrypt, aes_cbc_encrypt, Aes};
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
