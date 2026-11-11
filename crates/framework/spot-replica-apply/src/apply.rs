//! The apply path itself: store a received record version, atomically, without business logic.
//!
//! [`apply`] is the **one path** shared by sync, rebuild from the journal and tenant links
//! (ADR-062 section 2, ADR-058). Given the caller's transaction and a [`ReceivedRecord`], it:
//!
//! 1. checks ownership ([`OwnerCheck::allow_replica`]) **before anything else**: a document from
//!    a node that does not own its partition at the item's instant is refused and never read
//!    against the catalog (ADR-062 G1). [`apply_quarantining`] hands that refusal to the
//!    quarantine port;
//! 2. validates the record's shape against the catalog (entity kind, exact fields, value types);
//! 3. checks the metadata skew (ADR-066);
//! 4. compares the record's version with the one already held — the origin version for a mirror,
//!    the storage version for a locally-owned record — and returns [`ApplyOutcome::Duplicate`] or
//!    [`ApplyOutcome::Stale`] without writing;
//! 5. writes the root document, its child rows and its ledger and stock entries through the
//!    storage contract, marking a mirror with its origin (ADR-058 rule 5);
//! 6. appends a delivery notice for a `notify` topic (ADR-058 rule 9) and writes the
//!    `replicated` audit marker (ADR-061);
//! 7. commits. A failure anywhere before the commit rolls the whole transaction back, so the
//!    bundle commits or nothing does (ADR-062 G3).
//!
//! It never calls a controller, a hook, a server script, a naming series, a business-rule
//! validation or the outbox emitter, and it never emits an outbox event: a replicated record is
//! not a new change at this node.

use spot_storage_contract::{
    Catalog, Document, DocumentData, Name, RecordId, RowLock, StorageError, Transaction, Version,
    Wait,
};

use crate::ctx::{ApplyCtx, DeliveryNotice, Ownership, QuarantinePort, ReplicaMarker};
use crate::error::Reason;
use crate::outcome::ApplyOutcome;
use crate::record::{ReceivedRecord, held_version, mark_origin, with_fields};
use crate::validate::{validate_document, validate_origin_fields, validate_record};

/// The internal result of an attempt, before the commit.
enum Run {
    /// The record was written; commit it.
    Stored,
    /// The same `(name, version)` is already stored.
    Duplicate,
    /// The stored version is newer.
    Stale,
}

/// Applies one received record inside `tx` and commits it.
///
/// On any rejection the transaction is rolled back and the outcome carries the reason; nothing
/// is stored. On [`ApplyOutcome::Duplicate`] or [`ApplyOutcome::Stale`] nothing is written either.
///
/// The notice and the audit marker are written **before** the commit, so a `Stored` outcome means
/// the record, its notice and its marker all landed together. The audit chain is a separate
/// append-only store; a commit failure after a successful marker append leaves an orphan marker
/// (see the crate's open questions).
///
/// An ownership refusal is reported in the outcome and nothing more; use
/// [`apply_quarantining`] to hand it to the quarantine queue in the same call.
pub async fn apply<T: Transaction>(
    tx: T,
    record: &ReceivedRecord,
    ctx: &ApplyCtx<'_>,
) -> ApplyOutcome {
    finish(tx, record, ctx, None).await
}

/// Applies one received record, handing an **ownership refusal** to `quarantine` so the stream
/// continues with the next record (ADR-062 G1 and G6, WP-EDG-06.T2).
///
/// The transaction has already rolled back when the port is called, so a refused record writes
/// nothing and emits nothing. The port holds the record image unchanged; the queue maps
/// [`Reason::code`] to the reason it shows an operator, and a redelivery is refused and held
/// again.
///
/// A structural or storage rejection (a bad payload, an unknown entity, a write failure) is not
/// an ownership refusal and is returned without being handed over: the queue's intake adapter
/// quarantines those on its own, and this port is the G1 decision's handoff point.
///
/// A failing port is reported as [`Reason::QuarantineFailed`], never swallowed: a refused record
/// must not disappear (ADR-062 G2).
pub async fn apply_quarantining<T: Transaction>(
    tx: T,
    record: &ReceivedRecord,
    ctx: &ApplyCtx<'_>,
    quarantine: &dyn QuarantinePort,
) -> ApplyOutcome {
    finish(tx, record, ctx, Some(quarantine)).await
}

/// The whole apply, with an optional ownership-refusal port.
async fn finish<T: Transaction>(
    mut tx: T,
    record: &ReceivedRecord,
    ctx: &ApplyCtx<'_>,
    quarantine: Option<&dyn QuarantinePort>,
) -> ApplyOutcome {
    let outcome = match run(&mut tx, record, ctx).await {
        Ok(Run::Duplicate) => {
            let _ = tx.rollback().await;
            ApplyOutcome::Duplicate
        }
        Ok(Run::Stale) => {
            let _ = tx.rollback().await;
            ApplyOutcome::Stale
        }
        Ok(Run::Stored) => match tx.commit().await {
            Ok(()) => ApplyOutcome::Stored,
            Err(error) => ApplyOutcome::Rejected(Reason::WriteFailed {
                why: error.to_string(),
            }),
        },
        Err(reason) => {
            let _ = tx.rollback().await;
            ApplyOutcome::Rejected(reason)
        }
    };
    hand_off(&outcome, record, ctx, quarantine).await
}

/// Hands an ownership refusal to the quarantine port, when there is one.
async fn hand_off(
    outcome: &ApplyOutcome,
    record: &ReceivedRecord,
    ctx: &ApplyCtx<'_>,
    quarantine: Option<&dyn QuarantinePort>,
) -> ApplyOutcome {
    let Some(reason) = outcome.reason().filter(|r| r.is_ownership_refusal()) else {
        return outcome.clone();
    };
    let Some(port) = quarantine else {
        return outcome.clone();
    };
    match port.hold(record, reason, ctx.at).await {
        Ok(()) => outcome.clone(),
        Err(error) => ApplyOutcome::Rejected(Reason::QuarantineFailed {
            why: error.to_string(),
        }),
    }
}

/// The whole attempt, up to but not including the commit.
async fn run<T: Transaction>(
    tx: &mut T,
    record: &ReceivedRecord,
    ctx: &ApplyCtx<'_>,
) -> Result<Run, Reason> {
    let catalog = tx.catalog().clone();
    // The ownership check runs before anything else (ADR-062 G1): a document from a node that
    // does not own its partition at the item's instant is refused before this node even reads
    // its catalog, so a non-owner cannot probe what this node knows.
    let ownership = ctx.owner.allow_replica(&record.source, record, ctx.at)?;
    validate_record(&catalog, record)?;
    check_skew(&catalog, record, ctx)?;

    // Load the root with a write lock: a concurrent apply of the same record cannot interleave.
    let stored = load(tx, &record.document.entity, record.document.id).await?;
    match held_version_of(record, ownership, stored.as_ref(), ctx) {
        Held::Same => return Ok(Run::Duplicate),
        Held::Newer => return Ok(Run::Stale),
        Held::Older => {}
    }

    write_root(tx, &catalog, record, ownership, ctx, stored.as_ref()).await?;
    for entry in &record.entries {
        write_entry(tx, &catalog, entry, record, ownership, ctx).await?;
    }
    append_notice(record, ctx).await?;
    write_marker(record, ownership, ctx).await?;
    Ok(Run::Stored)
}

/// What this node already holds for the record's root document.
enum Held {
    /// The same version is stored.
    Same,
    /// A newer version is stored.
    Newer,
    /// Nothing, or an older version: write.
    Older,
}

/// The version already held for the root, compared with the record's version.
fn held_version_of(
    record: &ReceivedRecord,
    ownership: Ownership,
    stored: Option<&Document>,
    ctx: &ApplyCtx<'_>,
) -> Held {
    let Some(stored) = stored else {
        return Held::Older;
    };
    let held = match ownership {
        // A mirror records the sender's version in its origin field; its local storage version
        // counts this node's own applies, not the sender's.
        Ownership::Mirror => held_version(&stored.data.fields, &ctx.origin),
        // A locally-owned record's version *is* its storage version (ADR-053): the rebuild case.
        Ownership::Owned => Some(stored.version.0),
    };
    match held {
        Some(held) if held == record.version => Held::Same,
        Some(held) if held > record.version => Held::Newer,
        _ => Held::Older,
    }
}

/// Loads a document with its version, taking a write lock.
async fn load<T: Transaction>(
    tx: &mut T,
    entity: &Name,
    id: RecordId,
) -> Result<Option<Document>, Reason> {
    tx.load(entity, id, RowLock::Update(Wait::Block))
        .await
        .map_err(write_failed)
}

/// Writes the root document, using the version already loaded for the optimistic check.
async fn write_root<T: Transaction>(
    tx: &mut T,
    catalog: &Catalog,
    record: &ReceivedRecord,
    ownership: Ownership,
    ctx: &ApplyCtx<'_>,
    stored: Option<&Document>,
) -> Result<(), Reason> {
    let root = prepare_document(catalog, &record.document, ownership, record, ctx)?;
    match stored {
        Some(existing) => {
            tx.update(&root, existing.version)
                .await
                .map_err(write_failed)?;
        }
        None => {
            tx.insert(&root).await.map_err(write_failed)?;
        }
    }
    Ok(())
}

/// Writes one ledger or stock entry: insert when it is new, replace it when it is already stored.
async fn write_entry<T: Transaction>(
    tx: &mut T,
    catalog: &Catalog,
    entry: &DocumentData,
    record: &ReceivedRecord,
    ownership: Ownership,
    ctx: &ApplyCtx<'_>,
) -> Result<(), Reason> {
    let prepared = prepare_document(catalog, entry, ownership, record, ctx)?;
    let existing = load(tx, &prepared.entity, prepared.id).await?;
    match existing {
        Some(existing) => {
            tx.update(&prepared, existing.version)
                .await
                .map_err(write_failed)?;
        }
        None => {
            tx.insert(&prepared).await.map_err(write_failed)?;
        }
    }
    Ok(())
}

/// The document to write: the root record, marked with its origin when it is a mirror.
///
/// Only the root record is marked. Its ledger and stock entries are immutable derived records
/// whose provenance is in the audit marker; they are stored as they arrived.
fn prepare_document(
    catalog: &Catalog,
    doc: &DocumentData,
    ownership: Ownership,
    record: &ReceivedRecord,
    ctx: &ApplyCtx<'_>,
) -> Result<DocumentData, Reason> {
    if ownership != Ownership::Mirror || doc.id != record.document.id {
        return Ok(doc.clone());
    }
    let def = catalog
        .entity(&doc.entity)
        .map_err(|_| Reason::UnknownEntity {
            entity: doc.entity.as_str().to_owned(),
        })?;
    validate_origin_fields(def, &ctx.origin)?;
    let mut fields = doc.fields.clone();
    mark_origin(&mut fields, &ctx.origin, record)?;
    Ok(with_fields(doc, fields))
}

/// The metadata skew check (ADR-066): a sender too far ahead of this node's catalog is refused.
fn check_skew(
    catalog: &Catalog,
    record: &ReceivedRecord,
    ctx: &ApplyCtx<'_>,
) -> Result<(), Reason> {
    if record.meta_version > catalog.version.saturating_add(ctx.max_skew) {
        return Err(Reason::MetadataSkew {
            record: record.meta_version,
            local: catalog.version,
            skew: ctx.max_skew,
        });
    }
    Ok(())
}

/// Appends the delivery notice for a `notify` topic (ADR-058 rule 9).
async fn append_notice(record: &ReceivedRecord, ctx: &ApplyCtx<'_>) -> Result<(), Reason> {
    if !ctx.notify {
        return Ok(());
    }
    let Some(sink) = ctx.notice else {
        return Ok(());
    };
    let notice = DeliveryNotice {
        topic: record.topic.clone(),
        record_id: record.document.id,
        conversation: record.conversation.clone(),
        source: record.source.clone(),
        received_at: ctx.at,
    };
    sink.append(&notice)
        .await
        .map_err(|reason| Reason::NoticeFailed {
            why: reason.to_string(),
        })
}

/// Writes the `replicated` audit marker (ADR-061, ADR-062 rule 7).
async fn write_marker(
    record: &ReceivedRecord,
    ownership: Ownership,
    ctx: &ApplyCtx<'_>,
) -> Result<(), Reason> {
    let marker = ReplicaMarker {
        source: record.source.clone(),
        seq: record.seq,
        name: record.name.as_str().to_owned(),
        version: record.version,
        entity: record.document.entity.as_str().to_owned(),
        id: record.document.id.to_string(),
        mirror: ownership == Ownership::Mirror,
        at: ctx.at,
    };
    ctx.audit.replicated(&marker).await
}

/// Maps a storage error to a rejection reason. The message is the storage contract's own
/// explanation; it never contains a field value or SQL text.
fn write_failed(error: StorageError) -> Reason {
    Reason::WriteFailed {
        why: error.to_string(),
    }
}

/// The stored version of a loaded [`Document`], exposed so a caller that keeps the load can
/// compare it (the apply path compares mirror versions through the origin field).
#[must_use]
pub fn stored_version(doc: &Document) -> Version {
    doc.version
}

/// Validates a document against the catalog, exposed for callers that decode a payload before
/// applying it (the quarantine queue, WP-EDG-06.T2).
///
/// # Errors
/// The first structural problem found.
pub fn check_document(catalog: &Catalog, doc: &DocumentData) -> Result<(), Reason> {
    validate_document(catalog, doc)
}
