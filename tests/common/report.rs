//! The near-term diagram of a report's page: the one `<img>` that shows
//! the SVG as a `data:` URL of its own bytes in base64.

/// The SVG the page's diagram image shows, and the page without that
/// image; `None` for a page with no diagram image.
pub fn diagram_image(page: &str) -> Option<(String, String)> {
    let open = format!(
        "<div class=\"scroll diagram\"><img src=\"{}",
        dagq::domain::kpi::report::IMAGE_PREFIX
    );
    let start = page.find(&open)?;
    let data = start + open.len();
    let quote = data + page[data..].find('"')?;
    let end = quote + page[quote..].find("</div>")? + "</div>".len();
    let svg = String::from_utf8(base64_decoded(&page[data..quote])).ok()?;
    Some((svg, format!("{}{}", &page[..start], &page[end..])))
}

/// The bytes of base64 `text` with padding (RFC 4648 section 4).
fn base64_decoded(text: &str) -> Vec<u8> {
    let value = |b: u8| match b {
        b'A'..=b'Z' => b - b'A',
        b'a'..=b'z' => b - b'a' + 26,
        b'0'..=b'9' => b - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("{b} is not base64"),
    };
    let mut out = Vec::new();
    for chunk in text.as_bytes().chunks(4) {
        let data: Vec<u8> = chunk
            .iter()
            .copied()
            .take_while(|b| *b != b'=')
            .map(value)
            .collect();
        let group = data.iter().enumerate().fold(0u32, |group, (at, v)| {
            group | (u32::from(*v) << (18 - 6 * at))
        });
        for at in 0..data.len().saturating_sub(1) {
            out.push((group >> (16 - 8 * at)) as u8);
        }
    }
    out
}
