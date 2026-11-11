//! `spot-files`: byte storage for attachments and assets (blueprint §3, WP-PLT-10).
//!
//! Three pieces:
//!
//! - [`store::FileStore`]: the object-safe interface — [`store::Body`] bytes stream in and
//!   out, every operation is scoped to a [`spot_core::SiteId`], and async methods return
//!   pinned boxed `Send` futures so callers can hold `Arc<dyn FileStore>`;
//! - [`local::LocalFileStore`], [`memory::MemoryFileStore`] and
//!   [`object::ObjectFileStore`]: the disk-backed default, the in-memory provider, and the
//!   S3/Azure providers of WP-PLT-10.T2 (behind the optional `s3` and `azure` features,
//!   with [`object::S3Config`] and [`object::AzureConfig`]);
//! - [`contract_tests`]: the behaviour suite every provider must pass.
//!
//! Keys arrive as validated [`key::FileKey`]s (never invented here from a user file name);
//! there are no permission checks — file *records* belong to the document engine, this
//! crate stores bytes and reports [`error::FileError`]s.

pub mod contract_tests;
pub mod error;
pub mod key;
pub mod local;
pub mod memory;
#[cfg(any(feature = "s3", feature = "azure"))]
pub mod object;
pub mod store;

#[cfg(any(feature = "s3", feature = "azure"))]
pub use object::{
    AzureConfig, AzureCredential, ObjectFileStore, PART_SIZE, S3Config, SINGLE_PUT_LIMIT,
};
