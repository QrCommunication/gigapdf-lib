//! Engine-managed and imported digital signatures — audited RustCrypto
//! (`x509-cert`, `cms`, `rsa`, `sha2`).
//!
//! Builds a self-signed X.509 certificate and a detached CMS/PKCS#7 `SignedData`
//! (the `adbe.pkcs7.detached` subfilter PDF signatures use). The RSA key is
//! generated in-engine from host randomness (an ephemeral "digital ID", like
//! Adobe's self-signed IDs) or imported from a PKCS#12 file ([`pkcs12`]). This
//! signs *content*; it does not assert a CA-backed identity.

pub mod der; // the definite-length DER reader the PKCS#12 importer uses
pub mod pkcs12;

use crate::crypto::rsa::RsaPrivateKey;
use ::der::asn1::UtcTime;
use ::der::{Decode, Encode};
use cms::builder::{SignedDataBuilder, SignerInfoBuilder};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::signed_data::{EncapsulatedContentInfo, SignerIdentifier};
use rsa::pkcs1v15::SigningKey;
use rsa::RsaPublicKey;
use sha2::{Digest, Sha256};
use spki::{AlgorithmIdentifierOwned, SubjectPublicKeyInfoOwned};
use std::str::FromStr;
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::time::{Time, Validity};
use x509_cert::Certificate;

/// `id-sha256` (2.16.840.1.101.3.4.2.1).
fn sha256_alg() -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid: const_oid::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1"),
        parameters: None,
    }
}

/// `id-data` (1.2.840.113549.1.7.1) — the detached CMS encapsulated content type.
fn id_data() -> const_oid::ObjectIdentifier {
    const_oid::ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1")
}

/// Parse a `YYMMDDHHMMSSZ` UTCTime string into an X.509 [`Time`].
fn parse_utc_time(s: &str) -> Option<Time> {
    let b = s.as_bytes();
    if b.len() != 13 || b[12] != b'Z' {
        return None;
    }
    let two = |i: usize| -> Option<u16> {
        let d0 = (b[i] as char).to_digit(10)?;
        let d1 = (b[i + 1] as char).to_digit(10)?;
        Some((d0 * 10 + d1) as u16)
    };
    let yy = two(0)?;
    let year = if yy >= 50 { 1900 + yy } else { 2000 + yy };
    let dt = ::der::DateTime::new(
        year,
        two(2)? as u8,
        two(4)? as u8,
        two(6)? as u8,
        two(8)? as u8,
        two(10)? as u8,
    )
    .ok()?;
    Some(Time::UtcTime(UtcTime::from_date_time(dt).ok()?))
}

/// An engine-managed signer: a freshly generated RSA key and its self-signed
/// certificate (DER).
#[derive(Debug, Clone)]
pub struct Signer {
    key: RsaPrivateKey,
    certificate: Vec<u8>,
}

impl Signer {
    /// Generate a `bits`-bit signer named `common`, valid `[not_before,
    /// not_after]` (UTCTime `YYMMDDHHMMSSZ`), from host `randomness`.
    pub fn generate(
        common: &str,
        not_before: &str,
        not_after: &str,
        bits: usize,
        randomness: &[u8],
    ) -> Option<Signer> {
        let key = RsaPrivateKey::generate(bits, randomness)?;
        let subject = Name::from_str(&format!("CN={common}")).ok()?;
        let serial = SerialNumber::from(1u32);
        let validity = Validity {
            not_before: parse_utc_time(not_before)?,
            not_after: parse_utc_time(not_after)?,
        };
        let pub_key = RsaPublicKey::from(key.inner());
        let spki = SubjectPublicKeyInfoOwned::from_key(pub_key).ok()?;
        let signing_key = SigningKey::<Sha256>::new(key.inner().clone());

        let builder =
            CertificateBuilder::new(Profile::Root, serial, validity, subject, spki, &signing_key)
                .ok()?;
        let cert: Certificate = builder.build().ok()?;
        let certificate = cert.to_der().ok()?;

        Some(Signer { key, certificate })
    }

    /// Build a detached CMS `SignedData` (DER) over `content` — a PDF
    /// `adbe.pkcs7.detached` signature blob for the given signed bytes.
    pub fn detached_cms(&self, content: &[u8]) -> Vec<u8> {
        let cert = match Certificate::from_der(&self.certificate) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        build_detached_cms(&self.key, cert, content).unwrap_or_default()
    }

    /// The DER certificate bytes.
    pub fn certificate(&self) -> &[u8] {
        &self.certificate
    }
}

/// Build a detached CMS `SignedData` (DER `ContentInfo`) over `content`, signed
/// by `key` and embedding `cert`. The `SignerInfo` references the signer through
/// its issuer + serial number.
fn build_detached_cms(key: &RsaPrivateKey, cert: Certificate, content: &[u8]) -> Option<Vec<u8>> {
    let digest = Sha256::digest(content);
    let signing_key = SigningKey::<Sha256>::new(key.inner().clone());

    let econtent = EncapsulatedContentInfo {
        econtent_type: id_data(),
        econtent: None, // detached
    };
    let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
        issuer: cert.tbs_certificate.issuer.clone(),
        serial_number: cert.tbs_certificate.serial_number.clone(),
    });

    let signer_info =
        SignerInfoBuilder::new(&signing_key, sid, sha256_alg(), &econtent, Some(&digest)).ok()?;

    let content_info = SignedDataBuilder::new(&econtent)
        .add_digest_algorithm(sha256_alg())
        .ok()?
        .add_certificate(CertificateChoices::Certificate(cert))
        .ok()?
        .add_signer_info(signer_info)
        .ok()?
        .build()
        .ok()?;

    content_info.to_der().ok()
}

/// Detached CMS over `content` for an externally supplied identity — e.g. a key
/// and certificate imported from a PKCS#12 file ([`pkcs12::parse`]). `None` if
/// the certificate can't be parsed.
pub fn detached_cms_external(
    key: &RsaPrivateKey,
    cert_der: &[u8],
    content: &[u8],
) -> Option<Vec<u8>> {
    let cert = Certificate::from_der(cert_der).ok()?;
    build_detached_cms(key, cert, content)
}

/// Extract the DER `issuer` Name and `serialNumber` INTEGER from an X.509
/// certificate, each as its verbatim TLV bytes (a quick validity probe + for any
/// caller that needs the raw `issuerAndSerial` parts).
pub fn issuer_and_serial(cert_der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let cert = Certificate::from_der(cert_der).ok()?;
    let issuer = cert.tbs_certificate.issuer.to_der().ok()?;
    let serial = cert.tbs_certificate.serial_number.to_der().ok()?;
    Some((issuer, serial))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cms::content_info::ContentInfo;

    fn test_signer() -> Signer {
        let randomness: Vec<u8> = (0..256).map(|i| (i * 53 + 7) as u8).collect();
        Signer::generate(
            "GigaPDF Signer",
            "260614000000Z",
            "360614000000Z",
            1024,
            &randomness,
        )
        .expect("signer")
    }

    #[test]
    fn certificate_is_well_formed_der() {
        let signer = test_signer();
        let cert = signer.certificate();
        assert_eq!(cert[0], 0x30, "certificate is a SEQUENCE");
        assert!(
            cert.len() > 200,
            "non-trivial certificate ({} bytes)",
            cert.len()
        );
        // It round-trips through the X.509 parser.
        assert!(Certificate::from_der(cert).is_ok());
    }

    #[test]
    fn detached_cms_embeds_the_digest_and_parses() {
        let signer = test_signer();
        let content = b"the exact document bytes that were signed";
        let cms = signer.detached_cms(content);
        assert_eq!(cms[0], 0x30, "ContentInfo is a SEQUENCE");
        let digest = Sha256::digest(content);
        assert!(
            cms.windows(32).any(|w| w == digest.as_slice()),
            "messageDigest attribute carries the content hash"
        );
        // It parses back as a CMS ContentInfo.
        assert!(ContentInfo::from_der(&cms).is_ok());
    }
}
