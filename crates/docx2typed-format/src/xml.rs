//! The crate's own minimal XML view.
//!
//! Deliberately not the text core's walker and not the verifier's: this crate
//! plans formatting and writes runs, so what it needs is a tag list, a run's
//! property range and a run's text — nothing about paragraphs, anchors or
//! identity. Paragraph *identity* arrives from the caller as a byte range, which
//! is what keeps a third copy of the id scheme out of the tree.

/// One tag, with the byte range it occupies in the part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub start: usize,
    pub end: usize,
    pub closing: bool,
    pub self_closing: bool,
}

/// Every tag in a part, in order. Text between tags is not returned; the caller
/// reads it by offset.
pub fn scan_tags(xml: &[u8]) -> Vec<Tag> {
    let mut tags = Vec::new();
    let mut index = 0usize;
    while index < xml.len() {
        if xml[index] != b'<' {
            index += 1;
            continue;
        }
        // Skip comments, CDATA and processing instructions wholesale: they are
        // not elements and their contents are not markup.
        if xml[index..].starts_with(b"<!--") {
            match find(xml, index, b"-->") {
                Some(end) => index = end + 3,
                None => break,
            }
            continue;
        }
        if xml[index..].starts_with(b"<![CDATA[") {
            match find(xml, index, b"]]>") {
                Some(end) => index = end + 3,
                None => break,
            }
            continue;
        }
        if xml[index..].starts_with(b"<?") {
            match find(xml, index, b"?>") {
                Some(end) => index = end + 2,
                None => break,
            }
            continue;
        }
        let Some(close) = xml[index..].iter().position(|byte| *byte == b'>') else {
            break;
        };
        let end = index + close + 1;
        let inner = &xml[index + 1..end - 1];
        let closing = inner.first() == Some(&b'/');
        let body = if closing { &inner[1..] } else { inner };
        let self_closing = body.last() == Some(&b'/');
        let name_end = body
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || *byte == b'/')
            .unwrap_or(body.len());
        let name = String::from_utf8_lossy(&body[..name_end])
            .rsplit(':')
            .next()
            .unwrap_or("")
            .to_string();
        if !name.is_empty() {
            tags.push(Tag {
                name,
                start: index,
                end,
                closing,
                self_closing,
            });
        }
        index = end;
    }
    tags
}

fn find(xml: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    xml[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// Decode the entity forms Word writes in text runs.
pub fn decode_text(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_ref();
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        rest = &rest[index..];
        let entity = [
            ("&amp;", '&'),
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&apos;", '\''),
        ]
        .iter()
        .find(|(form, _)| rest.starts_with(form));
        match entity {
            Some((form, character)) => {
                out.push(*character);
                rest = &rest[form.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Encode text for a `w:t` body. Only what would break the XML is escaped.
pub fn encode_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// True when the text needs `xml:space="preserve"` to survive a round trip.
pub fn needs_space_preservation(text: &str) -> bool {
    text.starts_with(' ') || text.ends_with(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_carry_their_byte_ranges_and_names() {
        let xml = br#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>hi</w:t></w:r></w:p>"#;
        let tags = scan_tags(xml);
        let names: Vec<&str> = tags.iter().map(|tag| tag.name.as_str()).collect();
        assert_eq!(names, vec!["p", "r", "rPr", "b", "rPr", "t", "t", "r", "p"]);
        let first = &tags[0];
        assert_eq!(&xml[first.start..first.end], b"<w:p>");
        assert!(tags[3].self_closing, "w:b is self-closing");
        assert!(tags.iter().any(|tag| tag.closing && tag.name == "r"));
    }

    #[test]
    fn comments_and_cdata_are_not_markup() {
        let xml = b"<w:p><!-- <w:r/> --><w:t>x</w:t></w:p>";
        let names: Vec<String> = scan_tags(xml).into_iter().map(|tag| tag.name).collect();
        assert_eq!(names, vec!["p", "t", "t", "p"]);
    }

    #[test]
    fn entities_round_trip() {
        assert_eq!(decode_text(b"a &amp; b &lt;c&gt;"), "a & b <c>");
        assert_eq!(encode_text("a & b <c>"), "a &amp; b &lt;c&gt;");
        assert_eq!(
            decode_text(encode_text("中文 & <x>").as_bytes()),
            "中文 & <x>"
        );
        assert!(needs_space_preservation(" x"));
        // A trailing space is exactly what a round trip loses without it.
        assert!(needs_space_preservation("x "));
        assert!(!needs_space_preservation("x"));
    }
}
