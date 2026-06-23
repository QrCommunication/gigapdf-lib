//! Engine-managed and imported digital signatures — audited RustCrypto
//! (`x509-cert`, `cms`, `rsa`, `sha2`).
//!
//! Builds a self-signed X.509 certificate and a detached CMS/PKCS#7 `SignedData`
//! (the `adbe.pkcs7.detached` subfilter PDF signatures use). The RSA key is
//! generated in-engine from host randomness (an ephemeral "digital ID", like
//! Adobe's self-signed IDs) or imported from a PKCS#12 file ([`pkcs12`]). This
//! signs *content*; it does not assert a CA-backed identity.

pub mod der; // the definite-length DER reader the PKCS#12 importer uses
pub mod pades; // PAdES signing-certificate-v2 ESS attribute
pub mod pkcs12;
pub mod timestamp; // RFC 3161 TimeStampReq build / TimeStampResp parse

use crate::crypto::rsa::RsaPrivateKey;
use ::der::asn1::{Any, SetOfVec, UtcTime};
use ::der::{Decode, Encode};
use cms::builder::{SignedDataBuilder, SignerInfoBuilder};
use cms::cert::x509::attr::Attribute;
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

/// PDF signature `/SubFilter` profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubFilter {
    /// Legacy `adbe.pkcs7.detached` — the original engine profile.
    AdbePkcs7Detached,
    /// `ETSI.CAdES.detached` — PAdES baseline (B-B / B-T).
    EtsiCAdESDetached,
}

impl SubFilter {
    /// The `/SubFilter` name bytes written into the signature dictionary.
    pub fn name(self) -> &'static [u8] {
        match self {
            SubFilter::AdbePkcs7Detached => b"adbe.pkcs7.detached",
            SubFilter::EtsiCAdESDetached => b"ETSI.CAdES.detached",
        }
    }
}

/// `id-aa-timeStampToken` (1.2.840.113549.1.9.16.2.14) — the unsigned attribute
/// that carries an RFC 3161 timestamp token on a SignerInfo (PAdES-B-T).
fn oid_timestamp_token() -> const_oid::ObjectIdentifier {
    const_oid::ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.2.14")
}

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

    /// The signer's private key (for the two-phase PAdES-B-T flow, which signs in
    /// the core and later embeds the host-fetched timestamp token).
    pub fn key(&self) -> &RsaPrivateKey {
        &self.key
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

/// Configure a PAdES `SignerInfoBuilder` over `content`: a detached SignerInfo
/// whose signed attributes include the ESS `signing-certificate-v2` binding
/// (plus the `messageDigest`/`contentType` the CMS builder adds). `tst_token`,
/// when supplied, is embedded verbatim as the `id-aa-timeStampToken` **unsigned**
/// attribute — it is *not* covered by the signature, which is exactly why a
/// timestamp can be bolted on after the signature is computed without re-signing.
///
/// The RSA-PKCS#1 v1.5 signature this produces is **deterministic** in the
/// signed attributes, so building twice (once to learn the signature value to
/// timestamp, once with the resulting token attached) yields the same signature
/// both times — the foundation of the two-phase B-T flow. The builder borrows
/// `signing_key`, `econtent` and `digest`, so callers own those for its lifetime.
fn configure_pades_signer_info<'s>(
    signing_key: &'s SigningKey<Sha256>,
    cert: &Certificate,
    econtent: &'s EncapsulatedContentInfo,
    digest: &'s [u8],
    tst_token: Option<&[u8]>,
) -> Option<SignerInfoBuilder<'s, SigningKey<Sha256>>> {
    let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
        issuer: cert.tbs_certificate.issuer.clone(),
        serial_number: cert.tbs_certificate.serial_number.clone(),
    });

    let mut builder =
        SignerInfoBuilder::new(signing_key, sid, sha256_alg(), econtent, Some(digest)).ok()?;

    // PAdES-B mandatory signed attribute.
    let cert_der = cert.to_der().ok()?;
    let signing_cert = pades::signing_certificate_v2_attribute(&cert_der)?;
    builder.add_signed_attribute(signing_cert).ok()?;

    // RFC 3161 timestamp token as an unsigned attribute (PAdES-B-T).
    if let Some(token_der) = tst_token {
        builder
            .add_unsigned_attribute(timestamp_token_attribute(token_der)?)
            .ok()?;
    }

    Some(builder)
}

/// Wrap a raw RFC 3161 `TimeStampToken` (`ContentInfo`) as the
/// `id-aa-timeStampToken` unsigned attribute.
fn timestamp_token_attribute(token_der: &[u8]) -> Option<Attribute> {
    let token_any = Any::from_der(token_der).ok()?;
    let values = SetOfVec::from_iter([token_any]).ok()?;
    Some(Attribute {
        oid: oid_timestamp_token(),
        values,
    })
}

/// Build a complete detached PAdES `SignedData` (DER) over `content`. With
/// `tst_token = None` this is a PAdES-B-B blob (`signing-certificate-v2` + the
/// standard CMS attrs); with a token it is PAdES-B-T. `None` if the certificate
/// can't be parsed or the CMS can't be built.
pub fn build_pades_cms(
    key: &RsaPrivateKey,
    cert_der: &[u8],
    content: &[u8],
    tst_token: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let cert = Certificate::from_der(cert_der).ok()?;
    let signing_key = SigningKey::<Sha256>::new(key.inner().clone());
    let econtent = EncapsulatedContentInfo {
        econtent_type: id_data(),
        econtent: None,
    };
    let digest = Sha256::digest(content);
    let signer_info =
        configure_pades_signer_info(&signing_key, &cert, &econtent, &digest, tst_token)?;

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

/// The SignerInfo **signature value** (the RSA signature OCTET STRING bytes) that
/// a PAdES signature over `content` would carry — i.e. the bytes whose SHA-256
/// becomes the RFC 3161 `MessageImprint` for a B-T timestamp. Deterministic, so
/// the value returned here equals the signature embedded by a later
/// [`build_pades_cms`] over the same `content`/key/cert.
pub fn pades_signature_value(
    key: &RsaPrivateKey,
    cert_der: &[u8],
    content: &[u8],
) -> Option<Vec<u8>> {
    let cert = Certificate::from_der(cert_der).ok()?;
    let signing_key = SigningKey::<Sha256>::new(key.inner().clone());
    let econtent = EncapsulatedContentInfo {
        econtent_type: id_data(),
        econtent: None,
    };
    let digest = Sha256::digest(content);
    let builder = configure_pades_signer_info(&signing_key, &cert, &econtent, &digest, None)?;
    let signer_info = builder.build::<rsa::pkcs1v15::Signature>().ok()?;
    Some(signer_info.signature.as_bytes().to_vec())
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
