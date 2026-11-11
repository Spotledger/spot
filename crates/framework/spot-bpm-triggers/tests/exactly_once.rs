//! The exactly-once acceptance: every trigger type starts one Case per source event, under
//! duplicate delivery, out-of-order delivery, a restart mid-batch, two nodes racing, and two
//! scheduler instances producing the same slot.
//!
//! These tests drive the public API only, exactly as a host would.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use spot_bpm_model::NodeId;
use spot_bpm_triggers::{
    AllowAll, CaseStarter, DocumentEventSpec, InMemoryCaseStarter, PermissionGate, Result,
    ScheduleSpec, Source, SourceEvent, StartGate, StartInput, StartKey, StartOutcome, TriggerIndex,
    TriggerIntake, TriggerMatch, TriggerMatcher, TriggerRegistry, TriggerSpec, TriggerStarter,
};
use spot_core::ManualClock;
use spot_events_contract::Topic;
use spot_manifest::{Cron, Queue};
use spot_permissions::Principal;
use spot_scheduler::{
    InMemoryHighWater, InMemoryLeaseStore, MissedRunPolicy, RunSlot, Schedule, ScheduleBuilder,
    Scheduler, SchedulerConfig, SlotSink, TimeZone,
};
use spot_webhooks::{CommittedEvent, EventType};
use time::OffsetDateTime;

fn node(id: &str) -> NodeId {
    NodeId::new(id).unwrap()
}

fn document_index() -> TriggerIndex {
    let mut index = TriggerIndex::new();
    index.add_spec(
        "order_to_cash",
        "erp_selling",
        1,
        node("start"),
        TriggerSpec::DocumentEvent(DocumentEventSpec {
            doctype: "sales_order".into(),
            event: EventType::Post,
            condition: None,
        }),
    );
    index
}

fn registry() -> Arc<TriggerRegistry> {
    Arc::new(TriggerRegistry::with_builtin_kinds())
}

fn intake(starter: Arc<dyn CaseStarter>) -> TriggerIntake {
    TriggerIntake::new(
        Arc::new(document_index()),
        registry(),
        starter,
        Arc::new(AllowAll),
    )
}

fn posted(event_id: &str) -> SourceEvent {
    SourceEvent::document(
        CommittedEvent {
            event_id: event_id.into(),
            topic: Topic::any(EventType::Post.topic_name()).unwrap(),
            doctype: "sales_order".into(),
            document_id: format!("doc-{event_id}"),
            document_version: 1,
            event_type: EventType::Post,
            occurred_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            after_image: serde_json::json!({}),
            before_image: None,
        },
        Principal::system(),
    )
}

#[tokio::test]
async fn duplicate_delivery_starts_one_case() {
    let starter = Arc::new(InMemoryCaseStarter::new());
    let intake = intake(starter.clone());
    assert_eq!(intake.process(&posted("e1")).await.unwrap().started, 1);
    let second = intake.process(&posted("e1")).await.unwrap();
    assert_eq!(second.started, 0);
    assert_eq!(second.existing, 1);
    assert_eq!(starter.len(), 1);
}

#[tokio::test]
async fn out_of_order_delivery_still_starts_one_case_each() {
    let starter = Arc::new(InMemoryCaseStarter::new());
    let first = intake(starter.clone());
    let second = intake(starter.clone());
    first.process(&posted("e1")).await.unwrap();
    first.process(&posted("e2")).await.unwrap();
    // The same two events arrive in the other order on another node.
    let out_of_order_a = second.process(&posted("e2")).await.unwrap();
    let out_of_order_b = second.process(&posted("e1")).await.unwrap();
    assert_eq!(out_of_order_a.started, 0);
    assert_eq!(out_of_order_b.started, 0);
    assert_eq!(out_of_order_a.existing + out_of_order_b.existing, 2);
    assert_eq!(starter.len(), 2);
}

/// A starter that fails its second call, simulating a crash mid-batch, then behaves normally.
struct FlakyStarter {
    inner: InMemoryCaseStarter,
    calls: AtomicU64,
    fail_on: u64,
}

#[async_trait]
impl CaseStarter for FlakyStarter {
    async fn start(&self, key: StartKey, input: StartInput) -> Result<StartOutcome> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.fail_on {
            return Err(spot_bpm_triggers::Error::StartFailed {
                process: key.process.clone(),
                node: key.node.to_string(),
                reason: "injected crash".into(),
            });
        }
        self.inner.start(key, input).await
    }
}

#[tokio::test]
async fn a_restart_mid_batch_replays_without_duplicates() {
    let starter = Arc::new(FlakyStarter {
        inner: InMemoryCaseStarter::new(),
        calls: AtomicU64::new(0),
        fail_on: 2,
    });
    let intake = intake(starter.clone());
    let batch = [posted("e1"), posted("e2")];
    // The first pass starts e1, then crashes on e2.
    assert!(intake.process_batch(&batch).await.is_err());
    assert_eq!(starter.inner.len(), 1);

    // The restart replays the whole batch: e1 is existing, e2 starts.
    let reports = intake.process_batch(&batch).await.unwrap();
    assert_eq!(reports[0].existing, 1);
    assert_eq!(reports[1].started, 1);
    assert_eq!(starter.inner.len(), 2);
}

#[tokio::test]
async fn two_nodes_racing_start_one_case() {
    let starter = Arc::new(InMemoryCaseStarter::new());
    let a = intake(starter.clone());
    let b = intake(starter.clone());
    let event = posted("e1");
    let (ra, rb) = tokio::join!(a.process(&event), b.process(&event));
    let started = ra.unwrap().started + rb.unwrap().started;
    assert_eq!(started, 1, "exactly one node wins the race");
    assert_eq!(starter.len(), 1);
}

/// Collects the slots the scheduler hands over.
#[derive(Default)]
struct Collector {
    slots: std::sync::Mutex<Vec<RunSlot>>,
}

impl SlotSink for Collector {
    fn deliver(&self, slot: &RunSlot) -> bool {
        self.slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(slot.clone());
        true
    }
}

fn daily_schedule() -> Schedule {
    let cron = Cron::parse("0 * * * * *").unwrap();
    let step = spot_app_api::StepName::new("app:daily").unwrap();
    let zone = TimeZone::parse("UTC").unwrap();
    ScheduleBuilder::new("daily")
        .cron(cron)
        .step(step)
        .queue(Queue::Default)
        .budget(Duration::from_secs(600))
        .zone(zone)
        .build()
        .unwrap()
}

#[tokio::test]
async fn two_scheduler_instances_produce_one_case_per_slot() {
    // Two nodes contend for the schedule-leader lease; with a zero TTL the lease lapses at once,
    // so the second node takes over in the same instant and re-plans the same slot under the same
    // deduplication key. That is exactly the failover double-compute the start key must absorb.
    let leases = Arc::new(InMemoryLeaseStore::new());
    let schedule = Arc::new(daily_schedule());
    let config = |node_name: &str| SchedulerConfig {
        site: "site".into(),
        node: node_name.into(),
        ttl: Duration::ZERO,
        schedules: vec![(schedule.clone(), MissedRunPolicy::RunOnce)],
        app: "app".into(),
    };
    let collector = Arc::new(Collector::default());
    let clock = ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_767_225_660).unwrap(), // 2026-01-01T00:01:00Z
    );

    let mut a = Scheduler::new(config("a"), leases.clone(), InMemoryHighWater::new());
    let mut b = Scheduler::new(config("b"), leases.clone(), InMemoryHighWater::new());
    let slots_a = a.tick(&clock, &*collector).unwrap();
    let slots_b = b.tick(&clock, &*collector).unwrap();
    assert_eq!(slots_a.len(), 1, "the first node hands the slot over");
    assert_eq!(slots_b.len(), 1, "the second node re-plans the same slot");
    assert_eq!(
        slots_a[0].deduplication_key, slots_b[0].deduplication_key,
        "the failover re-plan carries the same key"
    );
    assert_eq!(a.held_lease().unwrap().token.0, 1);
    assert_eq!(
        b.held_lease().unwrap().token.0,
        2,
        "the fencing token strictly increases"
    );

    // Now feed both slots (the failover re-plan) into the intake: one Case.
    let mut index = TriggerIndex::new();
    index.add_spec(
        "order_to_cash",
        "app",
        1,
        node("daily"),
        TriggerSpec::Schedule(ScheduleSpec {
            app: "app".into(),
            schedule: (*schedule).clone(),
            policy: MissedRunPolicy::RunOnce,
        }),
    );
    let starter = Arc::new(InMemoryCaseStarter::new());
    let intake = TriggerIntake::new(
        Arc::new(index),
        registry(),
        starter.clone(),
        Arc::new(AllowAll),
    );
    let first = intake
        .process(&SourceEvent::system(Source::Schedule(slots_a[0].clone())))
        .await
        .unwrap();
    let second = intake
        .process(&SourceEvent::system(Source::Schedule(slots_b[0].clone())))
        .await
        .unwrap();
    assert_eq!(first.started, 1);
    assert_eq!(second.existing, 1);
    assert_eq!(starter.len(), 1);
}

/// A custom trigger kind (ADR-080), registered through the same `register_kind` as the built-ins.
struct MessageArrived;

impl TriggerMatcher for MessageArrived {
    fn matches(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<bool> {
        let (TriggerSpec::Custom(spec), Source::Custom(message)) = (&matched.spec, &event.source)
        else {
            return Ok(false);
        };
        Ok(spec.kind == message.kind)
    }
}

impl TriggerStarter for MessageArrived {
    fn start_input(&self, matched: &TriggerMatch, event: &SourceEvent) -> Result<StartInput> {
        let Source::Custom(message) = &event.source else {
            return Err(spot_bpm_triggers::Error::StartFailed {
                process: matched.process.clone(),
                node: matched.node.to_string(),
                reason: "a message-arrived start needs a message event".into(),
            });
        };
        Ok(StartInput {
            version: matched.version,
            variables: message.payload.clone(),
            initiator: None,
        })
    }
}

#[tokio::test]
async fn a_registered_custom_kind_starts_one_case_per_message() {
    let mut registry = TriggerRegistry::with_builtin_kinds();
    registry.register_kind(
        "message_arrived",
        Arc::new(MessageArrived),
        Arc::new(MessageArrived),
    );
    let mut index = TriggerIndex::new();
    index.add_spec(
        "conversation",
        "app",
        1,
        node("start"),
        TriggerSpec::Custom(spot_bpm_triggers::CustomSpec {
            kind: "message_arrived".into(),
            config: serde_json::json!({}),
        }),
    );
    let starter = Arc::new(InMemoryCaseStarter::new());
    let intake = TriggerIntake::new(
        Arc::new(index),
        Arc::new(registry),
        starter.clone(),
        Arc::new(AllowAll),
    );
    let message = |id: &str| {
        SourceEvent::system(Source::Custom(spot_bpm_triggers::CustomEvent {
            kind: "message_arrived".into(),
            source_event_id: id.into(),
            payload: serde_json::json!({"text": "hi"}),
        }))
    };
    assert_eq!(intake.process(&message("m1")).await.unwrap().started, 1);
    assert_eq!(intake.process(&message("m1")).await.unwrap().existing, 1);
    assert_eq!(starter.len(), 1);
}

/// A gate that refuses everything, to prove the intake consults it.
struct DenyAll;

impl StartGate for DenyAll {
    fn may_start(&self, _request: &spot_bpm_triggers::StartPermission<'_>) -> bool {
        false
    }
}

#[tokio::test]
async fn a_denied_start_is_reported_and_starts_nothing() {
    let starter = Arc::new(InMemoryCaseStarter::new());
    let intake = TriggerIntake::new(
        Arc::new(document_index()),
        registry(),
        starter.clone(),
        Arc::new(DenyAll),
    );
    let report = intake.process(&posted("e1")).await.unwrap();
    assert_eq!(report.started, 0);
    assert_eq!(report.rejected.len(), 1);
    assert_eq!(report.rejected[0].reason, "permission_denied");
    assert!(starter.is_empty());
}

#[tokio::test]
async fn a_manual_start_needs_the_named_role() {
    let mut index = TriggerIndex::new();
    index.add_spec(
        "order_to_cash",
        "erp_selling",
        1,
        node("start"),
        TriggerSpec::Manual(spot_bpm_triggers::ManualSpec {
            roles: vec!["Manager".into()],
            input_schema: None,
        }),
    );
    let starter = Arc::new(InMemoryCaseStarter::new());
    let intake = TriggerIntake::new(
        Arc::new(index),
        registry(),
        starter.clone(),
        Arc::new(PermissionGate::roles_only()),
    );
    let manual = |user: &str, roles: &[&str], id: &str| {
        SourceEvent::new(
            Source::Manual(spot_bpm_triggers::ManualStart {
                process: "order_to_cash".into(),
                node: None,
                idempotency_id: id.into(),
                input: serde_json::json!({}),
            }),
            Principal::with_roles(user, roles),
        )
    };
    assert_eq!(
        intake
            .process(&manual("u1", &["Viewer"], "m1"))
            .await
            .unwrap()
            .rejected
            .len(),
        1
    );
    assert_eq!(
        intake
            .process(&manual("u2", &["Manager"], "m2"))
            .await
            .unwrap()
            .started,
        1
    );
    assert_eq!(starter.len(), 1);
}

mod manual_and_api_input {
    use super::*;
    use serde_json::json;
    use spot_bpm_triggers::{ApiSpec, ApiStart, ManualSpec, ManualStart};

    fn schema() -> serde_json::Value {
        json!({"type": "object", "required": ["customer"], "additionalProperties": false,
               "properties": {"customer": {"type": "string"}, "qty": {"type": "integer"}}})
    }

    fn intake_over(spec: TriggerSpec) -> (TriggerIntake, Arc<InMemoryCaseStarter>) {
        let mut index = TriggerIndex::new();
        index.add_spec("p", "app", 1, node("start"), spec);
        let starter = Arc::new(InMemoryCaseStarter::new());
        let gate = PermissionGate::roles_only();
        let intake =
            TriggerIntake::new(Arc::new(index), registry(), starter.clone(), Arc::new(gate));
        (intake, starter)
    }

    fn manual(user: &str, input: serde_json::Value, id: &str) -> SourceEvent {
        SourceEvent::new(
            Source::Manual(ManualStart {
                process: "p".into(),
                node: None,
                idempotency_id: id.into(),
                input,
            }),
            Principal::with_roles(user, &["Manager"]),
        )
    }

    #[tokio::test]
    async fn a_manual_start_with_invalid_input_lists_every_problem_and_starts_nothing() {
        let (intake, starter) = intake_over(TriggerSpec::Manual(ManualSpec {
            roles: vec!["Manager".into()],
            input_schema: Some(schema()),
        }));
        let report = intake
            .process(&manual("u1", json!({"qty": "x", "extra": 1}), "m1"))
            .await
            .unwrap();
        assert_eq!(report.cases(), 0);
        assert!(starter.is_empty());
        let reason = &report.rejected[0].reason;
        assert!(reason.contains("/customer"), "{reason}");
        assert!(reason.contains("/qty"), "{reason}");
        assert!(reason.contains("/extra"), "{reason}");
    }

    #[tokio::test]
    async fn a_manual_start_without_the_role_is_refused_before_the_input_is_read() {
        let (intake, starter) = intake_over(TriggerSpec::Manual(ManualSpec {
            roles: vec!["Auditor".into()],
            input_schema: Some(schema()),
        }));
        let report = intake
            .process(&manual("u1", json!({"customer": "c"}), "m1"))
            .await
            .unwrap();
        assert_eq!(report.rejected[0].reason, "permission_denied");
        assert!(starter.is_empty());
    }

    #[tokio::test]
    async fn a_valid_manual_start_runs_once_even_when_the_same_event_is_retried() {
        let (intake, starter) = intake_over(TriggerSpec::Manual(ManualSpec {
            roles: vec!["Manager".into()],
            input_schema: Some(schema()),
        }));
        let event = manual("u1", json!({"customer": "c"}), "m1");
        assert_eq!(intake.process(&event).await.unwrap().started, 1);
        let again = intake.process(&event).await.unwrap();
        assert_eq!((again.started, again.existing), (0, 1));
        assert_eq!(starter.len(), 1);
    }

    #[tokio::test]
    async fn an_api_start_checks_roles_and_input() {
        let (intake, starter) = intake_over(TriggerSpec::Api(ApiSpec {
            name: "start_order".into(),
            roles: vec!["Integrator".into()],
            input_schema: Some(schema()),
        }));
        let api = |user: &str, roles: &[&str], input: serde_json::Value| {
            SourceEvent::new(
                Source::Api(ApiStart {
                    name: "start_order".into(),
                    process: None,
                    idempotency_id: "a1".into(),
                    input,
                }),
                Principal::with_roles(user, roles),
            )
        };
        let wrong_role = intake
            .process(&api("u1", &["Viewer"], json!({"customer": "c"})))
            .await
            .unwrap();
        assert_eq!(wrong_role.rejected[0].reason, "permission_denied");
        let bad_input = intake
            .process(&api("svc", &["Integrator"], json!({})))
            .await
            .unwrap();
        assert!(bad_input.rejected[0].reason.contains("/customer"));
        assert!(starter.is_empty());
        let ok = intake
            .process(&api("svc", &["Integrator"], json!({"customer": "c"})))
            .await
            .unwrap();
        assert_eq!(ok.started, 1);
    }
}
