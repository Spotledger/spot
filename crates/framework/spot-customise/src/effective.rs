//! The effective definitions of a site: the base definitions the installed apps ship, with the
//! applied customisation records folded over them (WP-EXT-09.T3).
//!
//! This is the fold [`CustomiseService`](crate::CustomiseService) runs whenever it brings a
//! node's snapshot in line with the records (`assemble` for the [`View::Current`] records, then
//! `build` through `spot-meta`), exposed as a function of its two inputs so a caller that has no
//! running service (the `spot types generate` command, which renders the `.d.ts` of what a site
//! serves) gets exactly what a node of that site serves, not a second implementation of it.

use spot_meta::{BuildOptions, Definition};
use spot_meta_live::Version;

use crate::assemble::{View, assemble, build};
use crate::error::{CustomiseError, CustomiseResult};
use crate::records::Records;

/// The definitions a node serves for `base` and `records`: every custom DocType, every custom
/// field and every property override that is in the schema (`applied`, or `removing` and still
/// served), on top of `base`, validated and merged by `spot-meta`.
///
/// It is the fold the service itself runs when it converges a node, with the default
/// [`BuildOptions`] (no seam check, no derivations). A site whose service is built with other
/// options calls [`effective_definitions_with`] with the same options.
///
/// Records that are `pending`, `failed` or `removed` do not count: they are not in what the
/// site serves. Nothing is written and nothing is cached.
///
/// # Errors
/// [`CustomiseError::Invalid`] listing **every** problem when a record does not parse or the
/// merged metadata does not build (the issues carry the stable `customise.*` codes and a path of
/// the form `<doctype>` or `<doctype>.<field>`). Never panics.
pub fn effective_definitions(
    base: &[Definition],
    records: &Records,
) -> CustomiseResult<Vec<Definition>> {
    effective_definitions_with(base, records, &BuildOptions::new())
}

/// As [`effective_definitions`], for a build with the given options (the known seams and
/// derivations the service was constructed with).
///
/// # Errors
/// As [`effective_definitions`].
pub fn effective_definitions_with(
    base: &[Definition],
    records: &Records,
    options: &BuildOptions,
) -> CustomiseResult<Vec<Definition>> {
    // The version only labels the snapshot; the definitions do not depend on it.
    let snapshot = build(
        assemble(base, records, View::Current),
        options,
        Version::new(1),
    )
    .map_err(|issues| CustomiseError::Invalid { issues })?;
    Ok(snapshot.meta().definitions().to_vec())
}
