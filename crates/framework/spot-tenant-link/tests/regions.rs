//! Residency: a grant's peer-region requirement is answered through the `RegionOracle`, and a
//! refusal names the `spot-data-policy` clause that said no.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{linked, tenant, topic};
use spot_data_policy::{Clause, Refusal, Region};
use spot_tenant_link::{
    Direction, Error, LinkService, LinkStore, MemoryAuditLog, MemoryLinkStore, MemoryRegionOracle,
    Party, RegionOracle,
};

/// An oracle that refuses every peer with the clause it was built with.
struct Refusing(Clause);

impl RegionOracle for Refusing {
    fn peer_region(&self, _peer: &Party) -> Result<Option<Region>, Refusal> {
        Err(Refusal {
            clause: self.0,
            region: Region::new("eu-west").unwrap(),
        })
    }
}

/// An oracle that knows no peer.
struct Unknown;

impl RegionOracle for Unknown {
    fn peer_region(&self, _peer: &Party) -> Result<Option<Region>, Refusal> {
        Ok(None)
    }
}

fn link_with_requirement(region: &str) -> spot_tenant_link::Link {
    let mut link = linked().a;
    link.grants[0].require_peer_region = Some(Region::new(region).unwrap());
    link
}

#[test]
fn a_grant_without_a_requirement_never_asks_the_oracle() {
    let link = linked().a;
    assert!(
        link.allows_with_region(&topic(), Direction::Send, 2_000, &Unknown)
            .is_ok()
    );
}

#[test]
fn a_grant_requires_the_peer_to_be_in_its_region() {
    let link = link_with_requirement("eu-west");
    let oracle = MemoryRegionOracle::new();
    oracle
        .insert(tenant(2), Region::new("eu-west").unwrap())
        .unwrap();
    assert!(
        link.allows_with_region(&topic(), Direction::Send, 2_000, &oracle)
            .is_ok()
    );

    let elsewhere = MemoryRegionOracle::new();
    elsewhere
        .insert(tenant(2), Region::new("us-east").unwrap())
        .unwrap();
    let error = link
        .allows_with_region(&topic(), Direction::Send, 2_000, &elsewhere)
        .unwrap_err();
    assert!(matches!(error, Error::PeerRegionMismatch { .. }));
    assert!(error.to_string().contains("us-east"));
    assert!(error.to_string().contains("eu-west"));
}

#[test]
fn an_oracle_refusal_keeps_the_policy_clause() {
    let link = link_with_requirement("eu-west");
    let error = link
        .allows_with_region(
            &topic(),
            Direction::Send,
            2_000,
            &Refusing(Clause::ForbiddenRegions),
        )
        .unwrap_err();
    assert_eq!(error.code(), "tenant_link.region_refused");
    assert!(error.to_string().contains("forbidden_regions"));
}

#[test]
fn an_unknown_peer_region_is_refused_not_assumed() {
    let link = link_with_requirement("eu-west");
    let error = link
        .allows_with_region(&topic(), Direction::Send, 2_000, &Unknown)
        .unwrap_err();
    assert!(matches!(error, Error::RegionRefused { .. }));
    assert!(error.to_string().contains("region_map"));
}

#[test]
fn the_use_check_refuses_an_unknown_region_and_audits_it() {
    let store = MemoryLinkStore::new();
    let audit = MemoryAuditLog::new();
    let service = LinkService::new(&store, &audit);
    let link = store.insert(link_with_requirement("eu-west")).unwrap();
    let error = service
        .authorize(
            &link.id,
            &topic(),
            Direction::Send,
            2_000,
            "user:a",
            Some(&Refusing(Clause::EgressRegion)),
        )
        .unwrap_err();
    assert!(error.to_string().contains("egress_region"));
    let events = audit.events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, spot_tenant_link::LinkEventKind::Refused);
    assert_eq!(
        events[0].detail,
        Some("tenant_link.region_refused"),
        "the audit names the refusal code, not a business value"
    );
}
