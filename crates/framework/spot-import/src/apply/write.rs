// The writing half of [`apply_import`](super::apply_import): planning, writing, limits and the
// helpers that turn an engine error into the report's row format.
//!
//! Split from `mod.rs` so each file stays small; everything here is private to `apply`.

use std::collections::{BTreeMap, HashSet};

use sha2::{Digest, Sha256};
use spot_app_api::context::Actor;

use spot_app_api::context::{DocumentService, Invocation, Outcome};
use spot_app_api::document::{Document, DocumentPatch, RowPatch};
use spot_app_api::error::{AppError, ErrorClass};
use spot_app_api::prelude::{CommandId, RecordId, Value, Version};

use super::{
    ApplyMode, ApplyOptions, ApplyOutcome, ApplySummary, BatchCreate, CreatedDocument, ImportItem,
    ProgressSink, SpoolEntry, StopReason,
};
use crate::error::{
    CONFIG_COMMAND_ID, DUPLICATE_KEY, IDEMPOTENCY_CONFLICT, KEY_MISSING, KEY_NOT_FOUND,
    MISSING_KEY_INDEX, SPOOL_FULL, TOO_MANY_ERRORS,
};
use crate::report::ImportRowError;
use crate::stream::ImportDocument;

/// [`ImportPolicy::SkipBadRows`] (and every dry run): validate a parent and apply it before
/// asking for the next one.
pub(super) async fn apply_as_it_streams(
    service: &dyn DocumentService,
    inv: &Invocation,
    items: impl Iterator<Item = ImportItem>,
    options: &ApplyOptions,
    progress: &mut dyn ProgressSink,
    mut summary: ApplySummary,
) -> ApplyOutcome {
    if !summary.dry_run
        && let Some((batcher, size)) = options.batcher()
    {
        return apply_in_batches(
            (service, batcher, size),
            inv,
            items,
            options,
            progress,
            summary,
        )
        .await;
    }
    let every = options.progress_every.max(1);
    let mut seen = 0u64;
    let mut failed_at = None;

    for item in items {
        if let Some(reason) = stop_reason(options, &summary) {
            summary.stopped = Some(reason);
            break;
        }
        seen += 1;
        let document = match item {
            ImportItem::Error(entry) => {
                summary.failed += 1;
                summary.errors.push(entry);
                None
            }
            ImportItem::Fatal(error) => {
                summary.errors.push(fatal_note(&error));
                summary.stopped = Some(StopReason::StreamFailed);
                break;
            }
            ImportItem::Document(document) => Some(document),
        };
        if let Some(document) = document {
            let row = first_row(&document);
            match build_one(options, &document) {
                Err(entry) => {
                    summary.failed += 1;
                    summary.errors.push(entry);
                }
                Ok(document) => {
                    match apply_one(service, inv, &document, row, options, &mut summary).await {
                        Ok(()) => {}
                        Err(ApplyError::Row(entry)) => {
                            summary.failed += 1;
                            summary.errors.push(entry);
                        }
                        Err(ApplyError::Write(entry, reason)) => {
                            summary.failed += 1;
                            failed_at = Some(entry.row);
                            summary.errors.push(entry);
                            summary.stopped = Some(reason);
                            break;
                        }
                    }
                }
            }
        }
        if seen.is_multiple_of(every) {
            progress.progress(seen, None);
        }
    }
    progress.progress(seen, None);
    finish(summary, failed_at)
}

/// What the stream gave, in file order, for one batch.
enum Entry {
    /// A row problem (the stream's, the conversion's or the permission probe's).
    Failed(ImportRowError),
    /// The next [`PendingCreate`] of the batch.
    Create,
}

/// One batch of the stream: its entries in file order and the documents to create.
#[derive(Default)]
struct Chunk {
    entries: Vec<Entry>,
    pending: Vec<PendingCreate>,
    /// Row problems among the entries (for the error limit).
    problems: u64,
}

/// [`ImportPolicy::SkipBadRows`] with a host that creates documents in batches: pull up to
/// `size` entries from the stream, create the documents in one call, then record the answers in
/// file order, exactly as the one-by-one path does (see the module docs).
async fn apply_in_batches(
    (service, batcher, size): (&dyn DocumentService, &dyn BatchCreate, usize),
    inv: &Invocation,
    items: impl Iterator<Item = ImportItem>,
    options: &ApplyOptions,
    progress: &mut dyn ProgressSink,
    mut summary: ApplySummary,
) -> ApplyOutcome {
    let every = options.progress_every.max(1);
    let mut seen = 0u64;
    let mut reported = 0u64;
    let mut failed_at = None;
    let mut chunk = Chunk::default();
    let mut fatal = None;
    let mut gather_stop = None;

    for item in items {
        if options.cancel.is_cancelled() {
            gather_stop = Some(StopReason::Cancelled);
            break;
        }
        if options.max_errors > 0 && summary.failed + chunk.problems >= options.max_errors {
            gather_stop = Some(StopReason::TooManyErrors);
            break;
        }
        seen += 1;
        match item {
            ImportItem::Error(entry) => {
                chunk.problems += 1;
                chunk.entries.push(Entry::Failed(entry));
            }
            ImportItem::Fatal(error) => {
                fatal = Some(fatal_note(&error));
                break;
            }
            ImportItem::Document(document) => {
                let row = first_row(&document);
                let planned = match build_one(options, &document) {
                    Ok(built) => {
                        let doctype = built.doctype().as_str().to_owned();
                        plan(service, inv, &doctype, &built, row, options).await
                    }
                    Err(entry) => Err(entry),
                };
                match planned {
                    Ok(Plan::Create(document)) => {
                        chunk.pending.push(PendingCreate { row, document });
                        chunk.entries.push(Entry::Create);
                    }
                    Ok(Plan::Skip | Plan::Update { .. }) => {}
                    Err(entry) => {
                        chunk.problems += 1;
                        chunk.entries.push(Entry::Failed(entry));
                    }
                }
            }
        }
        if chunk.entries.len() >= size {
            write_chunk(
                (service, batcher),
                inv,
                std::mem::take(&mut chunk),
                options,
                &mut summary,
                &mut failed_at,
            )
            .await;
            if seen - reported >= every {
                progress.progress(seen, None);
                reported = seen;
            }
            if summary.stopped.is_some() {
                break;
            }
        }
    }
    if summary.stopped.is_none() {
        write_chunk(
            (service, batcher),
            inv,
            chunk,
            options,
            &mut summary,
            &mut failed_at,
        )
        .await;
    }
    if summary.stopped.is_none() {
        if let Some(entry) = fatal {
            summary.errors.push(entry);
            summary.stopped = Some(StopReason::StreamFailed);
        } else {
            summary.stopped = gather_stop;
        }
    }
    progress.progress(seen, None);
    finish(summary, failed_at)
}

/// Creates a chunk's documents in one host call and records every answer in file order. The
/// first refused write stops the run; later documents of the chunk that the host did commit are
/// counted (they exist), and later row problems are not reported (the run never got to them).
async fn write_chunk(
    (service, batcher): (&dyn DocumentService, &dyn BatchCreate),
    inv: &Invocation,
    chunk: Chunk,
    options: &ApplyOptions,
    summary: &mut ApplySummary,
    failed_at: &mut Option<u64>,
) {
    if chunk.entries.is_empty() {
        return;
    }
    let mut results = create_batch(service, batcher, inv, &chunk.pending, options, summary)
        .await
        .into_iter();
    let mut stopped_by_write = false;
    for entry in chunk.entries {
        match entry {
            Entry::Failed(error) => {
                if !stopped_by_write {
                    summary.failed += 1;
                    summary.errors.push(error);
                }
            }
            Entry::Create => match results.next() {
                Some(Ok(())) | None => {}
                Some(Err(ApplyError::Row(error))) => {
                    summary.failed += 1;
                    summary.errors.push(error);
                }
                Some(Err(ApplyError::Write(error, reason))) => {
                    summary.failed += 1;
                    if !stopped_by_write {
                        stopped_by_write = true;
                        *failed_at = Some(error.row);
                        summary.stopped = Some(reason);
                    }
                    summary.errors.push(error);
                }
            },
        }
    }
}

/// [`ImportPolicy::AllOrNothing`] for a real write: validate and spool the whole stream first,
/// then write only when it validated cleanly.
pub(super) async fn apply_refusing_writes(
    service: &dyn DocumentService,
    inv: &Invocation,
    items: impl Iterator<Item = ImportItem>,
    options: &ApplyOptions,
    progress: &mut dyn ProgressSink,
    mut summary: ApplySummary,
) -> ApplyOutcome {
    let every = options.progress_every.max(1);
    let mut seen = 0u64;

    // Phase 1: validate every parent (the stream's checks, then the key, existence and
    // permission checks a write would meet) and keep it; write nothing.
    let mut keys: HashSet<String> = HashSet::new();
    for item in items {
        if let Some(reason) = stop_reason(options, &summary) {
            summary.stopped = Some(reason);
            break;
        }
        seen += 1;
        let document = match item {
            ImportItem::Error(entry) => {
                summary.failed += 1;
                summary.errors.push(entry);
                None
            }
            ImportItem::Fatal(error) => {
                summary.errors.push(fatal_note(&error));
                summary.stopped = Some(StopReason::StreamFailed);
                break;
            }
            ImportItem::Document(document) => Some(document),
        };
        if let Some(document) = document {
            let row = first_row(&document);
            match build_one(options, &document) {
                Err(entry) => {
                    summary.failed += 1;
                    summary.errors.push(entry);
                }
                Ok(document) => {
                    if let Err(entry) =
                        check_before_writing(service, inv, &document, row, options, &mut keys).await
                    {
                        summary.failed += 1;
                        summary.errors.push(entry);
                    } else if options.spool.push(SpoolEntry { row, document }).is_err() {
                        // The spool refuses the parent that does not fit; every validated
                        // parent stays unwritten.
                        summary.skipped += 1;
                        summary.stopped = Some(StopReason::SpoolFull);
                        break;
                    }
                }
            }
        }
        if seen.is_multiple_of(every) {
            progress.progress(seen, None);
        }
    }
    progress.progress(seen, None);

    // Phase 1's verdict: a row problem refuses the file; a stop writes nothing at all.
    if summary.stopped.is_none() && summary.failed > 0 {
        summary.stopped = Some(StopReason::PolicyRefused);
    }
    if summary.stopped.is_some() {
        skip_remaining(options, &mut summary);
        return finish(summary, None);
    }

    // Phase 2: write the validated parents in file order, one engine call each.
    let total = options.spool.len() as u64;
    let mut done = 0u64;
    let mut failed_at = None;
    if let Some((batcher, size)) = options.batcher() {
        while !options.spool.is_empty() {
            if let Some(reason) = stop_reason(options, &summary) {
                summary.stopped = Some(reason);
                break;
            }
            let pending: Vec<PendingCreate> = std::iter::from_fn(|| options.spool.pop())
                .take(size)
                .map(|entry| PendingCreate {
                    row: entry.row,
                    document: entry.document,
                })
                .collect();
            let count = pending.len() as u64;
            let rows: Vec<u64> = pending.iter().map(|entry| entry.row).collect();
            let results =
                create_batch(service, batcher, inv, &pending, options, &mut summary).await;
            for (row, result) in rows.into_iter().zip(results) {
                if let Err(ApplyError::Row(entry) | ApplyError::Write(entry, _)) = result {
                    summary.failed += 1;
                    failed_at.get_or_insert(row);
                    summary.errors.push(entry);
                    summary.stopped = Some(StopReason::WriteFailed);
                }
            }
            done += count;
            progress.progress(done, Some(total));
            if summary.stopped.is_some() {
                break;
            }
        }
    }
    while options.batcher().is_none()
        && let Some(entry) = options.spool.pop()
    {
        if let Some(reason) = stop_reason(options, &summary) {
            summary.stopped = Some(reason);
            break;
        }
        match apply_one(
            service,
            inv,
            &entry.document,
            entry.row,
            options,
            &mut summary,
        )
        .await
        {
            Ok(()) => {}
            Err(ApplyError::Row(entry) | ApplyError::Write(entry, _)) => {
                summary.failed += 1;
                failed_at = Some(entry.row);
                summary.errors.push(entry);
                summary.stopped = Some(StopReason::WriteFailed);
                break;
            }
        }
        done += 1;
        progress.progress(done, Some(total));
    }
    progress.progress(done, Some(total));
    skip_remaining(options, &mut summary);
    finish(summary, failed_at)
}

/// What planning one document decided.
enum Plan {
    /// Create it as a new draft.
    Create(Document),
    /// Patch the stored document at `expected`.
    Update {
        /// The stored document's id.
        id: RecordId,
        /// The version the patch is written against.
        expected: Version,
        /// The merge patch.
        patch: DocumentPatch,
    },
    /// The stored document already holds every imported value.
    Skip,
}

/// Why one document was not applied.
enum ApplyError {
    /// The document could not be planned (a missing key, a key that names nothing); the run may
    /// continue with the next one.
    Row(ImportRowError),
    /// The host refused a write; the run stops.
    Write(ImportRowError, StopReason),
}

/// The first source row of a parent (its idempotency key's input).
fn first_row(document: &ImportDocument) -> u64 {
    document.rows.first().map_or(0, |row| row.number)
}

/// Builds the defaulted, validated document out of one streamed parent.
fn build_one(
    options: &ApplyOptions,
    document: &ImportDocument,
) -> Result<Document, ImportRowError> {
    document
        .to_document(&options.meta)
        .map_err(|(entry, _error)| entry)
}

/// The phase-1 check of an all-or-nothing run: everything about `document` a write would
/// refuse that can be known without writing — its key (present, not repeated, existing for an
/// update), the stored version, and permission to create or update. Never writes.
async fn check_before_writing(
    service: &dyn DocumentService,
    inv: &Invocation,
    document: &Document,
    row: u64,
    options: &ApplyOptions,
    keys: &mut HashSet<String>,
) -> Result<(), ImportRowError> {
    let doctype = document.doctype().as_str().to_owned();
    if let Some(key_field) = options.mode.key_field() {
        let value = document.get(key_field);
        // A missing key is reported by `plan`; only a present one can repeat.
        if !value.is_null() && !keys.insert(format!("{value:?}")) {
            return Err(ImportRowError {
                row,
                column: key_field.to_owned(),
                cell: value_text(value),
                code: DUPLICATE_KEY.to_owned(),
                message: format!("another row of this file has the same {key_field}"),
            });
        }
    }
    plan(service, inv, &doctype, document, row, options)
        .await
        .map(|_| ())
}

/// Applies one validated document: plans it, then writes or skips it.
async fn apply_one(
    service: &dyn DocumentService,
    inv: &Invocation,
    document: &Document,
    row: u64,
    options: &ApplyOptions,
    summary: &mut ApplySummary,
) -> Result<(), ApplyError> {
    let doctype = document.doctype().as_str().to_owned();
    let plan = match plan(service, inv, &doctype, document, row, options).await {
        Ok(plan) => plan,
        Err(entry) => return Err(ApplyError::Row(entry)),
    };
    match plan {
        Plan::Skip => {
            summary.skipped += 1;
            Ok(())
        }
        Plan::Create(document) => {
            if summary.dry_run {
                summary.created += 1;
                return Ok(());
            }
            create_one(service, inv, &doctype, row, document, options, summary).await
        }
        Plan::Update {
            id,
            expected,
            patch,
        } => {
            if summary.dry_run {
                summary.updated += 1;
                return Ok(());
            }
            let call = match invocation_for(inv, 'u', &doctype, &options.batch_label, row) {
                Ok(call) => call,
                Err(entry) => return Err(ApplyError::Write(entry, StopReason::Configuration)),
            };
            match service.update(&call, &doctype, id, patch, expected).await {
                Ok(_) => {
                    summary.updated += 1;
                    Ok(())
                }
                Err(error) => Err(ApplyError::Write(
                    row_error(row, &error),
                    StopReason::WriteFailed,
                )),
            }
        }
    }
}

/// Creates one draft, posts it when asked to, and records it for a rollback.
async fn create_one(
    service: &dyn DocumentService,
    inv: &Invocation,
    doctype: &str,
    row: u64,
    document: Document,
    options: &ApplyOptions,
    summary: &mut ApplySummary,
) -> Result<(), ApplyError> {
    let (command_id, call) = create_call(inv, doctype, row, options)?;
    let sent = document.clone();
    let created = service
        .create(&call, document)
        .await
        .map(|Outcome { value, .. }| value);
    finish_create(
        service, inv, doctype, row, &sent, created, command_id, options, summary,
    )
    .await
}

/// The command id and the invocation that carries it, for creating the document of `row`.
fn create_call(
    inv: &Invocation,
    doctype: &str,
    row: u64,
    options: &ApplyOptions,
) -> Result<(String, Invocation), ApplyError> {
    let command_id = command_text(inv, 'c', doctype, &options.batch_label, row);
    match invocation_with_id(inv, &command_id) {
        Ok(call) => Ok((command_id, call)),
        Err(error) => Err(ApplyError::Write(
            row_error(row, &error),
            StopReason::Configuration,
        )),
    }
}

/// What a create's answer means: the document it returned is checked against what was sent,
/// counted, recorded for a rollback and (when asked) posted.
#[allow(clippy::too_many_arguments)]
async fn finish_create(
    service: &dyn DocumentService,
    inv: &Invocation,
    doctype: &str,
    row: u64,
    sent: &Document,
    created: Result<Document, AppError>,
    command_id: String,
    options: &ApplyOptions,
    summary: &mut ApplySummary,
) -> Result<(), ApplyError> {
    let created = match created {
        Ok(value) => value,
        Err(error) => {
            return Err(ApplyError::Write(
                row_error(row, &error),
                StopReason::WriteFailed,
            ));
        }
    };
    // The engine answers a command id it has already committed with the first result. If that
    // result is not this import's document, the id was someone else's: nothing was created.
    if !holds_imported_values(&created, sent) {
        return Err(ApplyError::Write(
            note(
                row,
                IDEMPOTENCY_CONFLICT,
                "the host answered this create with a different, earlier document (its command id was already used); nothing was created",
            ),
            StopReason::WriteFailed,
        ));
    }
    let id = created.id();
    let version = created.version().unwrap_or(Version::FIRST);
    summary.created += 1;
    summary.created_drafts.push(CreatedDocument {
        doctype: doctype.to_owned(),
        id,
        version,
        posted: false,
        command_id,
    });
    if options.post_after_import {
        let post_call = match invocation_for(inv, 'p', doctype, &options.batch_label, row) {
            Ok(call) => call,
            Err(entry) => {
                summary.errors.push(entry);
                return Ok(());
            }
        };
        match service.post(&post_call, doctype, id, version).await {
            Ok(posted) => {
                if let Some(draft) = summary.created_drafts.last_mut() {
                    draft.posted = true;
                    draft.version = posted.value.version().unwrap_or(version);
                }
            }
            // The draft exists and is reported as created; the failed post is a problem to
            // fix, not a document that was never written.
            Err(error) => summary.errors.push(row_error(row, &error)),
        }
    }
    Ok(())
}

/// One document waiting for the next batched create.
struct PendingCreate {
    row: u64,
    document: Document,
}

/// Creates `pending` through the host's batch writer, one result per document in order. Each
/// document goes through the same checks as a lone create ([`finish_create`]); a document the
/// host refused does not stop the ones after it from being counted, because the host committed
/// them in the same transaction.
async fn create_batch(
    service: &dyn DocumentService,
    batcher: &dyn BatchCreate,
    inv: &Invocation,
    pending: &[PendingCreate],
    options: &ApplyOptions,
    summary: &mut ApplySummary,
) -> Vec<Result<(), ApplyError>> {
    let mut results = Vec::with_capacity(pending.len());
    let mut calls = Vec::with_capacity(pending.len());
    let mut ids = Vec::with_capacity(pending.len());
    let mut sendable = Vec::with_capacity(pending.len());
    for (index, entry) in pending.iter().enumerate() {
        let doctype = entry.document.doctype().as_str();
        match create_call(inv, doctype, entry.row, options) {
            Ok((command_id, call)) => {
                calls.push((call, entry.document.clone()));
                ids.push(command_id);
                sendable.push(index);
                results.push(None);
            }
            Err(error) => results.push(Some(Err(error))),
        }
    }
    let mut answers = if calls.is_empty() {
        Vec::new()
    } else {
        batcher.create_many(calls).await
    };
    // A host that answers with the wrong number of results broke the trait's contract: never
    // guess which documents were written.
    if answers.len() != sendable.len() {
        let message = "the host's batch create answered a different number of results than documents sent; the batch is treated as failed";
        answers = sendable
            .iter()
            .map(|_| {
                Err(AppError::new(
                    ErrorClass::Internal,
                    crate::error::CONFIG_COMMAND_ID,
                    message,
                ))
            })
            .collect();
    }
    for ((index, command_id), answer) in sendable.into_iter().zip(ids).zip(answers) {
        let entry = &pending[index];
        let doctype = entry.document.doctype().as_str();
        let outcome = finish_create(
            service,
            inv,
            doctype,
            entry.row,
            &entry.document,
            answer.map(|Outcome { value, .. }| value),
            command_id,
            options,
            summary,
        )
        .await;
        results[index] = Some(outcome);
    }
    results.into_iter().flatten().collect()
}

/// Decides what one document does: create, update, or nothing.
async fn plan(
    service: &dyn DocumentService,
    inv: &Invocation,
    doctype: &str,
    document: &Document,
    row: u64,
    options: &ApplyOptions,
) -> Result<Plan, ImportRowError> {
    let Some(key_field) = options.mode.key_field() else {
        check_create(inv, doctype, row, options)?;
        return Ok(Plan::Create(document.clone()));
    };
    let Some(index) = options.index.as_ref() else {
        return Err(note(
            0,
            MISSING_KEY_INDEX,
            "This import mode needs a key index.",
        ));
    };
    let value = document.get(key_field).clone();
    if value.is_null() {
        return Err(ImportRowError {
            row,
            column: key_field.to_owned(),
            cell: String::new(),
            code: KEY_MISSING.to_owned(),
            message: format!(
                "the document has no value in {key_field}, the field this import keys on"
            ),
        });
    }
    let Some(id) = index.find(doctype, key_field, &value) else {
        return match options.mode {
            ApplyMode::Update { .. } => Err(ImportRowError {
                row,
                column: key_field.to_owned(),
                cell: value_text(&value),
                code: KEY_NOT_FOUND.to_owned(),
                message: format!("no document of {doctype} has this {key_field}"),
            }),
            // Upsert, and a keyed dry run, create when nothing matches.
            _ => {
                check_create(inv, doctype, row, options)?;
                Ok(Plan::Create(document.clone()))
            }
        };
    };
    options
        .permissions
        .check_update(inv, doctype, id)
        .map_err(|error| probe_error(row, &error))?;
    let stored = match service.load(inv, doctype, id).await {
        Ok(Outcome { value, .. }) => value,
        Err(error) => return Err(row_error(row, &error)),
    };
    // The index is the caller's (often a cache): the loaded document must still hold the key
    // it was found by, or a patch would overwrite the wrong document's key and fields.
    if stored.get(key_field) != &value {
        return Err(ImportRowError {
            row,
            column: key_field.to_owned(),
            cell: value_text(&value),
            code: KEY_NOT_FOUND.to_owned(),
            message: format!(
                "the document the key index named no longer has this {key_field} (the index is stale)"
            ),
        });
    }
    let Some(expected) = stored.version() else {
        return Err(note(
            0,
            crate::error::META_ERROR,
            "The loaded document has no version.",
        ));
    };
    if is_noop(&stored, document) {
        return Ok(Plan::Skip);
    }
    Ok(Plan::Update {
        id,
        expected,
        patch: patch_for(document),
    })
}

/// Asks the permission probe, when there is one, whether the caller may create `doctype`.
fn check_create(
    inv: &Invocation,
    doctype: &str,
    row: u64,
    options: &ApplyOptions,
) -> Result<(), ImportRowError> {
    options
        .permissions
        .check_create(inv, doctype)
        .map_err(|error| probe_error(row, &error))
}

/// Whether the document a create returned is the one that was sent: same DocType, every
/// imported field equal, and the same number of rows in every imported table.
fn holds_imported_values(returned: &Document, sent: &Document) -> bool {
    returned.doctype() == sent.doctype()
        && sent
            .fields()
            .iter()
            .all(|(name, value)| returned.get(name.as_str()) == value)
        && sent
            .tables()
            .iter()
            .all(|(table, rows)| returned.rows(table.as_str()).len() == rows.len())
}

/// The merge patch that writes every imported field and table onto a stored document.
///
/// Child rows are all new: an import row has no stored row id, and a patch that kept old rows
/// would merge the file into them instead of replacing the table with the file's rows.
fn patch_for(document: &Document) -> DocumentPatch {
    let fields: BTreeMap<_, _> = document
        .fields()
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let tables = document
        .tables()
        .iter()
        .map(|(table, rows)| {
            let rows: Vec<RowPatch> = rows
                .iter()
                .map(|row| RowPatch {
                    id: None,
                    fields: row.fields.clone(),
                })
                .collect();
            (table.clone(), rows)
        })
        .collect();
    DocumentPatch { fields, tables }
}

/// Whether the stored document already holds every value the import would write.
fn is_noop(stored: &Document, imported: &Document) -> bool {
    let fields_same = imported
        .fields()
        .iter()
        .all(|(name, value)| stored.get(name.as_str()) == value);
    let tables_same = imported.tables().iter().all(|(table, rows)| {
        let stored_rows = stored.rows(table.as_str());
        rows.len() == stored_rows.len()
            && rows
                .iter()
                .zip(stored_rows)
                .all(|(imported_row, stored_row)| imported_row.fields == stored_row.fields)
    });
    fields_same && tables_same
}

/// Counts what is still in the spool as skipped (the run is over).
fn skip_remaining(options: &ApplyOptions, summary: &mut ApplySummary) {
    while options.spool.pop().is_some() {
        summary.skipped += 1;
    }
}

/// Turns the final counters into the outcome, adding the note for a limit-driven stop.
fn finish(mut summary: ApplySummary, failed_at: Option<u64>) -> ApplyOutcome {
    match summary.stopped {
        Some(StopReason::Cancelled) => summary.cancelled = true,
        Some(StopReason::TooManyErrors) => summary.errors.push(note(
            0,
            TOO_MANY_ERRORS,
            "The import stopped at its error limit.",
        )),
        Some(StopReason::SpoolFull) => summary.errors.push(note(
            0,
            SPOOL_FULL,
            "The all-or-nothing buffer is full; nothing was written.",
        )),
        _ => {}
    }
    if summary.stopped.is_none() {
        return ApplyOutcome::Applied(summary);
    }
    if summary.written() == 0 {
        ApplyOutcome::NothingWritten(summary)
    } else {
        ApplyOutcome::PartiallyApplied {
            summary,
            failed_at: failed_at.unwrap_or(0),
        }
    }
}

/// A row error for a failed engine call, naming the field the host blamed when it names one.
fn row_error(row: u64, error: &AppError) -> ImportRowError {
    let column = error
        .fields
        .first()
        .map(|field| field.field.as_str().to_owned())
        .unwrap_or_default();
    ImportRowError {
        row,
        column,
        cell: String::new(),
        code: error.code.clone(),
        message: error.message.clone(),
    }
}

/// A permission probe's error as a row error, failing closed: a refusal keeps its own code and
/// message; any other error (the probe itself failed) becomes the catalogued
/// `permission.denied` with a fixed message, so nothing from the probe's internals reaches the
/// report. Either way the row is not written.
fn probe_error(row: u64, error: &AppError) -> ImportRowError {
    if error.class == ErrorClass::PermissionDenied {
        row_error(row, error)
    } else {
        note(
            row,
            "permission.denied",
            "the permission check could not be completed, so the row was not written",
        )
    }
}

/// A report entry for a problem that is not about one document (row 0, like a fatal).
pub(super) fn note(row: u64, code: &str, message: &str) -> ImportRowError {
    ImportRowError {
        row,
        column: String::new(),
        cell: String::new(),
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// The report entry for the stream's whole-file failure.
fn fatal_note(error: &crate::error::ImportError) -> ImportRowError {
    note(0, error.code(), &error.to_string())
}

/// The text of a key value for the report's cell column.
fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(flag) => flag.to_string(),
        Value::Int(number) => number.to_string(),
        Value::Decimal(number) => number.to_string(),
        Value::Text(text) => text.clone(),
        Value::Id(id) => id.to_string(),
        Value::Date(date) => date.to_string(),
        // Rare as a key; the derived Debug form names the value without inventing a format.
        other => format!("{other:?}"),
    }
}

/// The deterministic command id of one document's operation.
///
/// SHA-256 over the site, the actor, the DocType, the batch label, the row and the operation,
/// each length-prefixed so no two different tuples share an input; the id keeps the first 128
/// bits in hex. No clock, no randomness: the same key on every rerun in every process, and
/// different imports (other site, actor, DocType or label) get different keys (module docs).
pub(super) fn command_text(
    inv: &Invocation,
    kind: char,
    doctype: &str,
    batch_label: &str,
    row: u64,
) -> String {
    let mut hasher = Sha256::new();
    let mut part = |text: &str| {
        hasher.update((text.len() as u64).to_be_bytes());
        hasher.update(text.as_bytes());
    };
    part("spot-import/command-id/v1");
    part(inv.site.as_str());
    part(&actor_text(&inv.actor));
    part(doctype);
    part(batch_label);
    part(&row.to_string());
    part(&kind.to_string());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("imp{kind}-{hex}-{row}")
}

/// A stable text for who acts (the real person too, when they act as someone else).
fn actor_text(actor: &Actor) -> String {
    match actor {
        Actor::User { id, via } => format!(
            "user:{id}:{}",
            via.map_or_else(String::new, |person| person.to_string())
        ),
        Actor::Consumer { id } => format!("consumer:{id}:"),
        Actor::System { component } => format!("system:{}", component.as_str()),
    }
}

/// A copy of the invocation that carries the derived command id.
fn invocation_for(
    inv: &Invocation,
    kind: char,
    doctype: &str,
    batch_label: &str,
    row: u64,
) -> Result<Invocation, ImportRowError> {
    let text = command_text(inv, kind, doctype, batch_label, row);
    invocation_with_id(inv, &text).map_err(|error| row_error(row, &error))
}

/// A copy of the invocation with `text` as its command id.
///
/// A refused id is an error, never a silent fallback to the caller's own id: that one could be
/// shared by every document of the run, and the engine's dedupe would then hand back the first
/// document for all of them.
pub(super) fn invocation_with_id(inv: &Invocation, text: &str) -> Result<Invocation, AppError> {
    match CommandId::new(text) {
        Ok(command_id) => {
            let mut copy = inv.clone();
            copy.command_id = Some(command_id);
            Ok(copy)
        }
        Err(error) => Err(AppError::new(
            ErrorClass::Internal,
            CONFIG_COMMAND_ID,
            format!("the import could not build a valid command id: {error}"),
        )),
    }
}

/// The next reason to stop, from the cancellation flag and the error limit.
fn stop_reason(options: &ApplyOptions, summary: &ApplySummary) -> Option<StopReason> {
    if options.cancel.is_cancelled() {
        return Some(StopReason::Cancelled);
    }
    if options.max_errors > 0 && summary.failed >= options.max_errors {
        return Some(StopReason::TooManyErrors);
    }
    None
}
