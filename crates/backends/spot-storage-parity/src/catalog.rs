//! The catalogue every case runs against: one document entity, `item`, with a field of every
//! storage type at the limits the rules need, and one child table, `lines`.

use spot_storage_contract::{
    Catalog, ChildTableDef, Collation, EntityDef, EntityKind, ExclusionDef, FieldDef, FieldType,
    GEO_SRID, IndexDef, IndexMethod, LinkCheck, LinkTarget, Name, RangeOf, SearchDef, SearchField,
    SearchWeight, StorageResult,
};

fn name(s: &str) -> StorageResult<Name> {
    Name::new(s)
}

fn field(n: &str, ty: FieldType, nullable: bool) -> StorageResult<FieldDef> {
    Ok(FieldDef {
        name: name(n)?,
        ty,
        nullable,
    })
}

/// The declared fields of `item`, with their types: `code` (text, 40 characters, required,
/// unique), `short` (text, 10 characters), `note` (unlimited text), `money` (decimal 18,2),
/// `small` (decimal 4,2), `wide` (decimal 38,10), `qty` (integer), `flag` (boolean), `day`
/// (date), `clock` (time), `seen` (instant), `doc` (JSON), `blob` (bytes) and `ref` (a
/// database-checked link to another `item`). The pair (`short`, `qty`) is unique. All but
/// `code` are nullable.
pub const ITEM_FIELDS: [&str; 14] = [
    "code", "short", "note", "money", "small", "wide", "qty", "flag", "day", "clock", "seen",
    "doc", "blob", "ref",
];

/// The declared fields of the child entity `item_line`: `label` (text) and `amount` (decimal 18,2).
pub const LINE_FIELDS: [&str; 2] = ["label", "amount"];

/// The fields of `item` in the geo catalogue: the base fields plus `spot`, a WGS-84 point
/// (ADR-087 section 4).
pub const GEO_FIELDS: [&str; 15] = [
    "code", "short", "note", "money", "small", "wide", "qty", "flag", "day", "clock", "seen",
    "doc", "blob", "ref", "spot",
];

/// The fields of `item` in the range catalogue: the base fields plus `during` (a half-open date
/// range), `room` (the equality column the exclusion is scoped by) and `state` (a hold's
/// `tentative` or `confirmed` state, ADR-087 sections 5 and 6).
pub const RANGE_FIELDS: [&str; 17] = [
    "code", "short", "note", "money", "small", "wide", "qty", "flag", "day", "clock", "seen",
    "doc", "blob", "ref", "during", "room", "state",
];

/// The declared fields of `item` for the catalogue a case names; `exec` fills every one of them
/// with null when a setup line omits it. An unknown name is the main catalogue.
pub fn item_fields(catalog: Option<&str>) -> &'static [&'static str] {
    match catalog {
        Some("geo") => &GEO_FIELDS,
        Some("range") => &RANGE_FIELDS,
        _ => &ITEM_FIELDS,
    }
}

/// The base `item` entity: one field of every storage type at the limits the rules need, the
/// pair of unique indexes, and the `lines` child table. The ADR-087 catalogues below are this
/// entity with the construct's field and declaration added.
fn base_item() -> StorageResult<EntityDef> {
    let text = |max: Option<u32>| FieldType::Text { max_chars: max };
    let dec = |precision, scale| FieldType::Decimal { precision, scale };
    Ok(EntityDef {
        name: name("item")?,
        kind: EntityKind::Document,
        fields: vec![
            field("code", text(Some(40)), false)?,
            field("short", text(Some(10)), true)?,
            field("note", text(None), true)?,
            field("money", dec(18, 2), true)?,
            field("small", dec(4, 2), true)?,
            field("wide", dec(38, 10), true)?,
            field("qty", FieldType::Int, true)?,
            field("flag", FieldType::Bool, true)?,
            field("day", FieldType::Date, true)?,
            field("clock", FieldType::Time, true)?,
            field("seen", FieldType::DateTime, true)?,
            field("doc", FieldType::Json, true)?,
            field("blob", FieldType::Bytes, true)?,
            field(
                "ref",
                FieldType::Link {
                    target: LinkTarget::Entity(name("item")?),
                    check: LinkCheck::Database,
                },
                true,
            )?,
        ],
        children: vec![ChildTableDef {
            field: name("lines")?,
            entity: name("item_line")?,
        }],
        indexes: vec![
            IndexDef {
                name: name("item_code")?,
                fields: vec![name("code")?],
                unique: true,
                method: IndexMethod::BTree,
                opclass: None,
                include: vec![],
            },
            IndexDef {
                name: name("item_pair")?,
                fields: vec![name("short")?, name("qty")?],
                unique: true,
                method: IndexMethod::BTree,
                opclass: None,
                include: vec![],
            },
        ],
        exclusions: vec![],
        search: None,
    })
}

/// The child entity `item_line`.
fn base_line() -> StorageResult<EntityDef> {
    Ok(EntityDef {
        name: name("item_line")?,
        kind: EntityKind::Child {
            parent: name("item")?,
        },
        fields: vec![
            field("label", FieldType::Text { max_chars: None }, true)?,
            field(
                "amount",
                FieldType::Decimal {
                    precision: 18,
                    scale: 2,
                },
                true,
            )?,
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    })
}

/// Builds the catalogue with the tenant collation `locale`.
pub fn parity_catalog(locale: &str) -> StorageResult<Catalog> {
    Catalog::new(1, Collation::new(locale)?, vec![base_item()?, base_line()?])
}

/// The catalogue for the spatial cases: `item` with `spot`, a `geo` point (ADR-087 section 4).
/// The fake stores and round-trips the value; only the filters need PostGIS.
pub fn geo_catalog(locale: &str) -> StorageResult<Catalog> {
    let mut item = base_item()?;
    item.fields
        .push(field("spot", FieldType::Geo { srid: GEO_SRID }, true)?);
    Catalog::new(1, Collation::new(locale)?, vec![item, base_line()?])
}

/// The catalogue for the range, exclusion and hold cases: `item` with `during` (a half-open date
/// range), `room` (the equality column the exclusion is scoped by) and `state` (a hold's
/// `tentative`/`confirmed` state), and one exclusion over (`room`, `during`) — ADR-087 sections
/// 5.2 and 6.1. A scope column makes the constraint need `btree_gist`.
pub fn range_catalog(locale: &str) -> StorageResult<Catalog> {
    let mut item = base_item()?;
    item.fields.push(field(
        "during",
        FieldType::Range { of: RangeOf::Date },
        true,
    )?);
    item.fields.push(field(
        "room",
        FieldType::Text {
            max_chars: Some(20),
        },
        true,
    )?);
    item.fields.push(field(
        "state",
        FieldType::Text {
            max_chars: Some(20),
        },
        true,
    )?);
    item.exclusions.push(ExclusionDef {
        name: name("item_room_during")?,
        field: name("during")?,
        scope: vec![name("room")?],
        message: "Those dates are already taken for this room".to_owned(),
    });
    Catalog::new(1, Collation::new(locale)?, vec![item, base_line()?])
}

/// The catalogue for the search cases: `item` with a search declaration over `note` (ADR-087
/// section 7.2). No backend serves search yet (SEM-SCH-04: the projection arrives with
/// WP-STO-13.T1), so the cases that use this catalogue assert the refusal.
pub fn search_catalog(locale: &str) -> StorageResult<Catalog> {
    let mut item = base_item()?;
    item.search = Some(SearchDef {
        language: None,
        fields: vec![SearchField {
            name: name("note")?,
            weight: SearchWeight::A,
        }],
        facets: vec![],
        typo_tolerance: false,
    });
    Catalog::new(1, Collation::new(locale)?, vec![item, base_line()?])
}

/// A catalogue of 63-character names (the longest a storage name can be): two entities that
/// share their first 62 characters, each with a 63-character field and a unique index of 63
/// characters. A backend that derives table, index or constraint names from these and
/// truncates them silently makes the two collide.
pub fn long_names_catalog(locale: &str) -> StorageResult<Catalog> {
    let long = |suffix: &str| format!("{}{suffix}", "x".repeat(62));
    let entity = |suffix: &str| -> StorageResult<EntityDef> {
        Ok(EntityDef {
            name: name(&long(suffix))?,
            kind: EntityKind::Document,
            fields: vec![field(
                &long(suffix),
                FieldType::Text { max_chars: None },
                false,
            )?],
            children: vec![],
            indexes: vec![IndexDef {
                name: name(&long(suffix))?,
                fields: vec![name(&long(suffix))?],
                unique: true,
                method: spot_storage_contract::IndexMethod::BTree,
                opclass: None,
                include: vec![],
            }],
            exclusions: vec![],
            search: None,
        })
    };
    Catalog::new(1, Collation::new(locale)?, vec![entity("1")?, entity("2")?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_valid() {
        let c = parity_catalog("en");
        assert!(c.is_ok(), "{c:?}");
        assert!(parity_catalog("not a locale").is_err());
    }

    /// The construct catalogues are the same `item` with one construct added, and the field
    /// lists in [`item_fields`] name exactly what `exec` fills in for them.
    #[test]
    fn the_construct_catalogues_are_valid_and_match_their_field_lists() -> StorageResult<()> {
        let geo = geo_catalog("en")?;
        let range = range_catalog("en")?;
        let search = search_catalog("en")?;
        let item = name("item")?;
        let declared = |c: &Catalog| -> Vec<String> {
            c.entity(&item)
                .map(|e| e.fields.iter().map(|f| f.name.to_string()).collect())
                .unwrap_or_default()
        };
        assert_eq!(declared(&geo), GEO_FIELDS.to_vec());
        assert_eq!(declared(&range), RANGE_FIELDS.to_vec());
        assert_eq!(declared(&search), ITEM_FIELDS.to_vec());
        assert_eq!(item_fields(None), &ITEM_FIELDS);
        assert_eq!(item_fields(Some("main")), &ITEM_FIELDS);
        assert_eq!(item_fields(Some("geo")), &GEO_FIELDS);
        assert_eq!(item_fields(Some("range")), &RANGE_FIELDS);
        // The range catalogue declares the exclusion the hold and exclusion cases rely on.
        assert_eq!(range.entity(&item)?.exclusions.len(), 1);
        assert!(search.entity(&item)?.search.is_some());
        Ok(())
    }
}
