//! The plain-text alternative derived from an HTML-only mail. The rule is small and fixed:
//!
//! 1. `script` and `style` elements are removed with their content.
//! 2. A closing `p`, `div`, `tr`, `li`, `h1`-`h6`, `table` tag and every `br` tag become a line
//!    break; a closing `td` or `th` tag becomes a tab.
//! 3. Every other tag is dropped; its text stays.
//! 4. The entities `&amp; &lt; &gt; &quot; &apos; &#39; &nbsp;` and numeric references
//!    (`&#233;`, `&#xE9;`) are decoded; an unknown entity stays as written.
//! 5. Spaces and tabs at the end of a line are removed, three or more line breaks become two,
//!    and the text is trimmed.

/// Derives the text alternative of `html`.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let Some(end) = html[i..].find('>') else {
                // An unclosed tag: the rest is markup noise, drop it.
                break;
            };
            let tag = &html[i + 1..i + end];
            i += end + 1;
            let (closing, name) = tag_name(tag);
            if !closing && (name == "script" || name == "style") {
                i = skip_element(html, i, &name);
                continue;
            }
            match (closing, name.as_str()) {
                (_, "br") => out.push('\n'),
                (
                    true,
                    "p" | "div" | "tr" | "li" | "table" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6",
                ) => out.push('\n'),
                (true, "td" | "th") => out.push('\t'),
                _ => {}
            }
        } else if bytes[i] == b'&' {
            match decode_entity(&html[i..]) {
                Some((text, used)) => {
                    out.push_str(&text);
                    i += used;
                }
                None => {
                    out.push('&');
                    i += 1;
                }
            }
        } else {
            // Copy up to the next markup character in one go (valid UTF-8 boundaries: the
            // markers are ASCII).
            let next = html[i..]
                .find(['<', '&'])
                .map_or(html.len(), |offset| i + offset);
            out.push_str(&html[i..next]);
            i = next;
        }
    }
    tidy(&out)
}

/// The lowercase tag name and whether it is a closing tag.
fn tag_name(tag: &str) -> (bool, String) {
    let tag = tag.trim_start();
    let (closing, rest) = match tag.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, tag),
    };
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    (closing, name)
}

/// The index just after the closing tag of `name`, or the end of the text.
fn skip_element(html: &str, from: usize, name: &str) -> usize {
    let needle = format!("</{name}");
    let lower = html[from..].to_ascii_lowercase();
    match lower.find(&needle) {
        Some(offset) => {
            let start = from + offset;
            html[start..]
                .find('>')
                .map_or(html.len(), |close| start + close + 1)
        }
        None => html.len(),
    }
}

/// Decodes the entity at the start of `text` (which begins with `&`): the replacement and how
/// many bytes it used.
fn decode_entity(text: &str) -> Option<(String, usize)> {
    let end = text.find(';')?;
    if end > 10 {
        return None;
    }
    let name = &text[1..end];
    let decoded = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let digits = name.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse::<u32>().ok()?,
            };
            char::from_u32(code)?
        }
    };
    Some((decoded.to_string(), end + 1))
}

fn tidy(text: &str) -> String {
    let mut lines: Vec<&str> = text
        .lines()
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect();
    // Three or more line breaks become two: at most one empty line in a row.
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in lines.drain(..) {
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_entities_and_blocks() {
        let html = "<html><head><style>p{color:red}</style></head><body><p>Hello &amp; welcome,<br>Ayesha &#233;</p><script>alert(1)</script><table><tr><td>a</td><td>b</td></tr></table></body></html>";
        assert_eq!(html_to_text(html), "Hello & welcome,\nAyesha \u{e9}\na\tb");
    }

    #[test]
    fn unknown_entities_and_stray_markers_survive() {
        assert_eq!(html_to_text("a &unknown; b & c"), "a &unknown; b & c");
        assert_eq!(html_to_text("x < y"), "x");
    }

    #[test]
    fn blank_runs_collapse() {
        assert_eq!(html_to_text("<p>a</p><p></p><p></p><p>b</p>"), "a\n\nb");
    }
}
