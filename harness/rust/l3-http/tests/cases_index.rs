//! The committed `CASES.md` is the generated one, and case names are unique. No database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_http::index;

#[test]
fn the_committed_cases_file_equals_the_generated_text() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("CASES.md");
    let committed = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
    assert!(
        committed == index::cases_md(),
        "harness/rust/l3-http/CASES.md is stale: run `cargo run -p l3-http -- --write-cases` and commit the result"
    );
}

#[test]
fn case_names_are_unique() {
    let entries = index::entries();
    assert_eq!(index::duplicate_name(&entries), None);
}

#[test]
fn every_entry_has_a_section_and_one_line_of_pins() {
    for entry in index::entries() {
        assert!(!entry.section.is_empty(), "{} has no section", entry.name);
        assert!(!entry.pins.is_empty(), "{} pins nothing", entry.name);
        assert!(
            !entry.pins.contains('\n'),
            "{} pins more than a line",
            entry.name
        );
    }
}

#[test]
fn the_moved_blocked_entries_keep_their_owner_and_reason() {
    let entries = index::entries();
    let (name, owner, reason) = (
        "export: a_filtered_export_matches_the_query",
        "WP-PRD-01.T4",
        "no `/api/v1/export*` route is mounted by the host",
    );
    let entry = entries.iter().find(|entry| entry.name == name).unwrap();
    assert_eq!(entry.kind, index::Kind::Blocked { owner, reason });
}

#[test]
fn the_agent_tool_cases_name_exactly_what_is_missing() {
    let entries = index::entries();
    let blocked = |name: &str| {
        let entry = entries.iter().find(|entry| entry.name == name).unwrap();
        match entry.kind {
            index::Kind::Blocked { owner, reason } => (owner, reason),
            index::Kind::Case => panic!("{name} runs"),
        }
    };
    // The extension-tool seam and the embedding port are served: the retrieval cases run.
    for name in [
        "retrieval: a_restricted_users_agent_never_surfaces_an_unreadable_document",
        "retrieval: a_revoked_share_applies_to_the_next_retrieve_call",
    ] {
        let entry = entries.iter().find(|entry| entry.name == name).unwrap();
        assert_eq!(entry.kind, index::Kind::Case, "{name}");
    }
    let (owner, reason) =
        blocked("agent_tools: a_t1_remedy_dry_runs_applies_and_undoes_through_the_tools");
    assert_eq!(owner, "WP-AI-07.T5");
    assert!(reason.contains("WorldAccess"), "{reason}");
    // What is missing for `recall_memory` is the agent credential path, named exactly.
    let (owner, reason) = blocked("agent_tools: recall_memory_is_gated_by_the_agents_tier");
    assert_eq!(owner, "WP-AI-06.T4");
    assert!(reason.contains("registered agent"), "{reason}");
    assert!(!reason.contains("no LLM tenant"), "{reason}");
    let absent = entries
        .iter()
        .find(|entry| {
            entry.name
                == "agent_tools: the_agent_tools_are_absent_for_a_user_and_while_their_services_are_off"
        })
        .unwrap();
    assert_eq!(absent.kind, index::Kind::Case);
}
