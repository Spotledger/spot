//! `PlanStore` over `spot_migration_plan`.

use sha2::{Digest, Sha256};
use spot_migration_approval::DOMAIN_PREFIX;
use spot_migration_ops::{LifecycleError, PlanHash, PlanState, PlanStore, PlanSummary, StoredPlan};

use crate::schema::failed;
use crate::store::{LIST_DEADLINE, PgMigrationStore, READ_DEADLINE, WRITE_DEADLINE};

/// The largest plan text stored, in bytes (also a check constraint, in characters).
pub const MAX_PLAN_BYTES: usize = 1_048_576;

/// The most plans `list` returns; more is `migration.store_failed`, never a truncated answer.
const LIST_CEILING: i64 = 5_000;

const SELECT_PLAN: &str = "SELECT plan_hash, plan_json, author, created_at, state, version \
     FROM spot_migration_plan WHERE plan_hash = $1";

/// The digest of canonical plan text: SHA-256 over the domain prefix and the text, lower hex.
/// It is what `PlanDigest::of` computes over `ReleasePlan::json`.
fn digest_hex(json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_PREFIX);
    hasher.update(json.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

fn to_i64(value: u64) -> Result<i64, LifecycleError> {
    i64::try_from(value).map_err(|_| LifecycleError::Store)
}

fn to_u64(value: i64) -> Result<u64, LifecycleError> {
    u64::try_from(value).map_err(|_| LifecycleError::Store)
}

type Row = (String, String, String, i64, String, i64);

impl PgMigrationStore {
    fn verified(&self, row: Row) -> Result<StoredPlan, LifecycleError> {
        let (hash, json, author, created_at, state, version) = row;
        let hash = PlanHash::parse(&hash).map_err(|_| LifecycleError::Store)?;
        if digest_hex(&json) != hash.as_str() {
            self.alert.plan_tampered(&hash);
            return Err(LifecycleError::PlanChanged);
        }
        Ok(StoredPlan {
            hash,
            json,
            author,
            created_at,
            state: PlanState::parse(&state).ok_or(LifecycleError::Store)?,
            version: to_u64(version)?,
        })
    }
}

impl PlanStore for PgMigrationStore {
    fn save(&self, plan: StoredPlan) -> Result<(), LifecycleError> {
        if plan.json.len() > MAX_PLAN_BYTES || plan.version < 1 {
            return Err(LifecycleError::Store);
        }
        // The text must be the text the hash was taken over.
        if digest_hex(&plan.json) != plan.hash.as_str() {
            return Err(LifecycleError::PlanChanged);
        }
        let hash = plan.hash.as_str().to_owned();
        let (json, author) = (plan.json.clone(), plan.author.clone());
        let created = plan.created_at;
        let state = plan.state.as_str().to_owned();
        let version = to_i64(plan.version)?;
        let existing: Option<(String, String)> = self
            .bridge
            .run(WRITE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let inserted = conn
                        .execute(
                            "INSERT INTO spot_migration_plan \
                             (plan_hash, plan_json, author, created_at, state, version) \
                             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (plan_hash) DO NOTHING",
                            &[&hash, &json, &author, &created, &state, &version],
                        )
                        .await?;
                    if inserted == 1 {
                        return Ok::<_, tokio_postgres::Error>(None);
                    }
                    let row = conn
                        .query_one(
                            "SELECT plan_json, author FROM spot_migration_plan WHERE plan_hash = $1",
                            &[&hash],
                        )
                        .await?;
                    Ok(Some((row.get(0), row.get(1))))
                })
            })
            .map_err(failed)?;
        match existing {
            None => Ok(()),
            Some((json, author)) if json == plan.json && author == plan.author => Ok(()),
            Some(_) => Err(LifecycleError::PlanChanged),
        }
    }

    fn load(&self, hash: &PlanHash) -> Result<Option<StoredPlan>, LifecycleError> {
        let key = hash.as_str().to_owned();
        let row: Option<Row> = self
            .bridge
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_opt(SELECT_PLAN, &[&key]).await?;
                    Ok::<_, tokio_postgres::Error>(
                        row.map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5))),
                    )
                })
            })
            .map_err(failed)?;
        row.map(|r| self.verified(r)).transpose()
    }

    fn list(&self) -> Result<Vec<PlanSummary>, LifecycleError> {
        let rows: Vec<(String, String, i64, String, i64)> = self
            .bridge
            .run(LIST_DEADLINE, move |conn| {
                Box::pin(async move {
                    let rows = conn
                        .query(
                            "SELECT plan_hash, author, created_at, state, version \
                             FROM spot_migration_plan ORDER BY created_at, plan_hash LIMIT $1",
                            &[&(LIST_CEILING + 1)],
                        )
                        .await?;
                    Ok::<_, tokio_postgres::Error>(
                        rows.iter()
                            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
                            .collect(),
                    )
                })
            })
            .map_err(failed)?;
        if i64::try_from(rows.len()).map_err(|_| LifecycleError::Store)? > LIST_CEILING {
            return Err(LifecycleError::Store);
        }
        rows.into_iter()
            .map(|(hash, author, created_at, state, version)| {
                Ok(PlanSummary {
                    hash: PlanHash::parse(&hash).map_err(|_| LifecycleError::Store)?,
                    author,
                    created_at,
                    state: PlanState::parse(&state).ok_or(LifecycleError::Store)?,
                    version: to_u64(version)?,
                })
            })
            .collect()
    }

    fn mark_state(
        &self,
        hash: &PlanHash,
        expected: PlanState,
        expected_version: u64,
        next: PlanState,
    ) -> Result<u64, LifecycleError> {
        let key = hash.as_str().to_owned();
        let (from, to) = (expected.as_str().to_owned(), next.as_str().to_owned());
        let version = to_i64(expected_version)?;
        // `Ok(Ok(new version))` when the compare-and-set won, `Ok(Err(exists))` when it lost.
        let outcome: Result<i64, bool> = self
            .bridge
            .run(WRITE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let won = conn
                        .query_opt(
                            "UPDATE spot_migration_plan SET state = $2, version = version + 1 \
                             WHERE plan_hash = $1 AND state = $3 AND version = $4 \
                             RETURNING version",
                            &[&key, &to, &from, &version],
                        )
                        .await?;
                    if let Some(row) = won {
                        return Ok::<_, tokio_postgres::Error>(Ok(row.get::<_, i64>(0)));
                    }
                    let exists = conn
                        .query_opt(
                            "SELECT 1 FROM spot_migration_plan WHERE plan_hash = $1",
                            &[&key],
                        )
                        .await?
                        .is_some();
                    Ok(Err(exists))
                })
            })
            .map_err(failed)?;
        match outcome {
            Ok(new_version) => to_u64(new_version),
            Err(true) => Err(LifecycleError::PlanChanged),
            Err(false) => Err(LifecycleError::UnknownPlan),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_lowercase_hex_of_sixty_four_characters() {
        let d = digest_hex("{}");
        assert_eq!(d.len(), 64);
        assert!(PlanHash::parse(&d).is_ok());
        assert_ne!(d, digest_hex("{ }"));
    }
}
