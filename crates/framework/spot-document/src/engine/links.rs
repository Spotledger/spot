//! Link integrity on every write path (WP-DOC-06.T2; ADR-053, ADR-42).
//!
//! Storage owns referential integrity; `spot-links` explains it. The engine calls that crate at
//! the one write stage, inside the operation's own transaction, so a refusal rolls everything
//! back and no dangling link is ever committed:
//!
//! - **create, update, update of a posted document, post**: every `Link`, `DynamicLink` and
//!   table-multiselect value is validated before the storage write (target exists, right
//!   DocType, dynamic-link type allowed, not cancelled, a posted source not pointing at a draft),
//!   with the exact field path. `amended_from` is exempt from the cancelled-target rule: an
//!   amendment points at the cancelled document it replaces by definition;
//! - **delete, cancel**: the documents that still refer to the target are looked up first and
//!   the refusal is `links.referenced`, listing them. A storage `ReferenceViolation` that slips
//!   past (a reference created concurrently) is mapped to the same code, never to a bare
//!   constraint name, at the delete and at the commit;
//! - **rename**: the new name is checked against the other documents of a DocType whose names
//!   are unique (those with a `naming` property), under the same transaction lock create takes.
//!
//! # No existence leak
//!
//! The existence, state and referrer queries run through the **caller's read scope**, exactly
//! like a list read: a target the caller may not read is reported with the same
//! `links.target_not_found` as one that does not exist, and the referrers listed on a refusal
//! are only ones the caller may read. A failure to plan the scoped query is treated as "nothing
//! readable", never surfaced, because its error would tell a forbidden target from a missing one.
//! The dynamic-link type is an allow-list: only a non-child, non-reserved DocType of the
//! snapshot can be named, and the query is built from the snapshot's definition, never from the
//! stored text.

use std::sync::Arc;

use spot_app_api::context::{AMENDED_FROM, Invocation};
use spot_app_api::error::StorageOp;
use spot_app_api::prelude::{Expr, Name, Query, QueryPath, RecordId, Rows, Selection, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, DocState, DocTypeDef, Document, Operation};
use spot_links::{
    LinkChecks, ReferrerGroup, WhereUsed, check_rename_available, codes, explain_cancel,
    explain_delete, validate_links, validate_links_error, where_used,
};
use spot_meta::EffectiveMeta;
use spot_storage_contract::{QueryExecutor, StorageBackend};

use super::authz::{apply_masks, empty_page};
use super::work::Work;
use super::{Engine, Snapshot};

/// The registered wire code of a field whose link points at nothing the caller can see.
pub(super) const INVALID_LINK: &str = "document.invalid_link";

/// The seam `spot-links` runs its queries through, in the operation's transaction.
struct TxChecks<'a, B: StorageBackend> {
    engine: &'a Engine<B>,
    snap: &'a Snapshot,
    work: &'a mut Work<B::Tx>,
    inv: &'a Invocation,
    /// Whether the caller's read scope applies. Link and where-used checks are scoped; the
    /// uniqueness of a name is a property of the whole DocType and is not.
    scoped: bool,
}

impl<B: StorageBackend> LinkChecks for TxChecks<'_, B> {
    fn effective_meta(&self) -> Arc<EffectiveMeta> {
        self.snap.meta.clone()
    }

    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async move {
            if !self.scoped {
                return self
                    .work
                    .tx
                    .query(query)
                    .await
                    .map_err(|e| self.work.fail(e, StorageOp::Other));
            }
            // The caller's read rights compile into the query before it reaches storage. A
            // caller that may read nothing of the DocType, or whose rights cannot answer this
            // query, sees an empty result: the same as a document that is not there.
            let plan = match self.engine.query_plan(self.inv, query).await {
                Ok(Some(plan)) => plan,
                Ok(None) | Err(_) => return Ok(empty_page(query)),
            };
            let mut rows = self
                .work
                .tx
                .query(&plan.query)
                .await
                .map_err(|e| self.work.fail(e, StorageOp::Other))?;
            apply_masks(&mut rows, &plan.masks);
            Ok(rows)
        })
    }
}

/// The document as the post will leave it, for the "posted source" rule.
fn as_posted(doc: &Document) -> Document {
    let mut posted = doc.clone();
    if let Some(version) = doc.version() {
        posted.mark_saved(version, Some(DocState::Posted));
    }
    posted
}

impl<B: StorageBackend> Engine<B> {
    /// Validates every link value of `doc` before it is written. Runs for create, update,
    /// update of a posted document and post; every other operation changes no link.
    pub(super) async fn check_write_links(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        def: &DocTypeDef,
        op: Operation,
        doc: &Document,
    ) -> AppResult<()> {
        if !matches!(
            op,
            Operation::Create | Operation::Update | Operation::UpdatePosted | Operation::Post
        ) {
            return Ok(());
        }
        let posted;
        let subject = if op == Operation::Post {
            posted = as_posted(doc);
            &posted
        } else {
            doc
        };
        let mut checks = TxChecks {
            engine: self,
            snap,
            work,
            inv,
            scoped: true,
        };
        let mut issues = validate_links(def, subject, &mut checks).await?;
        issues.retain(|i| !(i.path.as_str() == AMENDED_FROM && i.code == codes::CANCELLED_TARGET));
        // The wire keeps its registered field code for a link to nothing readable
        // (`document.invalid_link`, contracts/native-api): missing and forbidden look the same.
        for issue in &mut issues {
            if issue.code == codes::TARGET_NOT_FOUND {
                issue.code = INVALID_LINK;
            }
        }
        match validate_links_error(issues) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Refuses a delete or a cancel while any document still refers to `doc`.
    ///
    /// Whether to refuse is decided over **every** referrer, readable or not: a referrer the
    /// caller may not read must still block the operation, or it would be orphaned. What the
    /// refusal says is limited to the caller's read scope: the readable referrers are listed
    /// and counted; if none is readable the error is the bare `links.referenced` with no field
    /// and no count, so it names nothing the caller may not see.
    pub(super) async fn check_unreferenced(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        def: &DocTypeDef,
        op: Operation,
        doc: &Document,
    ) -> AppResult<()> {
        // A cancel is blocked by live referrers only: one that is itself cancelled no longer
        // needs this document ("cancel the referencing documents first" must be satisfiable).
        let cancel = op == Operation::Cancel;
        let limit = if cancel { LIVE_SAMPLE } else { 1 };
        let mut all = TxChecks {
            engine: self,
            snap,
            work,
            inv,
            scoped: false,
        };
        let mut used = where_used(&mut all, def.name.as_str(), doc.id(), limit).await?;
        if cancel {
            used = live_referrers(&mut all, used).await?;
        }
        if used.total == 0 {
            return Ok(());
        }
        let work = all.work;
        let mut visible = TxChecks {
            engine: self,
            snap,
            work,
            inv,
            scoped: true,
        };
        let mut used = where_used(&mut visible, def.name.as_str(), doc.id(), limit).await?;
        if cancel {
            used = live_referrers(&mut visible, used).await?;
        }
        if used.total == 0 {
            return Err(if cancel {
                referenced_refusal(def, doc.id())
            } else {
                delete_refusal(referenced_refusal(def, doc.id()))
            });
        }
        Err(if cancel {
            explain_cancel(def, doc.id(), &used)
        } else {
            delete_refusal(explain_delete(def, doc.id(), &used))
        })
    }

    /// Refuses a rename onto a name another document of the DocType holds. Only a DocType with
    /// a `naming` property has unique names; for any other the title is a display text that
    /// several documents may share. The check is not scoped (a name is unique among all the
    /// DocType's documents) and holds the same lock create takes on the name.
    pub(super) async fn check_rename_free(
        &self,
        snap: &Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        def: &DocTypeDef,
        id: RecordId,
        new_name: &str,
    ) -> AppResult<()> {
        if snap.naming.get(def.name.as_str()).is_none() {
            return Ok(());
        }
        self.lock_name(work, def, new_name).await?;
        let mut checks = TxChecks {
            engine: self,
            snap,
            work,
            inv,
            scoped: false,
        };
        check_rename_available(def, id, new_name, &mut checks).await
    }
}

/// How many referrer ids per group a cancel looks at to tell live ones from cancelled ones.
const LIVE_SAMPLE: usize = 1000;

/// How many referrer ids per group a refusal lists.
const LISTED: usize = 20;

/// `used` without the referrers that are themselves cancelled. A group of a DocType that is not
/// posted has no cancelled documents. Ids beyond the sampled ones are counted as live, so a huge
/// group can only block more, never less.
async fn live_referrers(checks: &mut dyn LinkChecks, used: WhereUsed) -> AppResult<WhereUsed> {
    let meta = checks.effective_meta();
    let mut groups = Vec::new();
    let mut total = 0u64;
    for group in used.groups {
        let posted = meta.doctype(&group.doctype).is_some_and(|d| d.is_posted);
        let mut live: Vec<RecordId> = group.sample.clone();
        let unseen = group
            .count
            .saturating_sub(u64::try_from(group.sample.len()).unwrap_or(u64::MAX));
        if posted && !group.sample.is_empty() {
            let entity = Name::new(&group.doctype).map_err(internal)?;
            let alias = Name::new("referrer").map_err(internal)?;
            let id_field = Name::new("id").map_err(internal)?;
            let state_field = Name::new("doc_state").map_err(internal)?;
            let mut query = Query::from(&entity, &alias, QueryPath::Transactional);
            query.select.push(Selection {
                expr: Expr::field(&alias, &id_field),
                alias: id_field.clone(),
            });
            query.select.push(Selection {
                expr: Expr::field(&alias, &state_field),
                alias: state_field.clone(),
            });
            query.filter = Some(Expr::In(
                Box::new(Expr::field(&alias, &id_field)),
                group.sample.iter().map(|id| Value::Id(*id)).collect(),
            ));
            let rows = checks.query(&query).await?;
            let cancelled: Vec<RecordId> = rows
                .rows
                .iter()
                .filter_map(|row| match (row.first(), row.get(1)) {
                    (Some(Value::Id(id)), Some(Value::Text(state))) if state == "cancelled" => {
                        Some(*id)
                    }
                    _ => None,
                })
                .collect();
            live.retain(|id| !cancelled.contains(id));
        }
        let count = u64::try_from(live.len())
            .unwrap_or(u64::MAX)
            .saturating_add(unseen);
        if count == 0 {
            continue;
        }
        total = total.saturating_add(count);
        live.truncate(LISTED);
        groups.push(ReferrerGroup {
            doctype: group.doctype,
            field: group.field,
            count,
            sample: live,
        });
    }
    Ok(WhereUsed {
        doctype: used.doctype,
        id: used.id,
        groups,
        total,
    })
}

fn internal(error: impl std::fmt::Display) -> AppError {
    AppError::new(
        spot_app_api::ErrorClass::Internal,
        "document.internal_error",
        format!("link check: {error}"),
    )
}

/// A refused delete keeps the engine code it always had, `document.linked`, so no API consumer
/// or contract case sees a change; the spot-links code stays available as `details.reason`.
pub(super) fn delete_refusal(mut error: AppError) -> AppError {
    error.code = "document.linked".to_owned();
    error
        .details
        .insert("reason".to_owned(), codes::REFERENCED.into());
    error
}

/// The bare `links.referenced` refusal: documents refer to the target, but the caller may read
/// none of them, or storage reported the violation without a look-up. It names no field (the
/// storage error's field would name a referrer's DocType) and no count.
pub(super) fn referenced_refusal(def: &DocTypeDef, id: RecordId) -> AppError {
    let bare = spot_storage_contract::StorageError::new(
        spot_storage_contract::ErrorKind::ReferenceViolation,
        "reference violation",
    );
    spot_links::explain_reference_violation(def, id, &bare)
}

/// Maps a commit that storage refused with a reference violation after a delete (a deferred
/// constraint, or a reference committed by a concurrent transaction) to `links.referenced`.
pub(super) fn commit_violation(
    snap: &Snapshot,
    target: Option<&(String, RecordId)>,
    error: &spot_storage_contract::StorageError,
) -> Option<AppError> {
    if error.kind != spot_storage_contract::ErrorKind::ReferenceViolation {
        return None;
    }
    let (doctype, id) = target?;
    let def = snap.doctype(doctype).ok()?;
    Some(delete_refusal(referenced_refusal(&def, *id)))
}
