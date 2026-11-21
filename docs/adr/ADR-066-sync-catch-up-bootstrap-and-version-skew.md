# ADR-066: Offline catch-up, bootstrap, restored nodes and version skew

- **Status:** Accepted by the architecture owner, 26 Oct 2026, together with ADR-061 to ADR-067 as a group. Proposed by the lead, 2 Oct 2026. Open questions that state a proposal or starting value are accepted at that value (thresholds stay tunable); the open questions that stated none were answered on 26 Oct 2026 (ADR-062, "Open questions answered after acceptance").
- **Date:** 2 Oct 2026
- **Layer:** Platform (series ADR-062; generic)
- **Builds on:** ADR-23, ADR-34, ADR-053 (`ResyncRequired`, generations), ADR-056 (schema majors), ADR-061 (journal, snapshots, upcasters), ADR-062 to ADR-065, WP-MIG-12 (fleet rollout, migration epoch)
- **Raised by:** the architecture owner, 2 Oct 2026: branches go offline for a day or two when the internet is poor, "so they will need to do a lot of catching up".

## Context

An edge node is offline often, sometimes for long, and sometimes it disappears altogether (a failed disk, a replaced machine, a new branch opening). The hub may be restored from a backup. Branches upgrade software on their own schedule. Each of these situations must end with every node holding the right data and **nobody hand-copying files**.

ADR-053 gave the primitives: resumable positions, safe bookmarks, `ResyncRequired`, stream generations. ADR-061 added the journal, so history need not be kept in the hot database. ADR-064 made bundles dependency-complete. ADR-065 made applied state and obligations observable. This ADR assembles them into the **session protocol**, the **catch-up planner**, the **bootstrap** path, the handling of **restored and replaced nodes** and the rules for **mixed versions**.

## Decision

### 1. The sync session

A node synchronises in **sessions**. The edge always initiates (ADR-062).

1. **Handshake.** Mutual TLS identifies the node (ADR-23). The node sends its software version, metadata version, migration epoch, the event schema majors it can decode, and its **applied prefix** and stream generation per peer stream (ADR-065). The hub checks the node is `active` or `commissioning` (ADR-063) and not `suspended`.
2. **Plan.** The hub replies with a **plan** (section 2), the class horizons and the hub's current time (ADR-065).
3. **Transfer.** Down: the hub streams bundles (ADR-064) in the mandatory lane first, then bulk data. Up: the node pushes its stream in order. Each side applies bundles one at a time per source. After each applied batch the receiver **commits the applied prefix in the same transaction** as the data (an inbox committed atomically with the effects, as ADR-053 requires of consumers). A cut connection loses nothing and repeats nothing that matters.
4. **Acknowledge and close.** The applied prefix, holds and versions travel in the next request. The session ends when both sides report caught up, or a limit is reached (below).
5. **Long poll.** The node holds a connection open so the hub can push new mandatory events promptly. A branch with a poor link may configure a **sync window** (hours when bulk transfer is allowed). Mandatory events are exempt from windows.

**Resumability.** The only durable state is the applied prefix and the snapshot progress (section 3). A session can be cut at any point and the next one continues. Apply is idempotent (ADR-064 version rule), so a batch applied but not acknowledged is simply received again.

**Flow control.** Batches are bounded (default 2 MB or 500 records, whichever is smaller). A node declares a bandwidth profile (`normal` or `low`): `low` shrinks batches, compresses harder and defers bulk to its window.

### 2. The catch-up planner

On the hub, the planner takes the node's applied prefix `P`, the stream's current end and the node's scope, and chooses one of three modes.

| Mode | When | What is sent |
| --- | --- | --- |
| **Incremental** | The gap is small (default under 10,000 in-scope events) | Every in-scope event after `P`, in position order, as bundles |
| **Coalesced** | The gap is larger, or `P` is older than the hot database retention | The **latest image per record** in scope since `P`, delivered as dependency-ordered bundles (ADR-064 section 8) |
| **Snapshot** | The node is new, rebuilt, resynced (`ResyncRequired`), or the estimated coalesced size exceeds a fraction (default 40%) of a full scoped snapshot | A scoped snapshot at a consistent position (section 3) |

**Coalescing rules.**

- Only **state** events coalesce, because they are full images (ADR-061): the latest image of a record supersedes all earlier ones. A node holds the latest known state of its mirrors, not their history, which stays with the owner and in the journal.
- **Domain** events are delivered in full unless their topic declares `coalesce: latest_per_subject`. **Audit** events reach a node only when they are addressed to it (ADR-064 `sys_sync_missing`, ADR-063 adoption), never as a broadcast.
- The planner **orders coalesced images by dependency**, not by their original position (ADR-064). That is the correctness condition. Applying in position order could place a record before a newer record it references.
- **The mandatory lane is planned first.** Mandatory events in scope since `P` are delivered individually and in order, ahead of the bulk plan, so a tax change is applied within the first moments of a session whatever the backlog.

**Where the data comes from.** The hot database holds recent history, and the **journal holds all of it** (ADR-061). The planner reads from the hot outbox where it can and from journal segments beyond it. This removes the question "how long may a branch be offline?" as a hard limit: **there is no offline horizon**, only a cost. A branch gone for six months is bootstrapped by snapshot rather than replayed, and the planner decides by comparing costs, not by a configured cut-off. Exclusions for downward data (`excluded_fields.down`, ADR-063) are applied **at serve time** to images read from the journal. The journal stores originals.

### 3. Snapshot bootstrap

Used for a new branch, a rebuilt or replaced node, a resync, a policy backfill (ADR-063) and large gaps.

1. **Consistent point.** The hub takes a **consistent read** of every record in the node's scope as of a recorded feed position `S`. It uses a repeatable-read transaction together with the safe bookmark of ADR-053, or a journal snapshot rolled forward to `S` (ADR-061). Nothing committed before `S` is missed, and nothing after it is mixed in.
2. **The plan and its manifest.** The records are divided into **chunks in dependency order**: targets before the records that reference them, cycles in one chunk, each chunk within the closure cap (ADR-064). The ordered list of chunks, with sizes and digests, is the **snapshot plan** and is persisted at the hub for the life of the transfer.
3. **Resumable transfer.** The node stores the plan identity and its progress per chunk (`bootstrap_progress`). A cut resumes at the next chunk. A chunk applied twice is harmless (version rule). A digest mismatch re-requests that chunk.
4. **Finish.** After the last chunk the node sets its applied prefix to `S` and continues **incrementally from `S`**. Events after `S` that overlap rows already in the snapshot are applied idempotently.
5. **State during bootstrap.** The node is `bootstrapping`. It is **stale for every freshness class** (ADR-065) until it first confirms current, so each document type's stale policy applies. The user interface shows progress in plain terms. The node becomes `active` only when bootstrap completes and its first reconciliation passes (ADR-067).
6. **Sharing the work.** Many new nodes often share a scope. The hub may cache a snapshot plan keyed by *scope hash and position* and reuse it, regenerating only the last part. Large or remote branches can be **seeded offline**: the hub exports the plan as a signed file (the same chunk format) for the node to import from a disk or by a courier, then the node resumes incrementally. Seeding is an optimisation of transport. The protocol is unchanged.

### 4. Restored and replaced nodes

The hardest realistic failures are not network cuts. They are lost machines and restored backups. Each has a defined path.

| Situation | Detection | Resolution |
| --- | --- | --- |
| **New branch** | Node in `commissioning` | Bootstrap (section 3), then reconciliation passes, then `active` |
| **Replaced machine, same identity** (disk died; node restored empty or from an old backup) | Node's stream position or generation is behind what the hub recorded for it, or it is empty | **Bootstrap for mirrors, then origin recovery for the node's own records** (below). The node's stream starts a new generation, the hub records the fork, and the node's numbering series are advanced beyond the highest number the hub has seen from them (`SERIES_ADVANCE`), so numbers are never reused |
| **Hub restored from an older backup without journal roll-forward** | A node reports an applied prefix or record versions **beyond** the hub's stream end | The hub's stream starts a new generation. Nodes receive `ResyncRequired`. **Hub-owned records are not overwritten if a node holds a newer version.** That is a `NODE_AHEAD_OF_OWNER` condition (red) and a person decides (below) |
| **Hub restored by journal roll-forward (no loss)** | None needed | Positions are preserved, bookmarks stay valid, generation is unchanged (ADR-061). Nodes carry on |

**Origin recovery.** A node that lost its own data (a replaced machine) recovers its **own** records from the hub's copy, which the hub holds up to the node's last acknowledged upload:

- The hub sends the node's own records back as bundles flagged `origin_recovery`. The node, identified by the same node identity, applies them as its own records at their recorded versions. This is the only case where a node receives records it owns from the hub, and the receiver accepts it because the flag is signed by the hub and the sender's identity matches `origin_node`.
- What the hub never saw (transactions the branch made after its last upload and lost with the disk) is the branch's own loss. **Its size is bounded by its upload cadence and its own durability policy.** A branch that must not lose data can keep its own journal target (ADR-061 applies per source, so a branch edge may have one). The recovery report (ADR-061) states the exposure plainly.

**Node ahead of owner (the exception to ownership).** After a hub loss, branches may hold the **only surviving copy** of recent hub-owned records, because mirrors carry full images (ADR-061). Overwriting them from a restored hub would destroy the data the business needs. So:

1. The hub does not overwrite. It raises `NODE_AHEAD_OF_OWNER` and lists the records where a node's mirror version is above the hub's.
2. An authorised person can **import** them: the hub proposes the nodes' newer mirror images as **recoveries** in a review queue. Each is applied as a new hub-owned version only after approval, with the node and original version recorded.
3. Everything is audited. This is a deliberate, human-approved exception to "only the owner writes", used only to recover from the owner's own loss.

### 5. Version skew

Nodes are on different software and metadata versions (ADR-23: "branches within the allowed version skew keep syncing"; WP-MIG-12: skew limits and epochs).

- **Allowed skew** is a tenant setting (default: **one release** behind the hub). Each node reports `software_version`, `metadata_version`, `migration epoch` and the **event schema majors it can decode** in its handshake. The hub keeps them in `sync_node_state` (ADR-065).
- **Epochs.** A node never syncs against an **unverified migration epoch** (WP-MIG-12). While the hub or a node is mid-migration, sessions to it are refused with a reason, and the other side waits and retries.
- **Negotiated schema majors.** The hub serves each node only majors that node can decode. During a deprecation window the producer publishes both majors as separate events (ADR-056), and each node receives the one it can read.
- **Retirement is gated by the fleet, not the calendar.** A producer may stop publishing an old major only when every `active` node reports it can decode the new major (or has been suspended). Until then the condition `SCHEMA_RETIRE_BLOCKED` (amber) names the nodes in the way. This replaces a guessed deprecation window with a fact the hub knows.
- **Upward compatibility.** The hub accepts images in the previous major from older nodes and upgrades them with the image upcasters of ADR-061, so an old edge can keep uploading while the hub has moved on.
- **A node beyond the skew limit** (`NODE_TOO_OLD`, red): sessions are refused except for a minimal **upgrade channel** that offers the required version. The node keeps operating offline with its stale policies (ADR-065). On upgrade it resumes by catch-up. Nothing is lost, because the stream and journal retain it.
- **Per-record holds.** If an event cannot be decoded (an unsupported major reached a node despite the rules, or a record needs a metadata version the node lacks), **only that record is held**, not the stream. The applied prefix moves on, the hold is reported (ADR-065) and the obligation stays unsatisfied. After the node upgrades, the held records are re-requested by id and applied. Full images make this safe: a later version of the same record supersedes an earlier undecodable one.

### 6. Problem records: one taxonomy

Every record that cannot be applied lands in exactly one state. None blocks the stream, and none disappears.

| State | Cause | Resolution | Who |
| --- | --- | --- | --- |
| **Parked** | A dependency is missing (ADR-064) | Pull-on-miss repair, automatic retry on arrival | Automatic |
| **Held** | The node cannot decode it yet (version) | Released and re-requested after upgrade | Automatic once the node upgrades |
| **Quarantined** | Ownership violation, integrity or digest conflict, or repeated apply failure | Investigation | A person |
| **Circuit-broken** | The apply path itself failed on this record N times (default 5) for a reason other than a missing dependency | Moved to quarantine with the captured error | A person (and a bug report) |

Dependents of a problem record are parked behind it automatically (ADR-064), so partial state never appears.

### 7. Load: many nodes returning at once

A regional outage ending can bring 100 branches back in the same minute.

- **Admission control** per tenant: a limited number of concurrent catch-up sessions (default 8). Others receive `retry_after` with jitter. The **mandatory lane is served outside the limit** because it is small and urgent.
- **Fair scheduling** among waiting nodes, with nodes that are stale on a freshness class first.
- **Shared plans**: coalesced plans and snapshot plans are cached by `(scope hash, position range)` and reused across nodes with the same scope.
- **Per-session bandwidth caps** protect the hub's uplink.

## Consequences

- **WP-EDG-02** becomes the session protocol, and **WP-SYN-07 and WP-SYN-08** implement the planner, snapshot bootstrap, origin recovery, version negotiation and holds. Bootstrap shares its snapshot code with journal snapshots (WP-JRN-05).
- **WP-STO-08** and **WP-JRN-03** provide hot retention plus journal reads. Retention for the hot database is a tuning value (default 7 days) because the journal is the long-term source.
- **WP-MIG-12** supplies the epoch and skew limits. This ADR consumes them and adds the retirement gate.
- **Control plane** (WS18): the node registry gains lifecycle states (`commissioning`, `bootstrapping`, `active`, `suspended`) and the contact attribute.
- **Document engine and storage** (WP-DOC, WP-STO): a node-ahead check, series advance (WP-EDG-03), and recovery apply for `origin_recovery` and approved hub recoveries.
- **Admin Center** (WP-SYN-10): bootstrap progress, the held and parked lists, and the recovery review queue.
- **Tests** (WP-SYN-11):
  - **Coalescing correctness:** the state after a coalesced catch-up equals the state after a full replay of the same events.
  - **Cut at every step** of a session, a snapshot chunk and an origin recovery.
  - **Replaced machine:** a node wiped and re-seeded recovers its mirrors and its own records, never reuses a number and passes reconciliation.
  - **Hub restored older:** nothing a node holds is overwritten without approval, and a recovery imports it.
  - **Three versions old:** a node is refused, can still upgrade, and catches up afterwards.
  - **100 nodes returning together** stay within the admission limits and finish within the hub's budget.

## Open questions for the architecture owner

1. **Thresholds**: incremental up to 10,000 events, snapshot beyond 40% of a scoped snapshot, 8 concurrent catch-ups, 2 MB batches, 5 failures to circuit-break. Starting values to tune, or do you have a different scale in mind?
2. **Skew limit** of one release. Is your upgrade cadence likely to leave branches further behind?
3. **Offline seeding** (a signed file for the node to import): in v1 or later?
   - **Answered by the architecture owner, 26 Oct 2026 (accepted as recommended):** Later. Version 1 bootstraps a new node from a snapshot over the network.
4. **Origin recovery** treats the hub's copy of a node's records as the recovery source. Do you want it automatic (as proposed) or approval-gated for financial records?

## Alternatives considered

- **A configured offline horizon** (a branch offline longer than N days must be re-seeded by hand). Rejected: the journal removes the reason for it, and a cost comparison picks the cheaper path automatically.
- **Replaying every event after a long absence.** Rejected beyond small gaps: it is slower than a snapshot and offers the node nothing it needs, since full images make intermediate versions irrelevant to a mirror.
- **Applying coalesced images in position order.** Rejected: it breaks references (ADR-064). Dependency order is required.
- **Overwriting nodes from a restored hub automatically.** Rejected: after a hub loss the branches may hold the only copy of recent hub-owned data. A person decides.
- **Blocking the stream on a record that cannot be applied.** Rejected: one bad record would stop the branch. Parking, holding and quarantine isolate it, and its dependents follow it.
- **A fixed deprecation window for old schema majors.** Rejected: the hub knows exactly which nodes still need the old major, so retirement is gated on that.
