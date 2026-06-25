//! A from-scratch **CFB (Compound File Binary)** container reader — the legacy
//! Microsoft OLE2 / Compound Document format ([MS-CFB]). Zero dependencies.
//!
//! CFB is a FAT-like filesystem inside a single file: a 512-byte header, a
//! sector-allocation table (the **FAT**), a red-black-tree **directory** of
//! storages (directories) and streams (files), and a secondary **mini-FAT** /
//! **mini-stream** for streams smaller than the cutoff (4096 bytes). The legacy
//! `.doc`/`.xls`/`.ppt` formats are CFB containers — `WordDocument`, `Workbook`
//! and `PowerPoint Document` are streams inside them — so this reader is the
//! shared foundation for the upcoming binary Office importers. It only reads;
//! there is no writer.
//!
//! **Robustness.** These files are untrusted input. Every sector and chain walk
//! is bounded (iteration capped to the table length, with explicit cycle
//! detection), all slicing is length-checked, and any malformed, truncated or
//! cyclic structure yields `None`/empty rather than a panic or an infinite loop.
//! `#![forbid(unsafe_code)]` is in force crate-wide.
//!
//! [MS-CFB]: https://learn.microsoft.com/openspecs/windows_protocols/ms-cfb/

/// CFB file signature (`D0 CF 11 E0 A1 B1 1A E1`) — the first 8 header bytes.
const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Sentinel sector values stored in the FAT / mini-FAT (MS-CFB §2.2).
const FREESECT: u32 = 0xFFFF_FFFF; // unallocated sector
const ENDOFCHAIN: u32 = 0xFFFF_FFFE; // end of a sector chain
const FATSECT: u32 = 0xFFFF_FFFD; // sector used by the FAT itself
const DIFSECT: u32 = 0xFFFF_FFFC; // sector used by the DIFAT

/// The header stores the first 109 FAT-sector locations inline; the rest chain
/// through DIFAT sectors (MS-CFB §2.2 + §2.5).
const HEADER_DIFAT_COUNT: usize = 109;
/// Offset of the inline DIFAT array within the 512-byte header.
const HEADER_DIFAT_OFFSET: usize = 76;
/// A directory entry is exactly 128 bytes (MS-CFB §2.6.1).
const DIR_ENTRY_SIZE: usize = 128;
/// The maximum byte length of a directory-entry name field (32 UTF-16 units).
const DIR_NAME_BYTES: usize = 64;

/// Object types in a directory entry (MS-CFB §2.6.1).
const OBJ_STORAGE: u8 = 1; // a storage (directory)
const OBJ_STREAM: u8 = 2; // a stream (file)
const OBJ_ROOT: u8 = 5; // the single root storage

/// A "no sibling/child" stream-ID in the red-black directory tree.
const NOSTREAM: u32 = 0xFFFF_FFFF;

/// One parsed directory entry (MS-CFB §2.6.1). Tree links are kept so the
/// directory can be walked into a flat, path-addressable list.
#[derive(Debug, Clone)]
struct DirEntry {
    name: String,
    obj_type: u8,
    left: u32,
    right: u32,
    child: u32,
    start_sector: u32,
    /// Stream size in bytes. For the v3 (512-byte sector) layout only the low 32
    /// bits are significant; the high bytes are masked off at parse time.
    size: u64,
}

/// One resolved stream: its name, the full storage path that reaches it, and the
/// `(start_sector, size)` needed to reassemble its bytes.
#[derive(Debug, Clone)]
struct Stream {
    name: String,
    /// Path from (but excluding) the root, e.g. `["Storage", "SubStream"]`.
    path: Vec<String>,
    start_sector: u32,
    size: u64,
}

/// A read-only Compound File. Built by [`Cfb::open`]; query with
/// [`Cfb::read_stream`], [`Cfb::stream_names`] and [`Cfb::read_stream_at_path`].
#[derive(Debug)]
pub struct Cfb {
    /// The whole file, retained so streams can be sliced lazily on read.
    data: Vec<u8>,
    /// Regular sector size in bytes (512 for v3, 4096 for v4).
    sector_size: usize,
    /// Mini-sector size in bytes (always 64 in practice).
    mini_sector_size: usize,
    /// Streams `< mini_cutoff` live in the mini-stream; the rest use the FAT.
    mini_cutoff: u64,
    /// The master sector-allocation table (one `u32` next-pointer per sector).
    fat: Vec<u32>,
    /// The mini sector-allocation table (next-pointer per mini-sector).
    mini_fat: Vec<u32>,
    /// The reassembled mini-stream container (the root entry's FAT chain).
    mini_stream: Vec<u8>,
    /// Every stream found in the directory, flattened with its storage path.
    streams: Vec<Stream>,
}

impl Cfb {
    /// Parse `bytes` as a Compound File. Returns `None` if the signature is
    /// wrong or the header is malformed/truncated. Never panics.
    pub fn open(bytes: &[u8]) -> Option<Cfb> {
        if bytes.len() < 512 || bytes[..8] != SIGNATURE {
            return None;
        }

        // Byte-order mark must be little-endian `FE FF` (MS-CFB §2.2). The whole
        // format is little-endian; a big-endian mark is not defined.
        if read_u16(bytes, 28)? != 0xFFFE {
            return None;
        }

        // Sector shift: 0x0009 ⇒ 512-byte sectors (v3), 0x000C ⇒ 4096 (v4).
        let sector_shift = read_u16(bytes, 30)?;
        if sector_shift != 0x0009 && sector_shift != 0x000C {
            return None;
        }
        let sector_size = 1usize << sector_shift;

        // Mini-sector shift is fixed at 0x0006 ⇒ 64-byte mini-sectors.
        let mini_shift = read_u16(bytes, 32)?;
        if mini_shift != 0x0006 {
            return None;
        }
        let mini_sector_size = 1usize << mini_shift;

        let num_fat_sectors = read_u32(bytes, 44)? as usize;
        let dir_start = read_u32(bytes, 48)?;
        let mini_cutoff = read_u32(bytes, 56)? as u64;
        let mini_fat_start = read_u32(bytes, 60)?;
        let num_mini_fat = read_u32(bytes, 64)? as usize;
        let difat_start = read_u32(bytes, 68)?;
        let num_difat = read_u32(bytes, 72)? as usize;

        // Total sectors after the 512-byte header — the universal bound for any
        // sector index and for every chain-walk iteration cap.
        let total_sectors = bytes.len().saturating_sub(512) / sector_size;

        let fat_sector_list = build_fat_sector_list(
            bytes,
            sector_size,
            difat_start,
            num_difat,
            num_fat_sectors,
            total_sectors,
        );
        let fat = read_fat(bytes, sector_size, &fat_sector_list, total_sectors);

        // The directory is itself a regular FAT chain of 128-byte entries.
        let dir_bytes = read_fat_chain(bytes, sector_size, &fat, dir_start, None);
        let (entries, root) = parse_directory(&dir_bytes);
        let root = root?; // a valid CFB always has a root storage entry

        // Mini-FAT: a regular FAT chain reinterpreted as `u32` next-pointers.
        let mini_fat = if num_mini_fat > 0 && is_real_sector(mini_fat_start) {
            let raw = read_fat_chain(bytes, sector_size, &fat, mini_fat_start, None);
            le_u32s(&raw)
        } else {
            Vec::new()
        };

        // Mini-stream = the root entry's regular-FAT chain, truncated to its size.
        let mut mini_stream = read_fat_chain(bytes, sector_size, &fat, root.start_sector, None);
        mini_stream.truncate(root.size as usize);

        // Flatten the directory red-black trees into path-addressed streams.
        let streams = flatten_streams(&entries, root.child);

        Some(Cfb {
            data: bytes.to_vec(),
            sector_size,
            mini_sector_size,
            mini_cutoff,
            fat,
            mini_fat,
            mini_stream,
            streams,
        })
    }

    /// Read a stream by its entry name (e.g. `"WordDocument"`, `"1Table"`,
    /// `"Workbook"`). If several streams share the name (possible across nested
    /// storages), the first found in directory order wins — use
    /// [`Cfb::read_stream_at_path`] to disambiguate. `None` if no such stream.
    pub fn read_stream(&self, name: &str) -> Option<Vec<u8>> {
        let s = self.streams.iter().find(|s| s.name == name)?;
        Some(self.reassemble(s.start_sector, s.size))
    }

    /// All stream names in directory order (storages are not listed). Names may
    /// repeat if the same name occurs in different storages.
    pub fn stream_names(&self) -> Vec<String> {
        self.streams.iter().map(|s| s.name.clone()).collect()
    }

    /// Read a stream addressed by its full storage path, e.g.
    /// `&["ObjectPool", "_123", "CONTENTS"]` or simply `&["WordDocument"]` for a
    /// top-level stream. The path is matched exactly (root excluded). `None` if
    /// no stream lives at that path.
    pub fn read_stream_at_path(&self, path: &[&str]) -> Option<Vec<u8>> {
        let s = self
            .streams
            .iter()
            .find(|s| s.path.len() == path.len() && s.path.iter().zip(path).all(|(a, b)| a == b))?;
        Some(self.reassemble(s.start_sector, s.size))
    }

    /// Reassemble a stream's bytes: streams smaller than the cutoff come from the
    /// mini-stream via the mini-FAT; larger streams come from the regular FAT.
    fn reassemble(&self, start: u32, size: u64) -> Vec<u8> {
        if size < self.mini_cutoff {
            self.read_mini_chain(start, size)
        } else {
            let mut out = read_fat_chain(
                &self.data,
                self.sector_size,
                &self.fat,
                start,
                Some(size as usize),
            );
            out.truncate(size as usize);
            out
        }
    }

    /// Walk a mini-FAT chain inside the mini-stream, capped at `size` bytes.
    fn read_mini_chain(&self, start: u32, size: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let cap = self.mini_fat.len().saturating_add(1);
        let mut visited = vec![false; cap];
        let mut sector = start;
        let mut steps = 0usize;
        while is_real_sector(sector) && steps < cap {
            let idx = sector as usize;
            if idx >= self.mini_fat.len() || visited[idx] {
                break; // out-of-range pointer or a cycle ⇒ stop
            }
            visited[idx] = true;
            let base = idx.checked_mul(self.mini_sector_size);
            let Some(base) = base else { break };
            let end = base.saturating_add(self.mini_sector_size);
            if end > self.mini_stream.len() {
                break;
            }
            out.extend_from_slice(&self.mini_stream[base..end]);
            if out.len() >= size as usize {
                break; // enough bytes gathered; no need to chase the rest
            }
            sector = self.mini_fat[idx];
            steps += 1;
        }
        out.truncate(size as usize);
        out
    }
}

/// Read a little-endian `u16` at `off`, or `None` if it would run past the end.
fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    let end = off.checked_add(2)?;
    if end > bytes.len() {
        return None;
    }
    Some(u16::from_le_bytes([bytes[off], bytes[off + 1]]))
}

/// Read a little-endian `u32` at `off`, or `None` if it would run past the end.
fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    if end > bytes.len() {
        return None;
    }
    Some(u32::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
    ]))
}

/// True for a sector index that points at real data, i.e. not one of the four
/// reserved sentinels (MS-CFB §2.2). All four are ≥ `DIFSECT`, so the range
/// check is equivalent — spelled out here to be explicit about each marker.
fn is_real_sector(sector: u32) -> bool {
    !matches!(sector, FREESECT | ENDOFCHAIN | FATSECT | DIFSECT)
}

/// Byte offset of sector `n` within the file: the header occupies sector −1, so
/// sector `n` starts at `(n + 1) * sector_size`. `None` on overflow.
fn sector_offset(sector: u32, sector_size: usize) -> Option<usize> {
    (sector as usize).checked_add(1)?.checked_mul(sector_size)
}

/// Borrow the `sector_size` bytes of sector `n`, length-checked. `None` if the
/// sector lies (even partly) outside the file.
fn sector_slice(bytes: &[u8], sector: u32, sector_size: usize) -> Option<&[u8]> {
    let base = sector_offset(sector, sector_size)?;
    let end = base.checked_add(sector_size)?;
    bytes.get(base..end)
}

/// Reinterpret a byte buffer as little-endian `u32`s (trailing partial dropped).
fn le_u32s(raw: &[u8]) -> Vec<u32> {
    raw.chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Build the ordered list of FAT-sector locations: the 109 inline header
/// entries first, then any chained DIFAT sectors (MS-CFB §2.5). Every walk is
/// bounded by `total_sectors` and guarded against cycles.
fn build_fat_sector_list(
    bytes: &[u8],
    sector_size: usize,
    difat_start: u32,
    num_difat: usize,
    num_fat_sectors: usize,
    total_sectors: usize,
) -> Vec<u32> {
    let mut list = Vec::new();

    // Inline DIFAT: 109 entries in the header.
    for k in 0..HEADER_DIFAT_COUNT {
        if let Some(sec) = read_u32(bytes, HEADER_DIFAT_OFFSET + k * 4) {
            if is_real_sector(sec) {
                list.push(sec);
            }
        }
    }

    // Chained DIFAT sectors. Each holds `(sector_size/4 - 1)` FAT-sector
    // pointers, the last `u32` being the next DIFAT sector. Cap the walk to the
    // declared DIFAT count (plus slack) and to the file's sector count, and
    // detect cycles via a visited set.
    let entries_per_difat = sector_size / 4 - 1;
    let mut visited = vec![false; total_sectors.saturating_add(1)];
    let mut difat = difat_start;
    let mut steps = 0usize;
    let cap = num_difat.saturating_add(total_sectors).saturating_add(1);
    while is_real_sector(difat) && steps < cap {
        let idx = difat as usize;
        if idx < visited.len() {
            if visited[idx] {
                break; // DIFAT cycle
            }
            visited[idx] = true;
        }
        let Some(sec) = sector_slice(bytes, difat, sector_size) else {
            break;
        };
        for k in 0..entries_per_difat {
            let v =
                u32::from_le_bytes([sec[k * 4], sec[k * 4 + 1], sec[k * 4 + 2], sec[k * 4 + 3]]);
            if is_real_sector(v) {
                list.push(v);
            }
        }
        // Next DIFAT sector is the final pointer in this sector.
        let last = entries_per_difat * 4;
        difat = u32::from_le_bytes([sec[last], sec[last + 1], sec[last + 2], sec[last + 3]]);
        steps += 1;
    }

    // Keep at most the declared number of FAT sectors when that is the tighter
    // bound; a corrupt DIFAT can over-list, and reading extra junk sectors would
    // only pollute the FAT. `num_fat_sectors == 0` means "trust what we found".
    if num_fat_sectors > 0 && list.len() > num_fat_sectors {
        list.truncate(num_fat_sectors);
    }
    list
}

/// Read the FAT: concatenate every FAT sector's `u32` next-pointers in order.
/// Out-of-range FAT-sector locations are skipped (length-checked).
fn read_fat(
    bytes: &[u8],
    sector_size: usize,
    fat_sector_list: &[u32],
    total_sectors: usize,
) -> Vec<u32> {
    let mut fat = Vec::with_capacity(fat_sector_list.len() * (sector_size / 4));
    for &fs in fat_sector_list {
        // A FAT sector index past the file is corruption ⇒ skip it.
        if (fs as usize) > total_sectors {
            continue;
        }
        if let Some(sec) = sector_slice(bytes, fs, sector_size) {
            fat.extend(le_u32s(sec));
        }
    }
    fat
}

/// Walk a regular-FAT sector chain from `start`, returning the concatenated
/// sector bytes. Bounded by the FAT length and guarded against cycles. If
/// `byte_limit` is set, stop once that many bytes have been gathered (avoids
/// walking a whole huge chain when the caller only needs a prefix).
fn read_fat_chain(
    bytes: &[u8],
    sector_size: usize,
    fat: &[u32],
    start: u32,
    byte_limit: Option<usize>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let cap = fat.len().saturating_add(1);
    let mut visited = vec![false; cap];
    let mut sector = start;
    let mut steps = 0usize;
    while is_real_sector(sector) && steps < cap {
        let idx = sector as usize;
        if idx >= fat.len() || visited[idx] {
            break; // out-of-range pointer or a cycle ⇒ stop
        }
        visited[idx] = true;
        match sector_slice(bytes, sector, sector_size) {
            Some(sec) => out.extend_from_slice(sec),
            None => break, // sector outside the file ⇒ stop cleanly
        }
        if let Some(limit) = byte_limit {
            if out.len() >= limit {
                break;
            }
        }
        sector = fat[idx];
        steps += 1;
    }
    out
}

/// Parse the directory sector bytes into 128-byte entries, returning the entry
/// list and the root storage entry (object type 5), if present.
fn parse_directory(dir_bytes: &[u8]) -> (Vec<DirEntry>, Option<DirEntry>) {
    let mut entries = Vec::new();
    let mut root = None;
    let mut off = 0;
    while off + DIR_ENTRY_SIZE <= dir_bytes.len() {
        let raw = &dir_bytes[off..off + DIR_ENTRY_SIZE];
        let entry = parse_dir_entry(raw);
        if entry.obj_type == OBJ_ROOT {
            root = Some(entry.clone());
        }
        entries.push(entry);
        off += DIR_ENTRY_SIZE;
    }
    (entries, root)
}

/// Parse one 128-byte directory entry (MS-CFB §2.6.1). `raw` is exactly
/// [`DIR_ENTRY_SIZE`] bytes (the caller guarantees the length).
fn parse_dir_entry(raw: &[u8]) -> DirEntry {
    // Name: up to 64 bytes of UTF-16LE; `name_len` (bytes 64..66) counts bytes
    // including the NUL terminator. Clamp defensively to the field width.
    let name_len = u16::from_le_bytes([raw[64], raw[65]]) as usize;
    let name = decode_entry_name(&raw[0..DIR_NAME_BYTES], name_len);

    let obj_type = raw[66];
    let read32 = |o: usize| u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);
    let left = read32(68);
    let right = read32(72);
    let child = read32(76);
    let start_sector = read32(116);
    // Size is a u64 at 120; in the v3 (512-byte) layout only the low 32 bits are
    // valid, so mask the high word off to stay correct on both versions.
    let size = (read32(120) as u64) | ((read32(124) as u64) << 32);
    let size = size & 0xFFFF_FFFF; // v3-safe: ignore the high 32 bits

    DirEntry {
        name,
        obj_type,
        left,
        right,
        child,
        start_sector,
        size,
    }
}

/// Decode a directory-entry name from `field` (the 64-byte UTF-16LE name field)
/// using `name_len` (byte count incl. terminator). Returns the trimmed name.
fn decode_entry_name(field: &[u8], name_len: usize) -> String {
    if name_len < 2 {
        return String::new();
    }
    // `name_len` includes the NUL terminator; clamp to the 64-byte field.
    let usable = name_len.min(DIR_NAME_BYTES);
    let units = (usable / 2).saturating_sub(1); // drop the terminating NUL
    let mut name = String::with_capacity(units);
    for k in 0..units {
        let lo = field[k * 2];
        let hi = field[k * 2 + 1];
        let cu = u16::from_le_bytes([lo, hi]);
        if cu == 0 {
            break;
        }
        // Lone surrogates can't appear in well-formed names; map any stray
        // code unit through `from_u32`, dropping the rare invalid one.
        if let Some(ch) = char::from_u32(cu as u32) {
            name.push(ch);
        }
    }
    name
}

/// Flatten the directory red-black trees into a flat list of streams, each
/// carrying the storage path that reaches it. Traversal is bounded by the entry
/// count and cycle-guarded, so a corrupt tree cannot loop or over-recurse.
fn flatten_streams(entries: &[DirEntry], root_child: u32) -> Vec<Stream> {
    let mut streams = Vec::new();
    if entries.is_empty() {
        return streams;
    }
    // Iterative worklist of `(entry_id, parent_path)` to avoid deep recursion on
    // adversarial inputs. A visited set caps total work at the entry count.
    let mut visited = vec![false; entries.len()];
    let mut work: Vec<(u32, Vec<String>)> = Vec::new();
    collect_siblings(
        entries,
        root_child,
        &Vec::new(),
        &mut visited,
        &mut work,
        &mut streams,
    );

    // `collect_siblings` pushes storages onto `work`; descend into each.
    while let Some((storage_id, parent_path)) = work.pop() {
        let Some(storage) = entries.get(storage_id as usize) else {
            continue;
        };
        let mut path = parent_path;
        path.push(storage.name.clone());
        collect_siblings(
            entries,
            storage.child,
            &path,
            &mut visited,
            &mut work,
            &mut streams,
        );
    }
    streams
}

/// Walk one red-black sibling tree (left/right links) rooted at `node`, adding
/// streams to `streams` and queueing nested storages onto `work`. Cycle-guarded
/// via `visited`; bounded by the entry count.
fn collect_siblings(
    entries: &[DirEntry],
    node: u32,
    parent_path: &[String],
    visited: &mut [bool],
    work: &mut Vec<(u32, Vec<String>)>,
    streams: &mut Vec<Stream>,
) {
    // Explicit stack of sibling-subtree roots (in-order traversal of the tree).
    let mut stack = vec![node];
    while let Some(id) = stack.pop() {
        if !is_real_sector(id) || id == NOSTREAM {
            continue;
        }
        let idx = id as usize;
        if idx >= entries.len() || visited[idx] {
            continue; // out-of-range link or already seen ⇒ skip (cycle guard)
        }
        visited[idx] = true;
        let e = &entries[idx];
        match e.obj_type {
            OBJ_STREAM => {
                let mut path = parent_path.to_vec();
                path.push(e.name.clone());
                streams.push(Stream {
                    name: e.name.clone(),
                    path,
                    start_sector: e.start_sector,
                    size: e.size,
                });
            }
            OBJ_STORAGE => work.push((id, parent_path.to_vec())),
            _ => {}
        }
        // Both subtrees of this node are still siblings in the same storage.
        stack.push(e.left);
        stack.push(e.right);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built minimal v3 (512-byte sector) Compound File:
    ///   sector −1 : header
    ///   sector  0 : FAT
    ///   sector  1 : directory (Root + Big + Mini entries)
    ///   sector  2 : mini-FAT
    ///   sector  3 : mini-stream container (root's FAT chain)
    ///   sector  4 : the "Big" stream payload (≥ cutoff ⇒ regular FAT)
    /// The "Mini" stream (< cutoff) lives in mini-sector 0 of the mini-stream.
    struct Builder {
        sectors: Vec<[u8; 512]>,
    }

    impl Builder {
        fn new(n: usize) -> Builder {
            Builder {
                sectors: vec![[0u8; 512]; n],
            }
        }

        fn put_u16(buf: &mut [u8], off: usize, v: u16) {
            buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
        }

        fn put_u32(buf: &mut [u8], off: usize, v: u32) {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }

        /// Write a directory entry into `dir` at slot `i`.
        #[allow(clippy::too_many_arguments)]
        fn dir_entry(
            dir: &mut [u8; 512],
            i: usize,
            name: &str,
            obj_type: u8,
            left: u32,
            right: u32,
            child: u32,
            start: u32,
            size: u64,
        ) {
            let base = i * 128;
            // Name as UTF-16LE + NUL terminator.
            let mut nlen = 0usize;
            for (k, ch) in name.encode_utf16().enumerate() {
                Builder::put_u16(dir, base + k * 2, ch);
                nlen = (k + 1) * 2;
            }
            nlen += 2; // include the terminating NUL (already zero)
            Builder::put_u16(dir, base + 64, nlen as u16);
            dir[base + 66] = obj_type;
            dir[base + 67] = 1; // colour = black (irrelevant to the reader)
            Builder::put_u32(dir, base + 68, left);
            Builder::put_u32(dir, base + 72, right);
            Builder::put_u32(dir, base + 76, child);
            Builder::put_u32(dir, base + 116, start);
            Builder::put_u32(dir, base + 120, (size & 0xFFFF_FFFF) as u32);
            Builder::put_u32(dir, base + 124, (size >> 32) as u32);
        }

        fn build(self) -> Vec<u8> {
            let mut out = vec![0u8; 512 + self.sectors.len() * 512];
            for (i, sec) in self.sectors.iter().enumerate() {
                out[512 + i * 512..512 + (i + 1) * 512].copy_from_slice(sec);
            }
            out
        }
    }

    /// Construct the full minimal CFB and return its bytes plus the expected
    /// payloads, so the same fixture drives several assertions.
    fn minimal_cfb() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        // The big stream is 600 bytes (> 512 mini-cutoff would not apply; cutoff
        // is 4096, so "big" must be ≥ 4096 to use the regular FAT). Use 4608
        // bytes so it occupies 9 regular sectors and exceeds the cutoff.
        let big: Vec<u8> = (0..4608u32).map(|i| (i % 251) as u8).collect();
        let mini: Vec<u8> = (0..40u32).map(|i| (i * 7 % 255) as u8).collect();

        // Layout:
        //   sector 0      : FAT
        //   sector 1      : directory
        //   sector 2      : mini-FAT
        //   sector 3      : mini-stream container (one 512-sector ⇒ 8 mini-sectors)
        //   sectors 4..=12: big stream (9 sectors × 512 = 4608)
        let big_first = 4u32;
        let big_sectors = 9u32; // 4608 / 512
        let total = (4 + big_sectors) as usize; // 13 sectors
        let mut b = Builder::new(total);

        // ---- FAT (sector 0) ----
        {
            let fat = &mut b.sectors[0];
            // Sector 0 (FAT itself) → FATSECT.
            Builder::put_u32(fat, 0, FATSECT);
            // Sector 1 (directory) → ENDOFCHAIN.
            Builder::put_u32(fat, 4, ENDOFCHAIN);
            // Sector 2 (mini-FAT) → ENDOFCHAIN.
            Builder::put_u32(fat, 8, ENDOFCHAIN);
            // Sector 3 (mini-stream) → ENDOFCHAIN.
            Builder::put_u32(fat, 12, ENDOFCHAIN);
            // Big stream chain: 4→5→6→…→12→ENDOFCHAIN.
            for k in 0..big_sectors {
                let sec = big_first + k;
                let next = if k + 1 < big_sectors {
                    sec + 1
                } else {
                    ENDOFCHAIN
                };
                Builder::put_u32(fat, sec as usize * 4, next);
            }
            // Remaining FAT slots default to 0; mark them free to be clean.
            for sec in total..(512 / 4) {
                Builder::put_u32(fat, sec * 4, FREESECT);
            }
        }

        // ---- directory (sector 1) ----
        {
            let dir = &mut b.sectors[1];
            // Slot 0: Root storage. child → slot 1 (the stream tree root).
            // start = mini-stream sector (3), size = mini-stream length (512).
            Builder::dir_entry(
                dir,
                0,
                "Root Entry",
                OBJ_ROOT,
                NOSTREAM,
                NOSTREAM,
                1,
                3,
                512,
            );
            // Slot 1: "BigStream" (size 4608 ≥ cutoff ⇒ regular FAT, start 4).
            // right → slot 2 so both streams are siblings in the tree.
            Builder::dir_entry(
                dir,
                1,
                "BigStream",
                OBJ_STREAM,
                NOSTREAM,
                2,
                NOSTREAM,
                big_first,
                big.len() as u64,
            );
            // Slot 2: "MiniStream" (size 40 < cutoff ⇒ mini-FAT, start 0).
            Builder::dir_entry(
                dir,
                2,
                "MiniStream",
                OBJ_STREAM,
                NOSTREAM,
                NOSTREAM,
                NOSTREAM,
                0,
                mini.len() as u64,
            );
        }

        // ---- mini-FAT (sector 2) ----
        {
            let mf = &mut b.sectors[2];
            // The mini stream occupies one mini-sector (40 ≤ 64) ⇒ chain ends.
            Builder::put_u32(mf, 0, ENDOFCHAIN);
            // Mark the rest free.
            for k in 1..(512 / 4) {
                Builder::put_u32(mf, k * 4, FREESECT);
            }
        }

        // ---- mini-stream container (sector 3) ----
        {
            let ms = &mut b.sectors[3];
            ms[0..mini.len()].copy_from_slice(&mini);
        }

        // ---- big stream payload (sectors 4..=12) ----
        for k in 0..big_sectors as usize {
            let sec = &mut b.sectors[big_first as usize + k];
            sec.copy_from_slice(&big[k * 512..(k + 1) * 512]);
        }

        // ---- header ----
        let mut bytes = b.build();
        bytes[0..8].copy_from_slice(&SIGNATURE);
        // CLSID (16 bytes) zero. Minor (0x003E) / major (0x0003) version.
        Builder::put_u16(&mut bytes, 24, 0x0003);
        Builder::put_u16(&mut bytes, 26, 0x003E);
        // Byte order FE FF, sector shift 0x0009 (512), mini shift 0x0006 (64).
        Builder::put_u16(&mut bytes, 28, 0xFFFE);
        Builder::put_u16(&mut bytes, 30, 0x0009);
        Builder::put_u16(&mut bytes, 32, 0x0006);
        // num FAT sectors = 1, dir start = 1, mini cutoff = 4096.
        Builder::put_u32(&mut bytes, 44, 1);
        Builder::put_u32(&mut bytes, 48, 1);
        Builder::put_u32(&mut bytes, 56, 4096);
        // mini-FAT start = 2, num mini-FAT = 1.
        Builder::put_u32(&mut bytes, 60, 2);
        Builder::put_u32(&mut bytes, 64, 1);
        // DIFAT start = ENDOFCHAIN, num DIFAT = 0.
        Builder::put_u32(&mut bytes, 68, ENDOFCHAIN);
        Builder::put_u32(&mut bytes, 72, 0);
        // Inline DIFAT[0] = 0 (FAT lives in sector 0); the rest FREESECT.
        Builder::put_u32(&mut bytes, HEADER_DIFAT_OFFSET, 0);
        for k in 1..HEADER_DIFAT_COUNT {
            Builder::put_u32(&mut bytes, HEADER_DIFAT_OFFSET + k * 4, FREESECT);
        }

        (bytes, big, mini)
    }

    #[test]
    fn opens_minimal_container() {
        let (bytes, _, _) = minimal_cfb();
        assert!(Cfb::open(&bytes).is_some(), "valid CFB must parse");
    }

    #[test]
    fn reads_big_stream_via_regular_fat() {
        let (bytes, big, _) = minimal_cfb();
        let cfb = Cfb::open(&bytes).expect("open");
        let got = cfb.read_stream("BigStream").expect("BigStream present");
        assert_eq!(got, big, "big-stream bytes must match exactly");
    }

    #[test]
    fn reads_mini_stream_via_mini_fat() {
        let (bytes, _, mini) = minimal_cfb();
        let cfb = Cfb::open(&bytes).expect("open");
        let got = cfb.read_stream("MiniStream").expect("MiniStream present");
        assert_eq!(got, mini, "mini-stream bytes must match exactly");
    }

    #[test]
    fn lists_both_stream_names() {
        let (bytes, _, _) = minimal_cfb();
        let cfb = Cfb::open(&bytes).expect("open");
        let mut names = cfb.stream_names();
        names.sort();
        assert_eq!(names, vec!["BigStream".to_string(), "MiniStream".to_string()]);
    }

    #[test]
    fn reads_stream_by_top_level_path() {
        let (bytes, big, _) = minimal_cfb();
        let cfb = Cfb::open(&bytes).expect("open");
        let got = cfb.read_stream_at_path(&["BigStream"]);
        assert_eq!(got.as_deref(), Some(big.as_slice()), "path lookup must match");
    }

    #[test]
    fn unknown_stream_is_none() {
        let (bytes, _, _) = minimal_cfb();
        let cfb = Cfb::open(&bytes).expect("open");
        assert!(cfb.read_stream("Nope").is_none(), "missing stream ⇒ None");
        assert!(cfb.read_stream_at_path(&["A", "B"]).is_none(), "missing path ⇒ None");
    }

    #[test]
    fn garbage_buffer_is_none() {
        assert!(Cfb::open(b"not a compound file at all").is_none());
        assert!(Cfb::open(&[]).is_none(), "empty input ⇒ None");
        // Right signature, but far too short to hold a header.
        let mut tiny = SIGNATURE.to_vec();
        tiny.extend_from_slice(&[0u8; 8]);
        assert!(Cfb::open(&tiny).is_none(), "signature alone ⇒ None");
    }

    #[test]
    fn truncated_buffer_is_none_or_safe() {
        let (bytes, _, _) = minimal_cfb();
        // Truncate to just the header: open may fail, but must never panic.
        let head = &bytes[..512];
        let _ = Cfb::open(head);
        // Truncate mid-file: reads must stay bounded, never panic.
        let half = &bytes[..bytes.len() / 2];
        if let Some(cfb) = Cfb::open(half) {
            let _ = cfb.read_stream("BigStream");
            let _ = cfb.read_stream("MiniStream");
            let _ = cfb.stream_names();
        }
    }

    #[test]
    fn wrong_byte_order_mark_is_none() {
        let (mut bytes, _, _) = minimal_cfb();
        // Corrupt the byte-order mark (should be FE FF).
        bytes[28] = 0x00;
        bytes[29] = 0x00;
        assert!(Cfb::open(&bytes).is_none(), "bad BOM ⇒ None");
    }

    #[test]
    fn invalid_sector_shift_is_none() {
        let (mut bytes, _, _) = minimal_cfb();
        bytes[30] = 0x07; // 128-byte sectors: not a legal CFB shift
        bytes[31] = 0x00;
        assert!(Cfb::open(&bytes).is_none(), "illegal sector shift ⇒ None");
    }

    #[test]
    fn cyclic_fat_chain_terminates() {
        let (mut bytes, _, _) = minimal_cfb();
        // Point the big-stream's first sector (4) back at itself in the FAT.
        // FAT lives in sector 0 ⇒ file offset 512 + 0; entry 4 at +16.
        let fat_base = 512; // sector 0 starts right after the header
        bytes[fat_base + 16..fat_base + 20].copy_from_slice(&4u32.to_le_bytes());
        let cfb = Cfb::open(&bytes).expect("still opens");
        // Must return (a bounded amount) without looping forever or panicking.
        let got = cfb.read_stream("BigStream");
        assert!(got.is_some(), "cyclic chain still yields bytes, bounded");
    }
}
