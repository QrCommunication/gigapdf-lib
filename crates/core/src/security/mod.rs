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

/// The user access permissions encoded in an `/Encrypt` dictionary's `/P` entry
/// (ISO 32000-1 §7.6.3.2, Table 22). Each field maps to one permission bit; a
/// permission is **granted** when its field is `true`.
///
/// `/P` is a signed 32-bit integer. The spec fixes the reserved bits: bits 1–2
/// and 7–8 are `0`, and bits 13–32 are `1`. Only the eight bits below are
/// meaningful to a host. Round-trips losslessly through [`Permissions::to_p`] /
/// [`Permissions::from_p`].
///
/// | Bit (1-indexed) | Field | Granted when set |
/// |-----------------|-------|------------------|
/// | 3  | [`print`](Self::print)            | Print the document (low resolution if 12 is clear). |
/// | 4  | [`modify`](Self::modify)          | Modify the contents (other than 6/9/11). |
/// | 5  | [`copy`](Self::copy)              | Copy / extract text and graphics. |
/// | 6  | [`annotate`](Self::annotate)      | Add or modify annotations; fill form fields (with 4). |
/// | 9  | [`fill_forms`](Self::fill_forms)  | Fill existing interactive form fields (even if 6 is clear). |
/// | 10 | [`accessibility`](Self::accessibility) | Extract text/graphics for accessibility. |
/// | 11 | [`assemble`](Self::assemble)      | Assemble: insert, delete and rotate pages. |
/// | 12 | [`print_high_res`](Self::print_high_res) | Print to a high-resolution device (requires 3). |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    /// Bit 3 — print the document.
    pub print: bool,
    /// Bit 4 — modify the contents of the document.
    pub modify: bool,
    /// Bit 5 — copy or otherwise extract text and graphics.
    pub copy: bool,
    /// Bit 6 — add or modify text annotations and fill in interactive form fields.
    pub annotate: bool,
    /// Bit 9 — fill in existing interactive form fields (R3+).
    pub fill_forms: bool,
    /// Bit 10 — extract text and graphics for accessibility (R3+).
    pub accessibility: bool,
    /// Bit 11 — assemble the document: insert, rotate or delete pages (R3+).
    pub assemble: bool,
    /// Bit 12 — print to a high-resolution device (R3+).
    pub print_high_res: bool,
}

/// Bit masks for the meaningful `/P` permission bits (1-indexed in the spec).
const P_PRINT: u32 = 1 << 2; // bit 3
const P_MODIFY: u32 = 1 << 3; // bit 4
const P_COPY: u32 = 1 << 4; // bit 5
const P_ANNOTATE: u32 = 1 << 5; // bit 6
const P_FILL_FORMS: u32 = 1 << 8; // bit 9
const P_ACCESSIBILITY: u32 = 1 << 9; // bit 10
const P_ASSEMBLE: u32 = 1 << 10; // bit 11
const P_PRINT_HIGH_RES: u32 = 1 << 11; // bit 12

/// Reserved high bits (13–32) that the spec requires to be set to 1.
const P_RESERVED_HIGH: u32 = 0xFFFF_F000;

impl Default for Permissions {
    /// Everything allowed — the unrestricted default applied when a caller asks
    /// to encrypt without specifying any flags. Equivalent to [`Permissions::all`].
    fn default() -> Self {
        Self::all()
    }
}

impl Permissions {
    /// All eight permissions granted (the unrestricted default).
    pub const fn all() -> Self {
        Self {
            print: true,
            modify: true,
            copy: true,
            annotate: true,
            fill_forms: true,
            accessibility: true,
            assemble: true,
            print_high_res: true,
        }
    }

    /// No permissions granted (maximally restrictive; the owner password still
    /// lifts every restriction).
    pub const fn none() -> Self {
        Self {
            print: false,
            modify: false,
            copy: false,
            annotate: false,
            fill_forms: false,
            accessibility: false,
            assemble: false,
            print_high_res: false,
        }
    }

    /// Encode to the `/P` value (ISO 32000-1 Table 22): a signed 32-bit integer
    /// with the reserved bits 1–2 and 7–8 cleared and bits 13–32 set, plus one
    /// bit per granted permission.
    pub const fn to_p(self) -> i32 {
        let mut p = P_RESERVED_HIGH;
        if self.print {
            p |= P_PRINT;
        }
        if self.modify {
            p |= P_MODIFY;
        }
        if self.copy {
            p |= P_COPY;
        }
        if self.annotate {
            p |= P_ANNOTATE;
        }
        if self.fill_forms {
            p |= P_FILL_FORMS;
        }
        if self.accessibility {
            p |= P_ACCESSIBILITY;
        }
        if self.assemble {
            p |= P_ASSEMBLE;
        }
        if self.print_high_res {
            p |= P_PRINT_HIGH_RES;
        }
        p as i32
    }

    /// Decode a `/P` value back into the eight permission flags, ignoring the
    /// reserved bits. The inverse of [`Permissions::to_p`].
    pub const fn from_p(p: i32) -> Self {
        let p = p as u32;
        Self {
            print: p & P_PRINT != 0,
            modify: p & P_MODIFY != 0,
            copy: p & P_COPY != 0,
            annotate: p & P_ANNOTATE != 0,
            fill_forms: p & P_FILL_FORMS != 0,
            accessibility: p & P_ACCESSIBILITY != 0,
            assemble: p & P_ASSEMBLE != 0,
            print_high_res: p & P_PRINT_HIGH_RES != 0,
        }
    }
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
        *slot = if i < pw.len() {
            pw[i]
        } else {
            PAD[i - pw.len().min(32)]
        };
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

/// `/O` string for the RC4/AESV2 handlers (Algorithm 3), R3+ (50× MD5 + 20 RC4
/// passes). An empty `owner_password` falls back to the user password, per spec.
fn compute_legacy_o(owner_password: &[u8], user_password: &[u8], key_len: usize) -> Vec<u8> {
    let owner = if owner_password.is_empty() {
        user_password
    } else {
        owner_password
    };
    let mut okey = md5(&pad_password(owner)).to_vec();
    for _ in 0..50 {
        okey = md5(&okey[..key_len]).to_vec();
    }
    okey.truncate(key_len);
    let mut o = rc4(&okey, &pad_password(user_password));
    for i in 1..=19u8 {
        let xkey: Vec<u8> = okey.iter().map(|&b| b ^ i).collect();
        o = rc4(&xkey, &o);
    }
    o
}

/// `/U` string for the RC4/AESV2 handlers, revision 3+ (Algorithm 5): RC4 of
/// `md5(PAD || ID)` with 19 extra key-xor passes, padded to 32 bytes.
fn compute_legacy_u(file_key: &[u8], id0: &[u8]) -> Vec<u8> {
    let mut u = md5(&[PAD.as_slice(), id0].concat()).to_vec();
    u = rc4(file_key, &u);
    for i in 1..=19u8 {
        let xkey: Vec<u8> = file_key.iter().map(|&b| b ^ i).collect();
        u = rc4(&xkey, &u);
    }
    u.resize(32, 0); // pad to 32 bytes with arbitrary data
    u
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
        let length = encrypt
            .get(b"Length")
            .and_then(Object::as_i64)
            .unwrap_or(40) as usize;
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
                // Algorithm 2.A: try the user password, then the owner password.
                if hash_r6(password, &u[32..40], &[], r) == u[..32] {
                    let ik = hash_r6(password, &u[40..48], &[], r);
                    aes_cbc_decrypt(&ik, &[0u8; 16], &ue)
                } else {
                    let o = str_bytes(encrypt, b"O");
                    let oe = str_bytes(encrypt, b"OE");
                    // The owner hash is salted with the 48-byte /U string.
                    if o.len() < 48 || hash_r6(password, &o[32..40], &u, r) != o[..32] {
                        return None; // wrong password
                    }
                    let ik = hash_r6(password, &o[40..48], &u, r);
                    aes_cbc_decrypt(&ik, &[0u8; 16], &oe)
                }
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
    pub fn new_rc4(
        user_password: &[u8],
        owner_password: &[u8],
        id0: &[u8],
        permissions: i32,
    ) -> (Security, Dictionary) {
        let key_len = 16usize;
        let r = 3i32;

        let o = compute_legacy_o(owner_password, user_password, key_len);
        let key = legacy_file_key(&o, permissions, id0, key_len, r, true, user_password);
        let u = compute_legacy_u(&key, id0);

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

        (
            Security {
                method: Method::Rc4,
                key,
            },
            dict,
        )
    }

    /// Build an AESV2 (V4/R4, 128-bit AES-CBC) security context plus its
    /// `/Encrypt` dictionary. `user_password` opens the document; the (optional)
    /// `owner_password` governs permission changes. Round-trips through
    /// [`Security::open`].
    pub fn new_aes_v2(
        user_password: &[u8],
        owner_password: &[u8],
        id0: &[u8],
        permissions: i32,
    ) -> (Security, Dictionary) {
        let key_len = 16usize;
        let r = 4i32;

        let o = compute_legacy_o(owner_password, user_password, key_len);
        let key = legacy_file_key(&o, permissions, id0, key_len, r, true, user_password);
        let u = compute_legacy_u(&key, id0);

        // /CF << /StdCF << /CFM /AESV2 /Length 16 /AuthEvent /DocOpen >> >>
        let mut std_cf = Dictionary::new();
        std_cf.set(b"CFM".to_vec(), Object::Name(b"AESV2".to_vec()));
        std_cf.set(b"Length".to_vec(), Object::Integer(16));
        std_cf.set(b"AuthEvent".to_vec(), Object::Name(b"DocOpen".to_vec()));
        let mut cf = Dictionary::new();
        cf.set(b"StdCF".to_vec(), Object::Dictionary(std_cf));

        let mut dict = Dictionary::new();
        dict.set(b"Filter".to_vec(), Object::Name(b"Standard".to_vec()));
        dict.set(b"V".to_vec(), Object::Integer(4));
        dict.set(b"R".to_vec(), Object::Integer(r as i64));
        dict.set(b"Length".to_vec(), Object::Integer((key_len * 8) as i64));
        dict.set(b"CF".to_vec(), Object::Dictionary(cf));
        dict.set(b"StmF".to_vec(), Object::Name(b"StdCF".to_vec()));
        dict.set(b"StrF".to_vec(), Object::Name(b"StdCF".to_vec()));
        dict.set(b"P".to_vec(), Object::Integer(permissions as i64));
        dict.set(
            b"O".to_vec(),
            Object::String(o, crate::object::StringKind::Literal),
        );
        dict.set(
            b"U".to_vec(),
            Object::String(u, crate::object::StringKind::Literal),
        );

        (
            Security {
                method: Method::AesV2,
                key,
            },
            dict,
        )
    }

    /// Build an AESV3 (V5/R6, 256-bit AES-CBC) security context plus its
    /// `/Encrypt` dictionary (ISO 32000-2 Algorithms 8–10). `file_key` is the
    /// 32-byte file encryption key — it MUST be secret host randomness (the WASM
    /// engine has no RNG); any other length is hashed to 32 bytes. The salts are
    /// derived from this secret key, so they are unique and unpredictable per
    /// document. An empty `owner_password` falls back to the user password.
    pub fn new_aes_v3(
        user_password: &[u8],
        owner_password: &[u8],
        file_key: &[u8],
        permissions: i32,
        encrypt_metadata: bool,
    ) -> (Security, Dictionary) {
        let r = 6i32;
        let fek = if file_key.len() == 32 {
            file_key.to_vec()
        } else {
            sha256(file_key).to_vec()
        };
        let owner = if owner_password.is_empty() {
            user_password
        } else {
            owner_password
        };

        // Salts derived from the secret file key (unique + unpredictable per doc).
        let salt =
            |label: &[u8]| -> Vec<u8> { sha256(&[fek.as_slice(), label].concat())[..8].to_vec() };
        let uvs = salt(b"uvs");
        let uks = salt(b"uks");
        let ovs = salt(b"ovs");
        let oks = salt(b"oks");

        // Algorithm 8 — /U (48 bytes) and /UE (32 bytes).
        let mut u = hash_r6(user_password, &uvs, &[], r).to_vec();
        u.extend_from_slice(&uvs);
        u.extend_from_slice(&uks);
        let iuk = hash_r6(user_password, &uks, &[], r);
        let ue = aes_cbc_encrypt(&iuk, &[0u8; 16], &fek);

        // Algorithm 9 — /O (48 bytes, salted with /U) and /OE (32 bytes).
        let mut o = hash_r6(owner, &ovs, &u, r).to_vec();
        o.extend_from_slice(&ovs);
        o.extend_from_slice(&oks);
        let iok = hash_r6(owner, &oks, &u, r);
        let oe = aes_cbc_encrypt(&iok, &[0u8; 16], &fek);

        // Algorithm 10 — /Perms (16 bytes, AES-256 ECB of the perms block).
        let mut perms_block = [0u8; 16];
        perms_block[..4].copy_from_slice(&(permissions as u32).to_le_bytes());
        perms_block[4..8].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        perms_block[8] = if encrypt_metadata { b'T' } else { b'F' };
        perms_block[9..12].copy_from_slice(b"adb");
        perms_block[12..16].copy_from_slice(&sha256(&[fek.as_slice(), b"perms"].concat())[..4]);
        let perms = aes_cbc_encrypt(&fek, &[0u8; 16], &perms_block); // single block = ECB

        let lit = |b: Vec<u8>| Object::String(b, crate::object::StringKind::Literal);

        // /CF << /StdCF << /CFM /AESV3 /Length 32 /AuthEvent /DocOpen >> >>
        let mut std_cf = Dictionary::new();
        std_cf.set(b"CFM".to_vec(), Object::Name(b"AESV3".to_vec()));
        std_cf.set(b"Length".to_vec(), Object::Integer(32));
        std_cf.set(b"AuthEvent".to_vec(), Object::Name(b"DocOpen".to_vec()));
        let mut cf = Dictionary::new();
        cf.set(b"StdCF".to_vec(), Object::Dictionary(std_cf));

        let mut dict = Dictionary::new();
        dict.set(b"Filter".to_vec(), Object::Name(b"Standard".to_vec()));
        dict.set(b"V".to_vec(), Object::Integer(5));
        dict.set(b"R".to_vec(), Object::Integer(r as i64));
        dict.set(b"Length".to_vec(), Object::Integer(256));
        dict.set(b"CF".to_vec(), Object::Dictionary(cf));
        dict.set(b"StmF".to_vec(), Object::Name(b"StdCF".to_vec()));
        dict.set(b"StrF".to_vec(), Object::Name(b"StdCF".to_vec()));
        dict.set(b"P".to_vec(), Object::Integer(permissions as i64));
        dict.set(b"O".to_vec(), lit(o));
        dict.set(b"U".to_vec(), lit(u));
        dict.set(b"OE".to_vec(), lit(oe));
        dict.set(b"UE".to_vec(), lit(ue));
        dict.set(b"Perms".to_vec(), lit(perms));
        if !encrypt_metadata {
            dict.set(b"EncryptMetadata".to_vec(), Object::Boolean(false));
        }

        (
            Security {
                method: Method::AesV3,
                key: fek,
            },
            dict,
        )
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
        let (sec, dict) = Security::new_rc4(b"hunter2", b"owner-pw", b"file-id-0000", -44);
        assert_eq!(
            dict.get(b"Filter").and_then(Object::as_name),
            Some(b"Standard".as_slice())
        );
        assert_eq!(dict.get(b"V").and_then(Object::as_i64), Some(2));
        // The derived key actually decrypts what it encrypts.
        let enc = sec.encrypt(5, 0, b"hello");
        assert_eq!(sec.decrypt(5, 0, &enc), b"hello");
    }

    #[test]
    fn builds_aesv2_encrypt_dictionary_round_trip() {
        let id0 = b"file-id-0123456789ab";
        let (sec, dict) = Security::new_aes_v2(b"user-pw", b"owner-pw", id0, -44);
        assert_eq!(dict.get(b"V").and_then(Object::as_i64), Some(4));
        assert_eq!(dict.get(b"R").and_then(Object::as_i64), Some(4));

        // The handler decrypts what it encrypts (AES-CBC per object).
        let enc = sec.encrypt(5, 0, b"secret content, not a block multiple");
        assert_eq!(
            sec.decrypt(5, 0, &enc),
            b"secret content, not a block multiple"
        );

        // The /Encrypt dict re-opens with the USER password (Algorithm 6 via /U)
        // and the reopened context round-trips too.
        let reopened = Security::open(&dict, id0, b"user-pw").expect("user password opens");
        let enc2 = reopened.encrypt(9, 0, b"abc");
        assert_eq!(reopened.decrypt(9, 0, &enc2), b"abc");

        // A wrong password is rejected.
        assert!(Security::open(&dict, id0, b"wrong").is_none());
    }

    #[test]
    fn builds_aesv3_encrypt_dictionary_round_trip() {
        let fek = [0x5Au8; 32]; // stands in for secret host randomness
        let (sec, dict) = Security::new_aes_v3(b"user-pw", b"owner-pw", &fek, -44, true);
        assert_eq!(dict.get(b"V").and_then(Object::as_i64), Some(5));
        assert_eq!(dict.get(b"R").and_then(Object::as_i64), Some(6));

        let msg = b"top secret payload, not block aligned";
        let enc = sec.encrypt(7, 0, msg);
        assert_eq!(sec.decrypt(7, 0, &enc), msg);

        // AESV3 keys are independent of the file ID, so `id0` is irrelevant here.
        // Re-open with the USER password (Algorithm 2.A) recovers the file key.
        let by_user = Security::open(&dict, b"", b"user-pw").expect("user opens");
        assert_eq!(by_user.decrypt(7, 0, &enc), msg);

        // Re-open with the OWNER password also recovers it.
        let by_owner = Security::open(&dict, b"", b"owner-pw").expect("owner opens");
        assert_eq!(by_owner.decrypt(7, 0, &enc), msg);

        // A wrong password is rejected.
        assert!(Security::open(&dict, b"", b"nope").is_none());
    }

    // ─── Permissions (`/P`) — ISO 32000-1 Table 22 ──────────────────────────

    #[test]
    fn permissions_default_is_all_allowed() {
        assert_eq!(Permissions::default(), Permissions::all());
        let p = Permissions::all();
        assert!(p.print && p.modify && p.copy && p.annotate);
        assert!(p.fill_forms && p.accessibility && p.assemble && p.print_high_res);
    }

    #[test]
    fn permissions_all_encodes_to_spec_p_value() {
        // bits 3,4,5,6,9,10,11,12 set + reserved high bits 13..32 set,
        // reserved low bits 1,2,7,8 clear → 0xFFFFFF3C → -196 as i32.
        assert_eq!(Permissions::all().to_p(), -196);
    }

    #[test]
    fn permissions_none_encodes_to_reserved_only() {
        // Only reserved high bits set → 0xFFFFF000 → -4096 as i32.
        assert_eq!(Permissions::none().to_p(), -4096);
    }

    #[test]
    fn permissions_reserved_bits_are_fixed_per_spec() {
        // For ALL eight on/off combinations, reserved bits 1,2,7,8 stay 0 and
        // bits 13..32 stay 1.
        for mask in 0u32..256 {
            let p = Permissions {
                print: mask & 1 != 0,
                modify: mask & 2 != 0,
                copy: mask & 4 != 0,
                annotate: mask & 8 != 0,
                fill_forms: mask & 16 != 0,
                accessibility: mask & 32 != 0,
                assemble: mask & 64 != 0,
                print_high_res: mask & 128 != 0,
            };
            let bits = p.to_p() as u32;
            // Reserved low bits 1,2 (mask 0b11) and 7,8 (mask 0b1100_0000) clear.
            assert_eq!(bits & 0b0000_0011, 0, "bits 1-2 must be 0");
            assert_eq!(bits & 0b1100_0000, 0, "bits 7-8 must be 0");
            // Reserved high bits 13..32 set.
            assert_eq!(bits & 0xFFFF_F000, 0xFFFF_F000, "bits 13-32 must be 1");
        }
    }

    #[test]
    fn each_flag_sets_exactly_its_bit() {
        let base = Permissions::none().to_p() as u32;
        let cases = [
            (Permissions { print: true, ..Permissions::none() }, 1u32 << 2),
            (Permissions { modify: true, ..Permissions::none() }, 1 << 3),
            (Permissions { copy: true, ..Permissions::none() }, 1 << 4),
            (Permissions { annotate: true, ..Permissions::none() }, 1 << 5),
            (Permissions { fill_forms: true, ..Permissions::none() }, 1 << 8),
            (Permissions { accessibility: true, ..Permissions::none() }, 1 << 9),
            (Permissions { assemble: true, ..Permissions::none() }, 1 << 10),
            (Permissions { print_high_res: true, ..Permissions::none() }, 1 << 11),
        ];
        for (perm, bit) in cases {
            let bits = perm.to_p() as u32;
            assert_eq!(bits, base | bit, "exactly one extra bit set");
        }
    }

    #[test]
    fn permissions_round_trip_to_p_from_p() {
        for mask in 0u32..256 {
            let p = Permissions {
                print: mask & 1 != 0,
                modify: mask & 2 != 0,
                copy: mask & 4 != 0,
                annotate: mask & 8 != 0,
                fill_forms: mask & 16 != 0,
                accessibility: mask & 32 != 0,
                assemble: mask & 64 != 0,
                print_high_res: mask & 128 != 0,
            };
            assert_eq!(Permissions::from_p(p.to_p()), p, "mask {mask:#b}");
        }
    }

    #[test]
    fn from_p_ignores_reserved_bits() {
        // -1 (every bit set, including the reserved 1,2,7,8) decodes to all
        // eight permissions granted — reserved bits are ignored on decode.
        assert_eq!(Permissions::from_p(-1), Permissions::all());

        // The legacy sentinel -44 (0xFFFFFFD4) is NOT all-allowed: bit 4
        // (modify) and bit 6 (annotate) are clear, so it denies content
        // modification and annotation while granting the rest.
        let p = Permissions::from_p(-44);
        assert!(p.print && p.copy && p.fill_forms);
        assert!(p.accessibility && p.assemble && p.print_high_res);
        assert!(!p.modify, "bit 4 (modify) is clear in -44");
        assert!(!p.annotate, "bit 6 (annotate) is clear in -44");
    }

    #[test]
    fn no_print_clears_print_bit_in_p() {
        // A document encrypted with "no printing" has bit 3 cleared.
        let p = Permissions { print: false, ..Permissions::all() };
        let bits = p.to_p() as u32;
        assert_eq!(bits & (1 << 2), 0, "bit 3 (print) must be 0");
        // The other permissions remain granted.
        assert_ne!(bits & (1 << 3), 0, "bit 4 (modify) still set");
        assert_ne!(bits & (1 << 4), 0, "bit 5 (copy) still set");
        // And decoding confirms only printing is denied.
        let decoded = Permissions::from_p(p.to_p());
        assert!(!decoded.print);
        assert!(decoded.modify && decoded.copy && decoded.assemble);
    }

    #[test]
    fn copy_only_combination() {
        let p = Permissions { copy: true, ..Permissions::none() };
        let decoded = Permissions::from_p(p.to_p());
        assert!(decoded.copy);
        assert!(!decoded.print && !decoded.modify && !decoded.annotate);
        assert!(!decoded.fill_forms && !decoded.accessibility);
        assert!(!decoded.assemble && !decoded.print_high_res);
    }

    #[test]
    fn permissions_drive_encrypt_dictionary_p() {
        // The computed `/P` flows verbatim into the `/Encrypt` dict that the
        // AES-256 builder writes, and re-reads identically.
        let no_print = Permissions { print: false, ..Permissions::all() };
        let fek = [0x33u8; 32];
        let (_sec, dict) =
            Security::new_aes_v3(b"user", b"owner", &fek, no_print.to_p(), true);
        let stored = dict.get(b"P").and_then(Object::as_i64).unwrap() as i32;
        assert_eq!(stored, no_print.to_p());
        assert!(!Permissions::from_p(stored).print);
    }
}
