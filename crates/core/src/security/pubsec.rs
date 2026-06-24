//! Public-key (certificate) security handler — ISO 32000-1 §7.6.5.
//!
//! Encrypts a PDF to one or more X.509 recipients: a random 20-byte *seed* plus
//! the permission bits are wrapped, per recipient, in a CMS `EnvelopedData`
//! (PKCS#7) under the recipient's RSA public key. The file encryption key is
//! `Hash(seed || every recipient blob)`, and document objects are then encrypted
//! with the very same AESV2/AESV3 machinery as the password handler (a
//! seed-derived [`Security`]). Only a holder of a recipient private key can
//! recover the seed and open the file. `/SubFilter /adbe.pkcs7.s5` (recipients
//! carried inside the crypt filter). Built on RustCrypto `cms`/`rsa`/`x509-cert`.

use cms::builder::{
    ContentEncryptionAlgorithm, EnvelopedDataBuilder, KeyEncryptionInfo,
    KeyTransRecipientInfoBuilder,
};
use cms::cert::IssuerAndSerialNumber;
use cms::content_info::ContentInfo;
use cms::enveloped_data::{EnvelopedData, RecipientIdentifier, RecipientInfo};
use der::asn1::OctetString;
use der::{Any, Decode, Encode};
use rand::rngs::StdRng;
use rand::SeedableRng;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use x509_cert::Certificate;

use super::{Method, Security};
use crate::crypto::rsa::RsaPrivateKey;
use crate::crypto::{aes_cbc_decrypt, sha1, sha256};
use crate::object::{Dictionary, Object, StringKind};

/// A fresh deterministic CSPRNG seeded from `seed` and a `counter` (so the many
/// independent RNGs a CMS build needs are all reproducible from one host seed).
fn derive_rng(seed: &[u8], counter: u8) -> StdRng {
    let mut input = seed.to_vec();
    input.push(counter);
    StdRng::from_seed(sha256(&input))
}

/// The recipient's RSA public key, read from a certificate's `SubjectPublicKeyInfo`
/// (the PKCS#1 `RSAPublicKey` carried in the SPKI bit string).
fn cert_rsa_public_key(cert: &Certificate) -> Option<RsaPublicKey> {
    let spki = &cert.tbs_certificate.subject_public_key_info;
    let key_der = spki.subject_public_key.as_bytes()?;
    RsaPublicKey::from_pkcs1_der(key_der).ok()
}

/// `IssuerAndSerialNumber` identifying a recipient by its certificate.
fn issuer_and_serial(cert: &Certificate) -> IssuerAndSerialNumber {
    IssuerAndSerialNumber {
        issuer: cert.tbs_certificate.issuer.clone(),
        serial_number: cert.tbs_certificate.serial_number.clone(),
    }
}

/// The 24-byte enveloped block: 20-byte `seed` followed by the 4 permission
/// bytes (big-endian), per the public-key handler.
fn seed_block(seed20: &[u8], permissions: i32) -> Vec<u8> {
    let mut block = seed20[..20].to_vec();
    block.extend_from_slice(&(permissions as u32).to_be_bytes());
    block
}

/// File encryption key: `Hash(seed || every recipient blob [|| 0xFFFFFFFF if the
/// metadata is left in the clear])`. SHA-1 → 16 bytes for AESV2, SHA-256 → 32
/// bytes for AESV3.
fn derive_file_key(
    seed20: &[u8],
    recipient_blobs: &[Vec<u8>],
    aes256: bool,
    encrypt_metadata: bool,
) -> Vec<u8> {
    let mut input = seed20[..20].to_vec();
    for blob in recipient_blobs {
        input.extend_from_slice(blob);
    }
    if !encrypt_metadata {
        input.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    }
    if aes256 {
        sha256(&input).to_vec()
    } else {
        sha1(&input)[..16].to_vec()
    }
}

fn name(value: &[u8]) -> Object {
    Object::Name(value.to_vec())
}

/// Build the `/Adobe.PubSec` `/Encrypt` dictionary (`/SubFilter /adbe.pkcs7.s5`):
/// the recipient CMS blobs live in the default crypt filter as hex strings.
fn encrypt_dict(recipient_blobs: &[Vec<u8>], aes256: bool, encrypt_metadata: bool) -> Dictionary {
    let (cfm, length, v): (&[u8], i64, i64) = if aes256 {
        (b"AESV3", 32, 5)
    } else {
        (b"AESV2", 16, 4)
    };
    let recipients = Object::Array(
        recipient_blobs
            .iter()
            .map(|b| Object::String(b.clone(), StringKind::Hex))
            .collect(),
    );

    let mut default_cf = Dictionary::new();
    default_cf.set(b"CFM".to_vec(), name(cfm));
    default_cf.set(b"Length".to_vec(), Object::Integer(length));
    default_cf.set(b"AuthEvent".to_vec(), name(b"DocOpen"));
    default_cf.set(b"Recipients".to_vec(), recipients);
    if !encrypt_metadata {
        default_cf.set(b"EncryptMetadata".to_vec(), Object::Boolean(false));
    }
    let mut cf = Dictionary::new();
    cf.set(
        b"DefaultCryptFilter".to_vec(),
        Object::Dictionary(default_cf),
    );

    let mut dict = Dictionary::new();
    dict.set(b"Filter".to_vec(), name(b"Adobe.PubSec"));
    dict.set(b"SubFilter".to_vec(), name(b"adbe.pkcs7.s5"));
    dict.set(b"V".to_vec(), Object::Integer(v));
    dict.set(b"Length".to_vec(), Object::Integer(length * 8));
    dict.set(b"CF".to_vec(), Object::Dictionary(cf));
    dict.set(b"StmF".to_vec(), name(b"DefaultCryptFilter"));
    dict.set(b"StrF".to_vec(), name(b"DefaultCryptFilter"));
    if !encrypt_metadata {
        dict.set(b"EncryptMetadata".to_vec(), Object::Boolean(false));
    }
    dict
}

/// Wrap a built [`EnvelopedData`] in a `ContentInfo` and DER-encode it (the bytes
/// stored in `/Recipients`).
fn envelope_one(
    content: &[u8],
    cert: &Certificate,
    aes256: bool,
    rng_seed: &[u8],
    index: usize,
) -> Option<Vec<u8>> {
    let alg = if aes256 {
        ContentEncryptionAlgorithm::Aes256Cbc
    } else {
        ContentEncryptionAlgorithm::Aes128Cbc
    };
    let pubkey = cert_rsa_public_key(cert)?;
    let rid = RecipientIdentifier::IssuerAndSerialNumber(issuer_and_serial(cert));

    // Two independent RNGs: one held by the recipient builder (RSA key wrap), one
    // consumed by `build_with_rng` (content key + IV). They must outlive the
    // EnvelopedDataBuilder, hence separate `let` bindings.
    let mut rng_recipient = derive_rng(rng_seed, (index * 2) as u8);
    let mut rng_content = derive_rng(rng_seed, (index * 2 + 1) as u8);

    let recipient =
        KeyTransRecipientInfoBuilder::new(rid, KeyEncryptionInfo::Rsa(pubkey), &mut rng_recipient)
            .ok()?;
    let mut builder = EnvelopedDataBuilder::new(None, content, alg, None).ok()?;
    builder.add_recipient_info(recipient).ok()?;
    let enveloped = builder.build_with_rng(&mut rng_content).ok()?;

    let content_info = ContentInfo {
        content_type: const_oid::db::rfc5911::ID_ENVELOPED_DATA,
        content: Any::encode_from(&enveloped).ok()?,
    };
    content_info.to_der().ok()
}

/// Encrypt to `cert_ders` (DER X.509 recipients). `seed20` is 20 bytes of host
/// randomness (the shared seed), `rng_seed` ≥ 32 bytes more (CMS nonce material);
/// the engine has no RNG of its own. Returns the [`Security`] context (to encrypt
/// objects) and the `/Encrypt` dictionary.
pub fn encrypt_for_recipients(
    cert_ders: &[Vec<u8>],
    permissions: i32,
    encrypt_metadata: bool,
    aes256: bool,
    seed20: &[u8],
    rng_seed: &[u8],
) -> Option<(Security, Dictionary)> {
    if cert_ders.is_empty() || seed20.len() < 20 || rng_seed.len() < 32 {
        return None;
    }
    let content = seed_block(seed20, permissions);

    let mut blobs = Vec::with_capacity(cert_ders.len());
    for (i, der) in cert_ders.iter().enumerate() {
        let cert = Certificate::from_der(der).ok()?;
        blobs.push(envelope_one(&content, &cert, aes256, rng_seed, i)?);
    }

    let key = derive_file_key(seed20, &blobs, aes256, encrypt_metadata);
    let security = Security {
        method: if aes256 { Method::AesV3 } else { Method::AesV2 },
        key,
    };
    Some((security, encrypt_dict(&blobs, aes256, encrypt_metadata)))
}

/// Strip PKCS#7 padding (CMS content encryption) from a CBC plaintext.
fn unpad_pkcs7(mut data: Vec<u8>) -> Option<Vec<u8>> {
    let pad = *data.last()? as usize;
    if pad == 0 || pad > 16 || pad > data.len() {
        return None;
    }
    if data[data.len() - pad..].iter().any(|&b| b as usize != pad) {
        return None;
    }
    data.truncate(data.len() - pad);
    Some(data)
}

/// The recipient CMS blobs stored in `/CF /DefaultCryptFilter /Recipients`, plus
/// whether the handler is AESV3.
fn read_recipients(encrypt: &Dictionary) -> Option<(Vec<Vec<u8>>, bool, bool)> {
    let cf = encrypt.get(b"CF").and_then(Object::as_dict)?;
    let dcf = cf
        .get(b"DefaultCryptFilter")
        .and_then(Object::as_dict)
        .or_else(|| cf.get(b"DefCryptFilter").and_then(Object::as_dict))?;
    let aes256 = matches!(dcf.get(b"CFM").and_then(Object::as_name), Some(b"AESV3"));
    let encrypt_metadata = match encrypt
        .get(b"EncryptMetadata")
        .or_else(|| dcf.get(b"EncryptMetadata"))
    {
        Some(Object::Boolean(b)) => *b,
        _ => true,
    };
    let recipients = dcf.get(b"Recipients").and_then(Object::as_array)?;
    let mut blobs = Vec::new();
    for item in recipients {
        if let Object::String(bytes, _) = item {
            blobs.push(bytes.clone());
        }
    }
    if blobs.is_empty() {
        return None;
    }
    Some((blobs, aes256, encrypt_metadata))
}

/// Recover the file key from a recipient blob the holder of `private_key` can
/// open: RSA-unwrap the content key, AES-CBC-decrypt the seed block.
fn recover_seed(blob: &[u8], cert: &Certificate, private_key: &RsaPrivateKey) -> Option<Vec<u8>> {
    let content_info = ContentInfo::from_der(blob).ok()?;
    let enveloped: EnvelopedData = content_info.content.decode_as().ok()?;
    let want = issuer_and_serial(cert);

    // Find our recipient info and RSA-decrypt the content-encryption key.
    let mut cek: Option<Vec<u8>> = None;
    for ri in enveloped.recip_infos.0.iter() {
        if let RecipientInfo::Ktri(ktri) = ri {
            if let RecipientIdentifier::IssuerAndSerialNumber(isn) = &ktri.rid {
                if isn.issuer == want.issuer && isn.serial_number == want.serial_number {
                    cek = private_key
                        .inner()
                        .decrypt(Pkcs1v15Encrypt, ktri.enc_key.as_bytes())
                        .ok();
                    break;
                }
            }
        }
    }
    let cek = cek?;

    // AES-CBC-decrypt the enveloped content (IV in the algorithm parameters).
    let eci = &enveloped.encrypted_content;
    let iv_any: &Any = eci.content_enc_alg.parameters.as_ref()?;
    let iv_os: OctetString = iv_any.decode_as().ok()?;
    let iv: [u8; 16] = iv_os.as_bytes().try_into().ok()?;
    let ciphertext = eci.encrypted_content.as_ref()?.as_bytes();
    let plain = aes_cbc_decrypt(&cek, &iv, ciphertext);
    let block = unpad_pkcs7(plain)?;
    if block.len() < 20 {
        return None;
    }
    Some(block[..20].to_vec())
}

/// Open a public-key-encrypted document: with the recipient's `cert_der` +
/// `key_der` (PKCS#1 RSA private key), recover the seed and rebuild the
/// [`Security`] context that decrypts the objects. `None` if this key is not a
/// recipient or the handler is unsupported.
pub fn open_pubsec(encrypt: &Dictionary, cert_der: &[u8], key_der: &[u8]) -> Option<Security> {
    let (blobs, aes256, encrypt_metadata) = read_recipients(encrypt)?;
    let cert = Certificate::from_der(cert_der).ok()?;
    let private_key = RsaPrivateKey::from_pkcs1_der(key_der)?;

    // Recover the seed from whichever blob is addressed to us.
    let seed = blobs
        .iter()
        .find_map(|blob| recover_seed(blob, &cert, &private_key))?;

    let key = derive_file_key(&seed, &blobs, aes256, encrypt_metadata);
    Some(Security {
        method: if aes256 { Method::AesV3 } else { Method::AesV2 },
        key,
    })
}
