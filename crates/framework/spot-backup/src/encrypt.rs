//! Streaming authenticated encryption of a package payload (WP-PRD-04.T6).
//!
//! # Construction
//!
//! Every package gets a fresh random 32-byte **data key** (OS RNG, zeroized on drop).
//! The data key is wrapped through the host's [`spot_secrets::KeyProvider`], so this
//! crate never sees the key-encryption key. The payload is cut into fixed 64 KiB
//! plaintext chunks and each chunk is sealed with XChaCha20-Poly1305 (the STREAM
//! construction):
//!
//! * nonce (24 bytes) = random 16-byte per-package prefix, then a big-endian `u64`
//!   whose top bit is the **final-chunk flag** and whose other 63 bits are the chunk
//!   counter, so a nonce is never reused under one data key;
//! * associated data = a domain label, the package id, the format version, the chunk
//!   index, the final flag and, for chunk 0, the whole header bytes.
//!
//! Truncation (also at a chunk boundary), reordering, duplication, an appended chunk,
//! a chunk from another package and an altered header all fail authentication. A
//! stream whose last chunk does not carry the final flag is an error.
//!
//! # Wire format (version 1)
//!
//! ```text
//! header: "SPBENC01" | u16 version | u32 chunk size | 16-byte nonce prefix |
//!         u16 key id length | key id | u32 key version | u16 wrapped length | wrapped key
//! body:   sealed chunk 0 | sealed chunk 1 | ... (each chunk size + 16 byte tag,
//!         the last one shorter), to the end of the stream
//! ```
//!
//! The header is at most [`MAX_HEADER_LEN`] bytes; the key id is capped at 256 bytes
//! and the wrapped key at 1024 bytes. A reader allocates at most two chunks of the
//! declared size (capped at 1 MiB) plus the header, whatever the stream says.
//!
//! Nothing here logs or returns a key, a wrapped key, a nonce or plaintext: errors name
//! the chunk index and a fixed reason only.

use std::sync::Arc;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::TryRngCore;
use rand::rngs::OsRng;
use spot_secrets::{KeyProvider, KeyRef as SecretsKeyRef, SecretError, WrappedKey};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

use crate::error::{BackupError, Result};
use crate::model::{BackupRequest, KeyRef};
use crate::package::PackagedBackup;
use crate::runner::{BackupProducer, BoxFuture};

/// The encrypted-payload magic, exactly eight bytes.
pub const ENCRYPTION_MAGIC: &[u8; 8] = b"SPBENC01";
/// The encryption format version this build writes and reads.
pub const FORMAT_VERSION: u16 = 1;
/// The plaintext chunk size this build writes, in bytes.
pub const CHUNK_SIZE: usize = 64 * 1024;
/// The Poly1305 tag appended to every sealed chunk, in bytes.
pub const TAG_LEN: usize = 16;
/// The largest header a reader accepts, in bytes.
pub const MAX_HEADER_LEN: usize =
    8 + 2 + 4 + PREFIX_LEN + 2 + MAX_KEY_ID_LEN + 4 + 2 + MAX_WRAPPED_LEN;

const MIN_CHUNK_SIZE: usize = 1024;
const MAX_CHUNK_SIZE: usize = 1 << 20;
const PREFIX_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const MAX_KEY_ID_LEN: usize = 256;
const MAX_WRAPPED_LEN: usize = 1024;
const MAX_PACKAGE_ID_LEN: usize = 512;
/// The top bit of the nonce counter field: set on the final chunk only.
const FINAL_FLAG: u64 = 1 << 63;
/// Chunk indexes stop well below the flag bit.
const MAX_CHUNKS: u64 = 1 << 62;

/// Facts about one encryption, safe to log: no key, nonce or plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionInfo {
    /// The key-encryption key that wrapped the data key (id and version only).
    pub key: KeyRef,
    /// The plaintext chunk size used, in bytes.
    pub chunk_size: u32,
    /// How many chunks were written.
    pub chunks: u64,
    /// Plaintext bytes consumed.
    pub plaintext_bytes: u64,
    /// Bytes written, header included.
    pub encrypted_bytes: u64,
}

fn header_invalid(reason: impl Into<String>) -> BackupError {
    BackupError::EncryptionHeaderInvalid {
        reason: reason.into(),
    }
}

fn decrypt_failed(reason: impl Into<String>) -> BackupError {
    BackupError::DecryptFailed {
        reason: reason.into(),
    }
}

fn unavailable(reason: impl Into<String>) -> BackupError {
    BackupError::EncryptionUnavailable {
        reason: reason.into(),
    }
}

/// A provider refusal. `SecretError` messages never carry key material.
fn provider_refused(error: SecretError) -> BackupError {
    unavailable(format!("the key provider refused: {error}"))
}

/// The parsed, bounds-checked header. It deliberately has no `Debug`.
pub(crate) struct Header {
    pub(crate) chunk_size: u32,
    pub(crate) prefix: [u8; PREFIX_LEN],
    pub(crate) key: KeyRef,
    pub(crate) wrapped: Vec<u8>,
}

struct Slice<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Slice<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or_else(|| header_invalid("the header length overflows"))?;
        let part = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| header_invalid("the encryption header is truncated"))?;
        self.at = end;
        Ok(part)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let part = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(part);
        Ok(out)
    }
}

impl Header {
    pub(crate) fn chunk_len(&self) -> usize {
        usize::try_from(self.chunk_size).unwrap_or(CHUNK_SIZE)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let key_id = self.key.id.as_bytes();
        if key_id.is_empty() || key_id.len() > MAX_KEY_ID_LEN {
            return Err(header_invalid(
                "the key id is empty or longer than 256 bytes",
            ));
        }
        if self.wrapped.is_empty() || self.wrapped.len() > MAX_WRAPPED_LEN {
            return Err(header_invalid(
                "the wrapped key is empty or longer than 1024 bytes",
            ));
        }
        let key_id_len = u16::try_from(key_id.len())
            .map_err(|_| header_invalid("the key id length does not fit"))?;
        let wrapped_len = u16::try_from(self.wrapped.len())
            .map_err(|_| header_invalid("the wrapped key length does not fit"))?;
        let mut out = Vec::with_capacity(MAX_HEADER_LEN);
        out.extend_from_slice(ENCRYPTION_MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        out.extend_from_slice(&self.chunk_size.to_be_bytes());
        out.extend_from_slice(&self.prefix);
        out.extend_from_slice(&key_id_len.to_be_bytes());
        out.extend_from_slice(key_id);
        out.extend_from_slice(&self.key.version.to_be_bytes());
        out.extend_from_slice(&wrapped_len.to_be_bytes());
        out.extend_from_slice(&self.wrapped);
        Ok(out)
    }

    /// Parses a header from the start of `bytes` and returns it with the number of
    /// bytes it used. Every length is checked before anything is allocated.
    pub(crate) fn parse(bytes: &[u8]) -> Result<(Header, usize)> {
        let mut input = Slice { bytes, at: 0 };
        if input.take(ENCRYPTION_MAGIC.len())? != ENCRYPTION_MAGIC.as_slice() {
            return Err(header_invalid("the encryption magic is wrong"));
        }
        let version = u16::from_be_bytes(input.array::<2>()?);
        if version != FORMAT_VERSION {
            return Err(header_invalid(format!(
                "encryption format version {version} is not supported"
            )));
        }
        let chunk_size = u32::from_be_bytes(input.array::<4>()?);
        let chunk_len = usize::try_from(chunk_size)
            .map_err(|_| header_invalid("the chunk size does not fit"))?;
        if !(MIN_CHUNK_SIZE..=MAX_CHUNK_SIZE).contains(&chunk_len) {
            return Err(header_invalid(format!(
                "the chunk size {chunk_size} is outside {MIN_CHUNK_SIZE}..={MAX_CHUNK_SIZE}"
            )));
        }
        let prefix = input.array::<PREFIX_LEN>()?;
        let key_id_len = usize::from(u16::from_be_bytes(input.array::<2>()?));
        if key_id_len == 0 || key_id_len > MAX_KEY_ID_LEN {
            return Err(header_invalid("the key id length is outside 1..=256"));
        }
        let key_id = std::str::from_utf8(input.take(key_id_len)?)
            .map_err(|_| header_invalid("the key id is not UTF-8"))?
            .to_owned();
        let version_of_key = u32::from_be_bytes(input.array::<4>()?);
        if version_of_key == 0 {
            return Err(header_invalid("key versions start at 1"));
        }
        let wrapped_len = usize::from(u16::from_be_bytes(input.array::<2>()?));
        if wrapped_len == 0 || wrapped_len > MAX_WRAPPED_LEN {
            return Err(header_invalid("the wrapped key length is outside 1..=1024"));
        }
        let wrapped = input.take(wrapped_len)?.to_vec();
        Ok((
            Header {
                chunk_size,
                prefix,
                key: KeyRef {
                    id: key_id,
                    version: version_of_key,
                },
                wrapped,
            },
            input.at,
        ))
    }
}

fn check_package_id(package_id: &str) -> Result<()> {
    if package_id.is_empty() || package_id.len() > MAX_PACKAGE_ID_LEN {
        return Err(unavailable(
            "the package id is empty or longer than 512 bytes",
        ));
    }
    Ok(())
}

pub(crate) fn wrap_aad(package_id: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(32 + package_id.len());
    aad.extend_from_slice(b"spot-backup/wrap/v1\0");
    aad.extend_from_slice(package_id.as_bytes());
    aad
}

fn chunk_aad(package_id: &str, header: &[u8], index: u64, last: bool) -> Vec<u8> {
    let id_len = u32::try_from(package_id.len()).unwrap_or(u32::MAX);
    let mut aad = Vec::with_capacity(48 + package_id.len() + header.len());
    aad.extend_from_slice(b"spot-backup/chunk/v1\0");
    aad.extend_from_slice(&id_len.to_be_bytes());
    aad.extend_from_slice(package_id.as_bytes());
    aad.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    aad.extend_from_slice(&index.to_be_bytes());
    aad.push(u8::from(last));
    if index == 0 {
        // The header is authenticated as part of chunk 0.
        aad.extend_from_slice(header);
    }
    aad
}

fn nonce_for(prefix: &[u8; PREFIX_LEN], index: u64, last: bool) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    let (front, back) = nonce.split_at_mut(PREFIX_LEN);
    front.copy_from_slice(prefix);
    let counter = if last { index | FINAL_FLAG } else { index };
    back.copy_from_slice(&counter.to_be_bytes());
    nonce
}

fn cipher_for(key: &[u8; KEY_LEN]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(key.into())
}

fn random_bytes(buffer: &mut [u8]) -> Result<()> {
    OsRng
        .try_fill_bytes(buffer)
        .map_err(|_| unavailable("the operating system RNG is unavailable"))
}

/// Reads until `buffer` is full or the stream ends; returns the bytes read.
async fn read_full<R: AsyncRead + Unpin>(reader: &mut R, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0usize;
    while filled < buffer.len() {
        let read = reader.read(&mut buffer[filled..]).await?;
        if read == 0 {
            break;
        }
        filled = filled.saturating_add(read);
    }
    Ok(filled)
}

fn secrets_key(key: &KeyRef) -> SecretsKeyRef {
    SecretsKeyRef {
        id: key.id.clone(),
        version: key.version,
    }
}

/// Encrypts everything `reader` yields into `writer`.
///
/// The data key is wrapped under `key` through `provider`. The provider wraps under
/// its current key version only; if that is not `key`, this fails and never falls back
/// to another version. The payload is never buffered whole: two chunks are held.
///
/// # Errors
/// [`BackupError::EncryptionUnavailable`] when the provider refuses or wraps under a
/// different key, or the OS RNG is unavailable; [`BackupError::Io`] on a read or write
/// failure.
pub async fn encrypt_stream<R, W>(
    provider: &dyn KeyProvider,
    key: &KeyRef,
    package_id: &str,
    mut reader: R,
    mut writer: W,
) -> Result<EncryptionInfo>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    check_package_id(package_id)?;
    let mut data_key = Zeroizing::new([0u8; KEY_LEN]);
    random_bytes(&mut data_key[..])?;
    let mut prefix = [0u8; PREFIX_LEN];
    random_bytes(&mut prefix)?;

    let aad = wrap_aad(package_id);
    let wrapped = provider
        .wrap(&data_key[..], &aad)
        .await
        .map_err(provider_refused)?;
    if wrapped.key.id != key.id || wrapped.key.version != key.version {
        return Err(unavailable(format!(
            "the provider wraps under key {} version {}, not the requested {} version {}",
            wrapped.key.id, wrapped.key.version, key.id, key.version
        )));
    }
    let chunk_size =
        u32::try_from(CHUNK_SIZE).map_err(|_| unavailable("the chunk size does not fit"))?;
    let header = Header {
        chunk_size,
        prefix,
        key: key.clone(),
        wrapped: wrapped.bytes,
    };
    let header_bytes = header.encode()?;
    writer.write_all(&header_bytes).await?;
    let mut encrypted_bytes = u64::try_from(header_bytes.len()).unwrap_or(u64::MAX);

    let cipher = cipher_for(&data_key);
    let mut current = Zeroizing::new(vec![0u8; CHUNK_SIZE]);
    let mut next = Zeroizing::new(vec![0u8; CHUNK_SIZE]);
    let mut current_len = read_full(&mut reader, &mut current[..]).await?;
    let mut index: u64 = 0;
    let mut plaintext_bytes: u64 = 0;
    loop {
        // A chunk is final when nothing follows it. Only a full chunk can be followed.
        let next_len = if current_len == CHUNK_SIZE {
            read_full(&mut reader, &mut next[..]).await?
        } else {
            0
        };
        let last = next_len == 0;
        let aad = chunk_aad(package_id, &header_bytes, index, last);
        let nonce = nonce_for(&prefix, index, last);
        let sealed = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &current[..current_len],
                    aad: aad.as_slice(),
                },
            )
            .map_err(|_| unavailable(format!("the cipher refused chunk {index}")))?;
        writer.write_all(&sealed).await?;
        plaintext_bytes = plaintext_bytes.saturating_add(u64::try_from(current_len).unwrap_or(0));
        encrypted_bytes = encrypted_bytes.saturating_add(u64::try_from(sealed.len()).unwrap_or(0));
        if last {
            break;
        }
        std::mem::swap(&mut current, &mut next);
        current_len = next_len;
        index = match index.checked_add(1) {
            Some(value) if value < MAX_CHUNKS => value,
            _ => return Err(unavailable("the payload has too many chunks")),
        };
    }
    writer.flush().await?;
    Ok(EncryptionInfo {
        key: key.clone(),
        chunk_size,
        chunks: index.saturating_add(1),
        plaintext_bytes,
        encrypted_bytes,
    })
}

/// Decrypts a stream written by [`encrypt_stream`] into `writer`.
///
/// Each chunk is authenticated before its plaintext is written, so on any failure the
/// writer holds only plaintext of chunks that verified. The key named in the header is
/// unwrapped through `provider`; a retired or unknown version is refused by the
/// provider and reported, never replaced by another key.
///
/// # Errors
/// [`BackupError::EncryptionHeaderInvalid`] for a malformed header,
/// [`BackupError::EncryptionUnavailable`] when the provider cannot give the key,
/// [`BackupError::DecryptFailed`] for any authentication, ordering or truncation
/// failure, and [`BackupError::Io`] on a read or write failure.
pub async fn decrypt_stream<R, W>(
    provider: &dyn KeyProvider,
    package_id: &str,
    mut reader: R,
    mut writer: W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    check_package_id(package_id)?;
    let mut head = vec![0u8; MAX_HEADER_LEN];
    let got = read_full(&mut reader, &mut head).await?;
    head.truncate(got);
    let (header, used) = Header::parse(&head)?;
    let header_bytes = head[..used].to_vec();
    // Bytes read past the header belong to the first chunk: put them back in front.
    let leftover = head[used..].to_vec();
    drop(head);
    let mut body = std::io::Cursor::new(leftover).chain(reader);

    let key = secrets_key(&header.key);
    let wrapped = WrappedKey {
        key: key.clone(),
        bytes: header.wrapped.clone(),
    };
    let aad = wrap_aad(package_id);
    let secret = provider
        .unwrap(&key, &wrapped, &aad)
        .await
        .map_err(|error| match error {
            SecretError::Tampered => {
                decrypt_failed("the data key could not be unwrapped: authentication failed")
            }
            other => provider_refused(other),
        })?;
    if secret.expose().len() != KEY_LEN {
        return Err(decrypt_failed(
            "the unwrapped data key has the wrong length",
        ));
    }
    let mut data_key = Zeroizing::new([0u8; KEY_LEN]);
    data_key.copy_from_slice(secret.expose());
    drop(secret);
    let cipher = cipher_for(&data_key);

    let sealed_full = header.chunk_len().saturating_add(TAG_LEN);
    let mut current = vec![0u8; sealed_full];
    let mut next = vec![0u8; sealed_full];
    let mut current_len = read_full(&mut body, &mut current).await?;
    let mut index: u64 = 0;
    loop {
        if current_len < TAG_LEN {
            return Err(decrypt_failed(format!(
                "the stream is truncated at chunk {index}"
            )));
        }
        let next_len = if current_len == sealed_full {
            read_full(&mut body, &mut next).await?
        } else {
            0
        };
        let last = next_len == 0;
        let aad = chunk_aad(package_id, &header_bytes, index, last);
        let nonce = nonce_for(&header.prefix, index, last);
        let plain = cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &current[..current_len],
                    aad: aad.as_slice(),
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| decrypt_failed(format!("chunk {index} failed authentication")))?;
        if last && index > 0 && plain.is_empty() {
            return Err(decrypt_failed(format!(
                "chunk {index} is an empty final chunk after data"
            )));
        }
        writer.write_all(&plain).await?;
        if last {
            break;
        }
        std::mem::swap(&mut current, &mut next);
        current_len = next_len;
        index = match index.checked_add(1) {
            Some(value) if value < MAX_CHUNKS => value,
            _ => return Err(decrypt_failed("the stream has too many chunks")),
        };
    }
    writer.flush().await?;
    Ok(())
}

/// Checks the structure of an encrypted payload section without a key: the header
/// parses within its caps and names `expected`, and the chunk framing is plausible for
/// `total_len` bytes (header included). Used by quick verification.
///
/// # Errors
/// [`BackupError::EncryptionHeaderInvalid`] naming the first broken rule.
pub(crate) fn check_framing(head: &[u8], total_len: u64, expected: &KeyRef) -> Result<()> {
    let (header, used) = Header::parse(head)?;
    if header.key != *expected {
        return Err(header_invalid(
            "the header names a different key than the manifest",
        ));
    }
    let used = u64::try_from(used).unwrap_or(u64::MAX);
    let body = total_len
        .checked_sub(used)
        .ok_or_else(|| header_invalid("the payload is shorter than its header"))?;
    let tag = u64::try_from(TAG_LEN).unwrap_or(16);
    let full = u64::try_from(header.chunk_len())
        .unwrap_or(u64::MAX)
        .saturating_add(tag);
    if body < tag {
        return Err(header_invalid("the payload holds no sealed chunk"));
    }
    let tail = body % full;
    if tail != 0 && tail < tag {
        return Err(header_invalid(
            "the last sealed chunk is shorter than a tag",
        ));
    }
    Ok(())
}

/// A [`BackupProducer`] that seals what an inner producer builds, when the job asks
/// for encryption. Without `job.encrypt` it passes the package through unchanged.
///
/// The runner stays provider-agnostic: it only checks that a job asking for encryption
/// received an encrypted package, and asks the producer for the provider to use in
/// full verification ([`BackupProducer::key_provider`]).
pub struct EncryptingProducer {
    inner: Arc<dyn BackupProducer>,
    provider: Arc<dyn KeyProvider>,
}

impl std::fmt::Debug for EncryptingProducer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncryptingProducer")
            .finish_non_exhaustive()
    }
}

/// Wraps `inner` so packages are encrypted through `provider` when the job asks.
pub fn encrypting(
    inner: Arc<dyn BackupProducer>,
    provider: Arc<dyn KeyProvider>,
) -> EncryptingProducer {
    EncryptingProducer { inner, provider }
}

impl BackupProducer for EncryptingProducer {
    fn produce<'a>(&'a self, request: &'a BackupRequest) -> BoxFuture<'a, Result<PackagedBackup>> {
        Box::pin(async move {
            let packaged = self.inner.produce(request).await?;
            let Some(key) = request.job.encrypt.as_ref() else {
                return Ok(packaged);
            };
            match crate::package::seal_package(&packaged, self.provider.as_ref(), key).await {
                Ok(sealed) => Ok(sealed),
                Err(error) => {
                    // Never leave the plaintext package behind after a failed seal.
                    let _ = std::fs::remove_file(packaged.package.path());
                    Err(error)
                }
            }
        })
    }

    fn key_provider(&self) -> Option<&dyn KeyProvider> {
        Some(self.provider.as_ref())
    }
}
