//! CSV → unified editable [`Document`](crate::model::Document) importer.
//!
//! A correct, dependency-free CSV reader (RFC 4180) that lowers a delimited file
//! into a single typed [`SheetBlock`] — the natural editable shape for tabular
//! data, the same typed-cell model the XLSX/ODS reconstruction produces:
//!
//! - Quoted fields (`"…"`) may contain the delimiter, line breaks, and escaped
//!   quotes (`""` ⇒ a literal `"`).
//! - Record terminators `\n`, `\r\n` and a bare `\r` are all accepted.
//! - The delimiter is **auto-detected** among `,`, `;`, `\t` and `|` by scoring
//!   which one yields the most consistent column count across the first records.
//! - The first row is treated as a header (bold + lightly shaded). Ragged rows
//!   are padded/truncated to the header width so the grid stays rectangular.
//! - Each body cell's **type is inferred** from its text — number, boolean, or
//!   date — and stored as a typed [`CellValue`], conservatively falling back to
//!   text for anything ambiguous (leading-zero codes, phone numbers, …) so a ZIP
//!   like `01234` is never truncated to an integer. See `infer_cell`.
//!
//! A leading UTF-8 BOM is stripped. Empty input (no fields) yields `None`.

use crate::convert::style::Generic;
use crate::model::{
    Block, BlockKind, CellValue, CharStyle, Document, Page, Section, Sheet, SheetBlock, SheetCell,
    SheetRow,
};

/// Body run size for cell text (points).
const CELL_PT: f64 = 11.0;

/// Light grey header-row fill, components `0.0..=1.0`.
const HEADER_FILL: [f64; 3] = [0.93, 0.93, 0.93];

/// ISO date `number_format` stamped on cells inferred as a date/datetime, so a
/// downstream consumer can re-render the serial value in the original shape.
const DATE_FORMAT: &str = "yyyy-mm-dd";
/// ISO datetime `number_format` for cells inferred as a date *with* a time part.
const DATETIME_FORMAT: &str = "yyyy-mm-dd hh:mm:ss";

/// Candidate field delimiters, tried in order for auto-detection.
const DELIMITERS: [u8; 4] = [b',', b';', b'\t', b'|'];

/// CSV bytes → [`Document`]: one section / one page holding a single typed
/// [`SheetBlock`]. Returns `None` if the input has no parseable fields.
pub fn csv_to_model(bytes: &[u8]) -> Option<Document> {
    let text = decode(bytes);
    let delim = detect_delimiter(&text);
    let records = parse_records(&text, delim);
    // Reject input with no real content: no records, or every record is just
    // empty field(s) (e.g. a few blank lines).
    let has_content = records.iter().any(|r| r.iter().any(|f| !f.is_empty()));
    if !has_content {
        return None;
    }

    let ncols = records.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let mut rows = Vec::with_capacity(records.len());
    for (idx, rec) in records.iter().enumerate() {
        rows.push(make_row(rec, ncols, idx == 0));
    }

    let block = Block {
        kind: BlockKind::Sheet(SheetBlock {
            sheets: vec![Sheet {
                name: String::new(),
                rows,
                merges: Vec::new(),
                col_widths: Vec::new(),
            }],
        }),
        ..Block::default()
    };

    Some(Document {
        sections: vec![Section {
            geometry: crate::model::PageGeometry::default(),
            header: None,
            footer: None,
            pages: vec![Page {
                blocks: vec![block],
                absolute: false,
            }],
        }],
        ..Document::default()
    })
}

/// Decode the byte buffer as UTF-8 (lossy), stripping a leading BOM.
fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// Score each candidate delimiter on the first records and pick the best: prefer
/// the delimiter that yields the most columns while keeping the column count
/// consistent across rows (penalise ragged splits). Falls back to comma.
fn detect_delimiter(text: &str) -> u8 {
    let mut best = b',';
    let mut best_score = -1i64;
    for &d in &DELIMITERS {
        let recs = parse_records(text, d);
        let sample: Vec<usize> = recs.iter().take(10).map(Vec::len).collect();
        if sample.is_empty() {
            continue;
        }
        let max = *sample.iter().max().unwrap_or(&1);
        if max <= 1 {
            continue; // this delimiter doesn't split anything
        }
        // Reward consistency: rows whose width equals the modal width.
        let consistent = sample.iter().filter(|&&n| n == max).count() as i64;
        let score = consistent * 100 + max as i64;
        if score > best_score {
            best_score = score;
            best = d;
        }
    }
    best
}

/// Parse `text` into records (each a `Vec` of field strings) per RFC 4180 using
/// `delim` as the field separator. Quoted fields preserve delimiters/newlines and
/// `""` as a literal quote. A trailing newline does not produce an empty record.
fn parse_records(text: &str, delim: u8) -> Vec<Vec<String>> {
    let bytes = text.as_bytes();
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut i = 0;
    let mut field_started = false;

    let push_field = |field: &mut String, record: &mut Vec<String>| {
        record.push(std::mem::take(field));
    };

    while i < bytes.len() {
        let c = bytes[i];
        if in_quotes {
            if c == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    field.push('"'); // escaped quote
                    i += 2;
                    continue;
                }
                in_quotes = false;
                i += 1;
                continue;
            }
            // UTF-8 continuation bytes are appended as-is via the char below;
            // copy the raw byte's char by decoding lazily.
            push_byte(&mut field, bytes, &mut i);
            continue;
        }

        match c {
            b'"' => {
                in_quotes = true;
                field_started = true;
                i += 1;
            }
            d if d == delim => {
                push_field(&mut field, &mut record);
                field_started = false;
                i += 1;
            }
            b'\r' => {
                // `\r` or `\r\n` ends the record.
                push_field(&mut field, &mut record);
                records.push(std::mem::take(&mut record));
                field_started = false;
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            }
            b'\n' => {
                push_field(&mut field, &mut record);
                records.push(std::mem::take(&mut record));
                field_started = false;
                i += 1;
            }
            _ => {
                field_started = true;
                push_byte(&mut field, bytes, &mut i);
            }
        }
    }

    // Flush a trailing record (no terminating newline). Avoid emitting a phantom
    // empty record for a file that ended exactly on a newline.
    if field_started || !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

/// Append the (possibly multi-byte UTF-8) character starting at `bytes[*i]` to
/// `field`, advancing `*i` past it.
fn push_byte(field: &mut String, bytes: &[u8], i: &mut usize) {
    let start = *i;
    let len = utf8_len(bytes[start]);
    let end = (start + len).min(bytes.len());
    match std::str::from_utf8(&bytes[start..end]) {
        Ok(s) => field.push_str(s),
        Err(_) => field.push(char::REPLACEMENT_CHARACTER),
    }
    *i = end;
}

/// Width (in bytes) of a UTF-8 sequence given its leading byte.
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1, // stray continuation byte → consume one
    }
}

/// Build a [`SheetRow`] from a record, padded/truncated to `ncols`. Header cells
/// carry the verbatim label (bold + light fill); body cells are type-inferred.
fn make_row(fields: &[String], ncols: usize, header: bool) -> SheetRow {
    let mut cells = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let text = fields.get(c).map(String::as_str).unwrap_or("");
        cells.push(make_cell(text, header));
    }
    SheetRow {
        cells,
        height: None,
    }
}

/// One typed [`SheetCell`]. The header row keeps every field as verbatim
/// [`CellValue::Text`] (header labels are never coerced to a type) with a bold
/// run and light fill; body cells run through [`infer_cell`].
fn make_cell(text: &str, header: bool) -> SheetCell {
    let style = CharStyle {
        generic: Generic::Sans,
        size_pt: CELL_PT,
        bold: header,
        ..CharStyle::default()
    };
    let (value, number_format) = if header {
        // A header label is metadata, not data: keep it as text even if it
        // happens to look numeric (`"2024"` as a column name stays text).
        let v = if text.is_empty() {
            CellValue::Empty
        } else {
            CellValue::Text(text.to_string())
        };
        (v, None)
    } else {
        infer_cell(text)
    };
    SheetCell {
        value,
        number_format,
        fill: header.then_some(HEADER_FILL),
        style,
        ..SheetCell::default()
    }
}

/// Infer a body cell's typed [`CellValue`] from its raw text, returning the value
/// plus an optional spreadsheet `number_format` (set for dates so the serial
/// number can be re-rendered in its original ISO shape).
///
/// The order is deliberate — boolean and date are checked *before* number so that
/// `true`/`2026-06-25` are not mis-typed — and every rule is **conservative**:
/// any token that is not confidently one of the recognised types stays
/// [`CellValue::Text`]. In particular [`looks_like_number`] rejects leading-zero
/// runs (`01234` ZIP), over-long digit strings, and `+`-prefixed / separator-laden
/// tokens (`+1-555-0100` phone), matching spreadsheet-import convention.
fn infer_cell(text: &str) -> (CellValue, Option<String>) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return (CellValue::Empty, None);
    }
    if let Some(b) = parse_bool(trimmed) {
        return (CellValue::Bool(b), None);
    }
    if let Some((serial, has_time)) = parse_iso_date(trimmed) {
        let fmt = if has_time {
            DATETIME_FORMAT
        } else {
            DATE_FORMAT
        };
        return (CellValue::Number(serial), Some(fmt.to_string()));
    }
    if let Some(n) = parse_number(trimmed) {
        return (CellValue::Number(n), None);
    }
    (CellValue::Text(trimmed.to_string()), None)
}

/// Recognise the spreadsheet boolean literals `true`/`false` (case-insensitive,
/// covering `TRUE`/`FALSE`). Nothing else is a boolean.
fn parse_bool(s: &str) -> Option<bool> {
    if s.eq_ignore_ascii_case("true") {
        Some(true)
    } else if s.eq_ignore_ascii_case("false") {
        Some(false)
    } else {
        None
    }
}

/// Parse a numeric token to `f64`, but **only** when it is unambiguously numeric.
///
/// Conservative guards (anything failing one of these stays text):
/// - The integer part must not have a redundant leading zero (`01234` ZIP, `007`)
///   — a lone `0`, or `0.x` / `0e…`, is fine.
/// - Total digit count is capped (a 16+ digit run is treated as an identifier,
///   e.g. a card or account number, not an integer that would lose precision).
/// - Only a single leading sign, an optional fraction, and an optional decimal
///   exponent (`1e5`, `-3.14E-2`) are allowed — no thousands separators, spaces,
///   or other punctuation (so `+1-555-0100` and `1,234` never parse here; the
///   XLSX/ODS path likewise types only bare `f64`-parseable tokens).
fn parse_number(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i += 1;
    }

    // Integer digits.
    let int_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let int_len = i - int_start;

    // Optional fraction.
    let mut frac_len = 0;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        frac_len = i - frac_start;
    }

    // Need at least one digit somewhere in the mantissa.
    if int_len == 0 && frac_len == 0 {
        return None;
    }
    // Reject a redundant leading zero on a multi-digit integer part (ZIP/codes):
    // `0` and `0.5` are fine, `01234` and `00` are not.
    if int_len >= 2 && bytes[int_start] == b'0' {
        return None;
    }
    // Cap the digit budget: a very long all-digit run is an identifier, not a
    // number we should coerce (loses precision past 2^53).
    if int_len + frac_len > 15 {
        return None;
    }

    // Optional decimal exponent.
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return None; // `1e` with no exponent digits
        }
    }

    // The whole token must have been consumed — no trailing units/punctuation.
    if i != bytes.len() {
        return None;
    }
    s.parse::<f64>().ok()
}

/// Parse an ISO-8601 date / datetime to an Excel-style serial day number
/// (days since 1899-12-30, the spreadsheet epoch), returning `(serial, has_time)`.
///
/// Accepts `YYYY-MM-DD` optionally followed by `THH:MM[:SS]` (the `T` may be a
/// space). Calendar fields are range-checked (month `1..=12`, day valid for the
/// month including leap years, hour `0..=23`, minute/second `0..=59`). Anything
/// else — locale formats like `MM/DD/YYYY`, partial dates, out-of-range fields —
/// returns `None` and the cell stays text, so plausible-but-ambiguous strings are
/// never silently reinterpreted.
fn parse_iso_date(s: &str) -> Option<(f64, bool)> {
    let bytes = s.as_bytes();
    // `YYYY-MM-DD` is exactly 10 ASCII bytes.
    if bytes.len() < 10 {
        return None;
    }
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = ascii_digits(&bytes[0..4])?;
    let month: i64 = ascii_digits(&bytes[5..7])?;
    let day: i64 = ascii_digits(&bytes[8..10])?;
    if !(1..=12).contains(&month) {
        return None;
    }
    let dim = days_in_month(year, month as u32)?;
    if day < 1 || day > dim as i64 {
        return None;
    }

    // Optional time part: `T`/space then `HH:MM[:SS]` and nothing else.
    let mut frac_day = 0.0;
    let mut has_time = false;
    if bytes.len() > 10 {
        let sep = bytes[10];
        if sep != b'T' && sep != b' ' {
            return None;
        }
        let time = &s[11..];
        let (secs_of_day, ok) = parse_iso_time(time)?;
        if !ok {
            return None;
        }
        frac_day = secs_of_day / 86_400.0;
        has_time = true;
    }

    let serial = days_from_civil(year, month, day)? as f64 + frac_day;
    Some((serial, has_time))
}

/// Parse `HH:MM[:SS]` (the only forms accepted) into `(seconds_of_day, true)`.
/// Returns `None` on any malformed/out-of-range field.
fn parse_iso_time(t: &str) -> Option<(f64, bool)> {
    let b = t.as_bytes();
    // `HH:MM` = 5 bytes, `HH:MM:SS` = 8 bytes.
    if b.len() != 5 && b.len() != 8 {
        return None;
    }
    if b[2] != b':' {
        return None;
    }
    let hh: i64 = ascii_digits(&b[0..2])?;
    let mm: i64 = ascii_digits(&b[3..5])?;
    if !(0..=23).contains(&hh) || !(0..=59).contains(&mm) {
        return None;
    }
    let mut ss = 0i64;
    if b.len() == 8 {
        if b[5] != b':' {
            return None;
        }
        ss = ascii_digits(&b[6..8])?;
        if !(0..=59).contains(&ss) {
            return None;
        }
    }
    Some(((hh * 3600 + mm * 60 + ss) as f64, true))
}

/// Parse a slice of ASCII bytes that must all be digits into an integer.
/// Returns `None` if any byte is non-digit (so `"1a"` / `" 2"` are rejected).
fn ascii_digits(b: &[u8]) -> Option<i64> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut n = 0i64;
    for &d in b {
        n = n * 10 + (d - b'0') as i64;
    }
    Some(n)
}

/// Number of days in `month` (1-based) of `year`, honouring leap years.
fn days_in_month(year: i64, month: u32) -> Option<u32> {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    Some(match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    })
}

/// Days from the spreadsheet epoch (1899-12-30) to `y-m-d`, via the proleptic
/// Gregorian civil-day algorithm (Howard Hinnant). The 1899-12-30 base — rather
/// than 1900-01-01 — reproduces the Lotus/Excel serial convention (the spurious
/// 1900 leap-year offset is already baked into the constant).
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    // Days from 1970-01-01 (Unix epoch) for the civil date.
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days_since_unix = era * 146_097 + doe - 719_468;
    // Unix epoch 1970-01-01 is serial 25569 from the 1899-12-30 spreadsheet epoch.
    Some(days_since_unix + 25_569)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The single sheet on the page.
    fn sheet(doc: &Document) -> &Sheet {
        match &doc.sections[0].pages[0].blocks[0].kind {
            BlockKind::Sheet(sb) => &sb.sheets[0],
            other => panic!("expected sheet, got {other:?}"),
        }
    }

    /// The `[r][c]` cell of the single sheet.
    fn cell(doc: &Document, r: usize, c: usize) -> &SheetCell {
        &sheet(doc).rows[r].cells[c]
    }

    /// Display string of a cell's typed value (text/number/bool verbatim, empty
    /// ⇒ `""`) — the equivalent of the old paragraph-run concatenation.
    fn cell_text(cell: &SheetCell) -> String {
        match &cell.value {
            CellValue::Empty => String::new(),
            CellValue::Text(s) => s.clone(),
            CellValue::Number(n) => format!("{n}"),
            CellValue::Bool(b) => b.to_string(),
        }
    }

    #[test]
    fn simple_comma_table_with_header() {
        let doc = csv_to_model(b"Name,Age\nAlice,30\nBob,25").expect("csv");
        let t = sheet(&doc);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0].cells.len(), 2);
        assert_eq!(cell_text(&t.rows[0].cells[0]), "Name");
        // Body number is type-inferred.
        assert_eq!(t.rows[1].cells[1].value, CellValue::Number(30.0));
        assert_eq!(cell_text(&t.rows[2].cells[0]), "Bob");
        // Header is bold + filled, and kept as text even for a numeric-looking label.
        assert!(t.rows[0].cells[0].fill.is_some());
        assert!(t.rows[0].cells[0].style.bold);
        assert_eq!(t.rows[0].cells[1].value, CellValue::Text("Age".into()));
        // Body is not bold / not filled.
        assert!(!t.rows[1].cells[0].style.bold);
        assert!(t.rows[1].cells[0].fill.is_none());
    }

    #[test]
    fn quoted_field_with_comma_and_newline() {
        // A1 = `Hello, World`, A2 holds an embedded newline; quotes preserved.
        let csv = "greeting,note\n\"Hello, World\",\"line1\nline2\"\n";
        let doc = csv_to_model(csv.as_bytes()).expect("csv");
        let t = sheet(&doc);
        assert_eq!(t.rows.len(), 2, "embedded newline stays in the field");
        assert_eq!(cell_text(&t.rows[1].cells[0]), "Hello, World");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "line1\nline2");
    }

    #[test]
    fn escaped_quote_doubled() {
        // RFC 4180: `""` inside a quoted field is one literal `"`.
        let doc = csv_to_model(b"a,b\n\"say \"\"hi\"\"\",plain").expect("csv");
        let t = sheet(&doc);
        assert_eq!(cell_text(&t.rows[1].cells[0]), "say \"hi\"");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "plain");
    }

    #[test]
    fn semicolon_delimiter_autodetected() {
        let doc = csv_to_model(b"name;city;age\nAna;Paris;40\nLi;Rome;22").expect("csv");
        let t = sheet(&doc);
        assert_eq!(t.rows[0].cells.len(), 3, "split on ';'");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "Paris");
    }

    #[test]
    fn tab_delimiter_autodetected() {
        let doc = csv_to_model(b"a\tb\tc\n1\t2\t3").expect("csv");
        let t = sheet(&doc);
        assert_eq!(t.rows[0].cells.len(), 3, "split on tab");
        assert_eq!(t.rows[1].cells[2].value, CellValue::Number(3.0));
    }

    #[test]
    fn comma_preferred_when_both_present() {
        // Each line has 3 commas and 1 semicolon → comma wins (more, consistent).
        let doc = csv_to_model(b"a,b;x,c,d\n1,2;y,3,4").expect("csv");
        let t = sheet(&doc);
        assert_eq!(t.rows[0].cells.len(), 4);
        // The semicolon stays inside the cell that contained it.
        assert_eq!(cell_text(&t.rows[0].cells[1]), "b;x");
        // `2;y` is not numeric → stays text.
        assert_eq!(t.rows[1].cells[1].value, CellValue::Text("2;y".into()));
    }

    #[test]
    fn ragged_rows_padded_to_header_width() {
        let doc = csv_to_model(b"a,b,c\n1,2\n9,8,7,6").expect("csv");
        let t = sheet(&doc);
        let ncols = t.rows[0].cells.len().max(4);
        // Every row is rectangular to the widest record (4 here).
        assert_eq!(ncols, 4);
        for row in &t.rows {
            assert_eq!(row.cells.len(), 4);
        }
        // Short row padded with empty cells.
        assert_eq!(t.rows[1].cells[0].value, CellValue::Number(1.0));
        assert_eq!(t.rows[1].cells[2].value, CellValue::Empty);
    }

    #[test]
    fn crlf_line_endings() {
        let doc = csv_to_model(b"a,b\r\n1,2\r\n3,4\r\n").expect("csv");
        let t = sheet(&doc);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[2].cells[1].value, CellValue::Number(4.0));
    }

    #[test]
    fn bom_is_stripped() {
        let doc = csv_to_model(b"\xEF\xBB\xBFName,Age\nAna,9").expect("csv");
        let t = sheet(&doc);
        assert_eq!(cell_text(&t.rows[0].cells[0]), "Name", "BOM not in header");
    }

    #[test]
    fn trailing_newline_no_phantom_row() {
        let doc = csv_to_model(b"a,b\n1,2\n").expect("csv");
        assert_eq!(sheet(&doc).rows.len(), 2, "no empty trailing record");
    }

    #[test]
    fn empty_input_is_none() {
        assert!(csv_to_model(b"").is_none());
        assert!(csv_to_model(b"\n\n").is_none());
    }

    #[test]
    fn single_column_no_delimiter() {
        let doc = csv_to_model(b"header\nrow1\nrow2").expect("csv");
        let t = sheet(&doc);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0].cells.len(), 1);
        assert_eq!(cell_text(&t.rows[2].cells[0]), "row2");
    }

    #[test]
    fn utf8_multibyte_preserved() {
        let doc = csv_to_model("nom,ville\nÉloïse,Tōkyō".as_bytes()).expect("csv");
        let t = sheet(&doc);
        assert_eq!(cell_text(&t.rows[1].cells[0]), "Éloïse");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "Tōkyō");
    }

    // ── Type inference (the #4 "import" item) ──────────────────────────────

    #[test]
    fn integer_cell_is_number() {
        // `42` → integer-valued number cell.
        let doc = csv_to_model(b"n\n42").expect("csv");
        assert_eq!(cell(&doc, 1, 0).value, CellValue::Number(42.0));
    }

    #[test]
    fn float_cell_is_number() {
        // `3.14` → float number cell. (Expected value parsed, not a literal, to
        // keep clippy's `approx_constant` lint off the `3.14`≈π coincidence.)
        let doc = csv_to_model(b"n\n3.14").expect("csv");
        let expected = "3.14".parse::<f64>().unwrap();
        assert_eq!(cell(&doc, 1, 0).value, CellValue::Number(expected));
    }

    #[test]
    fn signed_and_scientific_numbers() {
        let doc = csv_to_model(b"a,b,c\n-7,1e5,-3.5E-2").expect("csv");
        assert_eq!(cell(&doc, 1, 0).value, CellValue::Number(-7.0));
        assert_eq!(cell(&doc, 1, 1).value, CellValue::Number(100_000.0));
        assert_eq!(cell(&doc, 1, 2).value, CellValue::Number(-0.035));
    }

    #[test]
    fn boolean_cell_case_insensitive() {
        // `true`/`false`/`TRUE`/`FALSE` (any case) → boolean.
        let doc = csv_to_model(b"a,b,c,d\ntrue,false,TRUE,False").expect("csv");
        assert_eq!(cell(&doc, 1, 0).value, CellValue::Bool(true));
        assert_eq!(cell(&doc, 1, 1).value, CellValue::Bool(false));
        assert_eq!(cell(&doc, 1, 2).value, CellValue::Bool(true));
        assert_eq!(cell(&doc, 1, 3).value, CellValue::Bool(false));
    }

    #[test]
    fn iso_date_cell_is_serial_number() {
        // `2026-06-25` → serial day number with a date number-format.
        let doc = csv_to_model(b"when\n2026-06-25").expect("csv");
        let c = cell(&doc, 1, 0);
        // Serial day count from the 1899-12-30 spreadsheet epoch (proleptic
        // Gregorian, i.e. without the legacy 1900 phantom-leap-day offset).
        assert_eq!(c.value, CellValue::Number(46_198.0));
        assert_eq!(c.number_format.as_deref(), Some("yyyy-mm-dd"));
        // A well-known anchor: 1900-01-01 == serial 2 in this convention.
        let anchor = csv_to_model(b"d\n1900-01-01").expect("csv");
        assert_eq!(cell(&anchor, 1, 0).value, CellValue::Number(2.0));
    }

    #[test]
    fn iso_datetime_cell_carries_time_fraction() {
        // Noon → +0.5 of a day; datetime number-format.
        let doc = csv_to_model(b"ts\n2026-06-25T12:00:00").expect("csv");
        let c = cell(&doc, 1, 0);
        assert_eq!(c.value, CellValue::Number(46_198.5));
        assert_eq!(c.number_format.as_deref(), Some("yyyy-mm-dd hh:mm:ss"));
        // A space separator is accepted too, and `HH:MM` without seconds.
        let spaced = csv_to_model(b"ts\n2026-06-25 06:00").expect("csv");
        assert_eq!(cell(&spaced, 1, 0).value, CellValue::Number(46_198.25));
    }

    #[test]
    fn leading_zero_numeric_stays_text() {
        // A ZIP like `01234` must NOT become the integer 1234.
        let doc = csv_to_model(b"zip\n01234").expect("csv");
        assert_eq!(cell(&doc, 1, 0).value, CellValue::Text("01234".into()));
        // …and a lone `0` is still a number.
        let zero = csv_to_model(b"n\n0").expect("csv");
        assert_eq!(cell(&zero, 1, 0).value, CellValue::Number(0.0));
    }

    #[test]
    fn phone_like_token_stays_text() {
        // `+1-555-0100` has separators → never a (truncated) number.
        let doc = csv_to_model(b"phone\n+1-555-0100").expect("csv");
        assert_eq!(
            cell(&doc, 1, 0).value,
            CellValue::Text("+1-555-0100".into())
        );
    }

    #[test]
    fn long_digit_run_stays_text() {
        // A 16-digit identifier (e.g. a card number) would lose precision as f64.
        let doc = csv_to_model(b"id\n4111111111111111").expect("csv");
        assert_eq!(
            cell(&doc, 1, 0).value,
            CellValue::Text("4111111111111111".into())
        );
    }

    #[test]
    fn ambiguous_and_partial_dates_stay_text() {
        // Locale date and out-of-range fields are not coerced.
        let doc = csv_to_model(b"a,b,c\n06/25/2026,2026-13-01,2026-02-30").expect("csv");
        assert_eq!(cell(&doc, 1, 0).value, CellValue::Text("06/25/2026".into()));
        assert_eq!(cell(&doc, 1, 1).value, CellValue::Text("2026-13-01".into()));
        assert_eq!(cell(&doc, 1, 2).value, CellValue::Text("2026-02-30".into()));
    }

    #[test]
    fn empty_cell_is_empty() {
        // An empty field (and a whitespace-only field) → Empty, not text.
        let doc = csv_to_model(b"a,b\nx,\ny, ").expect("csv");
        assert_eq!(cell(&doc, 1, 1).value, CellValue::Empty);
        assert_eq!(cell(&doc, 2, 1).value, CellValue::Empty);
    }

    #[test]
    fn leap_day_valid_but_non_leap_day_text() {
        // 2024 is a leap year → 02-29 is a date; 2023 is not → stays text.
        let leap = csv_to_model(b"d\n2024-02-29").expect("csv");
        assert!(matches!(cell(&leap, 1, 0).value, CellValue::Number(_)));
        let non_leap = csv_to_model(b"d\n2023-02-29").expect("csv");
        assert_eq!(
            cell(&non_leap, 1, 0).value,
            CellValue::Text("2023-02-29".into())
        );
    }

    #[test]
    fn unit_helpers_infer_cell() {
        assert_eq!(infer_cell("42"), (CellValue::Number(42.0), None));
        let pi_ish = "3.14".parse::<f64>().unwrap();
        assert_eq!(infer_cell("3.14"), (CellValue::Number(pi_ish), None));
        assert_eq!(infer_cell("TRUE"), (CellValue::Bool(true), None));
        assert_eq!(infer_cell("   "), (CellValue::Empty, None));
        assert_eq!(parse_number("01234"), None);
        assert_eq!(parse_number("0"), Some(0.0));
        assert_eq!(parse_number("1e"), None);
        assert_eq!(parse_iso_date("2026-06-25").map(|(s, _)| s), Some(46_198.0));
        assert_eq!(parse_iso_date("not-a-date"), None);
    }
}
