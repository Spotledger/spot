//! The SQL names this backend creates, in one place so that `prepare`, the document store and
//! the error mapping agree on them.
//!
//! Tables and columns are the entity and field names, exactly as `spot-pg-query`'s
//! `DefaultSchemaMap` has them (the query compiler quotes the same identifiers), and the
//! collation follows `spot_pg_query::collation_name`; that crate is not a dependency yet
//! (WP-STO-03.T2 adds it), so the rule is reproduced here.
//!
//! Every name that PostgreSQL keeps in a schema-wide namespace (constraints, indexes, functions,
//! triggers) is built by [`bounded`]: at most 63 bytes, and when a name has to be cut, a hash of
//! the full name keeps it unique and stable. Constraint names are chosen so that an error can be
//! mapped back to an entity and field from the catalog alone ([`link_for_constraint`]).

use spot_storage_contract::{Catalog, Collation, EntityDef, ExclusionDef, FieldType, Name};

/// PostgreSQL's identifier limit in bytes.
const MAX_IDENT: usize = 63;

/// The table holding the schema's own bookkeeping (catalog version and collation).
pub(crate) const META_TABLE: &str = "spot_meta";

/// `name` as a quoted identifier. Storage names are lowercase letters, digits and underscores
/// (validated by `Name`), so quoting is for reserved words only, never for safety; names built
/// here are bounded by [`bounded`], which keeps the same alphabet plus uppercase `A`.
pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The SQL name of the ICU collation for a tenant's collation: `spot_` and the lowercase locale
/// with `-` replaced by `_` (`ur-PK` becomes `spot_ur_pk`). Same rule as
/// `spot_pg_query::collation_name`.
pub(crate) fn collation_name(collation: &Collation) -> String {
    let locale: String = collation
        .as_str()
        .chars()
        .map(|c| {
            if c == '-' {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    format!("spot_{locale}")
}

/// `text`, cut to PostgreSQL's identifier limit. A name that fits is returned unchanged; a longer
/// one keeps its first 53 bytes, `_` and 8 hex digits of an FNV-1a hash of the whole name.
pub(crate) fn bounded(text: &str) -> String {
    if text.len() <= MAX_IDENT {
        return text.to_owned();
    }
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    // Names are ASCII, so any byte offset is a character boundary.
    let head: String = text.chars().take(MAX_IDENT - 9).collect();
    format!("{head}_{hash:08x}")
}

/// Primary key constraint of an entity's table.
pub(crate) fn pk_name(table: &Name) -> String {
    bounded(&format!("{table}_pkey"))
}

/// The foreign key (and error-naming constraint) of the link field `field` of `table`.
pub(crate) fn fk_name(table: &Name, field: &str) -> String {
    bounded(&format!("{table}_{field}_fkey"))
}

/// The foreign key of a child table's `parent` column.
pub(crate) fn parent_fk_name(child: &Name) -> String {
    fk_name(child, spot_storage_contract::schema::system::PARENT)
}

/// The check constraint limiting the length of a text column.
pub(crate) fn length_check_name(table: &Name, field: &Name) -> String {
    bounded(&format!("{table}_{field}_len"))
}

/// The index every child table gets on `(parent, idx)`.
pub(crate) fn parent_index_name(child: &Name) -> String {
    bounded(&format!("{child}_parent_idx"))
}

/// `text` with the `spot_` prefix the object-name gate requires, added only when it is not
/// already there. Same rule as `spot-pg-migrate`'s `names::owned`.
fn owned(text: &str) -> String {
    if text.starts_with("spot_") {
        text.to_owned()
    } else {
        format!("spot_{text}")
    }
}

/// The exclusion constraint `name` of `table` (ADR-087 section 5.3): `spot_<table>_<name>_excl`.
/// Same text as `spot-pg-migrate`'s `exclusion_name`.
pub(crate) fn exclusion_name(table: &Name, name: &Name) -> String {
    bounded(&owned(&format!("{table}_{name}_excl")))
}

/// The physical name of a GiST index declared as `name` on `table`: `spot_<table>_<name>_gix`.
pub(crate) fn gist_index_name(table: &Name, name: &Name) -> String {
    bounded(&owned(&format!("{table}_{name}_gix")))
}

/// The physical name of a GIN index declared as `name` on `table`: `spot_<table>_<name>_ginx`.
pub(crate) fn gin_index_name(table: &Name, name: &Name) -> String {
    bounded(&owned(&format!("{table}_{name}_ginx")))
}

/// The exclusion a constraint name belongs to: its entity and declaration.
pub(crate) fn exclusion_for_constraint<'a>(
    catalog: &'a Catalog,
    constraint: &str,
) -> Option<(&'a EntityDef, &'a ExclusionDef)> {
    for entity in catalog.entities() {
        for def in &entity.exclusions {
            if exclusion_name(&entity.name, &def.name) == constraint {
                return Some((entity, def));
            }
        }
    }
    None
}

/// The field a foreign key constraint belongs to: `(entity, field)` for a link field, or
/// `(child entity, "parent")` for a child's parent column. `None` for any other name.
pub(crate) fn link_for_constraint(catalog: &Catalog, constraint: &str) -> Option<(Name, Name)> {
    for entity in catalog.entities() {
        for field in &entity.fields {
            if matches!(field.ty, FieldType::Link { .. })
                && fk_name(&entity.name, field.name.as_str()) == constraint
            {
                return Some((entity.name.clone(), field.name.clone()));
            }
        }
        if matches!(entity.kind, spot_storage_contract::EntityKind::Child { .. })
            && parent_fk_name(&entity.name) == constraint
        {
            let parent = Name::new(spot_storage_contract::schema::system::PARENT).ok()?;
            return Some((entity.name.clone(), parent));
        }
    }
    None
}

/// The entity whose primary key constraint is `constraint`.
pub(crate) fn entity_for_pk<'a>(catalog: &'a Catalog, constraint: &str) -> Option<&'a EntityDef> {
    catalog.entities().find(|e| pk_name(&e.name) == constraint)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use spot_storage_contract::{ChildTableDef, EntityKind, FieldDef, LinkCheck, LinkTarget};

    use super::*;

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn short_names_are_kept_and_long_ones_are_bounded_and_distinct() {
        assert_eq!(bounded("invoice_party_fkey"), "invoice_party_fkey");
        let a = bounded(&format!("{}_a_fkey", "x".repeat(60)));
        let b = bounded(&format!("{}_b_fkey", "x".repeat(60)));
        assert!(a.len() <= MAX_IDENT && b.len() <= MAX_IDENT);
        assert_ne!(a, b);
        assert_eq!(a, bounded(&format!("{}_a_fkey", "x".repeat(60))));
    }

    #[test]
    fn construct_names_are_those_spot_pg_migrate_plans() {
        // Pinned to `spot-pg-migrate`'s `names` tests: the two crates must agree.
        assert_eq!(
            exclusion_name(&n("occupancy"), &n("unit_during")),
            "spot_occupancy_unit_during_excl"
        );
        assert_eq!(
            gist_index_name(&n("occupancy"), &n("location")),
            "spot_occupancy_location_gix"
        );
        assert_eq!(
            gin_index_name(&n("occupancy"), &n("terms")),
            "spot_occupancy_terms_ginx"
        );
        assert_eq!(
            exclusion_name(&n("spot_occupancy"), &n("unit_during")),
            "spot_occupancy_unit_during_excl"
        );
        let long = n(&"a".repeat(60));
        assert!(exclusion_name(&long, &long).len() <= MAX_IDENT);
    }

    #[test]
    fn collation_names_follow_the_locale() {
        for (locale, want) in [
            ("en", "spot_en"),
            ("ur-PK", "spot_ur_pk"),
            ("sr-Latn-RS", "spot_sr_latn_rs"),
        ] {
            assert_eq!(collation_name(&Collation::new(locale).unwrap()), want);
        }
    }

    #[test]
    fn constraint_names_map_back_to_entity_and_field() {
        let link = |t: &str| FieldType::Link {
            target: LinkTarget::Entity(n(t)),
            check: LinkCheck::Database,
        };
        let long = "a_field_with_a_rather_long_name_that_goes_on_and_on";
        let entities = vec![
            EntityDef {
                name: n("invoice"),
                kind: EntityKind::Document,
                fields: vec![
                    FieldDef {
                        name: n("party"),
                        ty: link("invoice"),
                        nullable: true,
                    },
                    FieldDef {
                        name: n(long),
                        ty: link("invoice"),
                        nullable: true,
                    },
                ],
                children: vec![ChildTableDef {
                    field: n("lines"),
                    entity: n("invoice_line"),
                }],
                indexes: vec![],
                exclusions: vec![],
                search: None,
            },
            EntityDef {
                name: n("invoice_line"),
                kind: EntityKind::Child {
                    parent: n("invoice"),
                },
                fields: vec![],
                children: vec![],
                indexes: vec![],
                exclusions: vec![],
                search: None,
            },
        ];
        let catalog = Catalog::new(1, Collation::new("en").unwrap(), entities).unwrap();
        let hit =
            |c: &str| link_for_constraint(&catalog, c).map(|(e, f)| (e.to_string(), f.to_string()));
        assert_eq!(
            hit("invoice_party_fkey"),
            Some(("invoice".into(), "party".into()))
        );
        assert_eq!(
            hit(&fk_name(&n("invoice"), long)),
            Some(("invoice".into(), long.into()))
        );
        assert_eq!(
            hit("invoice_line_parent_fkey"),
            Some(("invoice_line".into(), "parent".into()))
        );
        assert_eq!(hit("something_else"), None);
        assert_eq!(
            entity_for_pk(&catalog, "invoice_line_pkey").map(|e| e.name.to_string()),
            Some("invoice_line".into())
        );
    }
}
