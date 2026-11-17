//! The occupancy service: claim, confirm, release, availability and the sweeper.
//!
//! # Who decides
//!
//! The exclusion constraint on `(unit, during)` is the only arbiter of a double booking. Nothing
//! here reads availability and then writes: a claim is one insert, and two writers that race for
//! overlapping dates meet the constraint, one commits and one gets `ExclusionViolation`
//! ([`OccupancyError::Conflict`]). The application-side checks (range shape, ownership, holder)
//! only refuse a request earlier and more clearly; removing them cannot cause a double booking.
//!
//! # Expiry
//!
//! A hold is `[start, end)` with an `expires_at`. The constraint cannot see the clock, so an
//! expired hold keeps blocking until its row is deleted. Two things delete it:
//!
//! - the **sweeper** ([`Occupancy::sweep_once`]): a batch of `state = tentative AND expires_at <=
//!   $now` rows, locked with `SKIP LOCKED`, deleted together with their `occupancy_expired` events
//!   in one transaction. It is housekeeping: correctness never depends on it.
//! - the **takeover**: every claim first deletes the expired holds on the same unit that overlap
//!   the asked range, then inserts, in one transaction at `RepeatableRead` that is rerun when it
//!   fails to serialise. Of two takers of one expired hold, one commits; the other meets the
//!   constraint against the winner's live record.
//!
//! # One clock
//!
//! `expires_at` (`now + ttl`) and `$now` in the takeover, the sweeper and the late-confirm rule all
//! come from the one injected [`Clock`] (ADR-087 section 6.2). Nodes of a tenant may differ by a
//! clock skew; the deployment states and monitors its maximum (ADR-065 measures node staleness).
//! A node whose clock is behind by the skew `s` sees a hold as live for up to `s` longer than
//! the node that wrote it expects: a hold is never shorter than promised, only longer by `s`.
//!
//! # Never inside a transaction
//!
//! No notification, payment or HTTP call runs inside a transaction. Events are captured in the
//! outbox and published after commit; the [`AuditSink`] is called after the transaction ended.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use spot_core::Clock;
use spot_storage_contract::{
    Catalog, CmpOp, Document, DocumentData, DocumentStore, ErrorKind, Expr, FieldRef, Fields,
    Isolation, Name, Outbox, Query, QueryExecutor, QueryPath, RangePoint, RangeTest, RangeValue,
    RecordId, RowLock, Selection, StorageBackend, StorageError, StorageResult, Transaction,
    TxOptions, Value, Wait,
};
use time::OffsetDateTime;

use crate::audit::{Action, AuditEntry, AuditSink, Outcome};
use crate::declaration::Cols;
use crate::error::OccupancyError;
use crate::events::{self, Facts};
use crate::model::{
    Busy, Caller, ClaimRequest, Hold, Holder, Limits, Released, State, SweepReport,
};
use crate::owner::{Owner, OwnerRoutes};

const ALIAS: &str = "o";
/// Most rows `availability` returns.
const AVAILABILITY_LIMIT: u32 = 1000;

/// How a claim stores the record.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A tentative hold with a deadline.
    Hold,
    /// A confirmed booking in one insert: never visible as tentative (race case 12).
    BookNow,
}

/// The occupancy service over one tenant's storage backend (ADR-34).
pub struct Occupancy<B: StorageBackend> {
    backend: Arc<B>,
    catalog: Arc<Catalog>,
    clock: Arc<dyn Clock>,
    cols: Cols,
    alias: Name,
    node: Owner,
    routes: Arc<dyn OwnerRoutes>,
    audit: Arc<dyn AuditSink>,
    limits: Limits,
}

impl<B: StorageBackend> Occupancy<B> {
    /// Builds the service for the node `node`.
    ///
    /// Refuses (with `InvalidSchema`) a catalog that does not declare the occupancy entity with
    /// its exclusion over `(unit, during)`, and limits that cannot work. A backend that does not
    /// advertise range exclusion is refused with `Unsupported`: the in-memory fake answers
    /// `Unsupported` for holds by capability (ADR-087 section 5.5, option B).
    pub fn new(
        backend: Arc<B>,
        catalog: Arc<Catalog>,
        clock: Arc<dyn Clock>,
        node: Owner,
        routes: Arc<dyn OwnerRoutes>,
        audit: Arc<dyn AuditSink>,
        limits: Limits,
    ) -> StorageResult<Self> {
        limits
            .validate()
            .map_err(|why| StorageError::new(ErrorKind::InvalidValue, why))?;
        let capabilities = backend.capabilities();
        if !capabilities.range_exclusion {
            return Err(StorageError::new(
                ErrorKind::Unsupported,
                format!(
                    "the `{}` backend does not offer range exclusion, which holds need",
                    capabilities.backend
                ),
            ));
        }
        let cols = Cols::check(&catalog)?;
        Ok(Self {
            backend,
            catalog,
            clock,
            cols,
            alias: Name::new(ALIAS)?,
            node,
            routes,
            audit,
            limits,
        })
    }

    /// The limits in force.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    // --- claim -------------------------------------------------------------------------------

    /// Takes the unit for the range as a tentative hold that expires after the request's
    /// time-to-live. Claim = insert: a conflict is `OccupancyError::Conflict`. An expired hold on
    /// the same unit and range is taken over in the same transaction.
    pub async fn hold(&self, request: &ClaimRequest) -> Result<Hold, OccupancyError> {
        let result = self.claim(Mode::Hold, request).await;
        self.report(
            Action::Hold,
            &request.holder.to_text(),
            Some(request.unit),
            result.as_ref().ok().map(|h| h.id),
            &result,
        );
        result
    }

    /// Takes the unit for the range as a confirmed booking in one insert (a "book now" flow): one
    /// insert and no update, so the row is never visible as tentative and the change feed reports
    /// only the confirmed version. Captures `occupancy_confirmed` only.
    pub async fn book_now(&self, request: &ClaimRequest) -> Result<Hold, OccupancyError> {
        let result = self.claim(Mode::BookNow, request).await;
        self.report(
            Action::BookNow,
            &request.holder.to_text(),
            Some(request.unit),
            result.as_ref().ok().map(|h| h.id),
            &result,
        );
        result
    }

    async fn claim(&self, mode: Mode, request: &ClaimRequest) -> Result<Hold, OccupancyError> {
        let ttl = self.check_claim(mode, request)?;
        self.ensure_owner(request.unit)?;
        let mut attempt = 0;
        loop {
            match self.claim_once(mode, request, ttl).await {
                Err(OccupancyError::Storage(e))
                    if e.kind.is_retryable() && attempt < self.limits.max_retries =>
                {
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt)).await;
                }
                other => return other,
            }
        }
    }

    /// Validates the request before any storage call and returns the time-to-live to use.
    fn check_claim(&self, mode: Mode, request: &ClaimRequest) -> Result<Duration, OccupancyError> {
        self.check_range(&request.during)?;
        let ttl = request.ttl.unwrap_or(self.limits.default_ttl);
        if mode == Mode::Hold && (ttl.is_zero() || ttl > self.limits.max_ttl) {
            return Err(OccupancyError::InvalidRange(format!(
                "the time-to-live must be above zero and at most {} seconds",
                self.limits.max_ttl.as_secs()
            )));
        }
        Ok(ttl)
    }

    /// Refuses a range that is not storable, not of the field's kind, or longer than the limit.
    fn check_range(&self, during: &RangeValue) -> Result<(), OccupancyError> {
        during.validate().map_err(OccupancyError::InvalidRange)?;
        if during.of() != Some(self.cols.range_of) {
            return Err(OccupancyError::InvalidRange(format!(
                "the range must be of {:?} values like the occupancy field",
                self.cols.range_of
            )));
        }
        if let Some((start, end)) = during.bounds()
            && let Some(days) = start.days_until(&end)
            && days > self.limits.max_span_days
        {
            return Err(OccupancyError::InvalidRange(format!(
                "the range is longer than {} days",
                self.limits.max_span_days
            )));
        }
        Ok(())
    }

    async fn claim_once(
        &self,
        mode: Mode,
        request: &ClaimRequest,
        ttl: Duration,
    ) -> Result<Hold, OccupancyError> {
        let now = self.clock.now_utc();
        let expires_at = match mode {
            Mode::Hold => Some(add(now, ttl)?),
            Mode::BookNow => None,
        };
        let mut tx = self
            .begin("occupancy_claim", Isolation::RepeatableRead)
            .await?;
        let out = self.claim_in(&mut tx, mode, request, now, expires_at).await;
        finish(tx, out).await
    }

    async fn claim_in(
        &self,
        tx: &mut B::Tx,
        mode: Mode,
        request: &ClaimRequest,
        now: OffsetDateTime,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<Hold, OccupancyError> {
        // The takeover: delete the expired holds this claim would otherwise collide with. The
        // delete is scoped to the exact unit, to tentative rows whose expiry has passed and to
        // the asked range, so it can only remove something nobody may claim any more.
        for id in self
            .expired_overlapping(tx, request.unit, &request.during, now)
            .await?
        {
            self.take_over(tx, id).await?;
        }

        let id = RecordId::new_v7();
        let state = match mode {
            Mode::Hold => State::Tentative,
            Mode::BookNow => State::Confirmed,
        };
        let data = self.data(
            id,
            request.unit,
            &request.during,
            state,
            &request.holder,
            expires_at,
        );
        // The claim: one insert. A conflict here is the constraint speaking.
        let version = tx.insert(&data).await?;
        let hold = Hold {
            id,
            unit: request.unit,
            during: request.during,
            state,
            holder: request.holder.clone(),
            expires_at,
            version,
        };
        let topic = match mode {
            Mode::Hold => events::TOPIC_HELD,
            Mode::BookNow => events::TOPIC_CONFIRMED,
        };
        self.enqueue(tx, topic, &hold).await?;
        Ok(hold)
    }

    /// The ids of tentative holds on `unit` that expired at or before `now` and overlap `during`,
    /// locked for update (waiting for a lock holder, so a concurrent taker's delete serialises).
    async fn expired_overlapping(
        &self,
        tx: &mut B::Tx,
        unit: RecordId,
        during: &RangeValue,
        now: OffsetDateTime,
    ) -> Result<Vec<RecordId>, OccupancyError> {
        let (start, end) = bounds_of(during)?;
        let mut q = self.query();
        q.select = vec![self.select_id()?];
        q.filter = Some(Expr::And(vec![
            self.field(&self.cols.unit).eq(Expr::value(unit)),
            self.field(&self.cols.state)
                .eq(Expr::value(State::Tentative.as_str())),
            self.field(&self.cols.expires_at)
                .cmp(CmpOp::Le, Expr::value(Value::DateTime(now))),
            Expr::Range(
                Box::new(self.field(&self.cols.during)),
                RangeTest::Overlaps {
                    start: start.to_value(),
                    end: end.to_value(),
                },
            ),
        ]));
        q.limit = Some(self.limits.sweep_batch);
        q.lock = RowLock::Update(Wait::Block);
        let rows = tx.query(&q).await?;
        ids(&rows.rows)
    }

    /// Deletes one expired hold and captures `occupancy_expired` in the same transaction.
    async fn take_over(&self, tx: &mut B::Tx, id: RecordId) -> Result<(), OccupancyError> {
        let Some(doc) = tx
            .load(&self.cols.entity, id, RowLock::Update(Wait::Block))
            .await?
        else {
            // Another taker deleted it first; its transaction committed, so the dates are free.
            return Ok(());
        };
        let hold = self.hold_of(&doc)?;
        match tx.delete(&self.cols.entity, id, doc.version).await {
            Ok(()) => {}
            // The row changed under the lock: rerun the whole claim on a fresh snapshot.
            Err(e) if matches!(e.kind, ErrorKind::StaleVersion | ErrorKind::NotFound) => {
                return Err(OccupancyError::Storage(StorageError::new(
                    ErrorKind::SerializationFailure,
                    "an expired hold changed while it was being taken over; the claim is rerun",
                )));
            }
            Err(e) => return Err(e.into()),
        }
        self.enqueue(tx, events::TOPIC_EXPIRED, &hold).await?;
        Ok(())
    }

    // --- confirm -----------------------------------------------------------------------------

    /// Confirms a hold: the record becomes a booking and `expires_at` is cleared.
    ///
    /// The confirm is a read-modify-write of the row it locked, guarded by `expires_at > $now`: a
    /// hold is a promise with a deadline, and a late confirm fails with `OccupancyError::Expired`
    /// (storage kind `StaleVersion`) instead of winning because the sweeper has not run yet
    /// (case 5). A hold the sweeper already deleted fails the same way (case 4). Confirming a
    /// record that is already confirmed returns it unchanged, with no second event.
    pub async fn confirm(&self, id: RecordId, caller: &Caller) -> Result<Hold, OccupancyError> {
        let result = self.confirm_with_retry(id, caller).await;
        let unit = result.as_ref().ok().map(|h| h.unit);
        self.report(Action::Confirm, &actor(caller), unit, Some(id), &result);
        result
    }

    async fn confirm_with_retry(
        &self,
        id: RecordId,
        caller: &Caller,
    ) -> Result<Hold, OccupancyError> {
        let mut attempt = 0;
        loop {
            let mut tx = self
                .begin("occupancy_confirm", Isolation::ReadCommitted)
                .await?;
            let out = self.confirm_in(&mut tx, id, caller).await;
            match finish(tx, out).await {
                Err(OccupancyError::Storage(e))
                    if e.kind.is_retryable() && attempt < self.limits.max_retries =>
                {
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt)).await;
                }
                other => return other,
            }
        }
    }

    async fn confirm_in(
        &self,
        tx: &mut B::Tx,
        id: RecordId,
        caller: &Caller,
    ) -> Result<Hold, OccupancyError> {
        // The row lock makes the sweeper (SKIP LOCKED) and a takeover wait or skip, and makes a
        // delete that committed first visible as "gone".
        let Some(doc) = tx
            .load(&self.cols.entity, id, RowLock::Update(Wait::Block))
            .await?
        else {
            return Err(gone(
                "the hold no longer exists: it expired and was swept, or was released",
            ));
        };
        let hold = self.hold_of(&doc)?;
        self.ensure_owner(hold.unit)?;
        check_holder(caller, &hold.holder)?;
        if hold.state == State::Confirmed {
            return Ok(hold);
        }
        let now = self.clock.now_utc();
        match hold.expires_at {
            Some(t) if t > now => {}
            _ => {
                return Err(gone(
                    "the hold passed its expiry; a confirm never extends a hold, claim again",
                ));
            }
        }
        let data = self.data(
            id,
            hold.unit,
            &hold.during,
            State::Confirmed,
            &hold.holder,
            None,
        );
        let version = match tx.update(&data, doc.version).await {
            Ok(v) => v,
            Err(e) if matches!(e.kind, ErrorKind::StaleVersion | ErrorKind::NotFound) => {
                return Err(OccupancyError::Expired(e));
            }
            Err(e) => return Err(e.into()),
        };
        let confirmed = Hold {
            state: State::Confirmed,
            expires_at: None,
            version,
            ..hold
        };
        self.enqueue(tx, events::TOPIC_CONFIRMED, &confirmed)
            .await?;
        Ok(confirmed)
    }

    // --- release -----------------------------------------------------------------------------

    /// Releases (cancels) a record: the row is deleted, because only its absence frees the dates,
    /// and `occupancy_cancelled` is captured in the same transaction. Releasing twice is a success
    /// the second time ([`Released::AlreadyGone`]).
    pub async fn release(&self, id: RecordId, caller: &Caller) -> Result<Released, OccupancyError> {
        let result = self.release_with_retry(id, caller).await;
        let unit = match &result {
            Ok(Released::Released(h)) => Some(h.unit),
            _ => None,
        };
        self.report(Action::Release, &actor(caller), unit, Some(id), &result);
        result
    }

    async fn release_with_retry(
        &self,
        id: RecordId,
        caller: &Caller,
    ) -> Result<Released, OccupancyError> {
        let mut attempt = 0;
        loop {
            let mut tx = self
                .begin("occupancy_release", Isolation::ReadCommitted)
                .await?;
            let out = self.release_in(&mut tx, id, caller).await;
            match finish(tx, out).await {
                Err(OccupancyError::Storage(e))
                    if e.kind.is_retryable() && attempt < self.limits.max_retries =>
                {
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt)).await;
                }
                other => return other,
            }
        }
    }

    async fn release_in(
        &self,
        tx: &mut B::Tx,
        id: RecordId,
        caller: &Caller,
    ) -> Result<Released, OccupancyError> {
        let Some(doc) = tx
            .load(&self.cols.entity, id, RowLock::Update(Wait::Block))
            .await?
        else {
            return Ok(Released::AlreadyGone);
        };
        let hold = self.hold_of(&doc)?;
        self.ensure_owner(hold.unit)?;
        check_holder(caller, &hold.holder)?;
        match tx.delete(&self.cols.entity, id, doc.version).await {
            Ok(()) => {}
            Err(e) if e.kind == ErrorKind::NotFound => return Ok(Released::AlreadyGone),
            Err(e) => return Err(e.into()),
        }
        self.enqueue(tx, events::TOPIC_CANCELLED, &hold).await?;
        Ok(Released::Released(hold))
    }

    // --- availability ------------------------------------------------------------------------

    /// The stretches of `unit` overlapping `during` that are not free: confirmed bookings and
    /// tentative holds that have not expired. A read on the primary, any node may answer it. The
    /// holder is not returned. Expired but unswept holds are left out: they block nothing, since a
    /// claim takes them over.
    pub async fn availability(
        &self,
        unit: RecordId,
        during: &RangeValue,
    ) -> Result<Vec<Busy>, OccupancyError> {
        self.check_range(during)?;
        let (start, end) = bounds_of(during)?;
        let now = self.clock.now_utc();
        // The query compiler cannot select a range column yet (ADR-087, WP-STO-02.T9): select the
        // ids, then load each record, which does read it.
        let mut q = self.query();
        q.select = vec![self.select_id()?];
        q.filter = Some(Expr::And(vec![
            self.field(&self.cols.unit).eq(Expr::value(unit)),
            Expr::Range(
                Box::new(self.field(&self.cols.during)),
                RangeTest::Overlaps {
                    start: start.to_value(),
                    end: end.to_value(),
                },
            ),
            Expr::Or(vec![
                self.field(&self.cols.state)
                    .eq(Expr::value(State::Confirmed.as_str())),
                self.field(&self.cols.expires_at)
                    .cmp(CmpOp::Gt, Expr::value(Value::DateTime(now))),
            ]),
        ]));
        q.limit = Some(AVAILABILITY_LIMIT);
        let mut tx = self
            .begin("occupancy_availability", Isolation::ReadCommitted)
            .await?;
        let out = self.busy_in(&mut tx, &q).await;
        finish(tx, out).await
    }

    async fn busy_in(&self, tx: &mut B::Tx, q: &Query) -> Result<Vec<Busy>, OccupancyError> {
        let rows = tx.query(q).await?;
        let mut busy = Vec::with_capacity(rows.rows.len());
        for id in ids(&rows.rows)? {
            let Some(doc) = tx.load(&self.cols.entity, id, RowLock::None).await? else {
                continue;
            };
            let hold = self.hold_of(&doc)?;
            busy.push(Busy {
                during: hold.during,
                state: hold.state,
            });
        }
        Ok(busy)
    }

    // --- the sweeper -------------------------------------------------------------------------

    /// One sweeper batch: locks up to `sweep_batch` expired tentative holds with `SKIP LOCKED` (a
    /// row a confirm or a takeover holds is left for the next pass), deletes them and captures an
    /// `occupancy_expired` event for each, all in one transaction. Holds on units this node does
    /// not own are left alone: another node sweeps those. Run it from a scheduled job under the
    /// coordinator lease (`WP-STO-14.T3`); a missed run never makes a booking wrong.
    pub async fn sweep_once(&self) -> Result<SweepReport, OccupancyError> {
        let result = self.sweep_with_retry().await;
        self.report(Action::Sweep, "platform", None, None, &result);
        result
    }

    /// Runs [`sweep_once`](Self::sweep_once) until a batch is not full, at most `max_batches`
    /// times, and returns the sum.
    pub async fn sweep_all(&self, max_batches: u32) -> Result<SweepReport, OccupancyError> {
        let mut total = SweepReport::default();
        for _ in 0..max_batches {
            let r = self.sweep_once().await?;
            total.expired += r.expired;
            total.not_owned += r.not_owned;
            total.more = r.more;
            if !r.more {
                break;
            }
        }
        Ok(total)
    }

    async fn sweep_with_retry(&self) -> Result<SweepReport, OccupancyError> {
        let mut attempt = 0;
        loop {
            let now = self.clock.now_utc();
            let mut tx = self
                .begin("occupancy_sweep", Isolation::ReadCommitted)
                .await?;
            let out = self.sweep_in(&mut tx, now).await;
            match finish(tx, out).await {
                Err(OccupancyError::Storage(e))
                    if e.kind.is_retryable() && attempt < self.limits.max_retries =>
                {
                    attempt += 1;
                    tokio::time::sleep(backoff(attempt)).await;
                }
                other => return other,
            }
        }
    }

    async fn sweep_in(
        &self,
        tx: &mut B::Tx,
        now: OffsetDateTime,
    ) -> Result<SweepReport, OccupancyError> {
        let mut q = self.query();
        q.select = vec![self.select_id()?];
        q.filter = Some(Expr::And(vec![
            self.field(&self.cols.state)
                .eq(Expr::value(State::Tentative.as_str())),
            self.field(&self.cols.expires_at)
                .cmp(CmpOp::Le, Expr::value(Value::DateTime(now))),
        ]));
        q.limit = Some(self.limits.sweep_batch);
        q.lock = RowLock::Update(Wait::SkipLocked);
        let rows = tx.query(&q).await?;
        let found = ids(&rows.rows)?;
        let mut report = SweepReport {
            more: found.len() as u64 >= u64::from(self.limits.sweep_batch),
            ..SweepReport::default()
        };
        for id in found {
            let Some(doc) = tx
                .load(&self.cols.entity, id, RowLock::Update(Wait::Block))
                .await?
            else {
                continue;
            };
            let hold = self.hold_of(&doc)?;
            if self.routes.owner_of(hold.unit) != self.node {
                report.not_owned += 1;
                continue;
            }
            // Re-check under the lock: the row may have been confirmed since the query read it.
            if hold.state != State::Tentative || hold.expires_at.is_none_or(|t| t > now) {
                continue;
            }
            tx.delete(&self.cols.entity, id, doc.version).await?;
            self.enqueue(tx, events::TOPIC_EXPIRED, &hold).await?;
            report.expired += 1;
        }
        Ok(report)
    }

    // --- shared helpers ----------------------------------------------------------------------

    async fn begin(&self, label: &str, isolation: Isolation) -> Result<B::Tx, OccupancyError> {
        let options = TxOptions::new(Name::new(label)?, isolation);
        Ok(self.backend.begin(self.catalog.clone(), options).await?)
    }

    fn ensure_owner(&self, unit: RecordId) -> Result<(), OccupancyError> {
        let owner = self.routes.owner_of(unit);
        if owner == self.node {
            Ok(())
        } else {
            Err(OccupancyError::NotOwner { unit, owner })
        }
    }

    fn query(&self) -> Query {
        Query::from(&self.cols.entity, &self.alias, QueryPath::Transactional)
    }

    fn field(&self, name: &Name) -> Expr {
        Expr::Field(FieldRef {
            alias: self.alias.clone(),
            field: name.clone(),
        })
    }

    fn select_id(&self) -> Result<Selection, OccupancyError> {
        let id = Name::new("id")?;
        Ok(Selection {
            expr: self.field(&id),
            alias: id,
        })
    }

    fn data(
        &self,
        id: RecordId,
        unit: RecordId,
        during: &RangeValue,
        state: State,
        holder: &Holder,
        expires_at: Option<OffsetDateTime>,
    ) -> DocumentData {
        let mut fields = Fields::new();
        fields.insert(self.cols.unit.clone(), Value::Id(unit));
        fields.insert(self.cols.during.clone(), Value::Range(*during));
        fields.insert(
            self.cols.state.clone(),
            Value::Text(state.as_str().to_owned()),
        );
        fields.insert(self.cols.holder.clone(), Value::Text(holder.to_text()));
        fields.insert(
            self.cols.expires_at.clone(),
            expires_at.map_or(Value::Null, Value::DateTime),
        );
        DocumentData {
            entity: self.cols.entity.clone(),
            id,
            fields,
            children: BTreeMap::new(),
        }
    }

    fn hold_of(&self, doc: &Document) -> Result<Hold, OccupancyError> {
        let f = &doc.data.fields;
        let (Some(Value::Id(unit)), Some(Value::Range(during))) =
            (f.get(&self.cols.unit), f.get(&self.cols.during))
        else {
            return Err(internal("an occupancy record has no unit or range"));
        };
        let Some(Value::Text(state)) = f.get(&self.cols.state) else {
            return Err(internal("an occupancy record has no state"));
        };
        let Some(Value::Text(holder)) = f.get(&self.cols.holder) else {
            return Err(internal("an occupancy record has no holder"));
        };
        let expires_at = match f.get(&self.cols.expires_at) {
            Some(Value::DateTime(t)) => Some(*t),
            Some(Value::Null) | None => None,
            Some(_) => return Err(internal("an occupancy record has a bad expiry")),
        };
        Ok(Hold {
            id: doc.data.id,
            unit: *unit,
            during: *during,
            state: State::parse(state)
                .ok_or_else(|| internal("an occupancy record has an unknown state"))?,
            holder: Holder::parse(holder)
                .map_err(|_| internal("an occupancy record has a bad holder"))?,
            expires_at,
            version: doc.version,
        })
    }

    async fn enqueue(
        &self,
        tx: &mut B::Tx,
        topic: &str,
        hold: &Hold,
    ) -> Result<(), OccupancyError> {
        let event = events::event(
            &self.cols.entity,
            topic,
            &Facts {
                record: hold.id,
                unit: hold.unit,
                during: &hold.during,
                state: hold.state,
                holder: &hold.holder,
                expires_at: hold.expires_at,
                version: hold.version,
            },
        )?;
        tx.enqueue(&event).await?;
        Ok(())
    }

    /// Reports a finished operation to the audit sink, success or failure, outside any
    /// transaction.
    fn report<T>(
        &self,
        action: Action,
        actor: &str,
        unit: Option<RecordId>,
        record: Option<RecordId>,
        result: &Result<T, OccupancyError>,
    ) {
        let outcome = match result {
            Ok(_) => Outcome::Done,
            Err(e) => Outcome::Refused(e.code()),
        };
        self.audit.record(AuditEntry {
            at: self.clock.now_utc(),
            action,
            actor: actor.to_owned(),
            unit,
            record,
            outcome,
        });
    }
}

/// Commits on success and rolls back on failure; a commit error is returned as is.
async fn finish<T: Send, Tx: Transaction>(
    tx: Tx,
    out: Result<T, OccupancyError>,
) -> Result<T, OccupancyError> {
    match out {
        Ok(v) => {
            tx.commit().await?;
            Ok(v)
        }
        Err(e) => {
            // The original error is the one worth returning; a failed rollback changes nothing
            // (dropping the transaction rolls it back anyway).
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

fn check_holder(caller: &Caller, holder: &Holder) -> Result<(), OccupancyError> {
    match caller {
        Caller::Operator => Ok(()),
        Caller::Holder(h) if h == holder => Ok(()),
        Caller::Holder(_) => Err(OccupancyError::NotHolder),
    }
}

fn actor(caller: &Caller) -> String {
    match caller {
        Caller::Holder(h) => h.to_text(),
        Caller::Operator => "operator".to_owned(),
    }
}

/// The error for a confirm of a hold that is gone or past its deadline: kind `StaleVersion`.
fn gone(why: &str) -> OccupancyError {
    OccupancyError::Expired(StorageError::new(ErrorKind::StaleVersion, why))
}

fn internal(why: &str) -> OccupancyError {
    OccupancyError::Storage(StorageError::new(ErrorKind::Internal, why))
}

fn bounds_of(during: &RangeValue) -> Result<(RangePoint, RangePoint), OccupancyError> {
    during
        .bounds()
        .ok_or_else(|| OccupancyError::InvalidRange("the range has no finite bounds".into()))
}

fn ids(rows: &[Vec<Value>]) -> Result<Vec<RecordId>, OccupancyError> {
    rows.iter()
        .map(|r| match r.first() {
            Some(Value::Id(id)) => Ok(*id),
            _ => Err(internal("a query returned a row without an id")),
        })
        .collect()
}

/// `now + ttl`, refusing an overflow instead of panicking.
fn add(now: OffsetDateTime, ttl: Duration) -> Result<OffsetDateTime, OccupancyError> {
    time::Duration::try_from(ttl)
        .ok()
        .and_then(|d| now.checked_add(d))
        .ok_or_else(|| OccupancyError::InvalidRange("the expiry is out of range".into()))
}

/// 5 ms, doubling per attempt, at most 200 ms.
fn backoff(attempt: u32) -> Duration {
    let ms = 5u64.saturating_mul(1u64 << attempt.min(6));
    Duration::from_millis(ms.min(200))
}
