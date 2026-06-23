//! CSV → unified editable [`Document`](crate::model::Document) importer.
//!
//! A correct, dependency-free CSV reader (RFC 4180) that lowers a delimited file
//! into a single [`Table`] block — the natural editable shape for tabular data:
//!
//! - Quoted fields (`"…"`) may contain the delimiter, line breaks, and escaped
//!   quotes (`""` ⇒ a literal `"`).
//! - Record terminators `\n`, `\r\n` and a bare `\r` are all accepted.
//! - The delimiter is **auto-detected** among `,`, `;`, `\t` and `|` by scoring
//!   which one yields the most consistent column count across the first records.
//! - The first row is treated as a header (bold + lightly shaded). Ragged rows
//!   are padded/truncated to the header width so the table stays rectangular.
//!
//! A leading UTF-8 BOM is stripped. Empty input (no fields) yields `None`.

use crate::convert::style::Generic;
use crate::model::{
    Block, BlockKind, Cell, CharStyle, Document, Inline, InlineRun, Page, Paragraph, Row, Section,
    Table,
};

/// Body run size for cell text (points).
const CELL_PT: f64 = 11.0;

/// Candidate field delimiters, tried in order for auto-detection.
const DELIMITERS: [u8; 4] = [b',', b';', b'\t', b'|'];

/// CSV bytes → [`Document`]: one section / one page holding a single [`Table`].
/// Returns `None` if the input has no parseable fields.
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
        kind: BlockKind::Table(Table {
            rows,
            col_widths: Vec::new(),
            ..Table::default()
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

/// Build a table [`Row`] from a record, padded/truncated to `ncols`. The header
/// row gets bold runs and light shading.
fn make_row(fields: &[String], ncols: usize, header: bool) -> Row {
    let mut cells = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let text = fields.get(c).map(String::as_str).unwrap_or("");
        cells.push(Cell {
            blocks: vec![cell_paragraph(text, header)],
            shading: header.then_some([0.93, 0.93, 0.93]),
            ..Cell::default()
        });
    }
    Row {
        cells,
        height: None,
    }
}

/// A single-run paragraph for a cell; header runs are bold.
fn cell_paragraph(text: &str, header: bool) -> Block {
    let style = CharStyle {
        generic: Generic::Sans,
        size_pt: CELL_PT,
        bold: header,
        ..CharStyle::default()
    };
    let runs = vec![Inline::Run(InlineRun {
        text: text.to_string(),
        style,
        source_index: None,
    })];
    Block {
        kind: BlockKind::Paragraph(Paragraph {
            runs,
            ..Paragraph::default()
        }),
        ..Block::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The single table block on the page.
    fn table(doc: &Document) -> &Table {
        match &doc.sections[0].pages[0].blocks[0].kind {
            BlockKind::Table(t) => t,
            other => panic!("expected table, got {other:?}"),
        }
    }

    /// Concatenated text of a cell's paragraph runs.
    fn cell_text(cell: &Cell) -> String {
        let mut s = String::new();
        for blk in &cell.blocks {
            if let BlockKind::Paragraph(p) = &blk.kind {
                for inline in &p.runs {
                    if let Inline::Run(r) = inline {
                        s.push_str(&r.text);
                    }
                }
            }
        }
        s
    }

    #[test]
    fn simple_comma_table_with_header() {
        let doc = csv_to_model(b"Name,Age\nAlice,30\nBob,25").expect("csv");
        let t = table(&doc);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0].cells.len(), 2);
        assert_eq!(cell_text(&t.rows[0].cells[0]), "Name");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "30");
        assert_eq!(cell_text(&t.rows[2].cells[0]), "Bob");
        // Header is bold + shaded.
        assert!(t.rows[0].cells[0].shading.is_some());
        match &t.rows[0].cells[0].blocks[0].kind {
            BlockKind::Paragraph(p) => {
                assert!(matches!(&p.runs[0], Inline::Run(r) if r.style.bold));
            }
            _ => panic!(),
        }
        // Body is not bold / not shaded.
        assert!(t.rows[1].cells[0].shading.is_none());
    }

    #[test]
    fn quoted_field_with_comma_and_newline() {
        // A1 = `Hello, World`, A2 holds an embedded newline; quotes preserved.
        let csv = "greeting,note\n\"Hello, World\",\"line1\nline2\"\n";
        let doc = csv_to_model(csv.as_bytes()).expect("csv");
        let t = table(&doc);
        assert_eq!(t.rows.len(), 2, "embedded newline stays in the field");
        assert_eq!(cell_text(&t.rows[1].cells[0]), "Hello, World");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "line1\nline2");
    }

    #[test]
    fn escaped_quote_doubled() {
        // RFC 4180: `""` inside a quoted field is one literal `"`.
        let doc = csv_to_model(b"a,b\n\"say \"\"hi\"\"\",plain").expect("csv");
        let t = table(&doc);
        assert_eq!(cell_text(&t.rows[1].cells[0]), "say \"hi\"");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "plain");
    }

    #[test]
    fn semicolon_delimiter_autodetected() {
        let doc = csv_to_model(b"name;city;age\nAna;Paris;40\nLi;Rome;22").expect("csv");
        let t = table(&doc);
        assert_eq!(t.rows[0].cells.len(), 3, "split on ';'");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "Paris");
    }

    #[test]
    fn tab_delimiter_autodetected() {
        let doc = csv_to_model(b"a\tb\tc\n1\t2\t3").expect("csv");
        let t = table(&doc);
        assert_eq!(t.rows[0].cells.len(), 3, "split on tab");
        assert_eq!(cell_text(&t.rows[1].cells[2]), "3");
    }

    #[test]
    fn comma_preferred_when_both_present() {
        // Each line has 3 commas and 1 semicolon → comma wins (more, consistent).
        let doc = csv_to_model(b"a,b;x,c,d\n1,2;y,3,4").expect("csv");
        let t = table(&doc);
        assert_eq!(t.rows[0].cells.len(), 4);
        // The semicolon stays inside the cell that contained it.
        assert_eq!(cell_text(&t.rows[0].cells[1]), "b;x");
    }

    #[test]
    fn ragged_rows_padded_to_header_width() {
        let doc = csv_to_model(b"a,b,c\n1,2\n9,8,7,6").expect("csv");
        let t = table(&doc);
        let ncols = t.rows[0].cells.len().max(4);
        // Every row is rectangular to the widest record (4 here).
        assert_eq!(ncols, 4);
        for row in &t.rows {
            assert_eq!(row.cells.len(), 4);
        }
        // Short row padded with empty cells.
        assert_eq!(cell_text(&t.rows[1].cells[0]), "1");
        assert_eq!(cell_text(&t.rows[1].cells[2]), "");
    }

    #[test]
    fn crlf_line_endings() {
        let doc = csv_to_model(b"a,b\r\n1,2\r\n3,4\r\n").expect("csv");
        let t = table(&doc);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(cell_text(&t.rows[2].cells[1]), "4");
    }

    #[test]
    fn bom_is_stripped() {
        let doc = csv_to_model(b"\xEF\xBB\xBFName,Age\nAna,9").expect("csv");
        let t = table(&doc);
        assert_eq!(cell_text(&t.rows[0].cells[0]), "Name", "BOM not in header");
    }

    #[test]
    fn trailing_newline_no_phantom_row() {
        let doc = csv_to_model(b"a,b\n1,2\n").expect("csv");
        assert_eq!(table(&doc).rows.len(), 2, "no empty trailing record");
    }

    #[test]
    fn empty_input_is_none() {
        assert!(csv_to_model(b"").is_none());
        assert!(csv_to_model(b"\n\n").is_none());
    }

    #[test]
    fn single_column_no_delimiter() {
        let doc = csv_to_model(b"header\nrow1\nrow2").expect("csv");
        let t = table(&doc);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.rows[0].cells.len(), 1);
        assert_eq!(cell_text(&t.rows[2].cells[0]), "row2");
    }

    #[test]
    fn utf8_multibyte_preserved() {
        let doc = csv_to_model("nom,ville\nÉloïse,Tōkyō".as_bytes()).expect("csv");
        let t = table(&doc);
        assert_eq!(cell_text(&t.rows[1].cells[0]), "Éloïse");
        assert_eq!(cell_text(&t.rows[1].cells[1]), "Tōkyō");
    }
}
