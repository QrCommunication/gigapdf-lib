//! RC2 in CBC mode — RustCrypto [`rc2`] + [`cbc`]. Legacy PKCS#12 PBES1
//! (`pbeWithSHAAnd40BitRC2-CBC`) cert bags only; never new security. Raw CBC over
//! 8-byte blocks, no padding (the caller strips PKCS#7).

use cbc::cipher::generic_array::GenericArray;
use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, InnerIvInit};

/// Decrypt `data` (RC2-CBC) with `key`/`effective_bits` and an 8-byte `iv`.
/// `None` if the IV/data length is invalid. PKCS#7 padding is NOT removed here.
pub fn rc2_cbc_decrypt(
    key: &[u8],
    effective_bits: usize,
    iv: &[u8],
    data: &[u8],
) -> Option<Vec<u8>> {
    if iv.len() != 8 || data.is_empty() || !data.len().is_multiple_of(8) {
        return None;
    }
    let cipher = rc2::Rc2::new_with_eff_key_len(key, effective_bits);
    let dec = cbc::Decryptor::<rc2::Rc2>::inner_iv_init(cipher, GenericArray::from_slice(iv));
    let mut buf = data.to_vec();
    dec.decrypt_padded_mut::<NoPadding>(&mut buf).ok()?;
    Some(buf)
}
