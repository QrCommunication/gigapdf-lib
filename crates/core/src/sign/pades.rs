//! PAdES (ETSI EN 319 142-1) baseline pieces layered on the existing CMS path.
//!
//! Two things separate a PAdES-B signature from the legacy `adbe.pkcs7.detached`
//! profile, both small:
//!  - the PDF `/SubFilter` becomes `ETSI.CAdES.detached` (handled in
//!    `document.rs`), and
//!  - the SignerInfo carries a mandatory **`signing-certificate-v2`** ESS signed
//!    attribute binding the signature to a specific certificate (its SHA-256
//!    hash). This module builds that attribute.
//!
//! The `SigningCertificateV2` value is hand-rolled with the in-tree DER codec
//! ([`super::der`]) and wrapped as a `cms` [`Attribute`] so it can be fed to
//! `SignerInfoBuilder::add_signed_attribute` before the signature is computed.

use super::der;
use cms::cert::x509::attr::Attribute;
use sha2::{Digest, Sha256};

/// `id-aa-signingCertificateV2` (1.2.840.113549.1.9.16.2.47).
const OID_SIGNING_CERTIFICATE_V2: &[u64] = &[1, 2, 840, 113549, 1, 9, 16, 2, 47];

/// Build the ESS `signing-certificate-v2` signed attribute for `cert_der`.
///
/// ```text
/// SigningCertificateV2 ::= SEQUENCE { certs SEQUENCE OF ESSCertIDv2 }
/// ESSCertIDv2 ::= SEQUENCE {
///   hashAlgorithm AlgorithmIdentifier DEFAULT {id-sha256},  -- omitted (default)
///   certHash      OCTET STRING,                              -- SHA-256(cert)
///   issuerSerial  IssuerSerial OPTIONAL }                    -- omitted
/// ```
///
/// With SHA-256 (the default `hashAlgorithm`) the algorithm identifier is left
/// out, so each `ESSCertIDv2` is just the `certHash` OCTET STRING. The attribute
/// value is `SigningCertificateV2` wrapped in the SET that every CMS attribute
/// value lives in. Returns `None` only if the resulting DER fails to decode as a
/// `cms` attribute (which well-formed input never triggers).
pub fn signing_certificate_v2_attribute(cert_der: &[u8]) -> Option<Attribute> {
    let cert_hash = Sha256::digest(cert_der);
    // ESSCertIDv2 with the default SHA-256 hashAlgorithm omitted.
    let ess_cert_id = der::sequence(&[der::octet_string(&cert_hash)]);
    // SigningCertificateV2 ::= SEQUENCE { certs SEQUENCE OF ESSCertIDv2 }.
    let signing_certificate = der::sequence(&[der::sequence(&[ess_cert_id])]);

    // Attribute ::= SEQUENCE { type OID, values SET OF AttributeValue }.
    let attribute_der = der::sequence(&[
        der::oid(OID_SIGNING_CERTIFICATE_V2),
        der::set(&[signing_certificate]),
    ]);

    use ::der::Decode;
    Attribute::from_der(&attribute_der).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::der::Encode;

    #[test]
    fn attribute_carries_the_cert_hash_under_the_ess_oid() {
        let cert = b"a stand-in certificate body";
        let attr = signing_certificate_v2_attribute(cert).expect("attribute");

        // The OID is id-aa-signingCertificateV2.
        let oid = ::der::asn1::ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.2.47");
        assert_eq!(attr.oid, oid);

        // Its DER embeds the SHA-256 of the certificate.
        let der = attr.to_der().expect("der");
        let hash = Sha256::digest(cert);
        assert!(
            der.windows(32).any(|w| w == hash.as_slice()),
            "certHash present in the attribute"
        );
    }

    #[test]
    fn distinct_certs_yield_distinct_attributes() {
        let a = signing_certificate_v2_attribute(b"cert-a").expect("a");
        let b = signing_certificate_v2_attribute(b"cert-b").expect("b");
        assert_ne!(a.to_der().unwrap(), b.to_der().unwrap());
    }
}
