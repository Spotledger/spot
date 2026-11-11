//! The per-tenant journal key layout of ADR-061 section 5, in one place.
//!
//! ```text
//! <tenant>/manifest/<generation>.json          one signed manifest per generation
//! <tenant>/segments/<generation>/<first>-<last>.seg
//! <tenant>/tail/<generation>/<batch_id>.chunk  remote-sync tail, compacted into segments
//! <tenant>/snapshots/<generation>/<position>/  base images (a prefix, optionally with a name)
//! <tenant>/blobs/<hash>                        content-addressed attachments
//! ```
//!
//! Every number is zero-padded to [`DIGITS`] digits, so ascending lexical order of a listing is
//! ascending numeric order — the property the relay relies on when it picks the newest segment
//! by listing instead of by reading every manifest.
//!
//! The form is canonical and the parser accepts only the canonical form: a number shorter or
//! longer than [`DIGITS`] digits, an upper-case or short blob hash, or a key of any other shape
//! is refused with [`StoreError::KeyInvalid`]. Refusing variants is deliberate: `…/manifest/1.json`
//! and `…/manifest/00000000000000000001.json` must not be two names for one object, because a
//! backend that treated them as different would let a second manifest appear beside the first.

use std::fmt;

use crate::error::StoreError;

/// The canonical width of every zero-padded number in a key.
///
/// Twenty digits hold any `u64`, so no generation, sequence number or batch id can outgrow the
/// layout.
pub const DIGITS: usize = 20;

/// The `manifest/` key prefix segment.
pub const MANIFEST_PREFIX: &str = "manifest";
/// The `segments/` key prefix segment (never deletable).
pub const SEGMENTS_PREFIX: &str = "segments";
/// The `tail/` key prefix segment.
pub const TAIL_PREFIX: &str = "tail";
/// The `snapshots/` key prefix segment (never deletable).
pub const SNAPSHOTS_PREFIX: &str = "snapshots";
/// The `blobs/` key prefix segment.
pub const BLOBS_PREFIX: &str = "blobs";

/// The longest tenant id the layout accepts, in bytes.
pub const MAX_TENANT_BYTES: usize = 128;

/// What an object is, decided by its key alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    /// A signed manifest of one generation.
    Manifest,
    /// A sealed, immutable segment.
    Segment,
    /// A tail chunk awaiting compaction.
    TailChunk,
    /// A snapshot base image (a prefix, not a single object).
    Snapshot,
    /// A content-addressed attachment.
    Blob,
}

impl ObjectKind {
    /// Whether [`crate::JournalStore::delete`] may remove an object of this kind.
    ///
    /// Segments and snapshots are the recoverable record and are kept forever (ADR-061 s5), so
    /// only tail chunks are deletable — after they have been compacted into a segment.
    #[must_use]
    pub fn is_deletable(self) -> bool {
        !matches!(self, Self::Segment | Self::Snapshot)
    }

    /// Whether [`crate::JournalStore::put_replace`] may replace an object of this kind.
    ///
    /// The manifest is the one object of a generation that is replaced, and only through the
    /// etag-guarded replace; everything else is written once.
    #[must_use]
    pub fn is_replaceable(self) -> bool {
        matches!(self, Self::Manifest)
    }
}

/// A validated tenant id: one path component.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenantId(String);

impl TenantId {
    /// Accepts a tenant id if it is one safe path component.
    ///
    /// Refused: the empty id, `.` and `..`, anything containing `/` or `..`, a backslash or an
    /// ASCII control character, and anything longer than [`MAX_TENANT_BYTES`] bytes. The dot rule
    /// is the path-traversal guard: a tenant id reaches a filesystem path in
    /// [`crate::FolderStore`], so a `.` run inside it could climb out of the tenant's directory.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] naming the first rule the id breaks.
    pub fn new(raw: &str) -> Result<Self, StoreError> {
        if raw.is_empty() {
            return Err(StoreError::KeyInvalid("the tenant id is empty"));
        }
        if raw == "." {
            return Err(StoreError::KeyInvalid("the tenant id is a dot"));
        }
        if raw.len() > MAX_TENANT_BYTES {
            return Err(StoreError::KeyInvalid("the tenant id is too long"));
        }
        if raw.contains('/') {
            return Err(StoreError::KeyInvalid("the tenant id contains a slash"));
        }
        if raw.contains("..") {
            return Err(StoreError::KeyInvalid("the tenant id contains two dots"));
        }
        if raw.contains('\\') {
            return Err(StoreError::KeyInvalid("the tenant id contains a backslash"));
        }
        // A colon is a drive prefix or an alternate data stream on Windows, where `root.join`
        // of `C:/...` would leave the root; a leading dot would name a hidden or store-owned
        // directory such as `.staging`, whose objects `list` never shows.
        if raw.contains(':') || raw.starts_with('.') {
            return Err(StoreError::KeyInvalid(
                "the tenant id has a colon or starts with a dot",
            ));
        }
        if raw.chars().any(char::is_control) {
            return Err(StoreError::KeyInvalid(
                "the tenant id contains a control character",
            ));
        }
        Ok(Self(raw.to_owned()))
    }

    /// The id as a key component.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One generation's manifest: `<tenant>/manifest/<generation>.json`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ManifestKey {
    /// The tenant that owns the journal.
    pub tenant: TenantId,
    /// The generation the manifest describes.
    pub generation: u64,
}

impl ManifestKey {
    /// Builds the key of one generation's manifest.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is not one safe component.
    pub fn new(tenant: &str, generation: u64) -> Result<Self, StoreError> {
        Ok(Self {
            tenant: TenantId::new(tenant)?,
            generation,
        })
    }
}

/// One sealed segment: `<tenant>/segments/<generation>/<first>-<last>.seg`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SegmentKey {
    /// The tenant that owns the journal.
    pub tenant: TenantId,
    /// The generation the segment belongs to.
    pub generation: u64,
    /// The first sequence number it holds.
    pub first_seq: u64,
    /// The last sequence number it holds.
    pub last_seq: u64,
}

impl SegmentKey {
    /// Builds the key of a segment covering `first_seq..=last_seq`.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is not one safe component or the range is
    /// reversed (a segment holds at least one entry, so `first_seq` may not be after `last_seq`).
    pub fn new(
        tenant: &str,
        generation: u64,
        first_seq: u64,
        last_seq: u64,
    ) -> Result<Self, StoreError> {
        if first_seq > last_seq {
            return Err(StoreError::KeyInvalid(
                "the segment range ends before it starts",
            ));
        }
        Ok(Self {
            tenant: TenantId::new(tenant)?,
            generation,
            first_seq,
            last_seq,
        })
    }
}

/// One remote-sync tail chunk: `<tenant>/tail/<generation>/<batch_id>.chunk`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TailKey {
    /// The tenant that owns the journal.
    pub tenant: TenantId,
    /// The generation the chunk belongs to.
    pub generation: u64,
    /// The relay's batch id inside the generation.
    pub batch_id: u64,
}

impl TailKey {
    /// Builds the key of one tail chunk.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is not one safe component.
    pub fn new(tenant: &str, generation: u64, batch_id: u64) -> Result<Self, StoreError> {
        Ok(Self {
            tenant: TenantId::new(tenant)?,
            generation,
            batch_id,
        })
    }
}

/// A snapshot base image: `<tenant>/snapshots/<generation>/<position>/`, optionally followed by
/// one object name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotKey {
    /// The tenant that owns the journal.
    pub tenant: TenantId,
    /// The generation the snapshot was taken from.
    pub generation: u64,
    /// The stream position the snapshot is the base of.
    pub position: u64,
    /// The object inside the snapshot prefix, or `None` for the prefix itself.
    pub name: Option<String>,
}

impl SnapshotKey {
    /// Builds the key of a snapshot prefix.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is not one safe component.
    pub fn new(tenant: &str, generation: u64, position: u64) -> Result<Self, StoreError> {
        Ok(Self {
            tenant: TenantId::new(tenant)?,
            generation,
            position,
            name: None,
        })
    }

    /// The same snapshot prefix with one object name under it.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the name is empty, contains a slash or is `.`/`..`.
    pub fn with_name(mut self, name: &str) -> Result<Self, StoreError> {
        validate_name(name)?;
        self.name = Some(name.to_owned());
        Ok(self)
    }
}

/// One content-addressed attachment: `<tenant>/blobs/<hash>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BlobKey {
    /// The tenant that owns the blob.
    pub tenant: TenantId,
    /// The lower-case hex SHA-256 of the blob's bytes.
    pub hash: String,
}

impl BlobKey {
    /// Builds the key of one blob.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is not one safe component or `hash` is not
    /// 64 lower-case hex digits.
    pub fn new(tenant: &str, hash: &str) -> Result<Self, StoreError> {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(StoreError::KeyInvalid(
                "the blob hash is not 64 lower-case hex digits",
            ));
        }
        Ok(Self {
            tenant: TenantId::new(tenant)?,
            hash: hash.to_owned(),
        })
    }
}

/// A parsed journal key: exactly one of the five shapes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A generation's manifest.
    Manifest(ManifestKey),
    /// A sealed segment.
    Segment(SegmentKey),
    /// A tail chunk.
    Tail(TailKey),
    /// A snapshot prefix or an object inside one.
    Snapshot(SnapshotKey),
    /// A content-addressed blob.
    Blob(BlobKey),
}

impl Key {
    /// What the key holds.
    #[must_use]
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Manifest(_) => ObjectKind::Manifest,
            Self::Segment(_) => ObjectKind::Segment,
            Self::Tail(_) => ObjectKind::TailChunk,
            Self::Snapshot(_) => ObjectKind::Snapshot,
            Self::Blob(_) => ObjectKind::Blob,
        }
    }

    /// The tenant that owns the object.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        match self {
            Self::Manifest(key) => &key.tenant,
            Self::Segment(key) => &key.tenant,
            Self::Tail(key) => &key.tenant,
            Self::Snapshot(key) => &key.tenant,
            Self::Blob(key) => &key.tenant,
        }
    }

    /// The canonical key string.
    #[must_use]
    pub fn to_key(&self) -> String {
        match self {
            Self::Manifest(key) => format!(
                "{}/{}/{}.json",
                key.tenant,
                MANIFEST_PREFIX,
                number(key.generation)
            ),
            Self::Segment(key) => format!(
                "{}/{}/{}/{}-{}.seg",
                key.tenant,
                SEGMENTS_PREFIX,
                number(key.generation),
                number(key.first_seq),
                number(key.last_seq)
            ),
            Self::Tail(key) => format!(
                "{}/{}/{}/{}.chunk",
                key.tenant,
                TAIL_PREFIX,
                number(key.generation),
                number(key.batch_id)
            ),
            Self::Snapshot(key) => {
                let prefix = format!(
                    "{}/{}/{}/{}/",
                    key.tenant,
                    SNAPSHOTS_PREFIX,
                    number(key.generation),
                    number(key.position)
                );
                match &key.name {
                    Some(name) => format!("{prefix}{name}"),
                    None => prefix,
                }
            }
            Self::Blob(key) => format!("{}/{}/{}", key.tenant, BLOBS_PREFIX, key.hash),
        }
    }

    /// Parses a canonical key.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the key is not one of the five shapes in the canonical
    /// form; the reason names the first rule broken and never echoes the key.
    pub fn parse(raw: &str) -> Result<Self, StoreError> {
        let parts: Vec<&str> = raw.split('/').collect();
        if raw.is_empty() || raw.starts_with('/') {
            return Err(StoreError::KeyInvalid("the key is empty or absolute"));
        }
        if parts.len() < 3 {
            return Err(StoreError::KeyInvalid(
                "the key has fewer components than any journal shape",
            ));
        }
        let tenant = parts[0];
        match (parts[1], parts.len()) {
            (MANIFEST_PREFIX, 3) => Ok(Self::Manifest(ManifestKey {
                tenant: TenantId::new(tenant)?,
                generation: number_of(parts[2].strip_suffix(".json").ok_or(
                    StoreError::KeyInvalid("the manifest key does not end in .json"),
                )?)?,
            })),
            (SEGMENTS_PREFIX, 4) => {
                let body = parts[3].strip_suffix(".seg").ok_or(StoreError::KeyInvalid(
                    "the segment key does not end in .seg",
                ))?;
                let (first, last) = body.split_once('-').ok_or(StoreError::KeyInvalid(
                    "the segment key lacks its sequence range",
                ))?;
                Ok(Self::Segment(SegmentKey::new(
                    tenant,
                    number_of(parts[2])?,
                    number_of(first)?,
                    number_of(last)?,
                )?))
            }
            (TAIL_PREFIX, 4) => Ok(Self::Tail(TailKey {
                tenant: TenantId::new(tenant)?,
                generation: number_of(parts[2])?,
                batch_id: number_of(parts[3].strip_suffix(".chunk").ok_or(
                    StoreError::KeyInvalid("the tail key does not end in .chunk"),
                )?)?,
            })),
            (SNAPSHOTS_PREFIX, 5) => {
                let base = SnapshotKey::new(tenant, number_of(parts[2])?, number_of(parts[3])?)?;
                if parts[4].is_empty() {
                    Ok(Self::Snapshot(base))
                } else {
                    Ok(Self::Snapshot(base.with_name(parts[4])?))
                }
            }
            (BLOBS_PREFIX, 3) => Ok(Self::Blob(BlobKey::new(tenant, parts[2])?)),
            _ => Err(StoreError::KeyInvalid(
                "the key is not one of the five journal shapes",
            )),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_key())
    }
}

/// The typed constructors and the parser, as one entry point.
///
/// Thin by design: callers say *what* they store ([`Layout::segment`]) and the key form lives
/// here, not in the relay.
pub struct Layout;

impl Layout {
    /// One generation's manifest.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is not one safe component.
    pub fn manifest(tenant: &str, generation: u64) -> Result<Key, StoreError> {
        Ok(Key::Manifest(ManifestKey::new(tenant, generation)?))
    }

    /// One sealed segment covering `first_seq..=last_seq`.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is unsafe or the range is reversed.
    pub fn segment(
        tenant: &str,
        generation: u64,
        first_seq: u64,
        last_seq: u64,
    ) -> Result<Key, StoreError> {
        Ok(Key::Segment(SegmentKey::new(
            tenant, generation, first_seq, last_seq,
        )?))
    }

    /// One remote-sync tail chunk.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is not one safe component.
    pub fn tail_chunk(tenant: &str, generation: u64, batch_id: u64) -> Result<Key, StoreError> {
        Ok(Key::Tail(TailKey::new(tenant, generation, batch_id)?))
    }

    /// A snapshot prefix.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is not one safe component.
    pub fn snapshot(tenant: &str, generation: u64, position: u64) -> Result<Key, StoreError> {
        Ok(Key::Snapshot(SnapshotKey::new(
            tenant, generation, position,
        )?))
    }

    /// One object inside a snapshot prefix.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is unsafe or the name is not one component.
    pub fn snapshot_object(
        tenant: &str,
        generation: u64,
        position: u64,
        name: &str,
    ) -> Result<Key, StoreError> {
        Ok(Key::Snapshot(
            SnapshotKey::new(tenant, generation, position)?.with_name(name)?,
        ))
    }

    /// One content-addressed blob.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is unsafe or the hash is not hex.
    pub fn blob(tenant: &str, hash: &str) -> Result<Key, StoreError> {
        Ok(Key::Blob(BlobKey::new(tenant, hash)?))
    }

    /// Parses a canonical key.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the key is not canonical.
    pub fn parse(raw: &str) -> Result<Key, StoreError> {
        Key::parse(raw)
    }

    /// `n` in the zero-padded form every key uses.
    ///
    /// Public because a caller that builds a *prefix* by hand (`<tenant>/tail/<generation>`) has
    /// to pad it the way the layout does, or it would list nothing at all.
    #[must_use]
    pub fn padded(n: u64) -> String {
        number(n)
    }
}

/// `n` zero-padded to [`DIGITS`] digits.
fn number(n: u64) -> String {
    format!("{n:0width$}", width = DIGITS)
}

/// The inverse of [`number`]; only the canonical width is accepted.
fn number_of(raw: &str) -> Result<u64, StoreError> {
    if raw.len() != DIGITS || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(StoreError::KeyInvalid(
            "a sequence number, generation or batch id is not canonical",
        ));
    }
    raw.parse()
        .map_err(|_| StoreError::KeyInvalid("a number in the key does not fit 64 bits"))
}

/// Refuses a snapshot object name that is not one plain component.
fn validate_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(StoreError::KeyInvalid(
            "the snapshot name is not one component",
        ));
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.chars().any(char::is_control)
    {
        return Err(StoreError::KeyInvalid(
            "the snapshot name is not one component",
        ));
    }
    Ok(())
}
