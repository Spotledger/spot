//! Same-version metadata tests (WP-MET-03.T5): the metadata version is
//! informational, so neither a model nor its diff may read "same version" as
//! "nothing changed".
//!
//! Two effective metadata sets built from **different** definitions both
//! report version 1 — the version is the customisations' revision plus one,
//! and a fresh set has revision 0 — so equal versions say nothing about equal
//! content.

// Test code may unwrap what the test itself built (spot-dag house pattern).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{effective_of, fixtures, link, party};
use spot_graph_model::GraphModel;

/// The fixture set with one more link field on `party`: an edge the plain
/// fixture set does not have.
fn fixtures_with_party_territory() -> Vec<spot_meta::Definition> {
    let mut changed = party();
    changed.fields.push(link("territory", "party"));
    let mut defs = fixtures();
    defs[0] = changed;
    defs
}

#[test]
fn two_metadata_sets_at_one_version_diff_the_new_edge() {
    let first = effective_of(fixtures());
    let second = effective_of(fixtures_with_party_territory());
    // The premise: two different metadata sets, one version.
    assert_eq!(first.version(), second.version(), "same version, not v1");
    assert_eq!(first.version(), 1);

    let old = GraphModel::derive(&first);
    let new = GraphModel::derive(&second);
    assert_eq!(
        old.meta_version, new.meta_version,
        "models hold one version"
    );
    assert_ne!(old, new, "the content differs even though the version does");

    // The diff reports the new edge; an equal version short-circuits nothing.
    let diff = new.diff(&old);
    assert_eq!(diff.added_edges, vec!["party.territory".to_owned()]);
    assert!(diff.removed_edges.is_empty());
    assert!(diff.changed_edges.is_empty());
    assert!(diff.added_nodes.is_empty());
    assert!(diff.removed_nodes.is_empty());
    assert!(!diff.is_empty(), "an equal version hid a new edge");
    // The edge is a real one, not a diff artefact.
    let edge = new.edge("party.territory").expect("the new edge exists");
    assert_eq!(edge.to_one(), Some("party"));

    // The same version in the other direction: the edge is a removal.
    let back = old.diff(&new);
    assert_eq!(back.removed_edges, vec!["party.territory".to_owned()]);
    assert!(back.added_edges.is_empty());
}

#[test]
fn an_equal_version_over_equal_content_diffs_clean() {
    // The mirror image: the version may not invent a change either. Two
    // separate builds of the same definitions report version 1 and derive the
    // same model.
    let first = effective_of(fixtures());
    let second = effective_of(fixtures());
    assert_eq!(first.version(), second.version());
    let old = GraphModel::derive(&first);
    let new = GraphModel::derive(&second);
    assert_eq!(old, new);
    assert!(new.diff(&old).is_empty());
    assert!(old.diff(&new).is_empty());
}
