//! End-to-end acceptance: package from tenant A, publish, resolve, install on tenant B with
//! the real installer, and uninstall without leaving customisations behind. A failed upgrade
//! is rolled back by the installer and never touches the tenant's customisations.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use semver::Version;
use spot_custom_bundle::{Limits, SectionRegistry, capture};
use spot_installer::testing::FailKind;
use spot_installer::{Catalogue, Options, Request, SiteState};
use spot_nocode_publish::{
    PublishOptions, Registry, install, open, package, publish, sign, uninstall,
};

/// Packages, signs, publishes and resolves a draft at `version`, returning the opened bundle
/// and its fetch evidence.
fn published(version: Version) -> (spot_nocode_publish::OpenedApp, spot_installer::Verified) {
    let site = fixture_site();
    let source = package(&draft_at(version), &site).expect("package");
    let signed = sign(source, &signer(), NOW).expect("sign");
    let setup = setup();
    publish(
        &signed,
        &setup.registry,
        &PublishOptions::store(NOW, CREATED_AT),
    )
    .expect("publish");
    let fetched = setup
        .registry
        .fetch("no_code_crm", signed.version(), NOW)
        .expect("resolve");
    let opened = open(&fetched.bytes, &bundle_host()).expect("open");
    (opened, fetched.installer_verified())
}

#[tokio::test]
async fn a_no_code_app_installs_on_another_tenant_and_uninstalls_cleanly() {
    // The review pipeline builds its own current-thread runtime for the resource profile, so
    // the synchronous publish runs on a blocking thread, not inside the async test runtime.
    let (opened, verified, inner_a) = tokio::task::spawn_blocking(|| {
        let site_a = fixture_site();
        let source = package(&install_draft(), &site_a).expect("package");
        let inner_a = open_customisation(source.customisation());
        let signed = sign(source, &signer(), NOW).expect("sign");
        let setup = setup();
        publish(
            &signed,
            &setup.registry,
            &PublishOptions::store(NOW, CREATED_AT),
        )
        .expect("publish");
        let fetched = setup
            .registry
            .fetch("no_code_crm", &Version::new(1, 0, 0), NOW)
            .expect("resolve");
        let opened = open(&fetched.bytes, &bundle_host()).expect("open the fetched bundle");
        (opened, fetched.installer_verified(), inner_a)
    })
    .await
    .expect("the blocking publish completes");

    let harness = Harness::new().await;
    let site = Harness::site("tenant-b");
    let mut tenant = empty_tenant();
    let installed = SiteState::new();
    let host = installer_host();
    let approval = opened.full_approval();
    let dependencies = [crm_candidate()];
    let request = spot_nocode_publish::InstallRequest::new(
        &opened,
        &installed,
        &host,
        &approval,
        verified,
        &dependencies,
    );
    let outcome = install(
        &harness.executor,
        &site,
        &request,
        &Options::new("install-b"),
        &mut tenant,
    )
    .await
    .expect("the app installs");
    assert!(outcome.is_applied(), "{outcome:?}");
    assert_eq!(
        harness
            .registry
            .state(&site)
            .get("no_code_crm")
            .expect("the app is registered")
            .version,
        Version::new(1, 0, 0)
    );
    assert!(
        harness.components.serving(&site, "no_code_crm").is_some(),
        "the component host loaded the app"
    );

    // Tenant B's captured customisations are the same shared parts as tenant A's: same section
    // files, and the same digest once both carry the same header.
    let captured_b = capture(&tenant, &capture_options()).expect("capture B");
    assert_eq!(
        inner_a.kinds(),
        captured_b.kinds(),
        "the same sections arrive"
    );
    for kind in inner_a.kinds() {
        let a = inner_a.section(kind).expect("A has the section");
        let b = captured_b.section(kind).expect("B has the section");
        assert_eq!(a.files, b.files, "section `{kind}` is byte-identical");
    }
    let regraded = spot_custom_bundle::Bundle::pack(
        inner_a.header().clone(),
        captured_b.sections().clone(),
        &Limits::default(),
    )
    .expect("rebuild B's payload under A's header");
    assert_eq!(
        regraded.digest(),
        inner_a.digest(),
        "the shared customisation digest matches A's"
    );

    // The app's rows travel with the app, and only declared fixtures do.
    let fixtures = captured_b.section("fixtures").expect("fixtures");
    assert!(fixtures.files.contains_key("customer.json"));
    assert!(!fixtures.files.contains_key("sales_invoice.json"));

    // Uninstall removes the app and everything it added to the tenant.
    let installed = harness.registry.state(&site);
    let outcome = uninstall(
        &harness.executor,
        &site,
        &opened,
        &installed,
        &installer_host(),
        &Options::new("uninstall-b"),
        &mut tenant,
    )
    .await
    .expect("the app uninstalls");
    assert!(outcome.is_applied(), "{outcome:?}");
    assert!(
        harness.registry.state(&site).get("no_code_crm").is_none(),
        "the registry row is gone"
    );
    let captured_after = capture(&tenant, &capture_options()).expect("capture after uninstall");
    let base = capture(&empty_tenant(), &capture_options()).expect("capture the base tenant");
    assert_eq!(
        captured_after.sections(),
        base.sections(),
        "the tenant is back to its base customisation state"
    );
    let plan = spot_custom_bundle::plan_apply(&captured_after, &tenant, &SectionRegistry::new())
        .expect("an empty plan");
    assert!(plan.is_empty(), "the tenant is back to its base metadata");
}

#[tokio::test]
async fn a_failed_upgrade_rolls_back_and_keeps_the_tenant_intact() {
    // Install 1.0.0 first. The review runs on a blocking thread (see above).
    let (opened_v1, verified_v1) = tokio::task::spawn_blocking(|| published(Version::new(1, 0, 0)))
        .await
        .expect("the blocking publish completes");
    let harness = Harness::new().await;
    let site = Harness::site("tenant-b");
    let mut tenant = empty_tenant();
    let installed = SiteState::new();
    let host = installer_host();
    let approval = opened_v1.full_approval();
    let dependencies = [crm_candidate()];
    let request = spot_nocode_publish::InstallRequest::new(
        &opened_v1,
        &installed,
        &host,
        &approval,
        verified_v1,
        &dependencies,
    );
    install(
        &harness.executor,
        &site,
        &request,
        &Options::new("install-v1"),
        &mut tenant,
    )
    .await
    .expect("1.0.0 installs");
    let before = capture(&tenant, &capture_options()).expect("capture v1");
    let installed = harness.registry.state(&site);

    // Plan the 1.1.0 upgrade and fail its commit step.
    let (opened_v2, verified_v2) = tokio::task::spawn_blocking(|| published(Version::new(1, 1, 0)))
        .await
        .expect("the blocking publish completes");
    let catalogue = Catalogue::new(vec![opened_v2.candidate(verified_v2)]);
    let plan = spot_installer::plan(
        &installed,
        &catalogue,
        &installer_host(),
        &Request::Upgrade {
            app: "no_code_crm".to_owned(),
            to: Some(Version::new(1, 1, 0)),
        },
        &opened_v2.full_approval(),
    )
    .expect("the upgrade plan builds");
    harness.schedule.fail_at(0, FailKind::Commit);
    let outcome = harness
        .executor
        .run(&site, &plan, plan.hash(), &Options::new("upgrade-v2"))
        .await
        .expect("a step failure is an outcome, not a refusal");
    assert!(outcome.is_rolled_back(), "{outcome:?}");
    assert_eq!(
        harness
            .registry
            .state(&site)
            .get("no_code_crm")
            .expect("the app is still installed")
            .version,
        Version::new(1, 0, 0),
        "the failed upgrade left the old version"
    );
    let after = capture(&tenant, &capture_options()).expect("capture after rollback");
    assert_eq!(
        before.sections(),
        after.sections(),
        "the failed upgrade did not touch the tenant's customisations"
    );
}
