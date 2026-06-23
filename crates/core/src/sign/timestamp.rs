//! RFC 3161 trusted timestamps — the TimeStampReq we build and the
//! TimeStampResp we parse, hand-rolled with the in-tree DER codec ([`super::der`]).
//!
//! The core never performs the network round trip: it emits the request DER, the
//! host POSTs it to a TSA (`Content-Type: application/timestamp-query`), and
//! hands the reply back for parsing. This mirrors the two-phase HTML/font
//! resource model (`html::needed_resources` → host fetch → `html::render_with`).
//!
//! Only the slice of RFC 3161 a PAdES-B-T signature timestamp needs is
//! implemented: a SHA-256 `MessageImprint` over the signer's signature value,
//! `certReq = TRUE` (so the TSA cert rides inside the token), an optional nonce,
//! and — on the way back — the `PKIStatusInfo` gate plus extraction of the
//! `TimeStampToken` (a CMS `ContentInfo`) embedded verbatim as the signature's
//! unsigned `id-aa-timeStampToken` attribute.

use super::der;
use sha2::{Digest, Sha256};

/// `id-sha256` arc list (2.16.840.1.101.3.4.2.1).
const OID_SHA256: &[u64] = &[2, 16, 840, 1, 101, 3, 4, 2, 1];

/// Build an RFC 3161 `TimeStampReq` (DER) for the SHA-256 `imprint` of the
/// content being timestamped (for PAdES-B-T this is the signer's signature
/// value). `nonce` is optional 64–128 random bits supplied by the host; when
/// present it is echoed by the TSA and lets the caller correlate request and
/// response. `cert_req = TRUE` is always set so the TSA certificate travels
/// inside the returned token (needed for later validation).
///
/// ```text
/// TimeStampReq ::= SEQUENCE {
///   version        INTEGER { v1(1) },
///   messageImprint MessageImprint,
///   reqPolicy      TSAPolicyId OPTIONAL,   -- omitted (let the TSA choose)
///   nonce          INTEGER     OPTIONAL,
///   certReq        BOOLEAN DEFAULT FALSE } -- set TRUE
///
/// MessageImprint ::= SEQUENCE {
///   hashAlgorithm  AlgorithmIdentifier,    -- id-sha256, NULL params
///   hashedMessage  OCTET STRING }
/// ```
pub fn build_request(imprint: &[u8], nonce: Option<&[u8]>) -> Vec<u8> {
    let algorithm = der::sequence(&[der::oid(OID_SHA256), der::null()]);
    let message_imprint = der::sequence(&[algorithm, der::octet_string(imprint)]);

    let mut members = vec![der::integer_u32(1), message_imprint];
    if let Some(nonce) = nonce {
        members.push(der::integer(nonce));
    }
    // certReq BOOLEAN TRUE (0x01 0x01 0xFF).
    members.push(der::tlv(0x01, &[0xFF]));

    der::sequence(&members)
}

/// The SHA-256 digest of `data` — the `hashedMessage` of the `MessageImprint`.
pub fn sha256_imprint(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

/// Outcome of parsing a `TimeStampResp`: the extracted `TimeStampToken` (a CMS
/// `ContentInfo`, embedded verbatim) plus the diagnostic `PKIStatus`.
#[derive(Debug, Clone)]
pub struct TimestampToken {
    /// The `TimeStampToken` `ContentInfo` DER, ready to be wrapped as the
    /// `id-aa-timeStampToken` unsigned attribute value.
    pub token_der: Vec<u8>,
    /// The TSA's `PKIStatus` integer (0 = granted, 1 = grantedWithMods).
    pub status: u32,
}

/// Parse an RFC 3161 `TimeStampResp` and extract its `TimeStampToken`.
///
/// ```text
/// TimeStampResp ::= SEQUENCE {
///   status         PKIStatusInfo,            -- SEQUENCE { status INTEGER, ... }
///   timeStampToken TimeStampToken OPTIONAL } -- a CMS ContentInfo (SEQUENCE)
/// ```
///
/// Accepts a response whose body is the bare `TimeStampResp`, or — defensively —
/// a bare `TimeStampToken` `ContentInfo` (some misbehaving servers reply with
/// just the token). Returns `None` if the status is not granted/grantedWithMods,
/// or if no token is present, or the DER is malformed.
pub fn parse_response(response: &[u8]) -> Option<TimestampToken> {
    let mut outer = der::Reader::new(response);
    let mut body = outer.descend(der::TAG_SEQUENCE)?;

    // First member of the SEQUENCE: a PKIStatusInfo (status INTEGER first) for a
    // TimeStampResp, or an OID for a bare ContentInfo token.
    let (first, first_raw) = body.read_raw()?;

    if first.tag == der::TAG_OID {
        // The whole `response` was already a TimeStampToken ContentInfo.
        return Some(TimestampToken {
            token_der: response.to_vec(),
            status: 0,
        });
    }
    if first.tag != der::TAG_SEQUENCE {
        return None;
    }

    // PKIStatusInfo ::= SEQUENCE { status PKIStatus (INTEGER), ... }
    let mut status_info = first.reader();
    let status_tlv = status_info.next_tag(der::TAG_INTEGER)?;
    let status = be_u32(status_tlv.content)?;
    // 0 = granted, 1 = grantedWithMods. Anything else is a rejection.
    if status > 1 {
        return None;
    }
    let _ = first_raw; // the status info is consumed; the token is the next member

    // timeStampToken: the next member, a ContentInfo SEQUENCE, kept verbatim.
    let (token, token_raw) = body.read_raw()?;
    if token.tag != der::TAG_SEQUENCE {
        return None;
    }
    Some(TimestampToken {
        token_der: token_raw.to_vec(),
        status,
    })
}

/// Decode a short big-endian INTEGER content (the status code) into a `u32`.
/// Tolerates the optional DER sign byte; rejects anything wider than 4 octets.
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

    #[test]
    fn request_is_well_formed_der() {
        let imprint = [0xABu8; 32];
        let req = build_request(&imprint, Some(&[0x01, 0x02, 0x03, 0x04]));
        // SEQUENCE wrapper.
        assert_eq!(req[0], 0x30);
        // It parses: version INTEGER 1, then the MessageImprint SEQUENCE.
        let mut top = der::Reader::new(&req);
        let mut inner = top.descend(der::TAG_SEQUENCE).expect("req seq");
        let version = inner.next_tag(der::TAG_INTEGER).expect("version");
        assert_eq!(version.content, &[0x01]);
        let imprint_seq = inner.next_tag(der::TAG_SEQUENCE).expect("messageImprint");
        // The hashed message OCTET STRING carries our digest.
        let mut mi = imprint_seq.reader();
        let _algo = mi.next_tag(der::TAG_SEQUENCE).expect("alg");
        let hashed = mi.next_tag(der::TAG_OCTET_STRING).expect("hashedMessage");
        assert_eq!(hashed.content, &imprint);
        // nonce INTEGER then certReq BOOLEAN TRUE.
        let nonce = inner.next_tag(der::TAG_INTEGER).expect("nonce");
        assert_eq!(nonce.content, &[0x01, 0x02, 0x03, 0x04]);
        let cert_req = inner.read().expect("certReq");
        assert_eq!(cert_req.tag, 0x01);
        assert_eq!(cert_req.content, &[0xFF]);
    }

    #[test]
    fn request_omits_nonce_when_absent() {
        let req = build_request(&[0u8; 32], None);
        let mut top = der::Reader::new(&req);
        let mut inner = top.descend(der::TAG_SEQUENCE).expect("seq");
        inner.next_tag(der::TAG_INTEGER).expect("version");
        inner.next_tag(der::TAG_SEQUENCE).expect("messageImprint");
        // With no nonce the next member is certReq BOOLEAN, not an INTEGER.
        let next = inner.read().expect("certReq");
        assert_eq!(next.tag, 0x01);
    }

    #[test]
    fn parse_extracts_token_from_granted_response() {
        // TimeStampResp { PKIStatusInfo { status 0 }, token ContentInfo }.
        let status_info = der::sequence(&[der::integer_u32(0)]);
        // A stand-in token: a SEQUENCE shaped like a ContentInfo. Its exact bytes
        // must come back verbatim.
        let token = der::sequence(&[
            der::oid(&[1, 2, 840, 113549, 1, 7, 2]), // signedData OID
            der::context(0, &der::octet_string(b"opaque-token-body")),
        ]);
        let resp = der::sequence(&[status_info, token.clone()]);

        let parsed = parse_response(&resp).expect("granted");
        assert_eq!(parsed.status, 0);
        assert_eq!(parsed.token_der, token, "token returned verbatim");
    }

    #[test]
    fn parse_rejects_non_granted_status() {
        let status_info = der::sequence(&[der::integer_u32(2)]); // rejection
        let token = der::sequence(&[der::oid(&[1, 2, 840, 113549, 1, 7, 2])]);
        let resp = der::sequence(&[status_info, token]);
        assert!(parse_response(&resp).is_none());
    }

    #[test]
    fn parse_accepts_bare_token_content_info() {
        // Some servers return only the ContentInfo (OID first, no status wrapper).
        let token = der::sequence(&[
            der::oid(&[1, 2, 840, 113549, 1, 7, 2]),
            der::context(0, &der::octet_string(b"body")),
        ]);
        let parsed = parse_response(&token).expect("bare token");
        assert_eq!(parsed.token_der, token);
        assert_eq!(parsed.status, 0);
    }
}
