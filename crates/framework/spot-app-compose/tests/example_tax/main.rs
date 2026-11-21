//! The worked example: a regional tax app extends a host app through a WIT import.
//!
//! The host exports the seam interface `pricing.adjust` v1; the extension imports v1 and
//! registers a step. This test walks the whole story: resolution, linking, a failing version
//! range, and a capability escalation the provider does not grant.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use semver::{Version, VersionReq};
use spot_app_api::{
    AppResult, BoxFuture, Controller, HookContext, HookPoint, OverrideKind, Phase, StepDecl,
    StepName, StepOverride, Target,
};
use spot_app_compose::interface::{
    AppInterfaces, Capability, ExportedInterface, FunctionSig, ImportedInterface, TypeRef,
};
use spot_app_compose::link::{ComposedInstance, PoolPlan};
use spot_app_compose::seams::{StepContribution, build_registry};
use spot_app_compose::{codes, resolve};
use spot_manifest::{Capabilities, DocTypeAccess, FileAccess};
use spot_wasm::Limits;

fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

fn req(text: &str) -> VersionReq {
    VersionReq::parse(text).unwrap()
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

/// The host app: it prices documents and lets an extension adjust the result.
fn accounts_host() -> AppInterfaces {
    AppInterfaces::new("erp_accounts", v("1.0.0"))
        .declared(read_customer())
        .export(
            ExportedInterface::new("erp_accounts", "pricing.adjust", v("1.0.0"))
                .function(
                    FunctionSig::new("adjust")
                        .param("amount", TypeRef::S64)
                        .result(TypeRef::S64),
                )
                .grant(Capability::ReadDocType("Customer".to_owned())),
        )
}

/// The regional tax app: it imports the host's seam interface at v1 and declares only what the
/// host grants it.
fn regional_tax() -> AppInterfaces {
    AppInterfaces::new("regional_tax", v("1.0.0"))
        .declared(read_customer())
        .import(ImportedInterface::new(
            "regional_tax",
            "pricing.adjust",
            req("^1.0"),
        ))
}

#[test]
fn the_tax_app_resolves_against_the_host() {
    let resolution = resolve(&[accounts_host()], &regional_tax()).unwrap();
    assert_eq!(
        resolution.order,
        vec!["erp_accounts".to_owned(), "regional_tax".to_owned()]
    );
    assert_eq!(resolution.bindings.len(), 1);
    assert_eq!(resolution.bindings[0].provider, "erp_accounts");
    assert_eq!(resolution.bindings[0].version, v("1.0.0"));
    assert_eq!(
        resolution.obtained_by("regional_tax"),
        &[Capability::ReadDocType("Customer".to_owned())]
    );
}

#[test]
fn the_tax_app_links_to_the_host_and_is_charged_for_its_calls() {
    let resolution = resolve(&[accounts_host()], &regional_tax()).unwrap();
    let pool = PoolPlan::even(&resolution.order, 8);
    let fuel = BTreeMap::from([
        ("regional_tax".to_owned(), 2_000),
        ("erp_accounts".to_owned(), 10_000_000),
    ]);
    let composed = ComposedInstance::from_resolution(&resolution, pool, Limits::default(), fuel);

    assert_eq!(composed.links.len(), 1);
    assert_eq!(composed.links[0].provider, "erp_accounts");

    let chain = composed
        .call_chain("regional_tax", "pricing.adjust")
        .unwrap();
    assert_eq!(chain.caller(), "regional_tax");
    let charge = composed.attribute_fuel(&chain, 1_500).unwrap();
    assert_eq!(charge.caller, "regional_tax");
    assert_eq!(charge.limit, 2_000);

    // The host cannot widen the extension's call: the extension's budget caps it.
    assert!(composed.attribute_fuel(&chain, 9_000).is_err());
}

#[test]
fn a_failing_version_range_is_refused() {
    let host_v2 = AppInterfaces::new("erp_accounts", v("2.0.0"))
        .declared(read_customer())
        .export(
            ExportedInterface::new("erp_accounts", "pricing.adjust", v("2.0.0"))
                .grant(Capability::ReadDocType("Customer".to_owned())),
        );
    let problems = resolve(&[host_v2], &regional_tax()).unwrap_err();
    assert!(problems.has(codes::VERSION_CONFLICT), "{problems}");
    assert!(problems.to_string().contains("2.0.0"), "{problems}");
}

#[test]
fn a_capability_the_host_does_not_grant_is_refused() {
    // The extension tries to write Sales Invoice, which the host never grants to importers.
    let mut escalating = regional_tax();
    escalating
        .declared
        .doctypes
        .write
        .push("Sales Invoice".to_owned());
    let problems = resolve(&[accounts_host()], &escalating).unwrap_err();
    assert!(problems.has(codes::CAPABILITY_ESCALATION), "{problems}");
    assert!(
        problems.to_string().contains("write Sales Invoice"),
        "{problems}"
    );
}

// --- the extension registers a step through the shared registry -------------------------------

struct TaxStep {
    doctype: String,
    step: StepName,
}

impl Controller for TaxStep {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: self.step.clone(),
            point: HookPoint::of(Phase::Validate, Target::Save),
            priority: 10,
            budget: Duration::from_millis(50),
        }]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move { Ok(()) })
    }
}

fn tax_step() -> Arc<dyn Controller> {
    Arc::new(TaxStep {
        doctype: "Sales Invoice".to_owned(),
        step: StepName::new("regional_tax:apply_tax").unwrap(),
    })
}

#[test]
fn the_tax_step_is_registered_through_the_shared_registry() {
    let host = StepContribution::new(
        "erp_accounts",
        Arc::new(TaxStep {
            doctype: "Sales Invoice".to_owned(),
            step: StepName::new("erp_accounts:adjust").unwrap(),
        }),
    );
    let extension = StepContribution::new("regional_tax", tax_step()).override_step(StepOverride {
        target: StepName::new("erp_accounts:adjust").unwrap(),
        kind: OverrideKind::After,
        by: Some(StepName::new("regional_tax:apply_tax").unwrap()),
        reason: "regional tax on the adjusted price".to_owned(),
    });
    let registry = build_registry(&[host, extension]).unwrap();
    let plan = registry.plan(
        "Sales Invoice",
        &HookPoint::of(Phase::Validate, Target::Save),
    );
    let steps: Vec<&str> = plan.iter().map(|p| p.step.as_str()).collect();
    assert_eq!(steps, vec!["erp_accounts:adjust", "regional_tax:apply_tax"]);
}
