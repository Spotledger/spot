//! Post-upload verification: re-read chunks, check their digests and open them.
//!
//! After an upload the client re-reads a random sample of the chunks it just wrote and
//! checks each one's SHA-256 against the manifest and opens it with the data key. A full
//! verification mode checks every chunk. A missing or corrupted chunk is a
//! [`ChunkFailure`] the caller retries; a backup that still fails after the retry budget
//! is recorded as [`BackupState::Unverified`](crate::manifest::BackupState::Unverified)
//! and is never presented as restorable.
//!
//! The sample is drawn from the operating system RNG, not the clock; verification of a
//! given list of indices is pure.

use crate::chunk;
use crate::crypto::{self, DataKey};
use crate::error::Result;
use crate::manifest::{BackupManifest, BackupState};
use crate::store::ObjectReader;

/// How much of a backup verification reads back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyMode {
    /// Re-read `chunks` random chunks.
    Sample {
        /// How many chunks to sample; clamped to the backup's chunk count.
        chunks: u32,
    },
    /// Re-read every chunk.
    Full,
}

impl Default for VerifyMode {
    fn default() -> Self {
        Self::Sample { chunks: 3 }
    }
}

/// Why one chunk failed verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkFailureReason {
    /// The store does not hold the chunk.
    Missing,
    /// The ciphertext digest differs from the manifest's.
    Digest,
    /// The chunk does not open under the data key (a tag or length mismatch).
    Decrypt,
}

impl ChunkFailureReason {
    /// A short stable name for reports.
    pub fn name(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Digest => "digest",
            Self::Decrypt => "decrypt",
        }
    }
}

/// One chunk that failed verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkFailure {
    /// The chunk index.
    pub index: u32,
    /// What failed.
    pub reason: ChunkFailureReason,
}

/// The outcome of one verification pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    /// The mode that was run.
    pub mode: VerifyMode,
    /// How many chunks the backup has.
    pub chunks_total: u64,
    /// How many chunks were actually read back.
    pub chunks_checked: u64,
    /// The chunks that failed, in index order.
    pub failures: Vec<ChunkFailure>,
}

impl VerifyReport {
    /// Whether every checked chunk passed.
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// The state a backup should record for this report, at `now`-independent wording.
    pub fn state(&self) -> BackupState {
        if self.failures.is_empty() {
            BackupState::Verified
        } else {
            BackupState::Unverified {
                reason: self.reason(),
            }
        }
    }

    /// A plain-language reason naming the first failing chunks, never a key or plaintext.
    pub fn reason(&self) -> String {
        let mut parts: Vec<String> = self
            .failures
            .iter()
            .take(4)
            .map(|failure| format!("chunk {} is {}", failure.index, failure.reason.name()))
            .collect();
        if self.failures.len() > parts.len() {
            parts.push(format!("and {} more", self.failures.len() - parts.len()));
        }
        format!(
            "{} of {} checked chunks failed: {}",
            self.failures.len(),
            self.chunks_checked,
            parts.join(", ")
        )
    }
}

/// Re-reads `chunks` chunks (or every chunk in [`VerifyMode::Full`]) and checks them.
///
/// # Errors
/// [`crate::error::CloudBackupError::Store`] only when sampling the RNG fails; a missing
/// or corrupt chunk is a report entry, not an error, because the caller retries it.
pub async fn verify(
    reader: &dyn ObjectReader,
    manifest: &BackupManifest,
    data_key: &DataKey,
    mode: VerifyMode,
) -> Result<VerifyReport> {
    let total = manifest.chunks.len();
    let indices: Vec<usize> = match mode {
        VerifyMode::Full => (0..total).collect(),
        VerifyMode::Sample { chunks } => {
            let wanted = usize::try_from(chunks).unwrap_or(total).min(total);
            sample_indices(total, wanted)?
        }
    };
    let mut failures = Vec::new();
    let mut checked = 0u64;
    for position in indices {
        let Some(entry) = manifest.chunks.get(position) else {
            continue;
        };
        checked = checked.saturating_add(1);
        let bytes = match reader.read(&entry.object).await {
            Ok(bytes) => bytes,
            Err(_) => {
                failures.push(ChunkFailure {
                    index: entry.index,
                    reason: ChunkFailureReason::Missing,
                });
                continue;
            }
        };
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != entry.cipher_len {
            failures.push(ChunkFailure {
                index: entry.index,
                reason: ChunkFailureReason::Digest,
            });
            continue;
        }
        match chunk::open(
            &manifest.backup_id,
            data_key,
            entry.index,
            &entry.sha256,
            &bytes,
        ) {
            Ok(plain) if u64::try_from(plain.len()).unwrap_or(u64::MAX) == entry.plain_len => {}
            Ok(_) => failures.push(ChunkFailure {
                index: entry.index,
                reason: ChunkFailureReason::Digest,
            }),
            Err(error) => failures.push(ChunkFailure {
                index: entry.index,
                reason: if error.code() == "cloud_backup.chunk_missing" {
                    ChunkFailureReason::Missing
                } else {
                    ChunkFailureReason::Decrypt
                },
            }),
        }
    }
    failures.sort_by_key(|failure| failure.index);
    Ok(VerifyReport {
        mode,
        chunks_total: u64::try_from(total).unwrap_or(u64::MAX),
        chunks_checked: checked,
        failures,
    })
}

/// Up to `wanted` distinct indices from `0..total`, drawn from the OS RNG.
fn sample_indices(total: usize, wanted: usize) -> Result<Vec<usize>> {
    if wanted >= total {
        return Ok((0..total).collect());
    }
    let mut chosen: Vec<usize> = Vec::with_capacity(wanted);
    while chosen.len() < wanted {
        let mut raw = [0u8; 8];
        crypto::random_bytes(&mut raw)?;
        let value = u64::from_be_bytes(raw);
        let candidate = usize::try_from(value % u64::try_from(total).unwrap_or(1)).unwrap_or(0);
        if !chosen.contains(&candidate) {
            chosen.push(candidate);
        }
    }
    chosen.sort_unstable();
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sample_larger_than_the_backup_is_everything() {
        assert_eq!(sample_indices(2, 3).expect("sample"), vec![0, 1]);
        assert!(sample_indices(0, 0).expect("sample").is_empty());
    }

    #[test]
    fn samples_are_distinct_and_in_range() {
        let sample = sample_indices(10, 5).expect("sample");
        assert_eq!(sample.len(), 5);
        let mut unique = sample.clone();
        unique.dedup();
        assert_eq!(unique.len(), 5);
        assert!(sample.iter().all(|index| *index < 10));
    }
}
