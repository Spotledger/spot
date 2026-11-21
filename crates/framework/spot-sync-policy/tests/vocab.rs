//! The lint flag for the old and the new sync vocabulary (WP-SYN-01.T6): one test per row of the
//! mapping (M1 to M8), per refusal rule (C1 to C5, C7), the flag default, and the repository.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use common::{doctype, text};
use spot_sync_policy::vocab::codes;
use spot_sync_policy::{
    Class, Direction, FieldName, PolicyMode, PolicySet, Preset, PresetPolicy, Refusal, Rewrite,
    ScopeKeyType, SyncPolicy, Trigger, WhenEmpty, lint_vocabulary, rewrite_candidate,
    rewrite_candidate_with, verify_with_mode,
};

fn key(name: &str) -> ScopeKeyType {
    ScopeKeyType::new(name).unwrap()
}

fn old(class: Class, direction: Direction, owner: Option<&str>) -> SyncPolicy {
    SyncPolicy::new(
        class,
        owner.map(key),
        direction,
        Trigger::OnSubmit,
        BTreeSet::new(),
    )
}

fn mechanical(policy: &SyncPolicy) -> PresetPolicy {
    match rewrite_candidate(policy) {
        Rewrite::Mechanical(new) => *new,
        other => panic!("expected a mechanical rewrite, got {other:?}"),
    }
}

fn reasons(policy: &SyncPolicy) -> Vec<Refusal> {
    match rewrite_candidate(policy) {
        Rewrite::NeedsHuman { reasons } => reasons,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ---- the eight rows ----

#[test]
fn m1_local_none_is_local() {
    let new = mechanical(&old(Class::Local, Direction::None, None));
    assert_eq!(new, PresetPolicy::new(Preset::Local));
}

#[test]
fn m2_master_down_is_master_with_scope_all() {
    let new = mechanical(&old(Class::Master, Direction::Down, None));
    assert_eq!(new, PresetPolicy::new(Preset::Master));
    assert_eq!(new.to_text(), "preset = \"master\"\n");
}

#[test]
fn m3_owner_scope_on_a_master_is_refused_and_the_key_is_kept_in_the_reason() {
    let r = reasons(&old(Class::Master, Direction::Down, Some("company")));
    assert_eq!(
        r,
        [Refusal::OwnerScopeOnMaster {
            key_type: key("company")
        }]
    );
    assert_eq!(r[0].decision(), "C1");
    assert!(r[0].question().contains("company"));
}

#[test]
fn m4_transactional_up_needs_the_key_field() {
    let r = reasons(&old(Class::Transactional, Direction::Up, Some("warehouse")));
    assert_eq!(
        r,
        [Refusal::OwnerKeyNeedsField {
            key_type: key("warehouse")
        }]
    );
    assert_eq!(r[0].decision(), "C2");
}

#[test]
fn m5_shared_up_needs_the_field_and_a_provable_key_holder_creator() {
    let r = reasons(&old(Class::Shared, Direction::Up, Some("warehouse")));
    assert_eq!(
        r,
        [
            Refusal::OwnerKeyNeedsField {
                key_type: key("warehouse")
            },
            Refusal::KeyHolderNotProvable {
                key_type: key("warehouse")
            },
        ]
    );
    assert_eq!(r[1].decision(), "C3");
    // C3: `Class::Shared` is not retired and gains no replacement word.
    assert_eq!(Class::parse("shared"), Some(Class::Shared));
}

#[test]
fn m6_shared_down_without_a_key_is_master() {
    let new = mechanical(&old(Class::Shared, Direction::Down, None));
    assert_eq!(new, PresetPolicy::new(Preset::Master));
}

#[test]
fn m7_shared_down_with_a_key_is_refused() {
    let r = reasons(&old(Class::Shared, Direction::Down, Some("branch")));
    assert_eq!(
        r,
        [Refusal::KeyedSharedDown {
            key_type: key("branch")
        }]
    );
    assert_eq!(r[0].decision(), "");
}

#[test]
fn m8_shared_both_is_refused_with_both_candidates_printed() {
    let r = reasons(&old(Class::Shared, Direction::Both, Some("branch")));
    let [Refusal::SharedBoth { candidates, .. }] = r.as_slice() else {
        panic!("expected one C4 refusal, got {r:?}");
    };
    assert_eq!(r[0].decision(), "C4");
    assert_eq!(candidates.len(), 2);
    assert!(candidates[0].contains("handover"));
    assert!(candidates[1].contains("record") && candidates[1].contains("relay"));
    let set = declared_with("order", old(Class::Shared, Direction::Both, Some("branch")));
    let report = lint_vocabulary(&set, PolicyMode::Both);
    let detail = report.warnings[0].detail();
    assert!(
        detail.contains("handover") && detail.contains("relay"),
        "{detail}"
    );
}

// ---- settings ----

#[test]
fn exclude_fields_map_to_the_local_exclusion_list() {
    let mut policy = old(Class::Master, Direction::Down, None);
    policy.exclude_fields = ["b", "a"]
        .iter()
        .map(|n| FieldName::new(n).unwrap())
        .collect();
    let new = mechanical(&policy);
    assert_eq!(
        new.to_text(),
        "preset = \"master\"\nexcluded_fields = { local = [\"a\", \"b\"] }\n"
    );
}

#[test]
fn exclusions_on_a_local_declaration_are_refused_not_dropped() {
    let mut policy = old(Class::Local, Direction::None, None);
    policy.exclude_fields = [FieldName::new("a").unwrap()].into_iter().collect();
    let r = reasons(&policy);
    assert!(
        matches!(r.as_slice(), [Refusal::CandidateInvalid { .. }]),
        "{r:?}"
    );
}

#[test]
fn c7_when_empty_away_from_the_default_is_refused() {
    let policy = old(Class::Master, Direction::Down, None);
    assert!(matches!(
        rewrite_candidate_with(&policy, WhenEmpty::None),
        Rewrite::Mechanical(_)
    ));
    let Rewrite::NeedsHuman { reasons } = rewrite_candidate_with(&policy, WhenEmpty::All) else {
        panic!("expected a refusal");
    };
    assert_eq!(
        reasons,
        [Refusal::WhenEmptyAwayFromDefault {
            set: WhenEmpty::All
        }]
    );
    assert_eq!(reasons[0].decision(), "C7");
}

#[test]
fn c7_is_listed_beside_the_row_reasons_not_instead_of_them() {
    let policy = old(Class::Master, Direction::Down, Some("company"));
    let Rewrite::NeedsHuman { reasons } = rewrite_candidate_with(&policy, WhenEmpty::All) else {
        panic!("expected a refusal");
    };
    assert_eq!(reasons.len(), 2);
}

#[test]
fn an_illegal_combination_has_nothing_to_rewrite() {
    for policy in [
        old(Class::Local, Direction::Up, None),
        old(Class::Master, Direction::Up, Some("k")),
        old(Class::Transactional, Direction::Down, None),
        old(Class::Shared, Direction::Up, None),
        old(Class::Shared, Direction::None, None),
        old(Class::Local, Direction::None, Some("k")),
    ] {
        assert_eq!(
            reasons(&policy),
            [Refusal::NotALegalDeclaration],
            "{policy:?}"
        );
    }
}

#[test]
fn the_rewrite_is_a_pure_function() {
    for class in Class::ALL {
        for direction in Direction::ALL {
            for owner in [None, Some("k")] {
                let policy = old(class, direction, owner);
                assert_eq!(rewrite_candidate(&policy), rewrite_candidate(&policy));
            }
        }
    }
}

// ---- the flag ----

fn declared_with(name: &str, policy: SyncPolicy) -> PolicySet {
    let mut set = PolicySet::new();
    set.declare(name, policy);
    set
}

#[test]
fn the_flag_defaults_to_both() {
    assert_eq!(PolicyMode::default(), PolicyMode::Both);
}

#[test]
fn both_warns_and_new_only_refuses_with_the_rewrite_text() {
    let set = declared_with("item", old(Class::Master, Direction::Down, None));
    let both = lint_vocabulary(&set, PolicyMode::Both);
    assert!(both.errors.is_empty());
    assert_eq!(both.warnings.len(), 1);
    assert_eq!(both.warnings[0].code(), codes::RETIRED_VOCABULARY);
    assert_eq!(both.warnings[0].doctype(), "item");
    assert!(both.warnings[0].detail().contains("preset = \"master\""));

    let new_only = lint_vocabulary(&set, PolicyMode::NewOnly);
    assert!(new_only.warnings.is_empty());
    assert_eq!(new_only.errors.len(), 1);
    assert_eq!(new_only.errors[0].code(), codes::RETIRED_VOCABULARY);
    assert!(new_only.errors[0].detail().contains("preset = \"master\""));
}

#[test]
fn a_refused_row_is_a_finding_that_says_nothing_is_rewritten() {
    let set = declared_with("rate", old(Class::Master, Direction::Down, Some("company")));
    let report = lint_vocabulary(&set, PolicyMode::NewOnly);
    let detail = report.errors[0].detail();
    assert!(detail.contains("needs a person"), "{detail}");
    assert!(detail.contains("company"), "{detail}");
}

#[test]
fn c5_trigger_is_accepted_and_ignored_with_a_warning() {
    let mut policy = old(Class::Master, Direction::Down, None);
    policy.trigger = Trigger::OnSave;
    let set = declared_with("item", policy.clone());
    for mode in [PolicyMode::Both, PolicyMode::NewOnly] {
        let report = lint_vocabulary(&set, mode);
        let w: Vec<_> = report
            .warnings
            .iter()
            .filter(|p| p.code() == codes::RETIRED_TRIGGER)
            .collect();
        assert_eq!(w.len(), 1, "{mode:?}");
        assert!(w[0].detail().contains("implemented by nothing"));
    }
    // The trigger never changes the rewrite.
    let mut other = policy;
    other.trigger = Trigger::Manual;
    assert_eq!(
        rewrite_candidate(&other),
        rewrite_candidate(&old(Class::Master, Direction::Down, None))
    );
    // No warning for the default trigger.
    let plain = declared_with("item", old(Class::Master, Direction::Down, None));
    assert!(
        lint_vocabulary(&plain, PolicyMode::Both)
            .warnings
            .iter()
            .all(|p| p.code() != codes::RETIRED_TRIGGER)
    );
}

#[test]
fn an_empty_declaration_set_is_clean() {
    assert!(lint_vocabulary(&PolicySet::new(), PolicyMode::NewOnly).is_clean());
}

#[test]
fn verify_with_mode_keeps_the_policy_errors_and_adds_the_vocabulary_ones() {
    let meta = common::build(vec![doctype("item", vec![text("title")])]);
    let mut set = declared_with("item", old(Class::Master, Direction::Down, None));
    set.declare("ghost", old(Class::Master, Direction::Down, None));
    let (table, both) = verify_with_mode(&meta, &set, PolicyMode::Both);
    assert!(table.get("item").is_some());
    assert_eq!(both.errors.len(), 1, "{:?}", both.errors);
    assert_eq!(both.errors[0].code(), "sync_policy.unknown_doctype");
    assert_eq!(both.warnings.len(), 2);
    let (_, new_only) = verify_with_mode(&meta, &set, PolicyMode::NewOnly);
    assert_eq!(new_only.errors.len(), 3);
    assert!(new_only.warnings.is_empty());
}

// ---- the repository ----

fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "target" || name == "node_modules" || name == ".git" {
            continue;
        }
        if path.is_dir() {
            collect(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("toml" | "json" | "yaml" | "yml")
        ) {
            out.push(path);
        }
    }
}

#[test]
fn no_metadata_in_the_repository_declares_the_old_words() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let keys = [
        "sync_policy",
        "sync_class",
        "sync_direction",
        "sync_trigger",
        "sync_owner_scope",
        "sync_exclude_fields",
    ];
    let mut files = Vec::new();
    collect(&root.join("apps"), &mut files);
    let mut hits = Vec::new();
    for file in files {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (n, line) in content.lines().enumerate() {
            let trimmed = line.trim_start().trim_start_matches('"');
            for k in keys {
                if let Some(rest) = trimmed.strip_prefix(k)
                    && rest
                        .trim_start()
                        .trim_start_matches('"')
                        .starts_with(['=', ':'])
                {
                    hits.push(format!("{}:{}", file.display(), n + 1));
                }
            }
        }
    }
    assert!(hits.is_empty(), "old-vocabulary declarations: {hits:?}");
}
