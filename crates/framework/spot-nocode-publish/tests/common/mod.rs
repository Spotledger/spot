//! Shared fixtures for the no-code publish tests: a site with every customisation kind, a
//! designer's draft, a publisher signer, an in-memory store registry and an installer harness.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use semver::{Version, VersionReq};
use spot_bundle::{Bundle, Digest, HostInfo, Limits};
use spot_bundle_signing::{
    BundleFacts, BundleSignature, CertificateChain, KeyPair, PublisherCertificate, RevocationBody,
    RevocationList, SignedCertificate, TrustAnchor, TrustedRoot, generate_keypair,
    issue_certificate, issue_revocation_list, sign_bundle,
};
use spot_core::{ManualClock, SiteId};
use spot_custom_bundle::{Bundle as CustomBundle, FakeSite, SectionRegistry};
use spot_installer::Executor;
use spot_installer::testing::{
    FakeComponents, FakeFixtures, FakePatches, FakeRegistry, FakeSnapshots, Schedule,
};
use spot_meta::{Definition, FieldSpec, FieldType, Property};
use spot_nocode_model::Draft;
use spot_nocode_publish::{
    AppDraft, Dependency, ItemRef, PLATFORM_WIT_WORLD, Signer, SigningMaterial, StoreRegistry,
};
use spot_registry::{
    BlobValidation, MemoryAuditSink, MemoryBlobStore, MemoryIndexStore, RegistryPolicy,
    RegistryTrust,
};
use spot_views::{ViewDef, ViewLayer};

/// The one capture/creation instant the tests state, never the wall clock.
pub const CREATED_AT: &str = "2026-10-01T09:00:00Z";
/// The registry/review time (UTC seconds).
pub const NOW: i64 = 500;
/// The publisher id every fixture uses.
pub const PUBLISHER: &str = "acme";

/// The customerDocType an installed app owns.
pub fn customer_definition() -> Definition {
    let mut definition = Definition::new("customer");
    definition.app = Some("crm".to_owned());
    definition.label = Some("Customer".to_owned());
    definition.fields = vec![FieldSpec::new("name", FieldType::Text)];
    definition
}

/// The app's base form view for `customer`.
pub fn base_view() -> ViewDef {
    ViewDef::parse(
        "base.toml",
        "doctype = \"customer\"\nkind = \"form\"\n\n[[sections]]\nid = \"main\"\n\n\
         [[sections.items]]\ntype = \"field\"\nfield = \"name\"\n",
    )
    .expect("the base view parses")
}

/// A site layer that moves `name` into a section of its own.
pub fn site_layer() -> ViewLayer {
    ViewLayer::parse(
        "site.toml",
        "doctype = \"customer\"\nkind = \"site\"\n\n[[ops]]\nop = \"move_field\"\n\
         field = \"name\"\n",
    )
    .expect("the site layer parses")
}

/// The site the app is designed on: an installed `crm` app with a customer DocType, its base
/// view, one custom field, a view layer, a script, a rule and fixture rows.
///
/// It also carries fixture rows for a DocType the draft does not declare (`sales_invoice`), so
/// the package tests can prove those rows never travel.
pub fn fixture_site() -> FakeSite {
    let mut tier = FieldSpec::new("custom_tier", FieldType::Select);
    tier.options = Some("bronze\nsilver\ngold".to_owned());
    tier.label = Some("Tier".to_owned());
    let mut site = FakeSite::new()
        .with_app("crm", Version::new(1, 0, 0))
        .with_definition(customer_definition())
        .with_base_view(base_view())
        .with_custom_field("customer", tier, None)
        .with_override(
            "customer",
            Some("custom_tier"),
            Property::Label("Customer tier".to_owned()),
        )
        .with_layer(site_layer())
        .with_script(
            "validate",
            "crm",
            "customer.validate",
            "export function validate(): boolean { return true; }\n",
        )
        .with_rule("tier_positive", "doc.custom_tier >= 0")
        .with_fixtures(
            "customer",
            vec![
                serde_json::json!({"name": "Acme"}),
                serde_json::json!({"name": "Globex"}),
            ],
        );
    site = site.with_fixtures(
        "sales_invoice",
        vec![serde_json::json!({"name": "not-declared"})],
    );
    site
}

/// A newly designed DocType carried by the draft.
pub fn designed_draft() -> Draft {
    let mut draft = Draft::new("design-1", PUBLISHER, "asset", "Asset");
    draft
        .doctype_def
        .fields
        .push(FieldSpec::new("serial_no", FieldType::Text));
    draft
}

/// The full draft: every item kind plus one designed DocType.
pub fn full_draft() -> AppDraft {
    base_draft().draft(designed_draft())
}

/// The install-test draft: the site customisations only, no designed DocType.
pub fn install_draft() -> AppDraft {
    base_draft()
}

/// The draft everything is built from.
fn base_draft() -> AppDraft {
    AppDraft::new("no_code_crm", Version::new(1, 0, 0), PUBLISHER)
        .title("No-code CRM")
        .licence("SPDX:MIT")
        .site("tenant-a")
        .created_at(CREATED_AT)
        .requires_host(VersionReq::parse(">=0.1, <0.2").expect("a range"))
        .depends_on(Dependency::new(
            "crm",
            VersionReq::parse("^1").expect("a range"),
        ))
        .item(ItemRef::DocType {
            name: "customer".to_owned(),
        })
        .item(ItemRef::CustomField {
            doctype: "customer".to_owned(),
            field: "custom_tier".to_owned(),
        })
        .item(ItemRef::View {
            doctype: "customer".to_owned(),
        })
        .item(ItemRef::Script {
            name: "validate".to_owned(),
        })
        .item(ItemRef::Rule {
            name: "tier_positive".to_owned(),
        })
        .item(ItemRef::Fixture {
            doctype: "customer".to_owned(),
        })
        .item(ItemRef::Process {
            name: "onboarding".to_owned(),
            source: "[process]\nname = \"onboarding\"\n".to_owned(),
        })
        .item(ItemRef::Report {
            name: "tiers".to_owned(),
            source: "[report]\nname = \"tiers\"\n".to_owned(),
        })
}

/// A draft for the same app at another version (same content, a new version).
pub fn draft_at(version: Version) -> AppDraft {
    let mut draft = base_draft();
    draft.version = version;
    draft
}

/// The host the app is packaged against.
pub fn bundle_host() -> HostInfo {
    HostInfo {
        version: Version::new(0, 1, 0),
    }
}

/// The installer's host.
pub fn installer_host() -> spot_installer::Host {
    spot_installer::Host::new(Version::new(0, 1, 0), PLATFORM_WIT_WORLD)
}

/// A catalogue candidate for the `crm` app the draft depends on.
pub fn crm_candidate() -> spot_installer::Candidate {
    let manifest = spot_manifest::parse(
        "[app]\nname = \"crm\"\nversion = \"1.0.0\"\ntitle = \"CRM\"\n",
        "manifest.toml",
    )
    .expect("the crm manifest parses");
    let digest: Digest = "11".repeat(32).parse().expect("64 hex characters");
    spot_installer::Candidate::new(
        manifest,
        digest,
        spot_installer::Verified::trusted("crm-publisher", 1),
        PLATFORM_WIT_WORLD,
    )
}

/// The options another tenant's capture uses; they match what packaging used.
pub fn capture_options() -> spot_custom_bundle::CaptureOptions {
    spot_custom_bundle::CaptureOptions::new("no_code_crm", "1.0.0", PUBLISHER, CREATED_AT)
}

/// Opens the customisation bundle a package carries.
pub fn open_customisation(bytes: &[u8]) -> CustomBundle {
    CustomBundle::open(
        bytes,
        &spot_custom_bundle::Limits::default(),
        &SectionRegistry::new(),
        false,
    )
    .unwrap_or_else(|errors| panic!("the customisation bundle opens: {errors:?}"))
}

// ---------------------------------------------------------------------------------------------
// Signing material
// ---------------------------------------------------------------------------------------------

fn root_key() -> KeyPair {
    generate_keypair(&[1; 32])
}

fn publisher_key() -> KeyPair {
    generate_keypair(&[2; 32])
}

fn certificate_valid_to(valid_to: i64) -> SignedCertificate {
    issue_certificate(
        PublisherCertificate {
            publisher_id: PUBLISHER.to_owned(),
            display_name: "Acme Software".to_owned(),
            key: publisher_key().public(),
            valid_from: 100,
            valid_to,
            issued_by: root_key().public().key_id(),
            serial: 7,
        },
        &root_key(),
    )
}

/// The revocation list the store and the signer both hold.
pub fn revocations() -> RevocationList {
    issue_revocation_list(
        RevocationBody {
            generation: 1,
            issued_at: 0,
            issued_by: root_key().public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &root_key(),
    )
}

/// A publisher signer whose certificate is valid at [`NOW`].
pub struct TestSigner {
    /// The certificate the signer presents.
    pub certificate: SignedCertificate,
    /// The signer's key.
    pub key: KeyPair,
}

impl Signer for TestSigner {
    fn publisher_id(&self) -> &str {
        PUBLISHER
    }

    fn sign(
        &self,
        digest: &Digest,
        app_id: &str,
        app_version: &str,
        signed_at: i64,
    ) -> Result<SigningMaterial, String> {
        let signature: BundleSignature = sign_bundle(
            digest,
            app_id,
            app_version,
            &self.certificate,
            &self.key,
            signed_at,
        )
        .map_err(|error| format!("{}: {error}", error.code()))?;
        Ok(SigningMaterial {
            signature,
            chain: CertificateChain::new(self.certificate.clone()),
            anchor: TrustAnchor::new(vec![TrustedRoot::new(root_key().public(), 0, None)]),
            revocations: revocations(),
        })
    }
}

/// The normal publisher.
pub fn signer() -> TestSigner {
    TestSigner {
        certificate: certificate_valid_to(2000),
        key: publisher_key(),
    }
}

/// A publisher whose certificate expired before [`NOW`].
pub fn expired_signer() -> TestSigner {
    TestSigner {
        certificate: certificate_valid_to(200),
        key: publisher_key(),
    }
}

// ---------------------------------------------------------------------------------------------
// The store registry
// ---------------------------------------------------------------------------------------------

/// An in-memory store registry behind the crate's object-safe seam.
pub struct Setup {
    /// The adapter the crate publishes through.
    pub registry: StoreRegistry<MemoryIndexStore, MemoryBlobStore>,
    /// The raw registry, for assertions about releases and blobs.
    pub store: Arc<spot_registry::Registry<MemoryIndexStore, MemoryBlobStore>>,
}

/// A fresh in-memory registry trusting the test root.
pub fn setup() -> Setup {
    let policy = RegistryPolicy::default();
    let index = Arc::new(MemoryIndexStore::default());
    let blobs = Arc::new(MemoryBlobStore::new(BlobValidation {
        limits: policy.limits,
        host: policy.host.clone(),
    }));
    let trust = Arc::new(
        RegistryTrust::new(
            TrustAnchor::new(vec![TrustedRoot::new(root_key().public(), 0, None)]),
            revocations(),
            NOW,
        )
        .expect("the test trust material is valid"),
    );
    let audit = Arc::new(MemoryAuditSink::default());
    let store = Arc::new(spot_registry::Registry::new(
        index, blobs, policy, trust, audit,
    ));
    Setup {
        registry: StoreRegistry::new(store.clone()),
        store,
    }
}

// ---------------------------------------------------------------------------------------------
// The installer harness (the same fakes the installer's own tests use)
// ---------------------------------------------------------------------------------------------

/// The executor and its fakes, wired to an in-process coordinator and a manual clock.
pub struct Harness {
    /// The registry fake.
    pub registry: Arc<FakeRegistry>,
    /// The patch fake.
    pub patches: Arc<FakePatches>,
    /// The component fake.
    pub components: Arc<FakeComponents>,
    /// The fixture fake.
    pub fixtures: Arc<FakeFixtures>,
    /// The snapshot fake.
    pub snapshots: Arc<FakeSnapshots>,
    /// The failure schedule.
    pub schedule: Schedule,
    /// The executor.
    pub executor: Executor,
}

impl Harness {
    /// Builds a harness alone on its coordinator.
    pub async fn new() -> Self {
        use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId};
        let schedule = Schedule::new();
        let registry = Arc::new(FakeRegistry::new(schedule.clone()));
        let patches = Arc::new(FakePatches::new(schedule.clone()));
        let components = Arc::new(FakeComponents::new(schedule.clone()));
        let fixtures = Arc::new(FakeFixtures::new(schedule.clone()));
        let snapshots = Arc::new(FakeSnapshots::new(schedule.clone()));
        let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
        let coordinator = InProcessCoordinator::start(
            Arc::new(InMemoryPresence::new()),
            NodeId::new("nocode-publish-test").expect("a constant node id is valid"),
            clock.clone(),
        )
        .await
        .expect("the in-process coordinator starts alone");
        let executor = Executor::new(
            registry.clone(),
            patches.clone(),
            components.clone(),
            fixtures.clone(),
            snapshots.clone(),
            coordinator,
            clock,
        );
        Self {
            registry,
            patches,
            components,
            fixtures,
            snapshots,
            schedule,
            executor,
        }
    }

    /// A site id for a test.
    pub fn site(name: &str) -> SiteId {
        SiteId::new(name).expect("a test site id is valid")
    }
}

/// The tenant the app is installed on, with the same base metadata as the design site.
///
/// `crm` is already installed: the captured customisations target its `customer` DocType, so
/// the target must report the same app the bundle was captured against.
pub fn empty_tenant() -> FakeSite {
    FakeSite::new()
        .with_app("crm", Version::new(1, 0, 0))
        .with_definition(customer_definition())
        .with_base_view(base_view())
}

/// Opens a bundle the installer's way.
pub fn open_bundle(bytes: &[u8]) -> Bundle {
    Bundle::open(bytes, &Limits::default(), &bundle_host())
        .unwrap_or_else(|errors| panic!("the bundle opens: {errors:?}"))
}

/// The facts a signature covers, for a test that signs by hand.
pub fn facts<'a>(digest: &'a Digest, app: &'a str, version: &'a str) -> BundleFacts<'a> {
    BundleFacts {
        digest,
        app_id: app,
        version,
    }
}
