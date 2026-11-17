//! [`PgPromotionStore`]: the `PromotionStore` over the table of [`crate::schema`].

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use serde_json::Value;
use spot_custom_bundle::{Limits, SectionRegistry};
use spot_migration_audit::RunId;
use spot_pg_bridge::Bridge;
use spot_promotion::{Promotion, PromotionError, PromotionStore, Snapshot};
use spot_tenancy::TenantId;
use tokio_postgres::IsolationLevel;

use crate::error::{row_invalid, store_failed, store_note};
use crate::schema;

/// The deadline of `load`: one indexed read.
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// The deadline of `insert` and `save`: one or two statements in a transaction.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// The deadline of `list`, which reads up to [`LIST_CEILING`] rows in pages.
const LIST_DEADLINE: Duration = Duration::from_secs(30);
/// The deadline of the schema preparation.
const SCHEMA_DEADLINE: Duration = Duration::from_secs(30);

/// Rows read per statement by `list`.
pub const LIST_PAGE: usize = 100;
/// The most promotions `list` returns. More than this fails with `promotion.store_failed`
/// rather than returning a truncated answer.
pub const LIST_CEILING: usize = 5_000;

/// A stored row as read: id, revision and document.
type Row = (String, i64, Value);

/// The promotion store over PostgreSQL, for one tenant. Use it from a control-plane process, a
/// CLI command or a job worker; never from a request handler (see the crate docs).
pub struct PgPromotionStore {
    bridge: Arc<Bridge>,
    tenant: TenantId,
    limits: Limits,
    registry: SectionRegistry,
}

impl PgPromotionStore {
    /// A store for `tenant` (the target environment's tenant) that reaches the database through
    /// `bridge`, the process's shared bridge, built on `pool`. A stored pre-image is opened
    /// with the default limits and the built-in sections; see
    /// [`with_pre_image_opening`](Self::with_pre_image_opening).
    ///
    /// # Errors
    /// `promotion.store_failed` when the bridge configuration does not fit the pool's size, or
    /// when called on a current-thread tokio runtime (the bridge's blocking calls could never
    /// run there).
    pub fn new(bridge: Arc<Bridge>, pool: &Pool, tenant: TenantId) -> Result<Self, PromotionError> {
        bridge
            .config()
            .validate(pool.status().max_size)
            .map_err(|_| {
                store_note("the bridge configuration does not fit the pool size; see the README")
            })?;
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread
        {
            return Err(store_note(
                "the store cannot be built on a current-thread runtime; build it on a plain thread",
            ));
        }
        Ok(Self {
            bridge,
            tenant,
            limits: Limits::default(),
            registry: SectionRegistry::new(),
        })
    }

    /// Opens stored pre-images with `limits` and `registry`: use the ones the bundle applier
    /// captured them with.
    #[must_use]
    pub fn with_pre_image_opening(mut self, limits: Limits, registry: SectionRegistry) -> Self {
        self.limits = limits;
        self.registry = registry;
        self
    }

    /// Creates the schema and table if they are missing (idempotent, safe from several
    /// processes at once). A host calls it once at start-up.
    ///
    /// # Errors
    /// `promotion.store_failed`.
    pub fn ensure_schema(&self) -> Result<(), PromotionError> {
        schema::ensure(&self.bridge, SCHEMA_DEADLINE)
    }

    /// The tenant this store is scoped to.
    #[must_use]
    pub fn tenant(&self) -> TenantId {
        self.tenant
    }

    /// Refuses a promotion whose target is another tenant.
    fn check_tenant(&self, promotion: &Promotion) -> Result<(), PromotionError> {
        let found = promotion.to_env().tenant;
        if found == self.tenant {
            return Ok(());
        }
        Err(PromotionError::EnvironmentMismatch {
            expected: self.tenant.to_string(),
            found: found.to_string(),
        })
    }

    /// The promotion a row holds, which must restore and carry the row's id and revision.
    fn decode(&self, id: &str, revision: i64, doc: Value) -> Result<Promotion, PromotionError> {
        let mut snapshot = Snapshot::from_value(doc)?;
        if snapshot.id != id {
            return Err(row_invalid(
                "a stored promotion carries another id than its row",
            ));
        }
        // The column is the authority: the document was written at the revision before it.
        snapshot.revision = u64::try_from(revision)
            .map_err(|_| row_invalid("a stored promotion has a negative revision"))?;
        let promotion = Promotion::restore(snapshot, &self.limits, &self.registry)?;
        if promotion.to_env().tenant != self.tenant {
            return Err(row_invalid("a stored promotion targets another tenant"));
        }
        Ok(promotion)
    }

    /// The row of a promotion: its snapshot document and state name.
    fn encode(promotion: &Promotion) -> Result<(Value, String), PromotionError> {
        Ok((
            promotion.snapshot()?.to_value()?,
            promotion.state().as_str().to_owned(),
        ))
    }
}

impl fmt::Debug for PgPromotionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgPromotionStore").finish_non_exhaustive()
    }
}

/// What a save statement found.
enum Outcome {
    Saved,
    Stale,
    Missing,
}

impl PromotionStore for PgPromotionStore {
    fn insert(&self, promotion: Promotion) -> Result<(), PromotionError> {
        self.check_tenant(&promotion)?;
        let (doc, state) = Self::encode(&promotion)?;
        let revision = i64::try_from(promotion.revision())
            .map_err(|_| store_note("the promotion revision is out of range"))?;
        let tenant = self.tenant.to_string();
        let id = promotion.id().to_string();
        let key = id.clone();
        let inserted = self
            .bridge
            .run(WRITE_DEADLINE, move |conn| {
                Box::pin(async move {
                    conn.execute(
                        "INSERT INTO spot_control.spot_promotion \
                         (tenant, id, revision, state, doc) VALUES ($1, $2, $3, $4, $5) \
                         ON CONFLICT (tenant, id) DO NOTHING",
                        &[&tenant, &key, &revision, &state, &doc],
                    )
                    .await
                })
            })
            .map_err(|e| store_failed(&e))?;
        if inserted == 1 {
            Ok(())
        } else {
            Err(PromotionError::Duplicate { id })
        }
    }

    fn save(&self, promotion: Promotion) -> Result<(), PromotionError> {
        self.check_tenant(&promotion)?;
        let (doc, state) = Self::encode(&promotion)?;
        let id = promotion.id().to_string();
        let Ok(expected) = i64::try_from(promotion.revision()) else {
            // No stored revision can be this large, so it can only be missing or stale.
            return self.load(promotion.id()).and_then(|held| match held {
                Some(_) => Err(PromotionError::StaleSave { id }),
                None => Err(PromotionError::NotFound { id }),
            });
        };
        let tenant = self.tenant.to_string();
        let key = id.clone();
        let outcome = self
            .bridge
            .run(WRITE_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    let updated = tx
                        .execute(
                            "UPDATE spot_control.spot_promotion \
                             SET doc = $3, state = $4, revision = revision + 1 \
                             WHERE tenant = $1 AND id = $2 AND revision = $5",
                            &[&tenant, &key, &doc, &state, &expected],
                        )
                        .await?;
                    let outcome = if updated == 1 {
                        Outcome::Saved
                    } else if tx
                        .query_opt(
                            "SELECT 1 FROM spot_control.spot_promotion \
                             WHERE tenant = $1 AND id = $2",
                            &[&tenant, &key],
                        )
                        .await?
                        .is_some()
                    {
                        Outcome::Stale
                    } else {
                        Outcome::Missing
                    };
                    tx.commit().await?;
                    Ok::<Outcome, tokio_postgres::Error>(outcome)
                })
            })
            .map_err(|e| store_failed(&e))?;
        match outcome {
            Outcome::Saved => Ok(()),
            Outcome::Stale => Err(PromotionError::StaleSave { id }),
            Outcome::Missing => Err(PromotionError::NotFound { id }),
        }
    }

    fn load(&self, id: &RunId) -> Result<Option<Promotion>, PromotionError> {
        let tenant = self.tenant.to_string();
        let key = id.to_string();
        let row = self
            .bridge
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn
                        .query_opt(
                            "SELECT id, revision, doc FROM spot_control.spot_promotion \
                             WHERE tenant = $1 AND id = $2",
                            &[&tenant, &key],
                        )
                        .await?;
                    row.map(|r| {
                        Ok::<Row, tokio_postgres::Error>((
                            r.try_get(0)?,
                            r.try_get(1)?,
                            r.try_get(2)?,
                        ))
                    })
                    .transpose()
                })
            })
            .map_err(|e| store_failed(&e))?;
        row.map(|(id, revision, doc)| self.decode(&id, revision, doc))
            .transpose()
    }

    fn list(&self) -> Result<Vec<Promotion>, PromotionError> {
        let tenant = self.tenant.to_string();
        let rows = self
            .bridge
            .run(LIST_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    // One snapshot for every page, so a concurrent write cannot tear the list.
                    let tx = conn
                        .build_transaction()
                        .isolation_level(IsolationLevel::RepeatableRead)
                        .read_only(true)
                        .start()
                        .await?;
                    let page = i64::try_from(LIST_PAGE).unwrap_or(100);
                    let mut rows: Vec<Row> = Vec::new();
                    let mut after = String::new();
                    loop {
                        let batch = tx
                            .query(
                                "SELECT id, revision, doc FROM spot_control.spot_promotion \
                                 WHERE tenant = $1 AND id > $2 ORDER BY id LIMIT $3",
                                &[&tenant, &after, &page],
                            )
                            .await?;
                        let full = batch.len() == LIST_PAGE;
                        for r in &batch {
                            rows.push((r.try_get(0)?, r.try_get(1)?, r.try_get(2)?));
                        }
                        if rows.len() > LIST_CEILING {
                            break;
                        }
                        match rows.last() {
                            Some(last) if full => after.clone_from(&last.0),
                            _ => break,
                        }
                    }
                    tx.commit().await?;
                    Ok::<Vec<Row>, tokio_postgres::Error>(rows)
                })
            })
            .map_err(|e| store_failed(&e))?;
        if rows.len() > LIST_CEILING {
            return Err(store_note(
                "there are more promotions than one list returns; nothing was truncated",
            ));
        }
        rows.into_iter()
            .map(|(id, revision, doc)| self.decode(&id, revision, doc))
            .collect()
    }
}
