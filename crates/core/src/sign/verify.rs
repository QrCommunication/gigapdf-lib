//! PDF signature **verification** (ISO 32000-1 §12.8.1) — the inverse of the
//! signing stack. Given the detached CMS (`/Contents`) and the bytes it covers
//! (the `/ByteRange` slice), check that the embedded `messageDigest` matches the
//! content and that the SignerInfo signature validates under the signer's public
//! key. RSA + SHA-256 (what this engine produces) is verified; other algorithms
//! are reported as unsupported rather than silently passing.

use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerIdentifier};
use const_oid::ObjectIdentifier;
use der::{Decode, Encode};
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use rsa::RsaPublicKey;
use sha2::{Digest, Sha256};
use x509_cert::Certificate;

/// PKCS#9 `messageDigest` (1.2.840.113549.1.9.4).
const OID_MESSAGE_DIGEST: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");
/// PKCS#1 `sha256WithRSAEncryption` / `rsaEncryption` family — what we verify.
const OID_RSA_ENCRYPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const OID_SHA256_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11");

/// The result of cryptographically checking one CMS signature against the bytes
/// it covers.
#[derive(Debug, Clone, Default)]
pub struct CmsVerification {
    /// The `messageDigest` signed attribute equals SHA-256 of the covered bytes.
    pub digest_ok: bool,
    /// The SignerInfo signature validates under the signer certificate's key.
    pub signature_ok: bool,
    /// Number of certificates embedded in the CMS (the chain, signer first-ish).
    pub cert_count: usize,
    /// The signer certificate's Common Name (`CN=`), if present.
    pub signer_common_name: Option<String>,
    /// The signature algorithm we recognised (`RSA+SHA-256`), or a note that it
    /// is unsupported by this verifier.
    pub algorithm: String,
}

/// Verify a detached CMS `cms_der` over `content` (the `/ByteRange` bytes).
/// Never panics — a malformed CMS yields `digest_ok = signature_ok = false`.
pub fn verify_detached_cms(cms_der: &[u8], content: &[u8]) -> CmsVerification {
    let mut out = CmsVerification {
        algorithm: "unknown".into(),
        ..Default::default()
    };
    let Some(signed) = parse_signed_data(cms_der) else {
        return out;
    };

    // The embedded certificates (the chain).
    let certs: Vec<Certificate> = signed
        .certificates
        .as_ref()
        .map(|set| {
            set.0
                .iter()
                .filter_map(|c| match c {
                    cms::cert::CertificateChoices::Certificate(cert) => Some(cert.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    out.cert_count = certs.len();

    let Some(signer) = signed.signer_infos.0.as_slice().first() else {
        return out;
    };

    // 1. messageDigest signed attribute must equal SHA-256(content).
    let want = Sha256::digest(content);
    if let Some(attrs) = signer.signed_attrs.as_ref() {
        for attr in attrs.iter() {
            if attr.oid == OID_MESSAGE_DIGEST {
                if let Some(val) = attr.values.as_slice().first() {
                    // The value is an OCTET STRING; its DER content is the digest.
                    if let Ok(os) = val.decode_as::<der::asn1::OctetString>() {
                        out.digest_ok = os.as_bytes() == want.as_slice();
                    }
                }
            }
        }
    } else {
        // No signed attributes: the signature is directly over the content.
        // (PAdES always uses signed attrs; treat this as digest-not-applicable.)
        out.digest_ok = false;
    }

    // 2. Recognise the algorithm and locate the signer certificate.
    let alg = signer.signature_algorithm.oid;
    if alg != OID_RSA_ENCRYPTION && alg != OID_SHA256_RSA {
        out.algorithm = format!("unsupported ({alg})");
        return out;
    }
    out.algorithm = "RSA+SHA-256".into();
    let Some(cert) = pick_signer_cert(&certs, &signer.sid) else {
        return out;
    };
    out.signer_common_name = common_name(&cert);

    // 3. Verify the RSA signature over the DER of the signed attributes
    //    (re-encoded as an explicit SET OF, per CMS §5.4).
    let signed_bytes = match signer.signed_attrs.as_ref() {
        Some(attrs) => match attrs.to_der() {
            Ok(d) => d,
            Err(_) => return out,
        },
        None => content.to_vec(),
    };
    let spki_der = match cert.tbs_certificate.subject_public_key_info.to_der() {
        Ok(d) => d,
        Err(_) => return out,
    };
    let Ok(pubkey) = RsaPublicKey::from_public_key_der(&spki_der) else {
        return out;
    };
    let Ok(sig) = Signature::try_from(signer.signature.as_bytes()) else {
        return out;
    };
    let verifying = VerifyingKey::<Sha256>::new(pubkey);
    out.signature_ok = verifying.verify(&signed_bytes, &sig).is_ok();
    out
}

/// Parse a CMS `ContentInfo` and pull out its `SignedData`. A PDF `/Contents`
/// string is zero-padded to its reserved width, so trim to the actual DER
/// element length first (strict DER rejects trailing bytes).
fn parse_signed_data(cms_der: &[u8]) -> Option<SignedData> {
    let trimmed = der_element(cms_der).unwrap_or(cms_der);
    let ci = ContentInfo::from_der(trimmed).ok()?;
    ci.content.decode_as::<SignedData>().ok()
}

/// The exact byte span of the leading DER element (tag + length header +
/// content), ignoring any trailing padding. Supports short and long-form
/// definite lengths.
fn der_element(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 2 {
        return None;
    }
    let len_byte = bytes[1];
    let (header, content_len) = if len_byte < 0x80 {
        (2usize, len_byte as usize)
    } else {
        let n = (len_byte & 0x7f) as usize;
        if n == 0 || n > 4 || bytes.len() < 2 + n {
            return None;
        }
        let mut len = 0usize;
        for &b in &bytes[2..2 + n] {
            len = (len << 8) | b as usize;
        }
        (2 + n, len)
    };
    let total = header.checked_add(content_len)?;
    (total <= bytes.len()).then(|| &bytes[..total])
}

/// Choose the signer certificate: match the `SignerIdentifier`'s issuer+serial
/// when possible, else fall back to the first embedded certificate.
fn pick_signer_cert(certs: &[Certificate], sid: &SignerIdentifier) -> Option<Certificate> {
    if let SignerIdentifier::IssuerAndSerialNumber(ias) = sid {
        for c in certs {
            if c.tbs_certificate.serial_number == ias.serial_number
                && c.tbs_certificate.issuer == ias.issuer
            {
                return Some(c.clone());
            }
        }
    }
    certs.first().cloned()
}

/// Extract the certificate subject's Common Name (`CN`), if any.
fn common_name(cert: &Certificate) -> Option<String> {
    // OID 2.5.4.3 = id-at-commonName.
    let cn_oid = ObjectIdentifier::new_unwrap("2.5.4.3");
    for rdn in cert.tbs_certificate.subject.0.iter() {
        for atv in rdn.0.iter() {
            if atv.oid == cn_oid {
                if let Ok(s) = atv.value.decode_as::<der::asn1::Utf8StringRef>() {
                    return Some(s.as_str().to_string());
                }
                if let Ok(s) = atv.value.decode_as::<der::asn1::PrintableStringRef>() {
                    return Some(s.as_str().to_string());
                }
            }
        }
    }
    None
}
