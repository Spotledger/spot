# ADR-062: Sync charter and architecture (series overview)

- **Status:** Accepted by the architecture owner, 26 Oct 2026, together with ADR-061 to ADR-067 as a group. Proposed by the lead, 2 Oct 2026. Open questions that state a proposal or starting value are accepted at that value (thresholds stay tunable); the open questions that stated none were answered on 26 Oct 2026 (ADR-062, "Open questions answered after acceptance").
- **Date:** 2 Oct 2026
- **Layer:** Platform (extends ADR-23, which stays in force)
- **Builds on:** ADR-11, ADR-23, ADR-34, ADR-053, ADR-056, ADR-058, ADR-061
- **Series:** ADR-062 (this), ADR-063 policy and ownership, ADR-064 dependency closure, ADR-065 obligations and freshness, ADR-066 catch-up and version skew, ADR-067 self-healing
- **Raised by:** the architecture owner, 2 Oct 2026. The first multi-branch case is head-office master data (items, taxes) replicated to every branch, branch transactions flowing up, and branches that may be offline for a day or two and must then catch up. "Guaranteed sync" is a real problem for financial transactions. A document must always travel with its dependencies. The system should be rich in features without burdening the user, and should heal itself.

## Context

ADR-23 fixed the direction: sync is application-level, not database replication. It set the ownership model (every record has one owning node and flows only from it), the sync unit (a submitted document version with its child rows and ledger entries), and the mechanics (outbox transport, at-least-once delivery, idempotent apply, quarantine, scoped subscriptions, daily reconciliation). It also recorded that "details are to be decided later". ADR-053 added the safe-bookmark feed and causal order. ADR-058 reuses the same apply and quarantine path for links between tenants. ADR-061 makes the stream complete and durable.

What remains undecided is the part that decides whether sync is trustworthy in practice:

- which data may move, in which direction, and who decides (**policy**);
- how a document always arrives with what it depends on (**dependency**);
- how the system knows that a mandatory change reached every node, and what a stale node may do (**obligations and freshness**);
- how a node that was away catches up, and how nodes on different software versions keep talking (**catch-up and skew**);
- how all of this finds and fixes its own problems and shows an operator only what needs a human (**self-healing**).

This is a large area. It is split into the six decisions listed above, because each can be settled and tested on its own. This ADR fixes what they share.

**Scope boundary.** These are **platform** ADRs. They speak of generic records, owners, scopes and policies. Which Foundation or ERP document types use which policy (items, taxes, invoices, transfers) is for the Foundation and ERP layers to declare when those stages open (ADR-060). Where an example uses an item or an invoice, it is an illustration, not a design.

## Decision

### 1. Vocabulary

- **Tenant, site.** One tenant is one site and one logical database (ADR-34).
- **Node.** A runtime with its own database and its own stream: the **hub** (the head office site) or an **edge** (a branch, plant, warehouse or any location that keeps working without the hub). An edge's `source_id` is `<site>#<partition>` (ADR-056). A tenant's **network** is its hub plus its edges.
- **Origin and destination.** For a given record, the origin is its owning node, and a destination is any node that receives a copy.
- **Mirror.** A record held by a node that does not own it. A mirror is read-only to local users and carries its origin (ADR-058).
- **Scope key.** An opaque `(key type, value)` pair that a node holds, for example `(company, C1)` or `(warehouse, W7)`. Apps register key types. The platform never interprets them. A node's scope keys decide what it owns and what it receives (ADR-063).
- **Bundle.** A root record version plus its dependency closure and derived records, applied as one transaction (ADR-064).
- **Obligation.** A promise that a class of change reaches a set of nodes within a deadline, and the evidence that it did (ADR-065).
- **Condition.** A named, detectable abnormal state with a detector, an automatic response and an escalation (ADR-067).

### 2. Sync is a consumer of the stream

There is **no separate sync pipeline**. A node's outbound data is its **stream** (ADR-061), and a destination's sync consumer reads that stream and applies it. This gives, with no extra mechanism:

- a single source of truth for what changed (the `doc_state` events, ADR-061);
- one apply path (the replication apply path, WP-EDG-06): store the record at its version, without business logic, side effects or new events;
- replay, rebuild and sync as one family of operations: rebuilding a database from the journal, bootstrapping a new branch, and catching up an offline branch are the same machinery at different sizes (ADR-066).

### 3. Topology: hub and spoke

- **Hub and edges only.** Every edge talks to its hub and to no other edge. Cross-edge flows (a stock transfer between branches) are relayed by the hub, and each leg is owned by its node (ADR-063). A mesh is not supported: it multiplies the failure modes and the ordering rules for a benefit no case needs.
- **Edges always initiate.** Every connection is outbound from the edge, over TLS with node identity (ADR-23). The hub never dials an edge. A long-poll keeps the downward direction responsive. This works behind NAT and restrictive networks with no inbound ports at the branch.
- **Between tenants,** the tenant link (ADR-058) uses the same stream, the same apply path and the same conditions, with grants in place of scope keys.

### 4. Six guarantees

The series is judged against these. Each later ADR says which it provides and how it is tested.

| # | Guarantee | In plain words |
| --- | --- | --- |
| G1 | **Ownership.** A record is changed only by its owner, and the receiver rejects anything else | There are no conflicts to resolve, because there is one writer |
| G2 | **Completeness.** Every change in a node's scope reaches it eventually, or the system says loudly that it has not | Nothing is silently dropped |
| G3 | **Integrity.** A record is applied with its dependencies, atomically, or not at all | A node never holds half a document |
| G4 | **Convergence.** Duplicates, reordering of batches, replays and retries do not change the result | Running it twice is harmless |
| G5 | **Visibility.** At any moment the system can state, per node and per obligation, what has been applied and what is outstanding | You can prove who has the new tax rate |
| G6 | **Self-healing.** Every failure mode has a bounded automatic response, and a human is asked only when automation is exhausted | The system fixes what it can and tells you plainly what it cannot |

What the series does **not** promise, so nobody assumes it: instantaneous consistency across nodes, a global commit order, exactly-once delivery (the platform provides at-least-once delivery with idempotent apply, which gives exactly-once *effect*), or that an offline node's local sales follow rules it has not yet received. ADR-065 deals with that last case explicitly.

### 5. Layers and where each is decided

```
  ADR-061   Stream and journal          what happened, durably, in order
  ADR-063   Policy and ownership        what may move, in which direction, to whom
  ADR-064   Dependency closure          nothing arrives without what it needs
  ADR-065   Obligations and freshness   proof that mandatory change landed; stale-node rules
  ADR-066   Catch-up and version skew   offline nodes, new nodes, mixed software versions
  ADR-067   Self-healing                detect, repair, reconcile, show
```

Each layer uses only the one below it and the stream, so each is testable alone.

### 6. Design rules that keep it simple for the people who use it

These bind every later ADR and every work package in the series.

1. **Nothing syncs unless declared.** The default sync policy is `local`. A document type opts in, and does so by choosing one of a few **presets** (ADR-063), not by writing rules.
2. **Few knobs, safe defaults.** An administrator sees a handful of settings per class of data (how stale a branch may be, what to do when it is), each with a default that is safe and explained in plain language. Everything else is metadata that app authors set once.
3. **Lints before runtime.** Inconsistent sync policy (a synced record linking to a record that never syncs, for example) is rejected when the metadata is saved or the app installed, never discovered in production.
4. **One health surface, three colours.** Operators see green, amber and red per node, each with a sentence in plain language. Amber means the system is handling it, red means it needs a person (ADR-067).
5. **Automatic first, manual always possible.** Every automatic repair is also available as an explicit, audited action, and none is required for normal operation.
6. **Never destructive without a person.** Automation may resend, rebuild a mirror from its owner and retry. It never edits an owner's posted financial record, never deletes data and never crosses ownership (ADR-067).
7. **The system explains itself.** Every condition, repair and decision is written as an audit event (ADR-061), so an operator or the support agent (WS13) can answer "what happened to this record on that branch?"

### 7. Dependencies on other work

- **Per-branch numbering** (WP-EDG-03): each edge allocates document numbers from its own series or range, so numbers never collide. This series assumes it and adds one condition (a range running low, ADR-067).
- **Ids** are caller-chosen UUIDv7 (ADR-33), so a record created offline never collides with another.
- **Metadata versions and migration epochs** (WP-MIG-12) define the version skew that ADR-066 negotiates.
- **Effective-dated master data** (ADR-065) needs a platform notion of a record becoming active by clock, not by message. The metadata layer provides it (WP-MET).

## Later ADRs in the series (not written yet)

Listed so the open topics are visible and owned. Each needs the foundation above first.

| Topic | Origin | Why separate |
| --- | --- | --- |
| Shared balances (loyalty, gift cards, customer credit, vouchers): online-only, allocated quota or accept-and-reconcile | ADR-23's open table, WP-EDG-09 | Money-like state that two nodes may both spend. It needs the obligations model first |
| Cross-node stock visibility at the till: live lookup or "as of" view | ADR-23's open table, WP-EDG-04 | A read-model decision more than a sync one |
| Branch lifecycle: commissioning, replacement of a failed machine, relocation, decommissioning, key rotation | New | Mostly operational flows built on bootstrap (ADR-066) |
| Identity resolution for records created at several nodes (the same customer registered at two tills) | ADR-063's handover | The mechanism is in ADR-063. The domain rules belong to the Foundation party model when that stage opens |

## Proposed work packages (for the lead to add to the blueprint once approved)

The current EDG packages are kept. They are re-based on this series and extended.

| ID | Work package | Depends on |
| --- | --- | --- |
| WP-SYN-01 | Sync policy model, presets, scope keys and the policy lints in metadata (ADR-063); supersedes the contents of EDG-08 | MET-01, EDG-02 |
| WP-SYN-02 | Routing: per-destination filtered stream views and the scope-by-reference rule | SYN-01, JRN-09 |
| WP-SYN-03 | Bundles and dependency closure on the sender (ADR-064) | SYN-02, DOC-04 |
| WP-SYN-04 | Receiver apply with parking, pull-on-miss and the closure repair loop | SYN-03, EDG-06 |
| WP-SYN-05 | Acknowledgements, obligation ledger, deadlines and escalation (ADR-065) | SYN-02 |
| WP-SYN-06 | Freshness policy, sync stamps, stale-document findings, effective-date lead-time check | SYN-05, MET-01 |
| WP-SYN-07 | Catch-up planner: coalescing, dependency-ordered chunks, snapshot bootstrap (ADR-066) | SYN-03, JRN-05 |
| WP-SYN-08 | Version negotiation, per-record holds and upgrade-triggered release | SYN-07, MIG-12 |
| WP-SYN-09 | Conditions catalogue, reconciler loops, range digests, healing journal (ADR-067) | SYN-05, SYN-07 |
| WP-SYN-10 | Sync health surface and "needs you" list in the Admin Center | SYN-09 |
| WP-SYN-11 | Chaos and scale suite: cuts at every protocol step, duplicates, reorders, restored nodes, skewed clocks, 100 nodes returning at once | SYN-04 to SYN-09, TST-07 |
| WP-SYN-12 | Process-definition distribution to edges (added 26 Oct 2026, ADR-083 and the amendment of ADR-063) | SYN-02, BPM-09 |

## Consequences

- ADR-23 stays in force. This series fills its "to be decided" list and adds the dependency, obligation and healing layers. The blueprint's ADR-23 section needs a pointer here once the ADRs are approved.
- WP-EDG-02 (sync protocol) is built on the stream pull API (ADR-061) rather than defining its own transport. WP-EDG-06 (apply path and quarantine) is the shared apply path for sync, rebuild and tenant links.
- The ordering rule of ADR-053 (causal order within a source) is the foundation for bundles and catch-up. This series adds no new ordering promise.

## Open questions answered after acceptance (26 Oct 2026)

ADR-061 to ADR-067 were accepted as a group by the architecture owner on 26 Oct 2026, with every open question that states a proposal or starting value accepted at that value. The five that stated none were answered the same day, each as the lead recommended:

| ADR | Question | Answer |
| --- | --- | --- |
| ADR-061 | 4. Recovery time objective per tier | A tenant setting; start at 4 hours (standard) and 1 hour (premium); the journal audit measures it and flags a breach. Placeholders to tune |
| ADR-061 | 5. Who may hold `journal_reader` | The tenant administrator by default; may be granted to a named external principal (an auditor) with expiring credentials; every grant audited; off by default |
| ADR-065 | 5. Default `sync_admin` | A new role, separate from the tenant administrator |
| ADR-066 | 3. Offline seeding by signed file | Later; v1 bootstraps from a snapshot over the network |
| ADR-067 | 4. Failing simulator blocks merges | Yes, from the first sync work package |

No sync question is open now. Amendment 1 of ADR-063 (26 Oct 2026) is part of the accepted text.

## Alternatives considered

- **A separate sync log distinct from the stream.** Rejected: two sources of truth for "what changed", twice the failure modes, and no reuse of the journal, rebuild and audit machinery.
- **Peer-to-peer mesh between branches.** Rejected: ordering and ownership across a mesh need a coordinator anyway, and the hub already is one.
- **Database-level replication for sync.** Already rejected in ADR-23.
- **One very large ADR.** Rejected: the topics have different owners, different test strategies and different rates of change, and a single document would be reviewed superficially.
