//! Typed errors of the update layer, with stable `updater.*` codes.
//!
//! Verification fails on the first check that does not hold, and that check's code names the
//! refusal. Callers match on [`UpdaterError::code`], never on the text. Every code has one entry
//! in `crates/framework/spot-errors/catalogue/updater.toml` with an administrator's remedy.

/// Every error code this crate raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdaterErrorCode {
    /// The manifest document does not parse or a field is missing or invalid.
    ManifestMalformed,
    /// A public key or a key id is not its expected shape.
    KeyMalformed,
    /// The `update.sig` document does not parse.
    SignatureMalformed,
    /// The update is not signed.
    Unsigned,
    /// The signature names a key the anchor does not trust.
    KeyUnknown,
    /// The signing key's window has not opened yet.
    KeyNotYetValid,
    /// The signing key's window has closed.
    KeyExpired,
    /// The signing key is on the revocation list.
    KeyRevoked,
    /// The revocation list's signature does not verify under its issuer.
    RevocationSignatureInvalid,
    /// The revocation list is older than the one the caller already holds.
    RevocationStale,
    /// The Ed25519 signature does not verify over the manifest bytes and payload digest.
    SignatureInvalid,
    /// The payload's length is not the manifest's.
    PayloadSizeMismatch,
    /// The payload's SHA-256 is not the manifest's.
    PayloadDigestMismatch,
    /// A file's length is not the manifest entry's.
    FileSizeMismatch,
    /// A file's SHA-256 is not the manifest entry's.
    FileDigestMismatch,
    /// The offered update is for another product.
    ProductMismatch,
    /// The offered update is on a channel the policy does not allow.
    ChannelNotAllowed,
    /// The offered version is older than the installed version.
    Downgrade,
    /// The offered version equals the installed version but its payload digest differs.
    DigestChanged,
    /// The offered update needs a database schema epoch the installed database cannot reach.
    SchemaEpoch,
    /// The installed version is older than `min_from` and no `via` step is listed.
    MinFrom,
    /// The manifest claims a release time in the future.
    NotReleased,
    /// A path in the staging layout is malformed or missing.
    Layout,
    /// The update journal is missing, malformed, or its checksum does not match.
    Journal,
    /// The payload archive is not a readable ZIP, or an entry is malformed.
    Archive,
    /// An archive entry's name would escape the staging directory.
    ArchiveEscape,
    /// An archive entry's compression ratio or inflated size is over a limit.
    ArchiveBomb,
    /// An archive entry is not listed in the manifest.
    ArchiveUnlisted,
    /// A file the manifest names is missing from the archive.
    ArchiveMissing,
    /// The volume has less free space than the update needs before unpacking.
    DiskSpace,
    /// A write ran out of disk space.
    DiskFull,
    /// A filesystem operation failed.
    Io,
    /// A service control operation failed.
    Service,
    /// The new version did not become ready or healthy within the deadline.
    Health,
    /// A data hook (backup, migrate or restore) failed.
    Hook,
    /// A version pointer names a version directory that does not exist or is incomplete.
    Pointer,
    /// The process was cut by a power failure: injected by the test filesystem, or a real loss.
    PowerCut,
}

impl UpdaterErrorCode {
    /// The stable machine-readable code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManifestMalformed => "updater.manifest",
            Self::KeyMalformed => "updater.key_malformed",
            Self::SignatureMalformed => "updater.signature_malformed",
            Self::Unsigned => "updater.unsigned",
            Self::KeyUnknown => "updater.key_unknown",
            Self::KeyNotYetValid => "updater.key_not_yet_valid",
            Self::KeyExpired => "updater.key_expired",
            Self::KeyRevoked => "updater.key_revoked",
            Self::RevocationSignatureInvalid => "updater.revocation_signature",
            Self::RevocationStale => "updater.revocation_stale",
            Self::SignatureInvalid => "updater.signature",
            Self::PayloadSizeMismatch => "updater.payload_size",
            Self::PayloadDigestMismatch => "updater.payload_digest",
            Self::FileSizeMismatch => "updater.file_size",
            Self::FileDigestMismatch => "updater.file_digest",
            Self::ProductMismatch => "updater.product",
            Self::ChannelNotAllowed => "updater.channel",
            Self::Downgrade => "updater.downgrade",
            Self::DigestChanged => "updater.digest_changed",
            Self::SchemaEpoch => "updater.schema_epoch",
            Self::MinFrom => "updater.min_from",
            Self::NotReleased => "updater.not_released",
            Self::Layout => "updater.layout",
            Self::Journal => "updater.journal",
            Self::Archive => "updater.archive",
            Self::ArchiveEscape => "updater.archive_escape",
            Self::ArchiveBomb => "updater.archive_bomb",
            Self::ArchiveUnlisted => "updater.archive_unlisted",
            Self::ArchiveMissing => "updater.archive_missing",
            Self::DiskSpace => "updater.disk_space",
            Self::DiskFull => "updater.disk_full",
            Self::Io => "updater.io",
            Self::Service => "updater.service",
            Self::Health => "updater.health",
            Self::Hook => "updater.hook",
            Self::Pointer => "updater.pointer",
            Self::PowerCut => "updater.power_cut",
        }
    }
}

/// Everything this crate can refuse an update with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdaterError {
    /// The manifest document does not parse or a field is missing or invalid.
    #[error("the update manifest is malformed: {detail}")]
    ManifestMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// A public key or a key id is not its expected shape.
    #[error("a signing key is malformed: {detail}")]
    KeyMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The `update.sig` document does not parse.
    #[error("the update signature document is malformed: {detail}")]
    SignatureMalformed {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The update is not signed.
    #[error("the update is not signed")]
    Unsigned,
    /// The signature names a key the anchor does not trust.
    #[error("the signing key is not trusted: {key_id}")]
    KeyUnknown {
        /// The key the signature names.
        key_id: String,
    },
    /// The signing key's window has not opened yet.
    #[error("the signing key is not yet valid: {key_id}")]
    KeyNotYetValid {
        /// The key the signature names.
        key_id: String,
    },
    /// The signing key's window has closed.
    #[error("the signing key has expired: {key_id}")]
    KeyExpired {
        /// The key the signature names.
        key_id: String,
    },
    /// The signing key is on the revocation list.
    #[error("the signing key is revoked: {key_id}")]
    KeyRevoked {
        /// The key the signature names.
        key_id: String,
    },
    /// The revocation list's signature does not verify under its issuer.
    #[error("the revocation list's signature does not verify under its issuer")]
    RevocationSignatureInvalid,
    /// The revocation list is older than the one the caller already holds.
    #[error(
        "the revocation list is older than the one already held: held {held}, offered {offered}"
    )]
    RevocationStale {
        /// The generation the caller holds.
        held: u64,
        /// The generation the list came with.
        offered: u64,
    },
    /// The Ed25519 signature does not verify over the manifest bytes and payload digest.
    #[error("the update signature does not verify over this manifest")]
    SignatureInvalid,
    /// The payload's length is not the manifest's.
    #[error("the payload size is wrong: expected {expected}, got {actual}")]
    PayloadSizeMismatch {
        /// The manifest's size.
        expected: u64,
        /// The payload's actual size.
        actual: u64,
    },
    /// The payload's SHA-256 is not the manifest's.
    #[error("the payload digest is wrong: expected {expected}, got {actual}")]
    PayloadDigestMismatch {
        /// The manifest's digest, in lower-case hex.
        expected: String,
        /// The payload's actual digest, in lower-case hex.
        actual: String,
    },
    /// A file's length is not the manifest entry's.
    #[error("the size of {path} is wrong: expected {expected}, got {actual}")]
    FileSizeMismatch {
        /// The file's path in the manifest.
        path: String,
        /// The manifest's size.
        expected: u64,
        /// The file's actual size.
        actual: u64,
    },
    /// A file's SHA-256 is not the manifest entry's.
    #[error("the digest of {path} is wrong: expected {expected}, got {actual}")]
    FileDigestMismatch {
        /// The file's path in the manifest.
        path: String,
        /// The manifest's digest, in lower-case hex.
        expected: String,
        /// The file's actual digest, in lower-case hex.
        actual: String,
    },
    /// The offered update is for another product.
    #[error("the update is for another product: installed {installed}, offered {offered}")]
    ProductMismatch {
        /// The installed product.
        installed: String,
        /// The offered product.
        offered: String,
    },
    /// The offered update is on a channel the policy does not allow.
    #[error("the update channel is not allowed: {channel}")]
    ChannelNotAllowed {
        /// The offered channel.
        channel: String,
    },
    /// The offered version is older than the installed version.
    #[error("an older version than installed is refused: installed {installed}, offered {offered}")]
    Downgrade {
        /// The installed version.
        installed: String,
        /// The offered version.
        offered: String,
    },
    /// The offered version equals the installed version but its payload digest differs.
    #[error(
        "the same version has a different payload digest: installed {installed}, offered {offered}"
    )]
    DigestChanged {
        /// The installed digest, in lower-case hex.
        installed: String,
        /// The offered digest, in lower-case hex.
        offered: String,
    },
    /// The offered update needs a database schema epoch the installed database cannot reach.
    #[error(
        "the update needs a database schema the installed version does not support: installed {installed}, offered {offered}"
    )]
    SchemaEpoch {
        /// The installed database's schema epoch.
        installed: u64,
        /// The offered update's schema epoch.
        offered: u64,
    },
    /// The installed version is older than `min_from` and no `via` step is listed.
    #[error(
        "the installed version must step through {via} first: installed {installed}, min_from {min_from}"
    )]
    MinFrom {
        /// The installed version.
        installed: String,
        /// The manifest's `min_from`.
        min_from: String,
        /// The listed `via`, or a dash when none is listed.
        via: String,
    },
    /// The manifest claims a release time in the future.
    #[error("the manifest is dated in the future: released_at {released_at}, now {now}")]
    NotReleased {
        /// The manifest's `released_at`.
        released_at: i64,
        /// The caller's `now`.
        now: i64,
    },
    /// A path in the staging layout is malformed or missing.
    #[error("the update layout is wrong: {detail}")]
    Layout {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The update journal is missing, malformed, or its checksum does not match.
    #[error("the update journal is not usable: {detail}")]
    Journal {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// The payload archive is not a readable ZIP, or an entry is malformed.
    #[error("the update payload archive is not usable: {detail}")]
    Archive {
        /// What is wrong, in plain words.
        detail: String,
    },
    /// An archive entry's name would escape the staging directory.
    #[error("the archive entry `{entry}` would escape the staging directory")]
    ArchiveEscape {
        /// The offending entry name.
        entry: String,
    },
    /// An archive entry's compression ratio or inflated size is over a limit.
    #[error("the archive entry `{entry}` is refused: {detail}")]
    ArchiveBomb {
        /// The offending entry name.
        entry: String,
        /// Which limit it crossed, in plain words.
        detail: String,
    },
    /// An archive entry is not listed in the manifest.
    #[error("the archive holds `{entry}`, which the manifest does not list")]
    ArchiveUnlisted {
        /// The unlisted entry name.
        entry: String,
    },
    /// A file the manifest names is missing from the archive.
    #[error("the archive is missing `{entry}`, which the manifest lists")]
    ArchiveMissing {
        /// The missing entry name.
        entry: String,
    },
    /// The volume has less free space than the update needs before unpacking.
    #[error("not enough free space to unpack the update: need {needed} bytes, {available} free")]
    DiskSpace {
        /// The bytes the update needs free before unpacking.
        needed: u64,
        /// The bytes the volume reports free.
        available: u64,
    },
    /// A write ran out of disk space.
    #[error("the disk filled up: {detail}")]
    DiskFull {
        /// Where the write was, in plain words.
        detail: String,
    },
    /// A filesystem operation failed.
    #[error("a filesystem operation failed: {detail}")]
    Io {
        /// The operation and the underlying fault, in plain words.
        detail: String,
    },
    /// A service control operation failed.
    #[error("the service control operation failed: {detail}")]
    Service {
        /// The operation and the underlying fault, in plain words.
        detail: String,
    },
    /// The new version did not become ready or healthy within the deadline.
    #[error("the new version is not serving: {detail}")]
    Health {
        /// What failed, in plain words.
        detail: String,
    },
    /// A data hook (backup, migrate or restore) failed.
    #[error("the data hook failed: {detail}")]
    Hook {
        /// Which hook and the underlying fault, in plain words.
        detail: String,
    },
    /// A version pointer names a version directory that does not exist or is incomplete.
    #[error("a version pointer is not trustworthy: {detail}")]
    Pointer {
        /// What the pointer names and why it is not usable.
        detail: String,
    },
    /// The process was cut by a power failure: injected by the test filesystem, or a real loss.
    #[error("the update was interrupted by a power failure")]
    PowerCut,
}

impl UpdaterError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> UpdaterErrorCode {
        match self {
            Self::ManifestMalformed { .. } => UpdaterErrorCode::ManifestMalformed,
            Self::KeyMalformed { .. } => UpdaterErrorCode::KeyMalformed,
            Self::SignatureMalformed { .. } => UpdaterErrorCode::SignatureMalformed,
            Self::Unsigned => UpdaterErrorCode::Unsigned,
            Self::KeyUnknown { .. } => UpdaterErrorCode::KeyUnknown,
            Self::KeyNotYetValid { .. } => UpdaterErrorCode::KeyNotYetValid,
            Self::KeyExpired { .. } => UpdaterErrorCode::KeyExpired,
            Self::KeyRevoked { .. } => UpdaterErrorCode::KeyRevoked,
            Self::RevocationSignatureInvalid => UpdaterErrorCode::RevocationSignatureInvalid,
            Self::RevocationStale { .. } => UpdaterErrorCode::RevocationStale,
            Self::SignatureInvalid => UpdaterErrorCode::SignatureInvalid,
            Self::PayloadSizeMismatch { .. } => UpdaterErrorCode::PayloadSizeMismatch,
            Self::PayloadDigestMismatch { .. } => UpdaterErrorCode::PayloadDigestMismatch,
            Self::FileSizeMismatch { .. } => UpdaterErrorCode::FileSizeMismatch,
            Self::FileDigestMismatch { .. } => UpdaterErrorCode::FileDigestMismatch,
            Self::ProductMismatch { .. } => UpdaterErrorCode::ProductMismatch,
            Self::ChannelNotAllowed { .. } => UpdaterErrorCode::ChannelNotAllowed,
            Self::Downgrade { .. } => UpdaterErrorCode::Downgrade,
            Self::DigestChanged { .. } => UpdaterErrorCode::DigestChanged,
            Self::SchemaEpoch { .. } => UpdaterErrorCode::SchemaEpoch,
            Self::MinFrom { .. } => UpdaterErrorCode::MinFrom,
            Self::NotReleased { .. } => UpdaterErrorCode::NotReleased,
            Self::Layout { .. } => UpdaterErrorCode::Layout,
            Self::Journal { .. } => UpdaterErrorCode::Journal,
            Self::Archive { .. } => UpdaterErrorCode::Archive,
            Self::ArchiveEscape { .. } => UpdaterErrorCode::ArchiveEscape,
            Self::ArchiveBomb { .. } => UpdaterErrorCode::ArchiveBomb,
            Self::ArchiveUnlisted { .. } => UpdaterErrorCode::ArchiveUnlisted,
            Self::ArchiveMissing { .. } => UpdaterErrorCode::ArchiveMissing,
            Self::DiskSpace { .. } => UpdaterErrorCode::DiskSpace,
            Self::DiskFull { .. } => UpdaterErrorCode::DiskFull,
            Self::Io { .. } => UpdaterErrorCode::Io,
            Self::Service { .. } => UpdaterErrorCode::Service,
            Self::Health { .. } => UpdaterErrorCode::Health,
            Self::Hook { .. } => UpdaterErrorCode::Hook,
            Self::Pointer { .. } => UpdaterErrorCode::Pointer,
            Self::PowerCut => UpdaterErrorCode::PowerCut,
        }
    }
}
