//! The near-term dependency diagram's SVG as the report holds it
//! (ADR-0077 decision 7): the report loads nothing from anywhere else
//! (ADR-0051 decision 20), and the SVG may come from a client of the queue
//! service as well as from the host's d2. The page shows it as an image,
//! an `<img>` whose source is a `data:` URL of the SVG's own bytes
//! ([`image_source`]), which a browser renders in its image mode: no
//! script of it runs, nothing outside those bytes is fetched, and its
//! styles and text stay inside the image, so nothing of the SVG reaches
//! the page's document. It is checked only to be one well-formed `<svg>`
//! element of XML, with no declaration and only the references XML
//! defines, and kept as written up to its end tag; one that is not is
//! refused with why.

/// `svg` (d2's output, or a client's) from its `<svg>` start tag to the
/// end tag of that element, as written, or why it is not kept.
pub fn checked_svg(svg: &str) -> Result<String, String> {
    let start = svg
        .find("<svg")
        .ok_or_else(|| "d2 wrote no <svg> element".to_owned())?;
    let text = &svg[start..];
    well_formed(text)
        .map(|end| text[..end].to_owned())
        .map_err(|why| format!("the SVG d2 wrote is not kept: {why}"))
}

/// The `src` of the `<img>` that shows `svg`: a `data:` URL of its bytes
/// in base64, which holds no character that ends the attribute.
pub fn image_source(svg: &str) -> String {
    format!("{IMAGE_PREFIX}{}", base64(svg.as_bytes()))
}

/// What [`image_source`] starts with.
pub const IMAGE_PREFIX: &str = "data:image/svg+xml;base64,";

/// `bytes` in base64 with padding (RFC 4648 section 4).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().enumerate().fold(0u32, |group, (at, byte)| {
            group | (u32::from(*byte) << (16 - 8 * at))
        });
        for at in 0..4 {
            out.push(if at <= chunk.len() {
                char::from(ALPHABET[((group >> (18 - 6 * at)) & 63) as usize])
            } else {
                '='
            });
        }
    }
    out
}

/// The length of the `<svg>` element at the start of `text` when it is
/// well-formed XML; what follows its end tag is not read.
fn well_formed(text: &str) -> Result<usize, String> {
    let mut open: Vec<&str> = Vec::new();
    let mut reader = Reader { text, at: 0 };
    while let Some(token) = reader.token()? {
        match token {
            Token::Start { name, empty } => {
                if open.is_empty() && name != "svg" {
                    return Err(format!("the drawing is a <{name}>, not an <svg>"));
                }
                if !empty {
                    open.push(name);
                } else if open.is_empty() {
                    return Ok(reader.at);
                }
            }
            Token::End(name) => {
                let Some(closed) = open.pop() else {
                    return Err(format!("</{name}> ends no element"));
                };
                if closed != name {
                    return Err(format!("</{name}> ends <{closed}>"));
                }
                if open.is_empty() {
                    return Ok(reader.at);
                }
            }
        }
    }
    Err(match open.last() {
        Some(last) => format!("<{last}> is not closed"),
        None => "no <svg> element".to_owned(),
    })
}

/// The tags the SVG is read as.
enum Token<'a> {
    /// A start tag (`empty` when it ends with `/>`).
    Start {
        name: &'a str,
        empty: bool,
    },
    End(&'a str),
}

/// An XML reader of the SVG's text, at the byte `at`: what is not
/// well-formed is an error saying where. Text and CDATA sections are
/// checked and passed over, as are comments and processing instructions.
struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    /// The next tag, none at the end of the text.
    fn token(&mut self) -> Result<Option<Token<'a>>, String> {
        loop {
            let rest = &self.text[self.at..];
            if rest.is_empty() {
                return Ok(None);
            }
            if !rest.starts_with('<') {
                let end = rest.find('<').unwrap_or(rest.len());
                check_references(&rest[..end])?;
                self.at += end;
                continue;
            }
            if let Some((open, close)) = [("<!--", "-->"), ("<?", "?>")]
                .into_iter()
                .find(|(open, _)| rest.starts_with(open))
            {
                let end = rest[open.len()..]
                    .find(close)
                    .ok_or_else(|| format!("{open} is not closed"))?;
                self.at += open.len() + end + close.len();
                continue;
            }
            if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
                let end = cdata
                    .find("]]>")
                    .ok_or_else(|| "<![CDATA[ is not closed".to_owned())?;
                self.at += "<![CDATA[".len() + end + "]]>".len();
                continue;
            }
            if rest.starts_with("<!") {
                return Err("a declaration inside the drawing".to_owned());
            }
            return self.tag().map(Some);
        }
    }

    /// The start or end tag at `<`; an attribute named twice is refused.
    fn tag(&mut self) -> Result<Token<'a>, String> {
        let end = self.text[self.at..].starts_with("</");
        self.at += if end { 2 } else { 1 };
        let name = self.name()?;
        if end {
            self.space();
            return self.expect(">").map(|()| Token::End(name));
        }
        let mut attributes = std::collections::HashSet::new();
        loop {
            let spaced = self.space();
            let rest = &self.text[self.at..];
            if let Some(empty) = [(">", false), ("/>", true)]
                .into_iter()
                .find_map(|(close, empty)| rest.starts_with(close).then_some(empty))
            {
                self.at += if empty { 2 } else { 1 };
                return Ok(Token::Start { name, empty });
            }
            if !spaced {
                return Err(format!(
                    "<{name}> is cut short or has no space before an attribute"
                ));
            }
            let attribute = self.name()?;
            if !attributes.insert(attribute) {
                return Err(format!("<{name}> has {attribute} twice"));
            }
            self.space();
            self.expect("=")?;
            self.space();
            let quote = self.text[self.at..]
                .chars()
                .next()
                .filter(|c| matches!(c, '"' | '\''))
                .ok_or_else(|| format!("{attribute} of <{name}> is not quoted"))?;
            let value_start = self.at + 1;
            let value_end = self.text[value_start..]
                .find(quote)
                .map(|end| value_start + end)
                .ok_or_else(|| format!("{attribute} of <{name}> is not closed"))?;
            let raw = &self.text[value_start..value_end];
            if raw.contains('<') {
                return Err(format!("{attribute} of <{name}> holds a <"));
            }
            check_references(raw)?;
            self.at = value_end + 1;
        }
    }

    /// An XML name of ASCII letters, digits and `-_.:`, starting with a
    /// letter, `_` or `:`.
    fn name(&mut self) -> Result<&'a str, String> {
        let rest = &self.text[self.at..];
        let len = rest
            .bytes()
            .enumerate()
            .take_while(|(index, b)| {
                b.is_ascii_alphabetic()
                    || matches!(b, b'_' | b':')
                    || (*index > 0 && (b.is_ascii_digit() || matches!(b, b'-' | b'.')))
            })
            .count();
        if len == 0 {
            let near: String = rest.chars().take(12).collect();
            return Err(format!("no name at {near:?}"));
        }
        self.at += len;
        Ok(&rest[..len])
    }

    /// Pass the white space here; whether there was any.
    fn space(&mut self) -> bool {
        let rest = &self.text[self.at..];
        let len = rest.len() - rest.trim_start_matches([' ', '\t', '\n', '\r']).len();
        self.at += len;
        len > 0
    }

    fn expect(&mut self, what: &str) -> Result<(), String> {
        if self.text[self.at..].starts_with(what) {
            self.at += what.len();
            Ok(())
        } else {
            let near: String = self.text[self.at..].chars().take(12).collect();
            Err(format!("{what} expected at {near:?}"))
        }
    }
}

/// That each `&` of `raw` starts a reference XML defines: one of the five
/// named ones, or a numeric one of a character XML allows.
fn check_references(raw: &str) -> Result<(), String> {
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        let after = &rest[amp + 1..];
        let name = after
            .find(';')
            .map(|semi| &after[..semi])
            .ok_or_else(|| "an & that starts no reference".to_owned())?;
        if !matches!(name, "lt" | "gt" | "amp" | "quot" | "apos") {
            let (digits, radix) = match name.strip_prefix("#x") {
                Some(hex) => (hex, 16),
                None => (name.strip_prefix('#').unwrap_or(""), 10),
            };
            let code = if digits.bytes().all(|b| char::from(b).is_digit(radix)) {
                u32::from_str_radix(digits, radix).ok()
            } else {
                None
            };
            code.and_then(char::from_u32)
                .filter(|c| {
                    matches!(c, '\t' | '\n' | '\r')
                        || (*c >= ' ' && !matches!(*c, '\u{fffe}' | '\u{ffff}'))
                })
                .ok_or_else(|| format!("&{name}; is no reference of a character XML allows"))?;
        }
        rest = &after[name.len() + 1..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drawing as d2 writes one (trimmed): the prologue, the namespace
    /// declarations, nested `svg`s, a stylesheet in CDATA with its fonts
    /// as `data:` URLs, shapes, text with references, a marker and a mask
    /// referred to by `url(#id)`.
    const D2_DRAWING: &str = concat!(
        r##"<?xml version="1.0" encoding="utf-8"?><svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" data-d2-version="v0.9.0" preserveAspectRatio="xMinYMin meet" viewBox="0 0 975 368">"##,
        r##"<svg class="d2-1 d2-svg" width="975" height="368" viewBox="-101 -85 975 368"><rect x="-101" y="-85" width="975" height="368" rx="0" fill="#FFFFFF" class=" fill-N7" stroke-width="0" />"##,
        "<style type=\"text/css\"><![CDATA[\n.d2-1 .text {\n\tfont-family: \"d2-1-font-regular\";\n}\n@font-face {\n\tfont-family: d2-1-font-regular;\n\tsrc: url(\"data:application/font-woff;base64,d09GRg==\");\n}]]></style>",
        r##"<style type="text/css"><![CDATA[.blend { mix-blend-mode: multiply; opacity: 0.5; } .d2-1 .fill-N1{fill:#0A0F25;}]]></style>"##,
        r##"<g class="dDc="><g class="shape" ><rect x="0" y="86" width="151" height="66" stroke="#1a73e8" fill="#e8f0fe" style="stroke-width:2;" /></g>"##,
        r##"<text x="384" y="124.5" fill="#0A0F25" class="text-bold fill-N1" style="text-anchor:middle;font-size:16px"><tspan x="384" dy="0.000000">#9 another &amp; &lt;task&gt;</tspan></text></g>"##,
        r##"<g class="KHQ3"><marker id="mk-1" markerWidth="10" markerHeight="12" refX="7" refY="6" viewBox="0 0 10 12" orient="auto" markerUnits="userSpaceOnUse"> <polygon points="0,0 10,6 0,12" fill="#0D32B2" class="connection fill-B1" stroke-width="2" /> </marker>"##,
        r##"<path d="M 153 119 C 207 119 235 119 287 119" stroke="#0D32B2" fill="none" class="connection stroke-B1" style="stroke-width:2;" marker-end="url(#mk-1)" mask="url(#d2-1)" /></g>"##,
        "<mask id=\"d2-1\" maskUnits=\"userSpaceOnUse\" x=\"-101\" y=\"-85\" width=\"975\" height=\"368\">\n<rect x=\"-101\" y=\"-85\" width=\"975\" height=\"368\" fill=\"white\"></rect>\n</mask></svg></svg>\n",
    );

    /// d2's drawing is kept as written from its `<svg>` to its end tag: its
    /// namespace declarations, stylesheets, fonts and references, which
    /// its image needs, stay.
    #[test]
    fn a_d2_drawing_is_kept_as_written() {
        let start = D2_DRAWING.find("<svg").unwrap();
        assert_eq!(
            checked_svg(D2_DRAWING).unwrap(),
            D2_DRAWING[start..].trim_end()
        );
    }

    /// The image's source is the SVG's bytes in base64 (RFC 4648's test
    /// vectors), with nothing but base64 after its prefix.
    #[test]
    fn an_image_source_is_the_svg_s_bytes_in_base64() {
        for (bytes, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(bytes.as_bytes()), encoded, "{bytes}");
        }
        let source = image_source("<svg>\"'<>&\u{ff}</svg>");
        assert_eq!(source, "data:image/svg+xml;base64,PHN2Zz4iJzw+JsO/PC9zdmc+");
        assert!(
            source[IMAGE_PREFIX.len()..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        );
    }

    /// What is not well-formed XML is refused with where, as is a drawing
    /// that is no `<svg>`; nothing after the `<svg>` element is read.
    #[test]
    fn what_is_not_well_formed_is_refused() {
        assert!(checked_svg("no drawing").unwrap_err().contains("no <svg>"));
        for (svg, why) in [
            ("</svg><svg", "<svg> is cut short"),
            ("<svgx/>", "a <svgx>, not an <svg>"),
            ("<svg><g></svg>", "</svg> ends <g>"),
            ("<svg><rect>", "<rect> is not closed"),
            ("<svg><rect x=1/></svg>", "x of <rect> is not quoted"),
            ("<svg><rect x=\"1</svg>", "x of <rect> is not closed"),
            ("<svg><rect x=\"<\"/></svg>", "holds a <"),
            (
                "<svg><rect x=\"1\"y=\"2\"/></svg>",
                "no space before an attribute",
            ),
            ("<svg><rect x=\"1\" x=\"2\"/></svg>", "has x twice"),
            (
                "<svg><text>a &nbsp; b</text></svg>",
                "&nbsp; is no reference",
            ),
            (
                "<svg><text>a & b</text></svg>",
                "an & that starts no reference",
            ),
            ("<svg><text>&#0;</text></svg>", "&#0; is no reference"),
            ("<svg><text>&#x+41;</text></svg>", "&#x+41; is no reference"),
            ("<svg><rect x=\"&#0;\"/></svg>", "&#0; is no reference"),
            ("<svg><!-- open</svg>", "<!-- is not closed"),
            ("<svg><![CDATA[ open</svg>", "<![CDATA[ is not closed"),
            ("<svg><!DOCTYPE x></svg>", "a declaration"),
        ] {
            let refused = checked_svg(svg).unwrap_err();
            assert!(refused.contains(why), "{refused} of {svg}");
        }
        assert_eq!(checked_svg("<svg/><script>x</script>").unwrap(), "<svg/>");
        assert_eq!(
            checked_svg("<svg><!-- a --><?p i?><![CDATA[<x>]]>&amp;</svg>tail").unwrap(),
            "<svg><!-- a --><?p i?><![CDATA[<x>]]>&amp;</svg>"
        );
        // Deep nesting is read without recursion.
        let deep = format!(
            "<svg>{}{}</svg>",
            "<g>".repeat(10_000),
            "</g>".repeat(10_000)
        );
        assert_eq!(checked_svg(&deep).unwrap(), deep);
        // Many attributes are read once.
        let attributes: String = (0..20_000).map(|n| format!(" data-a{n}=\"\"")).collect();
        let wide = format!("<svg><rect{attributes}/></svg>");
        assert_eq!(checked_svg(&wide).unwrap(), wide);
    }
}
