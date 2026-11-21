//! Typst output escaping (WP-PLT-12; the path SPK-03 proved feeds Typst).
//!
//! For `.typ` templates every interpolated value is escaped so it can never
//! act as Typst markup or code. Typst's own character-escape syntax is used:
//! a backslash before a character inserts that character literally. The
//! escaped set mirrors the Typst 0.15 markup lexer:
//!
//! - escaped anywhere: `\` `#` `$` `*` `_` `@` `<` `>` `[` `]` `~` `'` `"`
//!   `` ` `` (code, math, emphasis, references, labels/content blocks,
//!   shorthands, smart quotes and raw blocks all start on these);
//! - escaped at the start of a line (string start or after a newline):
//!   `=` heading, `-` list, `+` enum, `/` term (and `/*` block comment),
//!   `:` term definition;
//! - escaped wherever they form a shorthand pair: `...` (each dot),
//!   `--` / `---` (each dash), `!?` / `?!`; and `*` `+` `-` before a digit
//!   (list/enum markers and the `5-min` style hyphen shorthands).
//!
//! Newlines themselves stay raw: inside a string they can only start a new
//! paragraph or line of text, which is data, not markup.

use minijinja::{Error, Output, State, Value};

/// The `AutoEscape::Custom` marker name used for Typst output.
pub(crate) const AUTO_ESCAPE_NAME: &str = "typst";

/// Characters escaped at every position in Typst content mode.
const ALWAYS_SPECIAL: &[char] = &[
    '\\', '#', '$', '*', '_', '@', '<', '>', '[', ']', '~', '\'', '"', '`',
];

/// Characters escaped at the start of a line in Typst content mode.
const LINE_START_SPECIAL: &[char] = &['=', '-', '+', '/', ':'];

/// Escapes `text` for inclusion as data in Typst content mode.
///
/// See the module docs for the exact rules; every returned `\X` is a Typst
/// character escape that renders as `X`, so the escaped text displays
/// exactly the original data and is inert against markup and code.
#[must_use]
pub(crate) fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut line_start = true;
    let mut numbered = false;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        let prev = if index == 0 {
            None
        } else {
            Some(chars[index - 1])
        };
        let next = chars.get(index + 1).copied();
        let escaped = ALWAYS_SPECIAL.contains(&character)
            || (line_start && LINE_START_SPECIAL.contains(&character))
            || (numbered && character == '.')
            || shorthand_char(prev, character, next);
        if escaped {
            out.push('\\');
        }
        out.push(character);
        // Typst recognises these markers after any leading blanks, and every Unicode
        // line break starts a new line, not just `\n`. `numbered` is "only a number since
        // the line start", so `12.` (a numbered item) is caught at its dot.
        let blank = matches!(character, ' ' | '\t');
        let digit = character.is_ascii_digit();
        let was_start = line_start;
        line_start = is_line_break(character) || (line_start && blank);
        numbered = (was_start || numbered) && digit;
        index += 1;
    }
    out
}

/// Characters Typst treats as a line break (`\n`, `\r`, VT, FF, NEL, LS, PS).
fn is_line_break(character: char) -> bool {
    matches!(
        character,
        '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// True when `character` sits inside a markup shorthand sequence (`...`,
/// `--`, `---`, `!?`, `?!`, `/*`) and must be escaped wherever it occurs.
fn shorthand_char(prev: Option<char>, character: char, next: Option<char>) -> bool {
    let dashes = |c: Option<char>| c == Some('-');
    let bangs = |c: Option<char>| matches!(c, Some('!') | Some('?'));
    match character {
        // `...` becomes the ellipsis character.
        '.' => prev == Some('.') || next == Some('.'),
        // `--` / `---` become en- and em-dashes; `-5` becomes the minus
        // sign; a lone dash before letters is text.
        '-' => dashes(prev) || dashes(next) || next.is_some_and(char::is_numeric),
        // `!?` and `?!` become the interrobang characters.
        '!' | '?' => bangs(prev) || bangs(next),
        // `//` opens a line comment and `/*` a block comment (anywhere in
        // markup); escape slashes that neighbour a comment opener.
        '/' => matches!(next, Some('*' | '/')) || prev == Some('/'),
        _ => false,
    }
}

/// Formats `value` for Typst output: its text, escaped for content mode.
///
/// Values explicitly marked safe (via the `safe` filter) pass through
/// verbatim, exactly like the HTML auto-escape contract.
pub(crate) fn format_value(out: &mut Output, _state: &State, value: &Value) -> Result<(), Error> {
    let text = value.to_string();
    if value.is_safe() {
        return out.write_str(&text).map_err(Into::into);
    }
    out.write_str(&escape_markup(&text)).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Escapes `text` for assertion.
    fn esc(text: &str) -> String {
        escape_markup(text)
    }

    /// True when `character` would be escaped mid-line in `context` text.
    fn escaped_mid_line(character: char) -> bool {
        // "a<character>b" is mid-line and pair-free.
        esc(&format!("a{character}b")) != format!("a{character}b")
    }

    #[test]
    fn every_always_special_character_is_escaped() {
        for character in ALWAYS_SPECIAL {
            assert_eq!(
                esc(&character.to_string()),
                format!("\\{character}"),
                "{character}"
            );
            assert!(escaped_mid_line(*character), "{character}");
        }
    }

    #[test]
    fn line_start_markers_are_escaped_only_at_line_start() {
        for character in LINE_START_SPECIAL {
            // string start counts as a line start
            assert_eq!(
                esc(&character.to_string()),
                format!("\\{character}"),
                "{character}"
            );
            assert_eq!(
                esc(&format!("a\n{character} b")),
                format!("a\n\\{character} b"),
                "after newline {character}"
            );
        }
        // mid-line they are ordinary text (no shorthand partner follows)
        assert_eq!(esc("a = b + c / d: e"), "a = b + c / d: e");
        assert_eq!(esc("x-y"), "x-y");
    }

    #[test]
    fn shorthand_sequences_are_escaped() {
        assert_eq!(esc("a...b"), "a\\.\\.\\.b");
        assert_eq!(esc("well--done"), "well\\-\\-done");
        assert_eq!(esc("---"), "\\-\\-\\-");
        assert_eq!(esc("what!?"), "what\\!\\?");
        assert_eq!(esc("/* comment */"), "\\/\\* comment \\*/");
        assert_eq!(esc("a // b"), "a \\/\\/ b");
        assert_eq!(esc("-5 minus"), "\\-5 minus");
        assert_eq!(esc("a-b"), "a-b");
        assert_eq!(esc("one dot . here"), "one dot . here");
    }

    #[test]
    fn markup_injections_are_inert() {
        // The brief's examples plus the full markup-trigger list.
        assert_eq!(esc("#import \"x\""), "\\#import \\\"x\\\"");
        assert_eq!(esc("$x$"), "\\$x\\$");
        assert_eq!(esc("*bold*"), "\\*bold\\*");
        assert_eq!(esc("_em_ @ref <lab>"), "\\_em\\_ \\@ref \\<lab\\>");
        assert_eq!(esc("[link]"), "\\[link\\]");
        assert_eq!(esc("= Heading"), "\\= Heading");
        assert_eq!(
            esc("- item\n+ item\n/ term: one"),
            "\\- item\n\\+ item\n\\/ term: one"
        );
        assert_eq!(esc("~short\\cut`raw`"), "\\~short\\\\cut\\`raw\\`");
        // escaped output, if it were ever pasted raw, displays the data
        assert!(
            !esc("#let x = 1").starts_with('#'),
            "code injection prevented"
        );
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(esc("Rs 1,234.56"), "Rs 1,234.56");
        assert_eq!(esc("Invoice 42"), "Invoice 42");
        assert_eq!(esc("total (net)"), "total (net)");
        assert_eq!(esc(""), "");
        assert_eq!(esc("two\nlines"), "two\nlines");
        assert_eq!(esc("3 / 4 = 0.75"), "3 / 4 = 0.75");
    }

    #[test]
    fn exhaustive_ascii_table() {
        // Every printable ASCII character has an explicit rule: escaped
        // always, escaped at line start only, or inert plain text.
        let mut covered = String::new();
        for code in 0x20u8..=0x7E {
            let character = char::from(code);
            covered.push(character);
            let always = ALWAYS_SPECIAL.contains(&character);
            let line_start_only = LINE_START_SPECIAL.contains(&character) && !always;
            let at_start = esc(&character.to_string());
            let mid_line = esc(&format!("a{character}b {character}c"));
            if always {
                assert_eq!(at_start, format!("\\{character}"), "{character} at start");
                assert!(
                    mid_line.contains(&format!("\\{character}")),
                    "{character} must escape mid-line: {mid_line:?}"
                );
            } else if line_start_only {
                assert_eq!(at_start, format!("\\{character}"), "{character} at start");
            } else if character != '-' {
                // '-' participates in dash-pair shorthands; other plain
                // characters must survive mid-line untouched.
                assert_eq!(
                    mid_line,
                    format!("a{character}b {character}c"),
                    "{character}"
                );
            }
        }
        assert_eq!(covered.len(), 95);
    }

    #[test]
    fn newline_positions_reset_line_start() {
        assert_eq!(esc("#a\n#b"), "\\#a\n\\#b");
        assert_eq!(esc("x=1\n=2"), "x=1\n\\=2");
        assert_eq!(esc("a\n=b\nc"), "a\n\\=b\nc");
    }

    #[test]
    fn line_markers_are_escaped_after_blanks_and_any_line_break() {
        assert_eq!(esc("x\n  - item"), "x\n  \\- item");
        assert_eq!(esc("x\n\t= head"), "x\n\t\\= head");
        assert_eq!(esc("x\n  + item"), "x\n  \\+ item");
        assert_eq!(esc("x\n  / term: d"), "x\n  \\/ term: d");
        for brk in ['\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}'] {
            assert_eq!(
                esc(&format!("x{brk}- a")),
                format!("x{brk}\\- a"),
                "{brk:?}"
            );
        }
        // numbered items: `1.` and `12.` at line start (after blanks) lose their dot marker
        assert_eq!(esc("1. one"), "1\\. one");
        assert_eq!(esc("a\n  12. twelve"), "a\n  12\\. twelve");
        // a number mid-line is ordinary text
        assert_eq!(esc("pay 3. then"), "pay 3. then");
        assert_eq!(esc("v1.2"), "v1.2");
    }
}
