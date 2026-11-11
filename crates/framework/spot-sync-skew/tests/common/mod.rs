//! Shared fixtures.
#![allow(dead_code)]
use std::collections::{BTreeMap, BTreeSet};

use spot_sync_skew::{NodeReport, SoftwareVersion};

pub fn report(minor: u32) -> NodeReport {
    NodeReport {
        software_version: SoftwareVersion::new(2, minor, 0),
        metadata_version: 10,
        migration_epoch: 4,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: BTreeMap::new(),
    }
}

pub fn decodes(mut r: NodeReport, topic: &str, majors: &[u32]) -> NodeReport {
    r.decodable_majors.insert(
        topic.to_string(),
        majors.iter().copied().collect::<BTreeSet<_>>(),
    );
    r
}
