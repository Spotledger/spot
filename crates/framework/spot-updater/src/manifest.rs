//! The update manifest (`update.toml`) and its canonical bytes.
//!
//! The manifest is a TOML document with a fixed `format`, the product, the channel, the version
//! and its `min_from`, the payload archive, every file inside it, the schema epoch, and the
//! release metadata. It is parsed with unknown fields refused, so a typo is an error and never a
//! silently ignored field.
//!
//! The bytes a signature covers are [`UpdateManifest::signing_message`]: the [`UPDATE_DOMAIN`]
//! prefix, the canonical JSON of every field ([`UpdateManifest::canonical_bytes`]) and the
//! payload's raw digest. The canonical JSON is one exact byte sequence, so any change to any
//! field changes the signed bytes.

use std::collections::BTreeMap;

use semver::Version;
use serde::Deserialize;
use serde_json::Value;

use crate::canonical::{self, UPDATE_DOMAIN};
use crate::digest::Digest;
use crate::error::UpdaterError;

/// The `format` field of every update manifest.
pub const MANIFEST_FORMAT: &str = "spot-update-manifest";

/// The release channel an update is published on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Channel {
    /// The stable channel.
    Stable,
    /// The beta channel.
    Beta,
}

impl Channel {
    /// The channel's lower-case name, as the manifest carries it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }

    /// Parses a channel name.
    ///
    /// # Errors
    /// [`UpdaterError::ManifestMalformed`] for any name other than `stable` or `beta`.
    pub fn parse(text: &str) -> Result<Self, UpdaterError> {
        match text {
            "stable" => Ok(Self::Stable),
            "beta" => Ok(Self::Beta),
            other => Err(UpdaterError::ManifestMalformed {
                detail: format!("`{other}` is not a release channel (stable or beta)"),
            }),
        }
    }
}

/// The payload archive: the file to fetch, its size and its SHA-256.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdatePayload {
    /// The archive's file name.
    pub file: String,
    /// The archive's size in bytes.
    pub size: u64,
    /// The archive's SHA-256.
    pub sha256: Digest,
}

/// One file inside the payload, so partial corruption is located.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateFile {
    /// The file's path relative to the version directory.
    pub path: String,
    /// The file's size in bytes.
    pub size: u64,
    /// The file's SHA-256.
    pub sha256: Digest,
}

/// The update manifest: what an offered update claims about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateManifest {
    /// The product the update is for.
    pub product: String,
    /// The release channel.
    pub channel: Channel,
    /// The offered version.
    pub version: Version,
    /// The oldest installed version that may update directly to this one.
    pub min_from: Version,
    /// The intermediate versions an older install must step through, oldest first.
    pub via: Vec<Version>,
    /// When the update was released (UTC seconds).
    pub released_at: i64,
    /// The payload archive.
    pub payload: UpdatePayload,
    /// Every file inside the payload.
    pub files: Vec<UpdateFile>,
    /// The installed schema epochs this payload can migrate up from; an update that needs a
    /// newer schema epoch than the database has is refused unless it lists that epoch here.
    pub migrations: Vec<u64>,
    /// The oldest operating system the update runs on, or `None` when it does not care.
    pub requires_os: Option<String>,
    /// The database schema epoch the update needs; the data layer compares it.
    pub schema_epoch: u64,
    /// Release notes for an administrator.
    pub notes: Option<String>,
}

/// The raw TOML shape, with string versions and digests, checked into the typed manifest.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    format: String,
    product: String,
    channel: String,
    version: String,
    min_from: String,
    #[serde(default)]
    via: Vec<String>,
    released_at: i64,
    payload: RawPayload,
    #[serde(default)]
    files: Vec<RawFile>,
    #[serde(default)]
    migrations: Vec<u64>,
    #[serde(default)]
    requires_os: Option<String>,
    schema_epoch: u64,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayload {
    file: String,
    size: u64,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    path: String,
    size: u64,
    sha256: String,
}

impl UpdateManifest {
    /// Parses an `update.toml` document, checking every field.
    ///
    /// # Errors
    /// [`UpdaterError::ManifestMalformed`] when the TOML does not parse, an unknown field is
    /// present, the `format` is not [`MANIFEST_FORMAT`], a version is not semver, the channel is
    /// not `stable` or `beta`, or a digest is not 64 hex characters.
    pub fn from_toml(text: &str) -> Result<Self, UpdaterError> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|error| UpdaterError::ManifestMalformed {
                detail: error.to_string(),
            })?;
        if raw.format != MANIFEST_FORMAT {
            return Err(UpdaterError::ManifestMalformed {
                detail: format!("the format is `{}`, not `{MANIFEST_FORMAT}`", raw.format),
            });
        }
        Ok(Self {
            product: raw.product,
            channel: Channel::parse(&raw.channel)?,
            version: version_of(&raw.version)?,
            min_from: version_of(&raw.min_from)?,
            via: raw
                .via
                .iter()
                .map(|v| version_of(v))
                .collect::<Result<_, _>>()?,
            released_at: raw.released_at,
            payload: UpdatePayload {
                file: raw.payload.file,
                size: raw.payload.size,
                sha256: digest_of(&raw.payload.sha256)?,
            },
            files: raw
                .files
                .into_iter()
                .map(|file| {
                    Ok(UpdateFile {
                        path: file.path,
                        size: file.size,
                        sha256: digest_of(&file.sha256)?,
                    })
                })
                .collect::<Result<_, UpdaterError>>()?,
            migrations: raw.migrations,
            requires_os: raw.requires_os,
            schema_epoch: raw.schema_epoch,
            notes: raw.notes,
        })
    }

    /// The canonical JSON of every field: sorted keys, no whitespace, integers only. One exact
    /// byte sequence per manifest.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.signed_fields()).unwrap_or_default()
    }

    /// The bytes an update signature covers: the [`UPDATE_DOMAIN`] prefix, the canonical
    /// manifest bytes, and the payload's raw digest.
    #[must_use]
    pub fn signing_message(&self) -> Vec<u8> {
        let mut message = canonical::encode(UPDATE_DOMAIN, &self.signed_fields());
        message.extend_from_slice(self.payload.sha256.as_bytes());
        message
    }

    /// The manifest as the sorted map the canonical bytes are built from.
    fn signed_fields(&self) -> BTreeMap<&str, Value> {
        let payload = {
            let mut fields = BTreeMap::new();
            fields.insert("file", Value::from(self.payload.file.as_str()));
            fields.insert("sha256", Value::from(self.payload.sha256.to_hex()));
            fields.insert("size", Value::from(self.payload.size));
            canonical::object(fields)
        };
        let files = self
            .files
            .iter()
            .map(|file| {
                let mut fields = BTreeMap::new();
                fields.insert("path", Value::from(file.path.as_str()));
                fields.insert("sha256", Value::from(file.sha256.to_hex()));
                fields.insert("size", Value::from(file.size));
                canonical::object(fields)
            })
            .collect::<Vec<_>>();
        let via = self
            .via
            .iter()
            .map(|version| Value::from(version.to_string()))
            .collect::<Vec<_>>();
        let mut fields = BTreeMap::new();
        fields.insert("channel", Value::from(self.channel.as_str()));
        fields.insert("files", Value::from(files));
        fields.insert("format", Value::from(MANIFEST_FORMAT));
        fields.insert("migrations", Value::from(self.migrations.clone()));
        fields.insert("min_from", Value::from(self.min_from.to_string()));
        fields.insert("notes", optional(self.notes.as_deref()));
        fields.insert("payload", payload);
        fields.insert("product", Value::from(self.product.as_str()));
        fields.insert("released_at", Value::from(self.released_at));
        fields.insert("requires_os", optional(self.requires_os.as_deref()));
        fields.insert("schema_epoch", Value::from(self.schema_epoch));
        fields.insert("version", Value::from(self.version.to_string()));
        fields.insert("via", Value::from(via));
        fields
    }
}

/// A `null` for `None`, a string for `Some`, so the canonical bytes always carry the key.
fn optional(value: Option<&str>) -> Value {
    match value {
        Some(text) => Value::from(text),
        None => Value::Null,
    }
}

/// Parses a semver version.
fn version_of(text: &str) -> Result<Version, UpdaterError> {
    Version::parse(text).map_err(|error| UpdaterError::ManifestMalformed {
        detail: format!("`{text}` is not a semver version: {error}"),
    })
}

/// Parses a SHA-256 digest.
fn digest_of(text: &str) -> Result<Digest, UpdaterError> {
    Digest::from_hex(text)
}
