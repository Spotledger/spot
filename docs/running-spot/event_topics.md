Part of [Running Spot](../running-spot.md).

# Event topics

Every event the platform publishes has a **topic**, and every platform topic is registered once, in
`contracts/events/src/registry.rs`, with a class, a retention, a coalescing declaration, the crate that
owns it and the contract version it first appeared in. This page lists them all. It is for a
subscriber, an auditor or an app author who needs to know which topics exist, which are kept for the
audit period, which a rebuild replays and what an event of a topic looks like. How to receive the
stream is a different page: [the event stream](events.md).

## Reading the table

**Class** says what the event is (ADR-061 section 2):

| Class | Meaning | Applied by a rebuild |
| --- | --- | --- |
| `state` | The full state of one record after one committed transaction. | yes |
| `domain` | A business or app fact for consumers to react to. It is in the journal and available to a consumer. | no: replaying it would run the consumers' reactions twice |
| `audit` | A system fact that matters for proof or rebuild. | yes |

A rebuild (`applied_by_rebuild` in the registry) re-applies the events it must to reconstruct state.
It applies `state` and `audit` events and makes `domain` events available to a consumer without
applying them (ADR-061 amendment A.0.1).

**Retention** says how long the topic's events are kept: `audit` is the audit retention period
(years), not the outbox retention; `outbox` is the ordinary outbox retention, for events that are a
projection input or a notification.

**Coalescing** (ADR-066 section 2) says what a lagging replica receives when it catches up:
`keep_all` delivers every event; `latest_per_subject` delivers only the latest event per subject, and
is valid for `domain` topics only. No platform topic declares `latest_per_subject` today.

**Owner** is the crate that defines or emits the topic. **Since** is the contract version in which the
entry first appeared; it is never rewritten. **Status** is `retired` when the topic is no longer
emitted. **Example** links the example payloads under `contracts/events/examples/`, one link per file
(`<topic>.json` or `<topic>.<variant>.json`).

## The rules

- A class never changes after release, and an entry is never removed. A topic that is no longer
  emitted is marked `retired`, so an old journal still classifies. A changed class is a new topic name.
  A test compares the registry with the committed `registry.snapshot.json` and fails on a class change
  or a removal.
- A platform topic (`doc_*`, `sys_*`, `identity_*`) that has no entry is refused by `check_publishable`
  (the publish path does not call it yet). An app topic with no entry is allowed with a warning for one minor
  version, and the app manifest declares its class (ADR-056).
- Every `identity_*` topic is class `audit` (WP-CON-04.T5).

## Payload shapes

An example of what an event of each topic looks like is in `contracts/events/examples/`: the file
named after the topic, and one file per variant.
The nine `sys_*` topics registered in 0.7.1 also have payload schemas in the events contract crate
(`contracts/events/src/system_platform/`, WP-CON-04.T6); they are not enforced on the publish or
consume path yet.

## Adding a topic

Adding a topic is a change to the events contract: a registry entry in `registry.rs` and in
`registry.snapshot.json`, an example, a contract version bump and an entry in
`contracts/events/CHANGELOG.md`, following the rules above and the contract change process in
`the project coding rules`. This page does not change that process; it only reflects the result.

## The topics

The table is generated; do not edit it. CI runs `python3 tools/docs-check/gen_event_topics.py --check`,
which fails with a diff when the table is stale and also lints the registry, the examples and the
snapshot against each other.

Regenerate with python3 tools/docs-check/gen_event_topics.py --write

<!-- BEGIN GENERATED: topics -->

Generated from `contracts/events/registry.snapshot.json`, contract version 0.7.9: 61 topics.

### `doc_*` topics

| Topic | Class | Retention | Coalescing | Owner | Since | Status | Example |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `doc_created` | domain | outbox | keep_all | `spot-events-contract` | 0.1.0 | active | none |
| `doc_updated` | domain | outbox | keep_all | `spot-events-contract` | 0.1.0 | active | none |
| `doc_posted` | domain | outbox | keep_all | `spot-events-contract` | 0.1.0 | active | none |
| `doc_cancelled` | domain | outbox | keep_all | `spot-events-contract` | 0.1.0 | active | none |
| `doc_deleted` | domain | outbox | keep_all | `spot-events-contract` | 0.1.0 | active | none |
| `doc_renamed` | domain | outbox | keep_all | `spot-events-contract` | 0.1.0 | active | none |
| `doc_updated_posted` | domain | outbox | keep_all | `spot-events-contract` | 0.1.0 | active | none |
| `doc_state` | state | outbox | keep_all | `spot-journal-rebuild` | 0.7.1 | active | [child_rows](../../contracts/events/examples/doc/doc_state.child_rows.json), [delete](../../contracts/events/examples/doc/doc_state.delete.json), [insert](../../contracts/events/examples/doc/doc_state.insert.json), [protected_field](../../contracts/events/examples/doc/doc_state.protected_field.json), [update](../../contracts/events/examples/doc/doc_state.update.json) |
| `doc_edge` | domain | outbox | keep_all | `spot-graph-projection` | 0.7.1 | retired | none |

### `identity_*` topics

| Topic | Class | Retention | Coalescing | Owner | Since | Status | Example |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `identity_user_created` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_user_created.json) |
| `identity_user_enabled` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_user_enabled.json) |
| `identity_user_disabled` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_user_disabled.json) |
| `identity_user_anonymised` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_user_anonymised.json) |
| `identity_sign_in_succeeded` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_sign_in_succeeded.json) |
| `identity_sign_in_failed` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_sign_in_failed.json) |
| `identity_account_locked` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_account_locked.json) |
| `identity_account_unlocked` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_account_unlocked.json) |
| `identity_recovery_requested` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_recovery_requested.json) |
| `identity_recovery_completed` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_recovery_completed.json) |
| `identity_session_ended` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_session_ended.json) |
| `identity_api_key_created` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_api_key_created.json) |
| `identity_api_key_revoked` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_api_key_revoked.json) |
| `identity_role_changed` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_role_changed.json) |
| `identity_duty_changed` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_duty_changed.json) |
| `identity_assignment_created` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_assignment_created.json) |
| `identity_assignment_changed` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_assignment_changed.json) |
| `identity_assignment_ended` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_assignment_ended.json) |
| `identity_sso_policy_changed` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_sso_policy_changed.json) |
| `identity_sod_override` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_sod_override.json) |
| `identity_emergency_started` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_emergency_started.json) |
| `identity_emergency_ended` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_emergency_ended.json) |
| `identity_emergency_reviewed` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_emergency_reviewed.json) |
| `identity_impersonation_started` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_impersonation_started.json) |
| `identity_impersonation_ended` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_impersonation_ended.json) |
| `identity_field_accessed` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_field_accessed.json), [reveal](../../contracts/events/examples/identity/identity_field_accessed.reveal.json) |
| `identity_node_changed` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_node_changed.json) |
| `identity_snapshot_published` | audit | audit | keep_all | `spot-events-contract` | 0.2.0 | active | [example](../../contracts/events/examples/identity/identity_snapshot_published.json) |
| `identity_session_started` | audit | audit | keep_all | `spot-events-contract` | 0.3.0 | active | [example](../../contracts/events/examples/identity/identity_session_started.json) |
| `identity_password_changed` | audit | audit | keep_all | `spot-events-contract` | 0.3.0 | active | [example](../../contracts/events/examples/identity/identity_password_changed.json) |
| `identity_second_factor_enrolled` | audit | audit | keep_all | `spot-events-contract` | 0.3.0 | active | [example](../../contracts/events/examples/identity/identity_second_factor_enrolled.json) |
| `identity_second_factor_removed` | audit | audit | keep_all | `spot-events-contract` | 0.3.0 | active | [example](../../contracts/events/examples/identity/identity_second_factor_removed.json) |
| `identity_recovery_codes_regenerated` | audit | audit | keep_all | `spot-events-contract` | 0.3.0 | active | [example](../../contracts/events/examples/identity/identity_recovery_codes_regenerated.json) |
| `identity_sign_in_failed_burst` | audit | audit | keep_all | `spot-events-contract` | 0.3.0 | active | [example](../../contracts/events/examples/identity/identity_sign_in_failed_burst.json) |
| `identity_access_review_completed` | audit | audit | keep_all | `spot-events-contract` | 0.3.0 | active | [example](../../contracts/events/examples/identity/identity_access_review_completed.json) |

### `sys_*` topics

| Topic | Class | Retention | Coalescing | Owner | Since | Status | Example |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `sys_doc_audit` | audit | audit | keep_all | `spot-doc-audit` | 0.4.0 | active | [consumer](../../contracts/events/examples/sys/sys_doc_audit.consumer.json), [correction_half](../../contracts/events/examples/sys/sys_doc_audit.correction_half.json), [denial](../../contracts/events/examples/sys/sys_doc_audit.denial.json), [example](../../contracts/events/examples/sys/sys_doc_audit.json), [method_call](../../contracts/events/examples/sys/sys_doc_audit.method_call.json), [method_truncated](../../contracts/events/examples/sys/sys_doc_audit.method_truncated.json), [node_stamped](../../contracts/events/examples/sys/sys_doc_audit.node_stamped.json) |
| `sys_graph_edge` | domain | outbox | keep_all | `spot-events-contract` | 0.4.0 | active | [example](../../contracts/events/examples/sys/sys_graph_edge.json), [relation_update](../../contracts/events/examples/sys/sys_graph_edge.relation_update.json), [remove](../../contracts/events/examples/sys/sys_graph_edge.remove.json) |
| `sys_secret_audit` | audit | audit | keep_all | `spot-document` | 0.5.0 | active | [example](../../contracts/events/examples/sys/sys_secret_audit.json), [operator](../../contracts/events/examples/sys/sys_secret_audit.operator.json), [refused](../../contracts/events/examples/sys/sys_secret_audit.refused.json), [rewrap](../../contracts/events/examples/sys/sys_secret_audit.rewrap.json), [webhooks](../../contracts/events/examples/sys/sys_secret_audit.webhooks.json) |
| `sys_job_enqueued` | domain | outbox | keep_all | `spot-document` | 0.7.1 | active | [example](../../contracts/events/examples/sys/sys_job_enqueued.json), [minimal](../../contracts/events/examples/sys/sys_job_enqueued.minimal.json) |
| `sys_user_notice` | domain | outbox | keep_all | `spot-document` | 0.7.1 | active | [example](../../contracts/events/examples/sys/sys_user_notice.json) |
| `sys_operation_receipt` | audit | audit | keep_all | `spot-document` | 0.7.1 | active | [example](../../contracts/events/examples/sys/sys_operation_receipt.json) |
| `sys_operation_receipt_effect` | audit | audit | keep_all | `spot-document` | 0.7.1 | active | [example](../../contracts/events/examples/sys/sys_operation_receipt_effect.json) |
| `sys_partition_range_low` | domain | outbox | keep_all | `spot-partition` | 0.7.1 | active | [example](../../contracts/events/examples/sys/sys_partition_range_low.json) |
| `sys_bpm_governance` | audit | audit | keep_all | `spot-bpm-governance` | 0.7.1 | active | [example](../../contracts/events/examples/sys/sys_bpm_governance.json), [policy_change](../../contracts/events/examples/sys/sys_bpm_governance.policy_change.json) |
| `sys_bpm_case_migrated` | audit | audit | keep_all | `spot-bpm-versioning` | 0.7.1 | active | [example](../../contracts/events/examples/sys/sys_bpm_case_migrated.json) |
| `sys_condition_raised` | audit | audit | keep_all | `spot-conditions` | 0.7.1 | active | [escalated](../../contracts/events/examples/sys/sys_condition_raised.escalated.json), [example](../../contracts/events/examples/sys/sys_condition_raised.json) |
| `sys_condition_cleared` | audit | audit | keep_all | `spot-conditions` | 0.7.1 | active | [example](../../contracts/events/examples/sys/sys_condition_cleared.json) |
| `sys_journal_access` | audit | audit | keep_all | `spot-events-contract` | 0.7.6 | active | [denied](../../contracts/events/examples/sys/sys_journal_access.denied.json), [example](../../contracts/events/examples/sys/sys_journal_access.json) |
| `sys_journal_grant` | audit | audit | keep_all | `spot-events-contract` | 0.7.6 | active | [example](../../contracts/events/examples/sys/sys_journal_grant.json), [refused](../../contracts/events/examples/sys/sys_journal_grant.refused.json) |
| `sys_journal_admin` | audit | audit | keep_all | `spot-events-contract` | 0.7.6 | active | [example](../../contracts/events/examples/sys/sys_journal_admin.json), [refused](../../contracts/events/examples/sys/sys_journal_admin.refused.json) |
| `sys_sync_node_admin` | audit | audit | keep_all | `spot-events-contract` | 0.7.6 | active | [failed](../../contracts/events/examples/sys/sys_sync_node_admin.failed.json), [example](../../contracts/events/examples/sys/sys_sync_node_admin.json) |
| `sys_sync_decision` | audit | audit | keep_all | `spot-events-contract` | 0.7.6 | active | [example](../../contracts/events/examples/sys/sys_sync_decision.json), [refused](../../contracts/events/examples/sys/sys_sync_decision.refused.json) |

<!-- END GENERATED: topics -->
