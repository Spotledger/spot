//! The [`ObjectFileStore`]: one [`FileStore`](crate::store::FileStore) over
//! object-store-backed services (WP-PLT-10.T2).
//!
//! Two constructors select the backend, each behind its own optional feature so the crate
//! and the workspace never pull a cloud dependency without being asked to:
//!
//! - feature `s3` — [`S3Config`] + [`new_s3`]: AWS S3 and S3-compatible services (MinIO,
//!   Cloudflare R2, Wasabi);
//! - feature `azure` — [`AzureConfig`] + [`new_azure`]: Azure Blob Storage (including the
//!   Azurite emulator).
//!
//! Keys are stored as `<site>/<key>`: one [`SiteId`] is a flat prefix inside a single
//! bucket or container, and tenants stay isolated because every operation is re-scoped by
//! the `<site>/` prefix ([`key::FileKey`] validation keeps the text path-safe). Behaviour
//! follows the `FileStore` contract; the backend choices are:
//!
//! - every put streams its body once through a local spool file, hashing on the fly,
//!   and the `[u8; 32]` SHA-256 digest
//!   ([`StoredFile::sha256`](crate::store::StoredFile::sha256)) and the content type
//!   travel as object attributes, so `head` and `list` answer without reading bodies;
//! - a body up to `SINGLE_PUT_LIMIT` is PUT as one object, which makes conditional
//!   creates (`overwrite = false`) atomic; a larger body is uploaded again in 8 MiB
//!   parts (the AWS minimum for non-final parts and the equal-size rule R2 applies);
//! - `delete` HEADs first — S3's DeleteObject is idempotent, and a missing key must
//!   report `false`, not success;
//! - `list` enumerates with `list_with_delimiter`, walking the common prefixes the
//!   delimiter hands back (a key inside a directory only appears at its level),
//!   validates and sorts client-side, pages by the `after` cursor, and HEADs only the
//!   entries of the returned page (listings carry no attributes).
//!
//! Documented compromises inherent to cloud object stores; callers on the `s3`/`azure`
//! features must not assume what `local` guarantees unconditionally:
//!
//! - `get` returns the whole object collected in memory (the contract reads whole
//!   files);
//! - `list` walks every directory level under its prefix and pages in memory, then
//!   issues one HEAD per entry of the page;
//! - a conditional create of a body larger than `SINGLE_PUT_LIMIT` is guarded by a HEAD
//!   around the multipart upload, not one atomic request (object stores cannot
//!   conditionally initiate a multipart upload), so two concurrent creates of the same
//!   large key may both succeed;
//! - oversized bodies pass through a local spool file while hashing; a failed or
//!   over-limit upload removes the spool and nothing reaches the backend, and a
//!   multipart upload that fails after it started is aborted explicitly — parts
//!   uploaded to S3 would otherwise linger and could be completed into an object
//!   by a stale client (Azure blocks simply expire).
//!
//! The implementation lives in the submodules: [`config`] (the provider settings),
//! [`client`] (construction from the enabled feature), [`metadata`] (the mapping
//! between object-store paths/attributes and the contract's keys and metadata),
//! [`spool`] (the local spool every put streams through), [`filestore`] (the
//! [`FileStore`] implementation itself), and the fault-injecting unit tests.

mod client;
mod config;
mod filestore;
mod metadata;
mod spool;

use std::sync::Arc;

use object_store::ObjectStore;

pub use config::{AzureConfig, AzureCredential, S3Config};
pub use metadata::{PART_SIZE, SINGLE_PUT_LIMIT};

/// Which backend a store was built for — construction and messages only; the behaviour
/// is shared, so both backends run the same code paths.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Backend {
    /// Built by [`new_s3`]; the variant exists regardless of the enabled feature so
    /// the behaviour switch and the Debug and Display impls stay uniform.
    #[allow(dead_code)]
    S3,
    /// Built by [`new_azure`]; the variant exists regardless of the enabled feature so
    /// the behaviour switch and the Debug and Display impls stay uniform.
    #[allow(dead_code)]
    Azure,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::S3 => "s3",
            Backend::Azure => "azure",
        })
    }
}

/// The one object-store provider: an S3-compatible or an Azure Blob backend behind the
/// `s3`/`azure` features, sharing every code path of this module.
pub struct ObjectFileStore {
    /// The shared, object-safe client (`Arc` because the `FileStore` futures borrow it).
    store: Arc<dyn ObjectStore>,
    /// The backend the config built; for messages only.
    backend: Backend,
}

#[cfg(test)]
mod tests;
