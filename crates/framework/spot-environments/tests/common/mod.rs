//! Fixtures shared by the environment tests: three tenants, a set, a signed publisher and a
//! source bundle whose fixtures use `${env.NAME}` placeholders.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use spot_bundle_signing::{
    KeyPair, PublisherCertificate, RevocationBody, RevocationList, SignedCertificate, TrustAnchor,
    TrustedRoot, generate_keypair, issue_certificate, issue_revocation_list,
};
use spot_custom_bundle::{Bundle, CaptureOptions, FakeSite, capture};
use spot_environments::{
    AppRef, Environment, EnvironmentSet, Policy, Role, SecretRef, TransportPackage, Variables,
    transport,
};
use spot_tenancy::{
    ClusterId, DatabaseId, Edition, Placement, Region, Tenant, TenantFlags, TenantId, TenantName,
    TenantState,
};

/// A stable tenant id for member `number`.
pub fn tenant_id(number: u8) -> TenantId {
    TenantId::new_v7(1_760_000_000_000 + u64::from(number), [number; 10])
}

/// An active tenant in `region`.
pub fn tenant(number: u8, name: &str, region: &str) -> Tenant {
    let id = tenant_id(number);
    Tenant {
        id,
        name: TenantName::new(name).unwrap(),
        region: Region::new(region).unwrap(),
        edition: Edition::new("standard").unwrap(),
        state: TenantState::Active,
        placement: Placement {
            cluster: ClusterId::new("cluster-a").unwrap(),
            database: DatabaseId::new(format!("database-{number}")).unwrap(),
            schema: id.schema_name(),
        },
        created: 1_760_000_000_000,
        flags: TenantFlags::NONE,
        version: 1,
        retention: None,
        name_reserved_until: None,
    }
}

/// The one app every environment runs.
pub fn apps() -> Vec<AppRef> {
    vec![AppRef::new("crm", "1.0.0").unwrap()]
}

/// The three-member ladder over the crm app in `eu-west`.
pub fn set(policy: Policy) -> EnvironmentSet {
    EnvironmentSet::new(
        "acme",
        Region::new("eu-west").unwrap(),
        apps(),
        vec![
            Environment {
                tenant: tenant_id(1),
                role: Role::Dev,
                order: 10,
            },
            Environment {
                tenant: tenant_id(2),
                role: Role::Test,
                order: 20,
            },
            Environment {
                tenant: tenant_id(3),
                role: Role::Prod,
                order: 30,
            },
        ],
        policy,
    )
    .unwrap()
}

/// The member for a role.
pub fn member(set: &EnvironmentSet, role: Role) -> &Environment {
    set.member_for_role(role).unwrap()
}

/// The root that issues the publisher certificate and the revocation list.
pub fn root() -> KeyPair {
    generate_keypair(&[1_u8; 32])
}

/// The publisher's key.
pub fn publisher() -> KeyPair {
    generate_keypair(&[2_u8; 32])
}

/// The publisher's certificate, issued by [`root`] and valid over [100, 1000).
pub fn certificate() -> SignedCertificate {
    issue_certificate(
        PublisherCertificate {
            publisher_id: "acme-software".to_owned(),
            display_name: "Acme Software".to_owned(),
            key: publisher().public(),
            valid_from: 100,
            valid_to: 1000,
            issued_by: root().public().key_id(),
            serial: 7,
        },
        &root(),
    )
}

/// The store trusts [`root`].
pub fn anchor() -> TrustAnchor {
    TrustAnchor::new(vec![TrustedRoot::new(root().public(), 0, None)])
}

/// An empty revocation list, generation 1, signed by [`root`].
pub fn revocations() -> RevocationList {
    issue_revocation_list(
        RevocationBody {
            generation: 1,
            issued_at: 0,
            issued_by: root().public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &root(),
    )
}

/// An empty target site on the same app set as the source.
pub fn target_site() -> FakeSite {
    FakeSite::new().with_app("crm", semver::Version::new(1, 0, 0))
}

/// The source site: one fixture whose fields name the environment's values, and one rule.
pub fn source_site() -> FakeSite {
    target_site()
        .with_fixtures(
            "invoice",
            vec![json!({
                "name": "invoice-one",
                "endpoint": "${env.API_URL}",
                "token": "${env.TOKEN}",
            })],
        )
        .with_rule("positive_qty", "doc.qty > 0")
}

/// The bundle captured from the source site.
pub fn source_bundle() -> Bundle {
    capture(
        &source_site(),
        &CaptureOptions::new(
            "acme-customisations",
            "1.0.0",
            "glm",
            "2026-10-19T08:00:00Z",
        ),
    )
    .unwrap()
}

/// The variables for one environment; the endpoint differs and the secret stays a reference.
pub fn variables(environment: &str) -> Variables {
    let mut variables = Variables::new();
    variables
        .set_text("API_URL", format!("https://{environment}.acme.test"))
        .unwrap();
    variables
        .set_secret(
            "TOKEN",
            SecretRef::new(format!("{environment}-token")).unwrap(),
        )
        .unwrap();
    variables
}

/// A package of the source bundle from `from` to `to`.
pub fn package(set: &EnvironmentSet, from: Role, to: Role) -> TransportPackage {
    let bundle = source_bundle();
    let certificate = certificate();
    let key = publisher();
    transport(
        &bundle,
        set,
        member(set, from),
        member(set, to),
        "glm",
        &spot_environments::Signer {
            certificate: &certificate,
            key: &key,
        },
        150,
    )
    .unwrap()
}
