//! `plan_idp` and `plan_rules` carry `#[deprecated]` (WP-IAM-11.T17): they cannot apply the
//! portal rule (ADR-083 Barrier 3). This file denies the `deprecated` lint and calls only the
//! guarded planners, so it builds exactly while the guarded entry points are warning-free and
//! stays a place where a new caller of an unguarded planner would fail the build.
#![deny(deprecated)]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_iam_assign::{GroupRoleMap, IdSource, OutsideGuard, RoleDirectory, plan_idp_guarded};
use time::OffsetDateTime;

struct Ids(u32);

impl IdSource for Ids {
    fn next(&mut self) -> String {
        self.0 += 1;
        format!("a{}", self.0)
    }
}

#[test]
fn the_guarded_planner_is_callable_without_a_deprecation_warning() {
    let map = GroupRoleMap::load(
        vec![],
        &RoleDirectory::new(Vec::<spot_iam_assign::RoleName>::new()),
    )
    .unwrap();
    let plan = plan_idp_guarded(
        &map,
        &mut Ids(0),
        &[],
        false,
        &[],
        OffsetDateTime::UNIX_EPOCH,
        &OutsideGuard::none(),
    );
    assert!(plan.changes.is_empty());
}
