//! The restricted markdown subset of a `text` block.

/// What a valid `text` block refers to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Markdown {
    /// The destinations its `[text](dest:...)` links name, in source order.
    pub(crate) destinations: Vec<String>,
}

/// Why a `text` block is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MarkdownError {
    /// Markup, an image or a non-destination link.
    Markup,
}

/// Checks the subset and collects destination references.
pub(crate) fn check(source: &str) -> Result<Markdown, MarkdownError> {
    if has_markup(source) {
        return Err(MarkdownError::Markup);
    }
    Ok(Markdown {
        destinations: destinations(source),
    })
}

/// `<` followed by a letter, `/`, `!` or `?` is markup; `![` is an image.
/// Any `](` that does not open a `dest:` link, any `]:` (a reference-style
/// link definition, wherever it sits) and any `][` (a reference-style link)
/// is refused: the contract allows no link except `dest:`, so this fails
/// closed.
fn has_markup(source: &str) -> bool {
    let bytes = source.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'<' => {
                if let Some(next) = bytes.get(index + 1)
                    && (next.is_ascii_alphabetic() || matches!(next, b'/' | b'!' | b'?'))
                {
                    return true;
                }
            }
            b'!' if bytes.get(index + 1) == Some(&b'[') => return true,
            b']' if bytes.get(index + 1) == Some(&b'(')
                && !source[index + 2..].starts_with("dest:") =>
            {
                return true;
            }
            b']' if matches!(bytes.get(index + 1), Some(b':' | b'[')) => return true,
            _ => {}
        }
    }
    false
}

/// Every `](dest:...)` target, in order.
fn destinations(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = source;
    while let Some(at) = rest.find("](dest:") {
        let target = &rest[at + "](dest:".len()..];
        let end = target.find(')').unwrap_or(target.len());
        found.push(target[..end].to_owned());
        rest = &target[end..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_subset_passes() {
        let markdown = check("Open the [task list](dest:sample.tasks) to see more.")
            .unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(markdown.destinations, vec!["sample.tasks".to_owned()]);
    }

    #[test]
    fn emphasis_lists_and_code_are_plain_text() {
        for source in [
            "**bold** and *italic* and _also italic_",
            "- one\n- two\n- three",
            "1. one\n2. two",
            "run `cargo test` first",
            "a paragraph\n\nand a second",
            "less than < 3 is fine",
        ] {
            assert_eq!(check(source), Ok(Markdown::default()), "{source}");
        }
    }

    #[test]
    fn markup_images_and_other_links_are_refused() {
        for source in [
            "<script>alert(1)</script>",
            "a <b>bold</b> word",
            "<!-- comment -->",
            "![x](https://evil.example/p.png)",
            "[out](https://evil.example)",
            "[out](/sample/page)",
            "stray ](nowhere)",
            "[x][r]\n\n[r]: https://evil.example",
            "[r]: javascript:alert(1)",
        ] {
            assert_eq!(check(source), Err(MarkdownError::Markup), "{source}");
        }
    }

    #[test]
    fn reference_definitions_are_refused_wherever_they_sit() {
        for source in [
            "[r]: https://evil.example",
            "   [r]: https://evil.example",
            "text first [r]: https://evil.example",
            "para\n\n  [r]:/relative",
            "[a b-c_d!#$%&]: https://evil.example",
            "[R]: HTTPS://EVIL.EXAMPLE",
            "[r]:\n  https://evil.example",
            "- item\n- [r]: https://evil.example",
            "> [r]: https://evil.example",
            "> - [r]: https://evil.example",
            "[r]: dest:sample.a",
            "[x][r]",
            "[x][]\n\n[x]: dest:sample.a",
        ] {
            let result = check(source);
            assert_eq!(result, Err(MarkdownError::Markup), "{source}");
        }
    }

    #[test]
    fn destination_links_and_brackets_stay_legal() {
        let markdown =
            check("see [a](dest:sample.a), [b](dest:other.b) and [bracketed] text; x]y: z")
                .unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(
            markdown.destinations,
            vec!["sample.a".to_owned(), "other.b".to_owned()]
        );
    }

    #[test]
    fn destination_targets_are_collected_in_order() {
        let markdown = check("[a](dest:sample.a) then [b](dest:other.b)")
            .unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(
            markdown.destinations,
            vec!["sample.a".to_owned(), "other.b".to_owned()]
        );
    }
}
