//! The `spot-snapshot` transfer adapter.
//!
//! [`SnapshotTransfer`] takes one logical dump of the source through a
//! [`spot_snapshot::SnapshotProvider`], verifies its manifest, and streams the snapshot
//! files to the engine as checksummed [`Chunk`]s. The import pass writes the chunks into a
//! staging directory and, after the last one, opens, verifies and restores that snapshot
//! into the target through the same provider. A snapshot is tenant data (ADR-058 rule 1):
//! the adapter only ever writes to the destination and staging directories it was given.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_snapshot::hash::is_safe_relative;
use spot_snapshot::manifest::MANIFEST_NAME;
use spot_snapshot::{
    Destination, Progress, ProgressSink, RestoreOptions, RestoreTarget, Snapshot, SnapshotOptions,
    SnapshotProvider, SnapshotTarget,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::plan::MovePlan;
use crate::transfer::{Chunk, TransferPort};

/// The adapter's chunk size: 256 KiB. It stays below the engine's accounting bound.
const CHUNK_BYTES: u64 = 256 * 1024;

/// One file of the dump and its size.
#[derive(Debug, Clone)]
struct Span {
    file: String,
    size: u64,
}

/// One chunk's position in the dump.
struct Located {
    file: String,
    offset: u64,
    length: usize,
    last: bool,
}

struct Inner {
    snapshot: Option<Snapshot>,
    spans: Vec<Span>,
    staging: PathBuf,
}

/// The `spot-snapshot` logical dump/restore adapter.
pub struct SnapshotTransfer {
    provider: Arc<dyn SnapshotProvider>,
    source: SnapshotTarget,
    destination: Destination,
    target: RestoreTarget,
    snapshot_options: SnapshotOptions,
    restore_options: RestoreOptions,
    inner: Mutex<Inner>,
}

impl SnapshotTransfer {
    /// An adapter that takes a dump of `source` into `destination`, imports through
    /// `staging` and restores into `target` with default options.
    ///
    /// `destination` and `staging` must be absent or empty: a snapshot is never taken or
    /// restored over existing data.
    #[must_use]
    pub fn new(
        provider: Arc<dyn SnapshotProvider>,
        source: SnapshotTarget,
        destination: Destination,
        staging: PathBuf,
        target: RestoreTarget,
    ) -> Self {
        Self {
            provider,
            source,
            destination,
            target,
            snapshot_options: SnapshotOptions::default(),
            restore_options: RestoreOptions::default(),
            inner: Mutex::new(Inner {
                snapshot: None,
                spans: Vec::new(),
                staging,
            }),
        }
    }

    /// Replaces the take and restore options.
    #[must_use]
    pub fn with_options(
        mut self,
        snapshot_options: SnapshotOptions,
        restore_options: RestoreOptions,
    ) -> Self {
        self.snapshot_options = snapshot_options;
        self.restore_options = restore_options;
        self
    }

    async fn take(&self, tenant: &str) -> Result<Snapshot> {
        self.provider
            .take(
                &self.source,
                &self.destination,
                &self.snapshot_options,
                Arc::new(Silent),
            )
            .await
            .map(|outcome| outcome.snapshot)
            .map_err(|_| Error::TransferFailed {
                tenant: tenant.to_owned(),
                reason: "the snapshot take failed",
            })
    }

    async fn spans(&self, tenant: &str, snapshot: &Snapshot) -> Result<Vec<Span>> {
        self.provider
            .verify(snapshot)
            .await
            .map_err(|_| Error::TransferFailed {
                tenant: tenant.to_owned(),
                reason: "the snapshot did not verify",
            })?;
        let dir = snapshot.dir();
        let mut spans = vec![Span {
            file: MANIFEST_NAME.to_owned(),
            size: file_size(&dir.join(MANIFEST_NAME)).await?,
        }];
        for (path, checksum) in &snapshot.manifest().files {
            if !is_safe_relative(path) {
                return Err(Error::TransferFailed {
                    tenant: tenant.to_owned(),
                    reason: "the snapshot manifest names an unsafe path",
                });
            }
            spans.push(Span {
                file: path.clone(),
                size: checksum.bytes,
            });
        }
        Ok(spans)
    }
}

impl TransferPort for SnapshotTransfer {
    fn export_chunk<'a>(
        &'a self,
        plan: &'a MovePlan,
        after: Option<u64>,
    ) -> crate::BoxFuture<'a, Result<Option<Chunk>>> {
        Box::pin(async move {
            let tenant = plan.tenant.to_string();
            let (dir, spans) = {
                let mut inner = self.inner.lock().await;
                if inner.snapshot.is_none() {
                    let snapshot = self.take(&tenant).await?;
                    inner.snapshot = Some(snapshot);
                }
                if inner.spans.is_empty() {
                    let snapshot = inner.snapshot.as_ref().ok_or(Error::TransferFailed {
                        tenant: tenant.clone(),
                        reason: "the snapshot handle is missing",
                    })?;
                    inner.spans = self.spans(&tenant, snapshot).await?;
                }
                let dir = inner
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.dir().to_path_buf())
                    .ok_or(Error::TransferFailed {
                        tenant: tenant.clone(),
                        reason: "the snapshot handle is missing",
                    })?;
                (dir, inner.spans.clone())
            };
            let index = after.map_or(0, |sequence| sequence.saturating_add(1));
            let Some(located) = locate(&spans, index) else {
                return Ok(None);
            };
            let mut bytes = vec![0u8; located.length];
            if located.length > 0 {
                let mut file = tokio::fs::File::open(dir.join(&located.file))
                    .await
                    .map_err(|_| Error::TransferFailed {
                        tenant: tenant.clone(),
                        reason: "a snapshot file could not be opened",
                    })?;
                file.seek(std::io::SeekFrom::Start(located.offset))
                    .await
                    .map_err(|_| Error::TransferFailed {
                        tenant: tenant.clone(),
                        reason: "a snapshot file could not be positioned",
                    })?;
                file.read_exact(&mut bytes)
                    .await
                    .map_err(|_| Error::TransferFailed {
                        tenant: tenant.clone(),
                        reason: "a snapshot file could not be read",
                    })?;
            }
            Ok(Some(Chunk::new(
                index,
                located.file,
                located.offset,
                bytes,
                located.last,
            )))
        })
    }

    fn import_chunk<'a>(
        &'a self,
        plan: &'a MovePlan,
        chunk: &'a Chunk,
    ) -> crate::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let tenant = plan.tenant.to_string();
            if !is_safe_relative(&chunk.file) {
                return Err(Error::TransferFailed {
                    tenant,
                    reason: "a transfer chunk names an unsafe path",
                });
            }
            let staging = self.inner.lock().await.staging.clone();
            let path = staging.join(&chunk.file);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|_| Error::TransferFailed {
                        tenant: tenant.clone(),
                        reason: "the staging directory could not be created",
                    })?;
            }
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .await
                .map_err(|_| Error::TransferFailed {
                    tenant: tenant.clone(),
                    reason: "a staging file could not be opened",
                })?;
            file.seek(std::io::SeekFrom::Start(chunk.offset))
                .await
                .map_err(|_| Error::TransferFailed {
                    tenant: tenant.clone(),
                    reason: "a staging file could not be positioned",
                })?;
            file.write_all(&chunk.bytes)
                .await
                .map_err(|_| Error::TransferFailed {
                    tenant,
                    reason: "a staging file could not be written",
                })?;
            Ok(())
        })
    }

    fn finish_import<'a>(&'a self, plan: &'a MovePlan) -> crate::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let tenant = plan.tenant.to_string();
            let staging = self.inner.lock().await.staging.clone();
            let snapshot = Snapshot::open(&staging)
                .await
                .map_err(|_| Error::TransferFailed {
                    tenant: tenant.clone(),
                    reason: "the imported snapshot could not be opened",
                })?;
            self.provider
                .verify(&snapshot)
                .await
                .map_err(|_| Error::TransferFailed {
                    tenant: tenant.clone(),
                    reason: "the imported snapshot did not verify",
                })?;
            self.provider
                .restore(
                    &snapshot,
                    &self.target,
                    &self.restore_options,
                    Arc::new(Silent),
                )
                .await
                .map_err(|_| Error::TransferFailed {
                    tenant,
                    reason: "the imported snapshot could not be restored",
                })?;
            Ok(())
        })
    }

    fn cleanup<'a>(&'a self, plan: &'a MovePlan) -> crate::BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let tenant = plan.tenant.to_string();
            let (snapshot, staging) = {
                let mut inner = self.inner.lock().await;
                inner.spans.clear();
                (inner.snapshot.take(), inner.staging.clone())
            };
            if let Some(snapshot) = snapshot {
                self.provider
                    .delete(&snapshot)
                    .await
                    .map_err(|_| Error::TransferFailed {
                        tenant: tenant.clone(),
                        reason: "the exported snapshot could not be deleted",
                    })?;
            }
            match tokio::fs::remove_dir_all(&staging).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(Error::TransferFailed {
                        tenant,
                        reason: "the staging directory could not be removed",
                    });
                }
            }
            Ok(())
        })
    }
}

/// The chunk at `index`, if the dump has one.
fn locate(spans: &[Span], index: u64) -> Option<Located> {
    let mut remaining = index;
    for (position, span) in spans.iter().enumerate() {
        let count = span.size.div_ceil(CHUNK_BYTES).max(1);
        if remaining < count {
            let offset = remaining.saturating_mul(CHUNK_BYTES);
            let available = span.size.saturating_sub(offset);
            let length = usize::try_from(available.min(CHUNK_BYTES)).unwrap_or(0);
            return Some(Located {
                file: span.file.clone(),
                offset,
                length,
                last: position + 1 == spans.len() && remaining + 1 == count,
            });
        }
        remaining -= count;
    }
    None
}

async fn file_size(path: &Path) -> Result<u64> {
    tokio::fs::metadata(path)
        .await
        .map(|metadata| metadata.len())
        .map_err(|_| Error::TransferFailed {
            tenant: String::new(),
            reason: "a snapshot file's size could not be read",
        })
}

/// The adapter ignores progress: the engine's persisted cursor is the progress.
struct Silent;

impl ProgressSink for Silent {
    fn on(&self, _event: Progress) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn span(file: &str, size: u64) -> Span {
        Span {
            file: file.to_owned(),
            size,
        }
    }

    #[test]
    fn the_chunk_positions_cover_every_file_and_end_once() {
        let spans = vec![span("snapshot.toml", 10), span("dump.bin", 300 * 1024)];
        let first = locate(&spans, 0).unwrap();
        assert_eq!(
            (first.file.as_str(), first.offset, first.last),
            ("snapshot.toml", 0, false)
        );
        let second = locate(&spans, 1).unwrap();
        assert_eq!((second.file.as_str(), second.offset), ("dump.bin", 0));
        assert_eq!(second.length, 256 * 1024);
        let third = locate(&spans, 2).unwrap();
        assert_eq!(third.offset, 256 * 1024);
        assert!(third.last);
        assert!(locate(&spans, 3).is_none());
    }

    #[test]
    fn an_empty_file_still_has_one_chunk() {
        let spans = vec![span("empty", 0)];
        let only = locate(&spans, 0).unwrap();
        assert_eq!(only.length, 0);
        assert!(only.last);
        assert!(locate(&spans, 1).is_none());
    }
}
