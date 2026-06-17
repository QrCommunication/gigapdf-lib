//! AES-128 / AES-256 in CBC mode — RustCrypto [`aes`] + [`cbc`] (constant-time,
//! using AES-NI where available). Raw CBC over whole 16-byte blocks with **no
//! padding**: the PDF security handler and PKCS#12 PBES2 manage their own
//! padding, and a trailing partial block is dropped (the engine's prior
//! behaviour). The crate name is referenced as `::aes` to disambiguate from this
//! module.

use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, BlockEncryptMut, KeyIvInit};

type Aes128CbcEnc = cbc::Encryptor<::aes::Aes128>;
type Aes256CbcEnc = cbc::Encryptor<::aes::Aes256>;
type Aes128CbcDec = cbc::Decryptor<::aes::Aes128>;
type Aes256CbcDec = cbc::Decryptor<::aes::Aes256>;

/// AES-CBC encrypt (no padding). Key length selects AES-128 (16 bytes) or
/// AES-256 (32); any other length, or a partial trailing block, yields no extra
/// output. Returns the ciphertext for the whole blocks of `data`.
pub fn aes_cbc_encrypt(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let n = data.len() / 16 * 16;
    let mut buf = data[..n].to_vec();
    let ok = match key.len() {
        16 => Aes128CbcEnc::new_from_slices(key, iv)
            .map(|c| {
                let _ = c.encrypt_padded_mut::<NoPadding>(&mut buf, n);
            })
            .is_ok(),
        32 => Aes256CbcEnc::new_from_slices(key, iv)
            .map(|c| {
                let _ = c.encrypt_padded_mut::<NoPadding>(&mut buf, n);
            })
            .is_ok(),
        _ => false,
    };
    if ok {
        buf
    } else {
        Vec::new()
    }
}

/// AES-CBC decrypt (no padding removal); mirrors [`aes_cbc_encrypt`].
pub fn aes_cbc_decrypt(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let n = data.len() / 16 * 16;
    let mut buf = data[..n].to_vec();
    let ok = match key.len() {
        16 => Aes128CbcDec::new_from_slices(key, iv)
            .map(|c| {
                let _ = c.decrypt_padded_mut::<NoPadding>(&mut buf);
            })
            .is_ok(),
        32 => Aes256CbcDec::new_from_slices(key, iv)
            .map(|c| {
                let _ = c.decrypt_padded_mut::<NoPadding>(&mut buf);
            })
            .is_ok(),
        _ => false,
    };
    if ok {
        buf
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbc_round_trip_128_and_256() {
        let iv = [0x24u8; 16];
        let data = b"sixteen bytes!!!sixteen bytes!!!"; // 32 bytes, two blocks
        for key in [vec![0x42u8; 16], vec![0x42u8; 32]] {
            let enc = aes_cbc_encrypt(&key, &iv, data);
            assert_eq!(enc.len(), 32);
            assert_ne!(&enc[..], &data[..]);
            assert_eq!(aes_cbc_decrypt(&key, &iv, &enc), data);
        }
    }

    #[test]
    fn nist_aes128_cbc_first_block() {
        // NIST SP 800-38A F.2.1 AES-128-CBC, first block.
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let iv = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let pt = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];
        let ct = aes_cbc_encrypt(&key, &iv, &pt);
        let expected = [
            0x76, 0x49, 0xab, 0xac, 0x81, 0x19, 0xb2, 0x46, 0xce, 0xe9, 0x8e, 0x9b, 0x12, 0xe9,
            0x19, 0x7d,
        ];
        assert_eq!(ct, expected);
    }
}
