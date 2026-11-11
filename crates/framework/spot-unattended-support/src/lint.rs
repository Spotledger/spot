//! Style rules for help text: short sentences, plain words, no stack traces.

use crate::help::Help;

/// The longest sentence a help text may have, in words.
pub const MAX_SENTENCE_WORDS: usize = 30;
/// The most steps one entry may list.
pub const MAX_STEPS: usize = 6;

/// Words and phrases that mean nothing to the person reading the help. Matched on whole
/// words, case-insensitively.
pub const JARGON: &[&str] = &[
    "backtrace",
    "stacktrace",
    "stack trace",
    "segfault",
    "null pointer",
    "nullptr",
    "panic",
    "panicked",
    "deserialise",
    "deserialize",
    "mutex",
    "regex",
    "todo",
    "fixme",
    "tbd",
    "sql",
    "uuid",
    "stderr",
    "stdout",
    "errno",
];

/// Every style problem in `help`, one line each; empty when the entry is clean.
#[must_use]
pub fn lint_help(help: &Help) -> Vec<String> {
    let mut problems = Vec::new();
    check_text("what_happened", &help.what_happened, &mut problems);
    check_text("why", &help.why, &mut problems);
    if help.what_to_do.is_empty() {
        problems.push("what_to_do has no steps".to_owned());
    }
    if help.what_to_do.len() > MAX_STEPS {
        problems.push(format!("what_to_do has more than {MAX_STEPS} steps"));
    }
    for (i, step) in help.what_to_do.iter().enumerate() {
        check_text(&format!("what_to_do step {}", i + 1), step, &mut problems);
    }
    problems
}

fn check_text(field: &str, text: &str, problems: &mut Vec<String>) {
    if text.trim().is_empty() {
        problems.push(format!("{field} is empty"));
        return;
    }
    if text.contains('{') || text.contains('}') {
        problems.push(format!("{field} has an unfilled placeholder"));
    }
    if looks_like_stack_trace(text) {
        problems.push(format!("{field} looks like a stack trace or a source path"));
    }
    for word in jargon_in(text) {
        problems.push(format!("{field} uses the jargon word \"{word}\""));
    }
    for sentence in sentences(text) {
        let words = sentence.split_whitespace().count();
        if words > MAX_SENTENCE_WORDS {
            problems.push(format!(
                "{field} has a sentence of {words} words (limit {MAX_SENTENCE_WORDS})"
            ));
        }
    }
}

fn sentences(text: &str) -> impl Iterator<Item = &str> {
    text.split(['.', '!', '?'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn looks_like_stack_trace(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("stack backtrace")
        || lower.contains(".rs:")
        || lower.contains("src/")
        || lower.contains("thread '")
        || lower.contains("::")
        || lower.contains("0x")
        || lower.contains("\n   at ")
}

fn jargon_in(text: &str) -> Vec<&'static str> {
    let lower = text.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    JARGON
        .iter()
        .copied()
        .filter(|term| {
            let parts: Vec<&str> = term.split(' ').collect();
            words.windows(parts.len()).any(|w| w == parts.as_slice())
        })
        .collect()
}
