//! The statements of the store state, each inside the caller's transaction. Every statement is
//! static text with bound parameters; nothing is built from input (ADR-45).

use std::collections::BTreeSet;

use spot_registry::{AppName, Digest, Version};
use spot_store_state::{
    AuditEntry, AuditKind, AuditRecord, Begin, BundleRevocation, Fingerprint, IssuedList, KeyHash,
    KeyInfo, KeyRecord, KeyRevocation, ListingRecord, ListingState, ReviewRecord, ReviewStatus,
    ReviewSummary, ReviewUpdate, StoreStateError,
};
use tokio_postgres::{Row, Transaction};

use crate::error::{corrupt, is_foreign_key_violation, query_failed, unique_violation};

type Result<T> = std::result::Result<T, StoreStateError>;

const KEY_HASH_CONSTRAINT: &str = "spot_store_state_key_hash_key";

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| StoreStateError::too_large("number"))
}

fn from_i64(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| corrupt())
}

fn get<'a, T: tokio_postgres::types::FromSql<'a>>(row: &'a Row, index: usize) -> Result<T> {
    row.try_get(index).map_err(query_failed)
}

// ---- publisher keys ------------------------------------------------------------------------

pub(crate) async fn issue_key(
    tx: &Transaction<'_>,
    publisher_id: &str,
    key_hash: &KeyHash,
    label: &str,
    now: i64,
) -> Result<u64> {
    let hash: &[u8] = key_hash.as_bytes();
    match tx
        .query_one(
            "INSERT INTO spot_control.spot_store_state_key (publisher_id, key_hash, label, issued_at) \
             VALUES ($1, $2, $3, $4) RETURNING key_id",
            &[&publisher_id, &hash, &label, &now],
        )
        .await
    {
        Ok(row) => from_i64(get(&row, 0)?),
        Err(error) => match unique_violation(&error) {
            Some(constraint) if constraint == KEY_HASH_CONSTRAINT => {
                Err(StoreStateError::duplicate_key_hash())
            }
            _ => Err(query_failed(error)),
        },
    }
}

pub(crate) async fn lookup_key(
    tx: &Transaction<'_>,
    key_hash: &KeyHash,
) -> Result<Option<KeyRecord>> {
    let hash: &[u8] = key_hash.as_bytes();
    let row = tx
        .query_opt(
            "SELECT key_id, publisher_id, key_hash, revoked_at \
             FROM spot_control.spot_store_state_key WHERE key_hash = $1",
            &[&hash],
        )
        .await
        .map_err(query_failed)?;
    row.map(|row| {
        let stored: Vec<u8> = get(&row, 2)?;
        let bytes: [u8; 32] = stored.try_into().map_err(|_| corrupt())?;
        Ok(KeyRecord {
            key_id: from_i64(get(&row, 0)?)?,
            publisher_id: get(&row, 1)?,
            key_hash: KeyHash::from_bytes(bytes),
            revoked_at: get(&row, 3)?,
        })
    })
    .transpose()
}

pub(crate) async fn revoke_key_row(tx: &Transaction<'_>, key_id: u64, now: i64) -> Result<()> {
    let id = to_i64(key_id)?;
    let changed = tx
        .execute(
            "UPDATE spot_control.spot_store_state_key \
             SET revoked_at = COALESCE(revoked_at, $2) WHERE key_id = $1",
            &[&id, &now],
        )
        .await
        .map_err(query_failed)?;
    if changed == 0 {
        return Err(StoreStateError::key_unknown());
    }
    Ok(())
}

pub(crate) async fn list_keys(tx: &Transaction<'_>, publisher_id: &str) -> Result<Vec<KeyInfo>> {
    tx.query(
        "SELECT key_id, label, issued_at, revoked_at FROM spot_control.spot_store_state_key \
         WHERE publisher_id = $1 ORDER BY key_id",
        &[&publisher_id],
    )
    .await
    .map_err(query_failed)?
    .iter()
    .map(|row| {
        Ok(KeyInfo {
            key_id: from_i64(get(row, 0)?)?,
            label: get(row, 1)?,
            issued_at: get(row, 2)?,
            revoked_at: get(row, 3)?,
        })
    })
    .collect()
}

// ---- reviews -------------------------------------------------------------------------------

fn review_from_row(row: &Row) -> Result<ReviewRecord> {
    let status: String = get(row, 5)?;
    let summary: Option<String> = get(row, 7)?;
    let app: String = get(row, 2)?;
    let version: String = get(row, 3)?;
    let digest: String = get(row, 4)?;
    Ok(ReviewRecord {
        review_id: get(row, 0)?,
        publisher_id: get(row, 1)?,
        app: AppName::parse(&app).map_err(|_| corrupt())?,
        version: version.parse::<Version>().map_err(|_| corrupt())?,
        digest: digest.parse::<Digest>().map_err(|_| corrupt())?,
        status: ReviewStatus::parse(&status).ok_or_else(corrupt)?,
        verdict: get(row, 6)?,
        summary: summary
            .map(ReviewSummary::new)
            .transpose()
            .map_err(|_| corrupt())?,
        error_code: get(row, 8)?,
        created_at: get(row, 9)?,
        updated_at: get(row, 10)?,
    })
}

pub(crate) async fn create_review(tx: &Transaction<'_>, record: &ReviewRecord) -> Result<bool> {
    let version = record.version.to_string();
    let digest = record.digest.to_hex();
    let summary = record.summary.as_ref().map(ReviewSummary::as_str);
    let inserted = tx
        .execute(
            "INSERT INTO spot_control.spot_store_state_review (review_id, publisher_id, app, version, \
             digest, status, verdict, summary, error_code, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT (review_id) DO NOTHING",
            &[
                &record.review_id,
                &record.publisher_id,
                &record.app.as_str(),
                &version,
                &digest,
                &record.status.as_str(),
                &record.verdict,
                &summary,
                &record.error_code,
                &record.created_at,
                &record.updated_at,
            ],
        )
        .await
        .map_err(query_failed)?;
    Ok(inserted == 1)
}

pub(crate) async fn get_review(
    tx: &Transaction<'_>,
    review_id: &str,
) -> Result<Option<ReviewRecord>> {
    tx.query_opt(
        "SELECT review_id, publisher_id, app, version, digest, status, verdict, summary, \
         error_code, created_at, updated_at \
         FROM spot_control.spot_store_state_review WHERE review_id = $1",
        &[&review_id],
    )
    .await
    .map_err(query_failed)?
    .as_ref()
    .map(review_from_row)
    .transpose()
}

pub(crate) async fn update_review(
    tx: &Transaction<'_>,
    review_id: &str,
    update: &ReviewUpdate,
) -> Result<()> {
    let summary = update.summary.as_ref().map(ReviewSummary::as_str);
    let changed = tx
        .execute(
            "UPDATE spot_control.spot_store_state_review SET status = $2, verdict = $3, summary = $4, \
             error_code = $5, updated_at = $6 WHERE review_id = $1",
            &[
                &review_id,
                &update.status.as_str(),
                &update.verdict,
                &summary,
                &update.error_code,
                &update.now,
            ],
        )
        .await
        .map_err(query_failed)?;
    if changed == 0 {
        return Err(StoreStateError::review_unknown());
    }
    Ok(())
}

pub(crate) async fn reviews_for(
    tx: &Transaction<'_>,
    app: &AppName,
    version: &Version,
) -> Result<Vec<ReviewRecord>> {
    let version = version.to_string();
    tx.query(
        "SELECT review_id, publisher_id, app, version, digest, status, verdict, summary, \
         error_code, created_at, updated_at FROM spot_control.spot_store_state_review \
         WHERE app = $1 AND version = $2 ORDER BY created_at, review_id COLLATE \"C\"",
        &[&app.as_str(), &version],
    )
    .await
    .map_err(query_failed)?
    .iter()
    .map(review_from_row)
    .collect()
}

// ---- listing -------------------------------------------------------------------------------

/// The transition, one statement: an insert for a start, a compare-and-set on the previous state
/// otherwise. Zero rows means the transition is not allowed or another writer got there first.
pub(crate) async fn set_listing(
    tx: &Transaction<'_>,
    app: &AppName,
    version: &Version,
    state: ListingState,
    by: &str,
    now: i64,
) -> Result<()> {
    let version = version.to_string();
    let changed = if state.may_start() {
        tx.execute(
            "INSERT INTO spot_control.spot_store_state_listing (app, version, state, set_by, set_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (app, version) DO NOTHING",
            &[&app.as_str(), &version, &state.as_str(), &by, &now],
        )
        .await
    } else {
        let previous: Vec<&str> = state
            .allowed_previous()
            .iter()
            .map(|s| s.as_str())
            .collect();
        tx.execute(
            "UPDATE spot_control.spot_store_state_listing \
             SET state = $3, set_by = $4, set_at = $5 \
             WHERE app = $1 AND version = $2 AND state = ANY($6)",
            &[&app.as_str(), &version, &state.as_str(), &by, &now, &previous],
        )
        .await
    }
    .map_err(query_failed)?;
    if changed == 0 {
        return Err(StoreStateError::bad_transition());
    }
    Ok(())
}

pub(crate) async fn listing(
    tx: &Transaction<'_>,
    app: &AppName,
    version: &Version,
) -> Result<Option<ListingRecord>> {
    let version = version.to_string();
    tx.query_opt(
        "SELECT state, set_by, set_at FROM spot_control.spot_store_state_listing \
         WHERE app = $1 AND version = $2",
        &[&app.as_str(), &version],
    )
    .await
    .map_err(query_failed)?
    .map(|row| {
        let state: String = get(&row, 0)?;
        Ok(ListingRecord {
            state: ListingState::parse(&state).ok_or_else(corrupt)?,
            by: get(&row, 1)?,
            at: get(&row, 2)?,
        })
    })
    .transpose()
}

pub(crate) async fn listed_among(
    tx: &Transaction<'_>,
    candidates: &[(AppName, Version)],
) -> Result<BTreeSet<(AppName, Version)>> {
    let apps: Vec<&str> = candidates.iter().map(|(a, _)| a.as_str()).collect();
    let versions: Vec<String> = candidates.iter().map(|(_, v)| v.to_string()).collect();
    let rows = tx
        .query(
            "SELECT l.app, l.version FROM spot_control.spot_store_state_listing l \
             JOIN unnest($1::text[], $2::text[]) AS c(app, version) \
               ON l.app = c.app AND l.version = c.version \
             WHERE l.state = 'listed'",
            &[&apps, &versions],
        )
        .await
        .map_err(query_failed)?;
    rows.iter()
        .map(|row| {
            let app: String = get(row, 0)?;
            let version: String = get(row, 1)?;
            Ok((
                AppName::parse(&app).map_err(|_| corrupt())?,
                version.parse::<Version>().map_err(|_| corrupt())?,
            ))
        })
        .collect()
}

// ---- idempotency ---------------------------------------------------------------------------

/// Claims the key, or reports what holds it. The claim is `INSERT .. ON CONFLICT DO NOTHING`, so
/// two callers racing on one key see exactly one `New`. A row pruned between the failed insert and
/// the read is claimed on the second round.
pub(crate) async fn idempotency_begin(
    tx: &Transaction<'_>,
    publisher_id: &str,
    key: &str,
    fingerprint: &Fingerprint,
    now: i64,
) -> Result<Begin> {
    let print: &[u8] = &fingerprint.0;
    for _ in 0..2 {
        let inserted = tx
            .execute(
                "INSERT INTO spot_control.spot_store_state_idempotency \
                 (publisher_id, idem_key, fingerprint, created_at) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (publisher_id, idem_key) DO NOTHING",
                &[&publisher_id, &key, &print, &now],
            )
            .await
            .map_err(query_failed)?;
        if inserted == 1 {
            return Ok(Begin::New);
        }
        let row = tx
            .query_opt(
                "SELECT fingerprint, review_id FROM spot_control.spot_store_state_idempotency \
                 WHERE publisher_id = $1 AND idem_key = $2",
                &[&publisher_id, &key],
            )
            .await
            .map_err(query_failed)?;
        if let Some(row) = row {
            let stored: Vec<u8> = get(&row, 0)?;
            return Ok(if stored == print {
                Begin::Replay {
                    review_id: get(&row, 1)?,
                }
            } else {
                Begin::Conflict
            });
        }
    }
    Err(StoreStateError::backend(
        "the idempotency key kept changing under the call",
    ))
}

pub(crate) async fn idempotency_finish(
    tx: &Transaction<'_>,
    publisher_id: &str,
    key: &str,
    review_id: &str,
) -> Result<()> {
    match tx
        .execute(
            "UPDATE spot_control.spot_store_state_idempotency SET review_id = $3 \
             WHERE publisher_id = $1 AND idem_key = $2 AND (review_id IS NULL OR review_id = $3)",
            &[&publisher_id, &key, &review_id],
        )
        .await
    {
        Ok(0) => Err(StoreStateError::idempotency_conflict()),
        Ok(_) => Ok(()),
        Err(error) if is_foreign_key_violation(&error) => Err(StoreStateError::review_unknown()),
        Err(error) => Err(query_failed(error)),
    }
}

pub(crate) async fn idempotency_prune(tx: &Transaction<'_>, older_than: i64) -> Result<u64> {
    tx.execute(
        "DELETE FROM spot_control.spot_store_state_idempotency WHERE created_at < $1",
        &[&older_than],
    )
    .await
    .map_err(query_failed)
}

// ---- revocation state ----------------------------------------------------------------------

pub(crate) async fn revoke_signing_key(tx: &Transaction<'_>, r: &KeyRevocation) -> Result<bool> {
    let inserted = tx
        .execute(
            "INSERT INTO spot_control.spot_store_state_revoked_key (key_id, reason, at) \
             VALUES ($1, $2, $3) ON CONFLICT (key_id) DO NOTHING",
            &[&r.key_id, &r.reason, &r.at],
        )
        .await
        .map_err(query_failed)?;
    Ok(inserted == 1)
}

pub(crate) async fn revoke_bundle(tx: &Transaction<'_>, r: &BundleRevocation) -> Result<bool> {
    let inserted = tx
        .execute(
            "INSERT INTO spot_control.spot_store_state_revoked_bundle (digest, reason, at) \
             VALUES ($1, $2, $3) ON CONFLICT (digest) DO NOTHING",
            &[&r.digest.to_hex(), &r.reason, &r.at],
        )
        .await
        .map_err(query_failed)?;
    Ok(inserted == 1)
}

pub(crate) async fn revoked_keys(tx: &Transaction<'_>) -> Result<Vec<KeyRevocation>> {
    tx.query(
        "SELECT key_id, reason, at FROM spot_control.spot_store_state_revoked_key \
         ORDER BY at, key_id COLLATE \"C\"",
        &[],
    )
    .await
    .map_err(query_failed)?
    .iter()
    .map(|row| {
        Ok(KeyRevocation {
            key_id: get(row, 0)?,
            reason: get(row, 1)?,
            at: get(row, 2)?,
        })
    })
    .collect()
}

pub(crate) async fn revoked_bundles(tx: &Transaction<'_>) -> Result<Vec<BundleRevocation>> {
    tx.query(
        "SELECT digest, reason, at FROM spot_control.spot_store_state_revoked_bundle \
         ORDER BY at, digest COLLATE \"C\"",
        &[],
    )
    .await
    .map_err(query_failed)?
    .iter()
    .map(|row| {
        let digest: String = get(row, 0)?;
        Ok(BundleRevocation {
            digest: digest.parse::<Digest>().map_err(|_| corrupt())?,
            reason: get(row, 1)?,
            at: get(row, 2)?,
        })
    })
    .collect()
}

/// One statement on the single counter row: the row lock serialises two connections.
pub(crate) async fn next_generation(tx: &Transaction<'_>, now: i64) -> Result<u64> {
    let row = tx
        .query_one(
            "UPDATE spot_control.spot_store_state_meta SET generation = generation + 1, updated_at = $1 \
             WHERE id = 1 RETURNING generation",
            &[&now],
        )
        .await
        .map_err(query_failed)?;
    from_i64(get(&row, 0)?)
}

/// Upserts the single row only when the generation is above the stored one.
pub(crate) async fn record_issued_list(tx: &Transaction<'_>, list: &IssuedList) -> Result<()> {
    let generation = to_i64(list.generation)?;
    let changed = tx
        .execute(
            "INSERT INTO spot_control.spot_store_state_issued_list \
             (id, generation, issued_at, next_update, body) VALUES (1, $1, $2, $3, $4) \
             ON CONFLICT (id) DO UPDATE SET generation = EXCLUDED.generation, \
               issued_at = EXCLUDED.issued_at, next_update = EXCLUDED.next_update, body = EXCLUDED.body \
             WHERE spot_control.spot_store_state_issued_list.generation < EXCLUDED.generation",
            &[&generation, &list.issued_at, &list.next_update, &list.body_json],
        )
        .await
        .map_err(query_failed)?;
    if changed == 0 {
        return Err(StoreStateError::bad_transition());
    }
    Ok(())
}

pub(crate) async fn last_issued_list(tx: &Transaction<'_>) -> Result<Option<IssuedList>> {
    tx.query_opt(
        "SELECT generation, issued_at, next_update, body \
         FROM spot_control.spot_store_state_issued_list WHERE id = 1",
        &[],
    )
    .await
    .map_err(query_failed)?
    .map(|row| {
        Ok(IssuedList {
            generation: from_i64(get(&row, 0)?)?,
            issued_at: get(&row, 1)?,
            next_update: get(&row, 2)?,
            body_json: get(&row, 3)?,
        })
    })
    .transpose()
}

// ---- audit ---------------------------------------------------------------------------------

pub(crate) async fn append_audit(tx: &Transaction<'_>, entry: &AuditEntry) -> Result<u64> {
    let app = entry.app.as_ref().map(AppName::as_str);
    let version = entry.version.as_ref().map(ToString::to_string);
    let digest = entry.digest.as_ref().map(Digest::to_hex);
    let row = tx
        .query_one(
            "INSERT INTO spot_control.spot_store_state_audit \
             (kind, actor, publisher_id, app, version, digest, reason, at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
            &[
                &entry.kind.as_str(),
                &entry.actor,
                &entry.publisher_id,
                &app,
                &version,
                &digest,
                &entry.reason,
                &entry.at,
            ],
        )
        .await
        .map_err(query_failed)?;
    from_i64(get(&row, 0)?)
}

pub(crate) async fn read_audit(
    tx: &Transaction<'_>,
    after_id: u64,
    limit: i64,
) -> Result<Vec<AuditRecord>> {
    let after = to_i64(after_id)?;
    tx.query(
        "SELECT id, kind, actor, publisher_id, app, version, digest, reason, at \
         FROM spot_control.spot_store_state_audit WHERE id > $1 ORDER BY id LIMIT $2",
        &[&after, &limit],
    )
    .await
    .map_err(query_failed)?
    .iter()
    .map(|row| {
        let kind: String = get(row, 1)?;
        let app: Option<String> = get(row, 4)?;
        let version: Option<String> = get(row, 5)?;
        let digest: Option<String> = get(row, 6)?;
        Ok(AuditRecord {
            id: from_i64(get(row, 0)?)?,
            entry: AuditEntry {
                kind: AuditKind::parse(&kind).ok_or_else(corrupt)?,
                actor: get(row, 2)?,
                publisher_id: get(row, 3)?,
                app: app
                    .map(|a| AppName::parse(&a))
                    .transpose()
                    .map_err(|_| corrupt())?,
                version: version
                    .map(|v| v.parse::<Version>())
                    .transpose()
                    .map_err(|_| corrupt())?,
                digest: digest
                    .map(|d| d.parse::<Digest>())
                    .transpose()
                    .map_err(|_| corrupt())?,
                reason: get(row, 7)?,
                at: get(row, 8)?,
            },
        })
    })
    .collect()
}
