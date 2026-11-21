//! Lint S2 (WP-SYN-01.T6): a `requires_adopted` link to a `record` target, and a `handover` with
//! `adopt = never`, are errors; the near misses are accepted.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{build, doctype, link};
use spot_sync_policy::preset_lint::codes;
use spot_sync_policy::{
    AdoptMode, PlainExtras, Preset, PresetPolicy, PresetTable, Selector, lint_preset_entity,
    lint_presets,
};

fn defs() -> spot_meta::EffectiveMeta {
    build(vec![
        doctype("price_list", vec![link("note", "note")]),
        doctype("note", vec![]),
    ])
}

fn table(entries: &[(&str, PresetPolicy)]) -> PresetTable {
    let mut t = PresetTable::new();
    for (name, p) in entries {
        t.set(name, p.clone());
    }
    t
}

fn found(t: &PresetTable, extras: &PlainExtras) -> Vec<(String, &'static str)> {
    lint_presets(t, &defs(), extras)
        .iter()
        .map(|p| (p.doctype().to_owned(), p.code()))
        .collect()
}

#[test]
fn s2_a_requires_adopted_link_to_a_record_is_refused() {
    let marked = PlainExtras::new().with_requires_adopted("price_list", "note");
    let t = table(&[
        ("price_list", PresetPolicy::new(Preset::Master)),
        ("note", PresetPolicy::new(Preset::Record)),
    ]);
    assert_eq!(
        found(&t, &marked),
        [("price_list".to_owned(), codes::LINT_REQUIRES_ADOPTED_RECORD)]
    );
    // The finding involves the target too, so saving either entity reports it.
    for name in ["price_list", "note"] {
        let hit = lint_preset_entity(&t, &defs(), &marked, name);
        assert!(
            hit.iter()
                .any(|p| p.code() == codes::LINT_REQUIRES_ADOPTED_RECORD),
            "{name}"
        );
    }
}

#[test]
fn s2_near_misses_are_accepted() {
    let marked = PlainExtras::new().with_requires_adopted("price_list", "note");
    // A handover target can be adopted.
    let mut adoptable = PresetPolicy::new(Preset::Handover);
    adoptable.scope = Some(Selector::All);
    let handover = table(&[
        ("price_list", PresetPolicy::new(Preset::Master)),
        ("note", adoptable),
    ]);
    assert_eq!(found(&handover, &marked), []);
    // A record target without the mark is not an S2 matter (and a record to record link is fine).
    let unmarked = table(&[
        ("price_list", PresetPolicy::new(Preset::Record)),
        ("note", PresetPolicy::new(Preset::Record)),
    ]);
    assert!(found(&unmarked, &PlainExtras::new()).is_empty());
}

#[test]
fn s2_a_handover_that_is_never_adopted_is_refused() {
    let mut never = PresetPolicy::new(Preset::Handover);
    never.adopt = Some(AdoptMode::Never);
    let t = table(&[("note", never)]);
    assert_eq!(
        found(&t, &PlainExtras::new()),
        [("note".to_owned(), codes::LINT_ADOPT_NEVER)]
    );
    for mode in [None, Some(AdoptMode::Manual), Some(AdoptMode::Auto)] {
        let mut p = PresetPolicy::new(Preset::Handover);
        p.adopt = mode;
        assert!(
            found(&table(&[("note", p)]), &PlainExtras::new()).is_empty(),
            "{mode:?}"
        );
    }
}
