//! Minimal HTML-to-text conversion for [`super::web_fetch`].
//!
//! Dependency-free extraction of the readable text: script/style/head/svg
//! subtrees are dropped, block boundaries become newlines, list items are
//! prefixed with `- `, entities are decoded, and whitespace is collapsed.
//! It is intentionally not a layout engine — just enough structure for an LLM
//! to read a page.

const SKIP_ELEMENTS: &[&str] = &[
    "script", "style", "noscript", "template", "head", "svg", "title", "iframe", "canvas",
];

const MAX_TAG_SCAN_BYTES: usize = 1024;
const MAX_ENTITY_BYTES: usize = 12;

const BLOCK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "div",
    "dl",
    "dd",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "html",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "tfoot",
    "thead",
    "tr",
    "ul",
];

/// Convert an HTML document to readable plain text.
///
/// Scanning jumps between markup via substring search (memchr-backed) and
/// copies text runs wholesale, so plain-text bytes never go through per-char
/// dispatch — fetched pages are up to the 2 MB download cap.
pub fn html_to_text(input: &str) -> String {
    let mut text = String::with_capacity(input.len() / 2);
    let mut rest = input;
    let mut at_line_start = true;
    let mut next_tag_end = input.find('>');

    'page: while let Some(mark) = {
        let offset = input.len() - rest.len();
        while let Some(end) = next_tag_end.filter(|end| *end < offset) {
            next_tag_end = input[end + 1..]
                .find('>')
                .map(|relative| end + 1 + relative);
        }
        rest.find(['<', '&'])
    } {
        let (chunk, tail) = rest.split_at(mark);
        // Bulk-copy the text run before the markup.
        if !chunk.is_empty() {
            text.push_str(chunk);
            if chunk.chars().any(|c| !c.is_whitespace()) {
                at_line_start = false;
            }
        }

        if let Some(after_ampersand) = tail.strip_prefix('&') {
            match decode_entity(tail) {
                Some((decoded, consumed)) => {
                    text.push(decoded);
                    at_line_start = false;
                    rest = &tail[consumed..];
                }
                None => {
                    text.push('&');
                    rest = after_ampersand;
                }
            }
            continue;
        }

        if tail.starts_with("<!--") {
            // Comment: drop up to `-->`; an unterminated comment drops the
            // rest of the input.
            match tail.find("-->") {
                Some(end) => rest = &tail[end + 3..],
                None => {
                    rest = "";
                    break;
                }
            }
            continue;
        }

        if next_tag_end.is_none() {
            text.push('<');
            at_line_start = false;
            rest = &tail[1..];
            continue;
        }

        let Some(tag) = ParsedTag::parse(tail) else {
            // Stray `<` (not a tag): keep it as literal text.
            text.push('<');
            at_line_start = false;
            rest = &tail[1..];
            continue;
        };

        if matches_any(tag.name, SKIP_ELEMENTS) {
            if tag.is_closing {
                rest = &tail[tag.end..];
                continue;
            }
            // Drop the whole subtree: jump to its closing tag. An
            // unterminated element drops the rest of the input.
            let mut scan = &tail[tag.end..];
            loop {
                match scan.find('<') {
                    None => {
                        rest = "";
                        break 'page;
                    }
                    Some(position) => {
                        if is_closing_tag(&scan[position..], tag.name) {
                            match scan[position..].find('>') {
                                Some(end) => {
                                    rest = &scan[position + end + 1..];
                                    continue 'page;
                                }
                                None => {
                                    rest = "";
                                    break 'page;
                                }
                            }
                        }
                        scan = &scan[position + 1..];
                    }
                }
            }
        }

        if tag.name.eq_ignore_ascii_case("br") && !tag.is_closing {
            // `<br><br>` is a paragraph gap; keep every break.
            text.push('\n');
            at_line_start = true;
        } else if tag.name.eq_ignore_ascii_case("li") && !tag.is_closing {
            if !at_line_start {
                text.push('\n');
            }
            text.push_str("- ");
            at_line_start = false;
        } else if matches_any(tag.name, BLOCK_ELEMENTS) {
            // Runs of block boundaries collapse to one blank line later.
            text.push('\n');
            at_line_start = true;
        }
        rest = &tail[tag.end..];
    }
    text.push_str(rest);
    collapse_whitespace(&text)
}

/// Whether `name` matches any element in `list`, case-insensitively.
fn matches_any(name: &str, list: &[&str]) -> bool {
    list.iter()
        .any(|element| element.eq_ignore_ascii_case(name))
}

/// A tag parsed from a slice that starts at its `<`.
struct ParsedTag<'a> {
    /// Element name as written (`div`, `BR`, …); matching is case-insensitive.
    name: &'a str,
    /// `</name>` rather than `<name>`.
    is_closing: bool,
    /// Byte index just past the closing `>`, relative to the slice start.
    end: usize,
}

impl<'a> ParsedTag<'a> {
    /// Parse the tag starting at the slice's leading `<`; `None` for stray `<`
    /// sequences that do not open a real tag (e.g. a lone `<` or `<!`).
    fn parse(starting_at_open_bracket: &'a str) -> Option<Self> {
        let rest = &starting_at_open_bracket[1..];
        let is_closing = rest.starts_with('/');
        let body = rest.trim_start_matches('/');
        let name_len = body
            .bytes()
            .take(MAX_TAG_SCAN_BYTES)
            .position(|b| b == b'>' || b == b'/' || b.is_ascii_whitespace())
            .or_else(|| (body.len() <= MAX_TAG_SCAN_BYTES).then_some(body.len()))?;
        let name = &body[..name_len];
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        // Find the end of the tag, tolerating quoted attribute values
        // containing `>`.
        let mut quote: Option<char> = None;
        let end = rest
            .char_indices()
            .take_while(|(j, _)| *j < MAX_TAG_SCAN_BYTES)
            .find_map(|(j, c)| match quote {
                Some(q) if c == q => {
                    quote = None;
                    None
                }
                Some(_) => None,
                None => match c {
                    '"' | '\'' => {
                        quote = Some(c);
                        None
                    }
                    '>' => Some(j + 2), // past `>`, relative to the leading `<`
                    _ => None,
                },
            })?;
        Some(Self {
            name,
            is_closing,
            end,
        })
    }
}

/// Whether `candidate`, starting at its `<`, is `</{name}>` (case-insensitive).
fn is_closing_tag(candidate: &str, name: &str) -> bool {
    let Some(body) = candidate.strip_prefix("</") else {
        return false;
    };
    let name_end = body
        .find(|c: char| c == '>' || c.is_whitespace())
        .unwrap_or(body.len());
    body[..name_end].eq_ignore_ascii_case(name)
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut newlines = 0usize;
    let mut content_end = 0usize; // byte length of out ignoring trailing whitespace
    for c in text.chars() {
        match c {
            '\n' => {
                pending_space = false;
                newlines += 1;
            }
            ' ' | '\t' | '\r' => {
                // Skip indentation after a newline and at the very start.
                if !out.is_empty() && !out.ends_with('\n') {
                    pending_space = true;
                }
            }
            _ => {
                if out.is_empty() {
                    // Drop leading breaks/indentation without allocating
                    // a trimmed copy afterwards.
                    newlines = 0;
                    pending_space = false;
                }
                if newlines > 0 {
                    out.push_str(if newlines >= 2 { "\n\n" } else { "\n" });
                    newlines = 0;
                } else if pending_space {
                    out.push(' ');
                }
                pending_space = false;
                out.push(c);
                content_end = out.len();
            }
        }
    }
    out.truncate(content_end);
    out
}

/// Decode one HTML entity at the start of `s` (which begins with `&`),
/// returning the character and the bytes it consumed (through `;`).
/// Numeric references that resolve to control characters are rejected (they
/// would inject NULs / escape sequences into the output).
fn decode_entity(s: &str) -> Option<(char, usize)> {
    let end = s
        .as_bytes()
        .iter()
        .take(MAX_ENTITY_BYTES + 1)
        .position(|byte| *byte == b';')?;
    if end == 0 || end > 10 {
        return None;
    }
    let body = &s[1..end];
    if let Some(num) = body.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        let decoded = char::from_u32(code).filter(|c| !c.is_control())?;
        return Some((decoded, end + 1));
    }
    let decoded = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" | "rsquo" => '\'',
        "ldquo" | "rdquo" => '"',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "laquo" => '«',
        "raquo" => '»',
        _ => return None,
    };
    Some((decoded, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_style_and_comments() {
        let html = r#"<html><head><title>T</title><style>.a{color:red}</style></head>
            <body><!-- note --><script>if (1 < 2) alert("x")</script>
            <p>Hello</p><style>a > b {}</style></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Hello"), "got: {text:?}");
        assert!(!text.contains("alert"));
        assert!(!text.contains("color"));
        assert!(!text.contains("note"));
    }

    #[test]
    fn block_tags_break_lines() {
        let text = html_to_text("<h1>Title</h1><p>One</p><p>Two</p>");
        assert_eq!(text, "Title\n\nOne\n\nTwo");
    }

    #[test]
    fn lists_become_dashes() {
        let text = html_to_text("<ul><li>Alpha</li><li>Beta</li></ul>");
        assert_eq!(text, "- Alpha\n- Beta");
    }

    #[test]
    fn entities_decode() {
        let text = html_to_text("<p>a &amp; b &lt;c&gt; &quot;d&quot; &#65;&#x42;</p>");
        assert_eq!(text, "a & b <c> \"d\" AB");
    }

    #[test]
    fn whitespace_collapses() {
        let text = html_to_text("<p>  spaced   out\ttext  </p><div>\n\n more </div>");
        assert_eq!(text, "spaced out text\n\nmore");
    }

    #[test]
    fn line_breaks_and_inline_tags() {
        let text = html_to_text("<p>a<br>b <strong>bold</strong> c</p>");
        assert_eq!(text, "a\nb bold c");
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(html_to_text("just text & nothing"), "just text & nothing");
    }

    #[test]
    fn tag_names_are_case_insensitive() {
        let text = html_to_text("<DIV>One</DIV><P>Two</P><BR>");
        assert_eq!(text, "One\n\nTwo");
    }

    #[test]
    fn self_closing_and_quoted_angle_brackets() {
        // `>` inside a quoted attribute must not end the tag.
        let text = html_to_text("<p><a href=\"a>b\" title='c>d'>link</a><br/>tail</p>");
        assert_eq!(text, "link\ntail");
    }

    #[test]
    fn script_with_attributes_is_skipped() {
        let html = r#"<p>keep</p><script type="text/javascript" defer>var a = "</p>";</script><p>also keep</p>"#;
        let text = html_to_text(html);
        assert_eq!(text, "keep\n\nalso keep");
    }

    #[test]
    fn stray_angle_brackets_do_not_crash() {
        // Inline text before a block boundary gets a single break; a blank
        // line only appears between two block boundaries.
        assert_eq!(html_to_text("a < b < 3 <p>c</p>"), "a < b < 3\nc");
        assert_eq!(html_to_text(""), "");
        assert_eq!(html_to_text("<"), "<");
        assert_eq!(html_to_text("<!-- unclosed comment"), "");
    }

    #[test]
    fn numeric_entity_edges() {
        // Control characters are rejected and left as literal text.
        assert_eq!(html_to_text("a&#0;b"), "a&#0;b");
        // Out-of-range code points do not decode.
        assert_eq!(html_to_text("a&#x110000;b"), "a&#x110000;b");
        // Unknown named entities stay literal.
        assert_eq!(html_to_text("a&nope;b"), "a&nope;b");
        // Maximum accepted length is honored.
        assert_eq!(html_to_text("&#65;"), "A");
    }

    #[test]
    fn double_line_breaks_keep_paragraph_gap() {
        assert_eq!(html_to_text("<p>a</p><br><br><p>b</p>"), "a\n\nb");
        assert_eq!(html_to_text("a<br><br>b"), "a\n\nb");
    }

    #[test]
    fn hostile_unclosed_markup_and_entities_have_bounded_scans() {
        let unclosed_tags = "<a ".repeat(100_000);
        let unclosed_quotes = format!("<a \"{}>", "< ".repeat(100_000));
        let entities = "&".repeat(100_000);
        let started = std::time::Instant::now();

        assert_eq!(html_to_text(&unclosed_tags), unclosed_tags.trim_end());
        assert!(!html_to_text(&unclosed_quotes).is_empty());
        assert_eq!(html_to_text(&entities), entities);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "hostile HTML scans took {:?}",
            started.elapsed()
        );
    }
}
