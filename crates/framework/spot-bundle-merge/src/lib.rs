//! `spot-bundle-merge` — three-way merge and conflict detection between customization bundles
//! (blueprint WS19, WP-ALM-03; ADR-22).
//!
//! A customization bundle (`spot-custom-bundle`, WP-ALM-01) carries everything a site changed on
//! top of its apps: custom fields and property overrides, view layers, scripts, rules and
//! fixtures. `spot-custom-bundle::diff` is the plain structural diff; this crate is the merge
//! logic it deliberately left out.
//!
//! # Item-wise three-way merge
//!
//! [`merge3`] takes a common ancestor (`base`) and two changed versions (`ours`, `theirs`). For
//! every section kind it decomposes the section's files into **items** — the stable key to
//! canonical bytes `spot_custom_bundle::Section::items` returns: one custom field, one property
//! override, one view layer, one script, one rule, one fixture DocType — and compares the three
//! versions of each key:
//!
//! - the same bytes on both sides — clean;
//! - changed on one side only — that side is taken;
//! - changed differently — a [`Conflict`] with the kind `both_modified`, `modified_vs_removed` or
//!   `both_added_differently`, carrying all three versions.
//!
//! Nothing is resolved silently: [`MergeResult::merged`] carries only the cleanly merged items,
//! and a conflicted item is absent until [`resolve`] supplies a [`Choice`] for every conflict.
//!
//! # Section-aware semantic conflicts
//!
//! Some breaks survive a clean item-wise merge because they span more than one item. [`merge3`]
//! reports them too, as [`ConflictKind::Semantic`] conflicts: an override on a custom field the
//! other side removed, a custom field anchored (`insert_after`) after a removed field, two sides
//! anchoring different fields to the same predecessor, two sides changing different scripts bound
//! to the same hook point, and a merged metadata that fails `spot_meta::Registry::build`.
//!
//! # Against a platform upgrade
//!
//! [`check_against_upgrade`] reuses `spot-migration-conflicts` (WP-MIG-03) through the
//! [`UpgradeCheck`] trait: it rebuilds the release context from the bundle's customisations and
//! returns the release's conflicts as [`ConflictKind::Upgrade`] items, so a reviewer sees both a
//! bundle-versus-bundle merge and a bundle-versus-release check.
//!
//! # Resolution and report
//!
//! [`resolve`] applies one [`Choice`] (`Ours`, `Theirs`, `Base` or `Custom`) per conflict, refuses
//! to build a bundle while any conflict is unresolved, and returns a [`ResolutionLog`] recording
//! who chose what. [`MergeResult::render_text`] is the plain-text report a reviewer reads.
//!
//! # What this crate does not do
//!
//! It never applies a bundle to a site, never reads a clock or the environment (time and every
//! input are arguments), and never resolves a conflict without an explicit choice. It contains no
//! `unsafe`, no `unwrap`/`expect` outside tests, and no floating-point arithmetic (ADR-12).

#![forbid(unsafe_code)]

pub mod error;
mod items;
mod merge;
pub mod model;
mod report;
mod resolve;
mod semantic;
mod upgrade;

pub use error::{BundleMergeError, codes};
pub use merge::{merge3, merge3_with};
pub use model::{
    Choice, Conflict, ConflictKind, Edit, ItemVersion, MergeResult, Resolution, ResolutionEntry,
    ResolutionLog, Resolved, SemanticKind, UpgradeRef, Variants,
};
pub use resolve::resolve;
pub use upgrade::{MigrationConflicts, UpgradeCheck, check_against_upgrade};
