//! Property test: random valid definition sets compile, and recompiling gives equal output.
#![allow(missing_docs)]

mod common;

use common::{build, simple};
use proptest::prelude::*;
use spot_meta::{Definition, FieldSpec, FieldType, IndexSpec};
use spot_storage_contract::schema::Collation;

/// One generated field: a type code plus flags.
#[derive(Clone, Debug)]
struct Gen {
    code: u8,
    required: bool,
    unique: bool,
    index: bool,
    scale: u8,
}

fn gens() -> impl Strategy<Value = Vec<Gen>> {
    prop::collection::vec(
        (
            0u8..16,
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            0u8..=18,
        )
            .prop_map(|(code, required, unique, index, scale)| Gen {
                code,
                required,
                unique,
                index,
                scale,
            }),
        0..8,
    )
}

/// `n` DocTypes `d0..dn`. Even ones may be posted; `d{i}` gets a child DocType `d{i}_row`
/// when `with_child[i]`. Links point at any DocType in the set.
fn build_set(
    n: usize,
    fields: &[Vec<Gen>],
    posted: &[bool],
    with_child: &[bool],
) -> Vec<Definition> {
    let mut defs = Vec::new();
    for i in 0..n {
        let mut specs = Vec::new();
        for (j, g) in fields[i].iter().enumerate() {
            let name = format!("f{j}");
            let mut f = match g.code {
                0 => FieldSpec::new(&name, FieldType::Bool),
                1 => FieldSpec::new(&name, FieldType::Int),
                2 => {
                    let mut f = FieldSpec::new(&name, FieldType::Decimal);
                    f.scale = Some(g.scale);
                    f
                }
                3 => FieldSpec::new(&name, FieldType::Text),
                4 => FieldSpec::new(&name, FieldType::LongText),
                5 => {
                    let mut f = FieldSpec::new(&name, FieldType::Select);
                    f.options = Some("a\nb".into());
                    f
                }
                6 => FieldSpec::new(&name, FieldType::Date),
                7 => {
                    let mut f = FieldSpec::new(&name, FieldType::Link);
                    f.options = Some(format!("d{}", (i + j) % n));
                    f
                }
                8 => FieldSpec::new(&name, FieldType::Json),
                // ADR-068 types and formats: a format never changes how the set compiles.
                9 => with_format(FieldType::Text, &name, "email"),
                10 => {
                    let mut f = with_format(FieldType::LongText, &name, "code");
                    f.syntax = Some("sql".into());
                    f
                }
                11 => {
                    let mut f = with_format(FieldType::Int, &name, "rating");
                    f.max = Some(1 + g.scale % 10);
                    f
                }
                12 => {
                    let mut f = with_format(FieldType::Decimal, &name, "percent");
                    f.scale = Some(g.scale);
                    f
                }
                13 => FieldSpec::new(&name, FieldType::Secret),
                14 => FieldSpec::new(&name, FieldType::Attach),
                _ => {
                    let cur = format!("cur{j}");
                    let mut link = FieldSpec::new(&cur, FieldType::Link);
                    link.options = Some(format!("d{}", (i + j) % n));
                    specs.push(link);
                    let mut f = with_format(FieldType::Decimal, &name, "currency");
                    f.scale = Some(g.scale);
                    f.currency_field = Some(cur);
                    f
                }
            };
            f.required = g.required;
            // A secret has no column to index; a long text cannot be unique.
            f.unique = g.unique && !matches!(g.code, 4 | 10 | 13);
            f.index = g.index && g.code != 13;
            specs.push(f);
        }
        if with_child[i] {
            // Odd DocTypes use a multi-pick: its child holds exactly one link field.
            let kind = if i % 2 == 1 {
                FieldType::TableMultiselect
            } else {
                FieldType::Table
            };
            let mut t = FieldSpec::new("rows", kind);
            t.options = Some(format!("d{i}_row"));
            specs.push(t);
        }
        let mut d = simple(&format!("d{i}"), specs);
        d.is_posted = posted[i];
        if fields[i].first().is_some_and(|g| g.code != 13) {
            d.indexes.push(IndexSpec {
                fields: vec!["f0".into()],
                unique: false,
            });
        }
        defs.push(d);
        if with_child[i] {
            let row_field = if i % 2 == 1 {
                let mut link = FieldSpec::new("pick", FieldType::Link);
                link.options = Some("d0".into());
                link
            } else {
                FieldSpec::new("qty", FieldType::Int)
            };
            let mut row = simple(&format!("d{i}_row"), vec![row_field]);
            row.is_child = true;
            defs.push(row);
        }
    }
    // The platform file record that `attach` fields point at.
    defs.push(simple(
        "file",
        vec![FieldSpec::new("file_name", FieldType::Text)],
    ));
    defs
}

fn with_format(ty: FieldType, name: &str, format: &str) -> FieldSpec {
    let mut f = FieldSpec::new(name, ty);
    f.format = Some(format.into());
    f
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn random_valid_sets_compile_deterministically(
        n in 1usize..5,
        fields in prop::collection::vec(gens(), 4),
        posted in prop::collection::vec(any::<bool>(), 4),
        with_child in prop::collection::vec(any::<bool>(), 4),
    ) {
        let defs = build_set(n, &fields, &posted, &with_child);
        let meta = build(defs.clone()).map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        let again = build(defs).map_err(|e| TestCaseError::fail(format!("{e:?}")))?;

        let collation = Collation::new("en").map_err(|e| TestCaseError::fail(e.to_string()))?;
        let a = meta.catalog(collation.clone()).map_err(|e| TestCaseError::fail(e.to_string()))?;
        let b = again.catalog(collation).map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert_eq!(a, b);
        let views = |m: &spot_meta::EffectiveMeta| m.doctypes().map(|d| (**d).clone()).collect::<Vec<_>>();
        prop_assert_eq!(views(&meta), views(&again));
    }
}
