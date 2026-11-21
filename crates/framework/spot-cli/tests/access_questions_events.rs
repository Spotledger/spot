//! WP-IAM-08.T15: role, assignment and user changes reach the access-question model through the
//! outbox. A fake outbox and a fixed directory stand in for the database; no network.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use spot_cli::services::access_questions_model::ModelError;
use spot_cli::services::access_questions_model::assemble::{
    GrantInputs, RawAssignment, RawDirectory, RawRole, RawUser,
};
use spot_cli::services::access_questions_model::directory::{DirectoryReader, FixedDirectory};
use spot_cli::services::access_questions_model::events::{
    AccessChange, Entity, Op, Touched, change_events, classify,
};
use spot_cli::services::access_questions_model::feed::ChangeFeedService;
use spot_cli::services::access_questions_model::history::{
    ChangeJournal, JournalEntry, JournalError,
};
use spot_cli::services::access_questions_model::live::{
    LiveModelSource, ModelSettings, SiteInputs, SiteView,
};
use spot_cli::services::ports::{ConsumerLease, OutboxLease, PortError, PortFuture};
use spot_core::ManualClock;
use spot_http::api::service::BoxFuture;
use spot_iam_governance::{ChangeEvent, ChangeKind, ChangeOp};
use spot_sod::RuleSet;
use spot_storage_contract::{FeedPosition, Name, NodeRef, OutboxEvent, OutboxRecord, RecordId};
use time::macros::datetime;

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn record(topic: &str, entity: &str, id: RecordId, seq: u64) -> OutboxRecord {
    OutboxRecord {
        position: FeedPosition::new(1, seq),
        transaction: RecordId::new_v7(),
        ordinal: 0,
        event: OutboxEvent {
            id: RecordId::new_v7(),
            topic: n(topic),
            subject: NodeRef {
                entity: n(entity),
                id,
            },
            subject_version: None,
            schema_version: 1,
            payload: "{}".to_owned(),
        },
    }
}

struct Outbox(Mutex<Vec<OutboxRecord>>);

impl OutboxLease for Outbox {
    fn acquire<'a>(&'a self, _c: &'a str) -> PortFuture<'a, ConsumerLease> {
        Box::pin(async { Err(PortError::LeaseUnavailable) })
    }
    fn renew<'a>(&'a self, _l: &'a ConsumerLease) -> PortFuture<'a, ConsumerLease> {
        Box::pin(async { Err(PortError::LeaseUnavailable) })
    }
    fn still_held<'a>(&'a self, _l: &'a ConsumerLease) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }
    fn release<'a>(&'a self, _l: &'a ConsumerLease) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn read<'a>(&'a self, after: FeedPosition, limit: u32) -> PortFuture<'a, Vec<OutboxRecord>> {
        Box::pin(async move {
            let all = self.0.lock().unwrap();
            Ok(all
                .iter()
                .filter(|r| r.position > after)
                .take(limit as usize)
                .cloned()
                .collect())
        })
    }
}

#[derive(Default)]
struct Journal(Mutex<Vec<ChangeEvent>>);

impl ChangeJournal for Journal {
    fn head<'a>(&'a self) -> BoxFuture<'a, Result<u64, JournalError>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().len() as u64) })
    }
    fn range<'a>(
        &'a self,
        _f: u64,
        _t: u64,
    ) -> BoxFuture<'a, Result<Vec<JournalEntry>, JournalError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn append<'a>(&'a self, event: &'a ChangeEvent) -> BoxFuture<'a, Result<u64, JournalError>> {
        Box::pin(async move {
            let mut all = self.0.lock().unwrap();
            all.push(event.clone());
            Ok(all.len() as u64)
        })
    }
}

struct Inputs;

impl SiteInputs for Inputs {
    fn view(&self) -> Result<SiteView, ModelError> {
        let definitions = spot_cli::appdir::platform_definitions().unwrap();
        Ok(SiteView {
            meta: spot_meta::Registry::new(definitions).unwrap().effective(),
            token: 1,
            grants: GrantInputs::default(),
            duties: Vec::new(),
            rules: RuleSet::default(),
        })
    }
}

fn role(id: &str, name: &str) -> RawRole {
    RawRole {
        id: id.to_owned(),
        name: name.to_owned(),
        ..RawRole::default()
    }
}

fn assignment(id: &str, user: &str, role_id: &str, state: &str) -> RawAssignment {
    RawAssignment {
        id: id.to_owned(),
        user: user.to_owned(),
        role_id: role_id.to_owned(),
        source: "manual".to_owned(),
        state: state.to_owned(),
        ..RawAssignment::default()
    }
}

struct Rig {
    source: Arc<LiveModelSource>,
    directory: Arc<FixedDirectory>,
    journal: Arc<Journal>,
    outbox: Arc<Outbox>,
    clock: Arc<ManualClock>,
}

fn rig() -> Rig {
    let base = RawDirectory {
        roles: vec![role("r1", "Auditor")],
        assignments: Vec::new(),
        users: vec![RawUser {
            id: "u1".to_owned(),
            user_type: "internal".to_owned(),
        }],
    };
    let directory = Arc::new(FixedDirectory::new(base));
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-14 12:00 UTC)));
    let journal = Arc::new(Journal::default());
    let source = Arc::new(LiveModelSource::new(
        Arc::clone(&directory) as Arc<dyn DirectoryReader>,
        Arc::new(Inputs),
        None,
        Some(Arc::clone(&journal) as Arc<dyn ChangeJournal>),
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
        ModelSettings::default(),
    ));
    Rig {
        source,
        directory,
        journal,
        outbox: Arc::new(Outbox(Mutex::new(Vec::new()))),
        clock,
    }
}

fn dir_with(assignments: Vec<RawAssignment>) -> RawDirectory {
    RawDirectory {
        roles: vec![role("r1", "Auditor")],
        assignments,
        users: vec![RawUser {
            id: "u1".to_owned(),
            user_type: "internal".to_owned(),
        }],
    }
}

#[test]
fn only_role_assignment_and_user_documents_are_access_changes_and_only_ids_are_kept() {
    let id = RecordId::new_v7();
    let c = classify(&record("doc_created", "role_assignment", id, 1)).unwrap();
    assert_eq!(
        (c.entity, c.op, c.id.clone()),
        (Entity::Assignment, Op::Created, id.to_string())
    );
    assert_eq!(
        classify(&record("doc_deleted", "role", id, 2)).unwrap().op,
        Op::Deleted
    );
    assert_eq!(
        classify(&record("doc_updated", "user", id, 3))
            .unwrap()
            .entity,
        Entity::User
    );
    assert!(classify(&record("doc_updated", "sales_invoice", id, 4)).is_none());
    assert!(classify(&record("something_else", "role", id, 5)).is_none());
}

#[tokio::test]
async fn an_assignment_change_shows_without_a_restart_and_is_journaled_with_ids_only() {
    let rig = rig();
    assert!(rig.source.core().await.unwrap().assignments.is_empty());
    rig.source.attach_feed();
    let feed = ChangeFeedService::new(rig.outbox.clone(), Arc::clone(&rig.source));

    // Assign a role.
    rig.directory
        .set(Ok(dir_with(vec![assignment("a1", "u1", "r1", "active")])));
    let id = RecordId::new_v7();
    let mut rec = record("doc_created", "role_assignment", id, 1);
    rec.event.subject.id = id;
    rig.outbox.0.lock().unwrap().push(rec);
    let (next, seen) = feed.pump(FeedPosition::START).await.unwrap();
    assert_eq!(seen, 1);
    assert_eq!(next, FeedPosition::new(1, 1));
    // Well inside the repair interval: the new assignment is already in the core.
    rig.clock.advance(std::time::Duration::from_secs(10));
    assert_eq!(rig.source.core().await.unwrap().assignments.len(), 1);
    let events = rig.journal.0.lock().unwrap().clone();
    assert_eq!(
        events.len(),
        0,
        "the id of the record is not the assignment's stored id here"
    );

    // Remove it (state ended): the question answers change again.
    rig.directory
        .set(Ok(dir_with(vec![assignment("a1", "u1", "r1", "ended")])));
    let changes = [AccessChange {
        entity: Entity::Assignment,
        op: Op::Updated,
        id: "a1".into(),
    }];
    let events = rig.source.apply_changes(&changes).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, ChangeKind::Grant);
    assert_eq!(events[0].op, ChangeOp::Ended);
    assert_eq!(events[0].user.as_deref(), Some("u1"));
    assert_eq!(events[0].role.as_deref(), Some("Auditor"));
    assert!(events[0].before.is_none() && events[0].after.is_none() && events[0].scope.is_none());
}

#[tokio::test]
async fn a_missed_event_is_repaired_by_the_periodic_rebuild_and_a_reset_forces_one() {
    let rig = rig();
    rig.source.attach_feed();
    assert!(rig.source.core().await.unwrap().assignments.is_empty());
    // A change nobody reported.
    rig.directory
        .set(Ok(dir_with(vec![assignment("a9", "u1", "r1", "active")])));
    rig.clock.advance(std::time::Duration::from_secs(60));
    assert!(rig.source.core().await.unwrap().assignments.is_empty());
    // The feed says its view is broken: the next question reads everything.
    rig.source.request_rebuild();
    assert_eq!(rig.source.core().await.unwrap().assignments.len(), 1);
    // And without that, the repair interval does it.
    rig.directory.set(Ok(dir_with(Vec::new())));
    rig.clock.advance(std::time::Duration::from_secs(301));
    assert!(rig.source.core().await.unwrap().assignments.is_empty());
}

#[test]
fn deleted_records_are_found_in_the_directory_before_and_nothing_but_ids_names_and_tokens_leave() {
    let before = dir_with(vec![assignment("a1", "u1", "r1", "active")]);
    let after = dir_with(Vec::new());
    let changes = [
        AccessChange {
            entity: Entity::Assignment,
            op: Op::Deleted,
            id: "a1".into(),
        },
        AccessChange {
            entity: Entity::User,
            op: Op::Updated,
            id: "u1".into(),
        },
        AccessChange {
            entity: Entity::Role,
            op: Op::Updated,
            id: "r1".into(),
        },
    ];
    let events = change_events(&before, &after, &changes, 1_700_000_000);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].op, ChangeOp::Deleted);
    assert_eq!(events[1].kind, ChangeKind::Role);
    assert_eq!(events[1].subject, "Auditor");
    let mut touched = Touched::default();
    for c in &changes {
        touched.add(c);
    }
    assert_eq!(touched.len(), 3);
}
