//! Draft records of the form step (process-first entry, ADR-080).
//!
//! A form step stores what a person typed as a **draft record**: not yet a business document,
//! so it is not in the DocType's table, not in any list, not in search and not in reports. It
//! lives in this module's own table and is created, changed and submitted only through the
//! operations below, in the same transaction as the step's transition when `FormService` drives
//! it (see [`PgDraftWriter`]).
//!
//! # Who can read one
//!
//! - **The requester** (the person who entered it) always, while it is a draft.
//! - **The Run's participants** once the draft is **submitted**: the people the Run handed a
//!   grant to (the actors of its steps), through the [`RunParticipants`] seam.
//! - Nobody else. Another user's read, change or submit of a draft answers exactly what an
//!   unknown id answers (`None` / `NotYours`), so the table is not an existence oracle.
//!
//! # Retention
//!
//! A draft nobody touched for `draft_retention_days` (default 30) is removed by the module's
//! service, on the holder of the lease, in batches. Submitting or saving touches it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, Value as Json};
use spot_app_api::error::{StorageOp, map_storage_error};
use spot_app_api::prelude::OffsetDateTime;
use spot_app_api::{AppError, ErrorClass};
use spot_bpm_entry::{
    CompletionPlanner, DraftSave, DraftWriter, EntryError, FieldIssue, FormConfigs, FormService,
    Inbox, RequestAuthority,
};
use spot_bpm_model::DraftMode;
use spot_dag_exec::Unit;
use spot_dag_exec::unit::WorkFuture;
use spot_postgres::PgTransaction;

use super::grants::micros;
use super::ledger::Ledger;

/// The most bytes of values one draft may hold.
pub const MAX_DRAFT_BYTES: usize = 256 * 1024;

/// The most a field name may be, characters.
const MAX_FIELD_NAME: usize = 128;

const UPSERT_DRAFT: &str = "\
INSERT INTO spot_bpm_gov_draft \
    (tenant, id, request, node, doctype, owner, state, payload, created_us, updated_us) \
VALUES ($1, $2, $3, $4, $5, $6, 'draft', $7, $8, $8) \
ON CONFLICT (tenant, id) DO UPDATE SET payload = EXCLUDED.payload, updated_us = EXCLUDED.updated_us \
WHERE spot_bpm_gov_draft.state = 'draft' AND spot_bpm_gov_draft.owner = EXCLUDED.owner \
  AND spot_bpm_gov_draft.request = EXCLUDED.request";

const UPDATE_DRAFT: &str = "\
UPDATE spot_bpm_gov_draft SET payload = $5, updated_us = $6 \
WHERE tenant = $1 AND id = $2 AND owner = $3 AND request = $4 AND state = 'draft'";

const SUBMIT_DRAFT: &str = "\
UPDATE spot_bpm_gov_draft SET state = 'submitted', submitted_us = $4, updated_us = $4 \
WHERE tenant = $1 AND id = $2 AND owner = $3 AND state = 'draft'";

const GET_DRAFT: &str = "\
SELECT request, node, doctype, owner, state, payload, created_us, updated_us, submitted_us \
FROM spot_bpm_gov_draft WHERE tenant = $1 AND id = $2";

const LIST_DRAFTS: &str = "\
SELECT id FROM spot_bpm_gov_draft WHERE tenant = $1 AND owner = $2 AND state = 'draft' \
ORDER BY updated_us DESC, id LIMIT $3";

const IS_PARTICIPANT: &str = "\
SELECT 1 FROM spot_bpm_gov_request WHERE tenant = $1 AND request = $2 AND actor = $3 LIMIT 1";

const REAP_DRAFTS: &str = "\
DELETE FROM spot_bpm_gov_draft WHERE tenant = $1 AND id IN \
(SELECT id FROM spot_bpm_gov_draft WHERE tenant = $1 AND updated_us < $2 ORDER BY updated_us LIMIT $3)";

/// Whether a draft is still being entered or has been submitted to its Run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftState {
    /// Entered, not submitted: readable by the requester only.
    Draft,
    /// Submitted: readable by the requester and the Run's participants.
    Submitted,
}

/// One draft record, as a reader who may see it gets it.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftRecord {
    /// The draft's id.
    pub id: String,
    /// The request (Run) it belongs to.
    pub request: String,
    /// The form step that holds it.
    pub node: String,
    /// The record type the draft is for.
    pub doctype: String,
    /// The typed principal reference of the requester.
    pub owner: String,
    /// Draft or submitted.
    pub state: DraftState,
    /// What was entered.
    pub values: Json,
    /// When it was created, unix microseconds.
    pub created_us: i64,
    /// When it was last touched, unix microseconds.
    pub updated_us: i64,
}

/// Why a draft operation failed. The messages carry ids and tokens, never a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftError {
    /// Not found, or not the caller's to see: one answer for both.
    NotYours,
    /// The draft is submitted or owned by someone else, so it cannot be saved over.
    NotOpen,
    /// The values are too large or not an object.
    TooLarge,
    /// The database failed.
    Unavailable,
}

impl DraftError {
    /// The stable code this refusal carries.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotYours => spot_bpm_entry::codes::STEP_NOT_OPEN,
            Self::NotOpen | Self::TooLarge => spot_bpm_entry::codes::DRAFT_FAILED,
            Self::Unavailable => spot_bpm_entry::codes::INBOX,
        }
    }
}

/// Who took part in a Run: the people who may read a submitted draft of it.
pub trait RunParticipants: Send + Sync {
    /// Whether `actor` took part in `request`.
    fn took_part<'a>(
        &'a self,
        request: &'a str,
        actor: &'a str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// The participants as the ledger knows them: the actors a Run's steps were handed grants for.
pub struct LedgerParticipants {
    ledger: Ledger,
}

impl LedgerParticipants {
    /// The participants of `ledger`'s site.
    #[must_use]
    pub fn new(ledger: Ledger) -> Self {
        Self { ledger }
    }
}

impl RunParticipants for LedgerParticipants {
    fn took_part<'a>(
        &'a self,
        request: &'a str,
        actor: &'a str,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let Ok(client) = self.ledger.pool().get().await else {
                // An unreadable source denies.
                return false;
            };
            client
                .query_opt(IS_PARTICIPANT, &[&self.ledger.tenant(), &request, &actor])
                .await
                .map(|row| row.is_some())
                .unwrap_or(false)
        })
    }
}

/// The drafts of one site.
pub struct DraftStore {
    ledger: Ledger,
    participants: Arc<dyn RunParticipants>,
}

impl DraftStore {
    /// The drafts of `ledger`'s site, with `participants` deciding who may read a submitted one.
    #[must_use]
    pub fn new(ledger: Ledger, participants: Arc<dyn RunParticipants>) -> Self {
        Self {
            ledger,
            participants,
        }
    }

    /// Saves `values` as the draft `id` of `owner` for step `node` of `request`: creates it, or
    /// replaces the values of one the same person still has open.
    ///
    /// # Errors
    /// [`DraftError::TooLarge`], [`DraftError::NotOpen`] or [`DraftError::Unavailable`].
    #[allow(clippy::too_many_arguments)]
    pub async fn save(
        &self,
        id: &str,
        request: &str,
        node: &str,
        doctype: &str,
        owner: &str,
        values: &Map<String, Json>,
        now: OffsetDateTime,
    ) -> Result<(), DraftError> {
        let payload = encode(values)?;
        let client = self
            .ledger
            .pool()
            .get()
            .await
            .map_err(|_| DraftError::Unavailable)?;
        let at = micros(now);
        let changed = client
            .execute(
                UPSERT_DRAFT,
                &[
                    &self.ledger.tenant(),
                    &id,
                    &request,
                    &node,
                    &doctype,
                    &owner,
                    &payload,
                    &at,
                ],
            )
            .await
            .map_err(|_| DraftError::Unavailable)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(DraftError::NotOpen)
        }
    }

    /// The draft `id` as `actor` may see it, or `None`: for an unknown id, for another person's
    /// draft, and for a submitted draft of a Run `actor` took no part in. One answer for all.
    ///
    /// # Errors
    /// [`DraftError::Unavailable`] when the database fails (never an allow).
    pub async fn get(&self, id: &str, actor: &str) -> Result<Option<DraftRecord>, DraftError> {
        let client = self
            .ledger
            .pool()
            .get()
            .await
            .map_err(|_| DraftError::Unavailable)?;
        let row = client
            .query_opt(GET_DRAFT, &[&self.ledger.tenant(), &id])
            .await
            .map_err(|_| DraftError::Unavailable)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let record = decode(id, &row)?;
        if record.owner == actor {
            return Ok(Some(record));
        }
        let visible = record.state == DraftState::Submitted
            && self.participants.took_part(&record.request, actor).await;
        Ok(visible.then_some(record))
    }

    /// Submits the draft `id` of `owner`: from then on the Run's participants may read it.
    ///
    /// # Errors
    /// [`DraftError::NotYours`] for an unknown id, someone else's draft or one already
    /// submitted; [`DraftError::Unavailable`].
    pub async fn submit(
        &self,
        id: &str,
        owner: &str,
        now: OffsetDateTime,
    ) -> Result<(), DraftError> {
        let client = self
            .ledger
            .pool()
            .get()
            .await
            .map_err(|_| DraftError::Unavailable)?;
        let at = micros(now);
        let changed = client
            .execute(SUBMIT_DRAFT, &[&self.ledger.tenant(), &id, &owner, &at])
            .await
            .map_err(|_| DraftError::Unavailable)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(DraftError::NotYours)
        }
    }

    /// The ids of `owner`'s open drafts, most recently touched first, at most `limit`.
    ///
    /// # Errors
    /// [`DraftError::Unavailable`].
    pub async fn list(&self, owner: &str, limit: u32) -> Result<Vec<String>, DraftError> {
        let client = self
            .ledger
            .pool()
            .get()
            .await
            .map_err(|_| DraftError::Unavailable)?;
        let limit = i64::from(limit.min(500));
        let rows = client
            .query(LIST_DRAFTS, &[&self.ledger.tenant(), &owner, &limit])
            .await
            .map_err(|_| DraftError::Unavailable)?;
        rows.iter()
            .map(|row| {
                row.try_get::<_, String>(0)
                    .map_err(|_| DraftError::Unavailable)
            })
            .collect()
    }

    /// Removes up to `batch` drafts nobody touched since `before`. Returns how many went.
    ///
    /// # Errors
    /// [`DraftError::Unavailable`].
    pub async fn reap(&self, before: OffsetDateTime, batch: u32) -> Result<u64, DraftError> {
        let client = self
            .ledger
            .pool()
            .get()
            .await
            .map_err(|_| DraftError::Unavailable)?;
        let cutoff = micros(before);
        let limit = i64::from(batch.clamp(1, 10_000));
        client
            .execute(REAP_DRAFTS, &[&self.ledger.tenant(), &cutoff, &limit])
            .await
            .map_err(|_| DraftError::Unavailable)
    }
}

fn encode(values: &Map<String, Json>) -> Result<String, DraftError> {
    let payload = serde_json::to_string(values).map_err(|_| DraftError::TooLarge)?;
    if payload.len() > MAX_DRAFT_BYTES {
        return Err(DraftError::TooLarge);
    }
    Ok(payload)
}

fn decode(
    id: &str,
    row: &deadpool_postgres::tokio_postgres::Row,
) -> Result<DraftRecord, DraftError> {
    let bad = |_| DraftError::Unavailable;
    let state: String = row.try_get(4).map_err(bad)?;
    let payload: String = row.try_get(5).map_err(bad)?;
    Ok(DraftRecord {
        id: id.to_owned(),
        request: row.try_get(0).map_err(bad)?,
        node: row.try_get(1).map_err(bad)?,
        doctype: row.try_get(2).map_err(bad)?,
        owner: row.try_get(3).map_err(bad)?,
        state: if state == "submitted" {
            DraftState::Submitted
        } else {
            DraftState::Draft
        },
        values: serde_json::from_str(&payload).map_err(|_| DraftError::Unavailable)?,
        created_us: row.try_get(6).map_err(bad)?,
        updated_us: row.try_get(7).map_err(bad)?,
    })
}

/// `FormService`'s draft writer: the draft goes into this module's table **inside the unit's
/// transaction**, so the step's transition and the draft commit or roll back together.
pub struct PgDraftWriter {
    tenant: String,
}

impl PgDraftWriter {
    /// The writer of `tenant`'s drafts.
    #[must_use]
    pub fn new(tenant: &str) -> Self {
        Self {
            tenant: tenant.to_owned(),
        }
    }
}

impl DraftWriter<PgTransaction> for PgDraftWriter {
    /// Checks what can be checked without the record type's lifecycle: a draft is not a
    /// business document yet, so the type's own validation runs when the draft becomes one.
    fn validate(&self, save: &DraftSave) -> Result<(), Vec<FieldIssue>> {
        let mut issues = Vec::new();
        if save.doctype.trim().is_empty() {
            issues.push(FieldIssue::new(
                "record_type",
                "blank",
                "The record type is required.",
            ));
        }
        for field in save.values.keys() {
            if field.is_empty()
                || field.chars().count() > MAX_FIELD_NAME
                || !field
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            {
                issues.push(FieldIssue::new(
                    "values",
                    "field_name",
                    "A field name is not valid.",
                ));
                break;
            }
        }
        if encode(&save.values).is_err() {
            issues.push(FieldIssue::new(
                "values",
                "too_large",
                "The entered values are too large to keep as a draft.",
            ));
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    fn write<'t>(
        self: Arc<Self>,
        tx: &'t mut PgTransaction,
        save: Arc<DraftSave>,
    ) -> WorkFuture<'t> {
        Box::pin(async move {
            let payload =
                encode(&save.values).map_err(|_| draft_error("the draft is too large"))?;
            let id = save.document.to_string();
            let at = micros(save.now);
            let changed = match save.mode {
                DraftMode::Update => {
                    tx.raw_execute(
                        UPDATE_DRAFT,
                        &[
                            &self.tenant,
                            &id,
                            &save.actor,
                            &save.context.request,
                            &payload,
                            &at,
                        ],
                    )
                    .await
                }
                _ => {
                    tx.raw_execute(
                        UPSERT_DRAFT,
                        &[
                            &self.tenant,
                            &id,
                            &save.context.request,
                            &save.context.node,
                            &save.doctype,
                            &save.actor,
                            &payload,
                            &at,
                        ],
                    )
                    .await
                }
            }
            .map_err(|error| map_storage_error(&error, StorageOp::Write))?;
            if changed == 1 {
                Ok(())
            } else {
                Err(draft_error(
                    "the draft is submitted or belongs to someone else",
                ))
            }
        })
    }
}

fn draft_error(detail: &str) -> AppError {
    EntryError::DraftFailed {
        detail: detail.to_owned(),
    }
    .to_app_error()
}

/// `FormService` over this module's draft writer. The unit of work, the planner, the inbox, the
/// step configurations and the authority are the host's: the unit must be a `Unit<PgTransaction>`
/// (the PostgreSQL DAG execution store is one).
#[must_use]
pub fn form_service<U: Unit<PgTransaction>>(
    unit: Arc<U>,
    planner: Arc<dyn CompletionPlanner>,
    inbox: Arc<dyn Inbox>,
    configs: Arc<dyn FormConfigs>,
    authority: Arc<dyn RequestAuthority>,
    writer: Arc<PgDraftWriter>,
) -> FormService<PgTransaction, U> {
    FormService::new(
        unit,
        planner,
        writer as Arc<dyn DraftWriter<PgTransaction>>,
        inbox,
        configs,
        authority,
    )
}

/// The class a refused draft call rides, for a route that maps [`DraftError`].
#[must_use]
pub fn class_of(error: &DraftError) -> ErrorClass {
    match error {
        DraftError::NotYours => ErrorClass::NotFound,
        DraftError::NotOpen => ErrorClass::InvalidState,
        DraftError::TooLarge => ErrorClass::Validation,
        DraftError::Unavailable => ErrorClass::Unavailable,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn values_over_the_cap_are_refused() {
        let mut values = Map::new();
        values.insert("note".to_owned(), Json::String("x".repeat(MAX_DRAFT_BYTES)));
        assert_eq!(encode(&values), Err(DraftError::TooLarge));
        values.insert("note".to_owned(), Json::String("ok".to_owned()));
        assert!(encode(&values).is_ok());
    }

    #[test]
    fn every_refusal_has_a_class() {
        assert_eq!(class_of(&DraftError::NotYours), ErrorClass::NotFound);
        assert_eq!(class_of(&DraftError::Unavailable), ErrorClass::Unavailable);
    }
}
