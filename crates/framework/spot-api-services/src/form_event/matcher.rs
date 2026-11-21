//! Whole-word matching of a hidden field's name or label in an error's text.
//!
//! `super::filter_error` promises that an error cannot say a hidden field exists, so the match
//! has to see a field however the error spelled it: `grand_total`, `grandTotal`, `Grand Total`,
//! `GRAND-TOTAL` and `grandTotal` are one field, while a short name such as `tax` must not
//! swallow an error about `taxonomy`.
//!
//! # The canonical key form
//!
//! Both sides are folded to one **key**: every word character, in order, with the separators
//! dropped. A word character is folded as NFKC would fold it, as far as a table in this module
//! reaches:
//!
//! * compatibility forms first — fullwidth Latin letters, digits and punctuation, the `ff`/`fi`
//!   ligatures, the Kelvin and Angstrom signs — so `ｇｒａｎｄ` is `grand`;
//! * then zero-width and combining marks are dropped **without** breaking the word, so
//!   `grand\u{200b}total` and `cafe\u{301}` fold onto `grandtotal` and `cafe`;
//! * then the diacritics NFKD would strip are removed, so `café` and `cafe` fold together;
//! * then the Cyrillic and Greek letters that render as Latin ones are mapped to their Latin
//!   twin, so a homoglyph spelling folds onto the name it imitates;
//! * and finally a simple case fold (one character in, one out), so the Greek final sigma
//!   `ς` folds onto `σ` and `ß` stays itself.
//!
//! A term matches when its key appears in the text's key **on word boundaries**: the match
//! starts where a word starts in the text (after a separator, or at a `camelCase` hump) and ends
//! where the text's next word starts or at the text's end. That is why `tax` does not match
//! `taxonomy`, and why the whole hidden-name check never depends on how a step spelled the name.
//!
//! NFKC itself needs a Unicode table crate, which this crate does not have (and the task allows
//! no new dependency), so the tables below cover the Latin compatibility forms, the Latin
//! diacritics of Latin-1 and Latin Extended-A, and the common Cyrillic and Greek homoglyphs.
//! Anything outside them folds only by case and separators.

/// The hidden names and labels, keyed once up front: one error is matched against every term,
/// and a term must not be re-tokenised for each of the error's texts.
pub(crate) struct Terms {
    keys: Vec<Vec<char>>,
}

impl Terms {
    /// The terms' keys, built once. A term with no word character is dropped: it would match
    /// every text.
    pub(crate) fn new<I, S>(terms: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            keys: terms
                .into_iter()
                .filter_map(|term| key(term.as_ref()).filter(|key| !key.is_empty()))
                .collect(),
        }
    }

    /// Whether there is no term to match.
    pub(crate) fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Whether `text` mentions any of the terms as a whole run of words.
    pub(crate) fn mentions(&self, text: &str) -> bool {
        if self.keys.is_empty() {
            return false;
        }
        let text = Key::of(text);
        self.keys.iter().any(|term| text.mentions(term))
    }
}

/// Whether `text` mentions `term` as a whole run of words, ignoring case, separators and the
/// look-alike spellings [`Terms`] folds together. The single-term form, for the tests.
#[cfg(test)]
pub(crate) fn mentions(text: &str, term: &str) -> bool {
    Terms::new([term]).mentions(text)
}

/// A text's key, with the offsets at which a word starts.
struct Key {
    /// The folded word characters of the text, separators dropped.
    characters: Vec<char>,
    /// Offsets in `characters` where a word starts, ascending.
    starts: Vec<usize>,
}

impl Key {
    /// The text's key. Empty when the text has no word character.
    fn of(text: &str) -> Self {
        let mut items = Vec::with_capacity(text.len());
        for character in text.chars() {
            push_folded(character, &mut items);
        }
        let mut characters = Vec::with_capacity(items.len());
        let mut starts = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let Item::Character(character) = item else {
                continue;
            };
            if starts_a_word(&items, index) {
                starts.push(characters.len());
            }
            characters.push(fold(*character));
        }
        Self { characters, starts }
    }

    /// Whether `term`'s key appears at a word boundary of this key.
    fn mentions(&self, term: &[char]) -> bool {
        if term.is_empty() || term.len() > self.characters.len() {
            return false;
        }
        self.starts.iter().any(|start| {
            let end = start + term.len();
            end <= self.characters.len()
                && self.characters[*start..end] == *term
                && (end == self.characters.len() || self.starts.binary_search(&end).is_ok())
        })
    }
}

/// The term's key: the same fold as [`Key`], with no boundaries (a term matches however its own
/// words are spelled and separated).
fn key(term: &str) -> Option<Vec<char>> {
    let mut items = Vec::with_capacity(term.len());
    for character in term.chars() {
        push_folded(character, &mut items);
    }
    let key: Vec<char> = items
        .iter()
        .filter_map(|item| match item {
            Item::Character(character) => Some(fold(*character)),
            Item::Break => None,
        })
        .collect();
    (!key.is_empty()).then_some(key)
}

/// One folded input character, or the separator between two words.
enum Item {
    /// A word character, compatibility-folded but still in its own case (the boundaries need it).
    Character(char),
    /// A separator: whitespace, punctuation, `_`, `[`, `]`, `.` and the like.
    Break,
}

/// Whether the word character at `index` starts a word.
///
/// A word starts after a separator, at a lowercase-to-uppercase hump (`grandTotal`), and inside a
/// run of capitals that ends in a lowercase letter (`HTTPServer`).
fn starts_a_word(items: &[Item], index: usize) -> bool {
    let Item::Character(character) = items[index] else {
        return false;
    };
    match index.checked_sub(1).map(|previous| &items[previous]) {
        None | Some(Item::Break) => true,
        Some(Item::Character(previous)) => {
            let hump = previous.is_lowercase() && character.is_uppercase();
            let acronym = previous.is_uppercase()
                && character.is_uppercase()
                && items.get(index + 1).is_some_and(
                    |next| matches!(next, Item::Character(next) if next.is_lowercase()),
                );
            hump || acronym
        }
    }
}

/// Fold `character` into `out`: the compatibility form, or the character itself, or a separator.
///
/// A character that is transparent to the match — a zero-width space, a joiner, a soft hyphen, a
/// combining mark — adds nothing and, in particular, does not separate two words: a zero-width
/// space inside `grandtotal` must not turn one word into two.
fn push_folded(character: char, out: &mut Vec<Item>) {
    // Fullwidth ASCII (`U+FF01..U+FF5E`) is ASCII plus a fixed offset.
    if let '\u{ff01}'..='\u{ff5e}' = character
        && let Some(ascii) = char::from_u32(u32::from(character) - 0xfee0)
    {
        push_latin(ascii, out);
        return;
    }
    if let Some(forms) = compatibility(character) {
        for form in forms.chars() {
            push_latin(form, out);
        }
        return;
    }
    if transparent(character) {
        return;
    }
    push_latin(character, out);
}

/// Push one folded character: a word character (as its Latin twin when it is a homoglyph), or the
/// separator a folded punctuation mark stands for.
fn push_latin(character: char, out: &mut Vec<Item>) {
    if let Some(latin) = confusable(character) {
        out.push(Item::Character(latin));
    } else if character.is_alphanumeric() {
        out.push(Item::Character(character));
    } else {
        out.push(Item::Break);
    }
}

/// The characters a compatibility form folds to, or `None` when the character is not one.
///
/// Besides the fullwidth range (handled by offset in [`push_folded`]), the table covers the
/// `ff`/`fi`/`fl` ligatures and the two compatibility letters that appear in real field names.
fn compatibility(character: char) -> Option<&'static str> {
    match character {
        '\u{fb00}' => Some("ff"),
        '\u{fb01}' => Some("fi"),
        '\u{fb02}' => Some("fl"),
        '\u{fb03}' => Some("ffi"),
        '\u{fb04}' => Some("ffl"),
        '\u{212a}' => Some("K"),
        '\u{212b}' => Some("A"),
        _ => None,
    }
}

/// Whether `character` is transparent to the match: dropped without separating two words.
fn transparent(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{200b}'..='\u{200d}'
            | '\u{2060}'
            | '\u{feff}'
            | '\u{0300}'..='\u{036f}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe20}'..='\u{fe2f}'
    )
}

/// Latin letters with diacritics, by the base letter NFKD decomposes them to.
///
/// One row per base letter; the forms are the lowercase spellings, because [`fold`] runs after
/// [`confusable`] and a simple case fold, so only lowercase forms reach the lookup.
const DIACRITICS: [(char, &str); 19] = [
    ('a', "àáâãäåāăą"),
    ('c', "çćĉċč"),
    ('d', "ďđ"),
    ('e', "èéêëēĕėęě"),
    ('g', "ĝğġģ"),
    ('h', "ĥħ"),
    ('i', "ìíîïĩīĭįı"),
    ('j', "ĵ"),
    ('k', "ķ"),
    ('l', "ĺļľł"),
    ('n', "ñńņňŉ"),
    ('o', "òóôõöøōŏő"),
    ('r', "ŕŗř"),
    ('s', "śŝşšș"),
    ('t', "ţťŧț"),
    ('u', "ùúûüũūŭůűų"),
    ('w', "ŵ"),
    ('y', "ýÿŷ"),
    ('z', "źżž"),
];

/// The Cyrillic and Greek letters that render as a Latin one, both cases.
const CONFUSABLES: [(char, char); 34] = [
    // Cyrillic.
    ('А', 'A'),
    ('В', 'B'),
    ('Е', 'E'),
    ('К', 'K'),
    ('М', 'M'),
    ('Н', 'H'),
    ('О', 'O'),
    ('Р', 'P'),
    ('С', 'C'),
    ('Т', 'T'),
    ('У', 'Y'),
    ('Х', 'X'),
    ('а', 'a'),
    ('е', 'e'),
    ('о', 'o'),
    ('р', 'p'),
    ('с', 'c'),
    ('у', 'y'),
    ('х', 'x'),
    ('і', 'i'),
    ('ј', 'j'),
    ('ѕ', 's'),
    ('ԁ', 'd'),
    ('һ', 'h'),
    ('ӏ', 'l'),
    // Greek.
    ('Α', 'A'),
    ('Β', 'B'),
    ('Ε', 'E'),
    ('Ι', 'I'),
    ('Κ', 'K'),
    ('Ο', 'O'),
    ('Τ', 'T'),
    ('Χ', 'X'),
    ('ο', 'o'),
];

/// The Latin letter `character` renders as, when it is a homoglyph.
fn confusable(character: char) -> Option<char> {
    CONFUSABLES
        .iter()
        .find_map(|(homoglyph, latin)| (*homoglyph == character).then_some(*latin))
}

/// The character's key form: its diacritic-stripped, simple-folded lowercase.
///
/// Case folding's one irregular simple mapping is the Greek final sigma, which folds onto the
/// ordinary sigma; `to_lowercase` leaves it alone.
fn fold(character: char) -> char {
    if character == 'ς' {
        return 'σ';
    }
    let mut lower = character.to_lowercase();
    let lowered = match (lower.next(), lower.next()) {
        (Some(folded), None) => folded,
        // A character whose lowercase form is longer than one character keeps its own form;
        // `İ` is the one that reaches this, and it has no key of its own.
        _ => character,
    };
    DIACRITICS
        .iter()
        .find_map(|(base, forms)| forms.contains(lowered).then_some(*base))
        .unwrap_or(lowered)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::mentions;

    /// `(text, term, whether the term is mentioned)`. Each row is one case the review asked for:
    /// a name with underscores, a label with spaces, a label with punctuation, a one- and a
    /// two-character name, and a name that is a prefix of a visible one.
    const CASES: [(&str, &str, bool); 15] = [
        // Underscored names, in the text's own spelling and in another case.
        ("The grand_total must not be negative.", "grand_total", true),
        ("GRAND_TOTAL is not a number.", "grand_total", true),
        // A label with spaces, matched from the name and from the label.
        ("Grand Total is not a number.", "grand_total", true),
        ("GRAND TOTAL is not a number.", "Grand Total", true),
        ("Grand_TOTAL is not a number.", "grand_total", true),
        // A label with punctuation, and the same words without it.
        ("Rate (per unit) is invalid.", "Rate (per unit)", true),
        ("Rate per unit is invalid.", "Rate (per unit)", true),
        // Paths: a table column is named through its path.
        ("The value of items[2].rate is stale.", "rate", true),
        ("The value of items.rate is stale.", "rate", true),
        // A one-character name matches that word and nothing that merely starts with it.
        ("The x column is required.", "x", true),
        ("The xyz column is required.", "x", false),
        // A two-character name, the same way.
        ("The id is missing.", "id", true),
        ("The idx is wrong.", "id", false),
        // A hidden `tax` does not swallow an error about a visible `taxonomy`.
        ("The taxonomy is closed.", "tax", false),
        ("The tax is closed.", "tax", true),
    ];

    #[test]
    fn a_term_matches_whole_words_whatever_the_case_and_separators() {
        for (text, term, expected) in CASES {
            assert_eq!(
                mentions(text, term),
                expected,
                "text {text:?}, term {term:?}"
            );
        }
    }

    #[test]
    fn a_term_without_a_word_character_matches_nothing() {
        for term in ["", " ", "_", "[2].", "( )"] {
            assert!(!mentions("The tax is closed.", term), "term {term:?}");
        }
    }

    #[test]
    fn a_longer_term_does_not_match_a_shorter_text() {
        assert!(!mentions("taxonomy", "the taxonomy"));
        assert!(mentions("the taxonomy", "taxonomy"));
    }

    #[test]
    fn folding_is_simple_and_one_character_wide() {
        // Greek final sigma folds onto sigma: simple folding, one character in and out.
        assert!(mentions("ΟΔΟΣ", "οδος"));
        // A character with no single-character lowercase form stays itself, so it cannot match
        // a different character.
        assert!(!mentions("straße", "strasse"));
        assert!(mentions("straße", "STRAßE"));
    }

    /// `(text, term, whether the term is mentioned)`, one row per look-alike form the review
    /// named: `camelCase` against a hidden `snake_case`, `kebab-case`, `Title Case`, fullwidth
    /// Latin letters and digits, zero-width characters inside a word, decomposed accents, and
    /// Cyrillic homoglyphs.
    const LOOK_ALIKE_CASES: [(&str, &str, bool); 18] = [
        // `camelCase`, from the name and from the label, in both directions.
        ("The grandTotal is stale.", "grand_total", true),
        ("The grandTotal is stale.", "Grand Total", true),
        ("The GRAND-TOTAL is stale.", "grand_total", true),
        ("The grand-total is stale.", "grand_total", true),
        ("The GrandTotal is stale.", "grand_total", true),
        ("The grandTotal is stale.", "grandTotal", true),
        // Fullwidth Latin letters and punctuation.
        ("The ｇｒａｎｄ＿ｔｏｔａｌ is stale.", "grand_total", true),
        ("The ＧＲＡＮＤ ＴＯＴＡＬ is stale.", "grand_total", true),
        ("The ｒａｔｅ２ column is stale.", "rate2", true),
        // Zero-width characters inside a word do not split it.
        ("The grand\u{200b}total is stale.", "grand_total", true),
        ("The grand\u{200b}Total is stale.", "Grand Total", true),
        ("The grand\u{feff}total is stale.", "grand_total", true),
        // Decomposed accents match the precomposed spelling, and the other way round.
        ("The caf\u{e9} is closed.", "cafe", true),
        ("The cafe\u{301} is closed.", "café", true),
        ("The caf\u{e9} is closed.", "cafe\u{301}", true),
        // Cyrillic homoglyphs of `a` and `o`.
        ("The grаnd_tоtal is stale.", "grand_total", true),
        // Folding does not turn a prefix into a match: a hidden `tax` stays out of `taxonomy`,
        // however the text spells it.
        ("The ｔａｘｏｎｏｍｙ is closed.", "tax", false),
        ("The taxa are closed.", "tax", false),
    ];

    #[test]
    fn a_term_matches_its_look_alike_spellings() {
        for (text, term, expected) in LOOK_ALIKE_CASES {
            assert_eq!(
                mentions(text, term),
                expected,
                "text {text:?}, term {term:?}"
            );
        }
    }

    /// A deterministic xorshift64: the fuzz-style test needs a repeatable sequence, and a
    /// dependency for one test would break the no-new-dependencies rule.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// A coin, for the case-form choices.
        fn flip(&mut self) -> bool {
            self.next() & 1 == 0
        }
    }

    /// One random case form of `word`: each character upper- or lowercase, chosen by `random`.
    fn case_form(word: &str, random: &mut Random) -> String {
        word.chars()
            .map(|character| {
                if random.flip() {
                    character.to_uppercase().collect::<String>()
                } else {
                    character.to_lowercase().collect::<String>()
                }
            })
            .collect()
    }

    /// One random separator between the words of a name.
    fn separator(random: &mut Random) -> &'static str {
        const CHOICES: [&str; 4] = ["_", "-", " ", ""];
        CHOICES[(random.next() % CHOICES.len() as u64) as usize]
    }

    #[test]
    fn every_case_form_of_a_hidden_name_is_mentioned() {
        const NAMES: [&str; 6] = [
            "grand_total",
            "grandTotal",
            "Grand Total",
            "Grand-Total",
            "rate",
            "private_note",
        ];
        let mut random = Random(0x5eed_1234_9abc_def1);
        for name in NAMES {
            // The name's words, whatever separator the name itself uses, and every way the
            // metadata may spell them back: one term per separator plus the name's own case.
            let words: Vec<&str> = name
                .split(['_', '-', ' '])
                .filter(|word| !word.is_empty())
                .collect();
            let mut terms: Vec<String> = vec![name.to_owned(), name.to_uppercase()];
            for separator in ["_", "-", " "] {
                terms.push(words.join(separator));
            }
            for _ in 0..64 {
                let spelled = words
                    .iter()
                    .map(|word| case_form(word, &mut random))
                    .collect::<Vec<String>>()
                    .join(separator(&mut random));
                let text = format!("The {spelled} column is stale.");
                for term in &terms {
                    assert!(
                        mentions(&text, term),
                        "text {text:?}, term {term:?}, name {name:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_hidden_name_is_mentioned_from_inside_a_longer_message() {
        // The canonical key is what the error's own text is folded to, so a name matches in a
        // sentence that also carries other fields' names.
        assert!(mentions(
            "customer and grandTotal and rate are stale",
            "grand_total"
        ));
        assert!(!mentions(
            "customer and grandTotal and rate are stale",
            "grand_totals"
        ));
    }
}
