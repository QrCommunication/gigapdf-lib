//! Triple-DES (EDE3) in CBC mode — RustCrypto [`des`] + [`cbc`]. Legacy PKCS#12
//! PBES1 (`pbeWithSHAAnd3-KeyTripleDES-CBC`) key bags only; never new security.
//! Raw CBC over 8-byte blocks, no padding (the caller strips PKCS#7).

use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, BlockEncryptMut, KeyIvInit};

type Tdes3CbcEnc = cbc::Encryptor<des::TdesEde3>;
type Tdes3CbcDec = cbc::Decryptor<des::TdesEde3>;

/// 3DES-CBC encrypt. `key` 24 bytes, `iv` 8 bytes, `data` a multiple of 8.
/// `None` on a bad key/IV/length.
pub fn des3_cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if key.len() != 24 || iv.len() != 8 || !data.len().is_multiple_of(8) {
        return None;
    }
    let mut buf = data.to_vec();
    let n = buf.len();
    Tdes3CbcEnc::new_from_slices(key, iv)
        .ok()?
        .encrypt_padded_mut::<NoPadding>(&mut buf, n)
        .ok()?;
    Some(buf)
}

/// 3DES-CBC decrypt. PKCS#7 padding is NOT removed here.
pub fn des3_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    if key.len() != 24 || iv.len() != 8 || !data.len().is_multiple_of(8) {
        return None;
    }
    let mut buf = data.to_vec();
    Tdes3CbcDec::new_from_slices(key, iv)
        .ok()?
        .decrypt_padded_mut::<NoPadding>(&mut buf)
        .ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbc_round_trip() {
        let key = [0x07u8; 24];
        let iv = [0x03u8; 8];
        let data = b"8bytes!!8bytes!!"; // 16 bytes, two blocks
        let ct = des3_cbc_encrypt(&key, &iv, data).unwrap();
        assert_eq!(des3_cbc_decrypt(&key, &iv, &ct).unwrap(), data);
    }

    #[test]
    fn rejects_bad_lengths() {
        assert!(des3_cbc_encrypt(&[0; 16], &[0; 8], &[0; 8]).is_none());
        assert!(des3_cbc_decrypt(&[0; 24], &[0; 8], &[0; 7]).is_none());
    }
}
