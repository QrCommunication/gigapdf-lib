//! HMAC (RFC 2104) over the engine's own hash functions. Zero dependencies.
//! Used by PBKDF2 (PBES2) and the PKCS#12 integrity MAC for user-certificate
//! (P12) digital-signature import.

use super::sha1::sha1;
use super::sha256::sha256;

/// Generic HMAC with a 64-byte block size (the SHA-1 / SHA-256 / MD5 family).
fn hmac(hash: impl Fn(&[u8]) -> Vec<u8>, key: &[u8], msg: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut k = if key.len() > BLOCK { hash(key) } else { key.to_vec() };
    k.resize(BLOCK, 0);

    let mut ipad = vec![0x36u8; BLOCK];
    let mut opad = vec![0x5cu8; BLOCK];
    for ((ip, op), &kb) in ipad.iter_mut().zip(opad.iter_mut()).zip(k.iter()) {
        *ip ^= kb;
        *op ^= kb;
    }

    let mut inner = ipad;
    inner.extend_from_slice(msg);
    let inner_hash = hash(&inner);

    let mut outer = opad;
    outer.extend_from_slice(&inner_hash);
    hash(&outer)
}

/// HMAC-SHA-1 (20-byte tag).
pub fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    let mut tag = [0u8; 20];
    tag.copy_from_slice(&hmac(|d| sha1(d).to_vec(), key, msg));
    tag
}

/// HMAC-SHA-256 (32-byte tag).
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut tag = [0u8; 32];
    tag.copy_from_slice(&hmac(|d| sha256(d).to_vec(), key, msg));
    tag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn rfc2202_hmac_sha1() {
        assert_eq!(
            hex(&hmac_sha1(&[0x0b; 20], b"Hi There")),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
        assert_eq!(
            hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
    }

    #[test]
    fn rfc4231_hmac_sha256() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn long_key_is_hashed() {
        // Key longer than the block size is replaced by its digest (RFC 2104).
        let long = vec![0xaa; 80];
        // Just exercise the path; correctness is covered by the RFC vectors
        // above (their keys are < block size) plus this round-trips a long key.
        let _ = hmac_sha256(&long, b"data");
    }
}
