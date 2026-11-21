//! Scalar column codecs: the domain values this store writes as a column, and back.
//!
//! Every codec is total in one direction and fallible in the other. Encoding a value the domain
//! types have already validated cannot fail; decoding a stored column can, because a row may have
//! been written by an older version or by hand, and a store that trusted its own database would
//! hand a caller a half-valid entry.
//!
//! Decoding is deliberately routed back through the domain constructors — [`TenantId::parse`],
//! [`Symptom::parse`], [`AuditRef::parse`], the sanitiser for text — so a row is checked against
//! exactly the same rules a freshly recorded entry is. A database is not a trusted source; it is a
//! place values are kept.
//!
//! # No float arithmetic
//!
//! The only float this module touches is an embedding element, which it formats and parses and
//! never computes with: the vector exception `spot-agent-memory` documents, and nothing wider. The
//! cosine itself is computed by the `vector` extension inside the memory database.

use spot_agent_memory::{
    AuditRef, Confidence, EMBEDDING_DIM, EpisodeId, Epoch, FactId, FactKey, MemoryError, Outcome,
    ProcedureId, RecordRef, RemedyId, Sanitiser, SourceKind, Summary, Symptom, TenantId, Tier,
    vector_is_usable,
};

/// A store failure with a stable cause phrase and no value, id or driver text in it.
#[must_use]
pub(crate) fn store_error(detail: &str) -> MemoryError {
    MemoryError::Store {
        detail: detail.to_owned(),
    }
}

/// A stored row this crate cannot decode.
///
/// The message names the column, never the value: a corrupt row is a support case, and a support
/// case must not ship the tenant's memory text to a log.
#[must_use]
pub(crate) fn corrupt(column: &str) -> MemoryError {
    store_error(&format!("a stored row could not be decoded at `{column}`"))
}

/// Parses a stored column with `parse`, reporting `column` when it is refused.
pub(crate) fn parse_column<T>(
    column: &str,
    text: &str,
    parse: impl Fn(&str) -> Result<T, MemoryError>,
) -> Result<T, MemoryError> {
    parse(text).map_err(|_| corrupt(column))
}

/// A stored free-text column read back through the sanitiser.
///
/// A summary is the only free text a memory entry can hold and only [`Sanitiser`] produces one,
/// so a row is scanned again on the way out: text that no longer passes (because the field
/// classes changed, or because the row was written by hand) is refused rather than handed to a
/// caller. Redaction is idempotent — `[redacted:…]` markers are not re-redacted — so a summary
/// this crate wrote always reads back unchanged.
///
/// # Errors
///
/// [`MemoryError::Store`] when the text no longer passes the sanitiser.
pub(crate) fn summary_from_text(
    column: &str,
    sanitiser: &Sanitiser,
    text: &str,
) -> Result<Summary, MemoryError> {
    sanitiser.check(text).map_err(|_| MemoryError::Store {
        detail: format!(
            "a stored row could not be decoded at `{column}`: the text no longer passes the \
             sanitiser"
        ),
    })
}

/// The tier's column value: its index on the ladder, `0` for T0.
#[must_use]
pub(crate) fn tier_to_i16(tier: Tier) -> i16 {
    match tier {
        Tier::T0 => 0,
        Tier::T1 => 1,
        Tier::T2 => 2,
        Tier::T3 => 3,
    }
}

/// The tier a column value names.
///
/// # Errors
///
/// A store failure when the value is outside the ladder.
pub(crate) fn tier_from_i16(column: &str, value: i16) -> Result<Tier, MemoryError> {
    match value {
        0 => Ok(Tier::T0),
        1 => Ok(Tier::T1),
        2 => Ok(Tier::T2),
        3 => Ok(Tier::T3),
        _ => Err(corrupt(column)),
    }
}

/// The source's column value.
#[must_use]
pub(crate) fn source_to_text(source: SourceKind) -> &'static str {
    source.as_str()
}

/// The source a column value names.
///
/// # Errors
///
/// A store failure when the text is not a source this crate knows.
pub(crate) fn source_from_text(column: &str, text: &str) -> Result<SourceKind, MemoryError> {
    match text {
        "incident" => Ok(SourceKind::Incident),
        "remedy_run" => Ok(SourceKind::RemedyRun),
        "operator" => Ok(SourceKind::Operator),
        "support_case" => Ok(SourceKind::SupportCase),
        "deployment" => Ok(SourceKind::Deployment),
        "compaction" => Ok(SourceKind::Compaction),
        _ => Err(corrupt(column)),
    }
}

/// The outcome's column value.
#[must_use]
pub(crate) fn outcome_to_text(outcome: Outcome) -> &'static str {
    outcome.as_str()
}

/// The outcome a column value names.
///
/// # Errors
///
/// A store failure when the text is not an outcome this crate knows.
pub(crate) fn outcome_from_text(column: &str, text: &str) -> Result<Outcome, MemoryError> {
    match text {
        "resolved" => Ok(Outcome::Resolved),
        "partly" => Ok(Outcome::Partly),
        "unresolved" => Ok(Outcome::Unresolved),
        "harm" => Ok(Outcome::Harm),
        _ => Err(corrupt(column)),
    }
}

/// The confidence's column value, in basis points.
///
/// # Errors
///
/// A store failure when the stored value is above certainty (the column's own `CHECK` makes that
/// impossible for a row this crate wrote, and a migrated row may still carry it).
pub(crate) fn confidence_to_i16(confidence: Confidence) -> i16 {
    // Basis points are 0 to 10,000, which fits `smallint` with room to spare; the cast cannot
    // lose a value the type itself bounds.
    i16::try_from(confidence.get()).unwrap_or(i16::MAX)
}

/// The confidence a column value names.
///
/// # Errors
///
/// A store failure when the value is negative or above certainty.
pub(crate) fn confidence_from_i16(column: &str, value: i16) -> Result<Confidence, MemoryError> {
    let basis_points = u16::try_from(value).map_err(|_| corrupt(column))?;
    Confidence::new(basis_points).map_err(|_| corrupt(column))
}

/// The epoch's column value: an unsigned 64-bit counter does not fit a signed column, so the
/// store keeps epochs in a `bigint` and refuses one above `i64::MAX` rather than wrapping it.
///
/// # Errors
///
/// [`MemoryError::EntryInvalid`] when the epoch is above `i64::MAX`.
pub(crate) fn epoch_to_i64(epoch: Epoch) -> Result<i64, MemoryError> {
    i64::try_from(epoch.get()).map_err(|_| MemoryError::EntryInvalid {
        kind: "fact".to_owned(),
        id: "epoch".to_owned(),
        reason: "an epoch the store can keep is at most i64::MAX".to_owned(),
    })
}

/// The epoch a column value names.
///
/// # Errors
///
/// A store failure when the value is negative.
pub(crate) fn epoch_from_i64(column: &str, value: i64) -> Result<Epoch, MemoryError> {
    let value = u64::try_from(value).map_err(|_| corrupt(column))?;
    Ok(Epoch::new(value))
}

/// An embedding as the text form the `vector` type's input function reads: `[0.5,-1,2]`.
///
/// The width is checked before formatting, so a caller cannot hand the database a vector whose
/// length disagrees with the column: `vector(64)` would refuse it there, but the failure would
/// arrive as a driver error naming no column.
///
/// # Errors
///
/// [`MemoryError::EntryInvalid`] when the vector is not [`EMBEDDING_DIM`] wide or holds a value
/// that is not finite.
pub(crate) fn vector_to_text(vector: &[f32]) -> Result<String, MemoryError> {
    if !vector_is_usable(vector) {
        return Err(MemoryError::EntryInvalid {
            kind: "embedding".to_owned(),
            id: "vector".to_owned(),
            reason: format!(
                "a stored embedding is {EMBEDDING_DIM} finite values, and this one is not"
            ),
        });
    }
    let mut text = String::with_capacity(vector.len() * 8 + 2);
    text.push('[');
    for (slot, value) in vector.iter().enumerate() {
        if slot > 0 {
            text.push(',');
        }
        // `Display` for `f32` is the shortest round-tripping form; the extension's input
        // function parses it back to the same value.
        text.push_str(&format!("{value}"));
    }
    text.push(']');
    Ok(text)
}

/// The embedding a stored `vector` column renders as text.
///
/// # Errors
///
/// A store failure when the text is not a bracketed list, holds a value that does not parse, or
/// is not [`EMBEDDING_DIM`] wide.
pub(crate) fn vector_from_text(column: &str, text: &str) -> Result<Vec<f32>, MemoryError> {
    let inner = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or_else(|| corrupt(column))?;
    if inner.is_empty() {
        return Err(corrupt(column));
    }
    let values: Result<Vec<f32>, MemoryError> = inner
        .split(',')
        .map(|element| element.trim().parse::<f32>().map_err(|_| corrupt(column)))
        .collect();
    let values = values?;
    if !vector_is_usable(&values) {
        return Err(corrupt(column));
    }
    Ok(values)
}

/// A tenant, episode, procedure, fact, key, symptom, record, audit or remedy id read from a
/// column, checked by its own constructor.
///
/// # Errors
///
/// A store failure when the text is not valid for that id.
pub(crate) fn tenant_from_text(column: &str, text: &str) -> Result<TenantId, MemoryError> {
    parse_column(column, text, TenantId::parse)
}

/// An episode id read from a column.
///
/// # Errors
///
/// A store failure when the text is not a valid episode id.
pub(crate) fn episode_id_from_text(column: &str, text: &str) -> Result<EpisodeId, MemoryError> {
    parse_column(column, text, EpisodeId::parse)
}

/// A procedure id read from a column.
///
/// # Errors
///
/// A store failure when the text is not a valid procedure id.
pub(crate) fn procedure_id_from_text(column: &str, text: &str) -> Result<ProcedureId, MemoryError> {
    parse_column(column, text, ProcedureId::parse)
}

/// A fact id read from a column.
///
/// # Errors
///
/// A store failure when the text is not a valid fact id.
pub(crate) fn fact_id_from_text(column: &str, text: &str) -> Result<FactId, MemoryError> {
    parse_column(column, text, FactId::parse)
}

/// A fact key read from a column.
///
/// # Errors
///
/// A store failure when the text is not a valid fact key.
pub(crate) fn fact_key_from_text(column: &str, text: &str) -> Result<FactKey, MemoryError> {
    parse_column(column, text, FactKey::parse)
}

/// A symptom tag read from a column.
///
/// # Errors
///
/// A store failure when the text is not a valid symptom tag.
pub(crate) fn symptom_from_text(column: &str, text: &str) -> Result<Symptom, MemoryError> {
    parse_column(column, text, Symptom::parse)
}

/// A record reference read from its two columns.
///
/// # Errors
///
/// A store failure when the pair is not a valid record reference.
pub(crate) fn record_from_columns(
    doctype_column: &str,
    id_column: &str,
    doctype: &str,
    id: &str,
) -> Result<RecordRef, MemoryError> {
    RecordRef::new(doctype, id).map_err(|_| corrupt(&format!("{doctype_column}/{id_column}")))
}

/// An audit reference read from a column.
///
/// # Errors
///
/// A store failure when the text is not a valid audit id.
pub(crate) fn audit_from_text(column: &str, text: &str) -> Result<AuditRef, MemoryError> {
    parse_column(column, text, AuditRef::parse)
}

/// A remedy id read from a column.
///
/// # Errors
///
/// A store failure when the text is not a dotted lower-case remedy id.
pub(crate) fn remedy_from_text(column: &str, text: &str) -> Result<RemedyId, MemoryError> {
    RemedyId::parse(text).map_err(|_| corrupt(column))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn wide() -> Vec<f32> {
        let mut vector = vec![0.0; EMBEDDING_DIM];
        vector[0] = 0.5;
        vector[1] = -1.0;
        vector[2] = 2.25;
        vector
    }

    #[test]
    fn a_vector_round_trips_through_its_text_form() {
        let vector = wide();
        let text = vector_to_text(&vector).unwrap();
        assert!(text.starts_with('[') && text.ends_with(']'));
        assert_eq!(text.matches(',').count(), EMBEDDING_DIM - 1);
        assert_eq!(vector_from_text("embedding", &text).unwrap(), vector);
    }

    #[test]
    fn a_vector_of_the_wrong_width_or_shape_is_refused_both_ways() {
        assert_eq!(
            vector_to_text(&[1.0, 2.0]).unwrap_err().code(),
            "agent_memory.entry_invalid"
        );
        assert_eq!(
            vector_to_text(&vec![f32::NAN; EMBEDDING_DIM])
                .unwrap_err()
                .code(),
            "agent_memory.entry_invalid"
        );
        for text in ["", "[]", "1,2,3", "[1,2,3]", "[1,two]"] {
            assert_eq!(
                vector_from_text("embedding", text).unwrap_err().code(),
                "agent_memory.store_failed",
                "`{text}` is not a stored embedding"
            );
        }
    }

    #[test]
    fn tiers_round_trip_across_the_whole_ladder() {
        for tier in Tier::all() {
            assert_eq!(
                tier_from_i16("tier", tier_to_i16(tier)).unwrap(),
                tier,
                "{tier} survives the column"
            );
        }
        assert!(tier_from_i16("tier", 4).is_err());
        assert!(tier_from_i16("tier", -1).is_err());
    }

    #[test]
    fn sources_and_outcomes_round_trip_and_refuse_strangers() {
        for source in [
            SourceKind::Incident,
            SourceKind::RemedyRun,
            SourceKind::Operator,
            SourceKind::SupportCase,
            SourceKind::Deployment,
            SourceKind::Compaction,
        ] {
            assert_eq!(
                source_from_text("source", source_to_text(source)).unwrap(),
                source
            );
        }
        assert!(source_from_text("source", "document").is_err());
        for outcome in [
            Outcome::Resolved,
            Outcome::Partly,
            Outcome::Unresolved,
            Outcome::Harm,
        ] {
            assert_eq!(
                outcome_from_text("outcome", outcome_to_text(outcome)).unwrap(),
                outcome
            );
        }
        assert!(outcome_from_text("outcome", "fixed").is_err());
    }

    #[test]
    fn a_confidence_is_basis_points_in_a_smallint() {
        assert_eq!(confidence_to_i16(Confidence::FULL), 10_000);
        assert_eq!(
            confidence_from_i16("confidence", 10_000).unwrap(),
            Confidence::FULL
        );
        assert!(confidence_from_i16("confidence", 10_001).is_err());
        assert!(confidence_from_i16("confidence", -1).is_err());
        assert_eq!(confidence_to_i16(Confidence::new(6_667).unwrap()), 6_667);
    }

    #[test]
    fn an_epoch_too_large_for_the_column_is_refused_rather_than_wrapped() {
        assert_eq!(epoch_to_i64(Epoch::new(7)).unwrap(), 7);
        assert_eq!(
            epoch_from_i64("epoch", 7).unwrap().get(),
            7,
            "the epoch survives"
        );
        assert!(epoch_to_i64(Epoch::new(u64::MAX)).is_err());
        assert!(epoch_from_i64("epoch", -1).is_err());
    }

    #[test]
    fn a_corrupt_row_names_the_column_and_never_the_value() {
        let error = vector_from_text("embedding", "[secret-shaped-value]").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("embedding"));
        assert!(!text.contains("secret-shaped-value"), "{text}");
    }

    #[test]
    fn ids_come_back_through_their_own_constructors() {
        assert!(tenant_from_text("tenant", "tenant_a").is_ok());
        assert!(tenant_from_text("tenant", "Tenant A").is_err());
        assert!(episode_id_from_text("entry_id", "episode.e1").is_ok());
        assert!(episode_id_from_text("entry_id", "episode").is_err());
        assert!(procedure_id_from_text("entry_id", "procedure.p1").is_ok());
        assert!(fact_id_from_text("entry_id", "fact.f1").is_ok());
        assert!(fact_key_from_text("key", "backend.fact_1").is_ok());
        assert!(symptom_from_text("symptoms", "job.stuck_running").is_ok());
        assert!(audit_from_text("audits", "audit-1").is_ok());
        assert!(remedy_from_text("remedies", "job.retry_failed").is_ok());
        assert!(remedy_from_text("remedies", "Job Retry").is_err());
        assert!(record_from_columns("incident_doctype", "incident_id", "job", "018f-1").is_ok());
        assert!(record_from_columns("incident_doctype", "incident_id", "Job", "018f-1").is_err());
    }
}
