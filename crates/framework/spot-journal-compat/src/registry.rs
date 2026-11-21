//! Today's upcaster registry, and the shape changes the releases since the frozen one declared.
//!
//! The registry is the **table** every check walks: one row per `(entity, from, to)` step, built
//! with the declarative builders of `spot-image-upcast` so a step is data (the field names and
//! defaults) rather than a hand-written rewrite. Three entities reach schema 3 through it:
//!
//! | entity | step | what changed |
//! | --- | --- | --- |
//! | `account` | 1 → 2 | `title` renamed to `name` |
//! | `account` | 2 → 3 | `currency_code` added, default `PKR` |
//! | `invoice` | 2 → 3 | `currency` dropped, `currency_code` added, default `PKR` |
//! | `sales_order` | 1 → 2 | `warehouse` moved into every `lines` row |
//! | `sales_order` | 2 → 3 | `currency_code` added, default `PKR` |
//!
//! [`declared_shape_changes`] is the other half of the pair: the rows the catalog patches across
//! those releases declare. [`gate_findings`] runs `check_patch` over the two, which is the CI
//! check that fails when a patch ships a shape change with no upcaster to rewrite the images
//! written before it (ADR-061 s6). [`crate::generator::key`] holds the fixture's throwaway signing key;
//! this module holds no key, no clock and no randomness.

use serde_json::json;
use spot_image_upcast::{
    CheckFinding, Registry, ShapeChange, ShapeChangeKind, Step, Upcaster, add_field_with_default,
    check_patch, drop_field, move_field_to_child, rename_field,
};

use crate::error::CompatError;

/// One row of the upcaster table: the entity, the schema it starts at, the schema it reaches and
/// the rewrite.
pub type Row = (&'static str, u64, u64, Step);

/// `invoice` from schema 2 to 3: `currency` is dropped and `currency_code` takes its place with
/// the default the patch declares.
///
/// The registry holds one step per `(entity, from)`, so the two changes of the one patch are one
/// composite step: dropping is idempotent and adding never overwrites, so running the pair is
/// still a function of the image alone.
fn invoice_currency_code() -> Step {
    let drop = drop_field("currency");
    let add = add_field_with_default("currency_code", json!("PKR"));
    Box::new(move |image| {
        drop(image)?;
        add(image)
    })
}

/// The upcaster table, in the order the releases shipped it.
///
/// Every step is a pure rewrite: no clock, no random source, no key, no float.
#[must_use]
pub fn rows() -> Vec<Row> {
    vec![
        // Release 2: `account.title` became `account.name`, keeping its value.
        ("account", 1, 2, rename_field("title", "name")),
        // Release 3: amounts carry a currency code; the default is the one the patch names.
        (
            "account",
            2,
            3,
            add_field_with_default("currency_code", json!("PKR")),
        ),
        // Release 3: `invoice.currency` was replaced by `currency_code`.
        ("invoice", 2, 3, invoice_currency_code()),
        // Release 2: the warehouse a sales order was placed in became a property of each line.
        (
            "sales_order",
            1,
            2,
            move_field_to_child("lines", "warehouse", "warehouse"),
        ),
        (
            "sales_order",
            2,
            3,
            add_field_with_default("currency_code", json!("PKR")),
        ),
    ]
}

/// The table as a registry, leaving out the rows `omitted` names.
///
/// The omission exists for one test: the rebuild must refuse a journal an entity's chain cannot
/// carry, before it applies anything, and that is only provable by taking a step out. The CI gate
/// and every other caller pass `&[]`.
///
/// # Errors
/// [`CompatError`] when the table itself is not registrable (a duplicate or a backwards step),
/// which would be a bug in this crate.
pub fn registry_without(omitted: &[(&str, u64)]) -> Result<Registry, CompatError> {
    let mut registry = Registry::new();
    for (entity, from, to, step) in rows() {
        if omitted
            .iter()
            .any(|(skip_entity, skip_from)| *skip_entity == entity && *skip_from == from)
        {
            continue;
        }
        registry
            .register(Upcaster::new(entity, from, to, step))
            .map_err(|error| {
                CompatError::new(format!("the upcaster table is not registrable: {error}"))
            })?;
    }
    Ok(registry)
}

/// Today's registry: the whole table.
///
/// # Errors
/// [`CompatError`] when the table is not registrable, which would be a bug in this crate.
pub fn today() -> Result<Registry, CompatError> {
    registry_without(&[])
}

/// The shape changes the catalog patches made since the release the fixture was written by.
///
/// They are the rows a patch declares to the migration tooling, in the vocabulary
/// `ShapeChangeKind` shares with `spot-migration-plan`. Additive rows (`FieldAdded`,
/// `DocTypeAdded`) need no upcaster; every other row does.
#[must_use]
pub fn declared_shape_changes() -> Vec<ShapeChange> {
    vec![
        // Release 2: the account's display name was a `title`, now it is a `name`.
        ShapeChange::new("account", 1, 2, ShapeChangeKind::FieldRenamed),
        // Release 2: the order's warehouse moved from the header into each line.
        ShapeChange::new("sales_order", 1, 2, ShapeChangeKind::FieldRemoved),
        ShapeChange::new("sales_order", 1, 2, ShapeChangeKind::FieldAdded),
        // Release 3: free-text currency gave way to a declared code, on every entity that holds
        // an amount.
        ShapeChange::new("account", 2, 3, ShapeChangeKind::FieldAdded),
        ShapeChange::new("invoice", 2, 3, ShapeChangeKind::FieldRemoved),
        ShapeChange::new("invoice", 2, 3, ShapeChangeKind::FieldAdded),
        ShapeChange::new("sales_order", 2, 3, ShapeChangeKind::FieldAdded),
    ]
}

/// Every declared shape change that today's registry has no upcaster for.
///
/// This is the CI gate: an empty result means every change that rewrites or removes a stored
/// value can still be carried from the schema an old image was written under. `check_shapes`
/// exits non-zero when it is not empty.
///
/// # Errors
/// [`CompatError`] when the table is not registrable, which would be a bug in this crate.
pub fn gate_findings() -> Result<Vec<CheckFinding>, CompatError> {
    gate_findings_omitting(&[])
}

/// [`gate_findings`] with the rows `omitted` names left out of the table.
///
/// The omission is what makes the gate's failure provable: `check_shapes --omit invoice:2` reports
/// the change that step would have carried. The CI job passes no omissions.
///
/// # Errors
/// [`CompatError`] when the table is not registrable, which would be a bug in this crate.
pub fn gate_findings_omitting(omitted: &[(&str, u64)]) -> Result<Vec<CheckFinding>, CompatError> {
    let registry = registry_without(omitted)?;
    Ok(check_patch(&registry, &declared_shape_changes()))
}
