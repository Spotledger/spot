//! Residency: the region vocabulary, the [`ResidencyPolicy`], the four checks and the
//! [`RegionMap`].
//!
//! ADR-058 rule 1 makes the tenant the unit of residency: a tenant's data, backups, snapshots,
//! read replicas, analytics, search and AI-memory stores and rehearsal clones never leave the
//! tenant's region. This module is the decision core only — the control plane, the move
//! workflow, the backup target chooser and AI endpoint selection call these pure functions and
//! act on the refusal. No function reads a clock, a database or the environment.
//!
//! The policy is data (a TOML file owned by the control plane), not code; a region list change
//! needs no release.
//!
//! ```toml
//! allowed_regions = ["eu-west", "eu-central"]
//! forbidden_regions = ["us-east"]
//! backup_regions = ["eu-west-backup"]
//! ```
//!
//! `forbidden_regions` wins over `allowed_regions`: a region listed in both is refused, so an
//! accidental broad allow list cannot un-forbid a region. An empty `backup_regions` list means
//! backups may use any allowed region; a non-empty list is exhaustive.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::DataPolicyError;

/// A data-residency region: `[a-z][a-z0-9_-]*`, at most 63 characters.
///
/// The list of regions and its legal meaning are control-plane data; this type only enforces the
/// shape so a region can never reach a file name or a SQL identifier unvalidated.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Region(String);

impl Region {
    /// Validates and wraps a region.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidRegion`] when the value is malformed.
    pub fn new(value: impl Into<String>) -> Result<Self, DataPolicyError> {
        let value = value.into();
        let mut chars = value.chars();
        let first = chars.next();
        let reason = if value.is_empty() || value.len() > 63 {
            Some("a region has 1 to 63 characters")
        } else if !first.is_some_and(|c| c.is_ascii_lowercase()) {
            Some("a region starts with a lower-case letter")
        } else if !chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            Some("a region uses only lower-case letters, digits, '-' or '_'")
        } else {
            None
        };
        match reason {
            Some(reason) => Err(DataPolicyError::InvalidRegion {
                region: value,
                reason,
            }),
            None => Ok(Self(value)),
        }
    }

    /// The region as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Region {
    type Err = DataPolicyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for Region {
    type Error = DataPolicyError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Region> for String {
    fn from(value: Region) -> Self {
        value.0
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The clause of the policy a refusal cites.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Clause {
    /// The region is not in `allowed_regions`.
    AllowedRegions,
    /// The region is in `forbidden_regions` (which wins over an allow entry).
    ForbiddenRegions,
    /// The backup target is not in `backup_regions`.
    BackupRegions,
    /// The data would leave the tenant's region for a region the policy does not allow.
    EgressRegion,
    /// The resource is not in the [`RegionMap`], so its region is unknown.
    UnknownResource,
    /// A cross-region move has no owner approval (ADR-058 rule 1).
    RegionChangeUnapproved,
}

impl Clause {
    /// The name of the policy clause, as written in the policy file.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowedRegions => "allowed_regions",
            Self::ForbiddenRegions => "forbidden_regions",
            Self::BackupRegions => "backup_regions",
            Self::EgressRegion => "egress_region",
            Self::UnknownResource => "region_map",
            Self::RegionChangeUnapproved => "region_change_unapproved",
        }
    }
}

impl fmt::Display for Clause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A refusal, naming the region and the exact policy clause that refused it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The policy clause that refused the operation.
    pub clause: Clause,
    /// The region the operation would have used, or the region that holds the resource.
    pub region: Region,
}

impl Refusal {
    fn new(clause: Clause, region: &Region) -> Self {
        Self {
            clause,
            region: region.clone(),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "region {} refused by {}", self.region, self.clause)
    }
}

impl std::error::Error for Refusal {}

/// The residency policy of one tenant, as data.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidencyPolicy {
    /// Regions the tenant's data may live in. An empty list allows nothing.
    #[serde(default)]
    pub allowed_regions: Vec<Region>,
    /// Regions the tenant's data may never live in, even if also allowed.
    #[serde(default)]
    pub forbidden_regions: Vec<Region>,
    /// Regions a backup of the tenant may be stored in. Empty means any allowed region.
    #[serde(default)]
    pub backup_regions: Vec<Region>,
}

impl ResidencyPolicy {
    /// Parses the policy from TOML metadata text.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidPolicy`] when the text does not parse, names an unknown key or
    /// repeats a region.
    pub fn from_toml(text: &str) -> Result<Self, DataPolicyError> {
        let policy: Self =
            toml::from_str(text).map_err(|error| DataPolicyError::InvalidPolicy {
                reason: error.to_string(),
            })?;
        policy.validate()?;
        Ok(policy)
    }

    /// Checks the region lists have no duplicates and every region is valid.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidPolicy`] on a duplicate region.
    pub fn validate(&self) -> Result<(), DataPolicyError> {
        for (name, regions) in [
            ("allowed_regions", &self.allowed_regions),
            ("forbidden_regions", &self.forbidden_regions),
            ("backup_regions", &self.backup_regions),
        ] {
            let mut seen = BTreeSet::new();
            for region in regions {
                if !seen.insert(region.as_str()) {
                    return Err(DataPolicyError::InvalidPolicy {
                        reason: format!("{name} repeats region {}", region.as_str()),
                    });
                }
            }
        }
        Ok(())
    }

    /// Whether the region is allowed at all (allowed and not forbidden).
    #[must_use]
    pub fn permits(&self, region: &Region) -> bool {
        !self.is_forbidden(region) && self.is_allowed(region)
    }

    /// Whether the region is explicitly forbidden.
    #[must_use]
    pub fn is_forbidden(&self, region: &Region) -> bool {
        self.forbidden_regions.iter().any(|r| r == region)
    }

    /// Whether the region is explicitly allowed.
    #[must_use]
    pub fn is_allowed(&self, region: &Region) -> bool {
        self.allowed_regions.iter().any(|r| r == region)
    }

    fn check_allowed(&self, region: &Region, clause: Clause) -> Result<(), Refusal> {
        if self.is_forbidden(region) {
            return Err(Refusal::new(Clause::ForbiddenRegions, region));
        }
        if !self.is_allowed(region) {
            return Err(Refusal::new(clause, region));
        }
        Ok(())
    }
}

/// Checks a placement (a new tenant, database, schema or replica) stays in an allowed region.
///
/// # Errors
/// A [`Refusal`] citing `forbidden_regions` or `allowed_regions`.
pub fn check_placement(policy: &ResidencyPolicy, region: &Region) -> Result<(), Refusal> {
    policy.check_allowed(region, Clause::AllowedRegions)
}

/// Checks a backup target.
///
/// A forbidden region is refused first. A non-empty `backup_regions` is exhaustive; an empty
/// list means any allowed region may hold a backup.
///
/// # Errors
/// A [`Refusal`] citing `forbidden_regions` or `backup_regions`.
pub fn check_backup_target(policy: &ResidencyPolicy, region: &Region) -> Result<(), Refusal> {
    if policy.is_forbidden(region) {
        return Err(Refusal::new(Clause::ForbiddenRegions, region));
    }
    if policy.backup_regions.is_empty() {
        if policy.is_allowed(region) {
            Ok(())
        } else {
            Err(Refusal::new(Clause::BackupRegions, region))
        }
    } else if policy.backup_regions.iter().any(|r| r == region) {
        Ok(())
    } else {
        Err(Refusal::new(Clause::BackupRegions, region))
    }
}

/// Checks that data may leave for a region — an AI endpoint, an analytics mirror, a search
/// cluster, a tenant link export or any other egress.
///
/// # Errors
/// A [`Refusal`] citing `forbidden_regions` or `egress_region`.
pub fn check_egress_region(policy: &ResidencyPolicy, region: &Region) -> Result<(), Refusal> {
    policy.check_allowed(region, Clause::EgressRegion)
}

/// The owner's approval of a cross-region move (ADR-058 rule 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionChangeApproval {
    /// Who approved it (`user:<id>` or a service identity).
    pub approved_by: String,
    /// When it was approved, in Unix milliseconds.
    pub at: u64,
}

/// Checks a move from one region to another.
///
/// A move inside one region is a placement check. A move to another region must satisfy the
/// policy **and** carry the owner's approval; ADR-058 rule 1 refuses a region change otherwise.
///
/// # Errors
/// A [`Refusal`] citing `forbidden_regions`, `allowed_regions` or
/// `region_change_unapproved`.
pub fn check_move(
    policy: &ResidencyPolicy,
    from: &Region,
    to: &Region,
    approval: Option<&RegionChangeApproval>,
) -> Result<(), Refusal> {
    policy.check_allowed(to, Clause::AllowedRegions)?;
    if from == to {
        return Ok(());
    }
    match approval {
        Some(approval) if !approval.approved_by.trim().is_empty() => Ok(()),
        _ => Err(Refusal::new(Clause::RegionChangeUnapproved, to)),
    }
}

/// What a [`RegionMap`] entry describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    /// An endpoint the platform may call (an AI provider, a webhook, a connector).
    Endpoint,
    /// A database cluster.
    Cluster,
    /// An object store (backups, snapshots, files).
    ObjectStore,
}

impl ResourceKind {
    /// The kind as metadata text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Endpoint => "endpoint",
            Self::Cluster => "cluster",
            Self::ObjectStore => "object_store",
        }
    }
}

/// One resource and the region it physically sits in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRegion {
    /// The resource's stable id (an endpoint name, cluster id or bucket name).
    pub id: String,
    /// What kind of resource it is.
    pub kind: ResourceKind,
    /// The region its hardware is in.
    pub region: Region,
}

/// Where endpoints, clusters and object stores physically are — data, not code.
///
/// A resource missing from the map has an **unknown** region, and every check that needs it is
/// refused rather than guessed.
///
/// ```toml
/// [[resource]]
/// id = "llm-eu"
/// kind = "endpoint"
/// region = "eu-west"
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegionMap {
    entries: BTreeMap<(ResourceKind, String), Region>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegionMapFile {
    #[serde(default)]
    resource: Vec<ResourceRegion>,
}

impl RegionMap {
    /// Parses the map from TOML metadata text.
    ///
    /// # Errors
    /// [`DataPolicyError::InvalidPolicy`] when the text does not parse or repeats a resource.
    pub fn from_toml(text: &str) -> Result<Self, DataPolicyError> {
        let file: RegionMapFile =
            toml::from_str(text).map_err(|error| DataPolicyError::InvalidPolicy {
                reason: error.to_string(),
            })?;
        let mut entries = BTreeMap::new();
        for entry in file.resource {
            if entry.id.is_empty() || entry.id.len() > 256 {
                return Err(DataPolicyError::InvalidPolicy {
                    reason: "a resource id has 1 to 256 characters".to_owned(),
                });
            }
            let key = (entry.kind, entry.id);
            if entries.insert(key.clone(), entry.region).is_some() {
                return Err(DataPolicyError::InvalidPolicy {
                    reason: format!("the region map repeats {} {}", key.0.as_str(), key.1),
                });
            }
        }
        Ok(Self { entries })
    }

    /// The region of a resource, if the map knows it.
    #[must_use]
    pub fn region_of(&self, kind: ResourceKind, id: &str) -> Option<&Region> {
        self.entries.get(&(kind, id.to_owned()))
    }

    /// The region of a resource, or a refusal citing `region_map`.
    ///
    /// # Errors
    /// A [`Refusal`] citing `region_map` when the resource is unknown.
    pub fn require(&self, kind: ResourceKind, id: &str) -> Result<&Region, Refusal> {
        self.region_of(kind, id).ok_or_else(|| Refusal {
            clause: Clause::UnknownResource,
            region: Region(format!("unknown:{}:{id}", kind.as_str())),
        })
    }

    /// Checks that egress to `id` is allowed, returning the resource's region.
    ///
    /// # Errors
    /// A [`Refusal`] citing `region_map`, `forbidden_regions` or `egress_region`.
    pub fn check_egress(
        &self,
        policy: &ResidencyPolicy,
        kind: ResourceKind,
        id: &str,
    ) -> Result<&Region, Refusal> {
        let region = self.require(kind, id)?;
        check_egress_region(policy, region)?;
        Ok(region)
    }

    /// The number of resources in the map.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the map has no resources.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{
        Clause, Region, RegionChangeApproval, RegionMap, ResidencyPolicy, ResourceKind,
        check_backup_target, check_egress_region, check_move, check_placement,
    };

    fn region(name: &str) -> Region {
        Region::new(name).unwrap()
    }

    fn policy() -> ResidencyPolicy {
        ResidencyPolicy::from_toml(
            "allowed_regions = [\"eu-west\", \"eu-central\"]\n\
             forbidden_regions = [\"us-east\"]\n\
             backup_regions = [\"eu-west-backup\"]\n",
        )
        .unwrap()
    }

    #[test]
    fn placement_is_allowed_inside_and_refused_outside() {
        let p = policy();
        assert!(check_placement(&p, &region("eu-west")).is_ok());
        let refusal = check_placement(&p, &region("us-east")).unwrap_err();
        assert_eq!(refusal.clause, Clause::ForbiddenRegions);
        let refusal = check_placement(&p, &region("ap-south")).unwrap_err();
        assert_eq!(refusal.clause, Clause::AllowedRegions);
    }

    #[test]
    fn a_forbidden_region_wins_over_an_allow_entry() {
        let p = ResidencyPolicy::from_toml(
            "allowed_regions = [\"us-east\"]\nforbidden_regions = [\"us-east\"]\n",
        )
        .unwrap();
        assert_eq!(
            check_placement(&p, &region("us-east")).unwrap_err().clause,
            Clause::ForbiddenRegions
        );
    }

    #[test]
    fn backups_use_their_own_list() {
        let p = policy();
        assert!(check_backup_target(&p, &region("eu-west-backup")).is_ok());
        // An allowed data region is not automatically a backup region when the list is exhaustive.
        assert_eq!(
            check_backup_target(&p, &region("eu-west"))
                .unwrap_err()
                .clause,
            Clause::BackupRegions
        );
        let empty = ResidencyPolicy::from_toml("allowed_regions = [\"eu-west\"]\n").unwrap();
        assert!(check_backup_target(&empty, &region("eu-west")).is_ok());
    }

    #[test]
    fn egress_uses_the_egress_clause() {
        let p = policy();
        let refusal = check_egress_region(&p, &region("ap-south")).unwrap_err();
        assert_eq!(refusal.clause, Clause::EgressRegion);
    }

    #[test]
    fn a_cross_region_move_needs_the_owner() {
        let p = policy();
        let from = region("eu-west");
        let to = region("eu-central");
        assert_eq!(
            check_move(&p, &from, &to, None).unwrap_err().clause,
            Clause::RegionChangeUnapproved
        );
        let approval = RegionChangeApproval {
            approved_by: "user:owner".to_owned(),
            at: 1,
        };
        assert!(check_move(&p, &from, &to, Some(&approval)).is_ok());
        assert!(check_move(&p, &from, &from, None).is_ok());
    }

    #[test]
    fn a_move_into_a_forbidden_region_is_refused_even_with_approval() {
        let p = policy();
        let approval = RegionChangeApproval {
            approved_by: "user:owner".to_owned(),
            at: 1,
        };
        assert_eq!(
            check_move(&p, &region("eu-west"), &region("us-east"), Some(&approval))
                .unwrap_err()
                .clause,
            Clause::ForbiddenRegions
        );
    }

    #[test]
    fn the_region_map_knows_its_resources_and_refuses_the_rest() {
        let map = RegionMap::from_toml(
            "[[resource]]\nid = \"llm-eu\"\nkind = \"endpoint\"\nregion = \"eu-west\"\n\
             [[resource]]\nid = \"bucket-1\"\nkind = \"object_store\"\nregion = \"eu-west-backup\"\n",
        )
        .unwrap();
        assert_eq!(
            map.region_of(ResourceKind::Endpoint, "llm-eu"),
            Some(&region("eu-west"))
        );
        assert_eq!(
            map.region_of(ResourceKind::Cluster, "llm-eu"),
            None,
            "the kind is part of the key"
        );
        let p = policy();
        assert_eq!(
            map.check_egress(&p, ResourceKind::Endpoint, "llm-eu")
                .unwrap(),
            &region("eu-west")
        );
        assert_eq!(
            map.check_egress(&p, ResourceKind::Endpoint, "llm-us")
                .unwrap_err()
                .clause,
            Clause::UnknownResource
        );
    }
}
