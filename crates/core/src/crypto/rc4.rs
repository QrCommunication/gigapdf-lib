//! RC4 stream cipher. Zero dependencies. Used by the legacy RC4 PDF security
//! handler (R2/R3). Symmetric: the same call encrypts and decrypts.

/// RC4-transform `data` under `key`. With an empty key the data is returned
/// unchanged (there is no meaningful keystream).
pub fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return data.to_vec();
    }

    let mut s: [u8; 256] = core::array::from_fn(|i| i as u8);
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }

    let mut out = Vec::with_capacity(data.len());
    let mut i: u8 = 0;
    let mut j: u8 = 0;
    for &byte in data {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[s[i as usize].wrapping_add(s[j as usize]) as usize];
        out.push(byte ^ k);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn known_vectors() {
        // RFC 6229 / classic test vectors.
        assert_eq!(hex(&rc4(b"Key", b"Plaintext")), "bbf316e8d940af0ad3");
        assert_eq!(hex(&rc4(b"Wiki", b"pedia")), "1021bf0420");
    }

    #[test]
    fn symmetric_round_trip() {
        let key = b"secret-key";
        let data = b"the background must be preserved";
        let cipher = rc4(key, data);
        assert_eq!(rc4(key, &cipher), data);
    }
}
