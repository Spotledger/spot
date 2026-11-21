//! The generated default views hold for every DocType the repository ships (WP-UI-10.T3).
//!
//! The generator is tested on hand-built fixtures and on a runtime-created DocType. This file
//! loads every `doctypes/*.toml` of `apps/platform` and `apps/sample` into one effective
//! metadata, as `spot serve` does (platform first, one `Registry`), and asserts the promises of
//! the generator over the real DocTypes: a new DocType or field kind that breaks the default
//! views fails CI in the PR that adds it.
//!
//! What the generator does with every field (`src/generate/layout.rs`): `placement` has an arm
//! for **every** `FieldType` and every flag, so a field is never omitted; a hidden field is
//! still a form item (hiding is a layer's job), and there is no layout-only field kind. So the
//! rule used below is simply "every field of the effective DocType appears in the form exactly
//! once" (the engine-managed `doc_state` and `amended_from` of a posted DocType included).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_views::generate::default_view;
use spot_views::validate::validate_layer;
use spot_views::viewdef::{Item, ViewDef, ViewKind};

/// Real defects the generator or the validator shows on a shipped DocType, recorded so the suite
/// stays green while the defect is fixed: (DocType, view kind, one-line reason). Empty: none was
/// found by reading; the first run of the bundle fills it in if there is one, and the report
/// lists each entry.
const KNOWN_BAD: &[(&str, &str, &str)] = &[];

/// Sensitive fields (a secret, or in a protection group) that the generated form omits on
/// purpose, with the reason from `layout.rs`: (DocType, field, reason). Empty, because
/// `placement` gives every field a section; the test asserts the table is exactly the omissions
/// found, so both a new silent omission and a stale entry fail.
const OMITTED_BY_DESIGN: &[(&str, &str, &str)] = &[];

fn apps_dir(app: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../apps")
        .join(app)
        .join("doctypes")
}

/// Every `*.toml` of `dir`, parsed, in sorted file-name order (deterministic). A parse error
/// names the file.
fn load_definitions(dir: &Path) -> Vec<Definition> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let text =
                fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            Definition::parse(&path.display().to_string(), &text)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        })
        .collect()
}

/// The platform definitions, then the sample app's: the sample links to platform DocTypes.
fn all_definitions() -> (Vec<Definition>, usize, usize) {
    let platform = load_definitions(&apps_dir("platform"));
    let sample = load_definitions(&apps_dir("sample"));
    let counts = (platform.len(), sample.len());
    let mut all = platform;
    all.extend(sample);
    (all, counts.0, counts.1)
}

fn registry() -> Registry {
    let (definitions, _, _) = all_definitions();
    Registry::new(definitions).unwrap_or_else(|errors| {
        panic!(
            "the platform and sample apps do not build together: {}",
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )
    })
}

fn effective() -> Arc<EffectiveMeta> {
    registry().effective()
}

fn names(meta: &EffectiveMeta) -> Vec<String> {
    meta.definitions().iter().map(|d| d.name.clone()).collect()
}

fn known_bad(doctype: &str, kind: &str) -> bool {
    KNOWN_BAD
        .iter()
        .any(|(d, k, _)| *d == doctype && *k == kind)
}

/// Whether the generator treats `doctype` as a child: its grid request is not `view.bad_shape`.
fn is_child(meta: &EffectiveMeta, doctype: &str) -> bool {
    default_view(meta, doctype, ViewKind::Grid).is_ok()
}

/// Every field item of the view, in order.
fn field_items(view: &ViewDef) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    view.visit_items(&mut |item| {
        if let Item::Field { field, .. } = item {
            fields.push(field.clone());
        }
    });
    fields
}

fn kind_name(kind: ViewKind) -> &'static str {
    match kind {
        ViewKind::Form => "form",
        ViewKind::List => "list",
        ViewKind::Grid => "grid",
    }
}

/// Generates, resolves with no layers and validates; the problems of this DocType and kind, as
/// text, or none.
fn problems_of(meta: &EffectiveMeta, doctype: &str, kind: ViewKind) -> Vec<String> {
    let view = match default_view(meta, doctype, kind) {
        Ok(view) => view,
        Err(errors) => return vec![format!("does not generate: {errors:?}")],
    };
    let resolved = spot_views::resolve(&view, &[], meta);
    validate_layer(&resolved, meta)
        .iter()
        .map(|problem| format!("{problem:?}"))
        .collect()
}

#[test]
fn the_two_apps_load_and_build_into_one_effective_metadata() {
    let (definitions, platform, sample) = all_definitions();
    assert!(
        platform >= 10,
        "found {platform} platform DocTypes in apps/platform/doctypes, expected at least 10: did the directory move?"
    );
    assert!(
        sample >= 5,
        "found {sample} sample DocTypes in apps/sample/doctypes, expected at least 5: did the directory move?"
    );
    assert_eq!(definitions.len(), platform + sample);
    let meta = effective();
    for definition in &definitions {
        assert!(
            meta.definition(&definition.name).is_some(),
            "{} is not in the effective metadata",
            definition.name
        );
    }
    assert!(
        meta.definitions().len() >= definitions.len(),
        "the effective metadata holds fewer DocTypes than the apps declare"
    );
}

#[test]
fn every_doctype_has_a_default_form_and_list_that_validate() {
    let meta = effective();
    let mut failures = Vec::new();
    let mut checked = 0;
    for doctype in names(&meta) {
        if is_child(&meta, &doctype) {
            continue;
        }
        for kind in [ViewKind::Form, ViewKind::List] {
            checked += 1;
            let problems = problems_of(&meta, &doctype, kind);
            if !problems.is_empty() && !known_bad(&doctype, kind_name(kind)) {
                failures.push(format!(
                    "{doctype} {}: {}",
                    kind_name(kind),
                    problems.join("; ")
                ));
            }
        }
    }
    assert!(
        checked >= 20,
        "checked {checked} (DocType, kind) pairs, expected at least 20: are the apps found?"
    );
    assert!(
        failures.is_empty(),
        "{} default view(s) of the shipped DocTypes do not validate:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn every_child_table_has_a_default_grid_that_validates() {
    let meta = effective();
    // The DocTypes some other DocType names as the target of a table field.
    let mut targets: BTreeSet<String> = BTreeSet::new();
    for definition in meta.definitions() {
        for field in &definition.fields {
            let is_table = matches!(field.ty, FieldType::Table | FieldType::TableMultiselect);
            if let Some(target) = field.options.as_ref().filter(|_| is_table) {
                targets.insert(target.clone());
            }
        }
    }
    assert!(
        targets.len() >= 4,
        "found {} table targets, expected at least 4: {targets:?}",
        targets.len()
    );
    let mut failures = Vec::new();
    for target in &targets {
        match default_view(&meta, target, ViewKind::Grid) {
            Err(errors) => failures.push(format!(
                "{target} is the target of a table field but has no grid: {errors:?}"
            )),
            Ok(_) => {
                let problems = problems_of(&meta, target, ViewKind::Grid);
                if !problems.is_empty() && !known_bad(target, "grid") {
                    failures.push(format!("{target} grid: {}", problems.join("; ")));
                }
            }
        }
    }
    // The other way round: a DocType with a grid is one that a table field names (the apps
    // declare no child that nothing uses), and one without is a parent the first test covers.
    for doctype in names(&meta) {
        let has_grid = is_child(&meta, &doctype);
        if has_grid != targets.contains(&doctype) {
            failures.push(format!(
                "{doctype}: has a grid is {has_grid} but is a table target is {}",
                targets.contains(&doctype)
            ));
        }
    }
    for expected in ["checklist_item", "label_pick"] {
        if !targets.contains(expected) || !is_child(&meta, expected) {
            failures.push(format!(
                "the sample child `{expected}` is not a table target with a grid"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} problem(s) with the shipped child tables:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn every_visible_field_appears_exactly_once_in_the_form() {
    let meta = effective();
    let mut failures = Vec::new();
    let mut fields_seen = 0;
    for definition in meta.definitions() {
        let view = match default_view(&meta, &definition.name, ViewKind::Form) {
            Ok(view) => view,
            Err(errors) => {
                failures.push(format!(
                    "{} form does not generate: {errors:?}",
                    definition.name
                ));
                continue;
            }
        };
        let in_form = field_items(&view);
        let expected: Vec<String> = definition.fields.iter().map(|f| f.name.clone()).collect();
        fields_seen += expected.len();
        let mut sorted_form = in_form.clone();
        sorted_form.sort();
        let mut sorted_expected = expected.clone();
        sorted_expected.sort();
        if sorted_form != sorted_expected {
            let missing: Vec<&String> = sorted_expected
                .iter()
                .filter(|name| !in_form.contains(name))
                .collect();
            let extra: Vec<&String> = sorted_form
                .iter()
                .filter(|name| !expected.contains(name))
                .collect();
            let mut twice: Vec<&String> = sorted_form
                .windows(2)
                .filter(|pair| pair[0] == pair[1])
                .map(|pair| &pair[0])
                .collect();
            twice.dedup();
            failures.push(format!(
                "{}: the form omits {missing:?}, adds {extra:?} and repeats {twice:?}",
                definition.name
            ));
        }
    }
    assert!(
        fields_seen >= 100,
        "checked {fields_seen} fields, expected at least 100: are the apps found?"
    );
    assert!(
        failures.is_empty(),
        "{} generated form(s) do not show every field exactly once:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn secret_and_protected_fields_are_never_dropped_silently() {
    let meta = effective();
    let sensitive = |field: &FieldSpec| {
        matches!(field.ty, FieldType::Secret)
            || field.protection_group.is_some()
            || field.classification.iter().any(|tag| tag == "secret")
    };
    let mut seen_sensitive = 0;
    let mut omitted: Vec<(String, String)> = Vec::new();
    for definition in meta.definitions() {
        let view = default_view(&meta, &definition.name, ViewKind::Form)
            .unwrap_or_else(|errors| panic!("{} form: {errors:?}", definition.name));
        let in_form = field_items(&view);
        for field in definition.fields.iter().filter(|f| sensitive(f)) {
            seen_sensitive += 1;
            if !in_form.contains(&field.name) {
                omitted.push((definition.name.clone(), field.name.clone()));
            }
        }
    }
    assert!(
        seen_sensitive >= 5,
        "found {seen_sensitive} secret or protected fields in the apps, expected at least 5"
    );
    omitted.sort();
    let mut listed: Vec<(String, String)> = OMITTED_BY_DESIGN
        .iter()
        .map(|(d, f, _)| ((*d).to_owned(), (*f).to_owned()))
        .collect();
    listed.sort();
    assert_eq!(
        omitted, listed,
        "the sensitive fields the generated form omits must be exactly OMITTED_BY_DESIGN: a new silent omission is a defect, a stale entry is to be removed"
    );
}

#[test]
fn generation_is_deterministic_and_round_trips() {
    let first = effective();
    let second = effective();
    let mut failures = Vec::new();
    let mut texts = 0;
    for doctype in names(&first) {
        let mut kinds = vec![ViewKind::Form, ViewKind::List];
        if is_child(&first, &doctype) {
            kinds.push(ViewKind::Grid);
        }
        for kind in kinds {
            let label = format!("{doctype} {}", kind_name(kind));
            let text_of = |meta: &EffectiveMeta| -> Result<(ViewDef, String), String> {
                let view = default_view(meta, &doctype, kind).map_err(|e| format!("{e:?}"))?;
                let text = view.to_toml().map_err(|e| e.to_string())?;
                Ok((view, text))
            };
            let (view, text) = match text_of(&first) {
                Ok(pair) => pair,
                Err(why) => {
                    failures.push(format!("{label}: {why}"));
                    continue;
                }
            };
            texts += 1;
            match text_of(&second) {
                Ok((_, again)) if again == text => {}
                Ok(_) => {
                    failures.push(format!("{label}: the TOML bytes differ between two builds"))
                }
                Err(why) => failures.push(format!("{label}: {why}")),
            }
            match text_of(&first) {
                Ok((_, again)) if again == text => {}
                Ok(_) => failures.push(format!("{label}: the TOML bytes differ between two calls")),
                Err(why) => failures.push(format!("{label}: {why}")),
            }
            match ViewDef::parse(&label, &text) {
                Ok(parsed) if parsed == view => {}
                Ok(_) => {
                    failures.push(format!("{label}: the TOML parses back to a different view"))
                }
                Err(errors) => {
                    failures.push(format!("{label}: the TOML does not parse back: {errors:?}"))
                }
            }
        }
    }
    assert!(
        texts >= 40,
        "generated {texts} view texts, expected at least 40: are the apps found?"
    );
    assert!(
        failures.is_empty(),
        "{} problem(s) with determinism or the TOML round trip:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}

#[test]
fn adding_a_custom_field_to_a_real_doctype_adds_it_to_the_form() {
    let mut registry = registry();
    let before_meta = registry.effective();
    let before = field_items(
        &default_view(&before_meta, "task", ViewKind::Form)
            .unwrap_or_else(|errors| panic!("task form: {errors:?}")),
    );
    assert!(
        before.len() >= 5,
        "the sample `task` form shows {} fields, expected at least 5: {before:?}",
        before.len()
    );
    registry
        .add_custom_field("task", FieldSpec::new("custom_flag", FieldType::Bool), None)
        .unwrap_or_else(|errors| panic!("the custom field does not add: {errors:?}"));
    let after_meta = registry.effective();
    let view = default_view(&after_meta, "task", ViewKind::Form)
        .unwrap_or_else(|errors| panic!("task form after: {errors:?}"));
    let after = field_items(&view);
    assert_eq!(
        after.iter().filter(|name| *name == "custom_flag").count(),
        1,
        "the form shows custom_flag exactly once: {after:?}"
    );
    let without: Vec<String> = after
        .iter()
        .filter(|name| *name != "custom_flag")
        .cloned()
        .collect();
    assert_eq!(
        without, before,
        "every other field keeps its position relative to the others"
    );
    let problems = problems_of(&after_meta, "task", ViewKind::Form);
    assert!(
        problems.is_empty(),
        "task form no longer validates: {problems:?}"
    );
}
