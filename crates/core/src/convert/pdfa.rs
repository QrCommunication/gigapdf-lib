//! PDF/A metadata (XMP packet) for the archival export.
//!
//! [`Document::to_pdfa`](crate::Document::to_pdfa) adds the structural pieces of
//! PDF/A-2b conformance — this XMP identification packet plus an sRGB
//! OutputIntent (see [`super::srgb_icc`]). Full conformance additionally
//! requires every font embedded; that's documented on `to_pdfa`.

fn xml_escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
}

/// Build the XMP metadata packet identifying the file as PDF/A-2b, with a Dublin
/// Core title and a PDF producer.
pub fn xmp_metadata(title: &str, producer: &str) -> Vec<u8> {
    let (mut t, mut p) = (String::new(), String::new());
    xml_escape(title, &mut t);
    xml_escape(producer, &mut p);
    // The leading BOM + fixed packet id are part of the XMP convention.
    let xmp = format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n\
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n\
  <rdf:Description rdf:about=\"\" xmlns:pdfaid=\"http://www.aiim.org/pdfa/ns/id/\">\n\
   <pdfaid:part>2</pdfaid:part>\n\
   <pdfaid:conformance>B</pdfaid:conformance>\n\
  </rdf:Description>\n\
  <rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n\
   <dc:title><rdf:Alt><rdf:li xml:lang=\"x-default\">{t}</rdf:li></rdf:Alt></dc:title>\n\
  </rdf:Description>\n\
  <rdf:Description rdf:about=\"\" xmlns:pdf=\"http://ns.adobe.com/pdf/1.3/\">\n\
   <pdf:Producer>{p}</pdf:Producer>\n\
  </rdf:Description>\n\
 </rdf:RDF>\n\
</x:xmpmeta>\n\
<?xpacket end=\"w\"?>"
    );
    xmp.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmp_identifies_pdfa_2b() {
        let xmp = String::from_utf8(xmp_metadata("My <Title>", "GigaPDF")).unwrap();
        assert!(xmp.contains("<pdfaid:part>2</pdfaid:part>"));
        assert!(xmp.contains("<pdfaid:conformance>B</pdfaid:conformance>"));
        assert!(xmp.contains("My &lt;Title&gt;"), "title escaped");
        assert!(xmp.starts_with("<?xpacket begin"));
        assert!(xmp.trim_end().ends_with("<?xpacket end=\"w\"?>"));
    }
}
