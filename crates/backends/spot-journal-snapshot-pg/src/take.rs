//! Taking one snapshot: read a consistent image, write the parts, **wait until the journal has
//! reached the snapshot's position**, then write the signed index last (the writer's rule: a
//! snapshot with no index is not listed).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use spot_journal_rebuild_plan::SnapshotInfo;
use spot_journal_snapshot::{PartTransform, SnapshotParams, SnapshotSigner, SnapshotWriter};
use spot_journal_store::JournalStore;
use spot_postgres::PgBackend;
use spot_storage_contract::Catalog;
use std::collections::BTreeSet;

use crate::error::SnapshotPgError;
use crate::read::{Head, ImageReader};

/// Answers whether the journal has made `(generation, seq)` durable, waiting up to `timeout`.
pub trait JournalReach: Send + Sync {
    /// `true` once the shipped watermark is at or beyond the position.
    fn reached<'a>(
        &'a self,
        generation: u64,
        seq: u64,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
}

/// What one snapshot needs besides the store and the database.
pub struct TakeSettings {
    /// The tenant (the site name): the only prefix the snapshot is written under.
    pub tenant: String,
    /// Hash of the manifest the read was taken against (64 lower-case hex digits).
    pub manifest_hash: String,
    /// When the snapshot is taken, milliseconds since the Unix epoch.
    pub taken_at_millis: i64,
    /// Records per page.
    pub page_size: i64,
    /// The read's statement timeout.
    pub statement_timeout: Duration,
    /// How long to wait for the journal to reach the snapshot's position.
    pub reach_timeout: Duration,
    /// Entities outside the recoverable record.
    pub excluded: BTreeSet<String>,
}

/// Takes a snapshot and returns what the writer reports.
///
/// # Errors
/// [`SnapshotPgError`]; a failed snapshot writes no index, so nothing is listed.
#[allow(clippy::too_many_arguments)]
pub async fn take_snapshot(
    backend: &PgBackend,
    catalog: &Arc<Catalog>,
    store: &dyn JournalStore,
    signer: &dyn SnapshotSigner,
    transform: &dyn PartTransform,
    reach: &dyn JournalReach,
    settings: &TakeSettings,
) -> Result<SnapshotInfo, SnapshotPgError> {
    let mut reader = ImageReader::open(
        backend,
        catalog,
        &settings.excluded,
        settings.page_size,
        settings.statement_timeout,
    )
    .await?;
    let Head { generation, seq } = reader.head();
    let params = SnapshotParams {
        tenant: settings.tenant.clone(),
        generation,
        position: seq,
        manifest_hash: settings.manifest_hash.clone(),
        taken_at_millis: settings.taken_at_millis,
    };
    let mut writer = SnapshotWriter::new(store, params, signer)?.with_transform(transform);
    while let Some(page) = reader.next_page().await? {
        writer.write_part(page).await?;
    }
    reader.close().await;
    // The index is written last, and only once the journal holds everything up to the position:
    // a snapshot the journal cannot back would be a base image nothing could replay from.
    if !reach.reached(generation, seq, settings.reach_timeout).await {
        return Err(SnapshotPgError::JournalBehind);
    }
    Ok(writer.finish().await?)
}
