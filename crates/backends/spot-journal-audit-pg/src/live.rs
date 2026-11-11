//! The live side: the tenant's real database, read-only, as of one recorded position.
//!
//! [`PgLiveSide::open`] begins **one** `REPEATABLE READ` **read-only** transaction and keeps it
//! for the life of the side, reads the change feed's head inside it and answers every later call
//! from the same snapshot (ADR-061 s8). A write that commits while the audit runs is therefore
//! invisible to it: the snapshot is fixed at the head, and `as_of` may only be that head, because
//! the live database cannot be read in the past.
//!
//! Read-only is the transaction's own mode (`BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY` and
//! an explicit `SET TRANSACTION READ ONLY`), not a convention: a statement that writes fails with
//! `25006`, which [`crate::error::READ_ONLY`] names. This module also contains no statement that
//! writes, which its unit test pins.
//!
//! The transaction is held by [`crate::session`]: one bridge call stays open for the side's whole
//! life (a call's deadline is the bridge's `max_deadline`, so the host must size it for its
//! longest audit — see the README).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use spot_journal_audit::{DerivedEntities, Fingerprint, Position};
use spot_journal_audit_run::{LiveSide, Summaries};
use spot_pg_bridge::{BoxFuture, Bridge, PooledConn};
use spot_storage_contract::{Catalog, Name};

use crate::error::{
    PgSideError, as_of_not_head, feed_missing, query_failed, session_lost, tenant_mismatch,
};
use crate::pager::{Pages, fingerprints as read_fingerprints, page as read_page};
use crate::session::{HeadReport, Request, Requests, Session, next_request};
use crate::sql::{FingerprintQuery, Scope, SharedScope};

/// `BEGIN` of the side's transaction: repeatable read, read only, stated in the one statement so
/// PostgreSQL fixes the snapshot at the first read.
pub(crate) const BEGIN_SQL: &str = "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// The transaction mode spelled out again, as ADR-061 s8 asks for it by name.
pub(crate) const READ_ONLY_SQL: &str = "SET TRANSACTION READ ONLY";

/// The change feed's head: its generation and the last position it published.
pub(crate) const HEAD_SQL: &str = "SELECT (SELECT value::bigint FROM spot_meta WHERE key = \
     'feed_generation'), last_seq FROM spot_feed_state WHERE log = 'changes'";

/// Ends the snapshot; a read-only transaction has nothing to keep.
pub(crate) const ROLLBACK_SQL: &str = "ROLLBACK";

/// Rows per page when the caller does not choose.
pub const DEFAULT_PAGE_SIZE: i64 = 256;

/// A deadline long enough for any audit; the bridge clamps it to its own `max_deadline`.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// The live database of one tenant, as the audit sees it: one held read-only snapshot.
pub struct PgLiveSide {
    tenant: String,
    head: Position,
    session: Session,
    catalog: Arc<Catalog>,
    page_size: i64,
    derived: DerivedEntities,
    fingerprints: Vec<FingerprintQuery>,
    plan: SharedScope,
    scope: Arc<Scope>,
}

impl fmt::Debug for PgLiveSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgLiveSide")
            .field("tenant", &self.tenant)
            .field("head", &self.head)
            .field("entities", &self.scope.entities().len())
            .field("page_size", &self.scope.page_size())
            .finish_non_exhaustive()
    }
}

impl PgLiveSide {
    /// Opens the live side of `tenant` over `bridge`'s pool and holds its snapshot.
    ///
    /// The head is the change feed's last published position, read inside the transaction, and
    /// every later answer comes from that snapshot: records written afterwards do not count.
    ///
    /// # Errors
    ///
    /// [`crate::error::TENANT_MISMATCH`] when `tenant` is not a storage name,
    /// [`crate::error::OPTIONS_INVALID`] when the catalogue declares no document entity,
    /// [`crate::error::FEED_MISSING`] when the change feed is not installed, and
    /// [`crate::error::SESSION_FAILED`] when the session cannot be opened.
    pub fn open(
        bridge: Arc<Bridge>,
        tenant: &str,
        catalog: Arc<Catalog>,
    ) -> Result<Self, PgSideError> {
        Self::open_with(bridge, tenant, catalog, DEFAULT_PAGE_SIZE, DEFAULT_DEADLINE)
    }

    /// As [`Self::open`], with the page size and the deadline the host wants (the deadline is
    /// clamped by the bridge's `max_deadline`, and must outlast the audit).
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_with(
        bridge: Arc<Bridge>,
        tenant: &str,
        catalog: Arc<Catalog>,
        page_size: i64,
        deadline: Duration,
    ) -> Result<Self, PgSideError> {
        check_tenant(tenant)?;
        let derived = DerivedEntities::none();
        let scope = Arc::new(Scope::new(&catalog, None, &derived, page_size, Vec::new())?);
        let plan = SharedScope::new(Arc::clone(&scope));
        let (session, head) = Self::start(&bridge, deadline, plan.clone())?;
        Ok(Self {
            tenant: tenant.to_owned(),
            head,
            session,
            catalog,
            page_size,
            derived,
            fingerprints: Vec::new(),
            plan,
            scope,
        })
    }

    /// Flags the entities the catalogue calls derived, which the audit skips (ADR-061 s3).
    ///
    /// # Errors
    ///
    /// [`crate::error::OPTIONS_INVALID`] when no document entity is left to read.
    pub fn with_derived(mut self, derived: DerivedEntities) -> Result<Self, PgSideError> {
        let scope = Arc::new(self.read_plan(&derived, &self.fingerprints)?);
        self.plan.set(Arc::clone(&scope));
        self.scope = scope;
        self.derived = derived;
        Ok(self)
    }

    /// Sets the declared financial fingerprints this side answers (WP-MIG-06). With none, the
    /// side answers an empty list and the compare covers records only.
    ///
    /// # Errors
    ///
    /// [`crate::error::OPTIONS_INVALID`] when a declaration is unusable.
    pub fn with_fingerprints(
        mut self,
        queries: Vec<FingerprintQuery>,
    ) -> Result<Self, PgSideError> {
        let scope = Arc::new(self.read_plan(&self.derived, &queries)?);
        self.plan.set(Arc::clone(&scope));
        self.scope = scope;
        self.fingerprints = queries;
        Ok(self)
    }

    /// The read plan of this side's settings.
    fn read_plan(
        &self,
        derived: &DerivedEntities,
        fingerprints: &[FingerprintQuery],
    ) -> Result<Scope, PgSideError> {
        Scope::new(
            &self.catalog,
            None,
            derived,
            self.page_size,
            fingerprints.to_vec(),
        )
    }

    /// Runs one statement that writes on the side's own connection and reports the SQLSTATE of
    /// its refusal: the snapshot's transaction is read-only, so a write cannot pass (a test hook).
    ///
    /// # Errors
    ///
    /// [`crate::error::SESSION_FAILED`] when the held session is gone.
    #[cfg(feature = "testing")]
    pub fn probe_write(&self) -> Result<String, PgSideError> {
        self.session
            .probe("CREATE TEMP TABLE spot_journal_audit_probe (id integer)")
    }

    /// The head position this side is pinned to.
    #[must_use]
    pub fn position(&self) -> Position {
        self.head
    }

    /// How many pages this side has been asked for since it opened.
    #[must_use]
    pub fn pages_read(&self) -> usize {
        self.session.pages_read()
    }

    /// The first failure recorded since the side opened, if any: a stream that ended early
    /// (the trait's iterator cannot carry an error) leaves its reason here.
    #[must_use]
    pub fn failure(&self) -> Option<PgSideError> {
        self.session.failure()
    }

    /// Every record summary as of `as_of`, for `tenant`, sorted by entity then id.
    ///
    /// # Errors
    ///
    /// [`crate::error::TENANT_MISMATCH`] for another tenant, [`crate::error::AS_OF_NOT_HEAD`]
    /// for any position other than the recorded head.
    pub fn summaries_for(
        &self,
        tenant: &str,
        as_of: Position,
    ) -> Result<Summaries<'_>, PgSideError> {
        self.check(tenant, as_of)?;
        Ok(Box::new(Pages::new(&self.session, &self.scope)))
    }

    /// The declared fingerprints as of `as_of`, for `tenant`.
    ///
    /// # Errors
    ///
    /// As [`Self::summaries_for`], plus [`crate::error::QUERY_FAILED`] when a sum cannot be read.
    pub fn fingerprints_for(
        &self,
        tenant: &str,
        as_of: Position,
    ) -> Result<Vec<Fingerprint>, PgSideError> {
        self.check(tenant, as_of)?;
        self.session.fingerprints()
    }

    /// The tenant and position checks both answers share.
    fn check(&self, tenant: &str, as_of: Position) -> Result<(), PgSideError> {
        if tenant != self.tenant {
            return Err(tenant_mismatch("live"));
        }
        if as_of != self.head {
            return Err(as_of_not_head(self.head.seq));
        }
        Ok(())
    }

    /// Starts the held session over a scope and returns it with the head it read.
    fn start(
        bridge: &Arc<Bridge>,
        deadline: Duration,
        plan: SharedScope,
    ) -> Result<(Session, Position), PgSideError> {
        Session::open(bridge, deadline, move |conn, requests, head| {
            serve(plan, conn, requests, head)
        })
    }
}

/// The worker of the live side: begin the snapshot, report the head, serve until closed.
pub(crate) fn serve(
    plan: SharedScope,
    conn: PooledConn,
    requests: Requests,
    head: HeadReport<Position>,
) -> BoxFuture<'static, Result<(), PgSideError>> {
    Box::pin(async move {
        conn.batch_execute(BEGIN_SQL)
            .await
            .map_err(|error| session_lost(&error))?;
        conn.batch_execute(READ_ONLY_SQL)
            .await
            .map_err(|error| session_lost(&error))?;
        let report = read_head(&conn).await;
        let _ = head.send(report.clone());
        report?;
        let mut requests = requests;
        while let Some(request) = next_request(&mut requests).await {
            match request {
                Request::Page {
                    entity,
                    after,
                    limit,
                    reply,
                } => {
                    let scope = plan.get();
                    let result = match scope.entity(&entity) {
                        Some(found) => {
                            read_page(&conn, &scope, found, after.as_deref(), limit).await
                        }
                        None => Err(crate::error::options_invalid(
                            "a page was asked for an entity the scope does not read",
                        )),
                    };
                    reply.send(result);
                }
                Request::Fingerprints { reply } => {
                    let scope = plan.get();
                    reply.send(read_fingerprints(&conn, &scope).await);
                }
                // The live side answers reads only: it holds a read-only transaction and stores
                // nothing, so a rebuild's write cannot reach it.
                Request::ApplyImage { reply, .. }
                | Request::ApplyDelete { reply, .. }
                | Request::Truncate { reply }
                | Request::Discard { reply } => reply.send(Err(crate::error::error(
                    crate::error::OPTIONS_INVALID,
                    "the live side is read-only and stores nothing",
                ))),
                Request::Close { reply } => {
                    reply.send(Ok(()));
                    break;
                }
                #[cfg(feature = "testing")]
                Request::Probe { sql, reply } => {
                    reply.send(probe(&conn, &sql).await);
                }
            }
        }
        let _ = conn.batch_execute(ROLLBACK_SQL).await;
        Ok(())
    })
}

/// The change feed's head as a [`Position`]: its generation and its last published sequence.
async fn read_head(conn: &PooledConn) -> Result<Position, PgSideError> {
    let row = conn
        .query_opt(HEAD_SQL, &[])
        .await
        .map_err(|error| query_failed(&error))?
        .ok_or_else(|| feed_missing("the change feed has no state row in this database"))?;
    let generation: Option<i64> = row
        .try_get(0)
        .map_err(|_| feed_missing("the generation of the change feed is not a number"))?;
    let seq: Option<i64> = row
        .try_get(1)
        .map_err(|_| feed_missing("the last position of the change feed is not a number"))?;
    let generation = generation.and_then(|value| u64::try_from(value).ok());
    let seq = seq.and_then(|value| u64::try_from(value).ok());
    match (generation, seq) {
        (Some(generation), Some(seq)) => Ok(Position::new(generation, seq)),
        _ => Err(feed_missing(
            "the change feed's generation or position is missing",
        )),
    }
}

/// Runs `sql` and reports the SQLSTATE of its refusal, or an empty string when it went through.
#[cfg(feature = "testing")]
async fn probe(conn: &PooledConn, sql: &str) -> Result<String, PgSideError> {
    match conn.batch_execute(sql).await {
        Ok(()) => Ok(String::new()),
        Err(error) => Ok(error
            .code()
            .map(|state| state.code().to_owned())
            .unwrap_or_default()),
    }
}

/// Refuses a tenant that is not a storage name.
pub(crate) fn check_tenant(tenant: &str) -> Result<(), PgSideError> {
    Name::new(tenant)
        .map(|_| ())
        .map_err(|_| tenant_mismatch("live"))
}

impl LiveSide for PgLiveSide {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn head(&self) -> Position {
        self.head
    }

    /// The summaries as of `as_of`. A position other than the head is refused; the trait's stream
    /// cannot carry the refusal, so the side records it ([`Self::failure`]) and yields nothing.
    fn summaries(&self, as_of: Position) -> Summaries<'_> {
        match self.summaries_for(self.tenant.as_str(), as_of) {
            Ok(pages) => pages,
            Err(error) => {
                self.session.record(&error);
                Box::new(std::iter::empty())
            }
        }
    }

    /// The fingerprints as of `as_of`, with the same refusal rule as [`Self::summaries`].
    fn fingerprints(&self, as_of: Position) -> Vec<Fingerprint> {
        match self.fingerprints_for(self.tenant.as_str(), as_of) {
            Ok(fingerprints) => fingerprints,
            Err(error) => {
                self.session.record(&error);
                Vec::new()
            }
        }
    }
}
