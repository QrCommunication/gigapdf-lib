//! Native PKCS#12 (`.p12`/`.pfx`) import — zero dependencies.
//!
//! Takes a password-protected PKCS#12 file apart into the RSA private key and
//! the X.509 certificate(s) it carries, so the engine can sign a PDF with a
//! *user-supplied* identity (CA-issued, eIDAS, …) rather than only the
//! self-signed key [`super::Signer`] generates.
//!
//! The layout follows RFC 7292:
//!
//! ```text
//! PFX ::= SEQUENCE { version, authSafe ContentInfo, macData MacData OPTIONAL }
//!   authSafe  = pkcs7-data wrapping AuthenticatedSafe (SEQUENCE OF ContentInfo)
//!     each ContentInfo is either `data` (plaintext SafeContents) or
//!     `encryptedData` (password-encrypted SafeContents)
//!       SafeContents = SEQUENCE OF SafeBag
//!         certBag             → DER X.509
//!         keyBag              → PKCS#8 PrivateKeyInfo (plaintext)
//!         pkcs8ShroudedKeyBag → EncryptedPrivateKeyInfo (password-encrypted)
//!   macData   = HMAC over the AuthenticatedSafe bytes (integrity + password check)
//! ```
//!
//! Ciphers covered: **PBES2** (PBKDF2 + AES-128/192/256-CBC, HMAC-SHA1/256 PRF)
//! — the openssl-3 / modern default — and **PBES1**
//! `pbeWithSHAAnd3-KeyTripleDES-CBC` (PKCS#12 KDF + 3DES) for legacy key bags.
//! The legacy `pbeWithSHAAnd40BitRC2-CBC` cert cipher is not yet implemented; a
//! bag using it is skipped rather than failing the whole import.
//!
//! Conformity is pinned by decrypting real OpenSSL-generated `.p12` fixtures and
//! checking the recovered modulus and certificate byte-for-byte (see the tests).

use super::der::{
    Reader, Tlv, TAG_CONTEXT_0, TAG_CONTEXT_0_PRIM, TAG_INTEGER, TAG_OCTET_STRING, TAG_OID,
    TAG_SEQUENCE,
};
use crate::crypto::aes::aes_cbc_decrypt;
use crate::crypto::des::des3_cbc_decrypt;
use crate::crypto::hmac::{hmac_sha1, hmac_sha256};
use crate::crypto::rc2::rc2_cbc_decrypt;
use crate::crypto::kdf::{
    bmp_string, pbkdf2_hmac_sha1, pbkdf2_hmac_sha256, pkcs12_kdf_sha1, pkcs12_kdf_sha256,
};
use crate::crypto::rsa::RsaPrivateKey;

// ─── Object identifiers (RFC 7292 / 5208 / 8018) ─────────────────────────────
const OID_DATA: &[u64] = &[1, 2, 840, 113549, 1, 7, 1]; // pkcs7-data
const OID_ENCRYPTED_DATA: &[u64] = &[1, 2, 840, 113549, 1, 7, 6]; // pkcs7-encryptedData
const OID_KEY_BAG: &[u64] = &[1, 2, 840, 113549, 1, 12, 10, 1, 1];
const OID_SHROUDED_KEY_BAG: &[u64] = &[1, 2, 840, 113549, 1, 12, 10, 1, 2];
const OID_CERT_BAG: &[u64] = &[1, 2, 840, 113549, 1, 12, 10, 1, 3];
const OID_X509_CERTIFICATE: &[u64] = &[1, 2, 840, 113549, 1, 9, 22, 1];
const OID_RSA_ENCRYPTION: &[u64] = &[1, 2, 840, 113549, 1, 1, 1];
const OID_PBES2: &[u64] = &[1, 2, 840, 113549, 1, 5, 13];
const OID_PBKDF2: &[u64] = &[1, 2, 840, 113549, 1, 5, 12];
const OID_HMAC_SHA256: &[u64] = &[1, 2, 840, 113549, 2, 9];
const OID_PBE_SHA1_3DES: &[u64] = &[1, 2, 840, 113549, 1, 12, 1, 3];
const OID_PBE_SHA1_RC2_40: &[u64] = &[1, 2, 840, 113549, 1, 12, 1, 6];
const OID_AES128_CBC: &[u64] = &[2, 16, 840, 1, 101, 3, 4, 1, 2];
const OID_AES192_CBC: &[u64] = &[2, 16, 840, 1, 101, 3, 4, 1, 22];
const OID_AES256_CBC: &[u64] = &[2, 16, 840, 1, 101, 3, 4, 1, 42];
const OID_SHA1: &[u64] = &[1, 3, 14, 3, 2, 26];
const OID_SHA256: &[u64] = &[2, 16, 840, 1, 101, 3, 4, 2, 1];

/// An identity imported from a `.p12`/`.pfx`: the RSA private key plus the DER
/// X.509 certificate(s) (the signer/leaf certificate first, as OpenSSL emits).
#[derive(Debug, Clone)]
pub struct Pkcs12Identity {
    pub key: RsaPrivateKey,
    pub certificates: Vec<Vec<u8>>,
}

/// Why a PKCS#12 import failed. Deliberately coarse: the caller collapses every
/// variant into one generic "invalid certificate or password" message so an
/// attacker can't tell a wrong password from a malformed file (anti-enumeration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pkcs12Error {
    /// The ASN.1 structure didn't parse.
    Malformed,
    /// The integrity MAC didn't match — wrong password or a tampered file.
    MacMismatch,
    /// Parsed fine but carried no private key we could decrypt.
    NoPrivateKey,
}

/// Import `pfx` using `password`, returning the key and certificate chain.
pub fn parse(pfx: &[u8], password: &str) -> Result<Pkcs12Identity, Pkcs12Error> {
    use Pkcs12Error::*;

    // PFX ::= SEQUENCE { version INTEGER, authSafe ContentInfo, macData OPTIONAL }
    let mut top = Reader::new(pfx);
    let mut pfx_seq = top.descend(TAG_SEQUENCE).ok_or(Malformed)?;
    pfx_seq.next_tag(TAG_INTEGER).ok_or(Malformed)?; // version

    // authSafe = ContentInfo { OID pkcs7-data, [0] EXPLICIT OCTET STRING }.
    // The OCTET STRING content is the AuthenticatedSafe DER — and the exact
    // bytes the integrity MAC is computed over.
    let auth_safe_ci = pfx_seq.next_tag(TAG_SEQUENCE).ok_or(Malformed)?;
    let mut ci = auth_safe_ci.reader();
    let content_type = ci.next_tag(TAG_OID).ok_or(Malformed)?;
    if !content_type.is_oid(OID_DATA) {
        return Err(Malformed);
    }
    let mut explicit = ci.descend(TAG_CONTEXT_0).ok_or(Malformed)?;
    let auth_safe = explicit.next_tag(TAG_OCTET_STRING).ok_or(Malformed)?.content;

    // macData (optional) — verify before trusting any decrypted content.
    if let Some(mac_data) = pfx_seq.read() {
        if mac_data.tag == TAG_SEQUENCE && verify_mac(mac_data, auth_safe, password) != Some(true) {
            return Err(MacMismatch);
        }
    }

    // AuthenticatedSafe ::= SEQUENCE OF ContentInfo.
    let mut certificates: Vec<Vec<u8>> = Vec::new();
    let mut key: Option<RsaPrivateKey> = None;

    let mut authsafe = Reader::new(auth_safe);
    let mut authsafe_seq = authsafe.descend(TAG_SEQUENCE).ok_or(Malformed)?;
    while let Some(content_info) = authsafe_seq.read() {
        if content_info.tag != TAG_SEQUENCE {
            continue;
        }
        let mut cinfo = content_info.reader();
        let Some(oid) = cinfo.next_tag(TAG_OID) else {
            continue;
        };
        let Some(mut body) = cinfo.descend(TAG_CONTEXT_0) else {
            continue;
        };

        let safe_contents = if oid.is_oid(OID_DATA) {
            // [0] OCTET STRING = plaintext SafeContents DER.
            match body.next_tag(TAG_OCTET_STRING) {
                Some(t) => t.content.to_vec(),
                None => continue,
            }
        } else if oid.is_oid(OID_ENCRYPTED_DATA) {
            // [0] EncryptedData → decrypt to SafeContents DER (skip on an
            // unsupported cipher, e.g. a legacy RC2-encrypted cert bag).
            match decrypt_encrypted_data(&mut body, password) {
                Some(d) => d,
                None => continue,
            }
        } else {
            continue;
        };

        collect_bags(&safe_contents, password, &mut certificates, &mut key);
    }

    Ok(Pkcs12Identity {
        key: key.ok_or(NoPrivateKey)?,
        certificates,
    })
}

/// Verify the PKCS#12 integrity MAC over `auth_safe`. `None` = unknown MAC hash.
fn verify_mac(mac_data: Tlv, auth_safe: &[u8], password: &str) -> Option<bool> {
    // MacData ::= SEQUENCE { mac DigestInfo, macSalt OCTET STRING, iterations INTEGER DEFAULT 1 }
    let mut md = mac_data.reader();
    let digest_info = md.next_tag(TAG_SEQUENCE)?;
    let mac_salt = md.next_tag(TAG_OCTET_STRING)?;
    let iterations = match md.read() {
        Some(t) if t.tag == TAG_INTEGER => be_to_u32(t.content),
        _ => 1,
    };

    // DigestInfo ::= SEQUENCE { digestAlgorithm AlgId, digest OCTET STRING }
    let mut di = digest_info.reader();
    let alg = di.next_tag(TAG_SEQUENCE)?;
    let digest = di.next_tag(TAG_OCTET_STRING)?;
    let hash_oid = alg.reader().next_tag(TAG_OID)?;

    let pw = bmp_string(password);
    let computed = if hash_oid.is_oid(OID_SHA256) {
        let key = pkcs12_kdf_sha256(3, &pw, mac_salt.content, iterations, 32);
        hmac_sha256(&key, auth_safe).to_vec()
    } else if hash_oid.is_oid(OID_SHA1) {
        let key = pkcs12_kdf_sha1(3, &pw, mac_salt.content, iterations, 20);
        hmac_sha1(&key, auth_safe).to_vec()
    } else {
        return None;
    };
    Some(computed == digest.content)
}

/// Decrypt an `EncryptedData` ContentInfo body (reader positioned just inside
/// its `[0]`) to the plaintext SafeContents DER.
fn decrypt_encrypted_data(body: &mut Reader, password: &str) -> Option<Vec<u8>> {
    // EncryptedData ::= SEQUENCE { version INTEGER, EncryptedContentInfo }
    let enc_data = body.next_tag(TAG_SEQUENCE)?;
    let mut ed = enc_data.reader();
    ed.next_tag(TAG_INTEGER)?; // version
    // EncryptedContentInfo ::= SEQUENCE { contentType OID, contentEncryptionAlgorithm,
    //                                     [0] IMPLICIT OCTET STRING encryptedContent }
    let eci = ed.next_tag(TAG_SEQUENCE)?;
    let mut e = eci.reader();
    e.next_tag(TAG_OID)?; // contentType (data)
    let alg = e.next_tag(TAG_SEQUENCE)?;
    let ciphertext = e.next_tag(TAG_CONTEXT_0_PRIM)?;
    decrypt_pbe(alg, ciphertext.content, password)
}

/// Walk one SafeContents, appending certificates and capturing the first key.
fn collect_bags(
    safe_contents: &[u8],
    password: &str,
    certs: &mut Vec<Vec<u8>>,
    key: &mut Option<RsaPrivateKey>,
) {
    let mut r = Reader::new(safe_contents);
    let Some(mut seq) = r.descend(TAG_SEQUENCE) else {
        return;
    };
    // SafeBag ::= SEQUENCE { bagId OID, bagValue [0] EXPLICIT, bagAttributes SET OPTIONAL }
    while let Some(bag) = seq.read() {
        if bag.tag != TAG_SEQUENCE {
            continue;
        }
        let mut b = bag.reader();
        let Some(bag_id) = b.next_tag(TAG_OID) else {
            continue;
        };
        let Some(mut value) = b.descend(TAG_CONTEXT_0) else {
            continue;
        };

        if bag_id.is_oid(OID_CERT_BAG) {
            if let Some(cert) = parse_cert_bag(&mut value) {
                certs.push(cert);
            }
        } else if bag_id.is_oid(OID_KEY_BAG) {
            // bagValue = PrivateKeyInfo (plaintext PKCS#8).
            if key.is_none() {
                if let Some(pki) = value.next_tag(TAG_SEQUENCE) {
                    *key = rsa_from_pkcs8_content(pki.content);
                }
            }
        } else if bag_id.is_oid(OID_SHROUDED_KEY_BAG) {
            // bagValue = EncryptedPrivateKeyInfo ::= SEQUENCE { AlgId, OCTET STRING }.
            if key.is_none() {
                if let Some(epki) = value.next_tag(TAG_SEQUENCE) {
                    let mut ep = epki.reader();
                    if let (Some(alg), Some(enc)) =
                        (ep.next_tag(TAG_SEQUENCE), ep.next_tag(TAG_OCTET_STRING))
                    {
                        if let Some(pkcs8) = decrypt_pbe(alg, enc.content, password) {
                            *key = rsa_from_pkcs8(&pkcs8);
                        }
                    }
                }
            }
        }
    }
}

/// `CertBag ::= SEQUENCE { certId OID, certValue [0] EXPLICIT OCTET STRING }` → DER X.509.
fn parse_cert_bag(value: &mut Reader) -> Option<Vec<u8>> {
    let cert_bag = value.next_tag(TAG_SEQUENCE)?;
    let mut cb = cert_bag.reader();
    let cert_id = cb.next_tag(TAG_OID)?;
    if !cert_id.is_oid(OID_X509_CERTIFICATE) {
        return None;
    }
    let mut explicit = cb.descend(TAG_CONTEXT_0)?;
    Some(explicit.next_tag(TAG_OCTET_STRING)?.content.to_vec())
}

// ─── PBE decryption (PBES1 3DES / PBES2 AES) ─────────────────────────────────

/// Decrypt `ciphertext` given a content-encryption `AlgorithmIdentifier`.
/// `None` for any scheme we don't implement (e.g. RC2).
fn decrypt_pbe(alg: Tlv, ciphertext: &[u8], password: &str) -> Option<Vec<u8>> {
    let mut a = alg.reader();
    let scheme = a.next_tag(TAG_OID)?;
    if scheme.is_oid(OID_PBES2) {
        decrypt_pbes2(&mut a, ciphertext, password)
    } else if scheme.is_oid(OID_PBE_SHA1_3DES) {
        decrypt_pbes1_3des(&mut a, ciphertext, password)
    } else if scheme.is_oid(OID_PBE_SHA1_RC2_40) {
        decrypt_pbes1_rc2_40(&mut a, ciphertext, password)
    } else {
        None
    }
}

/// PBES1 `pbeWithSHAAnd40BitRC2-CBC`: PKCS#12 KDF (SHA-1, BMPString) → 5-byte
/// key (id 1) + IV (id 2), then RC2-40-CBC. Legacy cert bags (OpenSSL `-legacy`).
fn decrypt_pbes1_rc2_40(params: &mut Reader, ciphertext: &[u8], password: &str) -> Option<Vec<u8>> {
    // PKCS12PBEParams ::= SEQUENCE { salt OCTET STRING, iterations INTEGER }
    let p = params.next_tag(TAG_SEQUENCE)?;
    let mut pr = p.reader();
    let salt = pr.next_tag(TAG_OCTET_STRING)?;
    let iter = be_to_u32(pr.next_tag(TAG_INTEGER)?.content);

    let pw = bmp_string(password);
    let key = pkcs12_kdf_sha1(1, &pw, salt.content, iter, 5); // 40-bit key
    let iv = pkcs12_kdf_sha1(2, &pw, salt.content, iter, 8);
    let plain = rc2_cbc_decrypt(&key, 40, &iv, ciphertext)?;
    strip_pkcs7(plain, 8)
}

/// PBES1 `pbeWithSHAAnd3-KeyTripleDES-CBC`: PKCS#12 KDF (SHA-1, BMPString) → key
/// (id 1) + IV (id 2), then 3DES-CBC.
fn decrypt_pbes1_3des(params: &mut Reader, ciphertext: &[u8], password: &str) -> Option<Vec<u8>> {
    // PKCS12PBEParams ::= SEQUENCE { salt OCTET STRING, iterations INTEGER }
    let p = params.next_tag(TAG_SEQUENCE)?;
    let mut pr = p.reader();
    let salt = pr.next_tag(TAG_OCTET_STRING)?;
    let iter = be_to_u32(pr.next_tag(TAG_INTEGER)?.content);

    let pw = bmp_string(password);
    let key = pkcs12_kdf_sha1(1, &pw, salt.content, iter, 24);
    let iv = pkcs12_kdf_sha1(2, &pw, salt.content, iter, 8);
    let plain = des3_cbc_decrypt(&key, &iv, ciphertext)?;
    strip_pkcs7(plain, 8)
}

/// PBES2: PBKDF2 (raw-password bytes) → key, then AES-CBC.
fn decrypt_pbes2(params: &mut Reader, ciphertext: &[u8], password: &str) -> Option<Vec<u8>> {
    // PBES2-params ::= SEQUENCE { keyDerivationFunc AlgId, encryptionScheme AlgId }
    let p = params.next_tag(TAG_SEQUENCE)?;
    let mut pr = p.reader();
    let kdf = pr.next_tag(TAG_SEQUENCE)?;
    let enc = pr.next_tag(TAG_SEQUENCE)?;

    // keyDerivationFunc = SEQUENCE { OID pbkdf2, PBKDF2-params }.
    let mut kr = kdf.reader();
    if !kr.next_tag(TAG_OID)?.is_oid(OID_PBKDF2) {
        return None;
    }
    let kp = kr.next_tag(TAG_SEQUENCE)?;
    // PBKDF2-params ::= SEQUENCE { salt OCTET STRING, iterationCount INTEGER,
    //                              keyLength INTEGER OPTIONAL, prf AlgId DEFAULT hmacSHA1 }
    let mut kpr = kp.reader();
    let salt = kpr.next_tag(TAG_OCTET_STRING)?;
    let iter = be_to_u32(kpr.next_tag(TAG_INTEGER)?.content);
    let mut key_len_opt: Option<usize> = None;
    let mut prf_sha256 = false; // default hmacSHA1
    while let Some(t) = kpr.read() {
        match t.tag {
            TAG_INTEGER => key_len_opt = Some(be_to_u32(t.content) as usize),
            TAG_SEQUENCE => {
                if let Some(prf_oid) = t.reader().next_tag(TAG_OID) {
                    prf_sha256 = prf_oid.is_oid(OID_HMAC_SHA256);
                }
            }
            _ => {}
        }
    }

    // encryptionScheme = SEQUENCE { OID aes-cbc, OCTET STRING iv }.
    let mut er = enc.reader();
    let cipher_oid = er.next_tag(TAG_OID)?;
    let iv_tlv = er.next_tag(TAG_OCTET_STRING)?;
    let cipher_key_size = if cipher_oid.is_oid(OID_AES128_CBC) {
        16
    } else if cipher_oid.is_oid(OID_AES192_CBC) {
        24
    } else if cipher_oid.is_oid(OID_AES256_CBC) {
        32
    } else {
        return None;
    };
    let key_size = key_len_opt.unwrap_or(cipher_key_size);
    if iv_tlv.content.len() != 16 {
        return None;
    }
    let mut iv = [0u8; 16];
    iv.copy_from_slice(iv_tlv.content);

    // PBES2 derives the key from the *raw* password bytes (not the BMPString).
    let key = if prf_sha256 {
        pbkdf2_hmac_sha256(password.as_bytes(), salt.content, iter, key_size)
    } else {
        pbkdf2_hmac_sha1(password.as_bytes(), salt.content, iter, key_size)
    };
    let plain = aes_cbc_decrypt(&key, &iv, ciphertext);
    strip_pkcs7(plain, 16)
}

// ─── PKCS#8 / PKCS#1 key extraction ──────────────────────────────────────────

/// Parse a PKCS#8 `PrivateKeyInfo` DER (with its outer SEQUENCE) into an RSA key.
fn rsa_from_pkcs8(der: &[u8]) -> Option<RsaPrivateKey> {
    let mut r = Reader::new(der);
    let pki = r.next_tag(TAG_SEQUENCE)?;
    rsa_from_pkcs8_content(pki.content)
}

/// Parse the *content* of a PKCS#8 `PrivateKeyInfo` SEQUENCE into an RSA key.
fn rsa_from_pkcs8_content(content: &[u8]) -> Option<RsaPrivateKey> {
    // PrivateKeyInfo ::= SEQUENCE { version INTEGER, algId AlgId, privateKey OCTET STRING }
    let mut pki = Reader::new(content);
    pki.next_tag(TAG_INTEGER)?; // version
    let alg = pki.next_tag(TAG_SEQUENCE)?;
    if !alg.reader().next_tag(TAG_OID)?.is_oid(OID_RSA_ENCRYPTION) {
        return None;
    }
    let private_key = pki.next_tag(TAG_OCTET_STRING)?;
    rsa_from_pkcs1(private_key.content)
}

/// Parse a PKCS#1 `RSAPrivateKey` DER, keeping only `n`, `e`, `d`.
fn rsa_from_pkcs1(der: &[u8]) -> Option<RsaPrivateKey> {
    // RSAPrivateKey ::= SEQUENCE { version, n, e, d, p, q, dp, dq, qinv }
    let mut r = Reader::new(der);
    let mut k = r.descend(TAG_SEQUENCE)?;
    k.next_tag(TAG_INTEGER)?; // version
    let n = k.next_tag(TAG_INTEGER)?;
    let e = k.next_tag(TAG_INTEGER)?;
    let d = k.next_tag(TAG_INTEGER)?;
    Some(RsaPrivateKey::from_components(n.content, e.content, d.content))
}

// ─── Small helpers ───────────────────────────────────────────────────────────

/// Big-endian bytes → u32 (DER INTEGERs for iteration counts fit comfortably).
fn be_to_u32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b))
}

/// Strip PKCS#7 padding for a given `block` size; `None` if the padding is invalid.
fn strip_pkcs7(mut data: Vec<u8>, block: usize) -> Option<Vec<u8>> {
    let pad = *data.last()? as usize;
    if pad == 0 || pad > block || pad > data.len() {
        return None;
    }
    if data[data.len() - pad..].iter().all(|&b| b as usize == pad) {
        data.truncate(data.len() - pad);
        Some(data)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real OpenSSL-3 fixtures: a 1024-bit RSA key + self-signed cert, exported
    // both modern (PBES2/AES-256 + HMAC-SHA256) and legacy (PBES1/3DES key,
    // RC2-40 certs + HMAC-SHA1). Password "gigapdf". See the shell that built
    // them in the loop notes; regenerate with `openssl pkcs12 -export …`.
    const MODERN_P12: &[u8] = include_bytes!("fixtures/modern.p12");
    const LEGACY_P12: &[u8] = include_bytes!("fixtures/legacy.p12");
    const CERT_DER: &[u8] = include_bytes!("fixtures/cert.der");
    const PASSWORD: &str = "gigapdf";

    // The modulus `n` printed by `openssl rsa -modulus` for the fixture key.
    const EXPECTED_N: &str = "da3dd665e63b0748ff50a2f158ccfc6615183c03149d3d5b747b38afb6758ffb\
        cfd3097ed7184282c282bea7c4c4f9c011151228b8f8aabe2c5964e6f3a31af77\
        95270a06365b11daf72f8f4134f99517bae14bb3f4a23960880a4c07eb6e3b7f0\
        1328ed1ebddf1adb1de7d8d65e0862af396bd7ef660b7c0e06309bd1af0f2b";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn imports_modern_pbes2_aes_identity() {
        let id = parse(MODERN_P12, PASSWORD).expect("modern .p12 imports");
        // The recovered modulus matches the key OpenSSL exported.
        assert_eq!(hex(&id.key.n.to_bytes_be()), EXPECTED_N);
        assert_eq!(id.key.modulus_len, 128, "1024-bit modulus");
        // PBES2-encrypted cert bag decrypts to the exact DER certificate.
        assert_eq!(id.certificates.len(), 1);
        assert_eq!(id.certificates[0], CERT_DER);
        // The imported key actually signs (recovers its own digest).
        let sig = id.key.sign_sha256(b"contract.pdf");
        assert_eq!(sig.len(), 128);
    }

    #[test]
    fn imports_legacy_pbes1_3des_key_and_rc2_cert() {
        // Legacy export: 3DES key bag + RC2-40 cert bag + HMAC-SHA1 MAC. Both
        // ciphers are supported, so key AND certificate come through.
        let id = parse(LEGACY_P12, PASSWORD).expect("legacy .p12 imports");
        assert_eq!(hex(&id.key.n.to_bytes_be()), EXPECTED_N);
        assert_eq!(id.certificates.len(), 1);
        assert_eq!(id.certificates[0], CERT_DER);
    }

    #[test]
    fn wrong_password_is_rejected_by_the_mac() {
        assert!(matches!(parse(MODERN_P12, "wrong"), Err(Pkcs12Error::MacMismatch)));
        assert!(matches!(parse(LEGACY_P12, "nope"), Err(Pkcs12Error::MacMismatch)));
    }

    #[test]
    fn garbage_is_malformed_not_a_panic() {
        assert!(matches!(parse(b"not a pfx at all", PASSWORD), Err(Pkcs12Error::Malformed)));
        assert!(matches!(
            parse(&[0x30, 0x03, 0x02, 0x01, 0x03], PASSWORD),
            Err(Pkcs12Error::Malformed)
        ));
    }
}
