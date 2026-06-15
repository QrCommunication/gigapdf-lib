//! A minimal ASN.1 DER encoder — zero dependencies.
//!
//! Just the pieces X.509 certificates and CMS `SignedData` need: definite-length
//! TLV encoding for INTEGER, OID, NULL, OCTET/BIT STRING, SEQUENCE, SET, the
//! string and time types, and explicit/implicit context tags. Encoders return
//! owned `Vec<u8>` and compose by nesting.

/// Definite-length encoding (short form `< 128`, else long form).
fn length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else {
        let mut bytes = Vec::new();
        let mut n = len;
        while n > 0 {
            bytes.insert(0, (n & 0xFF) as u8);
            n >>= 8;
        }
        let mut out = vec![0x80 | bytes.len() as u8];
        out.extend_from_slice(&bytes);
        out
    }
}

/// A tag-length-value triple.
pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 4);
    out.push(tag);
    out.extend_from_slice(&length(content.len()));
    out.extend_from_slice(content);
    out
}

/// INTEGER from big-endian magnitude bytes (a leading `0x00` is added when the
/// high bit is set, keeping the value positive; zero encodes as a single `00`).
pub fn integer(magnitude: &[u8]) -> Vec<u8> {
    let first = magnitude
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(magnitude.len());
    let trimmed = &magnitude[first..];
    let mut body = Vec::new();
    if trimmed.is_empty() {
        body.push(0x00);
    } else {
        if trimmed[0] & 0x80 != 0 {
            body.push(0x00);
        }
        body.extend_from_slice(trimmed);
    }
    tlv(0x02, &body)
}

/// INTEGER from a small unsigned value.
pub fn integer_u32(value: u32) -> Vec<u8> {
    integer(&value.to_be_bytes())
}

/// OBJECT IDENTIFIER from its arc list.
pub fn oid(arcs: &[u64]) -> Vec<u8> {
    let mut body = Vec::new();
    if arcs.len() >= 2 {
        body.push((arcs[0] * 40 + arcs[1]) as u8);
        for &arc in &arcs[2..] {
            let mut stack = Vec::new();
            let mut v = arc;
            stack.push((v & 0x7F) as u8);
            v >>= 7;
            while v > 0 {
                stack.push((v & 0x7F) as u8 | 0x80);
                v >>= 7;
            }
            stack.reverse();
            body.extend_from_slice(&stack);
        }
    }
    tlv(0x06, &body)
}

/// NULL.
pub fn null() -> Vec<u8> {
    tlv(0x05, &[])
}

/// OCTET STRING.
pub fn octet_string(content: &[u8]) -> Vec<u8> {
    tlv(0x04, content)
}

/// BIT STRING (with the leading "0 unused bits" octet).
pub fn bit_string(content: &[u8]) -> Vec<u8> {
    let mut body = vec![0x00];
    body.extend_from_slice(content);
    tlv(0x03, &body)
}

/// SEQUENCE of already-encoded members.
pub fn sequence(members: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x30, &members.concat())
}

/// SET of already-encoded members.
pub fn set(members: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x31, &members.concat())
}

/// UTF8String.
pub fn utf8_string(text: &str) -> Vec<u8> {
    tlv(0x0C, text.as_bytes())
}

/// PrintableString.
pub fn printable_string(text: &str) -> Vec<u8> {
    tlv(0x13, text.as_bytes())
}

/// UTCTime (`YYMMDDHHMMSSZ`).
pub fn utc_time(text: &str) -> Vec<u8> {
    tlv(0x17, text.as_bytes())
}

/// A context-specific constructed tag `[n]` wrapping `content` (explicit).
pub fn context(tag: u8, content: &[u8]) -> Vec<u8> {
    tlv(0xA0 | tag, content)
}

/// A context-specific primitive tag `[n]` carrying `content` (implicit).
pub fn context_primitive(tag: u8, content: &[u8]) -> Vec<u8> {
    tlv(0x80 | tag, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn oid_encoding() {
        // 1.2.840.113549.1.1.11 (sha256WithRSAEncryption).
        let o = oid(&[1, 2, 840, 113549, 1, 1, 11]);
        assert_eq!(hex(&o), "06092a864886f70d01010b");
    }

    #[test]
    fn integer_pads_high_bit() {
        // 0x80 must gain a leading 0x00 to stay positive.
        assert_eq!(hex(&integer(&[0x80])), "02020080");
        // Leading zeros are trimmed.
        assert_eq!(hex(&integer(&[0x00, 0x00, 0x2a])), "02012a");
    }

    #[test]
    fn long_length_form() {
        let content = vec![0xABu8; 200];
        let seq = sequence(&[octet_string(&content)]);
        // SEQUENCE tag, then long-form length (0x81 0xCE = 206 bytes of content).
        assert_eq!(seq[0], 0x30);
        assert_eq!(seq[1], 0x81);
    }
}
