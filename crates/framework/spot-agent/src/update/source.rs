//! Where an offer and its payload come from: the gateway over the pinned channel, or a
//! local repository directory (the air-gapped path of WP-ADM-12.T2).
//!
//! A source only moves bytes. It never decides: signature, digest, size and path checks all
//! happen in `spot-updater` after the source has answered, so a hostile source can at worst
//! make an update fail.
//!
//! Over the channel the payload is fetched in `Range` chunks of at most [`CHUNK`] bytes,
//! each request signed like every other agent request, because the channel transport caps a
//! response at 1 MiB. The contract names no payload path: this module uses
//! `GET /agent/v1/update/payload?file=<name>` with `Range`, answered `206` (or `200` for a
//! payload that fits one chunk). That is recorded as an open question for the contract.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use spot_agent_enroll::keys::KeyStore;

use super::error::UpdateError;
use crate::channel::client::{ChannelError, Client, signing_input};
use crate::channel::transport::{HttpRequest, HttpResponse, Transport};

/// Bytes asked for per request over the channel.
pub const CHUNK: u64 = 512 * 1024;
/// Longest file name accepted for a payload.
pub const MAX_NAME: usize = 128;
/// Largest `update.json` a repository may hold.
pub const MAX_OFFER_FILE: u64 = 256 * 1024;

/// The seam the updater reads an offer and a payload through.
pub trait UpdateSource: Send + Sync {
    /// The `data` object of an update answer (`manifest`, `proof`, optional `revocations`).
    ///
    /// # Errors
    /// [`UpdateError::Unavailable`] when nothing is published, otherwise the transport's
    /// error.
    fn offer(&self, channel: Option<&str>, now: i64) -> Result<Value, UpdateError>;

    /// The payload archive named `file`, which the signed manifest says is `size` bytes.
    ///
    /// # Errors
    /// The transport's error; a wrong length is an error too (the digest check follows).
    fn payload(&self, file: &str, size: u64, now: i64) -> Result<Vec<u8>, UpdateError>;
}

/// Whether `name` is a plain file name: ASCII letters, digits, `.`, `-`, `_`, not starting
/// with a dot, at most [`MAX_NAME`] bytes. Nothing that names a directory gets through.
#[must_use]
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// The gateway over the pinned channel.
pub struct ChannelSource {
    client: Arc<Client>,
    transport: Arc<dyn Transport>,
    keys: Arc<dyn KeyStore>,
    agent_id: String,
}

impl ChannelSource {
    /// A source that asks the manifest through `client` and fetches the payload over
    /// `transport`, signing each request with `keys`.
    #[must_use]
    pub fn new(
        client: Arc<Client>,
        transport: Arc<dyn Transport>,
        keys: Arc<dyn KeyStore>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            client,
            transport,
            keys,
            agent_id: agent_id.into(),
        }
    }

    fn headers(&self, path: &str, now: i64, range: &str) -> Vec<(String, String)> {
        let timestamp = now.saturating_add(self.client.clock_skew());
        let mut random = [0u8; 16];
        rand::Rng::fill(&mut rand::rng(), &mut random);
        let mut nonce = String::with_capacity(32);
        for byte in random {
            nonce.push_str(&format!("{byte:02x}"));
        }
        let signature = self
            .keys
            .sign(&signing_input("GET", path, &[], timestamp, &nonce));
        vec![
            ("X-Spot-Agent-Id".to_owned(), self.agent_id.clone()),
            ("X-Spot-Timestamp".to_owned(), timestamp.to_string()),
            ("X-Spot-Nonce".to_owned(), nonce),
            (
                "X-Spot-Agent-Signature".to_owned(),
                STANDARD.encode(signature.to_bytes()),
            ),
            ("Range".to_owned(), range.to_owned()),
        ]
    }

    fn fetch_chunk(
        &self,
        file: &str,
        from: u64,
        to: u64,
        now: i64,
    ) -> Result<HttpResponse, UpdateError> {
        let path = format!("/agent/v1/update/payload?file={file}");
        let range = format!("bytes={from}-{to}");
        let response = self
            .transport
            .send(&HttpRequest {
                method: "GET",
                path: path.clone(),
                headers: self.headers(&path, now, &range),
                body: Vec::new(),
                timeout: Duration::from_secs(60),
            })
            .map_err(|error| UpdateError::Retryable(error.code().to_owned()))?;
        match response.status {
            200 | 206 => Ok(response),
            404 => Err(UpdateError::Unavailable),
            403 if error_code(&response.body).as_deref() == Some("agent.revoked") => {
                Err(UpdateError::Revoked)
            }
            429 | 500..=599 => Err(UpdateError::Retryable(format!("http {}", response.status))),
            status => Err(UpdateError::Source(
                error_code(&response.body).unwrap_or_else(|| format!("http {status}")),
            )),
        }
    }
}

fn error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value.get("error")?.get("code")?.as_str().map(str::to_owned)
}

impl UpdateSource for ChannelSource {
    fn offer(&self, channel: Option<&str>, now: i64) -> Result<Value, UpdateError> {
        self.client
            .fetch_update(channel, now)
            .map_err(|error| match error {
                ChannelError::Revoked => UpdateError::Revoked,
                ChannelError::Retryable(why) => UpdateError::Retryable(why),
                ChannelError::Fatal(code) if code == super::CODE_UNAVAILABLE => {
                    UpdateError::Unavailable
                }
                ChannelError::Fatal(code) => UpdateError::Source(code),
            })
    }

    fn payload(&self, file: &str, size: u64, now: i64) -> Result<Vec<u8>, UpdateError> {
        if !is_plain_name(file) {
            return Err(UpdateError::Wire("payload name"));
        }
        let mut out: Vec<u8> = Vec::new();
        let mut offset: u64 = 0;
        while offset < size {
            let end = offset.saturating_add(CHUNK).min(size) - 1;
            let want = end - offset + 1;
            let response = self.fetch_chunk(file, offset, end, now)?;
            let got = response.body.len() as u64;
            let whole = response.status == 200 && offset == 0 && got == size;
            if whole {
                return Ok(response.body);
            }
            if response.status != 206 || got != want {
                return Err(UpdateError::Source("short or unranged payload".to_owned()));
            }
            out.extend_from_slice(&response.body);
            offset = end + 1;
        }
        Ok(out)
    }
}

/// A local repository directory: `update.json` (the same object the gateway answers) and the
/// payload file next to it. Symlinks and anything that is not a regular file are refused, so
/// a repository cannot point the agent at another part of the disk.
#[derive(Clone, Debug)]
pub struct DirSource {
    dir: PathBuf,
}

impl DirSource {
    /// A source over the repository directory `dir`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn regular_file(&self, name: &str) -> Result<Option<PathBuf>, UpdateError> {
        let path = self.dir.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => Ok(Some(path)),
            Ok(_) => Err(UpdateError::Source("not a regular file".to_owned())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(UpdateError::Source("repository unreadable".to_owned())),
        }
    }
}

fn read_exact_capped(path: &Path, cap: u64) -> Result<Vec<u8>, UpdateError> {
    let file = fs::File::open(path)
        .map_err(|_| UpdateError::Source("repository unreadable".to_owned()))?;
    let mut bytes = Vec::new();
    // One byte past the cap: a file longer than allowed is detected, not silently cut.
    file.take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| UpdateError::Source("repository unreadable".to_owned()))?;
    if bytes.len() as u64 > cap {
        return Err(UpdateError::Source("repository file too large".to_owned()));
    }
    Ok(bytes)
}

impl UpdateSource for DirSource {
    fn offer(&self, _channel: Option<&str>, _now: i64) -> Result<Value, UpdateError> {
        let Some(path) = self.regular_file("update.json")? else {
            return Err(UpdateError::Unavailable);
        };
        let bytes = read_exact_capped(&path, MAX_OFFER_FILE)?;
        serde_json::from_slice(&bytes).map_err(|_| UpdateError::Wire("offer"))
    }

    fn payload(&self, file: &str, size: u64, _now: i64) -> Result<Vec<u8>, UpdateError> {
        if !is_plain_name(file) {
            return Err(UpdateError::Wire("payload name"));
        }
        let Some(path) = self.regular_file(file)? else {
            return Err(UpdateError::Unavailable);
        };
        let bytes = read_exact_capped(&path, size)?;
        if bytes.len() as u64 != size {
            return Err(UpdateError::Source(
                "payload length differs from the manifest".to_owned(),
            ));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn plain_names_only() {
        for good in ["spot-agent-2.0.0.zip", "a", "x_y-1.zip"] {
            assert!(is_plain_name(good), "{good}");
        }
        for bad in [
            "../x",
            "/etc/passwd",
            ".hidden",
            "a/b",
            "",
            "a b",
            "a\\b",
            "a?b=c",
        ] {
            assert!(!is_plain_name(bad), "{bad}");
        }
        assert!(!is_plain_name(&"a".repeat(MAX_NAME + 1)));
    }

    #[test]
    fn a_repository_refuses_odd_names_missing_files_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let source = DirSource::new(dir.path());
        assert_eq!(source.offer(None, 0).unwrap_err(), UpdateError::Unavailable);
        for name in ["../x", "/etc/passwd", ".hidden", "a/b", ""] {
            assert!(source.payload(name, 1, 0).is_err(), "{name}");
        }
        assert_eq!(
            source.payload("missing.zip", 1, 0).unwrap_err(),
            UpdateError::Unavailable
        );
        #[cfg(unix)]
        {
            std::fs::write(dir.path().join("real.zip"), b"x").unwrap();
            std::os::unix::fs::symlink(dir.path().join("real.zip"), dir.path().join("link.zip"))
                .unwrap();
            assert!(source.payload("link.zip", 1, 0).is_err());
            std::os::unix::fs::symlink(dir.path().join("real.zip"), dir.path().join("update.json"))
                .unwrap();
            assert!(source.offer(None, 0).is_err());
        }
    }

    #[test]
    fn a_payload_of_another_length_than_signed_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.zip"), b"12345").unwrap();
        let source = DirSource::new(dir.path());
        assert_eq!(source.payload("p.zip", 5, 0).unwrap(), b"12345".to_vec());
        assert!(source.payload("p.zip", 6, 0).is_err());
        assert!(source.payload("p.zip", 4, 0).is_err());
    }

    #[test]
    fn an_oversized_offer_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("update.json"),
            vec![b' '; (MAX_OFFER_FILE + 1) as usize],
        )
        .unwrap();
        assert!(DirSource::new(dir.path()).offer(None, 0).is_err());
    }
}
