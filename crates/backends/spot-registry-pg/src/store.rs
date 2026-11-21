//! [`PgIndexStore`]: the `IndexStore` over PostgreSQL through the shared bridge.

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_pg_bridge::{Bridge, PooledConn};
use spot_registry::{
    AppName, Caller, Digest, IndexStore, Ownership, Publisher, PublisherStatus, Release, Removal,
    Result, SearchPage, SearchQuery, TransferRecord, Version, Visibility,
};
use tokio_postgres::IsolationLevel;

use crate::codec::{RELEASE_COLUMNS, release_from_row};
use crate::error::{SetupError, corrupt, from_bridge, query_failed};
use crate::{ops, schema, search};

/// Deadline of a read.
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// Deadline of a write, one transaction.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of a catalogue search (candidate read).
pub const SEARCH_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the table creation.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

/// The PostgreSQL registry index. Build it with [`PgIndexStore::new`], create the tables once with
/// [`PgIndexStore::prepare`], then give it to `Registry` as the `IndexStore`.
///
/// **Use it only from a control-plane process, a CLI command or a job worker, never from a
/// request handler**, and never inside a transaction an async store holds (ADR-082 section 2).
pub struct PgIndexStore {
    bridge: Arc<Bridge>,
}

impl std::fmt::Debug for PgIndexStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgIndexStore").finish_non_exhaustive()
    }
}

impl PgIndexStore {
    /// Builds the store on the process's shared `bridge` over `pool`.
    ///
    /// `pool` is the pool the bridge was built on; the bridge configuration is validated against
    /// its size again here. See the README for the sizing.
    ///
    /// # Errors
    /// `registry_pg.bad_setup` when the bridge configuration cannot hold against the pool, or
    /// when the caller is inside a current-thread tokio runtime.
    pub fn new(bridge: Arc<Bridge>, pool: &Pool) -> std::result::Result<Self, SetupError> {
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread
        {
            return Err(SetupError::bad_setup(
                "the registry index cannot be built inside a current-thread runtime",
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

    /// Creates the registry tables if they are missing, in one transaction under an advisory lock.
    ///
    /// # Errors
    /// `registry_pg.migration_failed` with a fixed sentence.
    pub fn prepare(&self) -> std::result::Result<(), SetupError> {
        self.bridge
            .run(PREPARE_DEADLINE, |mut conn: PooledConn| {
                Box::pin(async move { schema::prepare(&mut conn).await.map_err(|_| ()) })
            })
            .map_err(|_| SetupError::migration("the registry tables could not be created"))
    }
}

macro_rules! write_tx {
    ($self:expr, $deadline:expr, |$tx:ident| $body:expr) => {
        from_bridge($self.bridge.run($deadline, move |mut conn: PooledConn| {
            Box::pin(async move {
                let $tx = conn.transaction().await.map_err(query_failed)?;
                let out = $body.await?;
                $tx.commit().await.map_err(query_failed)?;
                Ok(out)
            })
        }))
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

impl IndexStore for PgIndexStore {
    fn preflight(&self, release: &Release) -> Result<()> {
        let release = release.clone();
        read_tx!(self, |tx| ops::preflight(&tx, &release))
    }

    fn commit(
        &self,
        release: Release,
        publisher: Publisher,
    ) -> Result<spot_registry::PublishOutcome> {
        let committed = write_tx!(self, WRITE_DEADLINE, |tx| ops::commit(
            &tx, release, publisher
        ))?;
        Ok(spot_registry::PublishOutcome {
            release: committed.release,
            inserted: committed.inserted,
        })
    }

    fn ownership(&self, app: &AppName) -> Result<Option<Ownership>> {
        let app = app.clone();
        read_tx!(self, |tx| ops::ownership(&tx, &app))
    }

    fn publisher(&self, id: &str) -> Result<Option<Publisher>> {
        let id = id.to_owned();
        read_tx!(self, |tx| ops::publisher(&tx, &id))
    }

    fn set_publisher_status(&self, id: &str, status: PublisherStatus) -> Result<()> {
        let id = id.to_owned();
        write_tx!(self, WRITE_DEADLINE, |tx| ops::set_publisher_status(
            &tx, &id, &status
        ))
    }

    fn releases(
        &self,
        app: &AppName,
        visibility: &Visibility,
        caller: Option<&Caller>,
    ) -> Result<Vec<Release>> {
        if visibility.include_inactive && caller.is_none() {
            return Err(spot_registry::RegistryError::new(
                "registry.unauthorized",
                "inactive listings require a caller",
            ));
        }
        let app = app.clone();
        let include_pre = visibility.include_pre;
        let include_inactive = visibility.include_inactive;
        let caller = caller.map(|c| c.publisher().to_owned());
        let mut rows = read_tx!(self, |tx| async {
            let sql = format!(
                "SELECT {RELEASE_COLUMNS} FROM spot_control.spot_registry_release WHERE app = $1"
            );
            tx.query(&sql, &[&app.as_str()])
                .await
                .map_err(query_failed)?
                .iter()
                .map(release_from_row)
                .collect::<Result<Vec<_>>>()
        })?;
        rows.retain(|r| {
            (include_pre || r.version.pre.is_empty())
                && (r.active()
                    || (include_inactive && caller.as_deref().is_some_and(|c| c == r.publisher)))
        });
        rows.sort_by(|a, b| b.version.cmp(&a.version));
        Ok(rows)
    }

    fn by_digest(&self, digest: Digest) -> Result<Option<Release>> {
        let hex = digest.to_hex();
        read_tx!(self, |tx| async {
            let sql = format!(
                "SELECT {RELEASE_COLUMNS} FROM spot_control.spot_registry_release WHERE digest = $1"
            );
            tx.query_opt(&sql, &[&hex])
                .await
                .map_err(query_failed)?
                .as_ref()
                .map(release_from_row)
                .transpose()
        })
    }

    fn release(&self, app: &AppName, version: &Version) -> Result<Option<Release>> {
        let (app, version) = (app.clone(), version.clone());
        read_tx!(self, |tx| ops::release_by(&tx, &app, &version))
    }

    fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        search::validate(query)?;
        let probe = query.clone();
        let (rows, revision) = from_bridge(self.bridge.run(
            SEARCH_DEADLINE,
            move |mut conn: PooledConn| {
                Box::pin(async move {
                    let tx = conn
                        .build_transaction()
                        .isolation_level(IsolationLevel::RepeatableRead)
                        .read_only(true)
                        .start()
                        .await
                        .map_err(query_failed)?;
                    search::candidates(&tx, &probe).await
                })
            },
        ))?;
        search::rank_and_page(rows, query, revision)
    }

    fn remove(
        &self,
        app: &AppName,
        version: &Version,
        by: &str,
        removal: Removal,
        withdraw: bool,
    ) -> Result<Option<Release>> {
        let (app, version, by) = (app.clone(), version.clone(), by.to_owned());
        write_tx!(self, WRITE_DEADLINE, |tx| ops::remove(
            &tx, &app, &version, &by, removal, withdraw
        ))
    }

    fn transfer(&self, transfer: TransferRecord, recipient: Publisher) -> Result<()> {
        write_tx!(self, WRITE_DEADLINE, |tx| ops::transfer(
            &tx, transfer, recipient
        ))
    }

    fn transfers(&self, app: &AppName) -> Result<Vec<TransferRecord>> {
        let app = app.clone();
        read_tx!(self, |tx| ops::transfers(&tx, &app))
    }

    fn digests(&self) -> Result<Vec<Digest>> {
        read_tx!(self, |tx| async {
            tx.query(
                "SELECT digest FROM spot_control.spot_registry_release ORDER BY digest COLLATE \"C\"",
                &[],
            )
            .await
            .map_err(query_failed)?
            .iter()
            .map(|row| {
                let hex: String = row.try_get(0).map_err(query_failed)?;
                hex.parse::<Digest>().map_err(|_| corrupt())
            })
            .collect()
        })
    }

    fn describe(
        &self,
        app: &AppName,
        by: &str,
        description: String,
        category: String,
    ) -> Result<()> {
        let (app, by) = (app.clone(), by.to_owned());
        write_tx!(self, WRITE_DEADLINE, |tx| ops::describe(
            &tx,
            &app,
            &by,
            description,
            category
        ))
    }
}
