//! PAdES-LTV (long-term validation) building blocks — the validation material a
//! `/DSS` carries and the host round trips that fetch it.
//!
//! Like the RFC 3161 timestamp (`super::timestamp`), the core never performs the
//! network I/O: it computes *what to fetch* (each certificate's OCSP responder
//! URL from its AIA extension, and/or its CRL distribution point), the host
//! fetches, and the core consumes the bytes. The OCSP `OCSPRequest` is built and
//! the `OCSPResponse` / `CertificateList` (CRL) are inspected with the in-tree
//! DER codec ([`super::der`]) — no `x509-ocsp`/`tsp` crate, keeping the
//! "two narrow crypto exceptions" dependency posture (`cms` + `x509-cert`).
//!
//! What B-LT embeds in the `/DSS`:
//!  - `/Certs`  — the certificate chain (DER),
//!  - `/OCSPs`  — OCSP responses (DER), and/or
//!  - `/CRLs`   — CRLs (DER),
//!  - `/VRI`    — per-signature Validation Related Info, keyed by the **upper-hex
//!    SHA-1 of the signature's `/Contents`** (kept for older validators; modern
//!    ETSI advises material is referenceable from `/DSS` directly).

use super::der;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use x509_cert::der::{Decode, Encode};
use x509_cert::Certificate;

/// `id-ad-ocsp` (1.3.6.1.5.5.7.48.1) — the AIA access method whose location is an
/// OCSP responder URL.
const OID_AD_OCSP: &[u64] = &[1, 3, 6, 1, 5, 5, 7, 48, 1];
/// `id-pe-authorityInfoAccess` (1.3.6.1.5.5.7.1.1).
const OID_AIA: &[u64] = &[1, 3, 6, 1, 5, 5, 7, 1, 1];
/// `id-ce-cRLDistributionPoints` (2.5.29.31).
const OID_CRL_DP: &[u64] = &[2, 5, 29, 31];
/// `id-sha1` (1.3.14.3.2.26) — the CertID hash algorithm OCSP responders expect.
const OID_SHA1: &[u64] = &[1, 3, 14, 3, 2, 26];
/// `id-pkix-ocsp-nonce` (1.3.6.1.5.5.7.48.1.2).
const OID_OCSP_NONCE: &[u64] = &[1, 3, 6, 1, 5, 5, 7, 48, 1, 2];

/// The kind of revocation material a certificate can be checked against, with the
/// URL the host must fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevocationSource {
    /// An OCSP responder; fetch by POSTing the DER `request`
    /// (`Content-Type: application/ocsp-request`) and embedding the DER reply.
    Ocsp { url: String, request: Vec<u8> },
    /// A CRL distribution point; fetch the DER `CertificateList` by GET and embed
    /// it verbatim.
    Crl { url: String },
}

/// The fetch plan for one certificate in the chain: its DER bytes (embedded in
/// `/DSS/Certs`) and the revocation source(s) the host should retrieve. OCSP is
/// preferred (smaller, fresher); a CRL DP is offered as a fallback when present.
#[derive(Debug, Clone)]
pub struct CertFetchPlan {
    /// The certificate's DER (subject of this plan, embedded in `/Certs`).
    pub cert_der: Vec<u8>,
    /// Revocation sources discovered from this certificate's extensions, OCSP
    /// first. Empty when the cert advertises neither (e.g. a self-signed root).
    pub sources: Vec<RevocationSource>,
}

/// Extract every X.509v3 extension `(extn_id arcs, extn_value inner DER)` from a
/// DER certificate, with the in-tree reader. `extn_value` is unwrapped from its
/// `OCTET STRING` envelope so callers parse the extension's own structure
/// directly. Returns an empty list if the cert has no extensions / is malformed.
fn certificate_extensions(cert_der: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    // Certificate ::= SEQUENCE { tbsCertificate SEQUENCE { ... extensions [3] }, ... }
    let mut reader = der::Reader::new(cert_der);
    let Some(mut cert) = reader.descend(der::TAG_SEQUENCE) else {
        return Vec::new();
    };
    let Some(tbs) = cert.next_tag(der::TAG_SEQUENCE) else {
        return Vec::new();
    };
    // Walk the TBSCertificate looking for the [3] EXPLICIT extensions wrapper.
    let mut tbs_reader = tbs.reader();
    let mut extensions_seq = None;
    while let Some(field) = tbs_reader.read() {
        // [3] constructed = 0xA3.
        if field.tag == 0xA3 {
            extensions_seq = field.reader().descend(der::TAG_SEQUENCE);
            break;
        }
    }
    let Some(mut extensions) = extensions_seq else {
        return Vec::new();
    };

    let mut out = Vec::new();
    while let Some(ext) = extensions.next_tag(der::TAG_SEQUENCE) {
        // Extension ::= SEQUENCE { extnID OID, critical BOOLEAN DEFAULT FALSE,
        //                          extnValue OCTET STRING }
        let mut fields = ext.reader();
        let Some(oid) = fields.next_tag(der::TAG_OID) else {
            continue;
        };
        let mut value = match fields.read() {
            Some(v) => v,
            None => continue,
        };
        if value.tag == 0x01 {
            // critical BOOLEAN present; the real value is the next field.
            value = match fields.read() {
                Some(v) => v,
                None => continue,
            };
        }
        if value.tag != der::TAG_OCTET_STRING {
            continue;
        }
        out.push((oid.content.to_vec(), value.content.to_vec()));
    }
    out
}

/// The OCSP responder URL from a certificate's Authority Information Access
/// extension (the `accessLocation` of the `id-ad-ocsp` access method, a
/// `[6] IA5String` URI), if any.
fn ocsp_url(cert_der: &[u8]) -> Option<String> {
    let target = der::oid_arcs(OID_AD_OCSP);
    let aia_oid = der::oid_arcs(OID_AIA);
    for (oid, value) in certificate_extensions(cert_der) {
        if oid != aia_oid {
            continue;
        }
        // AuthorityInfoAccessSyntax ::= SEQUENCE OF AccessDescription
        // AccessDescription ::= SEQUENCE { accessMethod OID, accessLocation GeneralName }
        let mut reader = der::Reader::new(&value);
        let Some(mut descriptions) = reader.descend(der::TAG_SEQUENCE) else {
            continue;
        };
        while let Some(desc) = descriptions.next_tag(der::TAG_SEQUENCE) {
            let mut fields = desc.reader();
            let Some(method) = fields.next_tag(der::TAG_OID) else {
                continue;
            };
            if method.content != target.as_slice() {
                continue;
            }
            // accessLocation: GeneralName uniformResourceIdentifier = [6] IA5String.
            if let Some(location) = fields.read() {
                if location.tag == 0x86 {
                    if let Ok(url) = std::str::from_utf8(location.content) {
                        return Some(url.to_string());
                    }
                }
            }
        }
    }
    None
}

/// The first HTTP(S) CRL distribution-point URL from a certificate's
/// `cRLDistributionPoints` extension, if any.
fn crl_url(cert_der: &[u8]) -> Option<String> {
    let dp_oid = der::oid_arcs(OID_CRL_DP);
    for (oid, value) in certificate_extensions(cert_der) {
        if oid != dp_oid {
            continue;
        }
        // CRLDistributionPoints ::= SEQUENCE OF DistributionPoint
        // DistributionPoint ::= SEQUENCE { distributionPoint [0] DistributionPointName OPTIONAL, ... }
        // DistributionPointName ::= [0] fullName GeneralNames -> [6] IA5String URI
        if let Some(url) = find_uri_in(&value) {
            if url.starts_with("http://") || url.starts_with("https://") {
                return Some(url);
            }
        }
    }
    None
}

/// Recursively scan a DER blob for the first `[6] IA5String` GeneralName URI
/// (`tag 0x86`). Used to pull the URI out of the nested CRL-DP `[0] [0] [6]`
/// context wrappers without spelling each layer out.
fn find_uri_in(der_bytes: &[u8]) -> Option<String> {
    let mut reader = der::Reader::new(der_bytes);
    while let Some(tlv) = reader.read() {
        // [6] IA5String GeneralName.
        if tlv.tag == 0x86 {
            if let Ok(url) = std::str::from_utf8(tlv.content) {
                return Some(url.to_string());
            }
        }
        // Descend into any constructed value (high bit of the tag's form: 0x20).
        if tlv.tag & 0x20 != 0 {
            if let Some(found) = find_uri_in(tlv.content) {
                return Some(found);
            }
        }
    }
    None
}

/// Build an RFC 6960 `OCSPRequest` (DER) asking the responder about the
/// certificate `cert_der`, issued by `issuer_der`. The `CertID` uses SHA-1 over
/// the issuer's name and public key (as responders require), plus the subject's
/// serial number. `nonce` (optional host entropy) is added as the
/// `id-pkix-ocsp-nonce` request extension. Returns `None` if either certificate
/// can't be parsed.
///
/// ```text
/// OCSPRequest ::= SEQUENCE { tbsRequest TBSRequest }
/// TBSRequest  ::= SEQUENCE { requestList SEQUENCE OF Request,
///                            requestExtensions [2] EXPLICIT Extensions OPTIONAL }
/// Request     ::= SEQUENCE { reqCert CertID }
/// CertID      ::= SEQUENCE { hashAlgorithm AlgorithmIdentifier,  -- id-sha1
///                            issuerNameHash OCTET STRING,        -- SHA1(issuer DN)
///                            issuerKeyHash  OCTET STRING,        -- SHA1(issuer SPKI key)
///                            serialNumber   INTEGER }            -- subject serial
/// ```
pub fn build_ocsp_request(
    cert_der: &[u8],
    issuer_der: &[u8],
    nonce: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let cert = Certificate::from_der(cert_der).ok()?;
    let issuer = Certificate::from_der(issuer_der).ok()?;

    let issuer_name_der = issuer.tbs_certificate.subject.to_der().ok()?;
    let issuer_name_hash = Sha1::digest(&issuer_name_der);

    // issuerKeyHash = SHA-1 of the issuer public key BIT STRING value (the raw
    // key bytes, excluding tag/length and the unused-bits octet).
    let key_bits = issuer
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .raw_bytes();
    let issuer_key_hash = Sha1::digest(key_bits);

    let serial_der = cert.tbs_certificate.serial_number.to_der().ok()?;

    let hash_algorithm = der::sequence(&[der::oid(OID_SHA1), der::null()]);
    let cert_id = der::sequence(&[
        hash_algorithm,
        der::octet_string(&issuer_name_hash),
        der::octet_string(&issuer_key_hash),
        serial_der, // already a full INTEGER TLV
    ]);
    let request = der::sequence(&[cert_id]);
    let request_list = der::sequence(&[request]);

    let mut tbs_members = vec![request_list];
    if let Some(nonce) = nonce {
        // Extensions ::= SEQUENCE OF Extension; nonce extnValue is OCTET STRING(nonce).
        let nonce_ext = der::sequence(&[
            der::oid(OID_OCSP_NONCE),
            der::octet_string(&der::octet_string(nonce)),
        ]);
        let extensions = der::sequence(&[nonce_ext]);
        // requestExtensions [2] EXPLICIT.
        tbs_members.push(der::context(2, &extensions));
    }
    let tbs_request = der::sequence(&tbs_members);
    Some(der::sequence(&[tbs_request]))
}

/// The decoded outcome of an OCSP responder reply: the verbatim DER to embed in
/// `/DSS/OCSPs`, plus the responder `OCSPResponseStatus` for diagnostics.
#[derive(Debug, Clone)]
pub struct OcspResponse {
    /// The full `OCSPResponse` DER, embedded verbatim in `/DSS/OCSPs`.
    pub response_der: Vec<u8>,
    /// `OCSPResponseStatus` (0 = successful).
    pub status: u32,
}

/// Validate an RFC 6960 `OCSPResponse` shape and keep it for embedding. We do not
/// re-encode it — the full response (the signed `BasicOCSPResponse`) is what a
/// validator needs, so it is stored verbatim. Returns `None` if the
/// `responseStatus` is not `successful (0)` or the DER is malformed.
///
/// ```text
/// OCSPResponse ::= SEQUENCE {
///   responseStatus OCSPResponseStatus,           -- ENUMERATED, 0 = successful
///   responseBytes  [0] EXPLICIT ResponseBytes OPTIONAL }
/// ```
pub fn parse_ocsp_response(response: &[u8]) -> Option<OcspResponse> {
    let mut outer = der::Reader::new(response);
    let mut body = outer.descend(der::TAG_SEQUENCE)?;
    // responseStatus ENUMERATED (tag 0x0A).
    let status_tlv = body.read()?;
    if status_tlv.tag != 0x0A {
        return None;
    }
    let status = be_u32(status_tlv.content)?;
    if status != 0 {
        return None;
    }
    // responseBytes [0] must be present for a successful response.
    let response_bytes = body.read()?;
    if response_bytes.tag != der::TAG_CONTEXT_0 {
        return None;
    }
    Some(OcspResponse {
        response_der: response.to_vec(),
        status,
    })
}

/// Validate a DER `CertificateList` (CRL) shape and keep it verbatim for
/// `/DSS/CRLs`. Confirms the top-level SEQUENCE holds a `TBSCertList` SEQUENCE
/// (so a stray OCSP/HTML body isn't embedded as a CRL). Returns the verbatim DER,
/// or `None` if it doesn't parse as a CRL.
pub fn parse_crl(crl: &[u8]) -> Option<Vec<u8>> {
    // CertificateList ::= SEQUENCE { tbsCertList SEQUENCE, signatureAlgorithm,
    //                                signatureValue BIT STRING }
    let mut outer = der::Reader::new(crl);
    let mut body = outer.descend(der::TAG_SEQUENCE)?;
    let tbs = body.read()?;
    if tbs.tag != der::TAG_SEQUENCE {
        return None;
    }
    // A signatureValue BIT STRING must follow (after the signatureAlgorithm).
    let _sig_alg = body.next_tag(der::TAG_SEQUENCE)?;
    let sig = body.read()?;
    if sig.tag != der::TAG_BIT_STRING {
        return None;
    }
    Some(crl.to_vec())
}

/// Build the per-certificate LTV fetch plan for a chain: for each cert, its DER
/// (for `/DSS/Certs`) and the revocation sources discovered from its extensions.
/// OCSP is preferred; a CRL DP is added as a fallback when present. The `nonce`
/// (optional) is threaded into each OCSP request.
///
/// The chain is `[leaf, issuer, …, root]` (as recovered from a signed PDF's CMS).
/// A certificate's revocation status is checked against **its issuer** — i.e.
/// cert *i* against cert *i+1* — so the last (root / self-issued) cert gets no
/// source. Certs lacking an issuer in the slice yield no sources.
pub fn plan_chain(chain: &[Vec<u8>], nonce: Option<&[u8]>) -> Vec<CertFetchPlan> {
    let mut plans = Vec::with_capacity(chain.len());
    for (i, cert_der) in chain.iter().enumerate() {
        let mut sources = Vec::new();
        if let Some(issuer_der) = chain.get(i + 1) {
            if let Some(url) = ocsp_url(cert_der) {
                if let Some(request) = build_ocsp_request(cert_der, issuer_der, nonce) {
                    sources.push(RevocationSource::Ocsp { url, request });
                }
            }
            if let Some(url) = crl_url(cert_der) {
                sources.push(RevocationSource::Crl { url });
            }
        }
        plans.push(CertFetchPlan {
            cert_der: cert_der.clone(),
            sources,
        });
    }
    plans
}

/// Extract the certificate chain (each cert DER) embedded in a CMS `SignedData`
/// `ContentInfo` — i.e. the certs a PDF signature's `/Contents` blob carries. The
/// order follows the `certificates [0]` SET as encoded. Returns an empty vector
/// if the structure can't be walked.
///
/// Walks: `ContentInfo { contentType, content [0] { SignedData { …,
/// certificates [0] IMPLICIT CertificateSet, … } } }`.
pub fn certificates_from_cms(cms_der: &[u8]) -> Vec<Vec<u8>> {
    let mut reader = der::Reader::new(cms_der);
    let Some(mut content_info) = reader.descend(der::TAG_SEQUENCE) else {
        return Vec::new();
    };
    // contentType OID, then content [0] EXPLICIT.
    let Some(_oid) = content_info.next_tag(der::TAG_OID) else {
        return Vec::new();
    };
    let Some(mut content) = content_info.descend(der::TAG_CONTEXT_0) else {
        return Vec::new();
    };
    let Some(mut signed_data) = content.descend(der::TAG_SEQUENCE) else {
        return Vec::new();
    };
    // SignedData: version, digestAlgorithms SET, encapContentInfo SEQUENCE,
    //             certificates [0] IMPLICIT OPTIONAL, crls [1] OPTIONAL, signerInfos SET.
    let mut certs = Vec::new();
    while let Some((tlv, raw)) = signed_data.read_raw() {
        // certificates [0] IMPLICIT = 0xA0 (constructed, context 0).
        if tlv.tag == der::TAG_CONTEXT_0 {
            let mut set = tlv.reader();
            while let Some((choice, choice_raw)) = set.read_raw() {
                // CertificateChoices: a plain X.509 Certificate is a SEQUENCE.
                if choice.tag == der::TAG_SEQUENCE {
                    certs.push(choice_raw.to_vec());
                }
            }
            break;
        }
        let _ = raw;
    }
    certs
}

/// The `/VRI` dictionary key for a signature: the **upper-case hex of the SHA-1**
/// of the signature's `/Contents` bytes (the CMS blob), per the PAdES DSS/VRI
/// convention. `contents` is the raw (non-hex-encoded) signature value.
pub fn vri_key(contents: &[u8]) -> String {
    let digest = Sha1::digest(contents);
    let mut key = String::with_capacity(40);
    for byte in digest {
        key.push_str(&format!("{byte:02X}"));
    }
    key
}

/// The SHA-256 of a blob — used to surface a stable id for embedded validation
/// material in tests/diagnostics (not part of the PDF structure).
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Decode a short big-endian INTEGER/ENUMERATED content into a `u32`. Tolerates a
/// leading DER sign byte; rejects anything wider than 4 octets.
fn be_u32(bytes: &[u8]) -> Option<u32> {
    let trimmed = match bytes {
        [0x00, rest @ ..] => rest,
        other => other,
    };
    if trimmed.is_empty() {
        return Some(0);
    }
    if trimmed.len() > 4 {
        return None;
    }
    let mut value = 0u32;
    for &b in trimmed {
        value = (value << 8) | u32::from(b);
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sign::Signer;

    fn test_signer() -> Signer {
        let randomness: Vec<u8> = (0..256).map(|i| (i * 53 + 7) as u8).collect();
        Signer::generate(
            "GigaPDF LTV Signer",
            "260614000000Z",
            "360614000000Z",
            1024,
            &randomness,
        )
        .expect("signer")
    }

    #[test]
    fn ocsp_request_is_well_formed_der() {
        let signer = test_signer();
        let cert = signer.certificate();
        // Self-issued: issuer == subject, fine for shape checking.
        let req = build_ocsp_request(cert, cert, Some(&[0xDE, 0xAD, 0xBE, 0xEF])).expect("request");
        assert_eq!(req[0], 0x30, "OCSPRequest is a SEQUENCE");
        // It descends: OCSPRequest -> TBSRequest -> requestList -> Request -> CertID.
        let mut top = der::Reader::new(&req);
        let mut ocsp = top.descend(der::TAG_SEQUENCE).expect("ocsp req");
        let mut tbs = ocsp.descend(der::TAG_SEQUENCE).expect("tbs");
        let mut list = tbs.descend(der::TAG_SEQUENCE).expect("request list");
        let mut request = list.descend(der::TAG_SEQUENCE).expect("request");
        let mut cert_id = request.descend(der::TAG_SEQUENCE).expect("certID");
        let alg = cert_id.next_tag(der::TAG_SEQUENCE).expect("hashAlgorithm");
        // hashAlgorithm carries id-sha1.
        let mut alg_reader = alg.reader();
        let oid = alg_reader.next_tag(der::TAG_OID).expect("oid");
        assert!(oid.is_oid(OID_SHA1), "CertID hash is SHA-1");
        let name_hash = cert_id.next_tag(der::TAG_OCTET_STRING).expect("nameHash");
        assert_eq!(name_hash.content.len(), 20, "SHA-1 issuer name hash");
        let key_hash = cert_id.next_tag(der::TAG_OCTET_STRING).expect("keyHash");
        assert_eq!(key_hash.content.len(), 20, "SHA-1 issuer key hash");
        cert_id.next_tag(der::TAG_INTEGER).expect("serialNumber");
    }

    #[test]
    fn parse_ocsp_accepts_successful_and_rejects_failure() {
        // OCSPResponse { responseStatus ENUMERATED 0, responseBytes [0] {...} }.
        let ok = der::sequence(&[
            der::tlv(0x0A, &[0x00]), // successful
            der::context(0, &der::octet_string(b"basic-ocsp-response")),
        ]);
        let parsed = parse_ocsp_response(&ok).expect("successful");
        assert_eq!(parsed.status, 0);
        assert_eq!(parsed.response_der, ok, "embedded verbatim");

        // responseStatus 6 = unauthorized → rejected.
        let bad = der::sequence(&[der::tlv(0x0A, &[0x06])]);
        assert!(parse_ocsp_response(&bad).is_none());
    }

    #[test]
    fn parse_crl_requires_crl_shape() {
        // CertificateList { tbsCertList SEQ, sigAlg SEQ, sigValue BIT STRING }.
        let crl = der::sequence(&[
            der::sequence(&[der::integer_u32(1)]), // tbsCertList stand-in
            der::sequence(&[der::oid(&[1, 2, 840, 113549, 1, 1, 11])]), // sigAlg
            der::bit_string(&[0xAB, 0xCD]),        // signatureValue
        ]);
        assert_eq!(parse_crl(&crl).as_deref(), Some(crl.as_slice()));

        // An OCSP-looking blob (ENUMERATED first) is not a CRL.
        let not_crl = der::sequence(&[der::tlv(0x0A, &[0x00])]);
        assert!(parse_crl(&not_crl).is_none());
    }

    #[test]
    fn vri_key_is_upper_hex_sha1() {
        let key = vri_key(b"signature contents");
        assert_eq!(key.len(), 40, "40 hex chars for a 20-byte SHA-1");
        assert!(
            key.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()),
            "upper-case hex"
        );
        let expected = {
            let d = Sha1::digest(b"signature contents");
            d.iter().map(|b| format!("{b:02X}")).collect::<String>()
        };
        assert_eq!(key, expected);
    }

    #[test]
    fn self_signed_cert_has_no_revocation_sources() {
        // A freshly generated self-signed signer advertises neither AIA nor CRL-DP.
        let signer = test_signer();
        assert!(ocsp_url(signer.certificate()).is_none());
        assert!(crl_url(signer.certificate()).is_none());
        // And a single-cert chain (its own issuer absent) yields no sources.
        let plans = plan_chain(&[signer.certificate().to_vec()], None);
        assert_eq!(plans.len(), 1);
        assert!(plans[0].sources.is_empty());
    }

    #[test]
    fn certificates_round_trip_through_cms() {
        // Build a real detached CMS and pull its embedded cert back out.
        let signer = test_signer();
        let cms = signer.detached_cms(b"document bytes");
        let certs = certificates_from_cms(&cms);
        assert_eq!(certs.len(), 1, "one embedded certificate");
        assert_eq!(
            certs[0],
            signer.certificate(),
            "extracted cert is the signer's, verbatim"
        );
    }
}
