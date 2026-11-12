//! [`PgStoreState`]: the `StoreStateStore` over PostgreSQL through the shared bridge.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_pg_bridge::{Bridge, PooledConn};
use spot_registry::{AppName, Version};
use spot_store_state::{
    AuditEntry, AuditRecord, Begin, BundleRevocation, Fingerprint, IssuedList, KeyHash, KeyInfo,
    KeyRecord, KeyRevocation, ListingRecord, ListingState, MAX_AUDIT_PAGE, MAX_CANDIDATES,
    MAX_LABEL, ReviewRecord, ReviewUpdate, StoreStateError, StoreStateStore, check_id,
};
use tokio_postgres::IsolationLevel;

use crate::error::{SetupError, from_bridge, query_failed};
use crate::{ops, schema};

/// Deadline of a read.
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// Deadline of a write, one transaction.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the table creation.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

type Result<T> = std::result::Result<T, StoreStateError>;

/// The PostgreSQL store state. Build it with [`PgStoreState::new`], create the tables once with
/// [`PgStoreState::prepare`], then give it to the store server as the `StoreStateStore`.
///
/// **Call it only from a plain OS thread** of a control-plane process (a dedicated worker), never
/// from a tokio runtime thread, `spawn_blocking` included (ADR-082 section 2): the bridge refuses
/// such a call with `pg_bridge.wrong_thread` and nothing is changed.
pub struct PgStoreState {
    bridge: Arc<Bridge>,
}

impl std::fmt::Debug for PgStoreState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgStoreState").finish_non_exhaustive()
    }
}

impl PgStoreState {
    /// Builds the store on the process's shared `bridge` over `pool`.
    ///
    /// # Errors
    /// `store_state.bad_setup` when the bridge configuration cannot hold against the pool, or
    /// when the caller is inside a current-thread tokio runtime.
    pub fn new(bridge: Arc<Bridge>, pool: &Pool) -> std::result::Result<Self, SetupError> {
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread
        {
            return Err(SetupError::bad_setup(
                "the store state cannot be built inside a current-thread runtime",
            ));
        }
        bridge
            .config()
            .validate(pool.status().max_size)
            .map_err(|_| {
                SetupError::bad_setup("the bridge configuration does not hold against the pool")
            })?;
        Ok(Self { bridge })
    }

    /// Creates the tables if they are missing, in one transaction under an advisory lock. Safe to
    /// call on every start and from several processes at once.
    ///
    /// # Errors
    /// `store_state.migration_failed` with a fixed sentence.
    pub fn prepare(&self) -> std::result::Result<(), SetupError> {
        self.bridge
            .run(PREPARE_DEADLINE, |mut conn: PooledConn| {
                Box::pin(async move { schema::prepare(&mut conn).await.map_err(|_| ()) })
            })
            .map_err(|_| SetupError::migration("the store state tables could not be created"))
    }
}

macro_rules! write_tx {
    ($self:expr, |$tx:ident| $body:expr) => {
        from_bridge(
            $self
                .bridge
                .run(WRITE_DEADLINE, move |mut conn: PooledConn| {
                    Box::pin(async move {
                        let $tx = conn.transaction().await.map_err(query_failed)?;
                        let out = $body.await?;
                        $tx.commit().await.map_err(query_failed)?;
                        Ok(out)
                    })
                }),
        )
    };
}

macro_rules! read_tx {
    ($self:expr, |$tx:ident| $body:expr) => {
        from_bridge(
            $self
                .bridge
                .run(READ_DEADLINE, move |mut conn: PooledConn| {
                    Box::pin(async move {
                        let $tx = conn
                            .build_transaction()
                            .isolation_level(IsolationLevel::RepeatableRead)
                            .read_only(true)
                            .start()
                            .await
                            .map_err(query_failed)?;
                        $body.await
                    })
                }),
        )
    };
}

impl StoreStateStore for PgStoreState {
    fn issue_publisher_key(
        &self,
        publisher_id: &str,
        key_hash: KeyHash,
        label: &str,
        now: i64,
    ) -> Result<u64> {
        check_id(publisher_id, "publisher id")?;
        if label.is_empty() || label.len() > MAX_LABEL {
            return Err(StoreStateError::too_large("label"));
        }
        let (publisher_id, label) = (publisher_id.to_owned(), label.to_owned());
        write_tx!(self, |tx| ops::issue_key(
            &tx,
            &publisher_id,
            &key_hash,
            &label,
            now
        ))
    }

    fn lookup_publisher_key(&self, key_hash: &KeyHash) -> Result<Option<KeyRecord>> {
        let key_hash = *key_hash;
        read_tx!(self, |tx| ops::lookup_key(&tx, &key_hash))
    }

    fn revoke_publisher_key(&self, key_id: u64, now: i64) -> Result<()> {
        write_tx!(self, |tx| ops::revoke_key_row(&tx, key_id, now))
    }

    fn list_publisher_keys(&self, publisher_id: &str) -> Result<Vec<KeyInfo>> {
        let publisher_id = publisher_id.to_owned();
        read_tx!(self, |tx| ops::list_keys(&tx, &publisher_id))
    }

    fn create_review(&self, record: &ReviewRecord) -> Result<bool> {
        record.validate()?;
        let record = record.clone();
        write_tx!(self, |tx| ops::create_review(&tx, &record))
    }

    fn get_review(&self, review_id: &str) -> Result<Option<ReviewRecord>> {
        let review_id = review_id.to_owned();
        read_tx!(self, |tx| ops::get_review(&tx, &review_id))
    }

    fn update_review(&self, review_id: &str, update: &ReviewUpdate) -> Result<()> {
        update.validate()?;
        let (review_id, update) = (review_id.to_owned(), update.clone());
        write_tx!(self, |tx| ops::update_review(&tx, &review_id, &update))
    }

    fn set_listing(
        &self,
        app: &AppName,
        version: &Version,
        state: ListingState,
        by: &str,
        now: i64,
    ) -> Result<()> {
        check_id(by, "actor")?;
        let (app, version, by) = (app.clone(), version.clone(), by.to_owned());
        write_tx!(self, |tx| ops::set_listing(
            &tx, &app, &version, state, &by, now
        ))
    }

    fn listing(&self, app: &AppName, version: &Version) -> Result<Option<ListingRecord>> {
        let (app, version) = (app.clone(), version.clone());
        read_tx!(self, |tx| ops::listing(&tx, &app, &version))
    }

    fn listed_among(
        &self,
        candidates: &[(AppName, Version)],
    ) -> Result<BTreeSet<(AppName, Version)>> {
        if candidates.len() > MAX_CANDIDATES {
            return Err(StoreStateError::too_large("candidate list"));
        }
        let candidates = candidates.to_vec();
        read_tx!(self, |tx| ops::listed_among(&tx, &candidates))
    }

    fn reviews_for(&self, app: &AppName, version: &Version) -> Result<Vec<ReviewRecord>> {
        let (app, version) = (app.clone(), version.clone());
        read_tx!(self, |tx| ops::reviews_for(&tx, &app, &version))
    }

    fn idempotency_begin(
        &self,
        publisher_id: &str,
        key: &str,
        fingerprint: &Fingerprint,
        now: i64,
    ) -> Result<Begin> {
        check_id(publisher_id, "publisher id")?;
        check_id(key, "idempotency key")?;
        let (publisher_id, key, fingerprint) =
            (publisher_id.to_owned(), key.to_owned(), *fingerprint);
        write_tx!(self, |tx| ops::idempotency_begin(
            &tx,
            &publisher_id,
            &key,
            &fingerprint,
            now
        ))
    }

    fn idempotency_finish(&self, publisher_id: &str, key: &str, review_id: &str) -> Result<()> {
        let (publisher_id, key, review_id) = (
            publisher_id.to_owned(),
            key.to_owned(),
            review_id.to_owned(),
        );
        write_tx!(self, |tx| ops::idempotency_finish(
            &tx,
            &publisher_id,
            &key,
            &review_id
        ))
    }

    fn idempotency_prune(&self, older_than: i64) -> Result<u64> {
        write_tx!(self, |tx| ops::idempotency_prune(&tx, older_than))
    }

    fn revoke_key(&self, revocation: &KeyRevocation) -> Result<bool> {
        revocation.validate()?;
        let revocation = revocation.clone();
        write_tx!(self, |tx| ops::revoke_signing_key(&tx, &revocation))
    }

    fn revoke_bundle(&self, revocation: &BundleRevocation) -> Result<bool> {
        revocation.validate()?;
        let revocation = revocation.clone();
        write_tx!(self, |tx| ops::revoke_bundle(&tx, &revocation))
    }

    fn revoked_keys(&self) -> Result<Vec<KeyRevocation>> {
        read_tx!(self, |tx| ops::revoked_keys(&tx))
    }

    fn revoked_bundles(&self) -> Result<Vec<BundleRevocation>> {
        read_tx!(self, |tx| ops::revoked_bundles(&tx))
    }

    fn next_generation(&self, now: i64) -> Result<u64> {
        write_tx!(self, |tx| ops::next_generation(&tx, now))
    }

    fn record_issued_list(&self, list: &IssuedList) -> Result<()> {
        list.validate()?;
        let list = list.clone();
        write_tx!(self, |tx| ops::record_issued_list(&tx, &list))
    }

    fn last_issued_list(&self) -> Result<Option<IssuedList>> {
        read_tx!(self, |tx| ops::last_issued_list(&tx))
    }

    fn append_audit(&self, entry: &AuditEntry) -> Result<u64> {
        entry.validate()?;
        let entry = entry.clone();
        write_tx!(self, |tx| ops::append_audit(&tx, &entry))
    }

    fn read_audit(&self, after_id: u64, limit: u32) -> Result<Vec<AuditRecord>> {
        let limit = i64::from(limit.min(MAX_AUDIT_PAGE));
        read_tx!(self, |tx| ops::read_audit(&tx, after_id, limit))
    }
}
