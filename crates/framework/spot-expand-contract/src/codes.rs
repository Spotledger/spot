//! The stable `expand_contract.*` codes of this crate.
//!
//! Two vocabularies live here: the codes of a typed [`Error`](crate::Error) (input that cannot
//! be read), and the codes of a [`Refusal`](crate::check::Refusal) (a release the usage gate
//! rejects). Every constant has one entry in
//! `crates/framework/spot-errors/catalogue/expand_contract.toml`; the catalogue's summary is
//! the sentence a human reads and the code is the stable identity a report matches on.
//!
//! The codes are a library-internal vocabulary: nothing here raises a
//! [`spot_app_api::AppError`](https://docs.rs/spot-app-api), so every catalogue entry's `class`
//! is `none`.

/// An object name is not `table` or `table.column`.
pub const BAD_OBJECT: &str = "expand_contract.bad_object";
/// A usage manifest is not valid TOML, or names a field the format does not carry.
pub const BAD_MANIFEST: &str = "expand_contract.bad_manifest";
/// A release label is empty or unusable.
pub const BAD_RELEASE: &str = "expand_contract.bad_release";
/// A contract step ships in the same release as the code that stops using its object.
pub const CONTRACT_TOO_EARLY: &str = "expand_contract.contract_too_early";
/// A contract step removes an object the release's own code still reads or writes.
pub const CONTRACT_WHILE_USED: &str = "expand_contract.contract_while_used";
/// A contract step names no usable table or column.
pub const CONTRACT_OBJECT_MISSING: &str = "expand_contract.contract_object_missing";

/// Every code this crate can report, for a listing and the catalogue-coverage test.
pub const ALL: &[&str] = &[
    BAD_OBJECT,
    BAD_MANIFEST,
    BAD_RELEASE,
    CONTRACT_TOO_EARLY,
    CONTRACT_WHILE_USED,
    CONTRACT_OBJECT_MISSING,
];
