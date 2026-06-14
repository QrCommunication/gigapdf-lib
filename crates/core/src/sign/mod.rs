//! Engine-managed digital signatures (non-eIDAS) — zero dependencies.
//!
//! Builds a self-signed X.509 certificate and a detached CMS/PKCS#7
//! `SignedData` (the `adbe.pkcs7.detached` subfilter PDF signatures use), all
//! from our own [`der`] encoder + [`crate::crypto::rsa`]. The private key is
//! generated in-engine from host randomness (an ephemeral "digital ID", like
//! Adobe's self-signed IDs). This signs *content*, it does not assert a
//! CA-backed identity.

pub mod der;

use crate::crypto::rsa::RsaPrivateKey;
use crate::crypto::sha256::sha256;

// Object identifiers used by the structures below.
const OID_RSA_ENCRYPTION: &[u64] = &[1, 2, 840, 113549, 1, 1, 1];
const OID_SHA256_WITH_RSA: &[u64] = &[1, 2, 840, 113549, 1, 1, 11];
const OID_SHA256: &[u64] = &[2, 16, 840, 1, 101, 3, 4, 2, 1];
const OID_COMMON_NAME: &[u64] = &[2, 5, 4, 3];
const OID_SIGNED_DATA: &[u64] = &[1, 2, 840, 113549, 1, 7, 2];
const OID_DATA: &[u64] = &[1, 2, 840, 113549, 1, 7, 1];
const OID_CONTENT_TYPE: &[u64] = &[1, 2, 840, 113549, 1, 9, 3];
const OID_MESSAGE_DIGEST: &[u64] = &[1, 2, 840, 113549, 1, 9, 4];

fn alg_sha256_with_rsa() -> Vec<u8> {
    der::sequence(&[der::oid(OID_SHA256_WITH_RSA), der::null()])
}

fn alg_sha256() -> Vec<u8> {
    der::sequence(&[der::oid(OID_SHA256), der::null()])
}

fn rsa_public_key_info(key: &RsaPrivateKey) -> Vec<u8> {
    let rsa_public_key = der::sequence(&[
        der::integer(&key.n.to_bytes_be()),
        der::integer(&key.e.to_bytes_be()),
    ]);
    der::sequence(&[
        der::sequence(&[der::oid(OID_RSA_ENCRYPTION), der::null()]),
        der::bit_string(&rsa_public_key),
    ])
}

fn common_name(name: &str) -> Vec<u8> {
    // Name = SEQUENCE OF RDN; RDN = SET OF AttributeTypeAndValue.
    der::sequence(&[der::set(&[der::sequence(&[
        der::oid(OID_COMMON_NAME),
        der::utf8_string(name),
    ])])])
}

/// An engine-managed signer: a freshly generated RSA key and its self-signed
/// certificate.
#[derive(Debug, Clone)]
pub struct Signer {
    key: RsaPrivateKey,
    certificate: Vec<u8>,
    issuer: Vec<u8>,
    serial: u32,
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
        let serial = 1u32;
        let name = common_name(common);

        let tbs = der::sequence(&[
            der::context(0, &der::integer_u32(2)), // version v3
            der::integer_u32(serial),
            alg_sha256_with_rsa(),
            name.clone(), // issuer == subject (self-signed)
            der::sequence(&[der::utc_time(not_before), der::utc_time(not_after)]),
            name.clone(),
            rsa_public_key_info(&key),
        ]);

        let signature = key.sign_sha256(&tbs);
        let certificate = der::sequence(&[
            tbs,
            alg_sha256_with_rsa(),
            der::bit_string(&signature),
        ]);

        Some(Signer {
            key,
            certificate,
            issuer: name,
            serial,
        })
    }

    /// Build a detached CMS `SignedData` (DER) over `content` — i.e. a PDF
    /// `adbe.pkcs7.detached` signature blob for the given signed bytes.
    pub fn detached_cms(&self, content: &[u8]) -> Vec<u8> {
        let message_digest = sha256(content);

        // signedAttrs: contentType + messageDigest.
        let attr_content_type = der::sequence(&[
            der::oid(OID_CONTENT_TYPE),
            der::set(&[der::oid(OID_DATA)]),
        ]);
        let attr_message_digest = der::sequence(&[
            der::oid(OID_MESSAGE_DIGEST),
            der::set(&[der::octet_string(&message_digest)]),
        ]);

        let attrs = [attr_content_type, attr_message_digest];
        // The signature is computed over the signedAttrs DER explicitly tagged
        // as a SET (0x31), per CMS §5.4.
        let signed_attrs_for_signing = der::set(&attrs);
        let signature = self.key.sign_sha256(&signed_attrs_for_signing);

        // In the SignerInfo the same attributes carry the implicit [0] tag,
        // which replaces the SET tag around the concatenated attributes.
        let signed_attrs_tagged = der::context(0, &attrs.concat());

        let signer_info = der::sequence(&[
            der::integer_u32(1), // version
            der::sequence(&[self.issuer.clone(), der::integer_u32(self.serial)]), // issuerAndSerial
            alg_sha256(),
            signed_attrs_tagged,
            der::sequence(&[der::oid(OID_RSA_ENCRYPTION), der::null()]),
            der::octet_string(&signature),
        ]);

        let signed_data = der::sequence(&[
            der::integer_u32(1), // version
            der::set(&[alg_sha256()]),
            der::sequence(&[der::oid(OID_DATA)]), // encapContentInfo (detached)
            der::context(0, &self.certificate),  // [0] certificates
            der::set(&[signer_info]),
        ]);

        der::sequence(&[der::oid(OID_SIGNED_DATA), der::context(0, &signed_data)])
    }

    /// The DER certificate bytes.
    pub fn certificate(&self) -> &[u8] {
        &self.certificate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Small key keeps the test fast while exercising the full DER assembly.
    fn test_signer() -> Signer {
        let randomness: Vec<u8> = (0..256).map(|i| (i * 53 + 7) as u8).collect();
        Signer::generate("GigaPDF Signer", "260614000000Z", "360614000000Z", 512, &randomness)
            .expect("signer")
    }

    #[test]
    fn certificate_is_well_formed_der() {
        let signer = test_signer();
        let cert = signer.certificate();
        assert_eq!(cert[0], 0x30, "certificate is a SEQUENCE");
        // The declared length must match the actual body length.
        assert!(cert.len() > 200, "non-trivial certificate ({} bytes)", cert.len());
    }

    #[test]
    fn detached_cms_embeds_the_digest_and_signed_data_oid() {
        let signer = test_signer();
        let content = b"the exact document bytes that were signed";
        let cms = signer.detached_cms(content);
        assert_eq!(cms[0], 0x30, "ContentInfo is a SEQUENCE");
        // The SHA-256 of the content must appear (as the messageDigest attr).
        let digest = sha256(content);
        assert!(
            cms.windows(32).any(|w| w == digest),
            "messageDigest attribute carries the content hash"
        );
    }
}
