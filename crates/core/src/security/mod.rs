//! PDF Standard Security Handler (ISO 32000-1 §7.6) — zero dependencies.
//!
//! Reads encrypted PDFs (RC4 R2/R3, AESV2 R4, AESV3 R5/R6) with the user
//! password (empty by default), and writes RC4-encrypted PDFs. Decryption is
//! per-object: a file key is derived from the password + `/O` + `/P` + `/ID`,
//! then combined with each object's number/generation (Algorithm 1) — except
//! AESV3, which uses the file key directly. Built on [`crate::crypto`].

use crate::crypto::{aes_cbc_decrypt, aes_cbc_encrypt, md5, rc4, sha256, sha384, sha512};
use crate::object::{Dictionary, Object};

/// The 32-byte password padding string (Algorithm 2, step a).
const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Method {
    Rc4,
    AesV2,
    AesV3,
}

/// A resolved security context able to decrypt/encrypt object data.
#[derive(Debug, Clone)]
pub struct Security {
    method: Method,
    key: Vec<u8>,
}

fn pad_password(pw: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = if i < pw.len() { pw[i] } else { PAD[i - pw.len().min(32)] };
    }
    out
}

/// File key for RC4/AESV2 handlers (Algorithm 2).
fn legacy_file_key(
    o: &[u8],
    p: i32,
    id0: &[u8],
    key_len: usize,
    r: i32,
    encrypt_meta: bool,
    password: &[u8],
) -> Vec<u8> {
    let mut input = Vec::new();
    input.extend_from_slice(&pad_password(password));
    input.extend_from_slice(&o[..o.len().min(32)]);
    input.extend_from_slice(&(p as u32).to_le_bytes());
    input.extend_from_slice(id0);
    if r >= 4 && !encrypt_meta {
        input.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    }
    let mut hash = md5(&input).to_vec();
    if r >= 3 {
        for _ in 0..50 {
            hash = md5(&hash[..key_len]).to_vec();
        }
    }
    hash.truncate(key_len);
    hash
}

/// Per-object key for RC4/AESV2 (Algorithm 1).
fn object_key(file_key: &[u8], num: u32, gen: u16, aes: bool) -> Vec<u8> {
    let mut input = file_key.to_vec();
    input.extend_from_slice(&num.to_le_bytes()[..3]);
    input.extend_from_slice(&gen.to_le_bytes()[..2]);
    if aes {
        input.extend_from_slice(b"sAlT");
    }
    let hash = md5(&input);
    let n = (file_key.len() + 5).min(16);
    hash[..n].to_vec()
}

/// The R6 password hash (Algorithm 2.B); R5 is a single SHA-256.
fn hash_r6(password: &[u8], salt: &[u8], udata: &[u8], r: i32) -> [u8; 32] {
    let mut k = sha256(&[password, salt, udata].concat()).to_vec();
    if r < 6 {
        let mut out = [0u8; 32];
        out.copy_from_slice(&k[..32]);
        return out;
    }
    let mut round = 0usize;
    loop {
        let block = [password, &k, udata].concat();
        let mut k1 = Vec::with_capacity(block.len() * 64);
        for _ in 0..64 {
            k1.extend_from_slice(&block);
        }
        let mut iv = [0u8; 16];
        iv.copy_from_slice(&k[16..32]);
        let mut aes_key = [0u8; 16];
        aes_key.copy_from_slice(&k[0..16]);
        let e = aes_cbc_encrypt(&aes_key, &iv, &k1);
        let m = e[..16].iter().map(|&b| b as u32).sum::<u32>() % 3;
        k = match m {
            0 => sha256(&e).to_vec(),
            1 => sha384(&e).to_vec(),
            _ => sha512(&e).to_vec(),
        };
        round += 1;
        if round >= 64 && (*e.last().unwrap() as usize) <= round - 32 {
            break;
        }
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&k[..32]);
    out
}

fn str_bytes(dict: &Dictionary, key: &[u8]) -> Vec<u8> {
    match dict.get(key) {
        Some(Object::String(b, _)) => b.clone(),
        _ => Vec::new(),
    }
}

impl Security {
    /// Resolve a security context from an `/Encrypt` dictionary and the first
    /// `/ID` string, trying `password` (default empty). Returns `None` for an
    /// unsupported handler or a wrong password.
    pub fn open(encrypt: &Dictionary, id0: &[u8], password: &[u8]) -> Option<Security> {
        if encrypt.get(b"Filter").and_then(Object::as_name) != Some(b"Standard".as_slice()) {
            return None;
        }
        let v = encrypt.get(b"V").and_then(Object::as_i64).unwrap_or(0);
        let r = encrypt.get(b"R").and_then(Object::as_i64).unwrap_or(0) as i32;
        let length = encrypt.get(b"Length").and_then(Object::as_i64).unwrap_or(40) as usize;
        let o = str_bytes(encrypt, b"O");
        let p = encrypt.get(b"P").and_then(Object::as_i64).unwrap_or(0) as i32;
        let encrypt_meta = encrypt
            .get(b"EncryptMetadata")
            .map(|o| matches!(o, Object::Boolean(true)))
            .unwrap_or(true);

        let method = match v {
            1 | 2 => Method::Rc4,
            4 => Self::cfm_method(encrypt),
            5 => Method::AesV3,
            _ => return None,
        };

        let key = match method {
            Method::AesV3 => {
                let u = str_bytes(encrypt, b"U");
                let ue = str_bytes(encrypt, b"UE");
                if u.len() < 48 {
                    return None;
                }
                let check = hash_r6(password, &u[32..40], &[], r);
                if check != u[..32] {
                    return None; // wrong password
                }
                let ik = hash_r6(password, &u[40..48], &[], r);
                aes_cbc_decrypt(&ik, &[0u8; 16], &ue)
            }
            _ => {
                let key_len = if v == 1 { 5 } else { length / 8 };
                let key = legacy_file_key(&o, p, id0, key_len, r, encrypt_meta, password);
                // Reject a wrong password by checking /U (Algorithm 6).
                if !validate_user(&key, &str_bytes(encrypt, b"U"), id0, r) {
                    return None;
                }
                key
            }
        };

        Some(Security { method, key })
    }

    fn cfm_method(encrypt: &Dictionary) -> Method {
        // /CF /StdCF /CFM is AESV2 or V2 (RC4).
        let cfm = encrypt
            .get(b"CF")
            .and_then(Object::as_dict)
            .and_then(|cf| cf.get(b"StdCF"))
            .and_then(Object::as_dict)
            .and_then(|std| std.get(b"CFM"))
            .and_then(Object::as_name);
        match cfm {
            Some(b"AESV2") => Method::AesV2,
            Some(b"AESV3") => Method::AesV3,
            _ => Method::Rc4,
        }
    }

    /// Decrypt one object's string/stream bytes.
    pub fn decrypt(&self, num: u32, gen: u16, data: &[u8]) -> Vec<u8> {
        match self.method {
            Method::Rc4 => rc4(&object_key(&self.key, num, gen, false), data),
            Method::AesV2 => aes_decrypt_object(&object_key(&self.key, num, gen, true), data),
            Method::AesV3 => aes_decrypt_object(&self.key, data),
        }
    }

    /// Encrypt one object's string/stream bytes.
    pub fn encrypt(&self, num: u32, gen: u16, data: &[u8]) -> Vec<u8> {
        match self.method {
            Method::Rc4 => rc4(&object_key(&self.key, num, gen, false), data),
            Method::AesV2 => {
                aes_encrypt_object(&object_key(&self.key, num, gen, true), num, gen, data)
            }
            Method::AesV3 => aes_encrypt_object(&self.key, num, gen, data),
        }
    }

    /// Build an RC4 (V2/R3, 128-bit) security context plus its `/Encrypt`
    /// dictionary, given a user password and the document's first `/ID`.
    pub fn new_rc4(user_password: &[u8], id0: &[u8], permissions: i32) -> (Security, Dictionary) {
        let key_len = 16usize;
        let r = 3i32;

        // /O (Algorithm 3): owner == user here (single password).
        let mut okey = md5(&pad_password(user_password)).to_vec();
        for _ in 0..50 {
            okey = md5(&okey[..key_len]).to_vec();
        }
        okey.truncate(key_len);
        let mut o = rc4(&okey, &pad_password(user_password));
        for i in 1..=19u8 {
            let xkey: Vec<u8> = okey.iter().map(|&b| b ^ i).collect();
            o = rc4(&xkey, &o);
        }

        let key = legacy_file_key(&o, permissions, id0, key_len, r, true, user_password);

        // /U (Algorithm 5): RC4 of md5(PAD || ID) with 19 extra rounds.
        let mut u = md5(&[PAD.as_slice(), id0].concat()).to_vec();
        u = rc4(&key, &u);
        for i in 1..=19u8 {
            let xkey: Vec<u8> = key.iter().map(|&b| b ^ i).collect();
            u = rc4(&xkey, &u);
        }
        u.resize(32, 0); // pad to 32 bytes with arbitrary data

        let mut dict = Dictionary::new();
        dict.set(b"Filter".to_vec(), Object::Name(b"Standard".to_vec()));
        dict.set(b"V".to_vec(), Object::Integer(2));
        dict.set(b"R".to_vec(), Object::Integer(r as i64));
        dict.set(b"Length".to_vec(), Object::Integer((key_len * 8) as i64));
        dict.set(b"P".to_vec(), Object::Integer(permissions as i64));
        dict.set(
            b"O".to_vec(),
            Object::String(o, crate::object::StringKind::Literal),
        );
        dict.set(
            b"U".to_vec(),
            Object::String(u, crate::object::StringKind::Literal),
        );

        (Security { method: Method::Rc4, key }, dict)
    }
}

/// Validate a derived RC4/AESV2 file key against the stored `/U` (Algorithm 6).
/// Returns `true` when `/U` is too short to check (accept rather than block).
fn validate_user(key: &[u8], stored_u: &[u8], id0: &[u8], r: i32) -> bool {
    if stored_u.len() < 16 {
        return true;
    }
    let computed = if r >= 3 {
        let mut u = md5(&[PAD.as_slice(), id0].concat()).to_vec();
        u = rc4(key, &u);
        for i in 1..=19u8 {
            let xkey: Vec<u8> = key.iter().map(|&b| b ^ i).collect();
            u = rc4(&xkey, &u);
        }
        u
    } else {
        rc4(key, &PAD)
    };
    computed[..16] == stored_u[..16]
}

fn aes_decrypt_object(key: &[u8], data: &[u8]) -> Vec<u8> {
    if data.len() < 16 {
        return Vec::new();
    }
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&data[..16]);
    let mut out = aes_cbc_decrypt(key, &iv, &data[16..]);
    // Strip PKCS#7 padding.
    if let Some(&pad) = out.last() {
        let pad = pad as usize;
        if (1..=16).contains(&pad) && pad <= out.len() {
            out.truncate(out.len() - pad);
        }
    }
    out
}

fn aes_encrypt_object(key: &[u8], num: u32, gen: u16, data: &[u8]) -> Vec<u8> {
    // Deterministic, per-object IV (unique per object id; encrypt/decrypt agree
    // because decrypt reads the IV back from the ciphertext prefix).
    let seed = [key, &num.to_le_bytes(), &gen.to_le_bytes()].concat();
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&sha256(&seed)[..16]);

    // PKCS#7 pad to a 16-byte boundary.
    let pad = 16 - (data.len() % 16);
    let mut padded = data.to_vec();
    padded.resize(padded.len() + pad, pad as u8);

    let mut out = iv.to_vec();
    out.extend(aes_cbc_encrypt(key, &iv, &padded));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc4_object_round_trip() {
        let key = vec![0x11u8; 16];
        let sec = Security {
            method: Method::Rc4,
            key,
        };
        let data = b"The background must be preserved exactly.";
        let enc = sec.encrypt(7, 0, data);
        assert_ne!(&enc[..], &data[..], "ciphertext differs");
        assert_eq!(sec.decrypt(7, 0, &enc), data, "decrypt restores plaintext");
    }

    #[test]
    fn aes_object_round_trip() {
        let sec = Security {
            method: Method::AesV3,
            key: vec![0x42u8; 32],
        };
        let data = b"exactly sixteen!"; // 16 bytes → full padding block
        let enc = sec.encrypt(3, 0, data);
        assert!(enc.len() >= 16 + 16 + 16, "iv + padded ciphertext");
        assert_eq!(sec.decrypt(3, 0, &enc), data);
    }

    #[test]
    fn aesv2_object_round_trip() {
        let sec = Security {
            method: Method::AesV2,
            key: vec![0x09u8; 16],
        };
        let data = b"variable length payload that is not a block multiple";
        let enc = sec.encrypt(11, 0, data);
        assert_eq!(sec.decrypt(11, 0, &enc), data);
    }

    #[test]
    fn builds_rc4_encrypt_dictionary() {
        let (sec, dict) = Security::new_rc4(b"hunter2", b"file-id-0000", -44);
        assert_eq!(dict.get(b"Filter").and_then(Object::as_name), Some(b"Standard".as_slice()));
        assert_eq!(dict.get(b"V").and_then(Object::as_i64), Some(2));
        // The derived key actually decrypts what it encrypts.
        let enc = sec.encrypt(5, 0, b"hello");
        assert_eq!(sec.decrypt(5, 0, &enc), b"hello");
    }
}
