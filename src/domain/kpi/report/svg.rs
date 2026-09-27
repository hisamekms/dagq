//! The near-term dependency diagram's SVG made fit to put inline in the
//! report (ADR-0077 decision 7): the report loads nothing from anywhere
//! else (ADR-0051 decision 20), so the SVG d2 wrote keeps only what refers
//! inside it (`#id`) or carries its bytes along (`data:`). The XML prologue
//! and the namespace declarations go (HTML knows `svg` and `xlink`), and an
//! element, attribute, `url()` or `@import` that would load something is
//! removed. Only markup is read: the tags and the content of `<style>`;
//! text (a task's title in a label) is left as it is. An SVG that still
//! refers outside afterwards is refused with the references found.
use std::{fmt::Write, ops::Range};

/// Elements that load what they show or run, lead the page elsewhere or
/// change an attribute over time.
const LOADING_ELEMENTS: &[&str] = &[
    "script",
    "link",
    "img",
    "iframe",
    "frame",
    "object",
    "embed",
    "audio",
    "video",
    "source",
    "track",
    "portal",
    "base",
    "set",
    "animate",
    "animatemotion",
    "animatetransform",
];
/// Attributes whose value an element loads.
const LOADING_ATTRIBUTES: &[&str] = &[
    "href",
    "xlink:href",
    "src",
    "srcset",
    "action",
    "formaction",
    "poster",
    "background",
    "data",
];

/// `svg` (d2's output) fit to put inline in the page, or why it is not.
pub fn inline_svg(svg: &str) -> Result<String, String> {
    let lower = svg.to_ascii_lowercase();
    let (Some(start), Some(end)) = (lower.find("<svg"), lower.rfind("</svg>")) else {
        return Err("d2 wrote no <svg> element".to_owned());
    };
    if end < start {
        return Err("d2 wrote no <svg> element".to_owned());
    }
    let svg = remove_loading_elements(&svg[start..end + "</svg>".len()]);
    let svg: String = parts(&svg)
        .into_iter()
        .map(|(part, range)| {
            let text = &svg[range];
            match part {
                Part::Text => text.to_owned(),
                Part::Tag => rewrite_tag(text),
                Part::Style => replace_urls(&remove_imports(text)),
            }
        })
        .collect();
    let left = external_references(&svg);
    if left.is_empty() {
        Ok(svg)
    } else {
        Err(format!(
            "the SVG d2 wrote refers outside the page: {}",
            left.join(", ")
        ))
    }
}

/// What in `page` would load something from outside it: a loading element,
/// an `@import` or `image-set(` in a style, and an attribute or `url()`
/// naming neither `#id` nor a `data:` URL. Empty for a page that loads
/// nothing. Text is not read.
pub fn external_references(page: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (part, range) in parts(page) {
        let text = &page[range];
        match part {
            Part::Text => continue,
            Part::Tag => {
                let name = tag_name(text);
                let attributes = attributes(text);
                let refresh = name == "meta" && attributes.iter().any(|a| a.name == "http-equiv");
                if LOADING_ELEMENTS.contains(&name.as_str()) || refresh {
                    found.push(format!("<{name}>"));
                }
                for attribute in attributes {
                    if loads(&attribute.name, &attribute.value) {
                        found.push(format!("{}=\"{}\"", attribute.name, attribute.value));
                    }
                    found.extend(outside_urls(&attribute.value));
                }
            }
            Part::Style => {
                let lower = text.to_ascii_lowercase();
                for rule in ["@import", "image-set("] {
                    if lower.contains(rule) {
                        found.push(rule.to_owned());
                    }
                }
                found.extend(outside_urls(text));
            }
        }
    }
    found
}

/// The `url()`s of `text` that point outside the page or have no `)`.
fn outside_urls(text: &str) -> Vec<String> {
    urls(text)
        .into_iter()
        .filter_map(|(target, _)| match target {
            Some(target) if inside(&target) => None,
            Some(target) => Some(format!("url({target})")),
            None => Some("url( without its )".to_owned()),
        })
        .collect()
}

/// Whether an attribute `name="value"` loads from outside the page.
fn loads(name: &str, value: &str) -> bool {
    (LOADING_ATTRIBUTES.contains(&name) && !inside(value))
        || value.to_ascii_lowercase().contains("image-set(")
}

/// Whether a reference stays in the page: `#id`, a `data:` URL or nothing.
fn inside(reference: &str) -> bool {
    let reference = reference.trim().to_ascii_lowercase();
    reference.is_empty() || reference.starts_with('#') || reference.starts_with("data:")
}

/// What a stretch of a page is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    /// Text between tags, or a comment.
    Text,
    /// A start or end tag, `<` to `>`.
    Tag,
    /// The content of a `<style>` element.
    Style,
}

/// `text` cut into its text, tags and style contents, in order.
fn parts(text: &str) -> Vec<(Part, Range<usize>)> {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let Some(open) = lower[at..].find('<').map(|open| at + open) else {
            found.push((Part::Text, at..bytes.len()));
            break;
        };
        if open > at {
            found.push((Part::Text, at..open));
        }
        if lower[open..].starts_with("<!--") {
            let end = lower[open..]
                .find("-->")
                .map_or(bytes.len(), |end| open + end + 3);
            found.push((Part::Text, open..end));
            at = end;
            continue;
        }
        // To the `>` outside a quoted value.
        let mut end = open + 1;
        let mut quote = None;
        while end < bytes.len() {
            match (quote, bytes[end]) {
                (None, b'>') => break,
                (None, b @ (b'"' | b'\'')) => quote = Some(b),
                (Some(q), b) if b == q => quote = None,
                _ => {}
            }
            end += 1;
        }
        let end = (end + 1).min(bytes.len());
        found.push((Part::Tag, open..end));
        at = end;
        let tag = &text[open..end];
        if tag_name(tag) == "style" && !tag.ends_with("/>") {
            let close = lower[at..]
                .find("</style")
                .map_or(bytes.len(), |close| at + close);
            found.push((Part::Style, at..close));
            at = close;
        }
    }
    found
}

/// A tag's element name in lower case (`/name` for an end tag).
fn tag_name(tag: &str) -> String {
    let rest = tag.strip_prefix('<').unwrap_or(tag);
    let (slash, rest) = match rest.strip_prefix('/') {
        Some(rest) => ("/", rest),
        None => ("", rest),
    };
    let name: String = rest
        .chars()
        .take_while(|c| !c.is_ascii_whitespace() && *c != '>' && *c != '/')
        .collect();
    format!("{slash}{}", name.to_ascii_lowercase())
}

/// One attribute of a tag: the name in lower case, the value, and the
/// range it spans with the white space or `/` before it.
struct Attribute {
    name: String,
    value: String,
    range: Range<usize>,
}

/// The attributes of `tag` as HTML reads them: separated by white space or
/// `/`, with or without white space around `=`, the value quoted or not.
fn attributes(tag: &str) -> Vec<Attribute> {
    let bytes = tag.as_bytes();
    let separator = |b: u8| b.is_ascii_whitespace() || b == b'/';
    // Past `<` and the element's name.
    let mut at = 1;
    while at < bytes.len() && !separator(bytes[at]) && bytes[at] != b'>' {
        at += 1;
    }
    let mut found = Vec::new();
    while at < bytes.len() {
        let start = at;
        while at < bytes.len() && separator(bytes[at]) {
            at += 1;
        }
        if at >= bytes.len() || bytes[at] == b'>' {
            break;
        }
        let name_start = at;
        while at < bytes.len() && !separator(bytes[at]) && !matches!(bytes[at], b'=' | b'>') {
            at += 1;
        }
        let name = tag[name_start..at].to_ascii_lowercase();
        let mut look = at;
        while look < bytes.len() && bytes[look].is_ascii_whitespace() {
            look += 1;
        }
        if bytes.get(look) != Some(&b'=') {
            found.push(Attribute {
                name,
                value: String::new(),
                range: start..at,
            });
            continue;
        }
        at = look + 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let value = match bytes.get(at) {
            Some(&quote @ (b'"' | b'\'')) => {
                let value_start = at + 1;
                let value_end = tag[value_start..]
                    .find(char::from(quote))
                    .map_or(bytes.len(), |end| value_start + end);
                at = (value_end + 1).min(bytes.len());
                &tag[value_start..value_end]
            }
            _ => {
                let value_start = at;
                while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>' {
                    at += 1;
                }
                &tag[value_start..at]
            }
        };
        found.push(Attribute {
            name,
            value: value.to_owned(),
            range: start..at,
        });
    }
    found
}

/// `tag` without its namespace declarations and the attributes that load
/// from outside, and with each `url()` of a value that points outside
/// replaced by `none`.
fn rewrite_tag(tag: &str) -> String {
    let mut kept = String::with_capacity(tag.len());
    let mut from = 0;
    for Attribute { name, value, range } in attributes(tag) {
        let dropped = name == "xmlns" || name.starts_with("xmlns:") || loads(&name, &value);
        let rewritten = replace_urls(&value);
        if dropped || rewritten != value {
            kept.push_str(&tag[from..range.start]);
            if !dropped {
                let _ = write!(kept, " {name}=\"{}\"", rewritten.replace('"', "&quot;"));
            }
            from = range.end;
        }
    }
    kept.push_str(&tag[from..]);
    kept
}

/// `svg` without its loading elements: from the start tag to its end tag,
/// or only the start tag when there is no end tag.
fn remove_loading_elements(svg: &str) -> String {
    let mut svg = svg.to_owned();
    loop {
        let found = parts(&svg).into_iter().find_map(|(part, range)| {
            let name = tag_name(&svg[range.clone()]);
            // No `meta` belongs in a drawing (a refresh leads elsewhere).
            (part == Part::Tag && (LOADING_ELEMENTS.contains(&name.as_str()) || name == "meta"))
                .then_some((name, range))
        });
        let Some((name, range)) = found else {
            return svg;
        };
        let close = format!("</{name}>");
        let end = svg[range.end..]
            .to_ascii_lowercase()
            .find(&close)
            .map_or(range.end, |end| range.end + end + close.len());
        svg.replace_range(range.start..end, "");
    }
}

/// Each `url(...)` of a tag or a style: its target without quotes (none
/// when the `)` is missing), and the range it spans.
fn urls(text: &str) -> Vec<(Option<String>, Range<usize>)> {
    let lower = text.to_ascii_lowercase();
    lower
        .match_indices("url(")
        .map(|(at, _)| {
            let open = at + "url(".len();
            match lower[open..].find(')') {
                Some(close) => {
                    let close = open + close;
                    let target = text[open..close]
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'')
                        .to_owned();
                    (Some(target), at..close + 1)
                }
                None => (None, at..text.len()),
            }
        })
        .collect()
}

/// `text` with each `url()` that points outside it, or has no `)`,
/// replaced by `none`.
fn replace_urls(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut from = 0;
    for (target, range) in urls(text) {
        if !target.as_deref().is_some_and(inside) && range.start >= from {
            kept.push_str(&text[from..range.start]);
            kept.push_str("none");
            from = range.end;
        }
    }
    kept.push_str(&text[from..]);
    kept
}

/// `style` without its `@import` rules (to the `;` that ends each).
fn remove_imports(style: &str) -> String {
    let mut style = style.to_owned();
    while let Some(at) = style.to_ascii_lowercase().find("@import") {
        let end = style[at..]
            .find(';')
            .map_or(style.len(), |end| at + end + 1);
        style.replace_range(at..end, "");
    }
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What d2 writes, trimmed, with every way of loading from outside.
    const D2_LIKE: &str = r##"<?xml version="1.0" encoding="utf-8"?><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" data-d2-version="v0.9.0" viewBox="0 0 10 10"><svg class="d2-1 d2-svg" width="10" height="10"><style type="text/css"><![CDATA[@import url("https://fonts.example/a.css");
@font-face { font-family: d2-1-font; src: url("data:application/font-woff;base64,d09GRg=="); }
.d2-1 .fill { fill: url(#grad); stroke: url( 'http://example.com/p.svg#x' ); }]]></style><script type="text/javascript">fetch("https://example.com")</script><a href="https://example.com/task/1" xlink:href='http://example.com'><rect class="fill" x="0" y="0" width="10" height="10" /></a><image href="https://example.com/i.png" x="0"/><IMG SRC="http://example.com/x.png"><use href="#mark" /><image href="data:image/png;base64,iVBOR" /><text x="1">a -&gt; b (see https://example.com)</text><text>Fix url(parsing in 日本語 config</text><text>Support @import of 設定</text><rect style="fill: url(#g)" /></svg></svg>
"##;

    #[test]
    fn keeps_what_stays_in_the_page_and_drops_what_loads() {
        assert!(!external_references(D2_LIKE).is_empty());
        let svg = inline_svg(D2_LIKE).unwrap();
        assert_eq!(external_references(&svg), Vec::<String>::new());
        assert!(svg.starts_with("<svg data-d2-version=\"v0.9.0\""), "{svg}");
        assert!(svg.ends_with("</svg></svg>"));
        for gone in [
            "<?xml",
            "xmlns",
            "@import url",
            "<script",
            "fetch(",
            "<IMG",
            "p.svg",
            "task/1",
        ] {
            assert!(!svg.contains(gone), "{gone} in {svg}");
        }
        for kept in [
            "url(\"data:application/font-woff;base64,d09GRg==\")",
            "fill: url(#grad)",
            "stroke: none",
            "<use href=\"#mark\" />",
            "<image href=\"data:image/png;base64,iVBOR\" />",
            "<a><rect class=\"fill\"",
            "<image x=\"0\"/>",
            // Text is not read: a title keeps its words and the markup
            // after it stays whole.
            "(see https://example.com)",
            "<text>Fix url(parsing in 日本語 config</text>",
            "<text>Support @import of 設定</text>",
            "<rect style=\"fill: url(#g)\" /></svg></svg>",
        ] {
            assert!(svg.contains(kept), "{kept} not in {svg}");
        }
    }

    #[test]
    fn names_what_refers_outside() {
        let found = external_references(
            "<p style=\"background: url(http://x/y)\"><link rel=a><iframe src='https://x'></iframe>",
        );
        assert_eq!(
            found,
            ["url(http://x/y)", "<link>", "<iframe>", "src=\"https://x\"",]
        );
        // A longer name is not the element, a `data-` attribute not `data`,
        // and text is not read.
        assert!(
            external_references(
                "<linked data-x=\"http://x\" data=\"#a\">href=\"http://x\" url(http://x) @import"
            )
            .is_empty()
        );
        // However the attribute is written.
        for tag in [
            "<a href = \"http://x\">",
            "<a href=http://x>",
            "<image/href=\"http://x\">",
            "<use x=\"1\"href=\"http://x\">",
            "<set attributeName=\"href\" to=\"http://x\"/>",
            "<meta http-equiv=\"refresh\" content=\"0;url=http://x\">",
            "<style>a{b:image-set(\"http://x\" 1x)}</style>",
            "<rect fill=\"url(http://x\">",
        ] {
            assert!(!external_references(tag).is_empty(), "{tag}");
            let svg = inline_svg(&format!("<svg>{tag}<rect/></svg>")).unwrap_or_default();
            assert!(!svg.contains("http://x"), "{tag}: {svg}");
        }
        assert!(inline_svg("no drawing").unwrap_err().contains("no <svg>"));
        assert!(inline_svg("</svg><svg").is_err());
        // An end tag that never comes removes the start tag, and the rest
        // stays.
        assert_eq!(
            inline_svg("<svg><script src=\"https://x\"><rect/></svg>").unwrap(),
            "<svg><rect/></svg>"
        );
        // A value's `url(` without its `)` goes, and the tag stays whole.
        assert_eq!(
            inline_svg("<svg><rect fill='url(http://x' x=\"1\"/></svg>").unwrap(),
            "<svg><rect fill=\"none\" x=\"1\"/></svg>"
        );
        // The page's own `meta` loads nothing.
        assert!(external_references("<meta charset=\"utf-8\">").is_empty());
        // A tag cut short, and a comment, do not panic.
        assert!(external_references("<a href=\"http://x").len() == 1);
        assert!(external_references("<!-- <img src=http://x> -->").is_empty());
    }
}
