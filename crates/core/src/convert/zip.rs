//! A minimal ZIP archive **writer** (PKWARE APPNOTE) — zero dependencies.
//!
//! Office formats (OOXML: `.docx`/`.pptx`/`.xlsx`, ODF: `.odt`/`.odp`/`.ods`)
//! are ZIP containers of XML parts plus embedded media. This writer is the
//! container half; the per-format XML lives in [`super::office`]. Entries are
//! either **stored** (method 0 — for already-compressed media like PNG) or
//! **deflated** (method 8 — for XML), the latter reusing our own DEFLATE
//! encoder. Sizes and CRCs are known up front (we hold each part in memory),
//! so no data descriptors are emitted — the simplest spec-valid layout.

use crate::filters::deflate::deflate;

/// CRC-32 (IEEE 802.3, polynomial `0xEDB88320`) over `data` — the checksum ZIP
/// stores for every entry. (Self-contained here so the archive writer owns its
/// own checksum rather than coupling to the PNG encoder's private copy.)
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

// MS-DOS modification date/time. The format is deterministic (no clock access in
// the engine), so a fixed timestamp — 1980-01-01 00:00:00, the DOS epoch — keeps
// archives byte-reproducible.
const DOS_DATE: u16 = 0x0021; // (year-1980)<<9 | month<<5 | day = 0|1|1
const DOS_TIME: u16 = 0x0000;

#[derive(Debug)]
struct Entry {
    name: String,
    crc: u32,
    comp_size: u32,
    uncomp_size: u32,
    offset: u32,
    method: u16,
}

/// Accumulates entries into an in-memory ZIP archive.
#[derive(Debug, Default)]
pub struct ZipWriter {
    out: Vec<u8>,
    entries: Vec<Entry>,
}

impl ZipWriter {
    pub fn new() -> ZipWriter {
        ZipWriter::default()
    }

    /// Add an entry stored uncompressed (method 0). Use for already-compressed
    /// payloads (PNG/JPEG) where DEFLATE would only waste CPU.
    pub fn add_stored(&mut self, name: &str, data: &[u8]) {
        self.add_entry(name, data, data.to_vec(), 0);
    }

    /// Add an entry compressed with DEFLATE (method 8). Use for XML/text parts.
    pub fn add_deflated(&mut self, name: &str, data: &[u8]) {
        let compressed = deflate(data);
        // Never let "compression" grow a tiny part: fall back to stored.
        if compressed.len() < data.len() {
            self.add_entry(name, data, compressed, 8);
        } else {
            self.add_stored(name, data);
        }
    }

    fn add_entry(&mut self, name: &str, raw: &[u8], payload: Vec<u8>, method: u16) {
        let crc = crc32(raw);
        let offset = self.out.len() as u32;
        let name_bytes = name.as_bytes();

        // Local file header.
        self.out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        self.out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
        self.out.extend_from_slice(&method.to_le_bytes());
        self.out.extend_from_slice(&DOS_TIME.to_le_bytes());
        self.out.extend_from_slice(&DOS_DATE.to_le_bytes());
        self.out.extend_from_slice(&crc.to_le_bytes());
        self.out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        self.out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        self.out.extend_from_slice(name_bytes);
        self.out.extend_from_slice(&payload);

        self.entries.push(Entry {
            name: name.to_string(),
            crc,
            comp_size: payload.len() as u32,
            uncomp_size: raw.len() as u32,
            offset,
            method,
        });
    }

    /// Finalize: append the central directory + end-of-central-directory record
    /// and return the complete archive bytes.
    pub fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.out.len() as u32;
        for entry in &self.entries {
            let name_bytes = entry.name.as_bytes();
            self.out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            self.out.extend_from_slice(&20u16.to_le_bytes()); // version made by
            self.out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            self.out.extend_from_slice(&0u16.to_le_bytes()); // flags
            self.out.extend_from_slice(&entry.method.to_le_bytes());
            self.out.extend_from_slice(&DOS_TIME.to_le_bytes());
            self.out.extend_from_slice(&DOS_DATE.to_le_bytes());
            self.out.extend_from_slice(&entry.crc.to_le_bytes());
            self.out.extend_from_slice(&entry.comp_size.to_le_bytes());
            self.out.extend_from_slice(&entry.uncomp_size.to_le_bytes());
            self.out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            self.out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            self.out.extend_from_slice(&0u16.to_le_bytes()); // comment len
            self.out.extend_from_slice(&0u16.to_le_bytes()); // disk number start
            self.out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            self.out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            self.out.extend_from_slice(&entry.offset.to_le_bytes());
            self.out.extend_from_slice(name_bytes);
        }
        let cd_size = self.out.len() as u32 - cd_offset;
        let count = self.entries.len() as u16;

        // End of central directory record.
        self.out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        self.out.extend_from_slice(&0u16.to_le_bytes()); // disk with CD
        self.out.extend_from_slice(&count.to_le_bytes()); // entries this disk
        self.out.extend_from_slice(&count.to_le_bytes()); // total entries
        self.out.extend_from_slice(&cd_size.to_le_bytes());
        self.out.extend_from_slice(&cd_offset.to_le_bytes());
        self.out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        self.out
    }
}

/// Read a ZIP archive into a map of `entry name → uncompressed bytes`, for the
/// reverse Office converters (OOXML/ODF are ZIPs of XML). Supports the two
/// methods we and office suites emit: stored (0) and DEFLATE (8); other methods
/// are skipped. Walks local file headers (sufficient for well-formed archives).
pub fn read_zip(zip: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
    use crate::filters::inflate::inflate;
    let mut out = std::collections::BTreeMap::new();
    let mut i = 0;
    while i + 30 <= zip.len() && zip[i..i + 4] == [0x50, 0x4b, 0x03, 0x04] {
        let method = u16::from_le_bytes([zip[i + 8], zip[i + 9]]);
        let comp = u32::from_le_bytes(zip[i + 18..i + 22].try_into().unwrap()) as usize;
        let nlen = u16::from_le_bytes([zip[i + 26], zip[i + 27]]) as usize;
        let elen = u16::from_le_bytes([zip[i + 28], zip[i + 29]]) as usize;
        let name_start = i + 30;
        let data_start = name_start + nlen + elen;
        if data_start + comp > zip.len() {
            break;
        }
        let name = String::from_utf8_lossy(&zip[name_start..name_start + nlen]).to_string();
        let payload = &zip[data_start..data_start + comp];
        let bytes = match method {
            0 => Some(payload.to_vec()),
            8 => inflate(payload).ok(),
            _ => None,
        };
        if let Some(bytes) = bytes {
            out.insert(name, bytes);
        }
        i = data_start + comp;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::inflate::inflate;

    /// Parse our own archive back (local headers only) to prove the framing and
    /// that deflated entries inflate to the original bytes.
    fn read_back(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 30 <= zip.len() && zip[i..i + 4] == [0x50, 0x4b, 0x03, 0x04] {
            let method = u16::from_le_bytes([zip[i + 8], zip[i + 9]]);
            let comp = u32::from_le_bytes(zip[i + 18..i + 22].try_into().unwrap()) as usize;
            let nlen = u16::from_le_bytes([zip[i + 26], zip[i + 27]]) as usize;
            let elen = u16::from_le_bytes([zip[i + 28], zip[i + 29]]) as usize;
            let name = String::from_utf8_lossy(&zip[i + 30..i + 30 + nlen]).to_string();
            let data_start = i + 30 + nlen + elen;
            let payload = &zip[data_start..data_start + comp];
            let raw = if method == 8 {
                inflate(payload).unwrap()
            } else {
                payload.to_vec()
            };
            out.push((name, raw));
            i = data_start + comp;
        }
        out
    }

    #[test]
    fn stored_and_deflated_round_trip() {
        let mut zip = ZipWriter::new();
        zip.add_stored("a.bin", &[1, 2, 3, 4, 5]);
        let xml = b"<root>".repeat(50);
        zip.add_deflated("b.xml", &xml);
        let archive = zip.finish();

        // Spec markers present.
        assert_eq!(&archive[0..4], &[0x50, 0x4b, 0x03, 0x04]);
        assert!(archive.windows(4).any(|w| w == [0x50, 0x4b, 0x05, 0x06]));

        let parts = read_back(&archive);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], ("a.bin".to_string(), vec![1, 2, 3, 4, 5]));
        assert_eq!(parts[1], ("b.xml".to_string(), xml));
    }

    #[test]
    fn tiny_incompressible_entry_falls_back_to_stored() {
        let mut zip = ZipWriter::new();
        zip.add_deflated("x", &[0xAB]);
        let archive = zip.finish();
        let method = u16::from_le_bytes([archive[8], archive[9]]);
        assert_eq!(method, 0, "1-byte part stays stored, never grows");
    }
}
