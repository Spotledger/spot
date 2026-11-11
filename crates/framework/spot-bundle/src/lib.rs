//! The app bundle (WP-STR-01, blueprint WS11): the one file an app is shipped as.
//!
//! A [`Bundle`] is a ZIP archive with exactly `manifest.toml`, `app.wasm` (a WebAssembly
//! **component**), and optionally `assets/**`. It is read from bytes, **validated without
//! running anything**, and exposed with a canonical digest that signing and the installer
//! build on. No signing, no installation, no network.
//!
//! # Reading (`Bundle::open`)
//!
//! [`Bundle::open`] reports every problem it can find, not only the first:
//!
//! - the archive parses; no duplicate entry names, including names that differ only by
//!   case (they collide on Windows);
//! - **path safety:** `..` segments, absolute paths, backslashes, drive letters, NUL and
//!   non-UTF-8 names are rejected (normalisation is total and unit-tested);
//! - **size limits** ([`Limits`]: compressed, total inflated, entries, single file) are
//!   enforced against the bytes actually inflated, never only the sizes the archive
//!   declares — a zip bomb is refused without allocating its full size;
//! - `manifest.toml` parses and validates through `spot-manifest`; its `manifest.*` codes
//!   pass through in the `bundle.bad_manifest` message;
//! - `app.wasm` must start with the component header `00 61 73 6d 0d 00 01 00` (a core
//!   module starts `00 61 73 6d 01 ...` and is refused) and must parse and validate with
//!   `wasmparser`;
//! - **assets:** every `[[assets]]` path the manifest declares exists under `assets/`, and
//!   every file under `assets/` is declared;
//! - the manifest's `requires_host` range is checked against the [`HostInfo`] version.
//!
//! Directory entries are allowed and ignored; any other entry is
//! `bundle.unexpected_entry`.
//!
//! # Privileges (`Bundle::check_privileges`)
//!
//! [`Policy`] caps what a bundle may declare; [`Bundle::check_privileges`] applies it along
//! with the import table (mirroring `spot-wasm`'s, which stays authoritative there):
//!
//! - an import the host does not know is `bundle.unknown_import`;
//! - a known, capability-bearing import the manifest does not cover is
//!   `bundle.import_without_capability`;
//! - declared capabilities the component never imports are warnings, returned in
//!   [`Report::unused_capabilities`], not errors;
//! - declarations beyond the policy are `bundle.over_privileged`, naming the capability
//!   and the limit.
//!
//! # The canonical digest (`Bundle::digest`)
//!
//! SHA-256 over a fixed listing: for each entry in byte-wise sorted name order,
//! `len(name) | name | len(content) | content`, with fixed-width little-endian lengths and
//! the domain prefix `spotledger-bundle-v1\0`. The digest does **not** depend on ZIP
//! ordering, timestamps, compression level or extra fields: two archives with the same
//! content packed differently have the same digest, one changed byte gives a different
//! one. [`Digest`] has `to_hex()` and `FromStr`. [`BundleBuilder`] packs deterministically
//! for the `spot` CLI (WP-EXT-07).
//!
//! # Error codes
//!
//! Errors are [`BundleError`] `{ code, path, message }` with stable `bundle.*` codes. The
//! registry, in the order the checks can produce them:
//!
//! - `bundle.bad_archive` — the bytes are not a ZIP archive at all.
//! - `bundle.too_many_entries` — more entries than `Limits::max_entries` allow.
//! - `bundle.illegal_path` — a name that escapes the bundle: `..` segments, absolute
//!   paths, backslashes, drive letters, NUL, non-UTF-8, or empty after normalisation.
//! - `bundle.bad_symlink` — a symlink entry.
//! - `bundle.duplicate_entry` — two entries normalise to the same name (case included).
//! - `bundle.too_large` — the archive, one inflated file, or the total inflated size
//!   exceeds the limits; enforced while reading, so a bomb dies early.
//! - `bundle.bad_deflate` — an entry does not decompress, or its CRC does not match.
//! - `bundle.unexpected_entry` — an entry outside the fixed layout.
//! - `bundle.missing_entry` — `manifest.toml` or `app.wasm` is absent.
//! - `bundle.bad_manifest` — `manifest.toml` failed in `spot-manifest`.
//! - `bundle.bad_component` — `app.wasm` is not a component, or does not parse as one.
//! - `bundle.missing_asset` — a declared `[[assets]]` path is not in the bundle.
//! - `bundle.undeclared_asset` — a file under `assets/` the manifest does not declare.
//! - `bundle.host_required` — `requires_host` excludes the host's version.
//! - `bundle.unknown_import` — the component imports something the host does not know.
//! - `bundle.import_without_capability` — a known import without its declared capability.
//! - `bundle.over_privileged` — the manifest declares beyond the policy.

#![forbid(unsafe_code)]

pub(crate) mod bundle;
pub(crate) mod error;
pub(crate) mod imports;
pub(crate) mod imports_check;
pub(crate) mod paths;

pub mod builder;
pub mod digest;
pub mod limits;
pub mod policy;

pub use crate::builder::BundleBuilder;
pub use crate::bundle::{Bundle, HostInfo, Report};
pub use crate::digest::{Digest, DigestFromStrError};
pub use crate::error::BundleError;
pub use crate::limits::Limits;
pub use crate::policy::{Policy, UnusedCapability};
