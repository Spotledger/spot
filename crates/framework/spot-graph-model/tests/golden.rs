//! The stable text dump golden test (brief item 4): pins the exact
//! `to_text()` lines — header, meta_version, sorted nodes, sorted edges with
//! sources, targets and properties — for the full fixture set (every edge
//! kind). The dump format is the golden contract; regenerate only when the
//! format itself changes deliberately.
//!
//! House pattern (spot-secrets golden): raw committed bytes under
//! `tests/golden/`, compared in-file, regenerated via `--ignored`.

// Test code may unwrap what the test itself built (spot-dag house pattern).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_graph_model::{GRAPH_TEXT_VERSION, GraphModel};

mod common;

use common::effective;

fn golden_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// The dump over the full fixtures, fresh each call.
fn dump() -> String {
    GraphModel::derive(&effective()).to_text()
}

#[test]
fn golden_dump_matches_the_committed_bytes() {
    let text = dump();
    let committed = std::fs::read_to_string(golden_dir().join("model-v1.txt"))
        .expect("the committed golden dump is checked in");
    assert_eq!(
        text, committed,
        "the text dump format changed; a committed golden must not drift"
    );
}

#[test]
fn header_pins_the_dump_format_and_meta_version() {
    let text = dump();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some(GRAPH_TEXT_VERSION));
    let meta_version_line = lines.next().expect("meta_version line");
    let version: u64 = meta_version_line
        .strip_prefix("meta_version ")
        .expect("meta_version prefix")
        .parse()
        .expect("numeric meta_version");
    assert!(
        version > 0,
        "the registry build bumps to a version above zero"
    );
    // No other header line repeats.
    assert_eq!(
        text.matches(GRAPH_TEXT_VERSION).count(),
        1,
        "the version token appears once: the header"
    );
}

#[test]
fn every_node_and_edge_is_one_line_with_a_sorted_target_block() {
    let text = dump();
    let node_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("node ")).collect();
    let edge_lines: Vec<&str> = text.lines().filter(|l| l.starts_with("edge ")).collect();
    // Nodes sorted by name: order, party, project.
    assert_eq!(node_lines.len(), 3, "nodes:\n{text}");
    let names: Vec<&str> = node_lines
        .iter()
        .map(|l| l.split_whitespace().nth(1).expect("quoted token"))
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "node lines are sorted by name");
    // Edges of every kind, in sorted order: two dynamic links (AnyOf and
    // Dynamic), three link fields (one of them a self reference), one multi-pick and one
    // owned containment.
    assert_eq!(edge_lines.len(), 7, "edges:\n{text}");
    let edge_names: Vec<&str> = edge_lines
        .iter()
        .map(|l| l.split_whitespace().nth(1).expect("quoted token"))
        .collect();
    let mut sorted_edges = edge_names.clone();
    sorted_edges.sort();
    assert_eq!(edge_names, sorted_edges, "edge lines are sorted by name");
    for name in [
        "\"party.about\"",
        "\"order.ref\"",
        "\"project.customer\"",
        "\"project_contains_project_task\"",
    ] {
        assert!(
            edge_names.contains(&name),
            "{name} missing from the dump:\n{text}"
        );
    }
}

#[test]
fn quoted_escaping_is_stable() {
    // Every value token is quoted and closed; the escape set (space, tab,
    // newline, quote, backslash) is pinned by the committed golden bytes —
    // a label with a space lands there escaped, and the golden test fails if
    // the escaping drifts.
    let text = dump();
    for line in text.lines() {
        for tok in line.split_whitespace().skip(1) {
            if tok.starts_with('"') {
                assert!(
                    tok.ends_with('"'),
                    "every quoted token closes: {tok} in {line}"
                );
            }
        }
    }
    // The version token is unquoted; every edge and node name is quoted.
    assert!(text.contains("\"party.about\" dynamic_link_many_to_one"));
    assert!(text.contains("\"project.customer\" many_to_one"));
}

#[test]
#[ignore = "writes the golden files"]
fn regenerate() {
    let path = golden_dir().join("model-v1.txt");
    let written = dump();
    std::fs::create_dir_all(golden_dir()).expect("golden dir");
    std::fs::write(&path, written).expect("write golden dump");
    println!("rewrote {}", path.display());
}
