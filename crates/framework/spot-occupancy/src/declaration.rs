//! The occupancy entity, declared through `spot-meta` (ADR-087 section 6.1).
//!
//! ```text
//! entity "occupancy":  id, unit (link), during (range), state (tentative|confirmed|cancelled),
//!                      holder (user:<id> or consumer:<id>), expires_at (instant, null when confirmed)
//! EXCLUDE USING gist ("unit" WITH =, "during" WITH &&)
//! ```
//!
//! One table, one exclusion constraint. The constraint, not any application check, decides who
//! gets the dates: two writers that race for the same unit and overlapping dates meet it, and
//! one of them fails with `ExclusionViolation`.
//!
//! The `cancelled` value is declared because ADR-087 lists it, but this crate never stores it:
//! only the absence of the row frees the dates (section 6.2), so cancelling a booking deletes
//! the row and captures `occupancy_cancelled`.

use spot_meta::{Definition, ExclusionSpec, FieldSpec, FieldType, IndexSpec};
use spot_storage_contract::{
    Catalog, ErrorKind, FieldType as Storage, Name, RangeOf, StorageError, StorageResult,
};

/// The entity's name.
pub const ENTITY: &str = "occupancy";
/// The link to the bookable resource; the scope of the exclusion.
pub const UNIT: &str = "unit";
/// The half-open range `[start, end)` the record occupies.
pub const DURING: &str = "during";
/// `tentative`, `confirmed` (or the declared but never stored `cancelled`).
pub const STATE: &str = "state";
/// The typed principal reference, `user:<id>` or `consumer:<id>`.
pub const HOLDER: &str = "holder";
/// The instant a tentative hold stops being a promise; null once confirmed.
pub const EXPIRES_AT: &str = "expires_at";
/// The name `spot-meta` gives the exclusion (`ex_<doctype>_<field>`).
pub const EXCLUSION: &str = "ex_occupancy_during";
/// The state of a hold that has not been confirmed.
pub const STATE_TENTATIVE: &str = "tentative";
/// The state of a confirmed booking.
pub const STATE_CONFIRMED: &str = "confirmed";
/// Declared in the state list but never stored: cancelling deletes the row.
pub const STATE_CANCELLED: &str = "cancelled";

/// What the occupied range is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Span {
    /// Whole days (`date_range`): hotel nights, desk days.
    Dates,
    /// Instants (`instant_range`): appointments, equipment slots.
    Instants,
}

/// The occupancy DocType for a deployment whose bookable resource is the DocType
/// `unit_doctype`. Add it to the app's definitions; `spot-meta` compiles it into the storage
/// catalog, the exclusion included.
pub fn definition(unit_doctype: &str, span: Span) -> Definition {
    let mut d = Definition::new(ENTITY);

    let mut unit = FieldSpec::new(UNIT, FieldType::Link);
    unit.options = Some(unit_doctype.to_owned());
    // A record with a null scope never conflicts, so the scope must be required.
    unit.required = true;

    let mut during = FieldSpec::new(DURING, FieldType::Range);
    during.format = Some(
        match span {
            Span::Dates => "date_range",
            Span::Instants => "instant_range",
        }
        .to_owned(),
    );
    during.required = true;

    let mut state = FieldSpec::new(STATE, FieldType::Select);
    state.options = Some(format!(
        "{STATE_TENTATIVE}\n{STATE_CONFIRMED}\n{STATE_CANCELLED}"
    ));
    state.required = true;

    let mut holder = FieldSpec::new(HOLDER, FieldType::Text);
    holder.max_chars = Some(crate::model::MAX_HOLDER_CHARS);
    holder.required = true;

    let expires_at = FieldSpec::new(EXPIRES_AT, FieldType::DateTime);

    d.fields = vec![unit, during, state, holder, expires_at];
    // The sweeper's filter (state, expires_at) is a transactional query: it needs an index.
    d.indexes = vec![IndexSpec {
        fields: vec![STATE.to_owned(), EXPIRES_AT.to_owned()],
        unique: false,
    }];
    d.exclusions = vec![ExclusionSpec {
        field: DURING.to_owned(),
        scope: vec![UNIT.to_owned()],
        message: "Those dates are already taken for this unit".to_owned(),
    }];
    d
}

/// The storage names this crate reads and writes, checked once against the catalog.
#[derive(Clone, Debug)]
pub struct Cols {
    /// The entity.
    pub entity: Name,
    /// The unit link.
    pub unit: Name,
    /// The range.
    pub during: Name,
    /// The state.
    pub state: Name,
    /// The holder.
    pub holder: Name,
    /// The expiry.
    pub expires_at: Name,
    /// What the `during` range is a range of (dates or instants).
    pub range_of: RangeOf,
}

impl Cols {
    /// Resolves the names and refuses a catalog that does not declare the occupancy entity as
    /// this crate needs it: the five fields, a range `during`, and the exclusion over
    /// `(unit, during)`. Failing here, at construction, is loud; a catalog without the
    /// constraint would let two writers double-book silently.
    pub fn check(catalog: &Catalog) -> StorageResult<Cols> {
        let cols = Cols {
            entity: Name::new(ENTITY)?,
            unit: Name::new(UNIT)?,
            during: Name::new(DURING)?,
            state: Name::new(STATE)?,
            holder: Name::new(HOLDER)?,
            expires_at: Name::new(EXPIRES_AT)?,
            range_of: RangeOf::Date,
        };
        let entity = catalog.entity(&cols.entity)?;
        for f in [
            &cols.unit,
            &cols.during,
            &cols.state,
            &cols.holder,
            &cols.expires_at,
        ] {
            entity.field(f)?;
        }
        let range_of = match &entity.field(&cols.during)?.ty {
            Storage::Range { of } if matches!(of, RangeOf::Date | RangeOf::DateTime) => *of,
            other => {
                return Err(invalid(format!(
                    "`occupancy.during` must be a date or instant range, not {other:?}"
                )));
            }
        };
        let guarded = entity
            .exclusions
            .iter()
            .any(|x| x.field == cols.during && x.scope == [cols.unit.clone()]);
        if !guarded {
            return Err(invalid(
                "`occupancy` declares no exclusion over (unit, during): without it the \
                 database cannot refuse a double booking"
                    .to_owned(),
            ));
        }
        Ok(Cols { range_of, ..cols })
    }
}

fn invalid(explanation: String) -> StorageError {
    StorageError::new(ErrorKind::InvalidSchema, explanation)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use spot_meta::{Customisations, Registry};
    use spot_storage_contract::Collation;

    fn room() -> Definition {
        let mut room = Definition::new("room");
        room.fields = vec![FieldSpec::new("name", FieldType::Text)];
        room
    }

    fn catalog(span: Span) -> Catalog {
        let meta = Registry::build(
            vec![room(), definition("room", span)],
            &Customisations::new(),
        )
        .expect("the occupancy definition builds");
        meta.catalog(Collation::new("und").expect("collation"))
            .expect("the definition compiles")
    }

    #[test]
    fn the_definition_compiles_to_the_exclusion_over_unit_and_during() {
        let cat = catalog(Span::Dates);
        let cols = Cols::check(&cat).expect("the compiled catalog satisfies the crate");
        assert_eq!(cols.range_of, RangeOf::Date);
        let entity = cat.entity(&cols.entity).expect("entity");
        assert_eq!(entity.exclusions.len(), 1);
        assert_eq!(entity.exclusions[0].name.as_str(), EXCLUSION);
    }

    #[test]
    fn an_instant_span_compiles_to_a_datetime_range() {
        let cols = Cols::check(&catalog(Span::Instants)).expect("check");
        assert_eq!(cols.range_of, RangeOf::DateTime);
    }

    #[test]
    fn a_catalog_without_the_entity_is_refused() {
        let meta = Registry::build(vec![room()], &Customisations::new()).expect("build");
        let cat = meta
            .catalog(Collation::new("und").expect("collation"))
            .expect("catalog");
        let err = Cols::check(&cat).expect_err("no occupancy entity");
        assert_eq!(err.kind, ErrorKind::UnknownEntity);
    }
}
