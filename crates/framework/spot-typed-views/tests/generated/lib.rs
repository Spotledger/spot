//! The compile test (WP-MET-04.T1): the generated views exercised end to end against the
//! `StubHost`. A document built through the typed setters is created through
//! `DocumentService::create` with an `Invocation`, loaded again, and read back through the
//! typed getters; the child-row accessors, the select enum and the `.doc()` escape hatch
//! (the runtime-added custom field, ADR-05) are exercised too.
//!
//! The generated views of the 3 sample DocTypes (`Invoice`, `InvoiceLine`, `Party`,
//! `StatusOption`, the helpers) are compiled in verbatim: the source
//! `spot_typed_views::build_views` wrote at build time — the text a real app's `build.rs`
//! emits into its own crate.
//!
//! The StubHost validates links: the `Party` a draft invoice links to must exist when the
//! invoice is created, so the party is created first.

#[path = "harness.rs"]
pub mod harness;

// The generated views: the source `spot_typed_views::build_views` wrote at build time.
include!(concat!(env!("OUT_DIR"), "/generated_views.rs"));