//! The permission matrix: who may run a migration, and that a permit needs a recorded run.

use futures::executor::block_on;
use spot_permissions::{Principal, TENANT_ADMINISTRATOR};

use super::recorder;
use crate::events::RunSpec;
use crate::permit::{RUN_PERMISSION, RoleAccess, RunAccess, permit};
use crate::recorder::RunRecorder;
use crate::run::{ArtifactKind, ArtifactRef, PlanRef, RunId};

fn spec(id: &str) -> RunSpec {
    RunSpec {
        id: RunId::new(id).unwrap(),
        plan_digest: "plan-digest".to_owned(),
        plan: PlanRef {
            artifact: ArtifactRef::of(ArtifactKind::Plan, "plans/x.json", b"plan"),
            summary: super::summary(),
        },
        actor: "user:alice".to_owned(),
        at: 1_000,
    }
}

#[test]
fn the_default_access_lets_the_tenant_administrator_and_the_system() {
    let access = RoleAccess::new();
    assert_eq!(access.permission(), RUN_PERMISSION);
    assert!(access.may_run(&Principal::with_roles("admin", &[TENANT_ADMINISTRATOR])));
    assert!(!access.may_run(&Principal::with_roles("clerk", &["Sales User"])));
    assert!(!access.may_run(&Principal::user("nobody", vec![])));
    assert!(access.may_run(&Principal::system()));
}

#[test]
fn a_granted_role_holds_the_permission() {
    let access = RoleAccess::new().granting("Migration Operator");
    assert!(access.may_run(&Principal::with_roles("op", &["Migration Operator"])));
    assert!(access.may_run(&Principal::with_roles("admin", &[TENANT_ADMINISTRATOR])));
    assert!(!access.may_run(&Principal::with_roles("clerk", &["Sales User"])));
}

#[test]
fn a_permit_needs_the_permission_and_a_recorded_run() {
    let (recorder, _ring) = recorder(8);
    let handle = block_on(recorder.begin(spec("run-permit"))).unwrap();
    let access = RoleAccess::new();

    let allowed = permit(
        &access,
        &Principal::with_roles("alice", &[TENANT_ADMINISTRATOR]),
        &handle,
        super::SITE,
        5_000,
    )
    .unwrap();
    assert_eq!(allowed.run().as_str(), "run-permit");
    assert_eq!(allowed.site(), super::SITE);
    assert_eq!(allowed.actor(), "user:alice");
    assert_eq!(allowed.at(), 5_000);

    let refused = permit(
        &access,
        &Principal::with_roles("clerk", &["Sales User"]),
        &handle,
        super::SITE,
        5_000,
    )
    .unwrap_err();
    assert_eq!(refused.code(), crate::codes::NOT_PERMITTED);

    // An administrator who is not the run's recorded actor cannot use someone else's handle.
    let other = permit(
        &access,
        &Principal::with_roles("admin", &[TENANT_ADMINISTRATOR]),
        &handle,
        super::SITE,
        5_000,
    )
    .unwrap_err();
    assert_eq!(other.code(), crate::codes::NOT_PERMITTED);

    let wrong_site = permit(&access, &Principal::system(), &handle, "site-2", 5_000).unwrap_err();
    assert_eq!(wrong_site.code(), crate::codes::WRONG_SITE);
}
