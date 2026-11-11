//! Shared helpers for the integration tests.
#![allow(dead_code, missing_docs)]

use std::path::PathBuf;

use spot_meta::{
    Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, MetaError, Registry, load_dir,
};

pub fn fixtures() -> Vec<Definition> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/doctypes");
    load_dir(&dir).unwrap_or_else(|e| panic!("fixtures do not load: {e:?}"))
}

pub fn build(defs: Vec<Definition>) -> Result<EffectiveMeta, Vec<MetaError>> {
    Registry::build(defs, &Customisations::new())
}

pub fn effective() -> EffectiveMeta {
    build(fixtures()).unwrap_or_else(|e| panic!("fixtures do not build: {e:?}"))
}

pub fn simple(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut d = Definition::new(name);
    d.fields = fields;
    d
}

pub fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

pub fn of(ty: FieldType, name: &str, options: Option<&str>) -> FieldSpec {
    let mut f = FieldSpec::new(name, ty);
    f.options = options.map(str::to_owned);
    f
}

/// The kinds of the errors, for matching.
pub fn kinds(errors: &[MetaError]) -> Vec<spot_meta::MetaErrorKind> {
    errors.iter().map(|e| e.kind).collect()
}
