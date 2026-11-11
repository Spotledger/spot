//! The mapping between the object store's world and the contract's: paths, errors,
//! attributes, digests and timestamps. Pure functions — no I/O, no backend calls.

use object_store::path::Path as ObjectPath;
use object_store::{Attribute, AttributeValue, Attributes, Error as StoreError, ObjectMeta};
use spot_core::SiteId;

use crate::error::FileError;
use crate::key::{FileKey, FileKeyPrefix};
use crate::store::{FileMeta, StoredHash};

/// The user metadata attribute the puts stamp with the `sha256` of the stored bytes.
const SHA256_METADATA: &str = "sha256";

/// The digest reported for an object the provider did not put (no `sha256` metadata on
/// it): all-zero, with the size and bytes still trustworthy.
const UNKNOWN_SHA256: [u8; 32] = [0; 32];

/// The content type reported when an object carries none.
const UNKNOWN_CONTENT_TYPE: &str = "application/octet-stream";

/// Bodies up to this size are PUT as a single, conditionally atomic request; larger ones
/// are spooled and uploaded by parts. Public so callers and the multipart tests can size
/// bodies against the provider's boundaries exactly.
pub const SINGLE_PUT_LIMIT: u64 = 8 * 1024 * 1024;

/// The exact size of non-final multipart parts: AWS requires at least 5 MiB there, R2
/// wants equal-sized parts, Azure has no constraint — one number satisfies all. Public
/// so the multipart tests here and in `tests/object_contract.rs` can size bodies past
/// the boundaries exactly.
pub const PART_SIZE: usize = 8 * 1024 * 1024;

/// Maps the object store's error into the contract's stable errors: `AlreadyExists` and
/// `NotFound` keep their identities, reported against the key text (the path with the
/// `<site>/` prefix off, because callers name keys, not objects); anything else is a
/// backend-specific `files.io` failure carrying the action that hit it.
pub(super) fn store_error(error: StoreError, action: &str, site: &SiteId) -> FileError {
    match error {
        StoreError::AlreadyExists { path, .. } => FileError::AlreadyExists {
            key: object_key_text(site, &path),
        },
        StoreError::NotFound { path, .. } => FileError::NotFound {
            key: object_key_text(site, &path),
        },
        other => FileError::io(format!("{action}: {other}")),
    }
}

/// The key text of an object store path: the site prefix is stripped, so the reported
/// key reads exactly like the `FileKey` the caller passed in.
fn object_key_text(site: &SiteId, path: &str) -> String {
    let prefix = format!("{site}/");
    path.strip_prefix(&prefix).unwrap_or(path).to_owned()
}

/// The object path of `key` under `site`: `<site>/<segment>/<rest>`. Both parts were
/// validated on the way in, so the text is never empty and never ambiguous.
pub(super) fn object_path(site: &SiteId, key: &FileKey) -> ObjectPath {
    ObjectPath::from(format!("{site}/{key}"))
}

/// The object path a list matches below `site`: `<site>/<prefix>`, an empty prefix
/// ending in `/`, which keeps site ids that prefix each other from aliasing lists.
pub(super) fn list_prefix(site: &SiteId, prefix: &FileKeyPrefix) -> ObjectPath {
    ObjectPath::from(format!("{site}/{}", prefix.as_str()))
}

/// The contract's metadata of a stored object: the key, the object store's length and
/// modification stamp, the content type and the `sha256` digest read off the
/// attributes (object_store 0.14 listings carry no attributes, `ObjectMeta` no
/// content type).
pub(super) fn meta_of(key: FileKey, meta: &ObjectMeta, attributes: &Attributes) -> FileMeta {
    let content_type = match attributes.get(&Attribute::ContentType) {
        Some(value) => {
            let text: &str = value.as_ref();
            text.to_owned()
        }
        None => UNKNOWN_CONTENT_TYPE.to_owned(),
    };
    FileMeta {
        key,
        size: meta.size,
        sha256: sha256_of_attributes(attributes),
        content_type,
        modified: to_offset_time(
            meta.last_modified.timestamp(),
            meta.last_modified.timestamp_subsec_nanos(),
        ),
    }
}

/// The digest an object's `sha256` metadata reports, parsed back from the hex text;
/// [`UNKNOWN_SHA256`] when the object carries none or malformed text — an object this
/// provider did not put.
fn sha256_of_attributes(attributes: &Attributes) -> [u8; 32] {
    let Some(value) = attributes.get(&Attribute::Metadata(SHA256_METADATA.into())) else {
        return UNKNOWN_SHA256;
    };
    let hex: &str = value.as_ref();
    let mut digest = UNKNOWN_SHA256;
    if hex.len() != digest.len() * 2 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return UNKNOWN_SHA256;
    }
    for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
        // The text is validated hex, so both digits always parse.
        let high = (pair[0] as char).to_digit(16).unwrap_or_default() as u16;
        let low = (pair[1] as char).to_digit(16).unwrap_or_default() as u16;
        *byte = (high * 16 + low) as u8;
    }
    digest
}

/// The lower-case hex of a digest — the text form the `sha256` metadata carries.
fn hex_sha256(hash: &StoredHash) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(2 * hash.0.len());
    for byte in hash.0 {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 0x0f) as usize] as char);
    }
    text
}

/// The attributes a put stamps: the content type — always written, so reads never
/// guess between "missing" and "default" — and the `sha256` hex digest.
pub(super) fn put_attributes(content_type: &str, hash: &StoredHash) -> Attributes {
    let mut attributes = Attributes::new();
    attributes.insert(
        Attribute::ContentType,
        AttributeValue::from(content_type.to_owned()),
    );
    attributes.insert(
        Attribute::Metadata(SHA256_METADATA.into()),
        AttributeValue::from(hex_sha256(hash)),
    );
    attributes
}

/// The object store's modification stamp becomes the contract's
/// [`time::OffsetDateTime`]: seconds and nanos travel over exactly, which every
/// plausible store stamp (1970..2262) allows; a wilder stamp clamps to the epoch
/// rather than failing a whole read or listing over it. The stamp's chrono type is
/// never named here — the brief's allowed dependencies do not include chrono, so the
/// caller extracts its unix seconds and sub-second nanos through method calls.
fn to_offset_time(seconds: i64, subsec_nanos: u32) -> time::OffsetDateTime {
    let whole = time::OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    whole + time::Duration::nanoseconds(i64::from(subsec_nanos))
}
