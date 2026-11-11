//! Spotledger scaffolding (WP-DX-01.T1): generate a new app, DocType, native controller,
//! TypeScript script and permission set from templates into a virtual file tree, with a name
//! and path safety layer, and prove every generated app packs, validates and loads.
//!
//! # Shape
//!
//! - [`tree::FileTree`]: the ordered virtual tree every generator writes into;
//!   [`FileTree::write_to_disk`](tree::FileTree::write_to_disk) is the only I/O, and it
//!   refuses an absolute path, `..`, a symlink on the path, an overwrite without `--force`
//!   and a write that resolves outside the output root.
//! - [`names`]: the storage-name rules, the reserved words and prefixes, and the derivation
//!   of a storage name from a human label.
//! - [`generate`]: `app new`, `doctype add`, `controller add` and `script add`, each filling a
//!   [`FileTree`]; edits to an existing app go through [`manifest_edit`] (the parsed
//!   `spot-manifest` model and `toml_edit`), never a regex over the file.
//! - [`render`]/[`templates`]: the workspace's one template engine (`spot-template`), over
//!   `include_str!`ed sources under `templates/`.
//!
//! # The proof
//!
//! The test suite generates 24 argument combinations into temporary directories and then runs
//! `spot-pack`'s `check` and `pack`, loads the DocTypes through `spot-meta` into a catalogue,
//! loads the permission files with `spot-permissions`, registers a generated controller
//! against a test `spot-controllers` registry and transpiles a generated script with
//! `spot-ts-transpile`. A template that rots when a format changes fails the build.
//!
//! # What this crate does not do
//!
//! Re-implement manifest, DocType or pack validation (it calls the crates that own them), run
//! a process, read a clock, or touch the network.

#![forbid(unsafe_code)]

pub mod cli;
pub mod codes;
pub mod error;
pub mod generate;
pub mod manifest_edit;
pub mod names;
pub mod render;
pub mod templates;
pub mod tree;

pub use error::{Result, ScaffoldError};
pub use names::{AppName, DocTypeName, FieldName};
pub use tree::{FileTree, RelPath, WritePolicy, WriteReport};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-scaffold";
