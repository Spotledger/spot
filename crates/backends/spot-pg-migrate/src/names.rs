//! SQL names, spelled exactly as `spot-postgres` builds them. `spot-postgres` keeps its builders
//! private, so this module repeats the rules; the conformance tests prove the two agree (a schema
//! `prepare` built plans as unchanged, and a schema this crate built passes `prepare`).

use spot_storage_contract::Name;

const MAX_IDENT: usize = 63;

/// The table holding the schema's own bookkeeping.
pub const META_TABLE: &str = "spot_meta";

/// The table holding this crate's resumable progress.
pub const PROGRESS_TABLE: &str = "spot_migration_progress";

/// `name` as a quoted identifier.
pub fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `text` cut to PostgreSQL's identifier limit, with an FNV-1a hash of the whole name when it
/// has to be cut.
pub fn bounded(text: &str) -> String {
    if text.len() <= MAX_IDENT {
        return text.to_owned();
    }
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let head: String = text.chars().take(MAX_IDENT - 9).collect();
    format!("{head}_{hash:08x}")
}

/// Primary key constraint of a table.
pub fn pk_name(table: &Name) -> String {
    bounded(&format!("{table}_pkey"))
}

/// The foreign key of the link field `field` of `table`.
pub fn fk_name(table: &Name, field: &str) -> String {
    bounded(&format!("{table}_{field}_fkey"))
}

/// The foreign key of a child table's `parent` column.
pub fn parent_fk_name(child: &Name) -> String {
    fk_name(child, spot_storage_contract::schema::system::PARENT)
}

/// The check constraint limiting the length of a text column.
pub fn length_check_name(table: &Name, field: &Name) -> String {
    bounded(&format!("{table}_{field}_len"))
}

/// The index every child table gets on `(parent, idx)`.
pub fn parent_index_name(child: &Name) -> String {
    bounded(&format!("{child}_parent_idx"))
}

/// The prefix every object this crate (or a hook of it) creates must carry, the rule of
/// `tools/lints/table-prefix`.
pub const OWNED_PREFIX: &str = "spot_";

/// Whether `name` carries [`OWNED_PREFIX`].
pub fn is_owned(name: &str) -> bool {
    name.starts_with(OWNED_PREFIX)
}

/// `name` with the `spot_` prefix the object-name gate requires (`tools/lints/table-prefix`),
/// added only when it is not already there, so `spot_occupancy` is not doubled.
fn owned(name: &str) -> String {
    if name.starts_with("spot_") {
        name.to_owned()
    } else {
        format!("spot_{name}")
    }
}

/// The exclusion constraint `name` of `table` (ADR-087 section 5.3): `spot_<table>_<name>_excl`.
pub fn exclusion_name(table: &Name, name: &Name) -> String {
    bounded(&owned(&format!("{table}_{name}_excl")))
}

/// The physical name of a GiST index declared as `name` on `table` (ADR-087 section 4.3):
/// `spot_<table>_<name>_gix`.
pub fn gist_index_name(table: &Name, name: &Name) -> String {
    bounded(&owned(&format!("{table}_{name}_gix")))
}

/// The physical name of a GIN index declared as `name` on `table`: `spot_<table>_<name>_ginx`.
pub fn gin_index_name(table: &Name, name: &Name) -> String {
    bounded(&owned(&format!("{table}_{name}_ginx")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn construct_names_are_pinned_and_carry_the_prefix() {
        let occupancy = n("occupancy");
        assert_eq!(
            exclusion_name(&occupancy, &n("unit_during")),
            "spot_occupancy_unit_during_excl"
        );
        assert_eq!(
            gist_index_name(&occupancy, &n("location")),
            "spot_occupancy_location_gix"
        );
        assert_eq!(
            gin_index_name(&occupancy, &n("terms")),
            "spot_occupancy_terms_ginx"
        );
        // The ADR-087 example table is already prefixed: not doubled.
        assert_eq!(
            exclusion_name(&n("spot_occupancy"), &n("unit_during")),
            "spot_occupancy_unit_during_excl"
        );
    }

    #[test]
    fn a_long_construct_name_is_bounded_and_keeps_its_prefix() {
        let long = n(&"a".repeat(60));
        for name in [
            exclusion_name(&long, &long),
            gist_index_name(&long, &long),
            gin_index_name(&long, &long),
        ] {
            assert!(name.len() <= MAX_IDENT, "{name}");
            assert!(name.starts_with("spot_"), "{name}");
        }
        assert_ne!(
            exclusion_name(&long, &n("x_one")),
            exclusion_name(&long, &n("x_two"))
        );
    }
}
