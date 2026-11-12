//! Every `journal_ops.*` code constant is in the error catalogue and every catalogue entry
//! with that prefix has a constant (WP-RTB-04.T4).

use spot_mgmt_journal::{ExecError, ExecKind, OpError, error};

const SOURCE: &str = include_str!("../src/error.rs");
const CATALOGUE: &str = include_str!("../../spot-errors/catalogue/journal_ops.toml");

fn quoted(line: &str) -> Option<String> {
    let rest = line.split('"').nth(1)?;
    Some(rest.to_owned())
}

fn constants() -> Vec<String> {
    SOURCE
        .lines()
        .filter(|l| l.starts_with("pub const ") && l.contains("\"journal_ops."))
        .filter_map(quoted)
        .collect()
}

fn catalogue_codes() -> Vec<String> {
    CATALOGUE
        .lines()
        .filter(|l| l.starts_with("code = "))
        .filter_map(quoted)
        .filter(|c| c.starts_with("journal_ops."))
        .collect()
}

#[test]
fn every_constant_is_catalogued_and_every_entry_has_a_constant() {
    let mut from_source = constants();
    let mut from_catalogue = catalogue_codes();
    assert_eq!(from_source.len(), 8, "the constants were not all found");
    from_source.sort();
    from_catalogue.sort();
    assert_eq!(from_source, from_catalogue);
}

#[test]
fn catalogue_entries_follow_the_library_internal_shape() {
    for code in catalogue_codes() {
        let name = code.trim_start_matches("journal_ops.");
        assert!(
            CATALOGUE.contains(&format!("explanation_key = \"error.journal_ops.{name}\"")),
            "{code}: explanation_key"
        );
    }
    assert_eq!(
        CATALOGUE.matches("class = \"none\"").count(),
        catalogue_codes().len()
    );
}

#[test]
fn codes_are_stable_and_never_embed_the_reason() {
    for (kind, code) in [
        (ExecKind::Denied, error::DENIED),
        (ExecKind::NotFound, error::NOT_FOUND),
        (ExecKind::Conflict, error::CONFLICT),
        (ExecKind::Unavailable, error::UNAVAILABLE),
        (ExecKind::Failed, error::FAILED),
    ] {
        let e = ExecError::new(kind, "some_reason");
        assert_eq!(e.code(), code);
        assert_eq!(OpError::Exec(e).code(), code);
    }
    let unknown = OpError::UnknownOperation { op: "x.y".into() };
    assert_eq!(unknown.code(), "journal_ops.unknown_operation");
    let bad = OpError::BadArguments {
        op: "x.y".into(),
        param: "p".into(),
        reason: "r",
    };
    assert_eq!(bad.code(), "journal_ops.bad_arguments");
    let ser = OpError::Serialise { op: "x.y".into() };
    assert_eq!(ser.code(), "journal_ops.serialise");
}
