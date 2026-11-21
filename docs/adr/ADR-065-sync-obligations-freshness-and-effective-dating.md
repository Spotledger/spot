# ADR-065: Delivery obligations, acknowledgements, freshness and effective dating

- **Status:** Accepted by the architecture owner, 26 Oct 2026, together with ADR-061 to ADR-067 as a group. Proposed by the lead, 2 Oct 2026. Open questions that state a proposal or starting value are accepted at that value (thresholds stay tunable); the open questions that stated none were answered on 26 Oct 2026 (ADR-062, "Open questions answered after acceptance").
- **Date:** 2 Oct 2026
- **Layer:** Platform (series ADR-062; generic)
- **Builds on:** ADR-23, ADR-34, ADR-053, ADR-061, ADR-062, ADR-063, ADR-064
- **Raised by:** the architecture owner, 2 Oct 2026: "at system level we need to ensure that certain events are indeed consumed and updated in certain systems, like a tax update is mandatory sync across the branch network". Some branches will be offline for a day or two. "This guaranteed sync thing is actually a real problem in financial transactions."

## Context

At-least-once delivery with idempotent apply (ADR-053, ADR-064) gets a change to a node **eventually**. It does not tell anyone **whether** it got there, **when**, or what the node was allowed to do **in the meantime**. For a tax rate those are the questions that matter:

1. **Proof.** Which branches have rate v7 applied, which do not, and since when has each been out of reach?
2. **Deadline.** What happens when a branch has not applied it by the time it is due?
3. **Effect.** A branch that is offline on the day a new rate takes effect will keep selling at the old rate, however good the transport is. What may it do, and how do we find and correct the documents it posts?

"Delivered", "applied" and "in effect" are three different things (ADR-062 guarantees G2 and G5). This ADR makes each observable and sets the policy for the gap between them.

The honest limit: **a disconnected node cannot learn what it has not been told.** No protocol fixes that. What can be done is to send the change early enough, bound how long a node may run without confirmation, record exactly what each document was made under, and detect and report the exceptions. That is the design below.

## Decision

### 1. Acknowledgement: applied, not delivered

- Each node reports **what it has applied**, per peer stream, as an **applied prefix**: the highest position `P` such that every event up to `P` that is in the node's view has been applied, or is deliberately skipped because it is out of scope or stale (ADR-064 version rule). The sender's responses carry a `through` position even when a batch is empty, so a node advances its prefix across the gaps left by filtering (ADR-063).
- **Holds do not lower the prefix.** Records that are parked (ADR-064), held for a version (ADR-066) or quarantined are listed beside the prefix, with their oldest age. The prefix moves on, and the obligation ledger counts a mandatory record in a hold as **unsatisfied** (section 3).
- **Acknowledgements travel in the sync requests**, not as events. A pull or push carries the node's `applied_prefix`, `holds`, software version, metadata version, migration epoch and a freshness report (section 4). The receiving side keeps the **latest value per node** in a state table (`sync_node_state`: last seen, applied prefix per stream, holds, versions). A heartbeat request every 60 seconds while connected keeps `last_seen` current.
- **Audit events are written for transitions only**: an obligation satisfied (section 3), a node going silent or returning, a hold set or cleared, a version change. This keeps the journal (ADR-061) free of a heartbeat every few seconds from every node, and still records everything needed to prove what happened and when.
- **Both directions are symmetric.** Branch to hub (the branch's transactions) and hub to branch (master data) use the same acknowledgement mechanism, each side holding the other's `sync_node_state`.

### 2. Obligation classes

A policy's `obligation` (ADR-063) has two values.

| Class | Meaning | Default for |
| --- | --- | --- |
| `normal` | Delivered as soon as the node connects. Lag raises an alert (ADR-067) but no obligation is tracked | `master`, `handover` |
| `mandatory` | Each version creates an **obligation** for every destination in scope: it must be *applied* by a deadline, and the platform must be able to show evidence that it was | `record` (branch transactions travel up with a 24-hour window, so the hub always knows about a branch that stopped reporting) |

Administrators raise a class to `mandatory` where it matters (tax, price lists, anything with legal effect). Marking a whole bulk master `mandatory` is possible but rarely wanted. Obligations are cheap because they are **derived**, not stored per record (section 3).

### 3. The obligation ledger is derived

For a destination `D` and a source stream, the outstanding mandatory obligations are:

> mandatory events in `D`'s scope with position greater than `D`'s applied prefix, plus mandatory records in `D`'s holds.

Nothing per record is stored. The ledger is a query over the stream, `sync_node_state` and the policy. For each outstanding obligation:

- **Deadline.** `due_by = published_at + delivery_window` (policy default 24 hours). If the record has an `effective_from_field` (section 6), `due_by` is the earlier of that and `effective_from − safety_margin` (default 12 hours).
- **Escalation ladder** (all automatic, ADR-067): at 50% of the window the node shows as **amber** (information, no one paged). At the deadline the obligation is **overdue** (amber) and the node's contact receives the configured notification (mail). After a grace period (default 25% of the window, minimum 2 hours) it is **red** and the `sync_admin` role is notified with the list of affected nodes.
- **Priority.** Mandatory events are delivered **first** when a node connects: a **mandatory lane** is served before bulk data, so a tax update is not stuck behind 50,000 item updates.
- **Evidence.** When a node's applied prefix crosses mandatory events, the hub writes one `sys_obligation_satisfied` audit event covering the **range** (`node`, `through position`, `count`, `applied_at` as reported, `received_at` as observed). This makes evidence cheap even for large masters. A report answers the auditor's question: "show me that rate v7 reached every branch, and when": per node, either *applied at T* or *outstanding since T, node last seen at T'*.

### 4. Freshness: how stale may a node be?

Obligations track particular changes. **Freshness** bounds how long a node may operate without confirming it is current, for the cases where it cannot know what it is missing.

- A policy may declare a **freshness class** (`freshness_class: tax`). It names a kind of data whose staleness matters.
- At every successful exchange the hub returns the **class horizon**: the position of the latest event of that class. A node is **confirmed current for class K** at the moment its applied prefix covers K's horizon.
- A node measures **time since it was last confirmed current for K** with its **own monotonic clock** (elapsed time, not a wall-clock comparison with the hub), so clock skew cannot make it look fresher than it is.
- The tenant sets `max_staleness` per class (default **48 hours**, administrator-editable, with a plain-language explanation: "how long may a branch work without hearing from Head Office before it must warn or stop?").
- A node is **stale for K** when that time exceeds `max_staleness`.

**What stale means for posting** is declared by the document type that depends on the class, never by the platform:

| `stale_policy` | When a user posts a document of that type on a stale node |
| --- | --- |
| `allow_stamp` (**default**) | The document posts. It is stamped stale (section 5). The branch shows a banner. The hub raises a finding when it arrives (section 5) |
| `warn` | The user must confirm a plain warning ("This branch has not heard from Head Office for 52 hours. Rates may be out of date."). The confirmation is recorded in the stamp |
| `block` | Posting is refused with an explanation and what to do (connect, or ask a supervisor). A supervisor with permission `sync.override_stale` may override. The override is audited |

Document types declare `requires_fresh: [K, ...]` and a `stale_policy`. Most use the default. **Expected practice,** not decided here: a fiscal reporting connector (ADR-43) would set `block` for the documents it must transmit, because the legal rate must be right at issue. A POS sale defaults to `allow_stamp`, because the till must keep selling (availability matters more than a rate that is at most one cycle old, and the stamp makes the exception correctable).

The default is **stamp and correct later**, not block. The tenant decides otherwise class by class, because this is a business trade-off between continuing to trade and risking an incorrect document.

### 5. The sync stamp

Every submitted document of a `record` entity carries, in a system field set by the engine at submission and never edited, a **sync stamp**:

```
{ node, stamped_at, hub_offset_ms, applied_prefix, class_horizons_confirmed: {K: position},
  stale: [K, ...], metadata_version, software_version, override?, warning_confirmed? }
```

- It is about 200 bytes and is part of the document's image, so it travels up with the document and is in the journal.
- `stamped_at` is the node's wall-clock time, and `hub_offset_ms` is its current estimate of the difference from the hub's clock (section 7), so the hub can place the document on its own timeline.
- **Stale-document finding.** When the hub applies a `record` bundle, it checks the stamp against the mandatory events that were **in force** when the document was posted: for each mandatory record version `M` whose `published_at` was at or before the document's skew-corrected `stamped_at`, whose `effective_from` (if any) was at or before the document's posting time, and whose position is beyond the stamp's `applied_prefix`, the node posted without `M`. The hub creates a **finding** (`STAMP_VIOLATION`) naming the document, `M`, the node and the elapsed time.
- **Retroactive corrections are not violations.** If `M` was published *after* the document was posted, the node could not have known, so no finding is raised. The hub can still list affected documents with a report if a correction was retroactive.
- Findings are stored in a generic **finding queue** with reviewer, reason and status. **The platform detects and queues. What to do about a wrong document is an ERP flow** (a credit note and re-bill, say) built on the finding. The platform never edits a posted document.

### 6. Effective dating and lead time

Delivery guarantees are weak at the moment a change takes effect, because that moment may find a branch offline. The fix is for the change to **arrive before it takes effect and activate by the clock**, never by a message arriving at the moment.

- A policy may name an **`effective_from_field`**. The destination stores such a record on arrival even when its date is in the future. **The record is in force from its effective time**, resolved by the engine's effective-dated lookup (a metadata-layer requirement, WP-MET: `resolve(entity, key, as_of)`), not by when the record arrived.
- **Lead-time check at save.** When a user saves a mandatory record at the hub with an effective date, the platform computes the lead time (effective time minus now) and compares it with the **required lead**: the maximum `max_staleness` of the classes it affects plus the safety margin. When the lead is shorter, the author sees a **short-notice warning** listing the nodes that are currently behind or unreachable and would probably miss it. The author may proceed (the reason is recorded) or change the date. A tenant can make short notice a hard block for a class (`short_notice: block`), but the default is `warn`, because a legal change may take effect immediately.
- **Escalation near the date.** An obligation whose `effective_from` is within 24 hours and is not yet applied at some node is **red**, with the instruction to contact that node. It is not left to the normal ladder.

### 7. Clocks

- The hub sends its current time in every response. Each node maintains an **offset estimate** (using round-trip midpoints) and applies it when it needs the hub's timeline: effective-date resolution and the stamp's `hub_offset_ms`.
- `CLOCK_SKEW` (ADR-067): amber over 2 minutes, red over 5 minutes. A node red for clock skew shows a banner, and effective-dated resolution on that node uses the corrected time rather than the local clock.
- Freshness never depends on skew (section 4: monotonic elapsed time).

### 8. The hub's view of a node is also "as of"

A consolidated report at the hub combines data that has reached it. The report engine must **show the as-of time per node** (the node's applied prefix at the hub and its last exchange) so a head-office total is never mistaken for a complete one. A report may declare itself `requires_current: [nodes]` and warn when a listed node is behind. The platform provides the as-of data (`sync_node_state`). Reports use it (WP-RPT).

### 9. What this ADR does not promise

- It does not make a disconnected node learn a change. It bounds the damage (lead time, staleness limits), records what each document was made under (stamp), and finds the exceptions (findings).
- It does not make external effects exactly-once. A fiscal transmission or a bank file is an external effect. The platform gives each an idempotency key, an audit event and a reconciliation hook (ADR-43 and WP-INT own the details). It cannot make the other side idempotent.
- It does not decide shared balances (loyalty, gift cards, customer credit). Two nodes may both spend the same balance. That is a separate decision with its own ADR (ADR-062 later list), built on this obligation model.

## Consequences

- **WP-SYN-05 and WP-SYN-06** implement the state table, the derived ledger, deadlines, the mandatory lane, escalation, freshness classes, the stamp, the findings queue and the short-notice check.
- **Document engine** (WP-DOC-04): the stamp at submission, `requires_fresh` and `stale_policy` enforcement, the override permission.
- **Metadata** (WP-MET): `effective_from_field`, `freshness_class`, the effective-dated lookup primitive.
- **Notifications**: mail (`spot-mail`) to node contacts and the `sync_admin` role. Contacts are an attribute of a node in the registry (ADR-063).
- **Journal** (ADR-061): the transition events `sys_obligation_satisfied`, `sys_sync_node_state` and `sys_sync_finding` join the audit class.
- **Report engine** (WP-RPT): as-of time per node.
- **Tests** (WP-SYN-11): a node offline across an effective date with and without lead time; stamp findings raised exactly for documents posted without a rate that was already published, and not for retroactive corrections; the override path audited; 100 nodes with obligation evidence reconciled against ground truth; clock skew injected.

## Open questions for the architecture owner

1. **Defaults**: delivery window 24 hours, `max_staleness` 48 hours, safety margin 12 hours, escalation grace 25% (at least 2 hours). Reasonable for your first multi-branch customers, or do you want tighter?
2. **Default `stale_policy` of `allow_stamp`** for everything. Confirm the choice of availability over blocking, with fiscal documents expected to override.
3. **`record` defaults to a mandatory 24-hour window.** This makes a silent branch visible at the hub within a day. Confirm.
4. **Short notice** as `warn` by default. Is there a class (tax) that should be `block` from the start?
5. **Who is `sync_admin`** by default: the tenant administrator role, or a new role? A new role is cleaner for larger organisations.
   - **Answered by the architecture owner, 26 Oct 2026 (accepted as recommended):** A new role, separate from the tenant administrator (separation of duties).

## Alternatives considered

- **Block every node on missing acknowledgement.** Rejected: a till that cannot sell because the internet is down is a worse failure than a rate that is one cycle old, and the stamp makes the second correctable.
- **Per-record, per-node delivery rows.** Rejected: they would be the size of the data. The derived ledger from the applied prefix expresses the same facts.
- **Acknowledgements as events.** Rejected for heartbeats: they would flood the journal. Transition events capture the audit value.
- **Using the hub's clock to decide staleness at a branch.** Rejected: an offline node cannot see the hub's clock, and a skewed local clock must not be able to hide staleness. Monotonic elapsed time is immune to both.
- **Activation by message ("go live now").** Rejected: it fails exactly when a node is offline. Activation by effective time, delivered early, fails only when the lead time was too short, and the short-notice check warns about that.
