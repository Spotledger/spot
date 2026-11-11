# ADR-061: The event journal: the stream as the recoverable record of a tenant

- **Status:** Accepted by the architecture owner, 26 Oct 2026, together with ADR-061 to ADR-067 as a group. Proposed by the lead, 2 Oct 2026. Open questions that state a proposal or starting value are accepted at that value (thresholds stay tunable); the open questions that stated none were answered on 26 Oct 2026 (ADR-062, "Open questions answered after acceptance").
- **Date:** 2 Oct 2026
- **Layer:** Platform (amends ADR-053 and ADR-056 additively; no frozen contract changes)
- **Builds on:** ADR-11, ADR-23, ADR-34, ADR-47, ADR-51, ADR-053 (outbox, change feed), ADR-055 (value encoding), ADR-056 (event and outbox schema), ADR-058 (residency), SEC-03 (`spot-secrets`)
- **Series:** this is the first of seven. The sync series that builds on it is chartered in ADR-062.
- **Raised by:** the architecture owner, 2 Oct 2026: the platform should behave like an engine control unit that emits data continuously. If a business loses its entire database, the event history alone must be enough to rebuild it. Events also decouple the platform from any one database and let other systems consume the same history.

## Context

ADR-11 writes every business event to an outbox in the same transaction as its document. ADR-053 publishes the outbox as a feed with safe bookmarks, and ADR-056 gives events a schema and an envelope. Today that machinery is a **notification channel**. The database is the truth, and events tell consumers that something happened. Three gaps stop it from being a recoverable record:

1. **Lifecycle events are thin.** `doc_updated` carries the document name and the changed fields. A consumer must read the document back for anything else, so the events alone cannot rebuild a database.
2. **Nothing keeps the stream.** The outbox is read, published and eventually trimmed. There is no durable, independent, immutable copy. A lost database loses its events with it.
3. **Not every important thing is an event.** Logins, permission denials, metadata changes, key rotations, rejected commands and job runs are logged or not recorded at all. They are not part of a replayable history.

The architecture owner's direction is to close all three. This ADR makes the stream complete, keeps it off the database's failure domain, and defines how a tenant is rebuilt from it. It also settles how the platform relates to Kafka-style systems, and it sets up branch sync (ADR-062 to ADR-067), which is the same machinery applied to a different destination.

### Decisions taken with the architecture owner in discussion (2 Oct 2026)

| Question | Decision |
| --- | --- |
| Is the database the truth, or the log? | **Design for A, open for B.** The database stays the serving store. Every write commits with its event in one transaction (ADR-11), and the stream is complete enough to rebuild the database. The design must not rule out making the log the only write path later. |
| How much does a state event carry? | **The full state** of the document at that version. |
| Zero loss or bounded lag? | The platform must not lose data in a disaster. Durability is a **per-tenant policy**, chosen from a short list (below). |
| Which system events must be durable? | Those whose loss would change what can be proven or rebuilt. Operational telemetry may be lossy. |
| Is Kafka part of the stack? | **No, never a dependency.** The contract is Kafka-shaped so that a bridge is cheap. |

## Decision

### 1. One stream per source, and the journal is its sealed copy

- A **source** is a producing database (ADR-056): a tenant's site, or `<site>#<partition>` for a branch edge. Each source has **one stream**: its outbox feed, in publication order, with positions `(generation, seq)` (ADR-053). All event classes below live in this single stream, so a source has one total order.
- The **journal** is the durable, immutable, off-database copy of that stream, kept in object storage in the tenant's region (ADR-058). It is made of **segments**, **tail chunks**, **snapshots** and a **manifest** (section 5).
- The journal is the outbox stream, sealed. There is no second pipeline. Anything a consumer can read from the live feed is in the journal, and the reverse.

### 2. Event classes

Every topic belongs to exactly one class. The class is part of the topic's registered schema (ADR-056's registry gains a `class` attribute; this is an additive change).

| Class | What it is | Guarantee | Examples |
| --- | --- | --- | --- |
| **state** | The full state of one record after one committed transaction | Lossless, in the journal, replayable, covered by the durability policy | `doc_state` (one topic, see section 3) |
| **domain** | A business or app fact for consumers to react to | In the journal, replayable | `doc_posted` and the other ADR-56 lifecycle topics, app topics such as a threshold-crossed event |
| **audit** | A system fact that matters for proof or rebuild | In the journal, replayable; written in the same transaction as the cause where there is one, otherwise in its own small transaction | `sys_login_failed`, `sys_permission_denied`, `sys_role_changed`, `sys_meta_changed`, `sys_key_rotated`, `sys_command_rejected`, `sys_export_run`, `sys_job_effect`, `sys_sync_*` (ADR-065, ADR-067), `sys_license_changed` |
| **telemetry** | Diagnostic and performance signals | **Not** in the journal. Lossy, sampled, short retention, owned by `spot-telemetry` | request timings, queue depth, cache statistics, health, debug traces |

The test that separates audit from telemetry: **if losing the event would change what can be proven or rebuilt later, it is audit; if it only helps diagnose the present, it is telemetry.** The `sys_` prefix is reserved by ADR-056 and is used for audit topics. Telemetry is never written to the outbox, so its volume cannot hurt the stream.

Subscribers (including external consumers, section 7) can ask for any subset of classes. A consumer that wants only business facts never sees audit traffic.

### 3. State events

- **One topic, `doc_state`.** The `subject` of the envelope is the document (entity and id), and `subject_version` is the version the event describes. The payload `data` holds:
  - `op`: `insert`, `update` or `delete`, the net operation in the transaction (the same meaning as `ChangeOp` in ADR-053);
  - `entity_schema`: the catalog (metadata) version the image was written under;
  - `image`: the **whole document**: every declared field and every child row, in the native value encoding (ADR-055). For `delete`, the image is the last committed version, so the history keeps what was removed.
- **Net per transaction.** There is exactly one `doc_state` per document per committed transaction, at its final version, matching the change feed (ADR-053). A document created and deleted in one transaction produces none.
- **Full images, not deltas.** A state event is self-sufficient. Replay applies images in order, last version wins. A missing intermediate version does not break replay, and a consumer can safely **coalesce** a long backlog to the latest image per document. This property is used heavily by sync (ADR-066).
- **What is captured.** Every record written through the storage contract is captured, including child rows (inside their parent's image), ledger entries and metadata documents. Two kinds of record are excluded, and both exclusions must be explicit:
  - **Derived entities**, flagged `derived` in the catalog by their owner (balance tables, caches, search and graph indexes, the analytics mirror, ADR-47). They are rebuilt by projectors after a rebuild, and a derived flag with no registered projector is a catalog error.
  - **Local-only entities**, flagged by their sync policy as `local` (ADR-063), such as sessions or user interface state, only where the owner states they are not part of the recoverable record. The default is captured. Only the owner can exclude, and every exclusion appears in the tenant's recovery report (section 8).
- **Attachments** are not embedded. A file field carries its content hash and the file provider keeps content-addressed blobs. The journal manifest records which blobs each position references, and the file provider's backup must cover those blobs (section 9).
- **Secrets** are captured in their stored, encrypted form. The journal never holds a plaintext secret and never decrypts one (`spot-secrets` redaction rules apply to events as they do to logs).
- **Personal data** fields (flagged `personal` in metadata, with a purpose and a retention class) are encrypted in the image under a **per-subject key** (section 6). Destroying the key makes those fields unreadable in the journal and in every snapshot.
- **Where capture lives.** One platform write path captures state: the document engine, over `spot-storage`, enqueues one `doc_state` per document per transaction at commit, coalescing repeated writes. Nothing writes records except through that path (migrations and bulk loaders use it too). The **consistency invariant** is checked continuously and in CI: *every entry in the change feed has exactly one `doc_state` event of the same document and version.* The change feed is storage's own account of what changed, so it is an independent witness against a missed capture.
  - The storage contract stays at 0.1.0. If measurement shows double-writing costs too much, the backend can emit the image itself from the change feed (a contract 0.2 change) without changing the journal format.

### 4. Durability policies

"Zero loss" is a claim about **failure domains**. A commit that is not acknowledged until the event exists in a second failure domain cannot lose it when the first one dies. Because the outbox row commits in the same transaction as the data, replicating the database also replicates the events. So the policies are:

| Policy | Commit is acknowledged when | Survives | Cost | RPO |
| --- | --- | --- | --- | --- |
| `local` | The local database commit is durable | Process crash | None | Journal shipping lag, if the machine and disk are lost |
| `replica-sync` | A synchronous standby in another zone has the commit (database replication) | Loss of one zone, node or disk | One zone round trip | **Zero** for zone loss; journal lag only for loss of the whole region or a logical disaster |
| `remote-sync` | The entry is also durable in the journal's object store, as a tail chunk (section 5) | Loss of the entire site | One object-store round trip per commit batch (group commit) | **Zero** |

- The policy is a **per-tenant setting**, held by the control plane (ADR-34). It is not per-tenant infrastructure: the machinery is shared per node (section 10).
- **Defaults.** SaaS tenants run `replica-sync`. A standalone or on-premises tenant with a configured journal target runs `remote-sync` with `on_unreachable: degrade`. A tenant with no journal target is `local`, and the platform shows a standing warning that there is no off-site history.
- **`on_unreachable`** says what `remote-sync` does when the object store cannot be reached: `degrade` (commit locally, raise an RPO-breach alarm, and re-ship as soon as possible) or `halt` (refuse commits). `halt` suits a tenant for which losing data is worse than stopping, and it is the tenant's choice.
- **Disaster classes** and what answers each:

| Disaster | Answer |
| --- | --- |
| Disk or node loss | Synchronous standby (`replica-sync`) |
| Loss of a site or region | `remote-sync`, or restore from the journal with a stated RPO under `local` or `replica-sync` |
| Logical disaster (bug, corruption, operator error) | The journal's immutable history, rebuilt to a chosen position (point in time) |

- A commit waiting on `remote-sync` that dies after the local commit and before the acknowledgement returns `CommitOutcomeUnknown` (ADR-053). The caller's `Idempotency-Key` (`command_id`, ADR-056) makes the retry safe.

### 5. Journal format and layout

- **Layout** (per tenant, in the tenant's region):

```
<tenant>/
  manifest/<generation>.json         ordered list of segments, ranges, hashes (signed)
  segments/<generation>/<first_seq>-<last_seq>.seg
  tail/<generation>/<batch_id>.chunk    remote-sync only; compacted into segments
  snapshots/<generation>/<position>/    base images (section 6)
  blobs/<hash>                          content-addressed attachments
```

- **Segment.** A sealed, immutable file of consecutive stream entries. The body is the ADR-056 `EventEnvelope` as JSON, one per line, compressed with zstd in blocks, each block with its own checksum. A footer carries a block index. The header names the source, generation, first and last position, entry count, event-schema versions present, and the **hash of the previous segment**. The hash chain makes a missing or altered segment detectable. The manifest lists the segments and is signed with the tenant's journal key.
- **Sealing.** A segment is sealed by size or by time, whichever comes first (defaults: 64 MB or 5 minutes, only when entries exist). This keeps idle tenants from producing empty objects.
- **Tail chunks** exist only for `remote-sync`. The relay writes each group-commit batch as a small immutable chunk before the commit is acknowledged, and compacts chunks into segments afterwards. They share the position index, so readers see one stream.
- **Format is versioned and documented as a public contract** (section 7). JSON lines keeps it readable by any tool. A columnar export for analytics (Parquet or similar) is derivable from it and can be added later without changing the journal.
- **Encryption.** Segments are encrypted with per-segment data keys wrapped by a tenant journal key (envelope encryption, `spot-secrets`). Key material is **never stored in the journal**. It lives in the key provider (SEC-03), with its own backup procedure, because a journal without its keys is unreadable by design (section 9).

### 6. Snapshots, rebuild, retention and erasure

- **Snapshots.** At intervals (default daily, and after every migration, WP-MIG-07), the platform writes a **base image**: every captured record as of a recorded position `P`, taken from a consistent read (the same mechanism as scoped snapshots in ADR-066). A snapshot is stored beside the journal and is verified by a rebuild test before it is trusted (section 8).
- **Rebuild** = latest valid snapshot at or before the target position, then segments after it, applied in stream order, then projectors for derived entities, then the invariant suite.
  - **The apply path** is the replication apply path of WP-EDG-06: documents are stored with their recorded versions, through the storage contract, **without re-running business logic, without side effects, and without enqueuing new events.** Causal publication order (ADR-053) guarantees a target is applied before any record that links to it. Within one transaction, deferred foreign keys accept any order.
  - **Positions are preserved.** The rebuilt database adopts the journal's positions. Its outbox continues at the next position, **every consumer's bookmark stays valid**, and the stream generation does not change (nothing was lost). Consumers that were behind simply carry on.
  - **Roll-forward.** Restoring a database backup and then rolling the journal forward to its end is the normal disaster path. It replaces today's "supervised re-entry" rescue (WP-MIG-10) as the primary tool, because the journal holds the full state of everything committed after the backup.
  - **Point in time** is the same operation with a target position. Rebuilding to *before* a corruption **forks** history: the stream gets a new generation, the old generation stays in the journal forever, and the manifest records the fork point. Consumers see `ResyncRequired` for positions in the abandoned generation (ADR-053), which is the existing behaviour.
- **Schema across time.** Each image records its `entity_schema` (catalog version). Rebuild applies images into the current schema through **image upcasters**: a patch that changes the stored shape of an entity must ship an upcaster for the images of the older versions (WP-MIG-02 gains a declared field, and CI rejects a shape-changing patch without one, like an undeclared patch). Rebuild-from-last-release's-journal is a CI test (WP-MIG-09 style).
- **Retention.** State and audit events are kept **indefinitely** by default (cold storage tiering is a deployment choice). Domain events follow their topic's `retention`, never shorter than the slowest registered consumer's horizon. Compaction (dropping superseded images) is **not offered in v1**: it loses history, and the saving is small against compressed segments.
- **Erasure.** A `personal` field is encrypted under a per-subject key held in the key provider, not in the journal. An erasure request destroys the key. Replays and snapshots then yield a documented placeholder for those fields, and referential structure is untouched. What may be erased, and what the law requires to be kept (accounting records usually must be), is **policy data on the field**, owned by whoever declares the field. The platform provides the mechanism, not the legal answer.

### 7. Kafka-shaped, never Kafka-dependent

- **No broker, no Kafka client, no Zookeeper or KRaft anywhere in a profile's critical path.** A standalone machine, a branch edge and a SaaS cluster all run the same compact stack (ADR-053's compact profile).
- **The contract maps one to one onto Kafka's model**, so a bridge is cheap:

| Kafka concept | Ours |
| --- | --- |
| Topic-partition | One source's stream (ADR-056 `source_id`) |
| Record key | The `subject` (entity and id), so one document's events stay ordered |
| Offset | `position {generation, seq}` (the generation is an addition Kafka lacks; consumers resync on a new generation) |
| Message type | The envelope's `topic`, which is a header, not a Kafka topic |
| Consumer group, committed offset | A named consumer with a stored checkpoint (below) |

- **Three public surfaces**, all reading the same stream:
  1. **Stream pull API.** `fetch(after, limit, topics, classes, entities)` returns envelopes. Long-poll or server-sent events for tailing. A consumer registers a name and may let the server keep its checkpoint (applied position) or keep it itself. Authentication uses scoped tokens, and the **permission engine filters what each principal may see** (PLT-03, PLT-04): a consumer never reads a record or field its principal could not read through the API.
  2. **Journal access.** A trusted principal (a tenant administrator grants the `journal_reader` role) can read segments and the manifest directly from object storage through scoped, expiring credentials. This is unfiltered by design (it is the whole tenant), meant for the warehouse, the lakehouse and audit, and it puts no read load on the platform.
  3. **Webhooks and the realtime channel** (WP-INT-01, WP-PLT-08), unchanged.
- **Bridges are connectors, not core.** A Kafka bridge (and later NATS, Pulsar, Redpanda) is a connector in the WP-INT-05 framework: it reads surface 1 and publishes outward. A Kafka **wire-protocol gateway** is explicitly out of scope for v1 and is reconsidered only on customer demand.

### 8. Trust: proving the journal is complete

A journal that has never been replayed is a hope. The platform checks itself:

- **Capture invariant** (section 3), continuous: change feed against `doc_state`.
- **Chain and manifest verification**, continuous: every sealed segment is checked against its hash, its predecessor and the manifest.
- **Journal audit**, scheduled (default weekly; daily for tenants on the `always` replay policy of the migration engine): rebuild the latest snapshot plus the journal into a scratch database, run the invariant suite and compare the **financial fingerprint** (WP-MIG-06: Trial Balance, Stock Balance, AR and AP ageing) and the record counts with the live database. A mismatch raises `JOURNAL_AUDIT_MISMATCH` (ADR-067) and names the first differing record.
- **Recovery report**: a per-tenant page listing the journal target and region, the durability policy, the last verified position and time, the oldest snapshot, the key backup status, and every excluded entity with its owner's reason. It is the answer to "could I recover?" at any moment.

### 9. What the journal alone does not recover

Be explicit, because "rebuild everything from object storage" must not mislead:

- **Key material.** Without the journal key and the per-subject keys there is nothing readable. The key provider's backup is part of the recovery plan, and the recovery report shows its status.
- **Attachment blobs** are referenced, not embedded. They need the file provider's own durable store and a cross-check that every referenced hash exists (part of the journal audit).
- **Control-plane data** (tenant map, licences, node identities) belongs to the control plane (WS18) and has its own backup.
- **External effects already sent** (e-mails, bank files, fiscal transmissions) are not undone by a rebuild. Replay never resends side effects (the apply path enqueues nothing), and an audit event records each external effect so a rebuilt tenant knows what already left.

### 10. Operating many tenants without per-tenant machinery

The stream and journal are **logical per tenant**. The machinery is **shared per node**: one relay process per runtime node drains the outboxes of every tenant it hosts, batches across tenants (group commit), and applies per-tenant fairness (ADR-34). A tenant's cost is a bucket prefix, a key, a checkpoint row and its segments. At the default sealing interval, 100 tenants produce about 29,000 small objects a day, which is negligible. Dense hosting stays dense. Load tests must show the relay holds a fleet-sized tenant count at the declared commit rate (WP-TST-07).

### 11. Option B stays open

The design does not rule out making the stream the only write path (**event-sourced** operation, with the database as a pure projection, for example a service on its own SQLite store). The pieces that keep it open are all decided above: full-state events, one total order per source, caller-chosen ids (ADR-33), version-checked writes (ADR-053), idempotent apply keyed by id and version, and positions that survive a rebuild. What B would additionally need is a **single sequencer per stream** (one writer per tenant is already the model) and a read-your-writes rule. Neither is designed here, and this ADR changes nothing about PostgreSQL being the only supported primary backend (ADR-53, STO-05 retired). A projection into another store, such as a SQLite service that consumes a filtered stream, is possible today through surface 1.

## Consequences

- **ADR-056** gains the topic `class`, the `doc_state` topic family reservation and the `sys_` audit topics. All are additive and registry-level. The envelope is unchanged. The `events` contract crate gets a follow-up task (class attribute and `doc_state` payload schema), under the usual compatibility checker.
- **ADR-053** is unchanged. Its change feed becomes the witness for the capture invariant, and its stream-generation concept carries fork semantics.
- **WP-STO-08** (outbox and relay) gains: stamping `source_id`, the journal relay (tail chunks, segments, manifest), retention coordinated with the journal rather than with consumers alone, and the failure scenarios of the Event Delivery report as release gates.
- **WP-DOC-04** (document engine) captures `doc_state` through the single write path and carries `command_id`.
- **WP-MIG-05, MIG-06, MIG-09, MIG-10** are touched: snapshots become journal snapshots, replay fingerprints are reused by the journal audit, image upcasters join the patch contract, and roll-forward becomes the primary rescue.
- **WP-EDG-02 and the other sync packages** are reframed by ADR-062: sync consumes this stream.
- **WP-INT-02** (outbound streaming) becomes the stream pull API of section 7, and gains the journal access surface.
- **WP-SEC-03** (`spot-secrets`) provides the journal key hierarchy and per-subject keys.
- **Admin Center (WS22)** shows lag against retention, the recovery report and the journal audit result.
- **Not changed:** PostgreSQL is the only backend, ADR-11's transactional rule, the ownership model of ADR-23, and the frozen storage contract.

## Proposed work packages (for the lead to add to the blueprint once this is approved)

| ID | Work package | Depends on |
| --- | --- | --- |
| WP-JRN-01 | State event schema (`doc_state`), event classes and registry `class` attribute in the events contract | CON-04 |
| WP-JRN-02 | Capture path in the document engine and the capture invariant (change feed against `doc_state`) | JRN-01, DOC-04, STO-08 |
| WP-JRN-03 | Journal relay: tail chunks, segments, hash chain, signed manifest, per-tenant keys, region placement | JRN-01, STO-08, SEC-03 |
| WP-JRN-04 | Durability policies: `local`, `replica-sync`, `remote-sync`, `on_unreachable`, control-plane setting | JRN-03, CTL-01 |
| WP-JRN-05 | Snapshots, rebuild tool, positions preserved, roll-forward, point-in-time and fork | JRN-03, EDG-06, MIG-05 |
| WP-JRN-06 | Image upcasters in the patch contract and the rebuild-from-previous-release CI test | JRN-05, MIG-02 |
| WP-JRN-07 | Journal audit (scratch rebuild, invariants, fingerprint compare) and the recovery report | JRN-05, MIG-06 |
| WP-JRN-08 | Personal-field encryption, per-subject keys and erasure | JRN-03, SEC-03 |
| WP-JRN-09 | Stream pull API and journal access surface | JRN-03, INT-01, PLT-03 |
| WP-JRN-10 | Kafka bridge connector | JRN-09, INT-05 |
| WP-JRN-11 | Relay load test across a fleet-sized tenant count; chaos scenarios for the journal | JRN-03, TST-07 |

## Open questions for the architecture owner

1. **Default for `remote-sync` when the object store is unreachable.** Proposed: `degrade` for every tenant by default, because most would rather keep trading than stop, and `halt` as an explicit choice. Confirm.
2. **Who owns the legal retention and erasure policy per field?** Proposed: the app or Foundation layer that declares the field, with the platform enforcing the mechanism only. Confirm that the platform ships no legal defaults.
3. **Journal encoding.** Proposed: JSON lines with zstd now, columnar export later. If you expect heavy warehouse use from day one, say so and we decide the columnar form earlier.
4. **Snapshot cadence default** (daily, plus after migrations) and a stated **recovery time objective** per tier, which the journal audit measures.
   - **Answered by the architecture owner, 26 Oct 2026 (accepted as recommended):** The recovery time objective is a tenant setting, starting at 4 hours for the standard tier and 1 hour for premium; the journal audit measures it and flags a breach. The values are placeholders to tune, not commitments.
5. **Should `journal_reader` be grantable only to a tenant administrator, or also to a named external principal** (an auditor's account) with expiring credentials?
   - **Answered by the architecture owner, 26 Oct 2026 (accepted as recommended):** `journal_reader` is held by the tenant administrator by default. The administrator may grant it to a named external principal (an auditor) with expiring credentials. Every grant is audited and the grant is off by default.

## Alternatives considered

- **Kafka (or Redpanda, Pulsar) as the stream's home.** Rejected: it would put a broker in the critical path of every deployment, including a single machine, and a broker is fed after commit, which is the dual write ADR-11 forbids. The throughput it exists for is not the platform's constraint.
- **Object storage alone for zero loss.** Rejected as the sole mechanism: asynchronous shipping cannot give zero loss. Zero loss comes from synchronous durability in a second failure domain (standby or tail chunks). Object storage provides immutable history and independence from the database.
- **Event sourcing (B) now.** Rejected for v1: it reopens uniqueness, gapless numbering, locks and read-your-writes, all settled in ADR-46 and ADR-53, for a benefit (databases as pure projections) that no current requirement needs. The design keeps the door open.
- **Deltas instead of images.** Rejected: replay would depend on every intermediate event, coalescing would be unsafe, and a missed event would corrupt everything after it. Storage and bandwidth are the price of images, and compression plus the full-state benefits outweigh it.
- **Command replay.** Rejected as the rebuild mechanism: it needs deterministic business logic, clocks, number allocation and external calls. Commands remain in the audit class for proof, but recorded effects, not intentions, are replayed.
- **Putting capture in the storage contract (change feed with images).** Deferred, not rejected: it would reopen a frozen contract. The platform write path plus the capture invariant achieves the same result today.

## Amendment 2026-10-31 (lead, under the architecture owner's full authority of 31 Oct 2026)

Decisions A1, A2, B9, D1 to D6 of the decision log. Nothing above is removed; this adds to sections 1, 2 and 5.

**A.0 Positions are per generation (A1, section 1).** A position is the pair `(generation, seq)`; `seq` restarts at 1 in every generation and is meaningful **only together with its generation**. A fork opens a new generation (section 6). Everything that orders, bounds, acknowledges, plans or audits the stream uses the pair: the rebuild planner and executor walk `(generation, seq)`; `Target::At`, the acknowledgement gate, the audit facts and `audit-run` carry the generation. A bare `seq` identifies nothing. Continuity (`first_seq = previous last_seq + 1`) is checked only inside one generation. Evidence and the list of changes: `docs/research/journal-position-numbering-across-forks.md`, section 5.1.

**A.0.1 Domain events and rebuild (B9, section 2).** "Replayable" in section 2 means a Domain event is available to a consumer from the journal; a rebuild applies State events and does not apply Domain events (replaying them would run consumers' reactions twice). The class of a topic comes from the events contract registry, not from a topic prefix (`docs/research/event-class-registry-proposal.md`).

**Appendix A: the tail chunk (D1 to D6, section 5).** Referenced from the `tail/` line of section 5. Version 1 is the format the merged relay writes (`SPOTJTL1`); version 2 (`SPOTJTL2`) is the format every new chunk is written in once WP-JRN-03.T11 lands. A reader reads both.

- **A.1 Object.** One chunk is one `remote-sync` group-commit batch, durable before the commit is acknowledged. Key `<tenant>/tail/<generation>/<batch_id>.chunk`, both numbers zero-padded to twenty decimal digits; written once (`put_new`), never replaced, deleted only by A.5. A reader that needs positions the manifest does not cover reads chunks; a reader that needs only sealed history does not.
- **A.2 Layout, version 1 (big-endian).** `"SPOTJTL1" u32(count) { u64(generation) u64(seq) u32(line_len) line }* [32](sum)`; `sum = SHA-256("spot.journal.tail.v1\0" || every byte before sum)`. The line is one `EventEnvelope` as one UTF-8 line, no CR or LF, at most 16 MiB. No padding, compression or signature. Version 1 is **unauthenticated and plaintext**; it is read for chunks already written and is never written after WP-JRN-03.T11.
- **A.2.2 Layout, version 2 (decisions D2 and D3).** `"SPOTJTL2" u32(count) { u64(generation) u64(seq) u32(ct_len) ct }* [32](mac)`. Each `ct` is the AEAD ciphertext (`spot-secrets`) of the envelope line under a data key wrapped by the tenant journal key, with the associated data `generation || seq || batch_id`, so an entry cannot be moved to another position or chunk. `mac = HMAC-SHA-256(k_tail, "spot.journal.tail.v2\0" || every byte before mac || the object key)` with `k_tail` derived from the tenant journal key for the purpose `tail-mac`; the object key is covered, so the generation and batch id in the name are protected. A reader verifies the MAC before it parses or decrypts anything. A chunk is one MAC per batch on the acknowledgement path; it is not a signature and gives integrity against anyone without the journal key, not non-repudiation.
- **A.3 Validity.** (a) the key is a canonical tail key of the generation read; (b) the object is at least 32 bytes and the last 32 equal the checksum (v1) or the MAC (v2); (c) the body starts with the magic and `count` consumes it exactly; (d) every entry carries the key's generation and `count >= 1` (a zero-count chunk decodes and is invalid); (e) inside a chunk `seq` is strictly consecutive; (f) across chunks in ascending `batch_id` the first starts at `manifest.last_seq() + 1` and each next one at the previous last `seq` plus one. Failing (a) to (e) is a damaged chunk; failing (f) is a gap; a chunk starting at or before `manifest.last_seq()` and ending after it straddles the manifest; all three are chain-broken conditions (A.6), never a reason to drop data.
- **A.4 Order and generations.** Entries ascend by `seq` inside a chunk and chunks by `batch_id`; a reader must not rely on the lexical order of a listing. A chunk belongs to exactly one generation: key prefix, every entry and the reading relay must agree. A generation whose successor's manifest exists is closed and takes no new chunks. `batch_id` is derived at relay start as `max(listing) + 1` (D4); there is no persisted counter and no manifest format change.
- **A.5 Compaction.** A chunk may be deleted only when a signed manifest of its generation covers its whole range. Order: (1) `put_new` the segment holding the entries; (2) `put_replace` the manifest with that segment appended, guarded by the etag read in (1); (3) delete the covered chunks. A crash leaves chunks behind, which is harmless. Compacted chunks are not retained (D5).
- **A.6 Reader after a crash.** covered (`last_seq <= manifest.last_seq()`): delete, ignore content. Straddling: refuse (A.5 cannot produce one, so someone edited the object; treat as tampering, never merge). Beyond the manifest: the only copy of those positions; read in ascending `batch_id`, require A.3(f), then seal it or read it in stream order after the manifest; present it as **unsealed** (no hash-chain link, no signature). Failing A.3 (b) to (e): refuse the generation and stop; never skip, repair or delete (an acknowledgement was already returned under `remote-sync`, so skipping loses an acknowledged commit; availability is traded for zero-RPO, D5).
- **A.7 Versioning and size.** The eight magic bytes are the whole version; a reader fails closed on a magic it does not know. A later format is a new magic and a new domain string. A reader of version N reads every earlier version still present. Maximum object size is **64 MiB** (D6): the relay caps a batch below it and **refuses** an oversize batch with a typed error; a batch is never split, because it is the unit of acknowledgement. "Readers see one stream" is implemented by a reader that applies A.6 (WP-JRN-05.T8).
