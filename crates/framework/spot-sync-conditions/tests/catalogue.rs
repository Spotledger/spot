//! Catalogue tests: ADR-067 section 2 parity and the catalogue rules.
#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;

use spot_conditions::{ConditionEngine, ConditionParams, MemorySink, Millis, Severity};
use spot_sync_conditions::{
    catalogue, conditions_markdown, entries, message_keys, validate_catalogue,
};

/// Ladder rungs of ADR-065 that the acknowledgement ledger raises beside `OBLIGATION_OVERDUE`.
const COMPANIONS: [&str; 2] = ["OBLIGATION_DUE_SOON", "OBLIGATION_RED"];

fn adr_rows() -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../docs/adr/ADR-067-sync-self-healing-reconciliation-and-health.md");
    let text = std::fs::read_to_string(path).expect("ADR-067 is readable");
    let section = text
        .split("### 2. The catalogue")
        .nth(1)
        .and_then(|rest| rest.split("### 3.").next())
        .expect("ADR-067 has section 2");
    let mut ids = Vec::new();
    for line in section.lines().filter(|l| l.starts_with("| `")) {
        let cell = line.split('|').nth(1).expect("first cell");
        for token in cell.split('`').skip(1).step_by(2) {
            ids.push(token.to_owned());
        }
    }
    ids
}

#[test]
fn every_adr_row_has_exactly_one_definition_and_no_definition_lacks_a_row() {
    let rows = adr_rows();
    assert!(rows.len() >= 25, "parsed {} rows", rows.len());
    let row_set: BTreeSet<_> = rows.iter().cloned().collect();
    assert_eq!(
        row_set.len(),
        rows.len(),
        "a row id appears twice in the ADR"
    );
    let defs: Vec<String> = entries().into_iter().map(|e| e.def.id).collect();
    for row in &rows {
        assert_eq!(
            defs.iter().filter(|id| *id == row).count(),
            1,
            "{row} needs exactly one definition"
        );
    }
    for id in &defs {
        assert!(
            row_set.contains(id) || COMPANIONS.contains(&id.as_str()),
            "{id} has no ADR row"
        );
    }
    assert_eq!(defs.len(), rows.len() + COMPANIONS.len());
}

#[test]
fn ids_are_unique_and_the_registry_builds() {
    let registry = catalogue().expect("registry builds");
    let ids: BTreeSet<_> = entries().into_iter().map(|e| e.def.id).collect();
    assert_eq!(ids.len(), registry.len());
    assert_eq!(registry.len(), entries().len());
}

#[test]
fn escalation_thresholds_strictly_increase_and_go_up() {
    for entry in entries() {
        let mut level = entry.def.severity;
        let mut last = Millis::ZERO;
        for step in &entry.def.escalation {
            assert!(step.after_millis > last, "{}", entry.def.id);
            assert!(step.to > level, "{}", entry.def.id);
            last = step.after_millis;
            level = step.to;
        }
    }
}

#[test]
fn every_red_condition_has_a_response_step() {
    for entry in entries() {
        if entry.def.severity == Severity::Red {
            assert!(!entry.def.response.is_empty(), "{}", entry.def.id);
        }
    }
}

#[test]
fn calm_by_default_every_severity_above_info_has_a_reason() {
    for entry in entries() {
        if entry.def.severity > Severity::Info {
            assert!(!entry.reason.trim().is_empty(), "{}", entry.def.id);
        }
    }
    let raised_info = entries()
        .into_iter()
        .filter(|e| e.def.severity == Severity::Info)
        .count();
    assert!(raised_info >= 2);
}

#[test]
fn the_catalogue_passes_its_own_validation() {
    assert_eq!(validate_catalogue(), Ok(()));
}

#[test]
fn adr_severities_match() {
    let by_id = |id: &str| entries().into_iter().find(|e| e.def.id == id).expect(id);
    assert_eq!(by_id("NODE_TOO_OLD").def.severity, Severity::Red);
    assert_eq!(by_id("LAG_HIGH").def.severity, Severity::Amber);
    assert_eq!(by_id("EXTRAS_GROWTH").def.severity, Severity::Info);
    assert_eq!(by_id("SERIES_ADVANCE").def.severity, Severity::Info);
    assert_eq!(by_id("SERIES_LOW").def.severity, Severity::Amber);
    let missing = by_id("MISSING_DEP");
    assert_eq!(missing.def.escalation.len(), 1);
    assert_eq!(
        missing.def.escalation[0].after_millis,
        Millis::new(24 * 3_600_000)
    );
    assert_eq!(missing.def.escalation[0].to, Severity::Red);
}

#[test]
fn keys_are_sorted_unique_and_cover_every_text() {
    let keys = message_keys();
    let set: BTreeSet<_> = keys.iter().cloned().collect();
    assert_eq!(set.len(), keys.len());
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(sorted, keys);
    assert!(keys.iter().any(|k| k == "sync.condition.node_silent.title"));
    for entry in entries() {
        for key in [
            &entry.title_key,
            &entry.def.explanation_key,
            &entry.detector_key,
            &entry.clear_key,
        ] {
            assert!(set.contains(key), "{key}");
        }
        for step in &entry.def.response {
            assert!(set.contains(&format!("sync.remedy.{}", step.action)));
        }
        for command in &entry.def.actions {
            assert!(set.contains(&format!("sync.operator.{command}")));
        }
    }
}

#[test]
fn markdown_has_a_row_per_condition_and_is_stable() {
    let text = conditions_markdown();
    assert_eq!(text, conditions_markdown());
    assert_eq!(text.lines().count(), entries().len() + 2);
    assert!(text.contains("| `NODE_SILENT` | amber | node | red after 48h |"));
    assert!(text.contains("| `EXTRAS_GROWTH` | info |"));
    assert!(text.contains(
        "| `JOURNAL_AUDIT_MISMATCH` | red | tenant | none | capture_first_difference x1 | re_verify |"
    ));
}

#[test]
fn journal_audit_mismatch_matches_the_audit_job_shape() {
    let entry = spot_sync_conditions::entry("JOURNAL_AUDIT_MISMATCH").expect("catalogued");
    assert_eq!(entry.def.subject_kind, "tenant");
    assert_eq!(entry.def.params, ["first_differing_record", "difference"]);
    assert_eq!(entry.def.severity, Severity::Red);
    assert_eq!(entry.def.actions, ["re_verify"]);
    assert_eq!(entry.def.response.len(), 1);
    assert_eq!(entry.def.response[0].action, "capture_first_difference");
    assert_eq!(entry.def.response[0].limit, 1);
}

#[test]
fn journal_audit_mismatch_lists_both_parameters_when_raised_with_none() {
    let registry = catalogue().expect("registry");
    let def = registry.get("JOURNAL_AUDIT_MISMATCH").expect("def");
    assert_eq!(
        def.missing_params(&ConditionParams::new()),
        ["first_differing_record", "difference"]
    );
}

#[test]
fn the_engine_loads_the_catalogue_and_escalates_a_silent_node() {
    let mut engine = ConditionEngine::new(catalogue().expect("registry"), MemorySink::new());
    let params = ConditionParams::new()
        .with("node", "branch-12")
        .with("silent_for", "3h")
        .with("limit", "48h");
    engine
        .raise("NODE_SILENT", "branch-12", params, Millis::new(0))
        .expect("raise");
    let def = engine.registry().get("NODE_SILENT").expect("def");
    assert!(def.missing_params(&ConditionParams::new()).len() == 3);
    engine.tick(Millis::new(47 * 3_600_000));
    assert_eq!(engine.open()[0].severity, Severity::Amber);
    engine.tick(Millis::new(48 * 3_600_000));
    assert_eq!(engine.open()[0].severity, Severity::Red);
}

#[test]
fn the_obligation_rungs_match_the_acknowledgement_ledger() {
    let by_id = |id: &str| entries().into_iter().find(|e| e.def.id == id).expect(id);
    assert_eq!(by_id("OBLIGATION_DUE_SOON").def.severity, Severity::Info);
    assert_eq!(by_id("OBLIGATION_OVERDUE").def.severity, Severity::Amber);
    assert_eq!(by_id("OBLIGATION_RED").def.severity, Severity::Red);
    for id in [
        "OBLIGATION_DUE_SOON",
        "OBLIGATION_OVERDUE",
        "OBLIGATION_RED",
    ] {
        assert_eq!(by_id(id).def.subject_kind, "node");
        assert_eq!(
            by_id(id).def.params,
            ["node", "level", "positions", "count"]
        );
    }
}

/// The operator command vocabulary: every command any entry may name. A command outside it
/// fails the build of this test, so a misspelt headline cannot ship.
const COMMANDS: [&str; 10] = [
    "approve_recovery",
    "mark_reviewed",
    "notify_contact",
    "notify_role",
    "re_bootstrap_range",
    "re_verify",
    "release",
    "resend",
    "resume_node",
    "suspend_node",
];

#[test]
fn every_operator_command_is_in_the_vocabulary() {
    for entry in entries() {
        for command in &entry.def.actions {
            assert!(
                COMMANDS.contains(&command.as_str()),
                "{} names {command}",
                entry.def.id
            );
        }
    }
    // The brief allows a key-backup command only if the vocabulary has one: it has none.
    assert!(
        entries()
            .iter()
            .all(|e| e.def.actions.iter().all(|c| !c.contains("backup")))
    );
}

fn commands(id: &str) -> Vec<String> {
    spot_sync_conditions::entry(id)
        .expect("catalogued")
        .def
        .actions
}

fn headline(id: &str) -> Option<String> {
    spot_sync_conditions::entry(id)
        .expect("catalogued")
        .remedy_message_key()
}

#[test]
fn node_silent_notifies_the_contact_first() {
    assert_eq!(
        commands("NODE_SILENT"),
        ["notify_contact", "suspend_node", "resume_node"]
    );
    assert_eq!(
        headline("NODE_SILENT").as_deref(),
        Some("sync.operator.notify_contact")
    );
}

#[test]
fn schema_retire_blocked_asks_for_the_upgrade_first() {
    assert_eq!(
        commands("SCHEMA_RETIRE_BLOCKED"),
        ["notify_contact", "suspend_node"]
    );
    assert_eq!(
        headline("SCHEMA_RETIRE_BLOCKED").as_deref(),
        Some("sync.operator.notify_contact")
    );
}

#[test]
fn obligation_overdue_notifies_the_contact_before_resending() {
    assert_eq!(commands("OBLIGATION_OVERDUE"), ["notify_contact", "resend"]);
    assert_eq!(
        headline("OBLIGATION_OVERDUE").as_deref(),
        Some("sync.operator.notify_contact")
    );
}

#[test]
fn obligation_red_notifies_the_role_before_resending() {
    assert_eq!(commands("OBLIGATION_RED"), ["notify_role", "resend"]);
    assert_eq!(
        headline("OBLIGATION_RED").as_deref(),
        Some("sync.operator.notify_role")
    );
}

#[test]
fn key_backup_missing_notifies_the_operator_before_re_verifying() {
    assert_eq!(commands("KEY_BACKUP_MISSING"), ["notify_role", "re_verify"]);
    assert_eq!(
        headline("KEY_BACKUP_MISSING").as_deref(),
        Some("sync.operator.notify_role")
    );
}

#[test]
fn heal_budget_exhausted_notifies_the_operator_before_re_verifying() {
    assert_eq!(
        commands("HEAL_BUDGET_EXHAUSTED"),
        ["notify_role", "re_verify", "re_bootstrap_range"]
    );
    assert_eq!(
        headline("HEAL_BUDGET_EXHAUSTED").as_deref(),
        Some("sync.operator.notify_role")
    );
}
