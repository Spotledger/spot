//! Uninstall and upgrade safety.
#![allow(clippy::unwrap_used)]

use semver::{Version, VersionReq};
use spot_app_compose::interface::{AppInterfaces, ExportedInterface, ImportedInterface};
use spot_app_compose::{codes, uninstall, upgrade};

fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

fn req(text: &str) -> VersionReq {
    VersionReq::parse(text).unwrap()
}

fn host(version: &str, interface_version: &str) -> AppInterfaces {
    AppInterfaces::new("host", v(version)).export(ExportedInterface::new(
        "host",
        "pricing.adjust",
        v(interface_version),
    ))
}

fn extension() -> AppInterfaces {
    AppInterfaces::new("tax_extension", v("1.0.0")).import(ImportedInterface::new(
        "tax_extension",
        "pricing.adjust",
        req("^1.0"),
    ))
}

#[test]
fn uninstalling_a_provider_with_dependents_is_refused() {
    let problems = uninstall(&[host("1.0.0", "1.0.0"), extension()], "host", None).unwrap_err();
    assert!(problems.has(codes::UNINSTALL_HAS_DEPENDENTS), "{problems}");
    assert!(problems.to_string().contains("tax_extension"), "{problems}");
}

#[test]
fn a_satisfying_replacement_allows_the_uninstall() {
    let replacement = AppInterfaces::new("host_next", v("1.0.0")).export(ExportedInterface::new(
        "host_next",
        "pricing.adjust",
        v("1.2.0"),
    ));
    let plan = uninstall(
        &[host("1.0.0", "1.0.0"), extension()],
        "host",
        Some(&replacement),
    )
    .unwrap();
    assert_eq!(plan.dependents, vec!["tax_extension".to_owned()]);
    assert_eq!(plan.replacement, Some("host_next".to_owned()));
}

#[test]
fn an_incomplete_replacement_is_refused() {
    let replacement = AppInterfaces::new("host_next", v("2.0.0")).export(ExportedInterface::new(
        "host_next",
        "pricing.adjust",
        v("2.0.0"),
    ));
    let problems = uninstall(
        &[host("1.0.0", "1.0.0"), extension()],
        "host",
        Some(&replacement),
    )
    .unwrap_err();
    assert!(problems.has(codes::REPLACEMENT_INCOMPLETE), "{problems}");
}

#[test]
fn uninstalling_an_absent_app_is_refused() {
    let problems = uninstall(&[extension()], "host", None).unwrap_err();
    assert!(problems.has(codes::UNKNOWN_APP), "{problems}");
}

#[test]
fn an_upgrade_lists_the_impacted_dependents() {
    let new_exports = vec![ExportedInterface::new("host", "pricing.adjust", v("2.0.0"))];
    let plan = upgrade(&[host("1.0.0", "1.0.0"), extension()], "host", &new_exports).unwrap();
    assert!(!plan.is_safe());
    assert_eq!(plan.blast_radius, vec!["tax_extension".to_owned()]);
    assert_eq!(plan.impacted.len(), 1);
    assert_eq!(plan.impacted[0].app, "tax_extension");
    assert_eq!(plan.impacted[0].needed, req("^1.0"));
    assert_eq!(plan.impacted[0].offered, Some(v("2.0.0")));
}

#[test]
fn a_compatible_upgrade_impacts_nobody() {
    let new_exports = vec![ExportedInterface::new("host", "pricing.adjust", v("1.1.0"))];
    let plan = upgrade(&[host("1.0.0", "1.0.0"), extension()], "host", &new_exports).unwrap();
    assert!(plan.is_safe());
    assert_eq!(plan.blast_radius, vec!["tax_extension".to_owned()]);
}

#[test]
fn upgrading_an_absent_app_is_refused() {
    let problems = upgrade(&[extension()], "host", &[]).unwrap_err();
    assert!(problems.has(codes::UNKNOWN_APP), "{problems}");
}
