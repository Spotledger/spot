# Runbook: reading `GET /api/health/ready`

A load balancer, a systemd unit and a person all read this endpoint. This page says, for every
entry a site can report, what it is, whether it is serious and what to do. Source of truth: the
code; `tools/docs-check/check_readiness_runbook.py` fails when a check exists in the code that this
page does not describe, or when a quoted text is not in the code.

Other pages to read with this one: [`running-spot.md`](../running-spot.md) (services and
participants), [`audit-verify.md`](../running-spot/audit-verify.md),
[`conditions.md`](../running-spot/conditions.md), [`node_role.md`](../running-spot/node_role.md),
[`service-ports.md`](../running-spot/service-ports.md).

## The three answers

`crates/framework/spot-http/src/health.rs` decides the status code. Each check has 2 seconds.

| Answer | Status | When |
| --- | --- | --- |
| ready | `200`, `"status": "ready"` | every check returned `ok`, with or without a detail |
| ready, degraded | `200`, `"status": "ready"` | every check returned `ok`, and at least one detail starts `degraded:` or `WARNING` |
| not ready | `503`, `"status": "not_ready"` | at least one check returned `ok = false`, or timed out (`timed out after 2s`), or its task crashed (`check task crashed: ...`) |
| draining | `503`, `"status": "not_ready"`, `"checks": []` | the server has started to shut down (the list is empty on purpose) |

The body is `{"status": ..., "checks": [{"name": ..., "ok": ..., "detail": ...}]}`. The shapes are
asserted by the tests in `crates/framework/spot-http/src/health.rs` (`ready_without_checks_is_ready`
and its neighbours) and `crates/framework/spot-http/tests/integration.rs` (a failing check turns
`/ready` into 503 while `/live` stays 200; `ready` answers 503 once shutdown has begun). These
bodies are copied from those assertions, not from a running site:

```json
{"status":"ready","checks":[{"name":"storage:main","ok":true,"detail":""}]}
{"status":"ready","checks":[{"name":"main","ok":true,"detail":"degraded: mail (SMTP unreachable)"}]}
{"status":"not_ready","checks":[{"name":"storage:main","ok":false,"detail":"storage is not healthy"}]}
```

## The checks, one by one

A check is named `<kind>:<site>` (one entry per site) or just `<site>` (the services of the site).

### storage

- **Name**: `storage:<site>`.
- **Reported by**: `StorageReady` in `crates/framework/spot-cli/src/host.rs`.
- **Detail texts**: <!-- detail: storage is not healthy -->`storage is not healthy`.
- **Severity**: not ready (503). It asks the database pool now, on every call.
- **Meaning**: the site's PostgreSQL did not answer a health probe.
- **What to do**: check the database server and the `[postgres]` section of `common.toml`; `spot check` reports configuration and connection problems; restart nothing until the database answers.
- **Do not**: restart the host while a `spot migrate` holds the database.

### metadata

- **Name**: `metadata:<site>`.
- **Reported by**: `LiveReady` in `crates/framework/spot-cli/src/live.rs`.
- **Detail texts**: `degraded: <list joined by "; ">` <!-- detail: degraded: -->, or the failing problem text.
- **Severity**: a problem is not ready (503); a `degraded:` list keeps `200`. The list names what the live metadata could not do, for example a customisation layer that diverges from the other nodes.
- **Meaning**: the site's effective metadata (apps, doctypes, customisation) is loaded and current, or says what is not.
- **What to do**: `spot reload` after fixing the app or customisation named in the text; `spot check` to validate the files first.
- **Do not**: reload repeatedly in a loop; a reload that fails keeps the previous metadata and says why.

### participants

- **Name**: `participants:<site>`.
- **Reported by**: `ParticipantsReady` in `crates/framework/spot-cli/src/services/participants.rs`.
- **Detail texts**: `<partition>; <docaudit>; installed: <list>` with `partition: off`, `docaudit: on`, `docaudit: off` <!-- detail: docaudit: on --><!-- detail: docaudit: off --><!-- detail: partition: off -->.
- **Severity**: informational: always `ok`. A site that cannot check ownership refuses to start instead of reporting here.
- **Meaning**: which document-lifecycle participants are installed and whether the node is partitioned. See "What `ready` says about the participants" in `running-spot.md`.
- **What to do**: nothing, unless the line shows a participant you expected missing: then check `spot list-apps` and the site's apps.

### partition snapshot

- **Name**: `partition:<site>`.
- **Reported by**: `SnapshotReady` in `crates/framework/spot-cli/src/services/partition.rs`. Present only on a partitioned node.
- **Detail texts**: `degraded: <detail>` when the ownership snapshot is stale <!-- detail: degraded: -->, otherwise the registry's detail.
- **Severity**: degrades only: always `ok`, because a stale snapshot still serves reads and the node stays in rotation. It reads in-memory counters; no I/O.
- **Meaning**: the node's copy of which partition owns what is old; writes to owned partitions may be refused until it refreshes.
- **What to do**: check the partition store and the node's connection to it; the line clears when the refresh succeeds.
- **Do not**: take the node out of rotation for this alone.

### identity-audit (chain)

- **Name**: `identity-audit:<site>`.
- **Reported by**: `IdentityChainReady` in `crates/framework/spot-cli/src/audit/chain.rs`.
- **Detail texts**: `identity audit chain: verified through seq <n> of <head>` <!-- detail: identity audit chain: verified through seq -->, `identity audit chain: BROKEN at seq` <!-- detail: identity audit chain: BROKEN at seq -->, `identity audit chain: cannot read the chain` <!-- detail: identity audit chain: cannot read the chain -->, `identity audit chain: verification did not run` <!-- detail: identity audit chain: verification did not run -->. A problem is prefixed `WARNING ` <!-- detail: WARNING -->.
- **Severity**: a problem is a warning (`200`, detail `WARNING ...`) by default; with `[audit] identity_chain_strict` it is not ready (503). Verification resumes where the last call stopped and restarts from the first entry once a day.
- **Meaning**: the hash chain of the sign-in and password records verifies up to the shown position, or it is broken at the shown sequence number.
- **What to do**: `spot audit verify --site <site>` (reads only) for the full answer; see `audit-verify.md`; a break is a security event: preserve the database and escalate.
- **Do not**: edit the audit tables by hand.

### identity-audit (file only)

- **Name**: `identity-audit:<site>`.
- **Reported by**: `FileOnlyReady` in `crates/framework/spot-cli/src/audit/chain.rs`.
- **Detail texts**: <!-- detail: identity audit: file log only, NOT tamper-evident (no audit chain) -->`identity audit: file log only, NOT tamper-evident (no audit chain)`.
- **Severity**: informational: `ok`, but the line says the trail is not tamper-evident.
- **Meaning**: the site has no audit chain in its database; sign-in records go to `identity-audit.log` only.
- **What to do**: run `spot migrate` so the chain tables exist, then restart; until then do not rely on the trail for evidence.

### the services of the site

- **Name**: `<site>` (the site's own name, no prefix).
- **Reported by**: `ServiceHealth` in `crates/framework/spot-cli/src/services/mod.rs`.
- **Detail texts**: `required service `name` has not finished starting` <!-- detail: has not finished starting -->, `required service `name` is stopped: <reason>` <!-- detail: is stopped: -->, `degraded: <name> (starting)` and `degraded: <name> (<reason>)` <!-- detail: degraded: -->.
- **Severity**: a stopped or starting **required** service is not ready (503); a stopped optional service keeps `200` with `degraded: <service> (<reason>)`. At start-up a required service that fails its check stops the site; an optional one starts degraded and says so here.
- **Meaning**: one entry covers every service; the list below names them and their level.
- **What to do**: read the reason in the detail, then the page of that service under `docs/running-spot/`; fix the cause and `spot reload` or restart the host.
- **Do not**: restart the host for an optional service that is degraded; the site still serves.

Services (name: level). Required ones stop the site: `storage`, `auth`, `customise`, `metadata`, `jobs`, `scheduler`, `secrets`, `maintenance_gate`, `report` and `function`. The others are optional: `capture_invariant`, `conditions`, `discovery`, `events`, `export`, `files`, `mail`, `node_role`, `ops_signals`, `print`, `process`, `process_documents`, `process_integration`, `process_triggers`, `realtime`, `service_ports`, `session`, `telemetry`, `workload_routing` and `cluster_redis`. (`files` is required in the cluster profile.) `ops_signals` adds no readiness entry of its own: its line is in the log.

The services below were added after the list above; each is optional unless its own module says otherwise (a service's level is declared in its `fn level`, and a required one stops the site as described above).:
`access_questions_feed`, `access_review`, `ai_isolation`, `apps`, `assignment_rules`, `backup_schedule`, `command_ledger`, `connectors`, `devserver`, `devtools`, `docaudit`, `durability`, `edge_offline`, `effect_hold`, `egress`, `emergency_access`, `graph`, `graph_projection`, `heartbeat`, `idempotency`, `import`, `journal_audit`, `journal_privacy`, `journal_snapshots`, `licence`, `link_exchange`, `link_external`, `links`, `mail_intake`, `metering`, `mgmt_api`, `occupancy`, `process_conversations`, `process_effects`, `process_effects_graph`, `process_governance`, `process_governance_keys`, `process_insight`, `process_tasks`, `process_tasks_calendar`, `process_tasks_notices`, `process_throughput`, `process_versions`, `protected_keys`, `roles`, `rule_graph`, `sandbox_reaper`, `script_kinds`, `scripts`, `search`, `secrets_kms`, `support_access_wiring`, `sync_catchup`, `sync_hub`, `sync_hub_directory`, `sync_hub_stream`, `sync_node`, `sync_pins`, `sync_policy`, `updater`, `webhooks`, and `access_questions`, `agent_tiers`, `governance`, `governance_approval`, `identity_admin`, `invitations`, `journal_access`, `lineage`, `login_stores`, `mcp`, `scim`, `signin_policy`, `signin_throttle`, `sso_identity`, `sso_logout`, `sync_catchup_serve`, `sync_catchup_wiring`, `sync_process`, `totp`, `webhook_secret`.

### capture invariant, reports, edge offline, KMS, sync loops, sync node and sync policy

Seven further checks report on one subsystem each. All of them return `ok` except `PolicyReadiness`,
so a line here changes the answer to `503` only for the sync policy; the others show their state in
the detail text and, when they degrade, begin it with `degraded:`.

- **`CaptureReady`**: name `capture_invariant:<site>`. Reports the capture invariant (every committed change reaches the journal): `capture_invariant: ok (watermark g<generation>:s<feed>, <n> findings open)`, or `degraded: capture_invariant: degraded (<n> passes in a row could not read the streams: <reason>)` when passes keep failing. A stopped service answers `degraded: capture_invariant: stopped` and stays ready, because it is the site shutting down. **What to do**: for a degraded line, check that the journal streams are readable (storage first), then read the findings of the invariant. Do not restart the host for a finding.
- **`ReportReadiness`**: name `reports`. The detail is the report admission gate's own status line (how many report runs are admitted and waiting). It never makes the site not ready; a gate that is full means reports are being refused with a retry hint, not that anything is broken.
- **`EdgeOfflineReadiness`**: reports the offline standing of an edge: the identity lease standing, the remaining ids of each leased series range and whether the carrier is wired. Read it before an edge goes offline: a lapsed lease or an exhausted series range is refused while offline.
- **`KmsReadiness`**: the customer-managed-key provider of `[services.secrets]`. An empty detail means the KMS answers. `degraded: the KMS is not serving (secrets_kms.<reason>); cached data keys serve for <n> more seconds, then sealed values are refused` is the grace window; after it, `degraded: the KMS is not serving (secrets_kms.<reason>) and sealed values are being refused`. **What to do**: restore the KMS or its credentials before the grace window ends; sealed values come back by themselves when the provider answers again.
- **`SyncLoopsReadiness`**: the sync loops of the site. The line shows the state of the loops; a failing step adds `; <step>: <reason>`, and a loop the host could not wire adds `; not wired: <names>`.
- **`SyncNodeReadiness`**: the edge's sync service (`sync_node`): the line shows the node's last sync, or why nothing runs when the node is not wired.
- **`PolicyReadiness`**: the hosted sync policy (`sync_policy`). A refused policy generation makes the check not ok (`503`); the site keeps running on the last good generation, or with nothing syncing. **What to do**: read the refusal in the detail, fix the metadata that declares what syncs, and swap it again.

### a site that failed to start

A site whose services or database could not start adds **no** entry: its checks are missing, and requests to it get `503` with `server.unavailable` and no reason (the reason is printed at start-up and on reload). On a host with several sites the endpoint can answer `200` while one site is down; compare the entries with the sites you expect.

- **Detail texts**: <!-- detail: the site was built without its services -->`the site was built without its services` is the internal detail of a runtime built for a reload; it never reaches a client.
- **What to do**: read the host's start-up output for `site `<name>` is unavailable`; fix the cause; `spot reload`.

## Decision table

| What you see | Read |
| --- | --- |
| `503` and `storage:<site>` is false | storage |
| `503` and `metadata:<site>` is false | metadata |
| `200`, `metadata:<site>` says `degraded:` | metadata |
| `503`, entry named `<site>`, "required service ... is stopped" | the services of the site |
| `200`, entry named `<site>`, `degraded: mail (...)` | the services of the site |
| `200`, `identity-audit:<site>` says `WARNING` | identity-audit (chain) |
| `503`, `identity-audit:<site>` says `BROKEN` | identity-audit (chain) (strict mode) |
| `200`, `identity-audit:<site>` says "NOT tamper-evident" | identity-audit (file only) |
| `200`, `partition:<site>` says `degraded:` | partition snapshot |
| `200`, `participants:<site>` lists fewer participants than expected | participants |
| `503`, `checks` is empty | draining: the host is shutting down |
| `503`, a check says `timed out after 2s` | the dependency of that check is slow; read its section |
| a site you expect has no entries | a site that failed to start |
| `/api/health/live` is `200` but `ready` is `503` | any not-ready row above; `live` is process liveness only |

## What the endpoint does not tell you

It is not an authentication or a data-integrity check. A `ready` site can have a lagging replica
or a stale partition snapshot (that is `degraded:`, not `503`). The identity-chain check verifies a
page per call, so a break far back can take a day to show; run `spot audit verify`. `/api/health/live`
answers `200` as long as the process runs and runs no check.
