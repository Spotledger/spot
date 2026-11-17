//! WP-IAM-09.T5: every door that checks identity asks the offline decision (the coverage table of
//! `services::offline_identity` names the file each decision is taken in).
//!
//! Per door: an identity revoked offline is refused, an unrevoked one passes, a missing provider is
//! not ready. The table test fails when a door has no row or no file, or when the file does not ask
//! the decision (the wiring the row claims). No database is needed.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;

use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::lifecycle::{HookPoint, Phase, Target};
use spot_app_api::prelude::{Name, RecordId, SiteId, Value};
use spot_app_api::stub::StubHost;
use spot_app_api::{
    AppResult, BoxFuture, Controller, DocTypeDef, Document, FieldDef, FieldKind, HookContext,
    StepDecl, StepName, TextFormat,
};
use spot_cli::services::controllers::offline_controller;
use spot_cli::services::jobs::offline_refusal;
use spot_cli::services::offline_identity::{ENTRY_POINTS, EntryPoint, OfflineGate};
use spot_cli::services::realtime::{
    GateAdmission, LeaseProbe, NodeNow, OfflineSlot, door_slot, entry_point, unwired_readiness,
};
use spot_http::api::service::{Door, DoorTier, OfflineAdmission, OfflineRefusal};
use spot_offline_identity::{Decision, DenyReason, NodeClock, Tier};
use time::OffsetDateTime;

// ---- doubles ---------------------------------------------------------------------------------

/// Refuses the one revoked user, admits everyone else.
struct Gate {
    revoked: String,
}

impl OfflineGate for Gate {
    fn decide(
        &self,
        user: &str,
        _tier: Tier,
        _permission: Option<&str>,
        _now: NodeClock,
    ) -> Decision {
        if user == self.revoked {
            Decision::Deny(DenyReason::UserRevoked)
        } else {
            Decision::Allow
        }
    }
}

struct Now;

impl NodeNow for Now {
    fn now(&self) -> NodeClock {
        NodeClock {
            mono_ms: 1,
            boot_id: 1,
        }
    }
}

struct Lease(u64);

impl LeaseProbe for Lease {
    fn remaining_ms(&self, _tier: DoorTier) -> u64 {
        self.0
    }
}

fn admission(revoked: &str, lease_ms: Option<u64>) -> Arc<dyn OfflineAdmission> {
    let base = GateAdmission::new(
        Gate {
            revoked: revoked.to_owned(),
        },
        Arc::new(Now),
    );
    match lease_ms {
        Some(ms) => Arc::new(base.with_lease(Arc::new(Lease(ms)))),
        None => Arc::new(base),
    }
}

/// A slot with the decision installed.
fn slot(revoked: &str) -> Arc<OfflineSlot> {
    let slot = Arc::new(OfflineSlot::default());
    slot.set(Some(admission(revoked, Some(60_000))));
    slot
}

/// Refuses everything as "cannot decide": a provider is missing.
struct NotReady;

impl OfflineAdmission for NotReady {
    fn admit(&self, _: Door, _: &str, _: DoorTier, _: Option<&str>) -> Result<(), OfflineRefusal> {
        Err(OfflineRefusal::NotReady)
    }

    fn node_admitted(&self, _: Door, _: DoorTier) -> Result<(), OfflineRefusal> {
        Err(OfflineRefusal::NotReady)
    }
}

fn not_ready_slot() -> Arc<OfflineSlot> {
    let slot = Arc::new(OfflineSlot::default());
    slot.set(Some(Arc::new(NotReady)));
    slot
}

/// A deterministic UUIDv7-shaped id (the version and variant bits set), distinct per `n`.
fn user_id(n: u8) -> RecordId {
    let mut bytes = [n; 16];
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    RecordId::from_bytes(bytes).unwrap()
}

fn invocation(actor: Actor) -> Invocation {
    Invocation {
        site: SiteId::new("acme.test").unwrap(),
        actor,
        command_id: None,
        language: None,
        now: OffsetDateTime::UNIX_EPOCH,
    }
}

fn as_user(n: u8) -> Invocation {
    invocation(Actor::User {
        id: user_id(n),
        via: None,
    })
}

// ---- the table -------------------------------------------------------------------------------

/// The text each door's file must contain: the call that asks the decision.
fn marker(door: Door) -> &'static str {
    match door {
        Door::Api => "OfflineGuarded",
        Door::Realtime => "Door::Realtime",
        Door::Jobs => "Door::Jobs",
        Door::Scripts => "Door::Scripts",
        Door::Controllers => "Door::Controllers",
        Door::Management => "Door::Management",
    }
}

#[test]
fn every_door_has_one_row_and_its_file_asks_the_decision() {
    assert_eq!(
        Door::ALL.len(),
        ENTRY_POINTS.len(),
        "a door without a row, or a row without a door"
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    for door in Door::ALL {
        let rows: Vec<_> = ENTRY_POINTS
            .iter()
            .filter(|row| row.point == entry_point(door))
            .collect();
        assert_eq!(rows.len(), 1, "{} needs exactly one row", door.as_str());
        let text = std::fs::read_to_string(root.join(rows[0].enforced_in)).unwrap();
        assert!(
            text.contains(marker(door)),
            "{} does not ask the offline decision for {}",
            rows[0].enforced_in,
            door.as_str()
        );
    }
    for point in EntryPoint::ALL {
        assert!(
            Door::ALL.iter().any(|door| entry_point(*door) == point),
            "an entry point with no door"
        );
    }
}

// ---- the admission (every door's decision) ---------------------------------------------------

#[test]
fn the_gate_admission_refuses_the_revoked_and_passes_the_rest_at_every_door() {
    let admission = admission("clerk-gone", Some(1));
    for door in Door::ALL {
        for tier in [DoorTier::One, DoorTier::Two, DoorTier::Three] {
            assert_eq!(
                admission.admit(door, "clerk-gone", tier, Some("invoice.read")),
                Err(OfflineRefusal::Denied)
            );
            assert_eq!(
                admission.admit(door, "clerk", tier, Some("invoice.read")),
                Ok(())
            );
        }
    }
}

#[test]
fn the_node_lease_decides_the_door_with_no_user_and_a_missing_probe_is_not_ready() {
    assert_eq!(
        admission("x", Some(1)).node_admitted(Door::Management, DoorTier::One),
        Ok(())
    );
    assert_eq!(
        admission("x", Some(0)).node_admitted(Door::Management, DoorTier::One),
        Err(OfflineRefusal::Denied)
    );
    assert_eq!(
        admission("x", None).node_admitted(Door::Management, DoorTier::One),
        Err(OfflineRefusal::NotReady)
    );
}

#[test]
fn an_empty_slot_changes_nothing_and_a_provider_that_cannot_decide_refuses() {
    let empty = OfflineSlot::default();
    assert!(!empty.enforcing());
    for door in Door::ALL {
        assert_eq!(empty.admit(door, "anyone", DoorTier::One, None), Ok(()));
        assert_eq!(empty.admit_node(door, DoorTier::One), Ok(()));
    }
    let broken = not_ready_slot();
    for door in Door::ALL {
        assert_eq!(
            broken.admit(door, "anyone", DoorTier::Three, None),
            Err(OfflineRefusal::NotReady)
        );
    }
}

#[test]
fn the_door_slot_is_the_same_on_every_build_of_the_site() {
    let first = door_slot("acme.slots", Door::Realtime);
    let again = door_slot("acme.slots", Door::Realtime);
    assert!(Arc::ptr_eq(&first, &again));
    assert!(!Arc::ptr_eq(&first, &door_slot("acme.slots", Door::Jobs)));
    assert!(!Arc::ptr_eq(
        &first,
        &door_slot("other.slots", Door::Realtime)
    ));
}

// ---- realtime --------------------------------------------------------------------------------

#[test]
fn realtime_refuses_a_revoked_user_and_a_missing_provider_and_admits_the_rest() {
    let user = user_id(7).to_string();
    let live = slot(&user);
    assert_eq!(
        live.admit(Door::Realtime, &user, DoorTier::Three, None),
        Err(OfflineRefusal::Denied)
    );
    assert_eq!(
        live.admit(
            Door::Realtime,
            &user_id(8).to_string(),
            DoorTier::Three,
            None
        ),
        Ok(())
    );
    assert_eq!(
        not_ready_slot().admit(Door::Realtime, &user, DoorTier::Three, None),
        Err(OfflineRefusal::NotReady)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_door_with_no_provider_reports_not_ready_with_the_code_and_nothing_else() {
    use spot_cli::services::Service as _;
    let door = unwired_readiness("offline_door_realtime");
    let message = door.check().await.unwrap_err();
    assert!(
        message.starts_with("offline_identity_host.no_directory"),
        "{message}"
    );
}

// ---- jobs ------------------------------------------------------------------------------------

#[test]
fn a_job_acting_for_a_revoked_user_is_refused_and_a_system_job_needs_no_user() {
    let revoked = user_id(1).to_string();
    let live = slot(&revoked);
    let refused = offline_refusal(&live, &as_user(1)).expect("refused");
    assert_eq!(refused.code, "permission.denied");
    assert!(offline_refusal(&live, &as_user(2)).is_none());
    let system = invocation(Actor::System {
        component: Name::new("scheduler").unwrap(),
    });
    assert!(offline_refusal(&live, &system).is_none());
    let consumer = invocation(Actor::Consumer { id: user_id(1) });
    assert!(offline_refusal(&live, &consumer).is_none());
    // The person behind an impersonation is decided too.
    let impersonated = invocation(Actor::User {
        id: user_id(2),
        via: Some(user_id(1)),
    });
    assert!(offline_refusal(&live, &impersonated).is_some());
    let broken = offline_refusal(&not_ready_slot(), &as_user(2)).expect("not ready");
    assert_eq!(broken.code, "server.unavailable");
}

// ---- controllers -----------------------------------------------------------------------------

struct Note;

impl Controller for Note {
    fn doctype(&self) -> &str {
        "note"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("note:touch").unwrap(),
            point: HookPoint::of(Phase::Validate, Target::Save),
            priority: 100,
            budget: std::time::Duration::from_millis(500),
        }]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

fn note_host(slot: Arc<OfflineSlot>) -> StubHost {
    let host = StubHost::new();
    let mut def = DocTypeDef::new(
        Name::new("note").unwrap(),
        "Note".to_owned(),
        false,
        false,
        None,
        vec![FieldDef::new(
            Name::new("title").unwrap(),
            FieldKind::Text {
                max_chars: Some(40),
                format: TextFormat::Plain,
            },
            false,
            false,
        )],
    );
    def.version = 1;
    host.define(def);
    host.register(offline_controller(Arc::new(Note), slot));
    host
}

fn note() -> Document {
    let mut doc = Document::new(Name::new("note").unwrap(), RecordId::new_v7());
    doc.set(Name::new("title").unwrap(), Value::Text("hello".to_owned()));
    doc
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_controller_step_for_a_revoked_user_is_refused_and_an_unrevoked_one_passes() {
    let revoked = user_id(1).to_string();
    let host = note_host(slot(&revoked));
    let err = host.create(&as_user(1), note()).await.unwrap_err();
    assert_eq!(err.code, "permission.denied");
    host.create(&as_user(2), note()).await.unwrap();
    // The platform itself is never decided.
    let system = invocation(Actor::System {
        component: Name::new("scheduler").unwrap(),
    });
    host.create(&system, note()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_controller_with_no_provider_is_not_ready_and_a_site_without_offline_is_unchanged() {
    let host = note_host(not_ready_slot());
    let err = host.create(&as_user(2), note()).await.unwrap_err();
    assert_eq!(err.code, "server.unavailable");
    let unchanged = note_host(Arc::new(OfflineSlot::default()));
    unchanged.create(&as_user(1), note()).await.unwrap();
}
