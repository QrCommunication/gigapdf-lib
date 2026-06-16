//! A minimal ASN.1 DER codec — zero dependencies.
//!
//! Just the pieces X.509 certificates and CMS `SignedData` need: definite-length
//! TLV encoding for INTEGER, OID, NULL, OCTET/BIT STRING, SEQUENCE, SET, the
//! string and time types, and explicit/implicit context tags. Encoders return
//! owned `Vec<u8>` and compose by nesting.
//!
//! The [`Reader`] half walks definite-length DER the other way — it is what the
//! PKCS#12 importer ([`super::pkcs12`]) uses to take a `.p12`/`.pfx` apart.

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

/// The sub-identifier octets of an OBJECT IDENTIFIER's arc list (the value
/// *without* the `0x06` tag/length) — handy for comparing a parsed OID against a
/// known constant.
pub fn oid_arcs(arcs: &[u64]) -> Vec<u8> {
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
    body
}

/// OBJECT IDENTIFIER from its arc list.
pub fn oid(arcs: &[u64]) -> Vec<u8> {
    tlv(0x06, &oid_arcs(arcs))
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

// ─── Decoding (the [`Reader`] half) ──────────────────────────────────────────

/// Universal/constructed tag bytes the reader matches against.
pub const TAG_INTEGER: u8 = 0x02;
pub const TAG_BIT_STRING: u8 = 0x03;
pub const TAG_OCTET_STRING: u8 = 0x04;
pub const TAG_OID: u8 = 0x06;
pub const TAG_SEQUENCE: u8 = 0x30;
pub const TAG_SET: u8 = 0x31;
/// `[0]` constructed, explicit (e.g. a CMS `ContentInfo` content field).
pub const TAG_CONTEXT_0: u8 = 0xA0;
/// `[0]` primitive, implicit (e.g. `EncryptedContentInfo.encryptedContent`).
pub const TAG_CONTEXT_0_PRIM: u8 = 0x80;

/// A parsed tag-length-value: the tag byte and a borrow of the content octets.
#[derive(Clone, Copy, Debug)]
pub struct Tlv<'a> {
    pub tag: u8,
    pub content: &'a [u8],
}

impl<'a> Tlv<'a> {
    /// A reader positioned over this value's content (for constructed types).
    pub fn reader(&self) -> Reader<'a> {
        Reader::new(self.content)
    }

    /// True iff this is an OBJECT IDENTIFIER equal to `arcs`.
    pub fn is_oid(&self, arcs: &[u64]) -> bool {
        self.tag == TAG_OID && self.content == oid_arcs(arcs).as_slice()
    }
}

/// A cursor over definite-length DER, yielding one TLV at a time.
///
/// Strictly definite-length: an indefinite length (`0x80`) or a length whose
/// long form exceeds four octets is rejected as malformed (returns `None`),
/// which is exactly what well-formed `.p12`/X.509 DER never uses.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader over `data`, positioned at the start.
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    /// True once every byte has been consumed.
    pub fn at_end(&self) -> bool {
        self.pos >= self.data.len()
    }

    /// Read the next TLV, advancing the cursor. `None` on truncation or a
    /// length encoding we refuse (indefinite / over-wide long form).
    pub fn read(&mut self) -> Option<Tlv<'a>> {
        let tag = *self.data.get(self.pos)?;
        self.pos += 1;
        let first = *self.data.get(self.pos)?;
        self.pos += 1;
        let len = if first < 0x80 {
            first as usize
        } else {
            let width = (first & 0x7f) as usize;
            if width == 0 || width > 4 {
                return None; // indefinite form or an absurd width
            }
            let mut len = 0usize;
            for _ in 0..width {
                let b = *self.data.get(self.pos)?;
                self.pos += 1;
                len = (len << 8) | b as usize;
            }
            len
        };
        let end = self.pos.checked_add(len)?;
        let content = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(Tlv { tag, content })
    }

    /// Like [`read`](Self::read), but also returns the *full* TLV slice
    /// (`tag..end`). Use it when a value must be re-emitted verbatim — e.g. an
    /// X.509 issuer Name or serialNumber copied into a CMS `SignerInfo`.
    pub fn read_raw(&mut self) -> Option<(Tlv<'a>, &'a [u8])> {
        let start = self.pos;
        let tlv = self.read()?;
        Some((tlv, &self.data[start..self.pos]))
    }

    /// Read the next TLV and require it to carry `tag`.
    pub fn next_tag(&mut self, tag: u8) -> Option<Tlv<'a>> {
        let tlv = self.read()?;
        (tlv.tag == tag).then_some(tlv)
    }

    /// Read the next TLV, descend into it (a constructed value), and return a
    /// reader over its content.
    pub fn descend(&mut self, tag: u8) -> Option<Reader<'a>> {
        Some(self.next_tag(tag)?.reader())
    }
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

    #[test]
    fn reader_round_trips_encoded_structure() {
        // Encode SEQUENCE { OID 1.2.840.113549.1.1.1, OCTET STRING "hi",
        //                   [0] { INTEGER 0x1234 } } then take it apart.
        let blob = sequence(&[
            oid(&[1, 2, 840, 113549, 1, 1, 1]),
            octet_string(b"hi"),
            context(0, &integer(&[0x12, 0x34])),
        ]);

        let mut top = Reader::new(&blob);
        let mut inner = top.descend(TAG_SEQUENCE).expect("seq");
        assert!(top.at_end(), "one top-level value");

        let oid_tlv = inner.read().expect("oid");
        assert!(oid_tlv.is_oid(&[1, 2, 840, 113549, 1, 1, 1]));
        assert!(!oid_tlv.is_oid(&[1, 2, 840, 113549, 1, 1, 11]));

        let octets = inner.next_tag(TAG_OCTET_STRING).expect("octets");
        assert_eq!(octets.content, b"hi");

        let mut explicit = inner.descend(TAG_CONTEXT_0).expect("[0]");
        let int = explicit.next_tag(TAG_INTEGER).expect("int");
        assert_eq!(int.content, &[0x12, 0x34]);
        assert!(inner.at_end());
    }

    #[test]
    fn reader_rejects_truncated_and_indefinite_lengths() {
        // Truncated content (claims 5 bytes, has 1).
        assert!(Reader::new(&[0x04, 0x05, 0xAA]).read().is_none());
        // Indefinite length (0x80) is refused outright.
        assert!(Reader::new(&[0x30, 0x80, 0x00, 0x00]).read().is_none());
    }
}
