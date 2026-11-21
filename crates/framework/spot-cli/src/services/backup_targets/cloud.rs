//! The cloud target for `backup_targets` (WP-PRD-04.T5): sealed, chunked, resumable uploads
//! over the `spot_cloud_backup::ObjectStore` port.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use spot_backup::{BackupError, CloudStore};
use spot_cloud_backup::crypto::{chunk_aad, decrypt, encrypt, sha256_hex};
use spot_cloud_backup::{CHUNK_SIZE, ObjectStore, PutOutcome, RecoveryKey};

use super::{BResult, Outcome, segment_ok};

/// The sealed manifest object's name below a package's key.
const MANIFEST_NAME: &str = "manifest";
/// The manifest format written by [`ChunkedCloudStore`].
const CLOUD_FORMAT: &str = "spot-cli-cloud/1";

// ---------------------------------------------------------------------------------------------
// Cloud
// ---------------------------------------------------------------------------------------------

/// The access secret read from the secret store. Never printed.
pub struct Credential(String);

impl Credential {
    /// Wraps the secret text.
    pub fn new(secret: String) -> Self {
        Self(secret)
    }

    /// The secret, for the connector only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential(***)")
    }
}

/// Reads a credential from the secret store by its reference (`secret:<name>`).
pub trait CredentialSource: Send + Sync {
    /// The secret behind `reference`.
    ///
    /// # Errors
    /// `backup.encryption_unavailable` when the store does not have it.
    fn credential(&self, reference: &str) -> Outcome<Credential>;
}

/// Opens an S3-compatible endpoint as an object store (TLS is always verified; there is no
/// switch to turn that off).
pub trait CloudConnector: Send + Sync {
    /// A store for `endpoint` and `key_id` using `credential`.
    ///
    /// # Errors
    /// `backup.target_unavailable` when the endpoint cannot be reached.
    fn connect(
        &self,
        endpoint: &str,
        key_id: &str,
        credential: &Credential,
    ) -> Outcome<Arc<dyn ObjectStore>>;
}

/// KiB per second to the milliseconds a chunk of `bytes` must take; integer arithmetic only.
pub fn throttle_delay_ms(bytes: u64, max_kbps: u32) -> u64 {
    let per_second = u64::from(max_kbps).max(1).saturating_mul(1024);
    bytes.saturating_mul(1000) / per_second
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkRef {
    digest: String,
    len: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloudManifest {
    format: String,
    name: String,
    total: u64,
    digest: String,
    chunks: Vec<ChunkRef>,
}

fn store_error(error: impl fmt::Display) -> BackupError {
    BackupError::io(format!("cloud store: {error}"))
}

fn object_key(key: &str, index: u32) -> String {
    format!("{key}/c{index:08}")
}

/// Runs `future` to completion on a private runtime in its own thread, so it is safe inside any
/// caller, async or not.
fn block<F, T>(future: F) -> BResult<T>
where
    F: Future<Output = BResult<T>> + Send,
    T: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(move || -> BResult<T> {
                tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .map_err(|_| BackupError::io("the cloud runtime could not start"))?
                    .block_on(future)
            })
            .join()
            .unwrap_or_else(|_| Err(BackupError::io("the cloud transfer stopped unexpectedly")))
    })
}

/// A [`CloudStore`] over an [`ObjectStore`]: every package becomes sealed 4 MiB chunks plus a
/// sealed manifest, all under `<key>/`.
///
/// The key is the tenant's. Each chunk is sealed with an authenticated cipher bound to the package
/// key and the chunk index, so a swapped, reordered or foreign chunk fails to open. The manifest
/// is written last: a package without one does not exist for `list` and an interrupted upload is
/// resumed by the next put of the same key.
pub struct ChunkedCloudStore {
    store: Arc<dyn ObjectStore>,
    key: RecoveryKey,
    scope: String,
    max_kbps: Option<u32>,
    quota_bytes: Option<u64>,
    chunk: usize,
}

impl fmt::Debug for ChunkedCloudStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkedCloudStore")
            .field("scope", &self.scope)
            .field("max_kbps", &self.max_kbps)
            .finish_non_exhaustive()
    }
}

impl ChunkedCloudStore {
    /// A store over `store`, sealing with `key`. `scope` is the prefix quota is counted under.
    pub fn new(
        store: Arc<dyn ObjectStore>,
        key: RecoveryKey,
        scope: impl Into<String>,
        max_kbps: Option<u32>,
        quota_bytes: Option<u64>,
    ) -> Self {
        Self {
            store,
            key,
            scope: scope.into(),
            max_kbps,
            quota_bytes,
            chunk: usize::try_from(CHUNK_SIZE).unwrap_or(4 * 1024 * 1024),
        }
    }

    /// The same store with a smaller chunk size, for tests.
    #[must_use]
    pub fn with_chunk_bytes(mut self, bytes: usize) -> Self {
        self.chunk = bytes.max(1);
        self
    }

    async fn pace(&self, bytes: usize) {
        if let Some(kbps) = self.max_kbps {
            let wait = throttle_delay_ms(u64::try_from(bytes).unwrap_or(u64::MAX), kbps);
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    }

    async fn existing(&self, object: &str) -> BResult<Option<Vec<u8>>> {
        if self.store.exists(object).await.map_err(store_error)? {
            Ok(Some(self.store.get(object).await.map_err(store_error)?))
        } else {
            Ok(None)
        }
    }

    /// Puts one sealed object, replacing an existing object that is not ours; returns its digest.
    async fn put_sealed(&self, object: &str, aad: &[u8], plain: &[u8]) -> BResult<(String, u64)> {
        if let Some(found) = self.existing(object).await? {
            let same =
                decrypt(self.key.expose(), aad, &found).is_some_and(|p| p.as_slice() == plain);
            if same {
                return Ok((sha256_hex(&found), u64::try_from(found.len()).unwrap_or(0)));
            }
            self.store.delete(object).await.map_err(store_error)?;
        }
        let sealed = encrypt(self.key.expose(), aad, plain)
            .ok_or_else(|| BackupError::io("a chunk could not be sealed"))?;
        let digest = sha256_hex(&sealed);
        let len = u64::try_from(sealed.len()).unwrap_or(u64::MAX);
        match self.store.put(object, sealed).await.map_err(store_error)? {
            PutOutcome::Stored => {}
            PutOutcome::Exists => {
                return Err(BackupError::io("another writer holds the same object"));
            }
        }
        let back = self.store.get(object).await.map_err(store_error)?;
        if sha256_hex(&back) != digest {
            return Err(BackupError::VerifyFailed {
                reason: "an uploaded chunk read back with a different digest".to_owned(),
            });
        }
        Ok((digest, len))
    }

    async fn upload(&self, key: &str, bytes: &[u8]) -> BResult<()> {
        let mut chunks = Vec::new();
        for (index, plain) in bytes.chunks(self.chunk).enumerate() {
            let index = u32::try_from(index)
                .map_err(|_| BackupError::io("the package has too many chunks"))?;
            let (digest, len) = self
                .put_sealed(&object_key(key, index), &chunk_aad(key, index), plain)
                .await?;
            chunks.push(ChunkRef { digest, len });
            self.pace(plain.len()).await;
        }
        let manifest = CloudManifest {
            format: CLOUD_FORMAT.to_owned(),
            name: key.to_owned(),
            total: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            digest: sha256_hex(bytes),
            chunks,
        };
        let json = serde_json::to_vec(&manifest)
            .map_err(|_| BackupError::io("the cloud manifest could not be written"))?;
        // The manifest is the commit point; a stale one from an earlier try is replaced.
        let _ = self
            .put_sealed(
                &format!("{key}/{MANIFEST_NAME}"),
                &chunk_aad(key, u32::MAX),
                &json,
            )
            .await?;
        Ok(())
    }

    async fn manifest(&self, key: &str) -> BResult<CloudManifest> {
        let object = format!("{key}/{MANIFEST_NAME}");
        let sealed = self
            .existing(&object)
            .await?
            .ok_or_else(|| BackupError::NotFound {
                id: key.rsplit('/').next().unwrap_or(key).to_owned(),
            })?;
        let plain =
            decrypt(self.key.expose(), &chunk_aad(key, u32::MAX), &sealed).ok_or_else(|| {
                BackupError::DecryptFailed {
                    reason:
                        "the cloud manifest does not authenticate; this is not the tenant's key"
                            .to_owned(),
                }
            })?;
        let manifest: CloudManifest =
            serde_json::from_slice(&plain).map_err(|_| BackupError::ManifestInvalid {
                reason: "the cloud manifest does not parse".to_owned(),
            })?;
        if manifest.format != CLOUD_FORMAT || manifest.name != key {
            return Err(BackupError::ManifestInvalid {
                reason: "the cloud manifest is for another package".to_owned(),
            });
        }
        Ok(manifest)
    }

    async fn download(&self, key: &str) -> BResult<Vec<u8>> {
        let manifest = self.manifest(key).await?;
        let mut out = Vec::new();
        for (position, reference) in manifest.chunks.iter().enumerate() {
            let index = u32::try_from(position)
                .map_err(|_| BackupError::io("the package has too many chunks"))?;
            let sealed = self
                .store
                .get(&object_key(key, index))
                .await
                .map_err(store_error)?;
            if sha256_hex(&sealed) != reference.digest {
                return Err(BackupError::VerifyFailed {
                    reason: format!("chunk {index} does not match its recorded digest"),
                });
            }
            let plain =
                decrypt(self.key.expose(), &chunk_aad(key, index), &sealed).ok_or_else(|| {
                    BackupError::DecryptFailed {
                        reason: format!("chunk {index} does not authenticate"),
                    }
                })?;
            out.extend_from_slice(&plain);
        }
        if u64::try_from(out.len()).ok() != Some(manifest.total)
            || sha256_hex(&out) != manifest.digest
        {
            return Err(BackupError::VerifyFailed {
                reason: "the reassembled package does not match its recorded digest".to_owned(),
            });
        }
        Ok(out)
    }
}

fn key_ok(key: &str) -> BResult<()> {
    if key.is_empty() || key.len() > 400 || !key.split('/').all(segment_ok) {
        return Err(BackupError::ManifestInvalid {
            reason: "the cloud key is not usable".to_owned(),
        });
    }
    Ok(())
}

impl CloudStore for ChunkedCloudStore {
    fn put_object(&self, key: &str, bytes: &[u8]) -> BResult<()> {
        key_ok(key)?;
        block(self.upload(key, bytes))
    }

    fn get_object(&self, key: &str) -> BResult<Vec<u8>> {
        key_ok(key)?;
        block(self.download(key))
    }

    fn list_objects(&self, prefix: &str) -> BResult<Vec<String>> {
        block(async {
            let listed = self.store.list(prefix).await.map_err(store_error)?;
            let suffix = format!("/{MANIFEST_NAME}");
            let mut keys: Vec<String> = listed
                .into_iter()
                .filter_map(|o| o.key.strip_suffix(&suffix).map(str::to_owned))
                .collect();
            keys.sort();
            keys.dedup();
            Ok(keys)
        })
    }

    fn delete_object(&self, key: &str) -> BResult<()> {
        key_ok(key)?;
        block(async {
            let manifest = format!("{key}/{MANIFEST_NAME}");
            if !self.store.exists(&manifest).await.map_err(store_error)? {
                return Err(BackupError::NotFound {
                    id: key.rsplit('/').next().unwrap_or(key).to_owned(),
                });
            }
            // Manifest first: the package leaves `list` before its chunks go.
            self.store.delete(&manifest).await.map_err(store_error)?;
            for object in self
                .store
                .list(&format!("{key}/"))
                .await
                .map_err(store_error)?
            {
                self.store.delete(&object.key).await.map_err(store_error)?;
            }
            Ok(())
        })
    }

    fn remaining_bytes(&self) -> BResult<u64> {
        let Some(quota) = self.quota_bytes else {
            return Ok(u64::MAX);
        };
        block(async {
            let used: u64 = self
                .store
                .list(&self.scope)
                .await
                .map_err(store_error)?
                .iter()
                .map(|o| o.bytes)
                .fold(0, u64::saturating_add);
            Ok(quota.saturating_sub(used))
        })
    }
}
