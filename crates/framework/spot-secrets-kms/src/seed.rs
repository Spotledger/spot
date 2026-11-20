//! The derivation seed: 32 random bytes, generated once, stored wrapped under the KMS key in a
//! small JSON envelope, unwrapped at start and held only in zeroising memory.
//!
//! A remote KMS never reveals its key, so blind-index and derived keys cannot come from the KEK.
//! They come from this seed instead. The seed belongs to a key *id*, not to a transit version.

use crate::client::{KmsCiphertext, KmsClient};
use crate::error::KmsError;
use crate::fsutil::read_private;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rand::TryRngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

/// Length of the seed, in bytes.
pub(crate) const SEED_LEN: usize = 32;
/// The envelope's format tag.
const FORMAT: &str = "spot-kms-seed-v1";
/// The longest seed file, in bytes.
const MAX_FILE: u64 = 8 * 1024;

/// A source of random bytes. Injected, so tests are deterministic and the crate never reaches
/// for the operating system's generator on its own.
pub trait Entropy: Send + Sync {
    /// Fills `buf` with random bytes.
    fn fill(&self, buf: &mut [u8]) -> Result<(), KmsError>;
}

/// The operating system's random generator.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&self, buf: &mut [u8]) -> Result<(), KmsError> {
        OsRng
            .try_fill_bytes(buf)
            .map_err(|_| KmsError::SeedIo("the operating system random generator failed"))
    }
}

/// The seed bytes. Zeroed on drop; `Debug` prints nothing of the content.
pub(crate) struct Seed(Zeroizing<[u8; SEED_LEN]>);

impl Seed {
    pub(crate) fn expose(&self) -> &[u8; SEED_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for Seed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seed([redacted])")
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    format: String,
    key_id: String,
    key_version: u32,
    /// Base64 of the KMS ciphertext bytes.
    ciphertext: String,
}

/// The associated data the seed is wrapped with: it names the key id, so an envelope copied
/// from another key's site does not unwrap here.
fn seed_aad(key_id: &str) -> Vec<u8> {
    let mut aad = b"spot_kms_seed_v1".to_vec();
    aad.push(0);
    aad.extend_from_slice(key_id.as_bytes());
    aad
}

/// Loads the seed from `path`, or generates, wraps and stores a new one when the file is absent.
pub(crate) async fn load_or_create(
    path: &Path,
    key_id: &str,
    client: &dyn KmsClient,
    entropy: &dyn Entropy,
) -> Result<Seed, KmsError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => load(path, key_id, client).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut bytes = Zeroizing::new([0u8; SEED_LEN]);
            entropy.fill(bytes.as_mut())?;
            if bytes.iter().all(|b| *b == 0) {
                return Err(KmsError::SeedIo("the random generator returned only zeros"));
            }
            let wrapped = client.encrypt(&seed_aad(key_id), bytes.as_slice()).await?;
            store(path, key_id, &wrapped, false).await?;
            Ok(Seed(bytes))
        }
        Err(_) => Err(KmsError::SeedIo("the seed file cannot be inspected")),
    }
}

/// Reads and unwraps the seed at `path`.
pub(crate) async fn load(
    path: &Path,
    key_id: &str,
    client: &dyn KmsClient,
) -> Result<Seed, KmsError> {
    let raw = read_private(path, MAX_FILE).await.map_err(|reason| {
        if reason == "the file mode allows group or other access" {
            KmsError::SeedInvalid("the seed file mode allows group or other access")
        } else {
            KmsError::SeedIo("the seed file cannot be read")
        }
    })?;
    let envelope: Envelope = serde_json::from_slice(&raw)
        .map_err(|_| KmsError::SeedInvalid("the seed file is not a seed envelope"))?;
    if envelope.format != FORMAT {
        return Err(KmsError::SeedInvalid("the seed file has an unknown format"));
    }
    if envelope.key_id != key_id {
        return Err(KmsError::SeedInvalid(
            "the seed file belongs to another key",
        ));
    }
    if envelope.key_version == 0 {
        return Err(KmsError::SeedInvalid("the seed file names no key version"));
    }
    let bytes = STANDARD
        .decode(envelope.ciphertext.as_bytes())
        .map_err(|_| KmsError::SeedInvalid("the seed ciphertext is not base64"))?;
    let plain = client
        .decrypt(
            &seed_aad(key_id),
            &KmsCiphertext {
                key_version: envelope.key_version,
                bytes,
            },
        )
        .await
        .map_err(|e| match e {
            // The KMS's authentication failure: the envelope was not wrapped for this key and
            // purpose.
            KmsError::Rejected { status: 400 } => {
                KmsError::SeedInvalid("the seed does not unwrap under this key")
            }
            other => other,
        })?;
    let seed: [u8; SEED_LEN] = plain
        .expose()
        .try_into()
        .map_err(|_| KmsError::SeedInvalid("the seed has the wrong length"))?;
    Ok(Seed(Zeroizing::new(seed)))
}

/// Re-wraps the seed under the KMS key's latest version and replaces the file.
pub(crate) async fn rewrap(
    path: &Path,
    key_id: &str,
    client: &dyn KmsClient,
) -> Result<u32, KmsError> {
    let seed = load(path, key_id, client).await?;
    let wrapped = client.encrypt(&seed_aad(key_id), seed.expose()).await?;
    store(path, key_id, &wrapped, true).await?;
    Ok(wrapped.key_version)
}

fn io(_: std::io::Error) -> KmsError {
    KmsError::SeedIo("the seed file cannot be written")
}

/// Writes the envelope to a `0600` temp file (`create_new`, so a planted link is never followed),
/// syncs it, publishes it atomically (a no-clobber hard link for a first write, a rename for a
/// replacement) and syncs the directory.
async fn store(
    path: &Path,
    key_id: &str,
    wrapped: &KmsCiphertext,
    replace: bool,
) -> Result<(), KmsError> {
    let envelope = Envelope {
        format: FORMAT.to_owned(),
        key_id: key_id.to_owned(),
        key_version: wrapped.key_version,
        ciphertext: STANDARD.encode(&wrapped.bytes),
    };
    let json = serde_json::to_vec_pretty(&envelope)
        .map_err(|_| KmsError::SeedIo("the seed envelope cannot be encoded"))?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut out = match options.open(&tmp).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(KmsError::SeedIo(
                "a seed temp file is left over; remove it if no other process is writing",
            ));
        }
        Err(e) => return Err(io(e)),
    };
    out.write_all(&json).await.map_err(io)?;
    out.sync_all().await.map_err(io)?;
    drop(out);
    if replace {
        tokio::fs::rename(&tmp, path).await.map_err(io)?;
    } else {
        let linked = tokio::fs::hard_link(&tmp, path).await;
        let _ = tokio::fs::remove_file(&tmp).await;
        match linked {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(KmsError::SeedIo(
                    "the seed file appeared while it was being created",
                ));
            }
            Err(e) => return Err(io(e)),
        }
    }
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    dir_sync(dir).await.map_err(io)
}

#[cfg(unix)]
async fn dir_sync(dir: PathBuf) -> std::io::Result<()> {
    tokio::fs::File::open(&dir).await?.sync_all().await
}

#[cfg(not(unix))]
async fn dir_sync(_dir: PathBuf) -> std::io::Result<()> {
    Ok(())
}
