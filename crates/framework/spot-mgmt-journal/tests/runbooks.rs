//! The runbooks cannot drift from the product (WP-RTB-04.T5): every `journal.*` operation id they
//! name is in this crate's catalogue, every `spot ops journal ...` command block parses, validates
//! and renders back to itself, a critical operation carries its confirmation flag, no block puts
//! a secret on a command line, every relative link resolves, and the condition names the pages
//! use exist in the sync conditions catalogue (`the table of the branch runbook lists every red one`).
//!
//! The checks are plain functions over text, and the last tests run them on bad text, so the
//! checks themselves are proven to fail. No database, no network.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use spot_mgmt_journal::build_catalogue;
use spot_mgmt_ops::{Catalogue, Risk, SECRET_PLACEHOLDER, Shell, parse::parse, render};

const PREFIX: &str = "journal";
const OTHER_PREFIX: &str = "sync";

const JOURNAL_RECOVERY: &str = include_str!("../../../../docs/runbooks/journal-recovery.md");
const BRANCH_NODE: &str = include_str!("../../../../docs/runbooks/branch-node-lifecycle.md");
const PAGES: [(&str, &str); 2] = [
    ("journal-recovery.md", JOURNAL_RECOVERY),
    ("branch-node-lifecycle.md", BRANCH_NODE),
];

fn runbooks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs/runbooks")
}

fn catalogue() -> Catalogue {
    build_catalogue().expect("the catalogue builds")
}

// ---- the checks (plain functions over text) ----

/// The `<prefix>.<segments>` tokens in `text`: the prefix at a word start, then at least one
/// more lower-case segment. A trailing dot (the end of a sentence) is not part of the token.
fn operation_ids(text: &str, prefix: &str) -> BTreeSet<String> {
    let needle = format!("{prefix}.");
    let bytes = text.as_bytes();
    let mut found = BTreeSet::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(&needle) {
        let start = from + at;
        from = start + needle.len();
        let before_ok = start == 0
            || !(bytes[start - 1].is_ascii_alphanumeric()
                || matches!(bytes[start - 1], b'.' | b'-' | b'_' | b'/'));
        if !before_ok {
            continue;
        }
        let mut end = from;
        while end < bytes.len()
            && (bytes[end].is_ascii_lowercase() || bytes[end] == b'_' || bytes[end] == b'.')
        {
            end += 1;
        }
        let token = text[start..end].trim_end_matches('.');
        if token.len() > needle.len() {
            found.insert(token.to_owned());
        }
    }
    found
}

fn unknown_ids(text: &str, catalogue: &Catalogue) -> Vec<String> {
    operation_ids(text, PREFIX)
        .into_iter()
        .filter(|id| !catalogue.iter().any(|spec| spec.id.as_str() == id))
        .collect()
}

/// The fenced code blocks of `text`, each as its lines.
fn fenced_blocks(text: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match current.take() {
                Some(block) => blocks.push(block),
                None => current = Some(Vec::new()),
            }
        } else if let Some(block) = current.as_mut() {
            block.push(line.trim_end().to_owned());
        }
    }
    blocks
}

/// Every command line in the fenced blocks that starts `spot ops <prefix>`.
fn command_lines(text: &str, prefix: &str) -> Vec<String> {
    let head = format!("spot ops {prefix} ");
    let bare = format!("spot ops {prefix}");
    fenced_blocks(text)
        .into_iter()
        .flatten()
        .map(|l| l.trim().to_owned())
        .filter(|l| l.starts_with(&head) || *l == bare)
        .collect()
}

/// POSIX word splitting for what `render` writes: bare words, single-quoted strings and the
/// `'\''` escape.
fn split(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err(format!("an unclosed quote in `{line}`")),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(
                    chars
                        .next()
                        .ok_or_else(|| format!("a dangling backslash in `{line}`"))?,
                );
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// `Ok(())` when `line` parses, validates and renders back to itself.
fn round_trips(line: &str, catalogue: &Catalogue) -> Result<(), String> {
    let args = split(line)?;
    let invocation =
        parse(catalogue, &args).map_err(|e| format!("`{line}` does not parse: {e}"))?;
    let validated = catalogue.validate(&invocation).map_err(|errors| {
        format!(
            "`{line}` does not validate: {}",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )
    })?;
    let again = render(&validated, Shell::Posix);
    if again == line {
        Ok(())
    } else {
        Err(format!(
            "`{line}` is not the canonical form; the product renders `{again}`"
        ))
    }
}

/// A critical operation's command must carry a confirmation flag (`--confirm...`).
fn confirmation_missing(line: &str, catalogue: &Catalogue) -> Option<String> {
    let args = split(line).ok()?;
    let invocation = parse(catalogue, &args).ok()?;
    let spec = catalogue.get(&invocation.op)?;
    let has_confirm_param = spec.params.iter().any(|p| p.name.starts_with("confirm"));
    if spec.risk == Risk::Critical
        && has_confirm_param
        && !args.iter().any(|a| a.starts_with("--confirm"))
    {
        return Some(format!(
            "`{line}` is critical and carries no --confirm flag"
        ));
    }
    None
}

/// A secret parameter must show only the placeholder.
fn secret_on_command_line(line: &str, catalogue: &Catalogue) -> Option<String> {
    let args = split(line).ok()?;
    let invocation = parse(catalogue, &args).ok()?;
    let spec = catalogue.get(&invocation.op)?;
    for param in spec.params.iter().filter(|p| p.secret) {
        let flag = format!("--{}", param.name.replace('_', "-"));
        if let Some(at) = args.iter().position(|a| *a == flag)
            && args.get(at + 1).map(String::as_str) != Some(SECRET_PLACEHOLDER)
        {
            return Some(format!(
                "`{line}` puts the secret `{}` on the command line",
                param.name
            ));
        }
    }
    None
}

/// Relative links in `text` that do not resolve against `dir`.
fn broken_links(text: &str, dir: &Path) -> Vec<String> {
    let mut broken = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        rest = &rest[at + 2..];
        let Some(end) = rest.find(')') else { break };
        let target = &rest[..end];
        let path = target.split('#').next().unwrap_or("");
        let external = target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("mailto:");
        if !external && !path.is_empty() && !dir.join(path).exists() {
            broken.push(target.to_owned());
        }
    }
    broken
}

/// Backticked UPPER_SNAKE words (condition names) in `text`.
fn condition_names(text: &str) -> BTreeSet<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|w| {
            w.len() > 3
                && w.bytes()
                    .all(|b| b.is_ascii_uppercase() || b == b'_' || b.is_ascii_digit())
        })
        .map(str::to_owned)
        .collect()
}

fn unknown_conditions(text: &str) -> Vec<String> {
    let known: BTreeSet<String> = spot_sync_conditions::entries()
        .into_iter()
        .map(|e| e.def.id.clone())
        .collect();
    condition_names(text)
        .into_iter()
        .filter(|n| !known.contains(n))
        .collect()
}

/// Red conditions of the catalogue that the table of `text` does not list.
fn red_conditions_missing(text: &str) -> Vec<String> {
    let listed = condition_names(text);
    spot_sync_conditions::entries()
        .into_iter()
        .filter(|e| e.def.severity == spot_conditions::Severity::Red)
        .map(|e| e.def.id.clone())
        .filter(|id| !listed.contains(id))
        .collect()
}

// ---- the runbooks ----

#[test]
fn every_operation_id_the_pages_name_is_in_the_catalogue() {
    let catalogue = catalogue();
    for (name, text) in PAGES {
        assert_eq!(
            unknown_ids(text, &catalogue),
            Vec::<String>::new(),
            "{name}"
        );
        // The other crate's ids are its own test's business.
        let _ = operation_ids(text, OTHER_PREFIX);
    }
}

#[test]
fn every_command_block_round_trips_to_the_canonical_form() {
    let catalogue = catalogue();
    let mut seen = 0;
    for (name, text) in PAGES {
        for line in command_lines(text, PREFIX) {
            seen += 1;
            if let Err(why) = round_trips(&line, &catalogue) {
                panic!("{name}: {why}");
            }
        }
    }
    assert!(
        seen >= 5,
        "only {seen} `spot ops {PREFIX}` command lines in the pages"
    );
}

#[test]
fn critical_commands_carry_their_confirmation_and_no_secret_is_on_a_line() {
    let catalogue = catalogue();
    let mut critical = 0;
    for (name, text) in PAGES {
        for line in command_lines(text, PREFIX) {
            assert_eq!(confirmation_missing(&line, &catalogue), None, "{name}");
            assert_eq!(secret_on_command_line(&line, &catalogue), None, "{name}");
            if let Ok(args) = split(&line)
                && let Ok(invocation) = parse(&catalogue, &args)
                && catalogue
                    .get(&invocation.op)
                    .is_some_and(|s| s.risk == Risk::Critical)
            {
                critical += 1;
            }
        }
    }
    assert!(critical >= 1, "the pages show {critical} critical commands");
}

#[test]
fn every_relative_link_resolves() {
    for (name, text) in PAGES {
        assert_eq!(
            broken_links(text, &runbooks_dir()),
            Vec::<String>::new(),
            "{name}"
        );
    }
}

#[test]
fn the_condition_names_exist_and_the_table_lists_every_red_one() {
    for (name, text) in PAGES {
        assert_eq!(unknown_conditions(text), Vec::<String>::new(), "{name}");
    }
    assert_eq!(red_conditions_missing(BRANCH_NODE), Vec::<String>::new());
}

// ---- the checks fail on bad text ----

#[test]
fn an_unknown_operation_id_is_rejected() {
    let catalogue = catalogue();
    let text =
        format!("Run `{PREFIX}.no_such_operation` first, then `{PREFIX}.status.` ends a sentence.");
    assert_eq!(
        unknown_ids(&text, &catalogue),
        vec![format!("{PREFIX}.no_such_operation")]
    );
}

#[test]
fn an_unparsable_or_non_canonical_command_is_rejected() {
    let catalogue = catalogue();
    assert!(
        round_trips(
            &format!("spot ops {PREFIX} no_such_operation --flag"),
            &catalogue
        )
        .is_err()
    );
    assert!(round_trips(&format!("spot ops {PREFIX} 'unclosed"), &catalogue).is_err());
    // Not the canonical spelling: the product renders single spaces and the flags in
    // declaration order with defaults filled in.
    let lines = command_lines(JOURNAL_RECOVERY, PREFIX);
    assert!(!lines.is_empty());
    let sloppy = lines[0].replace("spot ops", "spot  ops");
    assert!(round_trips(&sloppy, &catalogue).is_err());
}

#[test]
fn a_missing_confirmation_and_a_broken_link_are_rejected() {
    let catalogue = catalogue();
    let line = "spot ops journal rebuild run --target latest --mode full";
    assert!(confirmation_missing(line, &catalogue).is_some());
    let text = "See [nothing](no-such-page.md) and [web](https://example.invalid/x).";
    assert_eq!(
        broken_links(text, &runbooks_dir()),
        vec!["no-such-page.md".to_owned()]
    );
}

#[test]
fn an_unknown_condition_and_a_missing_red_one_are_rejected() {
    assert_eq!(
        unknown_conditions("| `NO_SUCH_CONDITION` | red |"),
        vec!["NO_SUCH_CONDITION".to_owned()]
    );
    assert!(!red_conditions_missing("| `NODE_TOO_OLD` | red |").is_empty());
}
