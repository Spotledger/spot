//! Deterministic human labels: the one-line `label` fallback of the generated views
//! (brief deliverable 4). A field with no `label` shows the humanised storage name, so a
//! form never shows a bare `posting_date`.

/// Humanises a storage name: `posting_date` becomes "Posting date". Underscores separate
/// words (a run of them, `total__amount`, still collapses to one break; leading and
/// trailing separators add no space). The capitalisation is **sentence case**: the
/// label's first letter capitalises, and every other letter reads lowercase (`vat_no`
/// becomes "Vat no", not "Vat No"; shouting an abbreviation would need it declared,
/// never guessed). A digit run after letters opens a word of its own (`line2_note`
/// becomes "Line 2 note", `row10_total` keeps "10" one number), while a digit run that
/// *opens* the name rides with the letters that follow (`2nd_line` becomes "2nd line").
/// The transformation is pure text over the name — no field type, no context — so the
/// same name always humanises to the same label.
///
/// An empty name stays empty. Names are lowercase ids by spot-meta's own validation; any
/// other character rides through inside the word in progress.
#[must_use]
pub fn humanize_label(name: &str) -> String {
    // The words the name splits into, in order.
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for c in name.chars() {
        match c {
            '_' | ' ' | '-' | '.' => close_word(&mut words, &mut current),
            digit if digit.is_ascii_digit() => {
                // A digit run opens its own word when letters are in progress (`line2`
                // -> "line" + "2"), and gathers otherwise (`2nd` opens its word with the
                // 2).
                if current.chars().any(|c| c.is_ascii_alphabetic()) {
                    close_word(&mut words, &mut current);
                }
                current.push(digit);
            }
            letter if letter.is_ascii_alphabetic() => {
                // A letter gathers into the word in progress; that word splits away
                // first only when it is a digit run that opened behind letters
                // (`line2`-style) — checked by whether the word in progress is digits
                // only AND the words already written end with a letter word.
                if !current.is_empty()
                    && current.chars().all(|c| c.is_ascii_digit())
                    && words
                        .last()
                        .is_some_and(|w| w.chars().any(|c| c.is_ascii_alphabetic()))
                {
                    close_word(&mut words, &mut current);
                }
                current.push(letter.to_ascii_lowercase());
            }
            other => {
                // A character the metadata validator would have refused rides inside the
                // word in progress rather than becoming a word of its own.
                current.push(other);
            }
        }
    }
    close_word(&mut words, &mut current);
    // Sentence case: the label's first letter capitalises — but only when a letter
    // *opens* the label (`posting_date` -> "Posting date"; `vat_no` -> "Vat no"). A label
    // opening with a digit word stays lowercase (`2nd_line` -> "2nd line"): capitalising
    // `2nd`'s `n` would read "2Nd", which is neither a number nor a word.
    let mut joined = words.join(" ");
    let mut rest = joined.chars();
    if let Some(first) = rest.next()
        && first.is_ascii_alphabetic()
    {
        joined = first.to_ascii_uppercase().to_string() + rest.as_str();
    }
    joined
}

/// Pushes the word in progress (when there is one) onto `words` and clears it.
fn close_word(words: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        let word = std::mem::take(current);
        words.push(word);
    }
}

#[cfg(test)]
mod tests {
    use super::humanize_label as humanize;

    #[test]
    fn a_simple_name_gains_one_capital() {
        assert_eq!(humanize("posting_date"), "Posting date");
        assert_eq!(humanize("name"), "Name");
    }

    #[test]
    fn digits_break_words() {
        assert_eq!(humanize("line2_note"), "Line 2 note");
        assert_eq!(humanize("address_line_1"), "Address line 1");
        // A digit after a separator: the separator closes the word, the digit opens the
        // next one; no double space.
        assert_eq!(humanize("line_2"), "Line 2");
    }

    #[test]
    fn runs_of_underscores_collapse_to_one_break() {
        assert_eq!(humanize("total__amount"), "Total amount");
        assert_eq!(humanize("a__b"), "A b");
        assert_eq!(humanize("a_b__c___d"), "A b c d");
    }

    #[test]
    fn an_abbreviation_is_named_not_shouted() {
        assert_eq!(humanize("vat_no"), "Vat no");
        assert_eq!(humanize("qty"), "Qty");
        // An input already carrying capitals normalises; the first letter keeps the cap.
        assert_eq!(humanize("VAT_no"), "Vat no");
    }

    #[test]
    fn leading_and_trailing_separators_do_not_add_spaces() {
        assert_eq!(humanize("_lead"), "Lead");
        assert_eq!(humanize("trail_"), "Trail");
        assert_eq!(humanize("__both__"), "Both");
    }

    #[test]
    fn an_empty_or_separator_only_name_has_no_words() {
        assert_eq!(humanize(""), "");
        assert_eq!(humanize("_"), "");
        assert_eq!(humanize("__"), "");
    }

    #[test]
    fn digit_runs_after_letters_open_their_own_word() {
        assert_eq!(humanize("row10_total"), "Row 10 total");
        assert_eq!(humanize("address1_line2_note3"), "Address 1 line 2 note 3");
    }

    #[test]
    fn a_leading_digit_rides_with_the_letters_that_follow_it() {
        assert_eq!(humanize("2nd_line"), "2nd line");
    }
}
