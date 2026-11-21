//! [`PgSodStore`]: the PostgreSQL [`SodStore`].
//!
//! All writers of one tenant take the same transaction-scoped advisory lock, so a rule-set
//! write, an override grant and a revocation of a tenant are serialised and each decides on
//! what the previous one committed. The tables are append-only (see [`crate::schema`]): a new
//! rule-set version is a new row, an override is never edited, a revocation is its own row.

use deadpool_postgres::{Object, Pool};
use spot_sod::store::{
    Revocation, SodStore, SodStoreError, StoreFuture, StoredOverride, StoredRuleSet, check_limit,
    check_override, check_revocation, check_rule_set,
};
use spot_sod::{RuleSet, SodOverride};
use spot_tenancy::TenantId;
use tokio_postgres::Transaction;

use crate::error::{is_unique, sod_pool, sod_unavailable as unavailable};

const LOCK: &str = "SELECT pg_advisory_xact_lock(hashtextextended('spot_sod:' || $1::text, 0))";
const CURRENT_VERSION: &str =
    "SELECT COALESCE(MAX(version), 0) FROM spot_sod_rule_sets WHERE tenant = $1";
const INSERT_RULE_SET: &str = "INSERT INTO spot_sod_rule_sets (tenant, version, toml, set_by, set_at) \
    VALUES ($1, $2, $3, $4, $5)";
const LATEST_RULE_SET: &str = "SELECT version, toml, set_by, set_at FROM spot_sod_rule_sets \
    WHERE tenant = $1 ORDER BY version DESC LIMIT 1";
const ACTIVE_COVER: &str = "SELECT 1 FROM spot_sod_overrides o \
    WHERE o.tenant = $1 AND o.rule_id = $2 AND o.user_id = $3 \
      AND o.scope IS NOT DISTINCT FROM $4::text AND o.expires_at > $5 \
      AND NOT EXISTS (SELECT 1 FROM spot_sod_override_revocations r \
                      WHERE r.tenant = o.tenant AND r.seq = o.seq) LIMIT 1";
const NEXT_SEQ: &str = "SELECT COALESCE(MAX(seq), 0) + 1 FROM spot_sod_overrides WHERE tenant = $1";
const INSERT_OVERRIDE: &str = "INSERT INTO spot_sod_overrides \
    (tenant, seq, rule_id, user_id, scope, reason, mitigating_control, granted_by, granted_at, expires_at) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)";
const OVERRIDE_EXISTS: &str = "SELECT 1 FROM spot_sod_overrides WHERE tenant = $1 AND seq = $2";
const REVOKED: &str = "SELECT 1 FROM spot_sod_override_revocations WHERE tenant = $1 AND seq = $2";
const INSERT_REVOCATION: &str = "INSERT INTO spot_sod_override_revocations \
    (tenant, seq, revoked_by, reason, revoked_at) VALUES ($1, $2, $3, $4, $5)";
const SELECT_OVERRIDES: &str = "SELECT o.seq, o.rule_id, o.user_id, o.scope, o.reason, \
    o.mitigating_control, o.granted_by, o.granted_at, o.expires_at, \
    r.revoked_by, r.reason, r.revoked_at \
    FROM spot_sod_overrides o \
    LEFT JOIN spot_sod_override_revocations r ON r.tenant = o.tenant AND r.seq = o.seq \
    WHERE o.tenant = $1 AND o.seq > $2";

/// The SoD store in PostgreSQL. Clones share the pool.
#[derive(Clone)]
pub struct PgSodStore {
    pool: Pool,
}

impl std::fmt::Debug for PgSodStore {
    /// Names the type only, never a pool or connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgSodStore").finish_non_exhaustive()
    }
}

fn row_override(r: &tokio_postgres::Row) -> Result<StoredOverride, SodStoreError> {
    let revoked = r.get::<_, Option<String>>(9).map(|by| Revocation {
        by,
        reason: r.get(10),
        at: r.get(11),
    });
    Ok(StoredOverride {
        seq: u64::try_from(r.get::<_, i64>(0)).map_err(|_| SodStoreError::Corrupt)?,
        record: SodOverride {
            rule_id: r.get(1),
            user: r.get(2),
            scope: r.get(3),
            reason: r.get(4),
            mitigating_control: r.get(5),
            granted_by: r.get(6),
            granted_at: r.get(7),
        },
        expires_at: r.get(8),
        revoked,
    })
}

/// Parses the tenant's latest stored rule set; it is validated again, and a set that no longer
/// validates is `Corrupt` (never "no rules").
async fn latest(
    tx: &Transaction<'_>,
    tenant: &str,
) -> Result<Option<StoredRuleSet>, SodStoreError> {
    let Some(row) = tx
        .query_opt(LATEST_RULE_SET, &[&tenant])
        .await
        .map_err(|e| unavailable(&e))?
    else {
        return Ok(None);
    };
    let toml: String = row.get(1);
    let rules = RuleSet::from_toml(&toml).map_err(|_| SodStoreError::Corrupt)?;
    Ok(Some(StoredRuleSet {
        version: u64::try_from(row.get::<_, i64>(0)).map_err(|_| SodStoreError::Corrupt)?,
        rules,
        set_by: row.get(2),
        set_at: row.get(3),
    }))
}

impl PgSodStore {
    /// A store on `pool`. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the tables, guards and role of this crate if missing (the campaign tables too).
    ///
    /// # Errors
    /// `access_review_pg.migration_failed`.
    pub async fn prepare(&self) -> spot_app_api::AppResult<()> {
        crate::schema::prepare(&self.pool).await
    }

    async fn conn(&self) -> Result<Object, SodStoreError> {
        self.pool.get().await.map_err(|e| sod_pool(&e))
    }

    async fn overrides(
        &self,
        tenant: &TenantId,
        after_seq: u64,
        active_at: Option<i64>,
        limit: Option<u32>,
    ) -> Result<Vec<StoredOverride>, SodStoreError> {
        let after = i64::try_from(after_seq).unwrap_or(i64::MAX);
        let mut sql = SELECT_OVERRIDES.to_owned();
        let tenant = tenant.to_string();
        let conn = self.conn().await?;
        let rows = if let Some(now) = active_at {
            sql.push_str(" AND r.seq IS NULL AND o.expires_at > $3 ORDER BY o.seq");
            conn.query(&sql, &[&tenant, &after, &now]).await
        } else {
            sql.push_str(" ORDER BY o.seq LIMIT $3");
            let limit = i64::from(limit.unwrap_or(0));
            conn.query(&sql, &[&tenant, &after, &limit]).await
        }
        .map_err(|e| unavailable(&e))?;
        rows.iter().map(row_override).collect()
    }
}

impl SodStore for PgSodStore {
    fn put_rule_set<'a>(
        &'a self,
        tenant: &'a TenantId,
        toml: &'a str,
        expected_version: u64,
        by: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<u64, SodStoreError>> {
        Box::pin(async move {
            check_rule_set(toml, by)?;
            let expected = i64::try_from(expected_version)
                .map_err(|_| SodStoreError::InputInvalid("version"))?;
            let tenant = tenant.to_string();
            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(|e| unavailable(&e))?;
            tx.execute(LOCK, &[&tenant])
                .await
                .map_err(|e| unavailable(&e))?;
            let current: i64 = tx
                .query_one(CURRENT_VERSION, &[&tenant])
                .await
                .map_err(|e| unavailable(&e))?
                .get(0);
            if current != expected {
                return Err(SodStoreError::VersionConflict);
            }
            let next = current + 1;
            tx.execute(INSERT_RULE_SET, &[&tenant, &next, &toml, &by, &now])
                .await
                .map_err(|e| {
                    if is_unique(&e) {
                        SodStoreError::VersionConflict
                    } else {
                        unavailable(&e)
                    }
                })?;
            tx.commit().await.map_err(|e| unavailable(&e))?;
            u64::try_from(next).map_err(|_| SodStoreError::Corrupt)
        })
    }

    fn rule_set<'a>(
        &'a self,
        tenant: &'a TenantId,
    ) -> StoreFuture<'a, Result<Option<StoredRuleSet>, SodStoreError>> {
        Box::pin(async move {
            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(|e| unavailable(&e))?;
            latest(&tx, &tenant.to_string()).await
        })
    }

    fn grant_override<'a>(
        &'a self,
        tenant: &'a TenantId,
        record: SodOverride,
        expires_at: i64,
        now: i64,
    ) -> StoreFuture<'a, Result<u64, SodStoreError>> {
        Box::pin(async move {
            check_override(&record, expires_at, now)?;
            let tenant = tenant.to_string();
            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(|e| unavailable(&e))?;
            tx.execute(LOCK, &[&tenant])
                .await
                .map_err(|e| unavailable(&e))?;
            let rules = latest(&tx, &tenant)
                .await?
                .ok_or(SodStoreError::InputInvalid("the tenant has no rule set"))?;
            if rules.rules.rule(&record.rule_id).is_none() {
                return Err(SodStoreError::InputInvalid("rule id names no rule"));
            }
            let active = tx
                .query_opt(
                    ACTIVE_COVER,
                    &[&tenant, &record.rule_id, &record.user, &record.scope, &now],
                )
                .await
                .map_err(|e| unavailable(&e))?;
            if active.is_some() {
                return Err(SodStoreError::AlreadyActive);
            }
            let seq: i64 = tx
                .query_one(NEXT_SEQ, &[&tenant])
                .await
                .map_err(|e| unavailable(&e))?
                .get(0);
            tx.execute(
                INSERT_OVERRIDE,
                &[
                    &tenant,
                    &seq,
                    &record.rule_id,
                    &record.user,
                    &record.scope,
                    &record.reason,
                    &record.mitigating_control,
                    &record.granted_by,
                    &record.granted_at,
                    &expires_at,
                ],
            )
            .await
            .map_err(|e| unavailable(&e))?;
            tx.commit().await.map_err(|e| unavailable(&e))?;
            u64::try_from(seq).map_err(|_| SodStoreError::Corrupt)
        })
    }

    fn revoke_override<'a>(
        &'a self,
        tenant: &'a TenantId,
        seq: u64,
        by: &'a str,
        reason: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<(), SodStoreError>> {
        Box::pin(async move {
            check_revocation(by, reason)?;
            let seq = i64::try_from(seq).map_err(|_| SodStoreError::NotFound)?;
            let tenant = tenant.to_string();
            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(|e| unavailable(&e))?;
            tx.execute(LOCK, &[&tenant])
                .await
                .map_err(|e| unavailable(&e))?;
            if tx
                .query_opt(OVERRIDE_EXISTS, &[&tenant, &seq])
                .await
                .map_err(|e| unavailable(&e))?
                .is_none()
            {
                return Err(SodStoreError::NotFound);
            }
            if tx
                .query_opt(REVOKED, &[&tenant, &seq])
                .await
                .map_err(|e| unavailable(&e))?
                .is_some()
            {
                return Err(SodStoreError::AlreadyRevoked);
            }
            tx.execute(INSERT_REVOCATION, &[&tenant, &seq, &by, &reason, &now])
                .await
                .map_err(|e| unavailable(&e))?;
            tx.commit().await.map_err(|e| unavailable(&e))
        })
    }

    fn active_overrides<'a>(
        &'a self,
        tenant: &'a TenantId,
        now: i64,
    ) -> StoreFuture<'a, Result<Vec<StoredOverride>, SodStoreError>> {
        Box::pin(async move { self.overrides(tenant, 0, Some(now), None).await })
    }

    fn list_overrides<'a>(
        &'a self,
        tenant: &'a TenantId,
        after_seq: u64,
        limit: u32,
    ) -> StoreFuture<'a, Result<Vec<StoredOverride>, SodStoreError>> {
        Box::pin(async move {
            check_limit(limit)?;
            self.overrides(tenant, after_seq, None, Some(limit)).await
        })
    }
}
