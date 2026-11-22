use std::fmt::Write as _;

// Portrait pages of different heights, with filled rectangles covering their
// lower halves. This lets tests distinguish a real raster, the page index and Y orientation.
pub fn two_pages() -> Vec<u8> {
    document("")
}

pub fn rotated_second_page() -> Vec<u8> {
    document(" /Rotate 90")
}

fn stream(drawing: &str) -> String {
    format!(
        "<< /Length {} >>\nstream\n{drawing}endstream",
        drawing.len()
    )
}

fn document(second_page_options: &str) -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Count 2 /Kids [3 0 R 4 0 R] >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 800] /Resources << >> /Contents 5 0 R >>"
            .into(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 400] /Resources << >> /Contents 6 0 R{second_page_options} >>"
        ),
        stream("1 0 0 rg 0 0 200 400 re f\n"),
        stream("0 0 1 rg 0 0 200 200 re f\n"),
    ];
    serialize(&objects)
}

// Original, hand-drawn kana outlines in an embedded Type 3 font. Color labels
// the actual match glyphs for a raster-only oracle; there are no marker shapes.
pub fn japanese_text_pages() -> Vec<u8> {
    serialize(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Count 2 /Kids [3 0 R 4 0 R] /Resources << /Font << /F1 7 0 R >> >> >>"
            .into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 960] /Contents 5 0 R >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 600] /Contents 6 0 R >>".into(),
        stream(concat!(
            // Lower match deliberately comes first in PDF content order.
            "BT /F1 36 Tf 112 104 Td 0 0 1 rg (A) Tj (B) Tj ET\n",
            // Reversed term, cross-line pair and horizontally separated pair.
            "BT /F1 36 Tf 116 520 Td 0 g (BA) Tj ET\n",
            "BT /F1 36 Tf 42 420 Td (A) Tj 36 -40 Td (B) Tj ET\n",
            "BT /F1 36 Tf 28 270 Td (A) Tj 144 0 Td (B) Tj ET\n",
            // Upper match is inside イハイハ, not at the line's first glyph.
            "BT /F1 36 Tf 18 838 Td (B) Tj 1 0 0 rg [(A) 0 (B)] TJ 0 g (A) Tj ET\n",
        )),
        stream("BT /F1 36 Tf 77 288 Td 0 1 0 rg (AB) Tj ET\n"),
        concat!(
            "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] ",
            "/FontMatrix [0.001 0 0 0.001 0 0] /Resources << >> ",
            "/CharProcs << /probe-ha 8 0 R /probe-i 9 0 R >> ",
            "/Encoding << /Type /Encoding /Differences [65 /probe-ha /probe-i] >> ",
            "/FirstChar 65 /LastChar 66 /Widths [1000 1000] /ToUnicode 10 0 R >>",
        )
        .into(),
        stream(concat!(
            "1000 0 0 0 1000 1000 d1\n",
            "145 760 m 226 734 l 204 489 145 289 60 170 c 26 171 l 100 357 139 543 145 760 c f\n",
            "558 737 m 613 777 l 770 602 887 383 964 200 c 887 169 l 820 373 702 567 558 737 c f\n",
        )),
        stream(concat!(
            "1000 0 0 0 1000 1000 d1\n",
            "887 845 m 948 788 l 695 540 371 367 53 286 c 28 332 l 341 444 655 608 887 845 c f\n",
            "537 554 m 628 602 l 620 102 l 545 69 l f\n",
        )),
        stream(concat!(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n",
            "/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n",
            "/CMapName /ProbeKana def\n/CMapType 2 def\n",
            "1 begincodespacerange\n<00> <FF>\nendcodespacerange\n",
            "2 beginbfchar\n<41> <30CF>\n<42> <30A4>\nendbfchar\n",
            "endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n",
        )),
    ])
}

// Page 2 has an inherited crop with the same aspect ratio but a different
// origin; page 3 has a rotation. On page 1 only the horizontal glyphs count.
pub fn search_geometry_pages() -> Vec<u8> {
    serialize(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Count 3 /Kids [3 0 R 4 0 R 6 0 R] /MediaBox [0 0 200 400] /Resources << /Font << /F1 7 0 R >> >> >>".into(),
        "<< /Type /Page /Parent 2 0 R /Contents 8 0 R >>".into(),
        "<< /Type /Pages /Parent 2 0 R /Count 1 /Kids [5 0 R] /CropBox [10 20 190 380] >>".into(),
        "<< /Type /Page /Parent 4 0 R /Contents 9 0 R >>".into(),
        "<< /Type /Page /Parent 2 0 R /Rotate 180 /Contents 9 0 R >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        stream(concat!(
            "BT /F1 16 Tf 18 355 Td (Token Token) Tj ET\n",
            "BT /F1 16 Tf 18 285 Td (token) Tj ET\n",
            "BT /F1 16 Tf 0 1 -1 0 120 50 Tm (Token) Tj ET\n",
            "BT /F1 16 Tf 18 160 Td [(alpha) 0 ( beta)] TJ ET\n",
        )),
        stream("BT /F1 16 Tf 24 200 Td (Token) Tj ET\n"),
    ])
}

// Standard Japanese CID font names and a synthetic two-character Unicode map.
// Different DW and W values make an incorrect range decode observable at x=178.
pub fn cid_text_page(widths: &str, encoding: &str, mixed: bool) -> Vec<u8> {
    let drawing = if mixed {
        concat!(
            "BT /F1 20 Tf 178 320 Td <00410042> Tj ET\n",
            "q BT /F1 20 Tf 3 Tr 30 240 Td <00410042> Tj ET Q\n",
            "BT /FV 20 Tf 60 200 Td <00410042> Tj /F1 20 Tf 1 0 0 1 50 140 Tm <00410042> Tj ET\n",
            // Hidden horizontal text must not appear, but still advances the pen.
            "BT /F1 20 Tf 177 80 Td 3 Tr <0041> Tj 0 Tr <00410042> Tj ET\n",
        )
    } else {
        concat!(
            "BT /F1 20 Tf 178 320 Td <00410042> Tj ET\n",
            "BT /F1 20 Tf 55 140 Td [<0041> 0 <0042>] TJ ET\n",
        )
    };
    let font = |encoding: &str| {
        format!(
            "<< /Type /Font /Subtype /Type0 /BaseFont /HeiseiMin-W3 /Encoding /{encoding} /DescendantFonts [7 0 R] /ToUnicode 9 0 R >>"
        )
    };
    serialize(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 400] /Resources << /Font << /F1 5 0 R /FV 6 0 R >> >> /Contents 4 0 R >>".into(),
        stream(drawing),
        font(encoding),
        font("Identity-V"),
        format!("<< /Type /Font /Subtype /CIDFontType0 /BaseFont /HeiseiMin-W3 /CIDSystemInfo << /Registry (Adobe) /Ordering (Japan1) /Supplement 0 >> /FontDescriptor 8 0 R /DW 1000 /W [{widths}] >>"),
        "<< /Type /FontDescriptor /FontName /HeiseiMin-W3 /Flags 6 /FontBBox [-123 -257 1001 910] /Ascent 723 /Descent -241 /CapHeight 709 /StemV 69 /ItalicAngle 0 >>".into(),
        stream(concat!(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n",
            "/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n",
            "/CMapName /SyntheticCID def\n/CMapType 2 def\n",
            "1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
            "2 beginbfchar\n<0041> <30CF>\n<0042> <30A4>\nendbfchar\n",
            "endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n",
        )),
    ])
}

pub fn clipped_labels(path_clip: &str, text_mode: u8) -> Vec<u8> {
    serialize(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 240 150] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".into(),
        stream(&format!(
            "0 0.7 0 rg BT /F1 12 Tf 24 134 Td (TAG) Tj ET\n\
             q 0 0 12 150 re {path_clip} n\n\
             1 0 0 rg BT /F1 12 Tf 24 116 Td (TAG) Tj ET Q\n\
             q 0 0 1 rg 0 0 1 RG BT /F1 12 Tf {text_mode} Tr 24 98 Td (TAG) Tj\n\
             0 -18 Td (TAG) Tj ET\n\
             1 0 0 rg BT /F1 12 Tf 0 Tr 24 62 Td (TAG) Tj ET Q\n\
             0 0.7 0 rg BT /F1 12 Tf 24 44 Td (TAG) Tj ET\n"
        )),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ])
}

pub fn image_only_page() -> Vec<u8> {
    serialize(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Count 1 /Kids [3 0 R] >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 400] /Resources << /XObject << /Im1 5 0 R >> >> /Contents 4 0 R >>".into(),
        stream("q 200 0 0 400 0 0 cm /Im1 Do Q\n"),
        "<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /ASCIIHexDecode /Length 4 >>\nstream\n00>\nendstream".into(),
    ])
}

fn serialize(objects: &[String]) -> Vec<u8> {
    let mut pdf = "%PDF-1.4\n".to_owned();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        let result = writeln!(pdf, "{} 0 obj\n{object}\nendobj", index + 1);
        assert!(result.is_ok());
    }
    let xref = pdf.len();
    let result = writeln!(pdf, "xref\n0 {}\n0000000000 65535 f ", objects.len() + 1);
    assert!(result.is_ok());
    for offset in offsets {
        let result = writeln!(pdf, "{offset:010} 00000 n ");
        assert!(result.is_ok());
    }
    let result = writeln!(
        pdf,
        "trailer\n<< /Root 1 0 R /Size {} >>\nstartxref\n{xref}\n%%EOF",
        objects.len() + 1
    );
    assert!(result.is_ok());
    pdf.into_bytes()
}
