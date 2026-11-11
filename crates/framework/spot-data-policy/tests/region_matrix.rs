//! The residency matrix: placement, move, backup and egress against one policy, plus the region
//! map and the owner approval for a cross-region move.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_data_policy::{
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
fn the_region_matrix_refuses_the_right_clause_everywhere() {
    type Row = (&'static str, Option<Clause>, Option<Clause>, Option<Clause>);
    let policy = policy();
    // (region, placement, egress, backup clause or none)
    let matrix: &[Row] = &[
        ("eu-west", None, None, Some(Clause::BackupRegions)),
        ("eu-central", None, None, Some(Clause::BackupRegions)),
        (
            "eu-west-backup",
            Some(Clause::AllowedRegions),
            Some(Clause::EgressRegion),
            None,
        ),
        (
            "us-east",
            Some(Clause::ForbiddenRegions),
            Some(Clause::ForbiddenRegions),
            Some(Clause::ForbiddenRegions),
        ),
        (
            "ap-south",
            Some(Clause::AllowedRegions),
            Some(Clause::EgressRegion),
            Some(Clause::BackupRegions),
        ),
    ];
    for (name, placement, egress, backup) in matrix {
        let region = region(name);
        assert_eq!(
            check_placement(&policy, &region).err().map(|r| r.clause),
            *placement,
            "placement for {name}"
        );
        assert_eq!(
            check_egress_region(&policy, &region)
                .err()
                .map(|r| r.clause),
            *egress,
            "egress for {name}"
        );
        assert_eq!(
            check_backup_target(&policy, &region)
                .err()
                .map(|r| r.clause),
            *backup,
            "backup for {name}"
        );
    }
}

#[test]
fn a_refusal_names_the_region_and_the_policy_clause() {
    let refusal = check_placement(&policy(), &region("us-east")).unwrap_err();
    assert_eq!(refusal.region, region("us-east"));
    assert_eq!(refusal.clause.as_str(), "forbidden_regions");
    assert!(refusal.to_string().contains("us-east"));
    assert!(refusal.to_string().contains("forbidden_regions"));
}

#[test]
fn a_move_into_a_forbidden_region_is_refused_even_with_the_owner() {
    let policy = policy();
    let approval = RegionChangeApproval {
        approved_by: "user:owner".to_owned(),
        at: common::at(2026, 10, 19),
    };
    assert_eq!(
        check_move(
            &policy,
            &region("eu-west"),
            &region("us-east"),
            Some(&approval)
        )
        .unwrap_err()
        .clause,
        Clause::ForbiddenRegions
    );
}

#[test]
fn a_cross_region_move_needs_the_owner_and_a_same_region_move_does_not() {
    let policy = policy();
    let from = region("eu-west");
    let to = region("eu-central");
    assert_eq!(
        check_move(&policy, &from, &to, None).unwrap_err().clause,
        Clause::RegionChangeUnapproved
    );
    let approval = RegionChangeApproval {
        approved_by: "user:owner".to_owned(),
        at: common::at(2026, 10, 19),
    };
    assert!(check_move(&policy, &from, &to, Some(&approval)).is_ok());
    assert!(check_move(&policy, &from, &from, None).is_ok());
}

#[test]
fn endpoints_clusters_and_stores_are_looked_up_by_kind_not_just_id() {
    let map = RegionMap::from_toml(
        "[[resource]]\nid = \"llm-eu\"\nkind = \"endpoint\"\nregion = \"eu-central\"\n\
         [[resource]]\nid = \"db-1\"\nkind = \"cluster\"\nregion = \"eu-west\"\n\
         [[resource]]\nid = \"bucket-1\"\nkind = \"object_store\"\nregion = \"eu-west-backup\"\n",
    )
    .unwrap();
    let policy = policy();
    assert_eq!(
        map.check_egress(&policy, ResourceKind::Endpoint, "llm-eu")
            .unwrap(),
        &region("eu-central")
    );
    assert_eq!(
        map.check_egress(&policy, ResourceKind::Cluster, "db-1")
            .unwrap(),
        &region("eu-west")
    );
    assert_eq!(
        map.check_egress(&policy, ResourceKind::ObjectStore, "bucket-1")
            .unwrap_err()
            .clause,
        Clause::EgressRegion,
        "a backup-only region is not an egress target; backups use check_backup_target"
    );
    assert!(
        check_backup_target(
            &policy,
            map.require(ResourceKind::ObjectStore, "bucket-1").unwrap()
        )
        .is_ok()
    );
    assert_eq!(
        map.check_egress(&policy, ResourceKind::Endpoint, "db-1")
            .unwrap_err()
            .clause,
        Clause::UnknownResource,
        "the cluster id is not an endpoint"
    );
}
