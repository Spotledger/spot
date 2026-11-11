//! The transactional rules of the index, ported one for one from the in-memory reference
//! (`MemoryIndexStore`), which is the contract. Every function runs inside the caller's
//! transaction; every mutation first takes the per-app advisory lock, so two registry instances
//! sharing the database serialise on the app they touch and behave as one.

use spot_registry::{
    AppName, Ownership, Publisher, PublisherStatus, RegistryError, Release, Removal, Result,
    TransferRecord, Version,
};
use tokio_postgres::Transaction;
use tokio_postgres::types::ToSql;

use crate::codec::{
    RELEASE_COLUMNS, ReleaseRow, TRANSFER_COLUMNS, corpus, release_from_row, transfer_evidence,
    transfer_from_row,
};
use crate::error::{corrupt, is_unique_violation, query_failed};

type Param<'a> = &'a (dyn ToSql + Sync);

fn refuse(code: &'static str, message: &'static str) -> RegistryError {
    RegistryError::new(code, message)
}

/// The absent-record refusal, the same text as the in-memory index.
pub(crate) fn missing() -> RegistryError {
    refuse("registry.not_found", "index record absent")
}

/// What `commit` returns, mirroring `PublishOutcome`.
pub(crate) struct Committed {
    pub release: Release,
    pub inserted: bool,
}

pub(crate) fn status_name(status: &PublisherStatus) -> &'static str {
    match status {
        PublisherStatus::Active => "active",
        PublisherStatus::Suspended => "suspended",
    }
}

fn status_from(name: &str) -> Result<PublisherStatus> {
    match name {
        "active" => Ok(PublisherStatus::Active),
        "suspended" => Ok(PublisherStatus::Suspended),
        _ => Err(corrupt()),
    }
}

/// Serialises every mutation of one app across all instances; released at commit or rollback.
pub(crate) async fn lock_app(tx: &Transaction<'_>, app: &AppName) -> Result<()> {
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 7))",
        &[&app.as_str()],
    )
    .await
    .map(|_| ())
    .map_err(query_failed)
}

/// Bumps the index revision a search cursor is bound to.
pub(crate) async fn bump(tx: &Transaction<'_>) -> Result<()> {
    tx.execute(
        "UPDATE spot_control.spot_registry_meta SET revision = revision + 1 WHERE id = 1",
        &[],
    )
    .await
    .map(|_| ())
    .map_err(query_failed)
}

pub(crate) async fn revision(tx: &Transaction<'_>) -> Result<u64> {
    let row = tx
        .query_one(
            "SELECT revision FROM spot_control.spot_registry_meta WHERE id = 1",
            &[],
        )
        .await
        .map_err(query_failed)?;
    u64::try_from(row.try_get::<_, i64>(0).map_err(query_failed)?).map_err(|_| corrupt())
}

pub(crate) async fn ownership(tx: &Transaction<'_>, app: &AppName) -> Result<Option<Ownership>> {
    let row = tx
        .query_opt(
            "SELECT publisher, generation FROM spot_control.spot_registry_ownership WHERE app = $1",
            &[&app.as_str()],
        )
        .await
        .map_err(query_failed)?;
    row.map(|row| {
        Ok(Ownership {
            app: app.clone(),
            publisher: row.try_get(0).map_err(query_failed)?,
            generation: u64::try_from(row.try_get::<_, i64>(1).map_err(query_failed)?)
                .map_err(|_| corrupt())?,
        })
    })
    .transpose()
}

pub(crate) async fn publisher(tx: &Transaction<'_>, id: &str) -> Result<Option<Publisher>> {
    let row = tx
        .query_opt(
            "SELECT id, display_name, root_key, status \
             FROM spot_control.spot_registry_publisher WHERE id = $1",
            &[&id],
        )
        .await
        .map_err(query_failed)?;
    row.map(|row| {
        let root: String = row.try_get(2).map_err(query_failed)?;
        let status: String = row.try_get(3).map_err(query_failed)?;
        Ok(Publisher {
            id: row.try_get(0).map_err(query_failed)?,
            display_name: row.try_get(1).map_err(query_failed)?,
            certificate_chain_root: spot_bundle_signing::KeyId::parse(&root)
                .map_err(|_| corrupt())?,
            status: status_from(&status)?,
        })
    })
    .transpose()
}

/// `active`: a publisher with a stored record that is not active may not mutate.
async fn active(tx: &Transaction<'_>, id: &str) -> Result<()> {
    let status: Option<String> = tx
        .query_opt(
            "SELECT status FROM spot_control.spot_registry_publisher WHERE id = $1",
            &[&id],
        )
        .await
        .map_err(query_failed)?
        .map(|row| row.try_get(0).map_err(query_failed))
        .transpose()?;
    match status {
        Some(name) if status_from(&name)? != PublisherStatus::Active => {
            Err(refuse("registry.publisher_inactive", "publisher suspended"))
        }
        _ => Ok(()),
    }
}

/// `owner`: the caller must own the app and be active.
pub(crate) async fn require_owner(tx: &Transaction<'_>, app: &AppName, by: &str) -> Result<()> {
    match ownership(tx, app).await? {
        Some(owner) if owner.publisher == by => active(tx, by).await,
        _ => Err(refuse("registry.not_owner", "publisher does not own app")),
    }
}

/// `check`: ownership, then publisher status, then the version rules.
pub(crate) async fn check(tx: &Transaction<'_>, release: &Release) -> Result<()> {
    if let Some(owner) = ownership(tx, &release.app).await?
        && owner.publisher != release.publisher
    {
        return Err(refuse(
            "registry.not_owner",
            "app belongs to another publisher",
        ));
    }
    active(tx, &release.publisher).await?;
    let rows = tx
        .query(
            "SELECT version, digest FROM spot_control.spot_registry_release WHERE app = $1",
            &[&release.app.as_str()],
        )
        .await
        .map_err(query_failed)?;
    let mut known = Vec::with_capacity(rows.len());
    for row in &rows {
        let version: String = row.try_get(0).map_err(query_failed)?;
        let digest: String = row.try_get(1).map_err(query_failed)?;
        known.push((Version::parse(&version).map_err(|_| corrupt())?, digest));
    }
    if let Some((old, digest)) = known
        .iter()
        .find(|(old, _)| old.cmp_precedence(&release.version).is_eq())
    {
        if *digest != release.digest.to_hex() {
            return Err(refuse(
                "registry.version_conflict",
                "version already has another digest",
            ));
        }
        if *old != release.version {
            return Err(refuse(
                "registry.version_conflict",
                "build metadata does not create a new version",
            ));
        }
        return Ok(());
    }
    if known
        .iter()
        .any(|(old, _)| old.cmp_precedence(&release.version).is_ge())
    {
        return Err(refuse(
            "registry.version_not_increasing",
            "version must exceed every published version",
        ));
    }
    Ok(())
}

pub(crate) async fn upsert_publisher(tx: &Transaction<'_>, publisher: &Publisher) -> Result<()> {
    tx.execute(
        "INSERT INTO spot_control.spot_registry_publisher (id, display_name, root_key, status) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (id) DO UPDATE SET display_name = EXCLUDED.display_name, \
             root_key = EXCLUDED.root_key, status = EXCLUDED.status",
        &[
            &publisher.id,
            &publisher.display_name,
            &publisher.certificate_chain_root.as_str(),
            &status_name(&publisher.status),
        ],
    )
    .await
    .map(|_| ())
    .map_err(query_failed)
}

const INSERT_RELEASE: &str = "INSERT INTO spot_control.spot_registry_release \
    (app, version, digest, size, requires_host, published_at, publisher, yanked_at, yanked_reason, \
     withdrawn_at, withdrawn_reason, title, description, category, category_key, is_pre, \
     cap_network, cap_email, cap_files, cap_doc_read, cap_doc_write, evidence, search_tokens, \
     search_vector) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, \
            $19, $20, $21, $22::text::jsonb, $23, to_tsvector('simple', $24))";

async fn insert_release(tx: &Transaction<'_>, row: &ReleaseRow) -> Result<()> {
    let (yanked_at, yanked_reason) = row.yanked.clone().unzip();
    let (withdrawn_at, withdrawn_reason) = row.withdrawn.clone().unzip();
    let params: [Param<'_>; 24] = [
        &row.app,
        &row.version,
        &row.digest,
        &row.size,
        &row.requires_host,
        &row.published_at,
        &row.publisher,
        &yanked_at,
        &yanked_reason,
        &withdrawn_at,
        &withdrawn_reason,
        &row.title,
        &row.description,
        &row.category,
        &row.category_key,
        &row.is_pre,
        &row.cap_network,
        &row.cap_email,
        &row.cap_files,
        &row.cap_doc_read,
        &row.cap_doc_write,
        &row.evidence,
        &row.tokens,
        &row.vector_text,
    ];
    match tx.execute(INSERT_RELEASE, &params).await {
        Ok(_) => Ok(()),
        Err(error) if is_unique_violation(&error) => Err(refuse(
            "registry.version_conflict",
            "digest already indexed",
        )),
        Err(error) => Err(query_failed(error)),
    }
}

pub(crate) async fn release_by(
    tx: &Transaction<'_>,
    app: &AppName,
    version: &Version,
) -> Result<Option<Release>> {
    let sql = format!(
        "SELECT {RELEASE_COLUMNS} FROM spot_control.spot_registry_release \
         WHERE app = $1 AND version = $2"
    );
    tx.query_opt(&sql, &[&app.as_str(), &version.to_string()])
        .await
        .map_err(query_failed)?
        .as_ref()
        .map(release_from_row)
        .transpose()
}

/// `preflight`: the check alone, claims nothing.
pub(crate) async fn preflight(tx: &Transaction<'_>, release: &Release) -> Result<()> {
    check(tx, release).await
}

/// `commit`: check, then claim ownership, record the publisher and the release.
pub(crate) async fn commit(
    tx: &Transaction<'_>,
    mut release: Release,
    publisher: Publisher,
) -> Result<Committed> {
    lock_app(tx, &release.app).await?;
    check(tx, &release).await?;
    if release.publisher != publisher.id || publisher.status != PublisherStatus::Active {
        return Err(refuse(
            "registry.publisher_inactive",
            "publisher metadata mismatch",
        ));
    }
    if let Some(old) = release_by(tx, &release.app, &release.version).await? {
        return Ok(Committed {
            release: old,
            inserted: false,
        });
    }
    let taken = tx
        .query_opt(
            "SELECT 1 FROM spot_control.spot_registry_release WHERE digest = $1",
            &[&release.digest.to_hex()],
        )
        .await
        .map_err(query_failed)?;
    if taken.is_some() {
        return Err(refuse(
            "registry.version_conflict",
            "digest already indexed",
        ));
    }
    // Catalogue fields belong to the app, and survive new signed releases.
    let previous = tx
        .query_opt(
            "SELECT description, category FROM spot_control.spot_registry_release \
             WHERE app = $1 ORDER BY seq DESC LIMIT 1",
            &[&release.app.as_str()],
        )
        .await
        .map_err(query_failed)?;
    if let Some(previous) = previous {
        release.description = previous.try_get(0).map_err(query_failed)?;
        release.category = previous.try_get(1).map_err(query_failed)?;
    }
    upsert_publisher(tx, &publisher).await?;
    tx.execute(
        "INSERT INTO spot_control.spot_registry_ownership (app, publisher, generation) \
         VALUES ($1, $2, 0) ON CONFLICT (app) DO NOTHING",
        &[&release.app.as_str(), &release.publisher],
    )
    .await
    .map_err(query_failed)?;
    insert_release(tx, &ReleaseRow::of(&release)?).await?;
    bump(tx).await?;
    Ok(Committed {
        release,
        inserted: true,
    })
}

/// `remove`: mark a release once, never overwriting the first reason.
pub(crate) async fn remove(
    tx: &Transaction<'_>,
    app: &AppName,
    version: &Version,
    by: &str,
    removal: Removal,
    withdraw: bool,
) -> Result<Option<Release>> {
    lock_app(tx, app).await?;
    require_owner(tx, app, by).await?;
    let release = release_by(tx, app, version).await?.ok_or_else(missing)?;
    let marked = if withdraw {
        &release.withdrawn
    } else {
        &release.yanked
    };
    if marked.is_some() {
        return Ok(None);
    }
    let sql = if withdraw {
        "UPDATE spot_control.spot_registry_release \
         SET withdrawn_at = $3, withdrawn_reason = $4 WHERE app = $1 AND version = $2"
    } else {
        "UPDATE spot_control.spot_registry_release \
         SET yanked_at = $3, yanked_reason = $4 WHERE app = $1 AND version = $2"
    };
    tx.execute(
        sql,
        &[
            &app.as_str(),
            &version.to_string(),
            &removal.at,
            &removal.reason,
        ],
    )
    .await
    .map_err(query_failed)?;
    bump(tx).await?;
    Ok(Some(
        release_by(tx, app, version).await?.ok_or_else(missing)?,
    ))
}

/// `transfer`: compare-and-swap on the signed generation, keeping both signatures.
pub(crate) async fn transfer(
    tx: &Transaction<'_>,
    record: TransferRecord,
    recipient: Publisher,
) -> Result<()> {
    let request = &record.request;
    lock_app(tx, &request.app).await?;
    require_owner(tx, &request.app, &request.from).await?;
    active(tx, &request.to).await?;
    if recipient.id != request.to || recipient.status != PublisherStatus::Active {
        return Err(refuse("registry.transfer_invalid", "recipient mismatch"));
    }
    let owner = ownership(tx, &request.app).await?.ok_or_else(missing)?;
    if owner.generation != request.generation || request.from == request.to {
        return Err(refuse(
            "registry.transfer_replay",
            "stale ownership generation or self transfer",
        ));
    }
    let next = owner
        .generation
        .checked_add(1)
        .and_then(|g| i64::try_from(g).ok())
        .ok_or_else(|| refuse("registry.transfer_replay", "ownership generation exhausted"))?;
    let generation = i64::try_from(request.generation).map_err(|_| corrupt())?;
    upsert_publisher(tx, &recipient).await?;
    tx.execute(
        "UPDATE spot_control.spot_registry_ownership SET publisher = $2, generation = $3 \
         WHERE app = $1 AND generation = $4",
        &[&request.app.as_str(), &request.to, &next, &generation],
    )
    .await
    .map_err(query_failed)?;
    tx.execute(
        "INSERT INTO spot_control.spot_registry_transfer \
         (app, from_publisher, to_publisher, generation, at, expires_at, reason, evidence) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8::text::jsonb)",
        &[
            &request.app.as_str(),
            &request.from,
            &request.to,
            &generation,
            &request.at,
            &request.expires_at,
            &request.reason,
            &transfer_evidence(&record).to_string(),
        ],
    )
    .await
    .map_err(query_failed)?;
    bump(tx).await
}

pub(crate) async fn transfers(tx: &Transaction<'_>, app: &AppName) -> Result<Vec<TransferRecord>> {
    let sql = format!(
        "SELECT {TRANSFER_COLUMNS} FROM spot_control.spot_registry_transfer \
         WHERE app = $1 ORDER BY seq"
    );
    tx.query(&sql, &[&app.as_str()])
        .await
        .map_err(query_failed)?
        .iter()
        .map(transfer_from_row)
        .collect()
}

/// `describe`: curate the catalogue fields of every release of an owned app.
pub(crate) async fn describe(
    tx: &Transaction<'_>,
    app: &AppName,
    by: &str,
    description: String,
    category: String,
) -> Result<()> {
    lock_app(tx, app).await?;
    require_owner(tx, app, by).await?;
    let rows = tx
        .query(
            "SELECT version, title FROM spot_control.spot_registry_release WHERE app = $1",
            &[&app.as_str()],
        )
        .await
        .map_err(query_failed)?;
    if rows.is_empty() {
        return Err(missing());
    }
    let key = category.to_ascii_lowercase();
    for row in &rows {
        let version: String = row.try_get(0).map_err(query_failed)?;
        let title: String = row.try_get(1).map_err(query_failed)?;
        let words = corpus(app, &title, &description);
        let vector = words.iter().cloned().collect::<Vec<_>>().join(" ");
        let tokens: Vec<String> = words.into_iter().collect();
        tx.execute(
            "UPDATE spot_control.spot_registry_release SET description = $3, category = $4, \
                 category_key = $5, search_tokens = $6, search_vector = to_tsvector('simple', $7) \
             WHERE app = $1 AND version = $2",
            &[
                &app.as_str(),
                &version,
                &description,
                &category,
                &key,
                &tokens,
                &vector,
            ],
        )
        .await
        .map_err(query_failed)?;
    }
    bump(tx).await
}

/// `set_publisher_status`: one update; an unknown publisher is absent.
pub(crate) async fn set_publisher_status(
    tx: &Transaction<'_>,
    id: &str,
    status: &PublisherStatus,
) -> Result<()> {
    let changed = tx
        .execute(
            "UPDATE spot_control.spot_registry_publisher SET status = $2 WHERE id = $1",
            &[&id, &status_name(status)],
        )
        .await
        .map_err(query_failed)?;
    if changed == 0 {
        return Err(missing());
    }
    Ok(())
}
