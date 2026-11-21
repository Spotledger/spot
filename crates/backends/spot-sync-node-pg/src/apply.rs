//! [`PgBundleApply`]: the real `BundleApply`, over the one replication apply path.
//!
//! # One transaction per bundle
//!
//! `apply_bundle` opens **one** storage transaction (`PgBackend::begin`, read committed, every
//! document row locked for update by the apply path) and applies every record of the bundle in it
//! through [`spot_replica_apply::apply_quarantining`]:
//!
//! * ownership is checked **before the first write** of each record, against the node's own
//!   effective-dated view ([`OwnerCheck`], G1), and a record of a sender that does not own it is
//!   handed to the quarantine port and never stored;
//! * shape, skew, idempotence by `(name, version)`, the origin marking of a mirror and the
//!   `replicated` audit marker are the apply path's own;
//! * a tombstone is a delete in the same transaction (the stored document is checked against the
//!   ownership view first), and is remembered in the ledger so a replayed older image cannot
//!   bring the record back;
//! * each applied `(entity, id, version)` gets its ledger row in `spot_sync_node_applied`, and an
//!   optional checkpoint is advanced, **in the same transaction**;
//! * the bundle commits once, after the last record. A record that is rejected rolls the whole
//!   transaction back: nothing of the bundle is ever visible.
//!
//! The apply path commits the transaction it is handed, so each record gets a [`BundleTx`]
//! wrapper whose `commit` and `rollback` do nothing; this runner owns the real commit.
//!
//! # No event, no controller
//!
//! The wrapper refuses an outbox `enqueue` and counts it; the apply path never calls one, and a
//! bundle in which it tried is refused. A replicated record is not a new change at this node.
//!
//! # Where the source comes from
//!
//! The trait has no source argument: a node has one hub (`NodeConfig::hub_source`, and the intake
//! refuses a delivery from any other source before it reaches the apply). The source is therefore
//! fixed at construction, from the same configuration, and is what the ownership check and the
//! audit marker see.
//!
//! # Threads
//!
//! The trait is synchronous: the call crosses the shared bridge ([`spot_pg_bridge::Bridge`]) and
//! is refused (`pg_bridge.wrong_thread`) on an async runtime thread. The host calls the intake
//! from a blocking thread, never from a request path. The bridge's connection is released at once
//! (`PgBackend::begin` checks out the transaction's own connection from the same pool), so a call
//! holds one pool connection while it runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spot_postgres::{PgBackend, PgTransaction};
use spot_replica_apply::{
    ApplyCtx, ApplyOutcome, OriginFields, OwnerCheck, QuarantinePort, Reason, ReceivedRecord,
    ReplicaAudit, apply_quarantining,
};
use spot_storage_contract::{
    Catalog, DocumentStore, Isolation, Name, RecordId, RowLock, StorageBackend, StorageError,
    Transaction, TxOptions, Wait,
};
use spot_sync_closure::{Bundle, BundleRecord, Position, RecordKey};
use spot_sync_node::{AppliedRecord, AppliedSet, ApplyRefusal, BundleApply};
use spot_sync_protocol::{Checkpoint, RecordName, SourceId};

use crate::bundle_tx::{BundleTx, EventAttempts};
use crate::checkpoints;
use crate::db::{Db, valid_tenant};
use crate::error::{FailureLog, OpError, PgNodeError, map};
use crate::ledger::{self, PgLedger};

/// The catalog the transaction runs against: the storage catalog of the node's effective metadata.
pub trait CatalogSource: Send + Sync {
    /// The catalog now; a metadata swap changes what this returns for the next bundle.
    fn current(&self) -> Arc<Catalog>;
}

/// A catalog that never changes.
#[derive(Clone, Debug)]
pub struct StaticCatalog(
    /// The catalog.
    pub Arc<Catalog>,
);

impl CatalogSource for StaticCatalog {
    fn current(&self) -> Arc<Catalog> {
        self.0.clone()
    }
}

/// The instant of an apply, in Unix microseconds. The library reads no clock: the host supplies it.
pub trait ApplyClock: Send + Sync {
    /// Now, in Unix microseconds.
    fn now_micros(&self) -> i64;
}

/// Where a test may stop the apply, to prove what a crash leaves behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failpoint {
    /// Every record and its ledger row is written; the checkpoint is not yet.
    AfterRecords,
    /// The checkpoint is written too; the commit is not yet sent.
    BeforeCommit,
}

/// A crash simulator: when it answers `true` for a point, the transaction is dropped there
/// without commit (the connection is discarded and the server rolls it back), exactly what a
/// killed process leaves. Production passes none.
pub trait Failpoints: Send + Sync {
    /// Whether to crash at `point`.
    fn crash_at(&self, point: Failpoint) -> bool;
}

/// Limits of one bundle apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyLimits {
    /// Deadline of the whole call (it includes waiting for the bridge). A timed-out call may or
    /// may not have committed; the intake treats it as refused and the redelivery is a replay.
    pub deadline: Duration,
    /// Most writes of the transaction (documents, child rows and ledger rows).
    pub max_writes: u32,
}

impl Default for ApplyLimits {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(60),
            max_writes: 10_000,
        }
    }
}

/// What the apply path needs, shared by every bundle.
pub struct ApplyEnv {
    /// The storage backend whose transaction carries the bundle.
    pub backend: Arc<PgBackend>,
    /// The catalog source.
    pub catalog: Arc<dyn CatalogSource>,
    /// The ownership decision (G1).
    pub owner: Arc<dyn OwnerCheck>,
    /// Where the `replicated` marker is written.
    pub audit: Arc<dyn ReplicaAudit>,
    /// Where a refused record is held so the stream can move on (G6).
    pub quarantine: Arc<dyn QuarantinePort>,
    /// The instant of an apply.
    pub clock: Arc<dyn ApplyClock>,
    /// The field names a mirror carries its origin in.
    pub origin: OriginFields,
    /// The metadata skew accepted from the sender (ADR-066).
    pub max_skew: u64,
    /// The limits of one bundle.
    pub limits: ApplyLimits,
    /// A crash simulator for tests; `None` in production.
    pub failpoints: Option<Arc<dyn Failpoints>>,
}

impl std::fmt::Debug for ApplyEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplyEnv")
            .field("max_skew", &self.max_skew)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// The real `BundleApply` of one tenant (one tenant is one database).
#[derive(Clone, Debug)]
pub struct PgBundleApply {
    db: Db,
    tenant: String,
    source: SourceId,
    env: Arc<ApplyEnv>,
    ledger: PgLedger,
    failures: FailureLog,
}

impl PgBundleApply {
    /// The apply of `tenant`, for bundles from `source`.
    ///
    /// # Errors
    /// `sync_node_pg.unsupported_context` for an empty, long or unprintable tenant.
    pub fn new(
        db: Db,
        tenant: impl Into<String>,
        source: SourceId,
        env: ApplyEnv,
    ) -> Result<Self, PgNodeError> {
        let tenant = tenant.into();
        if !valid_tenant(&tenant) {
            return Err(PgNodeError::UnsupportedContext {
                reason: "the tenant is empty, too long or not printable",
            });
        }
        let failures = FailureLog::new();
        Ok(Self {
            ledger: PgLedger::with_failures(db.clone(), failures.clone()),
            db,
            tenant,
            source,
            env: Arc::new(env),
            failures,
        })
    }

    /// The tenant this apply writes for.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The ledger reader of this tenant.
    #[must_use]
    pub fn ledger(&self) -> &PgLedger {
        &self.ledger
    }

    /// The last failure of `has` or `has_tombstone` since the previous call.
    #[must_use]
    pub fn take_failure(&self) -> Option<PgNodeError> {
        self.failures.take()
    }

    /// Applies the bundle and advances `checkpoint` in the **same transaction**: the documents,
    /// the ledger rows and the checkpoint exist together or not at all.
    ///
    /// # Errors
    /// `ApplyRefusal` when the bundle is refused (nothing stored): a record rejected by the apply
    /// path (the text carries its `replica_apply.*` code), a checkpoint ahead of the ledger, or a
    /// store failure.
    pub fn apply_bundle_checkpointed(
        &self,
        tenant: &str,
        bundle: &Bundle,
        checkpoint: &Checkpoint,
    ) -> Result<AppliedSet, ApplyRefusal> {
        self.run_bundle(tenant, bundle, Some(checkpoint.clone()))
    }

    fn run_bundle(
        &self,
        tenant: &str,
        bundle: &Bundle,
        checkpoint: Option<Checkpoint>,
    ) -> Result<AppliedSet, ApplyRefusal> {
        if tenant != self.tenant {
            return Err(refusal("a bundle for another tenant is never applied here"));
        }
        let (env, tenant, source, bundle) = (
            self.env.clone(),
            self.tenant.clone(),
            self.source.clone(),
            bundle.clone(),
        );
        let deadline = env.limits.deadline;
        self.db
            .run(deadline, move |conn| {
                // The transaction checks out its own connection; give this one back now.
                drop(conn);
                Box::pin(async move { run(env, tenant, source, bundle, checkpoint).await })
            })
            .map_err(|error| ApplyRefusal {
                reason: map(error).to_string(),
            })
    }

    fn delete_one(
        &self,
        tenant: &str,
        key: &RecordKey,
        position: Position,
    ) -> Result<(), ApplyRefusal> {
        if tenant != self.tenant {
            return Err(refusal("a delete for another tenant is never applied here"));
        }
        let record = BundleRecord {
            role: spot_sync_closure::Role::Root,
            key: key.clone(),
            position,
            image: serde_json::Value::Null,
            deleted: true,
        };
        let bundle = Bundle {
            kind: spot_sync_closure::BundleKind::Root,
            records: vec![record],
            bytes: 0,
        };
        self.run_bundle(tenant, &bundle, None).map(|_| ())
    }
}

impl BundleApply for PgBundleApply {
    fn has(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool {
        tenant == self.tenant && self.ledger.has(tenant, key, min_version)
    }

    fn has_tombstone(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool {
        tenant == self.tenant && self.ledger.has_tombstone(tenant, key, min_version)
    }

    fn apply_bundle(&mut self, tenant: &str, bundle: &Bundle) -> Result<AppliedSet, ApplyRefusal> {
        self.run_bundle(tenant, bundle, None)
    }

    fn delete(
        &mut self,
        tenant: &str,
        key: &RecordKey,
        _version: u64,
        position: Position,
    ) -> Result<(), ApplyRefusal> {
        // The version the sink holds for a deletion is the position of the record's latest
        // event (the trait says so): the ledger stores it once, as the position.
        self.delete_one(tenant, key, position)
    }
}

/// Routes the intake's per-tenant calls to the apply of each tenant's database. A tenant with no
/// apply is never applied for.
#[derive(Debug, Default)]
pub struct PgApplyRouter {
    tenants: std::collections::BTreeMap<String, PgBundleApply>,
}

impl PgApplyRouter {
    /// An empty router.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the apply of one tenant (replacing an earlier one).
    pub fn insert(&mut self, apply: PgBundleApply) {
        self.tenants.insert(apply.tenant().to_owned(), apply);
    }

    /// The apply of `tenant`.
    #[must_use]
    pub fn get(&self, tenant: &str) -> Option<&PgBundleApply> {
        self.tenants.get(tenant)
    }
}

impl BundleApply for PgApplyRouter {
    fn has(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool {
        self.tenants
            .get(tenant)
            .is_some_and(|a| a.has(tenant, key, min_version))
    }

    fn has_tombstone(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool {
        self.tenants
            .get(tenant)
            .is_some_and(|a| a.has_tombstone(tenant, key, min_version))
    }

    fn apply_bundle(&mut self, tenant: &str, bundle: &Bundle) -> Result<AppliedSet, ApplyRefusal> {
        match self.tenants.get_mut(tenant) {
            Some(apply) => apply.apply_bundle(tenant, bundle),
            None => Err(refusal("no store is configured for this tenant")),
        }
    }

    fn delete(
        &mut self,
        tenant: &str,
        key: &RecordKey,
        version: u64,
        position: Position,
    ) -> Result<(), ApplyRefusal> {
        match self.tenants.get_mut(tenant) {
            Some(apply) => apply.delete(tenant, key, version, position),
            None => Err(refusal("no store is configured for this tenant")),
        }
    }
}

fn refusal(reason: &str) -> ApplyRefusal {
    ApplyRefusal {
        reason: format!("{}: {reason}", crate::error::APPLY_REFUSED),
    }
}

/// A record that could not be applied: why, and the decoded record when there is one (to hold it).
struct Failure {
    reason: Reason,
    record: Option<ReceivedRecord>,
}

impl From<Reason> for Failure {
    fn from(reason: Reason) -> Self {
        Self {
            reason,
            record: None,
        }
    }
}

fn write_failed(error: StorageError) -> Reason {
    Reason::WriteFailed {
        why: error.to_string(),
    }
}

fn refused(reason: impl Into<String>) -> OpError {
    OpError::Refused(PgNodeError::ApplyRefused {
        reason: reason.into(),
    })
}

/// Whether the rejection is a shape problem that the quarantine queue should hold: not an
/// ownership refusal (already handed over by the apply path), not a transient storage, audit,
/// notice or queue failure, and not a metadata skew (a version hold, released by a swap).
fn is_shape_refusal(reason: &Reason) -> bool {
    !reason.is_ownership_refusal()
        && !matches!(
            reason,
            Reason::WriteFailed { .. }
                | Reason::AuditFailed { .. }
                | Reason::NoticeFailed { .. }
                | Reason::QuarantineFailed { .. }
                | Reason::MetadataSkew { .. }
        )
}

/// The whole bundle, in one transaction.
async fn run(
    env: Arc<ApplyEnv>,
    tenant: String,
    source: SourceId,
    bundle: Bundle,
    checkpoint: Option<Checkpoint>,
) -> Result<AppliedSet, OpError> {
    let at = env.clock.now_micros();
    let label = Name::new("sync_node_apply").map_err(|_| refused("the transaction label"))?;
    let mut options = TxOptions::new(label, Isolation::ReadCommitted);
    options.max_writes = env.limits.max_writes;
    let mut tx = env
        .backend
        .begin(env.catalog.current(), options)
        .await
        .map_err(|_| refused("the transaction could not be opened"))?;
    let attempts: EventAttempts = Arc::new(AtomicUsize::new(0));
    let ctx = ApplyCtx::new(env.owner.as_ref(), env.audit.as_ref(), at)
        .with_origin(env.origin.clone())
        .with_max_skew(env.max_skew);

    let mut set = AppliedSet::default();
    for record in &bundle.records {
        let step = if record.deleted {
            apply_delete(&mut tx, &ctx, &tenant, &source, record, at).await
        } else {
            apply_put(&mut tx, &ctx, &env, &attempts, &tenant, &source, record, at).await
        };
        match step {
            Ok(applied) => set.records.push(applied),
            Err(failure) => {
                let _ = tx.rollback().await;
                return Err(route(&env, failure, at).await);
            }
        }
    }
    if crashes(&env, Failpoint::AfterRecords) {
        return Err(refused("failpoint: after the records"));
    }
    if let Some(checkpoint) = &checkpoint
        && let Err(error) = checkpoint_in_tx(&mut tx, &tenant, checkpoint).await
    {
        let _ = tx.rollback().await;
        return Err(error);
    }
    if crashes(&env, Failpoint::BeforeCommit) {
        return Err(refused("failpoint: before the commit"));
    }
    tx.commit()
        .await
        .map_err(|_| refused("the commit failed; the bundle is not stored or is not known"))?;
    Ok(set)
}

/// Simulates a crash: the caller returns without committing, dropping the transaction.
fn crashes(env: &ApplyEnv, point: Failpoint) -> bool {
    env.failpoints.as_ref().is_some_and(|f| f.crash_at(point))
}

/// Holds a shape-refused record in the quarantine queue and names the refusal by its code.
async fn route(env: &ApplyEnv, failure: Failure, at: i64) -> OpError {
    let Failure { reason, record } = failure;
    let mut code = reason.code().to_owned();
    if is_shape_refusal(&reason)
        && let Some(record) = record
        && env.quarantine.hold(&record, &reason, at).await.is_err()
    {
        code = spot_replica_apply::codes::QUARANTINE_FAILED.to_owned();
    }
    refused(code)
}

/// The record as the apply path takes it, with its identity checked against the key it claims.
fn received_record(source: &SourceId, record: &BundleRecord) -> Result<ReceivedRecord, Reason> {
    let key = &record.key;
    if record.position == 0 {
        return Err(Reason::InvalidIdentifier {
            what: "position".to_owned(),
        });
    }
    let name = RecordName::new(&format!("{}/{}", key.entity, key.id)).map_err(|_| {
        Reason::InvalidIdentifier {
            what: "name".to_owned(),
        }
    })?;
    let payload = serde_json::to_string(&record.image).map_err(|_| Reason::InvalidPayload {
        why: "the image cannot be encoded".to_owned(),
    })?;
    let received =
        ReceivedRecord::from_payload(source, name, record.position, record.position, &payload)?;
    // The ledger is keyed by the claimed key: the image must be that record, not another.
    if received.document.entity.as_str() != key.entity
        || !received
            .document
            .id
            .to_string()
            .eq_ignore_ascii_case(&key.id)
    {
        return Err(Reason::InvalidIdentifier {
            what: "key".to_owned(),
        });
    }
    Ok(received)
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::result_large_err)]
async fn apply_put(
    tx: &mut PgTransaction,
    ctx: &ApplyCtx<'_>,
    env: &ApplyEnv,
    attempts: &EventAttempts,
    tenant: &str,
    source: &SourceId,
    record: &BundleRecord,
    at: i64,
) -> Result<AppliedRecord, Failure> {
    let received = received_record(source, record)?;
    // A later delete supersedes this image: applying it would bring the record back.
    let position = i64::try_from(record.position).map_err(|_| Reason::InvalidIdentifier {
        what: "position".to_owned(),
    })?;
    let deleted_later = tx
        .raw_query_opt(
            ledger::HAS_TOMBSTONE,
            &[&tenant, &record.key.entity, &record.key.id, &position],
            None,
        )
        .await
        .map_err(write_failed)?
        .is_some_and(|row| row.get::<_, bool>(0));
    if deleted_later {
        return Ok(AppliedRecord {
            key: record.key.clone(),
            version: record.position,
        });
    }
    let outcome = {
        let shared = BundleTx::new(&mut *tx, attempts.clone());
        apply_quarantining(shared, &received, ctx, env.quarantine.as_ref()).await
    };
    if attempts.load(Ordering::SeqCst) > 0 {
        return Err(Reason::WriteFailed {
            why: "the apply path tried to emit an event".to_owned(),
        }
        .into());
    }
    match outcome {
        ApplyOutcome::Stored | ApplyOutcome::Duplicate => {
            ledger_insert(
                tx,
                tenant,
                source,
                &record.key,
                record.position,
                record.position,
                "put",
                at,
            )
            .await?;
        }
        // A newer version is stored already: nothing to write, and the record is settled.
        ApplyOutcome::Stale => {}
        ApplyOutcome::Rejected(reason) => {
            return Err(Failure {
                reason,
                record: Some(received),
            });
        }
        _ => {
            return Err(Reason::WriteFailed {
                why: "the apply path answered an unknown outcome".to_owned(),
            }
            .into());
        }
    }
    Ok(AppliedRecord {
        key: record.key.clone(),
        version: record.position,
    })
}

#[allow(clippy::result_large_err)]
async fn apply_delete(
    tx: &mut PgTransaction,
    ctx: &ApplyCtx<'_>,
    tenant: &str,
    source: &SourceId,
    record: &BundleRecord,
    at: i64,
) -> Result<AppliedRecord, Failure> {
    let key = &record.key;
    let applied = AppliedRecord {
        key: key.clone(),
        version: record.position,
    };
    let entity = Name::new(&key.entity).map_err(|_| Reason::InvalidPayload {
        why: "the entity name is not valid".to_owned(),
    })?;
    let id = parse_id(&key.id)?;
    let version = i64::try_from(record.position).map_err(|_| Reason::InvalidIdentifier {
        what: "position".to_owned(),
    })?;
    // A record re-created at a later version is not deleted by an older tombstone.
    let newest = tx
        .raw_query_opt(ledger::MAX_PUT, &[&tenant, &key.entity, &key.id], None)
        .await
        .map_err(write_failed)?
        .and_then(|row| row.get::<_, Option<i64>>(0));
    if newest.is_some_and(|put| put > version) {
        return Ok(applied);
    }
    let stored = tx
        .load(&entity, id, RowLock::Update(Wait::Block))
        .await
        .map_err(write_failed)?;
    if let Some(stored) = stored {
        // Ownership is checked, not trusted: the sender may delete only what it may replicate.
        let name = RecordName::new(&format!("{}/{}", key.entity, key.id)).map_err(|_| {
            Reason::InvalidIdentifier {
                what: "name".to_owned(),
            }
        })?;
        let probe = ReceivedRecord {
            name,
            version: record.position,
            seq: record.position,
            source: source.clone(),
            topic: None,
            conversation: None,
            meta_version: 0,
            document: stored.data.clone(),
            entries: Vec::new(),
        };
        ctx.owner.allow_replica(source, &probe, at)?;
        tx.delete(&entity, id, stored.version)
            .await
            .map_err(write_failed)?;
    }
    ledger_insert(
        tx,
        tenant,
        source,
        key,
        record.position,
        record.position,
        "delete",
        at,
    )
    .await?;
    Ok(applied)
}

fn parse_id(text: &str) -> Result<RecordId, Reason> {
    let invalid = || Reason::InvalidId {
        id: "<not a record id>".to_owned(),
    };
    let uuid = uuid::Uuid::parse_str(text).map_err(|_| invalid())?;
    RecordId::from_bytes(*uuid.as_bytes()).map_err(|_| invalid())
}

#[allow(clippy::too_many_arguments)]
async fn ledger_insert(
    tx: &mut PgTransaction,
    tenant: &str,
    source: &SourceId,
    key: &RecordKey,
    version: u64,
    position: u64,
    kind: &'static str,
    at: i64,
) -> Result<(), Reason> {
    let invalid = || Reason::InvalidIdentifier {
        what: "position".to_owned(),
    };
    let name = format!("{}/{}", key.entity, key.id);
    let version = i64::try_from(version).map_err(|_| invalid())?;
    let position = i64::try_from(position).map_err(|_| invalid())?;
    tx.raw_execute(
        ledger::INSERT,
        &[
            &tenant,
            &source.as_str(),
            &key.entity,
            &key.id,
            &name,
            &version,
            &position,
            &kind,
            &at,
        ],
    )
    .await
    .map_err(write_failed)?;
    Ok(())
}

/// Advances the checkpoint in the bundle's transaction. The ledger rows of this bundle are
/// already written in it, so a checkpoint is refused when it is ahead of what the ledger holds.
async fn checkpoint_in_tx(
    tx: &mut PgTransaction,
    tenant: &str,
    checkpoint: &Checkpoint,
) -> Result<(), OpError> {
    let seq = i64::try_from(checkpoint.last_applied_seq)
        .map_err(|_| OpError::Refused(PgNodeError::CheckpointAhead))?;
    let source = checkpoint.source.as_str();
    let highest = tx
        .raw_query_opt(ledger::MAX_POSITION, &[&tenant, &source], None)
        .await
        .map_err(|_| refused("the ledger could not be read"))?
        .and_then(|row| row.get::<_, Option<i64>>(0))
        .unwrap_or(0);
    if seq > highest {
        return Err(OpError::Refused(PgNodeError::CheckpointAhead));
    }
    tx.raw_execute(checkpoints::UPSERT, &[&tenant, &source, &seq])
        .await
        .map_err(|_| refused("the checkpoint could not be written"))?;
    Ok(())
}
