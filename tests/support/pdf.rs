use std::fmt::Write as _;

// Portrait pages of different heights, with filled rectangles covering their
// lower halves. This lets tests distinguish a real raster, the page index and Y orientation.
pub fn two_pages() -> Vec<u8> {
    document("")
}

pub fn rotated_second_page() -> Vec<u8> {
    document(" /Rotate 90")
}

fn document(second_page_options: &str) -> Vec<u8> {
    let stream = |drawing: &str| {
        format!(
            "<< /Length {} >>\nstream\n{drawing}endstream",
            drawing.len()
        )
    };
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
