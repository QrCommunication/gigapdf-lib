//! AES-128 and AES-256 (FIPS-197) with CBC mode. Zero dependencies.
//!
//! The S-box and its inverse are *computed* from the GF(2⁸) definition rather
//! than transcribed as 512 literal bytes — the construction matches the spec by
//! design, so there is no risk of a copy error (the FIPS-197 known-answer test
//! pins it down regardless).

/// Multiply two GF(2⁸) elements modulo the AES polynomial `x⁸+x⁴+x³+x+1`.
fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// Multiplicative inverse in GF(2⁸) (`0` maps to `0`), by exhaustive search.
fn gf_inv(a: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    (1u8..=255).find(|&b| gmul(a, b) == 1).unwrap_or(0)
}

fn build_sbox() -> [u8; 256] {
    let mut sbox = [0u8; 256];
    for (a, slot) in sbox.iter_mut().enumerate() {
        let inv = gf_inv(a as u8);
        // Affine transform: s ⊕ rotl(s,1) ⊕ rotl(s,2) ⊕ rotl(s,3) ⊕ rotl(s,4) ⊕ 0x63.
        let s = inv
            ^ inv.rotate_left(1)
            ^ inv.rotate_left(2)
            ^ inv.rotate_left(3)
            ^ inv.rotate_left(4)
            ^ 0x63;
        *slot = s;
    }
    sbox
}

fn invert_table(sbox: &[u8; 256]) -> [u8; 256] {
    let mut inv = [0u8; 256];
    for (i, &s) in sbox.iter().enumerate() {
        inv[s as usize] = i as u8;
    }
    inv
}

/// An AES cipher with a precomputed key schedule and S-boxes.
#[derive(Debug, Clone)]
pub struct Aes {
    round_keys: Vec<[u8; 16]>,
    sbox: [u8; 256],
    inv_sbox: [u8; 256],
    rounds: usize,
}

impl Aes {
    /// Build a cipher from a 16-byte (AES-128) or 32-byte (AES-256) key.
    pub fn new(key: &[u8]) -> Aes {
        let nk = key.len() / 4; // 4 or 8
        let rounds = nk + 6; // 10 or 14
        let total_words = 4 * (rounds + 1);
        let sbox = build_sbox();
        let inv_sbox = invert_table(&sbox);

        let mut words: Vec<[u8; 4]> = Vec::with_capacity(total_words);
        for i in 0..nk {
            words.push([key[i * 4], key[i * 4 + 1], key[i * 4 + 2], key[i * 4 + 3]]);
        }
        let mut rcon: u8 = 1;
        for i in nk..total_words {
            let mut temp = words[i - 1];
            if i % nk == 0 {
                temp.rotate_left(1); // RotWord
                for b in &mut temp {
                    *b = sbox[*b as usize]; // SubWord
                }
                temp[0] ^= rcon;
                rcon = gmul(rcon, 2);
            } else if nk > 6 && i % nk == 4 {
                for b in &mut temp {
                    *b = sbox[*b as usize];
                }
            }
            let prev = words[i - nk];
            words.push([
                prev[0] ^ temp[0],
                prev[1] ^ temp[1],
                prev[2] ^ temp[2],
                prev[3] ^ temp[3],
            ]);
        }

        let round_keys = words
            .chunks_exact(4)
            .map(|w| {
                let mut rk = [0u8; 16];
                for (col, word) in w.iter().enumerate() {
                    rk[col * 4..col * 4 + 4].copy_from_slice(word);
                }
                rk
            })
            .collect();

        Aes {
            round_keys,
            sbox,
            inv_sbox,
            rounds,
        }
    }

    fn add_round_key(state: &mut [u8; 16], rk: &[u8; 16]) {
        for (s, k) in state.iter_mut().zip(rk) {
            *s ^= k;
        }
    }

    fn sub_bytes(&self, state: &mut [u8; 16]) {
        for s in state.iter_mut() {
            *s = self.sbox[*s as usize];
        }
    }

    fn inv_sub_bytes(&self, state: &mut [u8; 16]) {
        for s in state.iter_mut() {
            *s = self.inv_sbox[*s as usize];
        }
    }

    // State is column-major: index = row + 4*col.
    fn shift_rows(state: &mut [u8; 16]) {
        let s = *state;
        for row in 1..4 {
            for col in 0..4 {
                state[row + 4 * col] = s[row + 4 * ((col + row) % 4)];
            }
        }
    }

    fn inv_shift_rows(state: &mut [u8; 16]) {
        let s = *state;
        for row in 1..4 {
            for col in 0..4 {
                state[row + 4 * col] = s[row + 4 * ((col + 4 - row) % 4)];
            }
        }
    }

    fn mix_columns(state: &mut [u8; 16]) {
        for col in 0..4 {
            let c = col * 4;
            let a = [state[c], state[c + 1], state[c + 2], state[c + 3]];
            state[c] = gmul(a[0], 2) ^ gmul(a[1], 3) ^ a[2] ^ a[3];
            state[c + 1] = a[0] ^ gmul(a[1], 2) ^ gmul(a[2], 3) ^ a[3];
            state[c + 2] = a[0] ^ a[1] ^ gmul(a[2], 2) ^ gmul(a[3], 3);
            state[c + 3] = gmul(a[0], 3) ^ a[1] ^ a[2] ^ gmul(a[3], 2);
        }
    }

    fn inv_mix_columns(state: &mut [u8; 16]) {
        for col in 0..4 {
            let c = col * 4;
            let a = [state[c], state[c + 1], state[c + 2], state[c + 3]];
            state[c] = gmul(a[0], 14) ^ gmul(a[1], 11) ^ gmul(a[2], 13) ^ gmul(a[3], 9);
            state[c + 1] = gmul(a[0], 9) ^ gmul(a[1], 14) ^ gmul(a[2], 11) ^ gmul(a[3], 13);
            state[c + 2] = gmul(a[0], 13) ^ gmul(a[1], 9) ^ gmul(a[2], 14) ^ gmul(a[3], 11);
            state[c + 3] = gmul(a[0], 11) ^ gmul(a[1], 13) ^ gmul(a[2], 9) ^ gmul(a[3], 14);
        }
    }

    /// Encrypt one 16-byte block.
    pub fn encrypt_block(&self, block: [u8; 16]) -> [u8; 16] {
        let mut state = block;
        Self::add_round_key(&mut state, &self.round_keys[0]);
        for round in 1..self.rounds {
            self.sub_bytes(&mut state);
            Self::shift_rows(&mut state);
            Self::mix_columns(&mut state);
            Self::add_round_key(&mut state, &self.round_keys[round]);
        }
        self.sub_bytes(&mut state);
        Self::shift_rows(&mut state);
        Self::add_round_key(&mut state, &self.round_keys[self.rounds]);
        state
    }

    /// Decrypt one 16-byte block.
    pub fn decrypt_block(&self, block: [u8; 16]) -> [u8; 16] {
        let mut state = block;
        Self::add_round_key(&mut state, &self.round_keys[self.rounds]);
        for round in (1..self.rounds).rev() {
            Self::inv_shift_rows(&mut state);
            self.inv_sub_bytes(&mut state);
            Self::add_round_key(&mut state, &self.round_keys[round]);
            Self::inv_mix_columns(&mut state);
        }
        Self::inv_shift_rows(&mut state);
        self.inv_sub_bytes(&mut state);
        Self::add_round_key(&mut state, &self.round_keys[0]);
        state
    }
}

fn to_block(slice: &[u8]) -> [u8; 16] {
    let mut block = [0u8; 16];
    block.copy_from_slice(slice);
    block
}

/// AES-CBC encrypt (no padding; `data` must be a multiple of 16 bytes).
pub fn aes_cbc_encrypt(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes::new(key);
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        if chunk.len() < 16 {
            break;
        }
        let mut block = to_block(chunk);
        for (b, p) in block.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        let enc = cipher.encrypt_block(block);
        out.extend_from_slice(&enc);
        prev = enc;
    }
    out
}

/// AES-CBC decrypt (no padding removal; `data` must be a multiple of 16 bytes).
pub fn aes_cbc_decrypt(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes::new(key);
    let mut prev = *iv;
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        if chunk.len() < 16 {
            break;
        }
        let block = to_block(chunk);
        let mut dec = cipher.decrypt_block(block);
        for (d, p) in dec.iter_mut().zip(prev.iter()) {
            *d ^= p;
        }
        out.extend_from_slice(&dec);
        prev = block;
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
    fn fips197_aes128_block() {
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let plain: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let cipher = Aes::new(&key);
        let enc = cipher.encrypt_block(plain);
        assert_eq!(hex(&enc), "69c4e0d86a7b0430d8cdb78070b4c55a");
        assert_eq!(cipher.decrypt_block(enc), plain);
    }

    #[test]
    fn fips197_aes256_block() {
        let key: [u8; 32] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];
        let plain: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let cipher = Aes::new(&key);
        let enc = cipher.encrypt_block(plain);
        assert_eq!(hex(&enc), "8ea2b7ca516745bfeafc49904b496089");
        assert_eq!(cipher.decrypt_block(enc), plain);
    }

    #[test]
    fn cbc_round_trip() {
        let key = [0x42u8; 32];
        let iv = [0x24u8; 16];
        let data = b"sixteen bytes!!!sixteen bytes!!!"; // 32 bytes
        let enc = aes_cbc_encrypt(&key, &iv, data);
        assert_eq!(aes_cbc_decrypt(&key, &iv, &enc), data);
    }
}
