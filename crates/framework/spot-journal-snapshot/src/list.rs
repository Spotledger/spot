//! Listing snapshots for the rebuild planner, and marking one verified.

use spot_journal_rebuild_plan::SnapshotInfo;
use spot_journal_store::{JournalStore, Key, Layout, SNAPSHOTS_PREFIX, StoreError, TenantId};

use crate::error::SnapshotError;
use crate::index::{INDEX_NAME, MarkerBody, SignedMarker, VERIFIED_NAME, sign_marker};
use crate::reader::read_index;
use crate::sign::{SnapshotKeys, SnapshotSigner};
use crate::writer::put_idempotent;

const PAGE: usize = 1000;

async fn marker_ok(
    store: &dyn JournalStore,
    index: &crate::index::SignedIndex,
    index_sha256: &str,
    keys: &dyn SnapshotKeys,
) -> Result<bool, StoreError> {
    let key = Layout::snapshot_object(
        &index.body.tenant,
        index.body.generation,
        index.body.position,
        VERIFIED_NAME,
    )?
    .to_key();
    Ok(store
        .get(&key)
        .await?
        .is_some_and(|o| SignedMarker::matches(&o.bytes, index, index_sha256, keys)))
}

/// The snapshots of `tenant` that have a valid index (present, in its own prefix, signature
/// verifies), ordered by `(position, generation)`. A snapshot without an index, or with one that
/// does not verify, is not listed. `verified` is true only when a signed marker names this very
/// index. Parts are not read: use [`crate::verify_deep`] for that.
///
/// # Errors
/// A store failure.
pub async fn list_snapshots(
    store: &dyn JournalStore,
    tenant: &str,
    keys: &dyn SnapshotKeys,
) -> Result<Vec<SnapshotInfo>, SnapshotError> {
    let tenant_id = TenantId::new(tenant)?;
    let prefix = format!("{tenant_id}/{SNAPSHOTS_PREFIX}/");
    let mut found = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store.list(&prefix, after.as_deref(), PAGE).await?;
        let count = page.len();
        for entry in &page {
            let Ok(Key::Snapshot(snapshot)) = Key::parse(&entry.key) else {
                continue;
            };
            if snapshot.name.as_deref() != Some(INDEX_NAME) || snapshot.tenant != tenant_id {
                continue;
            }
            let Ok((index, sha)) =
                read_index(store, tenant, snapshot.generation, snapshot.position, keys).await
            else {
                continue;
            };
            let verified = marker_ok(store, &index, &sha, keys).await?;
            found.push(index.info(verified));
        }
        after = page.last().map(|e| e.key.clone());
        if count < PAGE {
            break;
        }
    }
    found.sort_by_key(|s| (s.position, s.generation));
    Ok(found)
}

/// Records that the rebuild test verified this snapshot: writes a signed `verified` object beside
/// the index, bound to the index's hash. Idempotent.
///
/// # Errors
/// The index errors of [`crate::SnapshotReader::open`], or a store failure.
pub async fn mark_verified(
    store: &dyn JournalStore,
    tenant: &str,
    generation: u64,
    position: u64,
    keys: &dyn SnapshotKeys,
    signer: &dyn SnapshotSigner,
) -> Result<(), SnapshotError> {
    let (index, sha) = read_index(store, tenant, generation, position, keys).await?;
    if marker_ok(store, &index, &sha, keys).await? {
        return Ok(());
    }
    let marker = sign_marker(
        MarkerBody {
            tenant: tenant.to_owned(),
            generation,
            position,
            index_sha256: sha,
        },
        signer,
    )?;
    let bytes = serde_json::to_vec(&marker)
        .map_err(|_| SnapshotError::IndexInvalid("the marker cannot be encoded"))?;
    let key = Layout::snapshot_object(tenant, generation, position, VERIFIED_NAME)?.to_key();
    put_idempotent(store, &key, bytes).await
}
