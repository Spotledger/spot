//! Resolution: semver matching, the provider-before-consumer order, cycles, capability
//! escalation, grants and the blast radius.
#![allow(clippy::unwrap_used)]

use semver::{Version, VersionReq};
use spot_app_compose::interface::{
    AppInterfaces, Capability, ExportedInterface, ImportedInterface, TypeRef,
};
use spot_app_compose::{codes, resolve};
use spot_manifest::{Capabilities, DocTypeAccess, FileAccess};

fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

fn req(text: &str) -> VersionReq {
    VersionReq::parse(text).unwrap()
}

fn export(app: &str, interface: &str, version: &str) -> ExportedInterface {
    ExportedInterface::new(app, interface, v(version))
}

fn import(app: &str, interface: &str, range: &str) -> ImportedInterface {
    ImportedInterface::new(app, interface, req(range))
}

fn caps(read: &[&str], write: &[&str]) -> Capabilities {
    Capabilities {
        network: Vec::new(),
        email: false,
        files: FileAccess::None,
        doctypes: DocTypeAccess {
            read: read.iter().map(|s| (*s).to_owned()).collect(),
            write: write.iter().map(|s| (*s).to_owned()).collect(),
        },
    }
}

fn host_app() -> AppInterfaces {
    AppInterfaces::new("host", v("1.0.0"))
        .declared(caps(&["Customer"], &[]))
        .export(
            export("host", "pricing.adjust", "1.0.0")
                .function(
                    spot_app_compose::FunctionSig::new("adjust")
                        .param("amount", TypeRef::S64)
                        .result(TypeRef::S64),
                )
                .grant(Capability::ReadDocType("Customer".to_owned())),
        )
}

fn extension() -> AppInterfaces {
    AppInterfaces::new("tax_extension", v("1.0.0"))
        .import(import("tax_extension", "pricing.adjust", "^1.0"))
        .declared(caps(&["Customer"], &[]))
}

#[test]
fn resolves_and_orders_providers_before_consumers() {
    let resolution = resolve(&[host_app()], &extension()).unwrap();
    assert_eq!(
        resolution.order,
        vec!["host".to_owned(), "tax_extension".to_owned()]
    );
    assert_eq!(resolution.bindings.len(), 1);
    let binding = &resolution.bindings[0];
    assert_eq!(binding.importer, "tax_extension");
    assert_eq!(binding.provider, "host");
    assert_eq!(binding.version, v("1.0.0"));
    assert_eq!(
        resolution.obtained_by("tax_extension"),
        &[Capability::ReadDocType("Customer".to_owned())]
    );
    assert!(resolution.blast_radius.is_empty());
}

#[test]
fn highest_matching_version_wins_and_ties_break_by_provider() {
    let mut old = host_app();
    old.exports[0].version = v("1.0.0");
    let mut newer = host_app();
    newer.app = "host_b".to_owned();
    newer.exports[0].app = "host_b".to_owned();
    newer.exports[0].version = v("1.4.0");
    let resolution = resolve(&[old, newer], &extension()).unwrap();
    assert_eq!(resolution.bindings[0].provider, "host_b");
    assert_eq!(resolution.bindings[0].version, v("1.4.0"));
}

#[test]
fn missing_provider_is_reported() {
    let problems = resolve(&[], &extension()).unwrap_err();
    assert!(problems.has(codes::MISSING_PROVIDER), "{problems}");
    assert_eq!(problems.len(), 1);
}

#[test]
fn optional_import_without_provider_is_allowed() {
    let candidate = AppInterfaces::new("tax_extension", v("1.0.0"))
        .import(import("tax_extension", "pricing.adjust", "^1.0").optional())
        .declared(caps(&[], &[]));
    let resolution = resolve(&[], &candidate).unwrap();
    assert!(resolution.bindings.is_empty());
    assert_eq!(resolution.order, vec!["tax_extension".to_owned()]);
}

#[test]
fn version_conflict_names_the_offered_versions() {
    let host =
        AppInterfaces::new("host", v("2.0.0")).export(export("host", "pricing.adjust", "2.0.0"));
    let problems = resolve(&[host], &extension()).unwrap_err();
    assert!(problems.has(codes::VERSION_CONFLICT), "{problems}");
    assert!(problems.to_string().contains("2.0.0"), "{problems}");
}

#[test]
fn a_cycle_between_apps_is_refused_with_the_witness() {
    let a = AppInterfaces::new("a", v("1.0.0"))
        .export(export("a", "a.seam", "1.0.0"))
        .import(import("a", "b.seam", "^1.0"));
    let b = AppInterfaces::new("b", v("1.0.0"))
        .export(export("b", "b.seam", "1.0.0"))
        .import(import("b", "a.seam", "^1.0"));
    let candidate = AppInterfaces::new("c", v("1.0.0")).import(import("c", "a.seam", "^1.0"));
    let problems = resolve(&[a, b], &candidate).unwrap_err();
    assert!(problems.has(codes::CYCLE), "{problems}");
}

#[test]
fn capability_escalation_is_refused() {
    // The host grants only read on Customer; the extension also declares write on Sales Invoice.
    let mut candidate = extension();
    candidate.declared = caps(&["Customer"], &["Sales Invoice"]);
    let problems = resolve(&[host_app()], &candidate).unwrap_err();
    assert!(problems.has(codes::CAPABILITY_ESCALATION), "{problems}");
    assert!(
        problems.to_string().contains("write Sales Invoice"),
        "{problems}"
    );
}

#[test]
fn a_grant_the_provider_does_not_hold_is_refused() {
    // The host grants read on Customer but never declares it.
    let host = AppInterfaces::new("host", v("1.0.0")).export(
        export("host", "pricing.adjust", "1.0.0")
            .grant(Capability::ReadDocType("Customer".to_owned())),
    );
    let problems = resolve(&[host], &extension()).unwrap_err();
    assert!(problems.has(codes::GRANT_NOT_HELD), "{problems}");
}

#[test]
fn a_duplicate_export_is_refused() {
    let mut other = host_app();
    other.app = "host_b".to_owned();
    other.exports[0].app = "host_b".to_owned();
    let problems = resolve(&[host_app(), other], &extension()).unwrap_err();
    assert!(problems.has(codes::DUPLICATE_EXPORT), "{problems}");
}

#[test]
fn installing_an_already_installed_app_is_refused() {
    let problems = resolve(&[host_app()], &host_app()).unwrap_err();
    assert!(problems.has(codes::DUPLICATE_APP), "{problems}");
}

#[test]
fn blast_radius_lists_the_apps_that_import_the_candidate() {
    // `report` (already installed) imports the candidate's interface, so installing the
    // candidate reaches it.
    let report = AppInterfaces::new("report", v("1.0.0"))
        .import(import("report", "pricing.adjust", "^1.0"))
        .declared(caps(&["Customer"], &[]));
    let candidate = AppInterfaces::new("host", v("1.0.0"))
        .declared(caps(&["Customer"], &[]))
        .export(
            export("host", "pricing.adjust", "1.0.0")
                .grant(Capability::ReadDocType("Customer".to_owned())),
        );
    let resolution = resolve(&[report], &candidate).unwrap();
    assert_eq!(resolution.blast_radius, vec!["report".to_owned()]);
}

#[test]
fn report_is_deterministic() {
    let first = resolve(&[host_app()], &extension()).unwrap();
    let second = resolve(&[host_app()], &extension()).unwrap();
    assert_eq!(first, second);
}
