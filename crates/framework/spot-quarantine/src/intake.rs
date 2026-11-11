//! The intake adapter: apply a received record, and on refusal quarantine it without blocking the
//! stream.
//!
//! [`Intake`] wraps [`spot_replica_apply::apply`] (WP-EDG-06.T1) and adds the queue's rules
//! (ADR-062 G3 and section 6, ADR-064 section 7):
//!
//! - a record that is **refused** is stored as a [`QuarantineItem`] and the stream **checkpoint
//!   advances past it**, so later records of other documents keep flowing (the "one bad document
//!   must never stall a branch" acceptance);
//! - a **later version of a document already quarantined** is parked behind it, in order, rather
//!   than jumping ahead ([`Dependents`]);
//! - a record the queue already holds is not stored twice;
//! - the automatic retry schedule ([`RetryPolicy`]) re-applies held items with backoff and a cap,
//!   after which the item stays `Held` and raises a [`Condition`] (ADR-067).
//!
//! Policy lives behind seams so the later sync ADRs change it without touching this adapter: the
//! catalog snapshot an upgrade swaps ([`CatalogSource`], ADR-066), what waits behind what
//! ([`Dependents`], ADR-064), and the retry schedule ([`RetryPolicy`]). Time is always an argument.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use spot_permissions::Principal;
use spot_replica_apply::{
    ApplyCtx, ApplyOutcome, NoticeSink, OriginFields, OwnerCheck, ReceivedRecord, ReplicaAudit,
};
use spot_storage_contract::{Catalog, DocumentData, Fields, StorageBackend, TxOptions};
use spot_sync_protocol::{Checkpoint, SourceId};

use crate::admin::{
    Access, CloseAuthority, ConditionView, ItemView, OperationResult, OperationView,
    QuarantineGuard, authorize_close,
};
use crate::audit::{AuditAction, AuditEntry, QuarantineAudit};
use crate::codes;
use crate::error::QuarantineError;
use crate::item::{
    Closure, ClosureOutcome, ClosureRecord, ItemFilter, ItemId, MAX_DETAIL_BYTES,
    MAX_PAYLOAD_BYTES, MAX_REASON_CODE_BYTES, MAX_SOURCE_BYTES, NewItem, QuarantineItem, RecordRef,
    State,
};
use crate::reason;
use crate::retry::{Condition, RetryPolicy};
use crate::store::QuarantineStore;

/// Where the apply path's catalog snapshot comes from.
///
/// A metadata upgrade swaps the snapshot (ADR-066): the intake reads the current one for each
/// apply, so an item quarantined for an unknown entity applies after the entity's metadata is
/// installed and `retry_all` runs.
pub trait CatalogSource: Send + Sync {
    /// The catalog snapshot to apply against.
    fn current(&self) -> Arc<Catalog>;
}

/// A [`CatalogSource`] that never changes.
pub struct FixedCatalog(Arc<Catalog>);

impl FixedCatalog {
    /// A source over one catalog.
    #[must_use]
    pub fn new(catalog: Arc<Catalog>) -> Self {
        Self(catalog)
    }
}

impl CatalogSource for FixedCatalog {
    fn current(&self) -> Arc<Catalog> {
        Arc::clone(&self.0)
    }
}

/// A [`CatalogSource`] an upgrade can swap, for tests and for the metadata-upgrade path.
pub struct SharedCatalog {
    inner: Mutex<Arc<Catalog>>,
}

impl SharedCatalog {
    /// A source over one catalog, replaceable.
    #[must_use]
    pub fn new(catalog: Arc<Catalog>) -> Self {
        Self {
            inner: Mutex::new(catalog),
        }
    }

    /// Replaces the snapshot; applies already running keep the snapshot they began with.
    pub fn replace(&self, catalog: Arc<Catalog>) {
        *self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = catalog;
    }
}

impl CatalogSource for SharedCatalog {
    fn current(&self) -> Arc<Catalog> {
        Arc::clone(
            &self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

/// Whether an incoming record must wait behind a quarantined one.
///
/// The default, [`SameDocument`], parks a later version of the same document behind the held
/// version. ADR-064's dependency closure replaces this with "records that reference a parked
/// record" when that ADR lands; the seam is here so this adapter does not change.
pub trait Dependents: Send + Sync {
    /// Whether `incoming` waits behind the held `item`.
    fn waits_behind(&self, item: &QuarantineItem, incoming: &ReceivedRecord) -> bool;
}

/// The default [`Dependents`]: a later version of the same document waits behind the held one.
#[derive(Clone, Copy, Debug, Default)]
pub struct SameDocument;

impl Dependents for SameDocument {
    fn waits_behind(&self, item: &QuarantineItem, incoming: &ReceivedRecord) -> bool {
        item.state.is_open()
            && item.record_ref.name == incoming.name
            && incoming.version > item.record_ref.version
    }
}

/// What receiving one record did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntakeOutcome {
    /// The record was stored.
    Applied,
    /// The record was already stored (a duplicate or a stale version).
    Unchanged,
    /// The record was refused and stored in the queue.
    Quarantined(QuarantineItem),
    /// The record waits behind a quarantined item.
    Parked(QuarantineItem),
    /// The queue already holds this record.
    AlreadyQuarantined,
    /// The record image, the sender name or the reason code is over the size limit: nothing of it
    /// was stored, and this short marker item (reason `quarantine.payload_too_large`, with the
    /// sizes and a hash of the image, never the text) records the refusal. A sender name too long
    /// to store is replaced by a stand-in made from its hash. The checkpoint advances and the
    /// marker can only be discarded.
    TooLarge(QuarantineItem),
}

/// Who is re-applying an item, and so who may close it.
enum Actor<'a> {
    /// A person, who was checked for `Access` on the item.
    Operator(&'a Principal, Access),
    /// The automatic retry schedule, which needs no operator permission.
    Automation,
}

/// The intake adapter over one storage backend and one quarantine store.
///
/// It is one value the sync consumer owns, one per source; the store, the guard and the audit
/// sinks it holds are `Arc`-shared.
pub struct Intake<B: StorageBackend> {
    backend: B,
    catalog: Arc<dyn CatalogSource>,
    store: Arc<dyn QuarantineStore>,
    guard: Arc<dyn QuarantineGuard>,
    audit_sink: Arc<dyn QuarantineAudit>,
    owner: Arc<dyn OwnerCheck>,
    replica_audit: Arc<dyn ReplicaAudit>,
    notice: Option<Arc<dyn NoticeSink>>,
    notify: bool,
    origin: OriginFields,
    max_skew: u64,
    dependents: Arc<dyn Dependents>,
    retry: RetryPolicy,
    options: TxOptions,
    checkpoints: Mutex<BTreeMap<String, u64>>,
}

impl<B: StorageBackend> Intake<B> {
    /// An intake over `backend`, the catalog `catalog` reports, `store`, the permission `guard`,
    /// the quarantine `audit_sink`, the ownership check and the replicated-marker audit.
    ///
    /// `options` is the transaction the apply path runs in; the isolation level has no default
    /// (ADR-046), so the caller declares it.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        backend: B,
        catalog: Arc<dyn CatalogSource>,
        store: Arc<dyn QuarantineStore>,
        guard: Arc<dyn QuarantineGuard>,
        audit_sink: Arc<dyn QuarantineAudit>,
        owner: Arc<dyn OwnerCheck>,
        replica_audit: Arc<dyn ReplicaAudit>,
        options: TxOptions,
    ) -> Self {
        Self {
            backend,
            catalog,
            store,
            guard,
            audit_sink,
            owner,
            replica_audit,
            notice: None,
            notify: false,
            origin: OriginFields::default(),
            max_skew: 0,
            dependents: Arc::new(SameDocument),
            retry: RetryPolicy::sync_default(),
            options,
            checkpoints: Mutex::new(BTreeMap::new()),
        }
    }

    /// The same intake with a delivery-notice sink (ADR-058 rule 9).
    #[must_use]
    pub fn with_notice(mut self, notice: Arc<dyn NoticeSink>, notify: bool) -> Self {
        self.notice = Some(notice);
        self.notify = notify;
        self
    }

    /// The same intake with explicit origin field names (ADR-058 rule 5).
    #[must_use]
    pub fn with_origin(mut self, origin: OriginFields) -> Self {
        self.origin = origin;
        self
    }

    /// The same intake accepting `skew` metadata versions ahead of this node (ADR-066).
    #[must_use]
    pub fn with_max_skew(mut self, skew: u64) -> Self {
        self.max_skew = skew;
        self
    }

    /// The same intake with a different dependency seam (ADR-064).
    #[must_use]
    pub fn with_dependents(mut self, dependents: Arc<dyn Dependents>) -> Self {
        self.dependents = dependents;
        self
    }

    /// The same intake with a different automatic retry schedule.
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The quarantine store this intake writes to.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn QuarantineStore> {
        &self.store
    }

    /// The retry schedule.
    #[must_use]
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry
    }

    /// The checkpoint this intake holds for `source`, if it has seen a record from it.
    #[must_use]
    pub fn checkpoint(&self, source: &SourceId) -> Option<Checkpoint> {
        let checkpoints = self
            .checkpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        checkpoints
            .get(source.as_str())
            .map(|seq| Checkpoint::new(source.clone(), *seq))
    }

    /// Receives one record: apply it, quarantine it on refusal, or park it behind a held item.
    ///
    /// The stream checkpoint advances in every case, so a refused record never blocks the ones
    /// after it.
    ///
    /// # Errors
    /// [`QuarantineError::Store`] when the store fails. An apply refusal is **not** an error: it is
    /// [`IntakeOutcome::Quarantined`].
    pub async fn receive(
        &self,
        record: &ReceivedRecord,
        at: i64,
    ) -> Result<IntakeOutcome, QuarantineError> {
        let outcome = self.receive_inner(record, at).await?;
        // The checkpoint advances past a quarantined or parked record exactly as past an applied
        // one (ADR-062 G2: nothing is silently dropped, and the stream does not stall). A storage
        // failure returns before this line, so the record is retried rather than skipped.
        self.advance(&record.source, record.seq);
        Ok(outcome)
    }

    async fn receive_inner(
        &self,
        record: &ReceivedRecord,
        at: i64,
    ) -> Result<IntakeOutcome, QuarantineError> {
        // A sender name too long to store is looked up under the stand-in its marker was stored
        // under, so a redelivery is recognised instead of queueing a second marker.
        let key_source = stored_source(&record.source)?;
        if self
            .store
            .find(&key_source, &record.name, record.version)
            .await?
            .is_some()
        {
            return Ok(IntakeOutcome::AlreadyQuarantined);
        }

        let open = self.store.list(&ItemFilter::all()).await?;
        if let Some(blocker) = open
            .iter()
            .find(|item| item.state.is_open() && self.dependents.waits_behind(item, record))
        {
            let (item, too_large) = self
                .quarantine(
                    record,
                    codes::DEPENDENT_HELD,
                    format!("waiting behind quarantine item {}", blocker.id),
                    at,
                )
                .await?;
            if too_large {
                return Ok(IntakeOutcome::TooLarge(item));
            }
            return Ok(IntakeOutcome::Parked(item));
        }

        let outcome = self.apply(record, at).await?;
        if outcome.is_stored() {
            return Ok(IntakeOutcome::Applied);
        }
        if let Some(reason) = outcome.reason() {
            let (item, too_large) = self
                .quarantine(record, reason.code(), reason.to_string(), at)
                .await?;
            if too_large {
                return Ok(IntakeOutcome::TooLarge(item));
            }
            return Ok(IntakeOutcome::Quarantined(item));
        }
        Ok(IntakeOutcome::Unchanged)
    }

    /// Stores `record` as a held item. A record image over [`MAX_PAYLOAD_BYTES`] is **not** stored:
    /// a short marker item takes its place, and the flag says so. A `detail` over
    /// [`MAX_DETAIL_BYTES`] is cut at a character boundary with a visible note of the original
    /// size, never silently.
    async fn quarantine(
        &self,
        record: &ReceivedRecord,
        reason_code: &str,
        detail: String,
        at: i64,
    ) -> Result<(QuarantineItem, bool), QuarantineError> {
        let image = record.payload_json().map_err(|_| QuarantineError::Store {
            why: "the record image could not be serialised".to_owned(),
        })?;
        let oversize = Oversize {
            image: image.len(),
            source: record.source.as_str().len(),
            reason: reason_code.len(),
        };
        if oversize.any() {
            // A marker takes the record's place: nothing of the image, the sender name or the
            // reason code is stored, only their sizes and a hash of the image. The stream moves
            // on, and the marker can only be discarded.
            let digest = Sha256::digest(image.as_bytes());
            let mut hash = String::with_capacity(64);
            for byte in digest {
                // Writing to a String cannot fail.
                let _ = write!(hash, "{byte:02x}");
            }
            let source = stored_source(&record.source)?;
            let marker = marker_record(record, &source);
            let item = self
                .store
                .insert(
                    NewItem::held(
                        source.clone(),
                        record.seq,
                        RecordRef::of(record),
                        codes::PAYLOAD_TOO_LARGE,
                        marker_detail(&source, &oversize, &hash),
                        at,
                    ),
                    marker,
                )
                .await?;
            return Ok((item, true));
        }
        let item = self
            .store
            .insert(
                NewItem::held(
                    record.source.clone(),
                    record.seq,
                    RecordRef::of(record),
                    reason_code,
                    bounded_detail(detail),
                    at,
                ),
                record.clone(),
            )
            .await?;
        Ok((item, false))
    }

    /// Retries one item now (a person's action), regardless of the automatic cap.
    ///
    /// # Errors
    /// [`QuarantineError::NotFound`], [`QuarantineError::Denied`], [`QuarantineError::NotOpen`],
    /// or a store or audit failure.
    pub async fn retry(
        &self,
        principal: &Principal,
        id: ItemId,
        at: i64,
    ) -> Result<OperationView, QuarantineError> {
        let item = self.open_item(principal, Access::Retry, id).await?;
        self.reapply(
            &item,
            at,
            Actor::Operator(principal, Access::Retry),
            "manual retry",
        )
        .await
    }

    /// Retries every held item, in order, recording `note` as the reason (for example "after a
    /// metadata upgrade").
    ///
    /// Items the principal may not retry are skipped. The order is insertion order, so a document
    /// and the later versions parked behind it are released in order.
    ///
    /// # Errors
    /// A store or audit failure.
    pub async fn retry_all(
        &self,
        principal: &Principal,
        note: &str,
        at: i64,
    ) -> Result<Vec<OperationView>, QuarantineError> {
        let items = self.store.list(&ItemFilter::all()).await?;
        let mut views = Vec::new();
        for item in items {
            if !item.state.is_open() || is_marker(&item) {
                continue;
            }
            if !self.guard.may(principal, Access::Retry, &item) {
                continue;
            }
            views.push(
                self.reapply(&item, at, Actor::Operator(principal, Access::Retry), note)
                    .await?,
            );
        }
        Ok(views)
    }

    /// Releases one item by re-applying it at `as_of` (a person's action).
    ///
    /// # Errors
    /// As [`retry`](Intake::retry).
    pub async fn release(
        &self,
        principal: &Principal,
        id: ItemId,
        as_of: i64,
    ) -> Result<OperationView, QuarantineError> {
        let item = self.open_item(principal, Access::Release, id).await?;
        self.reapply(
            &item,
            as_of,
            Actor::Operator(principal, Access::Release),
            "released by an operator",
        )
        .await
    }

    /// Discards one item: it leaves the active queue, its payload is **kept**, and the decision is
    /// audited. The sender's data is never deleted (WP-EDG-06.T2 "must not").
    ///
    /// # Errors
    /// [`QuarantineError::ReasonRequired`] when `reason` is blank, and as
    /// [`retry`](Intake::retry).
    pub async fn discard(
        &self,
        principal: &Principal,
        id: ItemId,
        reason: &str,
        at: i64,
    ) -> Result<OperationView, QuarantineError> {
        if reason.trim().is_empty() {
            return Err(QuarantineError::ReasonRequired { id });
        }
        let item = self.open_item(principal, Access::Discard, id).await?;
        let authority = authorize_close(
            &*self.guard,
            principal,
            Access::Discard,
            &item,
            ClosureOutcome::Discarded,
        )?;
        let mut discarded = item.clone();
        discarded.state = State::Discarded;
        // The decision is audited **before** it takes effect, so a discard can never be applied
        // unrecorded; if the store then fails, the audit shows the attempt and the item is intact.
        self.audit(
            AuditAction::Discarded,
            &discarded,
            actor(principal),
            reason,
            at,
        )
        .await?;
        let closed = self
            .store
            .close(
                id,
                Closure {
                    outcome: ClosureOutcome::Discarded,
                    by: actor(principal),
                    at,
                },
                authority,
            )
            .await?;
        Ok(OperationView {
            item: ItemView::of(&closed, &self.retry).with_closure(Some(ClosureRecord {
                by: actor(principal),
                at,
            })),
            result: OperationResult::Discarded,
            condition: None,
        })
    }

    /// Runs the automatic retry schedule for every held item due at `now`.
    ///
    /// The attempt that reaches the cap leaves the item `Held` and raises the `QUARANTINED`
    /// condition; a later run reports the condition without attempting again (ADR-067). Automation
    /// needs no operator permission; every attempt is audited.
    ///
    /// # Errors
    /// A store or audit failure.
    pub async fn run_due(&self, now: i64) -> Result<Vec<OperationView>, QuarantineError> {
        let items = self.store.list(&ItemFilter::all()).await?;
        let mut views = Vec::new();
        for item in items {
            if !item.state.is_open() {
                continue;
            }
            if self.retry.exhausted(&item) || is_marker(&item) {
                let info = reason::describe(&item.reason_code);
                views.push(OperationView {
                    item: ItemView::of(&item, &self.retry),
                    result: OperationResult::StillHeld,
                    condition: Some(ConditionView::from(Condition::quarantined(&item, &info))),
                });
                continue;
            }
            if self.retry.due_at(&item) > now {
                continue;
            }
            views.push(
                self.reapply(&item, now, Actor::Automation, "automatic retry")
                    .await?,
            );
        }
        Ok(views)
    }

    /// Loads an open item and checks the principal may act on it.
    async fn open_item(
        &self,
        principal: &Principal,
        access: Access,
        id: ItemId,
    ) -> Result<QuarantineItem, QuarantineError> {
        let item = self
            .store
            .item(id)
            .await?
            .ok_or(QuarantineError::NotFound { id })?;
        if !self.guard.may(principal, access, &item) {
            return Err(QuarantineError::Denied {
                access: access.as_str(),
                id,
            });
        }
        if !item.state.is_open() {
            return Err(QuarantineError::NotOpen {
                id,
                state: item.state,
                access: access.as_str(),
            });
        }
        Ok(item)
    }

    /// Re-applies one item, updates it, audits the attempt, and reports the outcome.
    async fn reapply(
        &self,
        item: &QuarantineItem,
        at: i64,
        who: Actor<'_>,
        note: &str,
    ) -> Result<OperationView, QuarantineError> {
        if is_marker(item) {
            // The marker holds no record image, only the refusal; there is nothing to apply.
            return Err(QuarantineError::MarkerOnly { id: item.id });
        }
        let actor = match &who {
            Actor::Operator(principal, _) => actor(principal),
            Actor::Automation => "system".to_owned(),
        };
        let record = self
            .store
            .record(item.id)
            .await?
            .ok_or(QuarantineError::NotFound { id: item.id })?;

        let mut working = item.clone();
        working.state = State::Retrying;
        self.store.update(&working).await?;

        let outcome = self.apply(&record, at).await?;
        let mut updated = working.clone();
        updated.attempts = updated.attempts.saturating_add(1);

        if outcome.is_stored() || outcome.is_unchanged() {
            // The attempt count is written while the item is still open; then it is closed.
            self.store.update(&updated).await?;
            let authority = match &who {
                Actor::Operator(principal, access) => authorize_close(
                    &*self.guard,
                    principal,
                    *access,
                    &updated,
                    ClosureOutcome::Released,
                )?,
                Actor::Automation => CloseAuthority::for_automation(updated.id),
            };
            let closed = self
                .store
                .close(
                    updated.id,
                    Closure {
                        outcome: ClosureOutcome::Released,
                        by: actor.clone(),
                        at,
                    },
                    authority,
                )
                .await?;
            self.audit(AuditAction::Released, &closed, actor.clone(), note, at)
                .await?;
            return Ok(OperationView {
                item: ItemView::of(&closed, &self.retry)
                    .with_closure(Some(ClosureRecord { by: actor, at })),
                result: OperationResult::Released,
                condition: None,
            });
        }

        updated.state = State::Held;
        if let Some(reason) = outcome.reason() {
            updated.reason_code = reason.code().to_owned();
            updated.detail = bounded_detail(reason.to_string());
        }
        self.store.update(&updated).await?;
        self.audit(AuditAction::Retried, &updated, actor, note, at)
            .await?;
        let condition = if self.retry.exhausted(&updated) {
            let info = reason::describe(&updated.reason_code);
            Some(ConditionView::from(Condition::quarantined(&updated, &info)))
        } else {
            None
        };
        Ok(OperationView {
            item: ItemView::of(&updated, &self.retry),
            result: OperationResult::StillHeld,
            condition,
        })
    }

    /// Applies one record through the shared apply path in a fresh transaction.
    ///
    /// A storage failure to open the transaction is **not** a rejection: it is returned as
    /// [`QuarantineError::Store`], so a backend outage retries the record rather than quarantining
    /// every record in the batch.
    async fn apply(
        &self,
        record: &ReceivedRecord,
        at: i64,
    ) -> Result<ApplyOutcome, QuarantineError> {
        let ctx = self.ctx(at);
        let tx = self
            .backend
            .begin(self.catalog.current(), self.options.clone())
            .await
            .map_err(|error| QuarantineError::Store {
                why: error.to_string(),
            })?;
        Ok(spot_replica_apply::apply(tx, record, &ctx).await)
    }

    /// The apply context: policy comes from the seams, time is the argument.
    fn ctx(&self, at: i64) -> ApplyCtx<'_> {
        ApplyCtx {
            owner: &*self.owner,
            audit: &*self.replica_audit,
            notice: self.notice.as_deref(),
            notify: self.notify,
            origin: self.origin.clone(),
            max_skew: self.max_skew,
            at,
            side_effects: None,
        }
    }

    /// Writes one audit entry.
    async fn audit(
        &self,
        action: AuditAction,
        item: &QuarantineItem,
        actor: String,
        reason: &str,
        at: i64,
    ) -> Result<(), QuarantineError> {
        let entry = AuditEntry {
            action,
            item: item.id,
            record_ref: item.record_ref.clone(),
            source: item.source.clone(),
            actor,
            reason: reason.to_owned(),
            at,
        };
        self.audit_sink.record(&entry).await
    }

    /// Advances the checkpoint for `source` past `seq`, never backwards.
    fn advance(&self, source: &SourceId, seq: u64) {
        let mut checkpoints = self
            .checkpoints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = checkpoints.entry(source.as_str().to_owned()).or_insert(0);
        *entry = (*entry).max(seq);
    }
}

/// The audit actor for a principal: `system` or the user's name.
fn actor(principal: &Principal) -> String {
    if principal.is_system {
        "system".to_owned()
    } else {
        principal.user.clone()
    }
}

/// Whether an item is the marker of a record too large to store.
fn is_marker(item: &QuarantineItem) -> bool {
    item.reason_code == codes::PAYLOAD_TOO_LARGE
}

/// `detail` cut to [`MAX_DETAIL_BYTES`] at a character boundary, with a visible note of the
/// original size when it was cut.
fn bounded_detail(detail: String) -> String {
    if detail.len() <= MAX_DETAIL_BYTES {
        return detail;
    }
    let note = format!(" [cut from {} bytes]", detail.len());
    let mut end = MAX_DETAIL_BYTES.saturating_sub(note.len());
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = detail;
    out.truncate(end);
    out.push_str(&note);
    out
}

/// The sizes of the values of one refused record, to decide whether it fits the queue.
struct Oversize {
    /// The record image's JSON text.
    image: usize,
    /// The sender name.
    source: usize,
    /// The rejection code.
    reason: usize,
}

impl Oversize {
    fn any(&self) -> bool {
        self.image > MAX_PAYLOAD_BYTES
            || self.source > MAX_SOURCE_BYTES
            || self.reason > MAX_REASON_CODE_BYTES
    }
}

/// The detail of a marker item: sizes and a hash, **never** the text of the sender name, the
/// reason code or the image (`source` is the name the marker is stored under, which is bounded).
fn marker_detail(source: &SourceId, sizes: &Oversize, hash: &str) -> String {
    let mut parts = Vec::new();
    if sizes.image > MAX_PAYLOAD_BYTES {
        parts.push(format!(
            "the record image from {source} is {} bytes, over the {MAX_PAYLOAD_BYTES} the queue stores",
            sizes.image
        ));
    } else {
        parts.push(format!(
            "the record from {source} cannot be queued under its own names, so only a marker was kept"
        ));
    }
    if sizes.source > MAX_SOURCE_BYTES {
        parts.push(format!(
            "the sender name is {} bytes, over the {MAX_SOURCE_BYTES} the queue stores",
            sizes.source
        ));
    }
    if sizes.reason > MAX_REASON_CODE_BYTES {
        parts.push(format!(
            "the reason code is {} bytes, over the {MAX_REASON_CODE_BYTES} the queue stores",
            sizes.reason
        ));
    }
    parts.push(format!("sha256 {hash}"));
    parts.join("; ")
}

/// The sender name an item is stored under: the name itself when the queue can hold it, else a
/// fixed-length stand-in made from a hash of the name (never a cut of it, which could make two
/// senders look alike).
fn stored_source(source: &SourceId) -> Result<SourceId, QuarantineError> {
    if source.as_str().len() <= MAX_SOURCE_BYTES {
        return Ok(source.clone());
    }
    let digest = Sha256::digest(source.as_str().as_bytes());
    let mut stand_in = String::from("oversize-");
    for byte in digest.iter().take(24) {
        // Writing to a String cannot fail.
        let _ = write!(stand_in, "{byte:02x}");
    }
    SourceId::new(&stand_in).map_err(|_| QuarantineError::Store {
        why: "the sender name is too long and no stand-in could be made".to_owned(),
    })
}

/// The stand-in stored for a record too large to keep: the same apply key and document identity,
/// no fields, no children and no entries. It is never applied. `source` is the bounded name the
/// marker item is stored under.
fn marker_record(record: &ReceivedRecord, source: &SourceId) -> ReceivedRecord {
    ReceivedRecord {
        name: record.name.clone(),
        version: record.version,
        seq: record.seq,
        source: source.clone(),
        topic: None,
        conversation: None,
        meta_version: record.meta_version,
        document: DocumentData {
            entity: record.document.entity.clone(),
            id: record.document.id,
            fields: Fields::new(),
            children: BTreeMap::new(),
        },
        entries: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn an_overlong_reason_code_is_described_by_its_size_only() {
        let reason = format!("replica_apply.{}", "secret".repeat(40));
        let sizes = Oversize {
            image: 100,
            source: 10,
            reason: reason.len(),
        };
        assert!(sizes.any());
        let source = SourceId::new("head.office").unwrap();
        let detail = marker_detail(&source, &sizes, "ab");
        assert!(detail.contains(&reason.len().to_string()));
        assert!(
            !detail.contains("secret"),
            "the text is never kept: {detail}"
        );
        assert!(detail.len() <= MAX_DETAIL_BYTES);
    }

    #[test]
    fn a_source_at_the_limit_is_kept_and_a_longer_one_gets_a_stand_in() {
        let label = "a".repeat(60);
        let long = format!("{label}.{label}.{label}.{label}");
        let source = SourceId::new(&long).unwrap();
        assert!(source.as_str().len() > MAX_SOURCE_BYTES);
        let stand_in = stored_source(&source).unwrap();
        assert!(stand_in.as_str().len() <= MAX_SOURCE_BYTES);
        assert!(!stand_in.as_str().contains(&label));
        assert_eq!(stored_source(&source).unwrap(), stand_in, "stable");
        let other = SourceId::new(&format!("{label}.{label}.{label}.{}", "b".repeat(60))).unwrap();
        assert_ne!(stored_source(&other).unwrap(), stand_in);
        let short = SourceId::new("head.office").unwrap();
        assert_eq!(stored_source(&short).unwrap(), short);
    }

    #[test]
    fn a_long_detail_is_cut_on_a_character_boundary_and_says_so() {
        let short = "fine".to_owned();
        assert_eq!(bounded_detail(short.clone()), short);

        let exact = "a".repeat(MAX_DETAIL_BYTES);
        assert_eq!(bounded_detail(exact.clone()), exact);

        // A multi-byte character straddling the cut must not split.
        let long = "\u{e9}".repeat(MAX_DETAIL_BYTES);
        let cut = bounded_detail(long.clone());
        assert!(cut.len() <= MAX_DETAIL_BYTES);
        assert!(cut.contains(&format!("[cut from {} bytes]", long.len())));
    }
}
