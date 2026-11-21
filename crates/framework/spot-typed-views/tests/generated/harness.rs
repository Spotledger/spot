//! The compile test's harness (WP-MET-04.T1): the 3 fixture DocTypes as host metadata, and
//! the invocation every stub call takes.
//!
//! The host's `DocTypeDef`s come through `spot_meta::Registry` (WP-MET-01's compile of the
//! same fixtures the views were generated from), so the stub validates exactly the
//! definitions the views were built from — plus whatever customisations a test applies
//! through [`host_with`], the runtime path of ADR-05.
//!
//! The lib target carries this module but uses only the generated views; the
//! `gen_views.rs` integration test drives every helper, so unused-in-lib is expected.

#![allow(dead_code)]

use spot_app_api::meta::DocTypeDef;
use spot_app_api::prelude::*;
use spot_app_api::stub::StubHost;
use spot_app_api::{Actor, Invocation};
use spot_meta::Registry;

/// The fixture directory the brief fixes: 3 sample DocTypes. Relative to the nested crate's
/// parent (`spot-typed-views`), whose `CARGO_MANIFEST_DIR-joined` path the brief names.
pub const FIXTURES: &str = "../fixtures/doctypes";

/// The fixtures, parsed.
///
/// # Panics
/// Checked-in fixtures are valid; a load failure is a setup bug.
#[must_use]
pub fn definitions() -> Vec<spot_meta::Definition> {
    spot_meta::load_dir(&fixture_dir()).unwrap_or_else(|e| panic!("fixtures do not load: {e:?}"))
}

/// The fixture directory: the nested crate's parent + `FIXTURES`, normalised so it reads as
/// the fixture path inside `spot-typed-views`.
///
/// # Panics
/// The parent exists; a resolution failure is a setup bug.
#[must_use]
pub fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURES)
        .canonicalize()
        .unwrap_or_else(|e| panic!("fixture directory: {e}"))
}

/// A host with the fixtures defined.
///
/// # Panics
/// The fixtures are valid; a registry failure is a setup bug.
#[must_use]
pub fn host() -> StubHost {
    host_with(|_| ())
}

/// A host whose metadata is the fixtures' registry, customised by `customise` before the
/// effective metadata is taken: the runtime path of ADR-05, where a site adds a custom field
/// and the views — built from the definitions long before — never regenerate and need no
/// rebuild. `custom_test_note` is reachable only through `.doc()` because the views have no
/// accessor for it; the host validates and accepts its value because the metadata knows it.
///
/// # Panics
/// The fixtures are valid; a registry failure is a setup bug.
#[must_use]
pub fn host_with(customise: impl FnOnce(&mut Registry)) -> StubHost {
    let mut registry = Registry::new(definitions()).unwrap_or_else(|e| panic!("registry: {e:?}"));
    customise(&mut registry);
    let host = StubHost::new();
    for def in registry.effective().doctypes() {
        host.define(DocTypeDef::clone(&**def));
    }
    host
}

/// The invocation every stub call takes.
///
/// # Panics
/// The names here are constants; constructing one is a setup bug.
#[must_use]
pub fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("typed-views.test").unwrap_or_else(|e| panic!("site: {e}")),
        actor: Actor::System {
            component: Name::new("typed_views_tests").unwrap_or_else(|e| panic!("component: {e}")),
        },
        command_id: None,
        language: None,
        now: OffsetDateTime::UNIX_EPOCH,
    }
}