//! Runtime linking: the links, the pool, and caller-attributed fuel.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use semver::{Version, VersionReq};
use spot_app_compose::interface::{
    AppInterfaces, Capability, ExportedInterface, ImportedInterface,
};
use spot_app_compose::link::{ComposedInstance, PoolPlan};
use spot_app_compose::{codes, resolve};
use spot_manifest::{Capabilities, DocTypeAccess, FileAccess};
use spot_wasm::Limits;

fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

fn read_customer() -> Capabilities {
    Capabilities {
        network: Vec::new(),
        email: false,
        files: FileAccess::None,
        doctypes: DocTypeAccess {
            read: vec!["Customer".to_owned()],
            write: Vec::new(),
        },
    }
}

fn host() -> AppInterfaces {
    AppInterfaces::new("host", v("1.0.0"))
        .declared(read_customer())
        .export(
            ExportedInterface::new("host", "pricing.adjust", v("1.0.0"))
                .grant(Capability::ReadDocType("Customer".to_owned())),
        )
}

fn extension() -> AppInterfaces {
    AppInterfaces::new("tax_extension", v("1.0.0")).import(ImportedInterface::new(
        "tax_extension",
        "pricing.adjust",
        VersionReq::parse("^1.0").unwrap(),
    ))
}

fn instance(fuel: &[(&str, u64)]) -> ComposedInstance {
    let resolution = resolve(&[host()], &extension()).unwrap();
    let apps = resolution.order.clone();
    let pool = PoolPlan::even(&apps, 8);
    let fuel_by_app: BTreeMap<String, u64> = fuel
        .iter()
        .map(|(app, units)| ((*app).to_owned(), *units))
        .collect();
    ComposedInstance::from_resolution(&resolution, pool, Limits::default(), fuel_by_app)
}

#[test]
fn links_carry_the_importer_provider_and_grants() {
    let composed = instance(&[]);
    assert_eq!(composed.links.len(), 1);
    let link = &composed.links[0];
    assert_eq!(link.importer, "tax_extension");
    assert_eq!(link.provider, "host");
    assert_eq!(link.version, v("1.0.0"));
    assert_eq!(
        link.grants,
        vec![Capability::ReadDocType("Customer".to_owned())]
    );
    assert_eq!(composed.pool.max_concurrent_calls, 8);
    assert_eq!(composed.pool.slots_for("host"), 4);
    assert_eq!(composed.pool.slots_for("tax_extension"), 4);
}

#[test]
fn a_call_chain_starts_at_the_entry_and_reaches_the_provider() {
    let composed = instance(&[]);
    let chain = composed
        .call_chain("tax_extension", "pricing.adjust")
        .unwrap();
    let apps: Vec<&str> = chain.steps.iter().map(|s| s.app.as_str()).collect();
    assert_eq!(apps, vec!["tax_extension", "host"]);
    assert_eq!(chain.caller(), "tax_extension");
}

#[test]
fn an_unknown_interface_is_reported() {
    let composed = instance(&[]);
    let problems = composed
        .call_chain("tax_extension", "other.seam")
        .unwrap_err();
    assert!(problems.has(codes::UNKNOWN_INTERFACE), "{problems}");
}

#[test]
fn an_extension_cannot_make_the_provider_run_unbounded() {
    // The provider has a generous budget; the extension's is small. The effective budget is the
    // extension's, and the fuel is charged to the extension, never to the provider.
    let composed = instance(&[("tax_extension", 1_000), ("host", 10_000_000)]);
    let chain = composed
        .call_chain("tax_extension", "pricing.adjust")
        .unwrap();

    let charge = composed.attribute_fuel(&chain, 800).unwrap();
    assert_eq!(charge.caller, "tax_extension");
    assert_eq!(charge.limit, 1_000);

    let problems = composed.attribute_fuel(&chain, 5_000).unwrap_err();
    assert!(problems.has(codes::FUEL_EXHAUSTED), "{problems}");
    assert!(problems.to_string().contains("tax_extension"), "{problems}");
}

#[test]
fn the_tightest_budget_on_the_chain_wins() {
    // A provider that refuses to run long even when called caps the whole call.
    let composed = instance(&[("tax_extension", 10_000_000), ("host", 100)]);
    let chain = composed
        .call_chain("tax_extension", "pricing.adjust")
        .unwrap();
    let problems = composed.attribute_fuel(&chain, 500).unwrap_err();
    assert!(problems.has(codes::FUEL_EXHAUSTED), "{problems}");
    // Still charged to the outermost caller.
    assert!(problems.to_string().contains("tax_extension"), "{problems}");
}
