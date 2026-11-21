//! The app author guide is tested (WP-UI-13.T4).
//!
//! `docs/running-spot/ui_extensions.md` marks every example it shows. This file extracts
//! them and runs each through `spot_ui_ext::validate` against the same facts as the contract's own
//! corpus (`support::sample_contribution`: the sample app's context, so an example may use only
//! the methods `task.start_work`, `task.close_task` and `project.archive`, the queries
//! `sample.open_tasks`, `sample.task_count` and `sample.recent_time_entries`, the destinations
//! `sample.projects`, `sample.tasks`, `sample.time_entries` and `sample.reports`, the route
//! `/sample/reports`, and the permissions of `context.json`).
//!
//! Markers, each on a line of its own and followed by a fenced block (blank lines may separate them):
//!
//! - `<!-- ui-example: valid file=ui.toml -->`: the block validates with no finding; the file name
//!   decides TOML or JSON;
//! - `<!-- ui-example: invalid code=uiext.unknown_slot file=ui.json -->`: the block fails with
//!   exactly that one code and no other;
//! - `<!-- ui-example: fragment -->`: shown, not run; the next prose line must begin "Not run:" and
//!   say why.
//!
//! A `json` or `toml` block with no marker is an error: nothing on the page is untested by accident.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use spot_ui_ext::{UiExtError, validate};

/// What a marker asks of its block.
#[derive(Debug, PartialEq, Eq)]
enum Marker {
    Valid { file: String },
    Invalid { code: String, file: String },
    Fragment,
}

/// One marked block.
#[derive(Debug, PartialEq, Eq)]
struct Example {
    /// 1-based line of the marker.
    line: usize,
    marker: Marker,
    /// The block's lines, each followed by a newline.
    text: String,
    /// The first non-blank line after the closing fence.
    after: Option<String>,
}

/// What a page holds: its marked blocks, the unmarked `json`/`toml` blocks, and malformed markers.
#[derive(Debug, Default, PartialEq, Eq)]
struct Extracted {
    examples: Vec<Example>,
    /// 1-based lines of fenced `json` or `toml` blocks that carry no marker.
    unmarked: Vec<usize>,
    /// Problems with the markers themselves, each naming its line.
    problems: Vec<String>,
}

fn repo_root() -> PathBuf {
    support::contracts_dir().join("../..")
}

fn read(path: PathBuf) -> String {
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn page() -> String {
    read(repo_root().join("docs/running-spot/ui_extensions.md"))
}

fn catalogue_codes() -> BTreeSet<String> {
    read(repo_root().join("crates/framework/spot-errors/catalogue/uiext.toml"))
        .lines()
        .filter_map(|line| line.trim().strip_prefix("code = \""))
        .filter_map(|rest| rest.split('"').next())
        .filter(|code| code.starts_with("uiext."))
        .map(str::to_owned)
        .collect()
}

/// `None` when the line is not a `ui-example` marker.
fn parse_marker(line: &str) -> Option<Result<Marker, String>> {
    let inner = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim();
    let rest = inner.strip_prefix("ui-example:")?.trim();
    let mut words = rest.split_whitespace();
    let kind = words.next().unwrap_or("");
    let mut file = None;
    let mut code = None;
    for word in words {
        match word.split_once('=') {
            Some(("file", value)) => file = Some(value.to_owned()),
            Some(("code", value)) => code = Some(value.to_owned()),
            _ => return Some(Err(format!("the marker has an unknown word `{word}`"))),
        }
    }
    Some(match (kind, file, code) {
        ("valid", Some(file), None) => Ok(Marker::Valid { file }),
        ("invalid", Some(file), Some(code)) => Ok(Marker::Invalid { code, file }),
        ("fragment", None, None) => Ok(Marker::Fragment),
        _ => Err(format!(
            "`{rest}` is not `valid file=..`, `invalid code=.. file=..` or `fragment`"
        )),
    })
}

/// Reads the page by hand: markers, fenced blocks, and the blocks no marker covers.
fn extract(text: &str) -> Extracted {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Extracted::default();
    let mut pending: Option<(usize, Marker)> = None;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(parsed) = parse_marker(line) {
            if let Some((at, _)) = pending.take() {
                out.problems.push(format!(
                    "line {at}: a marker is not followed by a code block"
                ));
            }
            match parsed {
                Ok(marker) => pending = Some((i + 1, marker)),
                Err(why) => out.problems.push(format!("line {}: {why}", i + 1)),
            }
            i += 1;
            continue;
        }
        if let Some(info) = line.strip_prefix("```") {
            let info = info.trim();
            let mut body = String::new();
            let mut j = i + 1;
            while j < lines.len() && !lines[j].starts_with("```") {
                body.push_str(lines[j]);
                body.push('\n');
                j += 1;
            }
            if j >= lines.len() {
                out.problems
                    .push(format!("line {}: a code block is never closed", i + 1));
                pending = None;
                break;
            }
            match pending.take() {
                Some((at, marker)) => {
                    let after = lines[j + 1..]
                        .iter()
                        .find(|l| !l.trim().is_empty())
                        .map(|l| (*l).to_owned());
                    out.examples.push(Example {
                        line: at,
                        marker,
                        text: body,
                        after,
                    });
                }
                None if info == "json" || info == "toml" => out.unmarked.push(i + 1),
                None => {}
            }
            i = j + 1;
            continue;
        }
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if let Some((at, _)) = pending.take() {
            out.problems.push(format!(
                "line {at}: a marker is not followed by a code block"
            ));
        }
        i += 1;
    }
    if let Some((at, _)) = pending {
        out.problems.push(format!(
            "line {at}: a marker is not followed by a code block"
        ));
    }
    out
}

/// The findings the validator reports for one example, as the corpus test builds them.
fn findings_of(example: &Example, file: &str) -> Vec<UiExtError> {
    let contribution = support::sample_contribution(vec![(file.to_owned(), example.text.clone())]);
    validate(&contribution)
}

fn codes_of(findings: &[UiExtError]) -> BTreeSet<&'static str> {
    findings.iter().map(|finding| finding.code).collect()
}

/// The `uiext.<name>` tokens of the page.
fn page_codes(page: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut rest = page;
    while let Some(at) = rest.find("uiext.") {
        let tail = &rest[at + 6..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || *c == '_')
            .collect();
        if !name.is_empty() && !matches!(name.as_str(), "toml" | "json" | "md" | "rs") {
            found.insert(format!("uiext.{name}"));
        }
        rest = tail;
    }
    found
}

#[test]
fn every_marked_example_behaves_as_the_page_says() {
    let extracted = extract(&page());
    let mut failures = extracted.problems.clone();
    for example in &extracted.examples {
        match &example.marker {
            Marker::Fragment => {}
            Marker::Valid { file } => {
                let findings = findings_of(example, file);
                if !findings.is_empty() {
                    failures.push(format!(
                        "page line {}: a valid example is refused: {findings:?}",
                        example.line
                    ));
                }
            }
            Marker::Invalid { code, file } => {
                let findings = findings_of(example, file);
                let found = codes_of(&findings);
                let wanted: BTreeSet<&str> = BTreeSet::from([code.as_str()]);
                if found != wanted {
                    failures.push(format!(
                        "page line {}: expected exactly {code}, got {findings:?}",
                        example.line
                    ));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} problem(s) with the guide's examples:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn there_are_enough_examples() {
    let extracted = extract(&page());
    let valid = extracted
        .examples
        .iter()
        .filter(|e| matches!(e.marker, Marker::Valid { .. }))
        .count();
    let invalid: Vec<&String> = extracted
        .examples
        .iter()
        .filter_map(|e| match &e.marker {
            Marker::Invalid { code, .. } => Some(code),
            _ => None,
        })
        .collect();
    let distinct: BTreeSet<&String> = invalid.iter().copied().collect();
    assert!(
        valid >= 8,
        "found {valid} valid examples, expected at least 8"
    );
    assert!(
        invalid.len() >= 12,
        "found {} invalid examples, expected at least 12",
        invalid.len()
    );
    assert!(
        distinct.len() >= 12,
        "the invalid examples cover {} distinct codes, expected at least 12: {distinct:?}",
        distinct.len()
    );
    let catalogue = catalogue_codes();
    for code in distinct {
        assert!(
            catalogue.contains(code),
            "an invalid example names `{code}`, which is not in the uiext catalogue"
        );
    }
}

#[test]
fn fragments_say_why_they_are_not_run() {
    let extracted = extract(&page());
    for example in extracted
        .examples
        .iter()
        .filter(|e| e.marker == Marker::Fragment)
    {
        let after = example.after.as_deref().unwrap_or("");
        assert!(
            after.starts_with("Not run:"),
            "page line {}: a fragment must be followed by a prose line beginning \"Not run:\" that says why; found {after:?}",
            example.line
        );
    }
}

#[test]
fn every_uiext_code_has_a_row_on_the_page() {
    let page = page();
    let catalogue = catalogue_codes();
    assert!(
        catalogue.len() >= 26,
        "found {} codes in the catalogue, expected at least 26: did the file change shape?",
        catalogue.len()
    );
    for code in &catalogue {
        assert!(
            page.contains(&format!("| `{code}` |")),
            "`{code}` is in the catalogue but has no row in the Refusals table of the page"
        );
    }
    for code in page_codes(&page) {
        assert!(
            catalogue.contains(&code),
            "the page names `{code}`, which is not in the uiext catalogue"
        );
    }
}

#[test]
fn the_page_names_the_contract_version() {
    let version = read(support::contracts_dir().join("VERSION"));
    let version = version.trim();
    assert!(!version.is_empty(), "contracts/ui-ext/VERSION is empty");
    assert!(
        page().contains(version),
        "the page does not name contract version {version}"
    );
}

#[test]
fn the_page_starts_with_the_standard_header() {
    assert!(
        page().starts_with("Part of [Running Spot](../running-spot.md)."),
        "the first line must be `Part of [Running Spot](../running-spot.md).`"
    );
}

#[test]
fn no_unmarked_blocks() {
    let extracted = extract(&page());
    assert!(
        extracted.unmarked.is_empty(),
        "json or toml blocks with no `ui-example` marker (nothing on the page may be untested by accident), at page lines {:?}",
        extracted.unmarked
    );
}

// ------------------------------------------------------------------------------------------
// The extractor itself.
// ------------------------------------------------------------------------------------------

#[test]
fn the_extractor_finds_a_marker_and_its_block() {
    let text = "intro\n<!-- ui-example: valid file=ui.toml -->\n```toml\na = 1\nb = 2\n```\n\nafter the block\n";
    let extracted = extract(text);
    assert!(extracted.problems.is_empty(), "{:?}", extracted.problems);
    assert!(extracted.unmarked.is_empty());
    assert_eq!(
        extracted.examples,
        vec![Example {
            line: 2,
            marker: Marker::Valid {
                file: "ui.toml".to_owned()
            },
            text: "a = 1\nb = 2\n".to_owned(),
            after: Some("after the block".to_owned()),
        }]
    );
}

#[test]
fn the_extractor_reads_every_kind_of_marker() {
    let text = "\
<!-- ui-example: invalid code=uiext.unknown_slot file=ui.json -->
```json
{}
```
<!-- ui-example: fragment -->
```sh
echo hi
```
Not run: a shell command.
";
    let extracted = extract(text);
    assert!(extracted.problems.is_empty(), "{:?}", extracted.problems);
    assert_eq!(extracted.examples.len(), 2);
    assert_eq!(
        extracted.examples[0].marker,
        Marker::Invalid {
            code: "uiext.unknown_slot".to_owned(),
            file: "ui.json".to_owned()
        }
    );
    assert_eq!(extracted.examples[1].marker, Marker::Fragment);
    assert_eq!(
        extracted.examples[1].after.as_deref(),
        Some("Not run: a shell command.")
    );
}

#[test]
fn the_extractor_reports_a_marker_with_no_block() {
    let text = "<!-- ui-example: valid file=ui.toml -->\nsome prose instead\n";
    let extracted = extract(text);
    assert_eq!(extracted.problems.len(), 1, "{:?}", extracted.problems);
    assert!(extracted.problems[0].contains("line 1"));
    assert!(extracted.examples.is_empty());

    let at_the_end = extract("text\n<!-- ui-example: fragment -->\n");
    assert_eq!(at_the_end.problems.len(), 1);
    assert!(at_the_end.problems[0].contains("line 2"));
}

#[test]
fn the_extractor_reports_an_unmarked_block_and_ignores_other_languages() {
    let text = "```json\n{}\n```\n```sh\nls\n```\n```toml\na = 1\n```\n";
    let extracted = extract(text);
    assert_eq!(extracted.unmarked, vec![1, 7]);
    assert!(extracted.examples.is_empty());
    assert!(extracted.problems.is_empty());
}

#[test]
fn the_extractor_keeps_two_blocks_apart() {
    let text = "\
<!-- ui-example: valid file=a.toml -->
```toml
one = 1
```
<!-- ui-example: valid file=b.json -->

```json
{\"two\": 2}
```
";
    let extracted = extract(text);
    assert!(extracted.problems.is_empty(), "{:?}", extracted.problems);
    let texts: Vec<&str> = extracted.examples.iter().map(|e| e.text.as_str()).collect();
    assert_eq!(texts, ["one = 1\n", "{\"two\": 2}\n"]);
    let lines: Vec<usize> = extracted.examples.iter().map(|e| e.line).collect();
    assert_eq!(lines, [1, 5]);
}

#[test]
fn the_extractor_rejects_a_malformed_marker_and_an_unclosed_block() {
    let bad = extract("<!-- ui-example: valid -->\n```toml\n```\n");
    assert_eq!(bad.problems.len(), 1, "{:?}", bad.problems);
    let odd = extract("<!-- ui-example: invalid file=ui.json -->\n```json\n{}\n```\n");
    assert_eq!(odd.problems.len(), 1, "{:?}", odd.problems);
    let extra = extract("<!-- ui-example: fragment color=red -->\n```sh\n```\n");
    assert_eq!(extra.problems.len(), 1, "{:?}", extra.problems);
    let open = extract("```toml\na = 1\n");
    assert_eq!(open.problems.len(), 1, "{:?}", open.problems);
    // An ordinary HTML comment is not a marker.
    let comment = extract("<!-- a note -->\n```sh\nls\n```\n");
    assert!(comment.problems.is_empty() && comment.examples.is_empty());
}

#[test]
fn page_codes_are_found_and_file_names_are_not_codes() {
    let found = page_codes("see `uiext.schema`, uiext.unknown_slot. And uiext.toml and uiext.");
    assert_eq!(
        found.into_iter().collect::<Vec<_>>(),
        ["uiext.schema", "uiext.unknown_slot"]
    );
}

#[test]
fn the_real_page_has_what_the_lower_bounds_expect() {
    let extracted = extract(&page());
    assert!(
        extracted.examples.len() >= 20,
        "found {} marked examples, expected at least 20: did the page change shape?",
        extracted.examples.len()
    );
    assert!(
        !extracted
            .examples
            .iter()
            .any(|e| matches!(&e.marker, Marker::Valid { file } if !(file.ends_with(".toml") || file.ends_with(".json")))),
        "an example's file must end in .toml or .json"
    );
}
