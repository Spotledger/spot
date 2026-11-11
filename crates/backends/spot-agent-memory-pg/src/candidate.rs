//! The candidate-remedy queue: the learning loop's state, kept beside the memory it was learned
//! from.
//!
//! A [`CandidateRemedy`] is the domain model's type and its transitions are crate-private: only a
//! `PromotionPort` moves one to `Promoted`, and only with a CI run id. This module therefore
//! **persists** a candidate and reads back a projection of it ([`StoredCandidate`]) — it never
//! rebuilds the domain type, because rebuilding it would hand a row the power to promote itself.
//! The table carries the same rule as a `CHECK`
//! (`spot_memory_candidate_promoted_needs_ci`): a row claiming `promoted` must name the CI run,
//! so the database refuses a promoted row with no CI run id (a non-null check; it does not prove the run exists or passed).
//!
//! The row also keeps the self-test scenario skeleton
//! ([`ScenarioSkeleton`](spot_agent_memory::ScenarioSkeleton)), so the queue can be handed to CI
//! from the database as well as from a file bundle.

use serde_json::{Map, Value};
use spot_agent_memory::{
    CandidateId, CandidateRemedy, CandidateStatus, CiRunId, MemoryError, ScenarioSkeleton, Tier,
};
use tokio_postgres::Client;

use crate::codec::{corrupt, remedy_from_text, tier_from_i16, tier_to_i16};
use crate::error::store;
use crate::wire;

/// The columns a candidate row is read by.
const COLUMNS: &str = "id, remedy, procedure_id, status, ci_run, rejection, confirmations, symptoms, \
     title, tier, risk, affects, preconditions, undo_kind, undo_detail, updated_ms";

const PUT_TEMPLATE: &str = "INSERT INTO spot_memory_candidate (tenant, id, remedy, procedure_id, status, \
     ci_run, rejection, confirmations, symptoms, steps, title, tier, risk, affects, preconditions, \
     undo_kind, undo_detail, params, scenario, updated_ms) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::text::jsonb, $11, $12, $13, $14, $15, $16, $17, \
     $18::text::jsonb, $19::text::jsonb, $20) \
     ON CONFLICT (tenant, id) DO UPDATE SET remedy = EXCLUDED.remedy, \
     procedure_id = EXCLUDED.procedure_id, status = EXCLUDED.status, ci_run = EXCLUDED.ci_run, \
     rejection = EXCLUDED.rejection, confirmations = EXCLUDED.confirmations, \
     symptoms = EXCLUDED.symptoms, steps = EXCLUDED.steps, title = EXCLUDED.title, \
     tier = EXCLUDED.tier, risk = EXCLUDED.risk, affects = EXCLUDED.affects, \
     preconditions = EXCLUDED.preconditions, undo_kind = EXCLUDED.undo_kind, \
     undo_detail = EXCLUDED.undo_detail, params = EXCLUDED.params, scenario = EXCLUDED.scenario, \
     updated_ms = EXCLUDED.updated_ms \
     WHERE ({RANK_OLD}) <= ({RANK_NEW})";

/// A status's place in the pipeline, as SQL over `prefix`'s `status` column: a later stage never
/// gives way to an earlier one, so a stale `proposed` write cannot overwrite a `promoted` row.
const RANK: &str = "CASE {P}status WHEN 'proposed' THEN 0 WHEN 'in_ci' THEN 1 \
     WHEN 'passed' THEN 2 WHEN 'rejected' THEN 2 WHEN 'promoted' THEN 3 ELSE 0 END";

/// The upsert statement, with the monotonic status guard filled in.
fn put_statement() -> String {
    PUT_TEMPLATE
        .replace("{RANK_OLD}", &RANK.replace("{P}", "spot_memory_candidate."))
        .replace("{RANK_NEW}", &RANK.replace("{P}", "EXCLUDED."))
}

/// The most candidates one call to [`list`] returns, and the most the queue holds.
pub const MAX_CANDIDATES: usize = 10_000;

/// A queued candidate, as the row records it.
///
/// This is a projection, not the domain type: `spot_agent_memory::CandidateRemedy`'s transitions
/// are crate-private, so a store that rebuilt one from a row could bypass the CI gate. Every field
/// a host needs to show the queue, hand it to CI and audit it is here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCandidate {
    /// The candidate id (its directory name in the `candidates/` queue).
    pub id: String,
    /// The remedy id the promoted remedy is registered under.
    pub remedy: String,
    /// The procedure it was learned from.
    pub procedure: String,
    /// Where it is in the pipeline.
    pub status: CandidateStatus,
    /// The CI run that is running or has run it.
    pub ci_run: Option<String>,
    /// Why CI rejected it, when it did.
    pub rejection: Option<String>,
    /// How many resolved incidents back it.
    pub confirmations: u32,
    /// The symptom signature it fixes.
    pub symptoms: Vec<String>,
    /// Its title.
    pub title: String,
    /// The tier it was learned at.
    pub tier: Tier,
    /// Its declared risk.
    pub risk: String,
    /// The DocTypes it affects.
    pub affects: Vec<String>,
    /// Its preconditions.
    pub preconditions: String,
    /// How undo works (`reversible` or `irreversible`) and its detail.
    pub undo: (String, String),
    /// When the row was last written, in Unix milliseconds (supplied by the caller).
    pub updated_ms: i64,
}

impl StoredCandidate {
    /// Whether the row is a promotion CI proved: the only status a remedy may run at.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.status.is_usable()
    }
}

/// Writes one candidate, replacing the row of the same id.
///
/// The write is monotonic: a row never moves to an earlier stage (`proposed` < `in_ci` <
/// `passed`/`rejected` < `promoted`), so a stale write is a no-op rather than an overwrite of a
/// promotion. A new id is refused when the queue already holds [`MAX_CANDIDATES`].
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses (including the `CHECK` that refuses a
/// promotion with no CI run), and [`MemoryError::Serialise`] when a structure cannot be rendered.
pub(crate) async fn put(
    client: &Client,
    tenant: &spot_agent_memory::TenantId,
    candidate: &CandidateRemedy,
    updated_ms: i64,
) -> Result<(), MemoryError> {
    let owner = tenant.as_str();
    let id = candidate.id().as_str();
    ensure_room(client, owner, id).await?;
    let remedy = candidate.remedy().to_string();
    let procedure = candidate.procedure().to_string();
    let status = candidate.status().as_str();
    let ci_run = candidate.ci_run().map(ToString::to_string);
    let rejection = candidate.rejection().map(str::to_owned);
    let confirmations =
        i32::try_from(candidate.confirmations()).map_err(|_| MemoryError::Store {
            detail: "a candidate's confirmation count does not fit the column".to_owned(),
        })?;
    let symptoms: Vec<String> = candidate
        .symptoms()
        .iter()
        .map(|symptom| symptom.as_str().to_owned())
        .collect();
    let steps = wire::steps_to_json(candidate.steps())?;
    let meta = candidate.meta();
    let title = &meta.title;
    let tier = tier_to_i16(meta.tier);
    let risk = meta.risk.as_str();
    let affects: Vec<String> = meta
        .affects
        .iter()
        .map(|doctype| doctype.as_str().to_owned())
        .collect();
    let preconditions = &meta.preconditions;
    // The undo kind is `spot-remedies`' type, rendered through its own serde form rather than
    // matched on: this crate does not depend on that crate, and a stored row needs the words, not
    // the enum.
    let undo = serde_json::to_value(&meta.undo).map_err(|error| MemoryError::Serialise {
        what: format!("the undo of `{id}`"),
        detail: error.to_string(),
    })?;
    let (undo_kind, undo_detail) = undo_parts(&undo)?;
    let params = render(params_of(candidate));
    let scenario = render_scenario(candidate)?;
    client
        .execute(
            &put_statement(),
            &[
                &owner,
                &id,
                &remedy,
                &procedure,
                &status,
                &ci_run,
                &rejection,
                &confirmations,
                &symptoms,
                &steps,
                &title,
                &tier,
                &risk,
                &affects,
                &preconditions,
                &undo_kind,
                &undo_detail,
                &params,
                &scenario,
                &updated_ms,
            ],
        )
        .await
        .map_err(|error| store(&error))?;
    Ok(())
}

/// The queued candidate with `id`, if there is one.
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses or a row cannot be decoded.
pub(crate) async fn get(
    client: &Client,
    tenant: &spot_agent_memory::TenantId,
    id: &CandidateId,
) -> Result<Option<StoredCandidate>, MemoryError> {
    let owner = tenant.as_str();
    let statement =
        format!("SELECT {COLUMNS} FROM spot_memory_candidate WHERE tenant = $1 AND id = $2");
    let rows = client
        .query(&statement, &[&owner, &id.as_str()])
        .await
        .map_err(|error| store(&error))?;
    rows.first().map(candidate_from_row).transpose()
}

/// A new candidate id is refused when the queue is full; an existing id may always be updated.
async fn ensure_room(client: &Client, tenant: &str, id: &str) -> Result<(), MemoryError> {
    let row = client
        .query_one(
            "SELECT count(*)::bigint, coalesce(bool_or(id = $2), false) \
             FROM spot_memory_candidate WHERE tenant = $1",
            &[&tenant, &id],
        )
        .await
        .map_err(|error| store(&error))?;
    let count: i64 = row.try_get(0).map_err(|_| corrupt("count"))?;
    let present: bool = row.try_get(1).map_err(|_| corrupt("count"))?;
    let max = i64::try_from(MAX_CANDIDATES).unwrap_or(i64::MAX);
    if !present && count >= max {
        return Err(MemoryError::Store {
            detail: "the candidate queue of this tenant is full".to_owned(),
        });
    }
    Ok(())
}

/// One page of queued candidates in id order, after `after_id`; `limit` is clamped to
/// `1..=`[`crate::row::MAX_PAGE`].
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses or a row cannot be decoded.
pub(crate) async fn list_page(
    client: &Client,
    tenant: &spot_agent_memory::TenantId,
    after_id: Option<&str>,
    limit: usize,
) -> Result<Vec<StoredCandidate>, MemoryError> {
    let owner = tenant.as_str();
    let limit = crate::row::clamp_page(limit);
    let statement = format!(
        "SELECT {COLUMNS} FROM spot_memory_candidate WHERE tenant = $1 \
         AND ($2::text IS NULL OR id > $2::text) ORDER BY id LIMIT $3"
    );
    let rows = client
        .query(&statement, &[&owner, &after_id, &limit])
        .await
        .map_err(|error| store(&error))?;
    rows.iter().map(candidate_from_row).collect()
}

/// Every queued candidate, in id order, read page by page, at most [`MAX_CANDIDATES`].
///
/// # Errors
/// [`MemoryError::Store`] when the memory database refuses or a row cannot be decoded.
pub(crate) async fn list(
    client: &Client,
    tenant: &spot_agent_memory::TenantId,
) -> Result<Vec<StoredCandidate>, MemoryError> {
    let mut all: Vec<StoredCandidate> = Vec::new();
    let mut after: Option<String> = None;
    while all.len() < MAX_CANDIDATES {
        let want = (MAX_CANDIDATES - all.len()).min(crate::row::MAX_PAGE);
        let page = list_page(client, tenant, after.as_deref(), want).await?;
        let full = page.len() >= want;
        if let Some(last) = page.last() {
            after = Some(last.id.clone());
        }
        all.extend(page);
        if !full {
            break;
        }
    }
    Ok(all)
}

fn candidate_from_row(row: &tokio_postgres::Row) -> Result<StoredCandidate, MemoryError> {
    let status_text: String = row.try_get(3).map_err(|_| corrupt("status"))?;
    let ci_text: Option<String> = row.try_get(4).map_err(|_| corrupt("ci_run"))?;
    Ok(StoredCandidate {
        id: text(row, 0, "id")?,
        remedy: remedy_from_text("remedy", &text(row, 1, "remedy")?)?.to_string(),
        procedure: text(row, 2, "procedure_id")?,
        status: status_from_text(&status_text)?,
        ci_run: match ci_text {
            Some(run) => Some(
                CiRunId::parse(&run)
                    .map_err(|_| corrupt("ci_run"))?
                    .to_string(),
            ),
            None => None,
        },
        rejection: row.try_get(5).map_err(|_| corrupt("rejection"))?,
        confirmations: u32::try_from(
            row.try_get::<_, i32>(6)
                .map_err(|_| corrupt("confirmations"))?,
        )
        .map_err(|_| corrupt("confirmations"))?,
        symptoms: strings(row, 7, "symptoms")?,
        title: text(row, 8, "title")?,
        tier: tier_from_i16("tier", row.try_get(9).map_err(|_| corrupt("tier"))?)?,
        risk: text(row, 10, "risk")?,
        affects: strings(row, 11, "affects")?,
        preconditions: text(row, 12, "preconditions")?,
        undo: (text(row, 13, "undo_kind")?, text(row, 14, "undo_detail")?),
        updated_ms: integer(row, 15, "updated_ms")?,
    })
}

/// The status a stored text names.
fn status_from_text(text: &str) -> Result<CandidateStatus, MemoryError> {
    match text {
        "proposed" => Ok(CandidateStatus::Proposed),
        "in_ci" => Ok(CandidateStatus::InCi),
        "passed" => Ok(CandidateStatus::Passed),
        "rejected" => Ok(CandidateStatus::Rejected),
        "promoted" => Ok(CandidateStatus::Promoted),
        _ => Err(corrupt("status")),
    }
}

/// A candidate's parameter schema as JSON: `{ "name": "type" }`.
fn params_of(candidate: &CandidateRemedy) -> Map<String, Value> {
    candidate
        .meta()
        .params
        .properties
        .iter()
        .map(|(name, kind)| (name.clone(), Value::String(kind.json_name().to_owned())))
        .collect()
}

/// The kind (`reversible`, `irreversible`) and the detail of a rendered undo.
fn undo_parts(value: &Value) -> Result<(String, String), MemoryError> {
    let object = value.as_object().ok_or_else(|| corrupt("undo"))?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("undo"))?;
    let detail = object
        .get("detail")
        .or_else(|| object.get("reason"))
        .and_then(Value::as_str)
        .ok_or_else(|| corrupt("undo"))?;
    Ok((kind.to_owned(), detail.to_owned()))
}

/// A JSON object rendered compactly for a `jsonb` column.
fn render(object: Map<String, Value>) -> String {
    Value::Object(object).to_string()
}

/// The self-test scenario skeleton, as the `jsonb` column.
fn render_scenario(candidate: &CandidateRemedy) -> Result<String, MemoryError> {
    serde_json::to_string(&ScenarioSkeleton::of(candidate)).map_err(|error| {
        MemoryError::Serialise {
            what: format!("the scenario skeleton of `{}`", candidate.id()),
            detail: error.to_string(),
        }
    })
}

fn text(row: &tokio_postgres::Row, index: usize, column: &str) -> Result<String, MemoryError> {
    row.try_get(index).map_err(|_| corrupt(column))
}

fn integer(row: &tokio_postgres::Row, index: usize, column: &str) -> Result<i64, MemoryError> {
    row.try_get(index).map_err(|_| corrupt(column))
}

fn strings(
    row: &tokio_postgres::Row,
    index: usize,
    column: &str,
) -> Result<Vec<String>, MemoryError> {
    row.try_get(index).map_err(|_| corrupt(column))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn every_status_round_trips_through_its_column_text() {
        for status in [
            CandidateStatus::Proposed,
            CandidateStatus::InCi,
            CandidateStatus::Passed,
            CandidateStatus::Rejected,
            CandidateStatus::Promoted,
        ] {
            assert_eq!(status_from_text(status.as_str()).unwrap(), status);
        }
        assert!(status_from_text("approved").is_err());
    }

    #[test]
    fn the_upsert_never_moves_a_row_to_an_earlier_stage() {
        let sql = put_statement();
        assert!(sql.contains("WHERE (CASE spot_memory_candidate.status"));
        assert!(sql.contains(") <= (CASE EXCLUDED.status"));
        assert!(sql.contains("WHEN 'promoted' THEN 3"));
        assert!(!sql.contains("{P}") && !sql.contains("{RANK"));
    }

    #[test]
    fn only_a_promoted_row_is_usable() {
        let row = StoredCandidate {
            id: "candidate.one".to_owned(),
            remedy: "learned.one".to_owned(),
            procedure: "procedure.one".to_owned(),
            status: CandidateStatus::Passed,
            ci_run: Some("run-1".to_owned()),
            rejection: None,
            confirmations: 3,
            symptoms: vec!["job.stuck".to_owned()],
            title: "retry".to_owned(),
            tier: Tier::T2,
            risk: "medium".to_owned(),
            affects: Vec::new(),
            preconditions: "the job is stuck".to_owned(),
            undo: ("reversible".to_owned(), "restores".to_owned()),
            updated_ms: 1_000,
        };
        assert!(!row.is_usable());
        assert!(
            StoredCandidate {
                status: CandidateStatus::Promoted,
                ..row
            }
            .is_usable()
        );
    }
}
