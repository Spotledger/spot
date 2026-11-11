//! The local spool every put streams its body through on the way to the backend: the
//! bytes land on disk while they hash, so no upload — small or large — is ever held in
//! memory, and a failed, over-limit or interrupted put leaves no spill behind.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use sha2::{Digest, Sha256};

use crate::error::FileError;
use crate::store::FileResult;

/// A local spool file: its path in the system temp directory, removed when the put ends.
pub(super) struct Spool {
    /// The spool file's path in the system temp directory.
    pub(super) path: PathBuf,
}

impl Spool {
    /// Names a uniquely fresh spool file in the system temp directory. The name mixes
    /// the process id, the clock's nanos and a process-wide counter, so concurrent
    /// puts never collide even within the same nanosecond.
    pub(super) fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        Spool {
            path: std::env::temp_dir().join(format!(
                "spot-files-{}-{}-{}.spool",
                std::process::id(),
                nanos,
                COUNTER.fetch_add(1, Ordering::Relaxed),
            )),
        }
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        // A missing spool is exactly where a finished put wants its file; drop cannot
        // report errors, so the removal stays best-effort.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// An [`tokio::io::AsyncWrite`] sink [`stream_hashed`](crate::store::stream_hashed)
/// writes a put through: every chunk is appended to the spool file and folded into the
/// rolling SHA-256 in the same order, and no retried chunk is counted twice.
pub(super) struct SpoolSink {
    /// The open spool file.
    file: tokio::fs::File,
    /// The rolling digest over the bytes written so far.
    hasher: Sha256,
}

impl SpoolSink {
    /// Creates the spool file exclusively, starting a fresh digest. `create_new` so a
    /// pre-existing path — including a symlink planted by another user in the shared
    /// temp directory — is an error rather than a write-through (lead review of #446).
    pub(super) async fn new(spool: &Spool) -> FileResult<Self> {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Tenant file bytes sit in the shared temp directory while the put runs, so only
        // the owner may read them (lead review of #446).
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&spool.path)
            .await
            .map_err(|e| FileError::io(format!("creating the spool file: {e}")))?;
        Ok(Self {
            file,
            hasher: Sha256::new(),
        })
    }
}

impl tokio::io::AsyncWrite for SpoolSink {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        // Only a completed write hashes its bytes: a chunk whose write errored or is
        // still pending may come back around again.
        match Pin::new(&mut this.file).poll_write(cx, buf) {
            Poll::Ready(Ok(bytes)) => {
                this.hasher.update(&buf[..bytes]);
                Poll::Ready(Ok(bytes))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().file).poll_shutdown(cx)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn the_spool_file_is_readable_by_its_owner_only() {
        let spool = Spool::new();
        let _sink = SpoolSink::new(&spool)
            .await
            .unwrap_or_else(|e| panic!("create the spool: {e}"));
        let mode = std::fs::metadata(&spool.path)
            .unwrap_or_else(|e| panic!("spool metadata: {e}"))
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "spool mode was {mode:o}");
    }
}
