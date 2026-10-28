//! A text run whose font cannot be resolved keeps a usable family name
//! (`"Helvetica"`, the standard viewer substitution) and is flagged by
//! `font_resolved = false`. An unreleased build reported a `"__MISSING__"`
//! sentinel in `font_family` instead, which hosts feed straight to CSS.

use gigapdf_core::Document;

/// One-page PDF declaring only `/F1` (Helvetica); `content` is the page stream.
fn one_page_pdf(content: &str) -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

#[test]
fn resolved_font_is_flagged_resolved() {
    let doc = Document::open(&one_page_pdf("BT /F1 12 Tf 72 700 Td (Known) Tj ET")).unwrap();
    let runs = doc.page_text_elements(1);

    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].font_family, "Helvetica");
    assert!(runs[0].font_resolved);
}

#[test]
fn unresolved_font_keeps_a_usable_family_and_is_flagged() {
    // /F9 is used but declared nowhere.
    let doc = Document::open(&one_page_pdf("BT /F9 12 Tf 72 700 Td (Ghost) Tj ET")).unwrap();
    let runs = doc.page_text_elements(1);

    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].text, "Ghost");
    assert_eq!(runs[0].font_family, "Helvetica");
    assert!(!runs[0].font_resolved);
}
