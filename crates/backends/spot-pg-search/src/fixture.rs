//! The unit tests' catalog: one searchable DocType with facets, a protected column and a null
//! body, which the contract's own fixture does not have.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_storage_contract::{
    Catalog, Collation, EntityDef, EntityKind, FieldDef, FieldType, Name, SearchDef, SearchField,
    SearchWeight,
};

use crate::plan::{Language, ProjectionPlan};
use crate::protect::Protection;

pub(crate) fn name(text: &str) -> Name {
    Name::new(text).unwrap_or_else(|e| panic!("{e}"))
}

fn text_field(text: &str, nullable: bool) -> FieldDef {
    FieldDef {
        name: name(text),
        ty: FieldType::Text { max_chars: None },
        nullable,
    }
}

pub(crate) fn catalog() -> Catalog {
    let entity = EntityDef {
        name: name("listing"),
        kind: EntityKind::Document,
        fields: vec![
            FieldDef {
                name: name("title"),
                ty: FieldType::Text {
                    max_chars: Some(140),
                },
                nullable: false,
            },
            text_field("body", true),
            text_field("owner_note", true),
            text_field("price_band", true),
            FieldDef {
                name: name("guests"),
                ty: FieldType::Int,
                nullable: true,
            },
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: Some(SearchDef {
            language: None,
            fields: vec![
                SearchField {
                    name: name("title"),
                    weight: SearchWeight::A,
                },
                SearchField {
                    name: name("body"),
                    weight: SearchWeight::D,
                },
            ],
            facets: vec![name("price_band"), name("guests")],
            typo_tolerance: false,
        }),
    };
    Catalog::new(1, Collation::new("en").unwrap(), vec![entity]).unwrap()
}

/// The refusal of `result`, or a panic naming what should have been refused.
pub(crate) fn refusal<T>(result: Result<T, crate::SearchError>) -> crate::SearchError {
    match result {
        Ok(_) => panic!("expected a refusal"),
        Err(error) => error,
    }
}

pub(crate) fn language(tag: &str) -> Language {
    Language::new(tag).unwrap_or_else(|e| panic!("{e}"))
}

/// The plan of [`catalog`], with no protection groups.
pub(crate) fn plan() -> ProjectionPlan {
    let catalog = catalog();
    ProjectionPlan::from_entity(
        catalog.entity(&name("listing")).unwrap(),
        &language("en"),
        &Protection::new(),
    )
    .unwrap()
    .unwrap()
}
