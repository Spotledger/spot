//! Rows to ledger records and back: the stored text and `jsonb` forms, and the corruption checks
//! that make an unreadable row a fail-closed error rather than a default.
//!
//! The ledger's structured values — the effect class, the payload reference and the hold state —
//! are stored as `jsonb` rendered from the same `serde` types the in-memory ledger holds, so a
//! stored row round-trips exactly. They are compared as `jsonb` too: the hold's compare-and-set
//! matches the whole state, not a status word, so `Released { at: 10 }` and `Released { at: 20 }`
//! are different values.

use serde::Serialize;
use serde::de::DeserializeOwned;
use spot_core::SiteId;
use spot_effect_hold::{
    EffectClass, EffectId, EffectRecord, EffectState, HoldState, PayloadRef, Sequence,
};
use tokio_postgres::Row;

use crate::error;

/// The `jsonb` document a hold state is stored as.
///
/// The envelope gives `Open` (a serde unit variant, which would otherwise be a bare JSON string)
/// and the struct variants one shape, so the table's shape check and a human inspecting a row see
/// the same thing for every state.
#[derive(Serialize)]
struct HoldEnvelope<'a> {
    hold: &'a HoldState,
}

/// The stored form of a hold state, read back.
#[derive(serde::Deserialize)]
struct StoredHold {
    hold: HoldState,
}

/// The stored text of an effect state.
pub(crate) fn state_text(state: EffectState) -> &'static str {
    match state {
        EffectState::Held => "held",
        EffectState::Releasing => "releasing",
        EffectState::Dispatched => "dispatched",
        EffectState::Discarded => "discarded",
        EffectState::NeedsConfirmation => "needs_confirmation",
    }
}

/// The effect state a `status` text names.
pub(crate) fn state_from_text(text: &str) -> Result<EffectState, spot_effect_hold::Error> {
    match text {
        "held" => Ok(EffectState::Held),
        "releasing" => Ok(EffectState::Releasing),
        "dispatched" => Ok(EffectState::Dispatched),
        "discarded" => Ok(EffectState::Discarded),
        "needs_confirmation" => Ok(EffectState::NeedsConfirmation),
        other => Err(error::corrupt(format!(
            "the effect status {other:?} is not an effect state"
        ))),
    }
}

/// The hold state stored in a `state` column.
pub(crate) fn hold_from_json(text: &str) -> Result<HoldState, spot_effect_hold::Error> {
    let stored: StoredHold = decode("hold state", text)?;
    Ok(stored.hold)
}

/// The `jsonb` document to store for a hold state.
pub(crate) fn hold_to_json(state: &HoldState) -> Result<String, spot_effect_hold::Error> {
    encode("hold state", &HoldEnvelope { hold: state })
}

/// The `jsonb` document to store for an effect class.
pub(crate) fn class_to_json(class: &EffectClass) -> Result<String, spot_effect_hold::Error> {
    encode("effect class", class)
}

/// The `jsonb` document to store for a payload reference.
pub(crate) fn payload_to_json(payload: &PayloadRef) -> Result<String, spot_effect_hold::Error> {
    encode("payload reference", payload)
}

/// An effect id from its `bigint` column.
pub(crate) fn effect_id(value: i64) -> Result<EffectId, spot_effect_hold::Error> {
    u64::try_from(value)
        .map(EffectId)
        .map_err(|_| error::corrupt(format!("the effect id {value} is negative")))
}

/// A sequence number from its `bigint` column.
pub(crate) fn sequence(value: i64) -> Result<Sequence, spot_effect_hold::Error> {
    u64::try_from(value)
        .map(Sequence)
        .map_err(|_| error::corrupt(format!("the sequence {value} is negative")))
}

/// A sequence number as its `bigint` parameter.
pub(crate) fn sequence_param(value: Sequence) -> Result<i64, spot_effect_hold::Error> {
    i64::try_from(value.0)
        .map_err(|_| error::corrupt(format!("the sequence {} does not fit a bigint", value.0)))
}

/// An effect id as its `bigint` parameter. An id too large to be one was never issued by this
/// ledger, so it is the caller's argument that is wrong.
pub(crate) fn effect_id_param(value: EffectId) -> Result<i64, spot_effect_hold::Error> {
    i64::try_from(value.0).map_err(|_| spot_effect_hold::Error::InvalidInput {
        what: format!("effect id {}", value.0),
        reason: "the ledger's ids are 64-bit signed".to_owned(),
    })
}

/// An attempt count from its `bigint` column.
pub(crate) fn attempts(value: i64) -> Result<u32, spot_effect_hold::Error> {
    u32::try_from(value)
        .map_err(|_| error::corrupt(format!("the attempt count {value} is out of range")))
}

/// A checkpoint count from its `numeric(20,0)` column, rendered as text by the query.
pub(crate) fn count(what: &str, text: &str) -> Result<u64, spot_effect_hold::Error> {
    text.parse::<u64>().map_err(|_| {
        error::corrupt(format!(
            "the stored {what} {text:?} is not an unsigned integer"
        ))
    })
}

/// One effect row, read in the order [`crate::sql::LIST_EFFECTS`] selects it.
pub(crate) fn effect_from_row(row: &Row) -> Result<EffectRecord, spot_effect_hold::Error> {
    let site = row.get::<_, String>("site");
    let site = SiteId::new(site)
        .map_err(|e| error::corrupt(format!("the effect row's site is not a site id: {e}")))?;
    let class: EffectClass = decode("effect class", &row.get::<_, String>("effect_class"))?;
    let payload_ref: PayloadRef =
        decode("payload reference", &row.get::<_, String>("payload_ref"))?;
    let migration = row.get::<_, String>("migration_id");
    Ok(EffectRecord {
        id: effect_id(row.get("id"))?,
        site,
        class,
        kind: row.get("kind"),
        key: row.get("effect_key"),
        source: row.get("source"),
        source_event: row.get("source_event"),
        created_at: row.get("created_at"),
        sequence: sequence(row.get("seq"))?,
        payload_ref,
        migration_id: migration.as_str().parse().map_err(|e| {
            error::corrupt(format!("the effect row's migration id is invalid: {e}"))
        })?,
        state: state_from_text(&row.get::<_, String>("status"))?,
        note: row.get("note"),
        attempts: attempts(row.get("attempts"))?,
    })
}

fn encode<T: Serialize>(what: &str, value: &T) -> Result<String, spot_effect_hold::Error> {
    serde_json::to_string(value)
        .map_err(|e| error::corrupt(format!("the {what} could not be encoded: {e}")))
}

fn decode<T: DeserializeOwned>(what: &str, text: &str) -> Result<T, spot_effect_hold::Error> {
    serde_json::from_str(text)
        .map_err(|e| error::corrupt(format!("the stored {what} is not readable: {e}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_effect_hold::MigrationId;

    #[test]
    fn every_effect_state_round_trips_through_its_text() {
        for state in [
            EffectState::Held,
            EffectState::Releasing,
            EffectState::Dispatched,
            EffectState::Discarded,
            EffectState::NeedsConfirmation,
        ] {
            assert_eq!(
                state_from_text(state_text(state)).expect("round trip"),
                state
            );
        }
    }

    #[test]
    fn an_unknown_effect_state_is_an_error_not_a_default() {
        assert!(state_from_text("gone").is_err());
    }

    #[test]
    fn every_hold_state_round_trips_through_json() {
        let migration = MigrationId::new("mig-1").expect("valid");
        for state in [
            HoldState::Open,
            HoldState::Held {
                migration_id: migration.clone(),
                reason: "verify".to_owned(),
                since: 10,
                expected_by: Some(20),
            },
            HoldState::Released {
                migration_id: migration.clone(),
                at: 30,
            },
            HoldState::Discarded {
                migration_id: migration,
                at: 40,
                reason: "rolled back".to_owned(),
            },
        ] {
            let json = hold_to_json(&state).expect("encode");
            assert_eq!(hold_from_json(&json).expect("decode"), state);
        }
    }

    #[test]
    fn a_hold_state_is_stored_as_the_object_shape_the_table_checks() {
        let json = hold_to_json(&HoldState::Open).expect("encode");
        assert_eq!(json, r#"{"hold":"Open"}"#);
    }

    #[test]
    fn an_unreadable_hold_state_is_an_error_not_open() {
        assert!(hold_from_json(r#"{"hold":"Gone"}"#).is_err());
        assert!(hold_from_json("not json").is_err());
    }
}
