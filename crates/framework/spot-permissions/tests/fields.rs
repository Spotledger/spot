//! The field permission matrix: one test reads `fields.toml`, plus the rules that are not a
//! table row (`filter_ok`, writes, `mask`, field sets).

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use spot_events_contract::lifecycle::Operation;
use spot_permissions::engine::{FieldChange, FieldLevel};
use spot_permissions::model::{AssignmentContext, FieldFacts, MaskFormat, Principal, Role};
use spot_permissions::{DocFacts, mask};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    #[serde(default)]
    principal: PrincipalSpec,
    doctype: String,
    field: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    created_by: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    values: BTreeMap<String, String>,
    level: String,
    #[serde(default)]
    flags: Vec<String>,
    #[serde(default)]
    kinds: Option<Vec<String>>,
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PrincipalSpec {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    roles: Option<Vec<String>>,
    #[serde(default)]
    assignments: Vec<AssignmentContext>,
    #[serde(default)]
    is_system: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Matrix {
    version: u32,
    #[serde(rename = "crate")]
    crate_: String,
    scenarios: Vec<Scenario>,
}

fn principal(spec: PrincipalSpec) -> Principal {
    if spec.is_system {
        return Principal::system();
    }
    let mut assignments = spec.assignments;
    if let Some(roles) = spec.roles {
        assignments.push(AssignmentContext {
            id: "default".to_owned(),
            roles: roles.into_iter().map(Role).collect(),
            scope: vec![],
        });
    }
    for a in &mut assignments {
        for up in &mut a.scope {
            for cell in &mut up.allowed {
                if let spot_app_api::prelude::Value::Id(id) = resolve_value(cell) {
                    *cell = id.to_string();
                }
            }
        }
    }
    Principal::user(spec.user.as_deref().unwrap_or("dave"), assignments)
}

fn level_name(level: FieldLevel) -> &'static str {
    match level {
        FieldLevel::Hidden => "hidden",
        FieldLevel::Masked(_) => "masked",
        FieldLevel::Read => "read",
        FieldLevel::ReadReveal => "read_reveal",
    }
}

fn facts_of(doctype: &str, field: &str) -> FieldFacts {
    let engine = engine();
    engine
        .snapshot()
        .facts(doctype)
        .and_then(|f| f.field(field).cloned())
        .unwrap_or_else(|| panic!("{doctype}.{field} is not a fixture field"))
}

#[test]
fn the_field_matrix() {
    let matrix: Matrix =
        toml::from_str(include_str!("fields.toml")).unwrap_or_else(|e| panic!("fields.toml: {e}"));
    assert_eq!(matrix.version, 1);
    assert_eq!(matrix.crate_, "spot-permissions");
    assert!(
        matrix.scenarios.len() >= 40,
        "the brief asks for at least 40 scenarios, found {}",
        matrix.scenarios.len()
    );
    let engine = engine();
    let mut failures = Vec::new();
    for s in matrix.scenarios {
        let doc = DocFacts {
            doctype: s.doctype.clone(),
            id: s.id.as_deref().map(resolve_doc_id),
            created_by: s.created_by.clone(),
            state: s.state.clone(),
            values: s
                .values
                .iter()
                .map(|(k, v)| (k.clone(), resolve_value(v)))
                .collect(),
        };
        let p = principal(s.principal);
        let facts = facts_of(&s.doctype, &s.field);
        let d = engine.field_access(&p, &s.doctype, &facts, &doc);
        let flags: BTreeSet<&str> = [
            ("create", d.can_create),
            ("update", d.can_update),
            ("report", d.can_report),
            ("reveal", d.can_reveal),
            ("reveal_bulk", d.can_reveal_bulk),
        ]
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(n, _)| n)
        .collect();
        let want_flags: BTreeSet<&str> = s.flags.iter().map(String::as_str).collect();
        let kinds: Vec<&str> = {
            let set: BTreeSet<&str> = d
                .reasons
                .iter()
                .map(|r| r.kind())
                .filter(|k| *k != "assignment")
                .collect();
            set.into_iter().collect()
        };
        let mut want_kinds: Vec<&str> = s
            .kinds
            .as_ref()
            .map(|k| k.iter().map(String::as_str).collect())
            .unwrap_or_default();
        want_kinds.sort_unstable();
        let mut problems = Vec::new();
        if level_name(d.level) != s.level {
            problems.push(format!(
                "level {} expected {}",
                level_name(d.level),
                s.level
            ));
        }
        if flags != want_flags {
            problems.push(format!("flags {flags:?} expected {want_flags:?}"));
        }
        if s.kinds.is_some() && kinds != want_kinds {
            problems.push(format!("reasons {kinds:?} expected {want_kinds:?}"));
        }
        if !problems.is_empty() {
            failures.push(format!(
                "{}: {}\n{}",
                s.name,
                problems.join("; "),
                engine.explain_field(&p, &s.doctype, &facts, &doc)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn a_masked_level_carries_the_fields_mask() {
    let engine = engine();
    let p = user("dave", &["Reader", "BankMasked"]);
    let doc = DocFacts {
        doctype: "sales_invoice".into(),
        ..DocFacts::default()
    };
    let d = engine.field_access(
        &p,
        "sales_invoice",
        &facts_of("sales_invoice", "bank_account"),
        &doc,
    );
    assert_eq!(d.level, FieldLevel::Masked(MaskFormat::LastChars(4)));
    // A field with no declared mask falls back to the placeholder, never to the value.
    let d = engine.field_access(
        &user("dave", &["Reader", "FinMasked"]),
        "sales_invoice",
        &facts_of("sales_invoice", "total"),
        &doc,
    );
    assert_eq!(d.level, FieldLevel::Masked(MaskFormat::Placeholder));
}

#[test]
fn filtering_needs_read_unmasked() {
    let engine = engine();
    let ok = |roles: &[&str], field: &str| {
        engine.filter_ok(&user("dave", roles), "sales_invoice", field)
    };
    assert!(ok(&["Reader"], "status"), "an unprotected readable field");
    assert!(!ok(&[], "status"), "no read, no filter");
    assert!(
        ok(&["Reader", "NotesRead"], "internal_notes"),
        "read unmasked"
    );
    assert!(!ok(&["Reader"], "internal_notes"), "hidden");
    assert!(!ok(&["Reader", "BankMasked"], "bank_account"), "masked");
    assert!(
        !ok(&["Reader", "BankReveal"], "bank_account"),
        "reveal alone is still masked"
    );
    assert!(ok(&["Reader", "BankReadReveal"], "bank_account"));
    assert!(
        !ok(&["Reader", "OwnNotes"], "internal_notes"),
        "a grant that depends on the document does not count without one"
    );
    assert!(engine.filter_ok(&Principal::system(), "sales_invoice", "bank_account"));
}

#[test]
fn a_write_names_the_field_and_an_unchanged_echo_is_accepted() {
    let engine = engine();
    let p = user("dave", &["Writer", "NotesUpdate", "BankMasked"]);
    let doc = DocFacts {
        doctype: "sales_invoice".into(),
        state: Some("draft".into()),
        ..DocFacts::default()
    };
    let changes = vec![
        ("status".to_owned(), FieldChange::Set),
        ("internal_notes".to_owned(), FieldChange::Set),
        // The edit form sends the masked field back as it received it.
        ("bank_account".to_owned(), FieldChange::Unchanged),
        // A hidden field changed by hand is a violation, never silently dropped.
        ("total".to_owned(), FieldChange::Set),
        ("bank_account".to_owned(), FieldChange::Set),
    ];
    let violations = engine.check_write(&p, "sales_invoice", Operation::Update, &doc, &changes);
    let paths: Vec<&str> = violations.iter().map(|v| v.path.as_str()).collect();
    assert_eq!(paths, vec!["total", "bank_account"]);
    assert!(
        violations
            .iter()
            .all(|v| v.code == "permission.field_denied")
    );
}

#[test]
fn field_sets_follow_the_decisions() {
    let engine = engine();
    let p = user("dave", &["Writer", "NotesUpdate", "FinMasked"]);
    let doc = DocFacts {
        doctype: "sales_invoice".into(),
        state: Some("draft".into()),
        ..DocFacts::default()
    };
    let readable = engine.readable_fields(&p, "sales_invoice", &doc);
    assert!(readable.contains("status"));
    assert!(readable.contains("internal_notes"));
    assert!(readable.contains("total"), "masked is still visible");
    assert!(!readable.contains("bank_account"));
    assert!(
        !readable.contains("number"),
        "protected by a field grant, none held"
    );
    let writable = engine.writable_fields(&p, "sales_invoice", Operation::Update, &doc);
    assert!(writable.contains("status"));
    assert!(writable.contains("internal_notes"));
    assert!(!writable.contains("total"));
    let creatable = engine.writable_fields(&p, "sales_invoice", Operation::Create, &doc);
    assert!(creatable.is_empty(), "Writer has no create right");
    let reportable = engine.reportable_fields(&p, "sales_invoice", &doc);
    assert!(reportable.is_empty(), "no report right anywhere");
}

#[test]
fn mask_shows_no_more_than_the_format_says() {
    assert_eq!(mask("1234567890", &MaskFormat::LastChars(4)), "******7890");
    assert_eq!(mask("12345678", &MaskFormat::LastChars(4)), "****5678");
    // A short value stays masked: at most half of it is ever shown.
    assert_eq!(mask("1234", &MaskFormat::LastChars(4)), "**34");
    assert_eq!(mask("ab", &MaskFormat::LastChars(8)), "*b");
    assert_eq!(mask("a", &MaskFormat::LastChars(4)), "*");
    assert_eq!(mask("", &MaskFormat::LastChars(4)), "");
    assert_eq!(mask("secret", &MaskFormat::Placeholder), "***");
    assert_eq!(mask("ab", &MaskFormat::Placeholder), "**");
    assert_eq!(mask("", &MaskFormat::Placeholder), "");
    // Characters, not bytes.
    assert_eq!(mask("ñññññññ", &MaskFormat::LastChars(2)), "*****ññ");
}
