//! The effects host of one site: the request record file, undo and rewind with compensation, and
//! the hold, joined over the site's ledger.
//!
//! # The record file
//!
//! `spot-bpm-effects` keeps a request's file in memory and has no wire form for it. This host
//! makes the **log** durable instead: every receipt (and every visit of a node that wrote
//! nothing) is appended to an append-only table, keyed by `(request, node, attempt, operation)`,
//! **before** it is recorded in memory. After a restart the file is rebuilt by replaying the log
//! in order, so the entries, their numbers and the plan of an undo come out the same.
//!
//! # Undo
//!
//! An undo is authorised (the person who started the request, or a supervisor), claimed in the
//! database (one runner per request, across nodes), then planned and executed on a blocking
//! thread: each reversal runs as the acting principal through [`DurableReversal`], which makes it
//! exactly-once per receipt. Its end is one of:
//!
//! - **done**: everything after the target is reversed; held effects were discarded first, so
//!   nothing that had not left ever does;
//! - **needs_attention**: a reversal was refused (a dependant, a closed period, an irreversible
//!   effect, a partner that refuses). The sequence stops, the request stays in the stable state
//!   `needs_attention`, a supervisor notice is raised and **nothing is skipped or forced**;
//! - **interrupted**: a port was unavailable; the lease holder resumes it on a later tick.
//!
//! Who may start an undo of somebody else's request is not decided by the file: this host asks the
//! [`Supervisors`] seam, and a source that cannot answer denies. A refusal is the same answer as
//! an unknown request.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_app_api::BoxFuture;
use spot_app_api::OperationReceipt;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_commit::EndCause;
use spot_bpm_effects::{
    AlternativeId, Choice, ChoiceOutcome, Declarations, EffectsError, GraphDependants, NewEntry,
    PartnerPort, RecordRef, RequestEffects, ReversalPort, UndoContext, UndoOutcome,
};
use spot_bpm_engine::CaseId;
use spot_core::{Clock, SiteId};
use spot_dag::NodeId;
use tokio::runtime::Handle;

use super::error::EffError;
use super::hold::{HoldHost, HoldParts};
use super::ledger::{EffLedger, LogRow, Resumable, UndoRow};
use super::reversal::DurableReversal;
use super::settings::Settings;

/// The most bytes one stored receipt may take.
const MAX_RECEIPT_BYTES: usize = 64 * 1024;

/// Who may supervise a request that is not their own.
pub trait Supervisors: Send + Sync {
    /// Whether `actor` (a typed principal reference) may supervise requests. `None`: the source
    /// could not answer, which denies.
    fn may_supervise<'a>(&'a self, actor: &'a str) -> BoxFuture<'a, Option<bool>>;
}

/// What a supervisor is told when an undo needs a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionNotice {
    /// The request.
    pub request: String,
    /// The entry whose reversal stopped the sequence.
    pub entry: u64,
    /// The reversal's index within the entry.
    pub index: u32,
    /// The alternatives the record type offers, as tokens.
    pub alternatives: Vec<String>,
}

/// Raises the supervisor notice. Best effort: a sink that fails is counted and never changes the
/// state of the request, which is already `needs_attention` when the notice is raised.
pub trait EffectNotice: Send + Sync {
    /// Tells a supervisor that `notice.request` needs attention. Carries ids and tokens only.
    fn needs_attention<'a>(&'a self, notice: &'a AttentionNotice) -> BoxFuture<'a, ()>;
}

/// What the host is built from.
pub struct Wiring {
    /// The site's name (also the tenant key of the ledger).
    pub site: String,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
    /// The site's pool.
    pub pool: Pool,
    /// The settings.
    pub settings: Settings,
    /// The runtime the blocking undo thread reaches back to.
    pub runtime: Handle,
    /// The effect declarations of the record types.
    pub declarations: Arc<dyn Declarations>,
    /// The business graph: which records depend on a record.
    pub graph: Arc<dyn GraphDependants>,
    /// The lifecycle port a reversal goes through (the durable once-per-key wrapper is added
    /// here).
    pub reversal: Arc<dyn ReversalPort>,
    /// The partner (conversations) port.
    pub partner: Arc<dyn PartnerPort>,
    /// Who may supervise.
    pub supervisors: Arc<dyn Supervisors>,
    /// The supervisor notice.
    pub notice: Arc<dyn EffectNotice>,
    /// The hold.
    pub hold: HoldParts,
}

/// One receipt to record in a request's file.
pub struct Recorded<'a> {
    /// The request.
    pub request: &'a str,
    /// The typed principal reference of the person who started it (kept the first time).
    pub requester: &'a str,
    /// The node that ran the operation.
    pub node: &'a str,
    /// The node's attempt.
    pub attempt: u32,
    /// Which operation of the attempt this is (0 for the first).
    pub op_index: u32,
    /// What the operation did, as the engine reported it: references, counts and digests.
    pub receipt: OperationReceipt,
    /// The records this one depends on.
    pub links: BTreeSet<RecordRef>,
    /// The instant.
    pub now: OffsetDateTime,
}

/// Where an undo ended up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoReport {
    /// Everything after the target is reversed.
    Done {
        /// Reversals that ran.
        reversed: usize,
        /// Reversals that needed no call.
        skipped: usize,
        /// Held effects discarded before anything was compensated.
        discarded: u64,
    },
    /// The sequence stopped and a person decides.
    NeedsAttention {
        /// The entry whose reversal stopped it.
        entry: u64,
        /// The reversal's index within the entry.
        index: u32,
        /// What the record type offers instead, as tokens.
        alternatives: Vec<String>,
    },
}

/// The effects host of one site.
pub struct EffectsHost {
    site: SiteId,
    clock: Arc<dyn Clock>,
    ledger: EffLedger,
    effects: Arc<RequestEffects>,
    reversal: Arc<DurableReversal>,
    partner: Arc<dyn PartnerPort>,
    graph: Arc<dyn GraphDependants>,
    supervisors: Arc<dyn Supervisors>,
    notice: Arc<dyn EffectNotice>,
    hold: HoldHost,
    loaded: tokio::sync::Mutex<BTreeSet<String>>,
    stale: Duration,
    max_resume: u32,
}

impl Wiring {
    /// Joins the parts into one effects host.
    ///
    /// # Errors
    /// A setting that does not validate (the key is named, never its value), or a site name that
    /// is not valid.
    pub fn build(self) -> Result<EffectsHost, String> {
        self.settings.validate().map_err(str::to_owned)?;
        let site = SiteId::new(self.site.as_str()).map_err(|_| "site".to_owned())?;
        let ledger = EffLedger::new(self.pool, &self.site);
        let reversal = Arc::new(DurableReversal::new(
            self.reversal,
            ledger.clone(),
            self.runtime,
        ));
        Ok(EffectsHost {
            hold: HoldHost::new(site.clone(), self.hold),
            site,
            clock: self.clock,
            ledger,
            effects: Arc::new(RequestEffects::new(self.declarations)),
            reversal,
            partner: self.partner,
            graph: self.graph,
            supervisors: self.supervisors,
            notice: self.notice,
            loaded: tokio::sync::Mutex::new(BTreeSet::new()),
            stale: Duration::from_secs(self.settings.stale_secs),
            max_resume: self.settings.max_resume_per_tick,
        })
    }
}

impl EffectsHost {
    /// The site this host serves.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The ledger (tables, audit rows).
    #[must_use]
    pub fn ledger(&self) -> &EffLedger {
        &self.ledger
    }

    /// The hold and its commit points.
    #[must_use]
    pub fn hold(&self) -> &HoldHost {
        &self.hold
    }

    /// The in-memory request files (rebuilt from the log on first use).
    #[must_use]
    pub fn effects(&self) -> &Arc<RequestEffects> {
        &self.effects
    }

    /// Creates the tables if absent and reads the hold state.
    ///
    /// # Errors
    /// The database refused.
    pub async fn prepare(&self) -> Result<(), EffError> {
        self.ledger.prepare().await?;
        self.hold.load(micros(self.clock.now_utc())).await;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // The record file.
    // -----------------------------------------------------------------------------------------

    fn case_of(request: &str) -> Result<CaseId, EffError> {
        CaseId::new(request).map_err(|_| EffError::ENTRY_INVALID)
    }

    /// Rebuilds the in-memory file of `request` from the log, once. A request with no log has
    /// nothing to rebuild.
    async fn ensure_loaded(&self, request: &str) -> Result<CaseId, EffError> {
        let case = Self::case_of(request)?;
        let mut loaded = self.loaded.lock().await;
        if loaded.contains(request) {
            return Ok(case);
        }
        for row in self.ledger.log(request).await? {
            self.replay(&case, &row)?;
        }
        loaded.insert(request.to_owned());
        Ok(case)
    }

    fn replay(&self, case: &CaseId, row: &LogRow) -> Result<(), EffError> {
        let node = NodeId::new(&row.node).map_err(|_| EffError::ENTRY_INVALID)?;
        let at = from_micros(row.at_us);
        if row.kind == "visit" {
            self.effects.note_visit(case, &node)?;
            return Ok(());
        }
        let receipt: OperationReceipt =
            serde_json::from_str(&row.body).map_err(|_| EffError::ENTRY_INVALID)?;
        let pairs: Vec<(String, String)> =
            serde_json::from_str(&row.links).map_err(|_| EffError::ENTRY_INVALID)?;
        let links = pairs
            .into_iter()
            .map(|(doctype, name)| RecordRef::new(doctype, name))
            .collect();
        self.effects.record(
            case,
            NewEntry {
                node,
                receipt,
                links,
                at,
            },
        )?;
        Ok(())
    }

    /// Records a receipt in the request's file: appended to the log first, then to memory. A
    /// repeat of the same `(request, node, attempt, op_index)` changes nothing and returns
    /// `false`.
    ///
    /// # Errors
    /// The receipt or a name is not valid, the request is held by an undo, or the database
    /// refused.
    pub async fn record(&self, new: Recorded<'_>) -> Result<bool, EffError> {
        let node = NodeId::new(new.node).map_err(|_| EffError::ENTRY_INVALID)?;
        let case = self.ensure_loaded(new.request).await?;
        let body = serde_json::to_string(&new.receipt).map_err(|_| EffError::ENTRY_INVALID)?;
        if body.len() > MAX_RECEIPT_BYTES {
            return Err(EffError::ENTRY_INVALID);
        }
        let pairs: Vec<(&str, &str)> = new
            .links
            .iter()
            .map(|r| (r.doctype.as_str(), r.name.as_str()))
            .collect();
        let links = serde_json::to_string(&pairs).map_err(|_| EffError::ENTRY_INVALID)?;
        let at = micros(new.now);
        self.ledger
            .open_request(new.request, new.requester, at)
            .await?;
        let attempt = i32::try_from(new.attempt).map_err(|_| EffError::ENTRY_INVALID)?;
        let op_index = i32::try_from(new.op_index).map_err(|_| EffError::ENTRY_INVALID)?;
        let added = self
            .ledger
            .append(
                new.request,
                "receipt",
                new.node,
                attempt,
                op_index,
                &body,
                &links,
                at,
            )
            .await?;
        if !added {
            return Ok(false);
        }
        self.effects.record(
            &case,
            NewEntry {
                node,
                receipt: new.receipt,
                links: new.links,
                at: new.now,
            },
        )?;
        Ok(true)
    }

    /// Notes that a node that wrote nothing has finished, so an undo can go back to it.
    ///
    /// # Errors
    /// A name is not valid, the request is held by an undo, or the database refused.
    pub async fn visit(
        &self,
        request: &str,
        requester: &str,
        node: &str,
        attempt: u32,
        now: OffsetDateTime,
    ) -> Result<bool, EffError> {
        let id = NodeId::new(node).map_err(|_| EffError::ENTRY_INVALID)?;
        let case = self.ensure_loaded(request).await?;
        let at = micros(now);
        self.ledger.open_request(request, requester, at).await?;
        let attempt = i32::try_from(attempt).map_err(|_| EffError::ENTRY_INVALID)?;
        if !self
            .ledger
            .append(request, "visit", node, attempt, 0, "", "", at)
            .await?
        {
            return Ok(false);
        }
        self.effects.note_visit(&case, &id)?;
        Ok(true)
    }

    // -----------------------------------------------------------------------------------------
    // Authorisation.
    // -----------------------------------------------------------------------------------------

    /// The person who started the request, or the supervisor, may act; everyone else (and every
    /// request nobody started) gets the answer an unknown request gets. A refusal is audited.
    async fn authorize(&self, actor: &str, request: &str, action: &str) -> Result<(), EffError> {
        let now = micros(self.clock.now_utc());
        let requester = match self.ledger.requester_of(request).await {
            Ok(requester) => requester,
            Err(_) => {
                self.ledger
                    .audit(actor, request, action, "refused", "ledger_unavailable", now)
                    .await;
                return Err(EffError::UNAVAILABLE);
            }
        };
        let Some(requester) = requester else {
            self.ledger
                .audit(actor, request, action, "refused", "request_unknown", now)
                .await;
            return Err(EffError::REQUEST_UNKNOWN);
        };
        if requester == actor {
            return Ok(());
        }
        // An erroring source denies: only an explicit yes lets a supervisor through.
        if self.supervisors.may_supervise(actor).await == Some(true) {
            return Ok(());
        }
        self.ledger
            .audit(actor, request, action, "refused", "not_supervisor", now)
            .await;
        Err(EffError::REQUEST_UNKNOWN)
    }

    /// The stored undo state of `request`, for the person who started it or a supervisor.
    ///
    /// # Errors
    /// The caller may not see the request, or the database refused.
    pub async fn state(&self, actor: &str, request: &str) -> Result<Option<UndoRow>, EffError> {
        self.authorize(actor, request, "state").await?;
        Ok(self.ledger.undo_row(request).await?)
    }

    // -----------------------------------------------------------------------------------------
    // Undo.
    // -----------------------------------------------------------------------------------------

    /// Cancels the request (`to = None`) or takes it back to node `to`: compensates everything
    /// after it, in reverse dependency order, as `actor`.
    ///
    /// Cancelling first discards the effects the request still holds (they never left), then
    /// compensates the ones that did. A reversal that cannot be carried out stops the sequence in
    /// `needs_attention`; one that could not be reached leaves it resumable.
    ///
    /// # Errors
    /// The caller may not act on the request, another runner holds the undo
    /// (`bpm_effects.request_locked`), a dependency is down (`bpm_effects.port_unavailable`), or
    /// the business graph cannot answer (`bpm_effects.graph_unavailable`: the undo is denied
    /// rather than guessed).
    pub async fn undo(
        &self,
        actor: &str,
        request: &str,
        to: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<UndoReport, EffError> {
        self.authorize(actor, request, "undo").await?;
        let to_node = to
            .map(|name| NodeId::new(name).map_err(|_| EffError::ENTRY_INVALID))
            .transpose()?;
        let case = self.ensure_loaded(request).await?;
        let at = micros(now);
        if !self
            .ledger
            .claim_undo(request, actor, to, at, self.stale_before(now))
            .await?
        {
            self.ledger
                .audit(actor, request, "undo", "refused", "request_locked", at)
                .await;
            return Err(EffError::REQUEST_LOCKED);
        }
        let discarded = if to_node.is_none() {
            // The held effects never left: discard them before compensating the ones that did.
            match self.hold.discard(request, EndCause::Cancelled, at).await {
                Ok(report) => report.discarded,
                Err(error) => {
                    self.ledger
                        .set_undo(request, "interrupted", error.code(), at)
                        .await?;
                    return Err(error);
                }
            }
        } else {
            0
        };
        self.drive(actor, request, &case, to_node, discarded, now)
            .await
    }

    /// Runs (or resumes) the claimed undo and records where it ended.
    async fn drive(
        &self,
        actor: &str,
        request: &str,
        case: &CaseId,
        to: Option<NodeId>,
        discarded: u64,
        now: OffsetDateTime,
    ) -> Result<UndoReport, EffError> {
        let at = micros(now);
        let outcome = self.execute(case.clone(), to, actor.to_owned(), now).await;
        match outcome {
            Ok(UndoOutcome::Done { reversed, skipped }) => {
                self.ledger.set_undo(request, "done", "", at).await?;
                self.ledger
                    .audit(actor, request, "undo", "done", "completed", at)
                    .await;
                Ok(UndoReport::Done {
                    reversed,
                    skipped,
                    discarded,
                })
            }
            Ok(UndoOutcome::NeedsDecision(decision)) => {
                let alternatives: Vec<String> = decision
                    .alternatives
                    .iter()
                    .map(|a| a.id.as_str().to_owned())
                    .collect();
                self.ledger
                    .set_undo(request, "needs_attention", "reversal_refused", at)
                    .await?;
                self.ledger
                    .audit(
                        actor,
                        request,
                        "undo",
                        "needs_attention",
                        "reversal_refused",
                        at,
                    )
                    .await;
                let notice = AttentionNotice {
                    request: request.to_owned(),
                    entry: decision.entry,
                    index: decision.index,
                    alternatives: alternatives.clone(),
                };
                self.notice.needs_attention(&notice).await;
                Ok(UndoReport::NeedsAttention {
                    entry: decision.entry,
                    index: decision.index,
                    alternatives,
                })
            }
            Err(error) => {
                let code = error.code();
                // A port that could not be reached leaves the undo resumable; any other failure
                // needs a person.
                let state = if matches!(
                    error,
                    EffectsError::PortUnavailable { .. } | EffectsError::CancellationFailed { .. }
                ) {
                    "interrupted"
                } else {
                    "needs_attention"
                };
                self.ledger.set_undo(request, state, code, at).await?;
                self.ledger
                    .audit(actor, request, "undo", state, code, at)
                    .await;
                Err(EffError::from(error))
            }
        }
    }

    /// Plans (or resumes the plan of) the undo and executes it on a blocking thread.
    async fn execute(
        &self,
        case: CaseId,
        to: Option<NodeId>,
        principal: String,
        now: OffsetDateTime,
    ) -> Result<UndoOutcome, EffectsError> {
        let effects = Arc::clone(&self.effects);
        let graph = Arc::clone(&self.graph);
        let port: Arc<dyn ReversalPort> = Arc::clone(&self.reversal) as Arc<dyn ReversalPort>;
        let partner = Arc::clone(&self.partner);
        let joined = tokio::task::spawn_blocking(move || {
            let locked = effects.file(&case)?.is_locked();
            let plan = if locked {
                effects.resume_plan(&case, &*graph)?
            } else {
                effects.plan_undo(&case, to.as_ref(), &*graph)?
            };
            let ctx = UndoContext {
                principal: &principal,
                now,
                port: &*port,
                partner: &*partner,
            };
            effects.execute_undo(&plan, &ctx)
        })
        .await;
        joined.unwrap_or_else(|_| {
            Err(EffectsError::PortUnavailable {
                request: String::new(),
                code: "undo_thread_failed".to_owned(),
            })
        })
    }

    fn stale_before(&self, now: OffsetDateTime) -> i64 {
        let stale = i64::try_from(self.stale.as_micros()).unwrap_or(i64::MAX);
        micros(now).saturating_sub(stale)
    }

    /// Records a person's decision at an open `needs_attention`: carry out one of the offered
    /// alternatives (the undo then resumes), or abandon.
    ///
    /// # Errors
    /// The caller may not act on the request, there is no open decision (after a restart, call
    /// [`undo`](Self::undo) again: it stops at the same decision), or the undo failed on resume.
    pub async fn choose(
        &self,
        actor: &str,
        request: &str,
        alternative: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<Option<UndoReport>, EffError> {
        self.authorize(actor, request, "choose").await?;
        let case = self.ensure_loaded(request).await?;
        let choice = match alternative {
            Some(token) => {
                Choice::Alternative(AlternativeId::new(token).ok_or(EffError::ENTRY_INVALID)?)
            }
            None => Choice::Abandon,
        };
        let at = micros(now);
        let effects = Arc::clone(&self.effects);
        let port: Arc<dyn ReversalPort> = Arc::clone(&self.reversal) as Arc<dyn ReversalPort>;
        let partner = Arc::clone(&self.partner);
        let (principal, case_for_run) = (actor.to_owned(), case.clone());
        let chosen = tokio::task::spawn_blocking(move || {
            let ctx = UndoContext {
                principal: &principal,
                now,
                port: &*port,
                partner: &*partner,
            };
            effects.choose(&case_for_run, &choice, &ctx)
        })
        .await
        .unwrap_or_else(|_| {
            Err(EffectsError::PortUnavailable {
                request: request.to_owned(),
                code: "undo_thread_failed".to_owned(),
            })
        })?;
        self.ledger
            .audit(actor, request, "choose", "recorded", "decision", at)
            .await;
        match chosen {
            ChoiceOutcome::Abandoned => {
                self.ledger.set_undo(request, "abandoned", "", at).await?;
                Ok(None)
            }
            ChoiceOutcome::Applied => {
                let to = self
                    .ledger
                    .undo_row(request)
                    .await?
                    .and_then(|row| row.to_node)
                    .map(|node| NodeId::new(&node).map_err(|_| EffError::ENTRY_INVALID))
                    .transpose()?;
                if !self
                    .ledger
                    .claim_undo(request, actor, None, at, self.stale_before(now))
                    .await?
                {
                    return Err(EffError::REQUEST_LOCKED);
                }
                Ok(Some(self.drive(actor, request, &case, to, 0, now).await?))
            }
            ChoiceOutcome::NeedsDecision(decision) => Ok(Some(UndoReport::NeedsAttention {
                entry: decision.entry,
                index: decision.index,
                alternatives: decision
                    .alternatives
                    .iter()
                    .map(|a| a.id.as_str().to_owned())
                    .collect(),
            })),
        }
    }

    /// Resumes the undos that were interrupted (a port was down, the node stopped): each runs as
    /// the person who started it and finishes exactly once per receipt, because every reversal
    /// the journal holds is returned, not run again. Returns how many were taken up.
    pub async fn resume_pending(&self) -> usize {
        let now = self.clock.now_utc();
        let waiting: Vec<Resumable> = match self
            .ledger
            .resumable(self.stale_before(now), self.max_resume)
            .await
        {
            Ok(waiting) => waiting,
            Err(_) => {
                tracing::warn!("the undos to resume could not be listed");
                return 0;
            }
        };
        let mut resumed = 0;
        for undo in waiting {
            let at = micros(now);
            let claimed = self
                .ledger
                .claim_undo(
                    &undo.request,
                    &undo.started_by,
                    None,
                    at,
                    self.stale_before(now),
                )
                .await
                .unwrap_or(false);
            if !claimed {
                continue;
            }
            resumed += 1;
            let case = match self.ensure_loaded(&undo.request).await {
                Ok(case) => case,
                Err(error) => {
                    let _ = self
                        .ledger
                        .set_undo(&undo.request, "interrupted", error.code(), at)
                        .await;
                    continue;
                }
            };
            let to = undo
                .to_node
                .as_deref()
                .and_then(|node| NodeId::new(node).ok());
            let outcome = self
                .drive(&undo.started_by, &undo.request, &case, to, 0, now)
                .await;
            if let Err(error) = outcome {
                tracing::warn!(code = error.code(), "a resumed undo did not finish");
            }
        }
        resumed
    }
}

/// Unix microseconds of an instant.
#[must_use]
pub fn micros(at: OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
}

fn from_micros(us: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(us) * 1_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}
