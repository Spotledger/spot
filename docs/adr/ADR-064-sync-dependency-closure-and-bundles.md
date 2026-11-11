# ADR-064: Dependency closure and bundles

- **Status:** Accepted by the architecture owner, 26 Oct 2026, together with ADR-061 to ADR-067 as a group. Proposed by the lead, 2 Oct 2026. Open questions that state a proposal or starting value are accepted at that value (thresholds stay tunable); the open questions that stated none were answered on 26 Oct 2026 (ADR-062, "Open questions answered after acceptance").
- **Date:** 2 Oct 2026
- **Layer:** Platform (series ADR-062; generic)
- **Builds on:** ADR-23, ADR-053 (deferred link checks, causal order), ADR-056 (transaction and ordinal in the envelope), ADR-061 (full-state events), ADR-062, ADR-063
- **Raised by:** the architecture owner, 2 Oct 2026: "when we say some doc will sync it will always be with its full dependency". If a branch customer has not reached the head office, the branch's sale fails to apply there. A sales invoice or a material movement must never arrive without what it needs.

## Context

A synced document links to other records (a party, an item, a warehouse, an earlier document it amends) and produces others (ledger entries, stock movements). The storage contract makes structural links **database-checked** with deferred foreign keys (ADR-053). A record whose target is absent at the destination fails at commit with `ReferenceViolation`. ADR-053 already notes the consequence: the apply path must apply targets before the records that link to them, and a record whose target never arrived goes to quarantine and is retried.

That is a safety net, not a design. A sync that depends on quarantine and retry for ordinary operation is a sync that fails often and loudly. This ADR makes dependency completeness a property of **how data is sent**, and adds a repair loop for the cases where it fails anyway (a restored backup, a bug, a scope change).

Two facts from earlier decisions make a clean solution possible:

- **Causal order** (ADR-053): within one source, anything committed before another transaction is published before it. A record's targets exist before it, so they appear earlier in the same stream.
- **Full images** (ADR-061): a state event is the whole record. Applying the latest version of a record needs no earlier version of it.

## Decision

### 1. The unit is the transaction, plus what it needs

The atomic unit of sync is a **bundle**. A bundle contains:

| Role | What | Why |
| --- | --- | --- |
| **root** | The in-scope state events of one source transaction that are roots, normally the documents the user saved or submitted | The reason the bundle exists |
| **satellite** | Every other in-scope state event of the **same source transaction** (ledger entries, stock movements, balance-relevant records, side documents created by hooks) | Business consistency: the invoice and its postings arrive together or not at all |
| **extra** | A record the destination does not have that a root or satellite references, with its own missing references, recursively | Dependency completeness (section 3) |

- The source transaction is identified by the `transaction` id and the `ordinal` of each event (ADR-053, ADR-056). No new grouping is invented.
- A bundle is applied in **one destination transaction**. It succeeds entirely or not at all (G3). Deferred foreign keys let the records inside it arrive in any order.
- A bundle is never larger than the source transaction plus its closure. Source transactions are bounded by `max_writes` (ADR-053), and the closure has its own bound (section 4).
- **Out-of-scope records in the same transaction** (data the destination must not receive) are omitted. Metadata lint L7 (ADR-063) guarantees that omitting them never removes a dependency of what is sent.

### 2. What counts as a reference

References are read from the **catalog**, not guessed from data. For a record image:

- **Link fields** with a fixed target entity, in the parent and in every child row.
- **Dynamic links** (ADR-053 `EngineOnly`): the target entity is read from the record's type field. A dynamic link must declare in metadata the closed list of entities it may point to (`allowed_targets`), so the policy lints (ADR-063 L1) can check it. A dynamic link without a declared list is a metadata error on a synced entity.
- **Amendment and cancellation links** (`amended_from` and the like) are ordinary links.

Text fields that happen to contain a name or number are **not references**. Only declared links count.

### 3. Closure on the sender

For a bundle going to destination `D`, the sender builds the closure like this:

1. Start with the roots and satellites (section 1).
2. For each reference target `X` that is not already in the bundle, decide whether `D` **has** `X`:
   - `D` owns `X` (it came from `D`), or
   - `X` is in `D`'s scope and the latest event of `X` is at or before `D`'s **acknowledged position** (the completed prefix of what `D` has applied, ADR-065), or
   - `X` is in `D`'s **extras set** (section 5), or
   - `X` was already included earlier in the same pull session.
3. If `D` does not have `X` (or the sender cannot tell), **add `X`'s current image to the bundle as an extra**, then repeat from step 2 for `X`'s own references. Sets, not lists: a cycle terminates because each record is added once.
4. Apply the destination's exclusions (`excluded_fields.down`, ADR-063) to every image.

**Rule for doubt: include.** Sending a record the destination already has is harmless, because apply is idempotent (section 6). Failing to send one it lacks is the failure this ADR exists to prevent.

**Why this is usually cheap.** Because of causal order, a target in `D`'s scope appears earlier in the same stream and `D` already has it or is receiving it in the same pass. Extras happen only when a record's scope does not include its target: a transfer to a branch that references a record owned by another branch, or a hub master in one company that references a hub master in another.

**Up direction.** An edge sending to the hub needs no extras for hub-owned targets (the hub owns them and never hard-deletes them, ADR-063) or for targets that are mirrors at the edge (they came from the hub, so the hub has them). It needs closure only for records the edge itself owns, and causal order delivers those first. The same code path handles both directions. In the up direction, it finds nothing to add in normal operation.

### 4. Bounds and splitting

- The closure is capped (default 500 records or 4 MB of images per bundle). If it exceeds the cap, the sender **splits** it: the closure is sent first as one or more **prerequisite bundles** in dependency order (cycles stay together), then the root bundle. Each is complete in itself, and the destination applies them in order.
- A closure larger than a hard limit (default 5,000 records) is a **condition** (`CLOSURE_TOO_LARGE`, ADR-067), because it signals a metadata problem such as a master that references most of the database, not a data problem.

### 5. The extras set

A record sent as an extra is outside the destination's normal scope, so routine routing would never send it again. If it changes later, the destination's copy would silently go stale.

- The sender keeps an **extras set** per destination: the ids of out-of-scope records it has delivered as extras.
- **While a record is in the set, its later versions are routed to that destination** like any in-scope record. The destination holds it as a mirror flagged `by_reference`.
- Records leave the set only when the destination is decommissioned or a scope change brings them in scope properly. The set is small in practice (only cross-scope references), and its size is monitored (`EXTRAS_GROWTH`, ADR-067).
- This is the **scope-by-reference** rule referred to in ADR-063. It is the only way a record reaches a destination outside its declared scope, and it exposes no more than the closure strictly needs.

### 6. Apply on the receiver

The receiver processes one source's bundles **one at a time in position order**. Different sources apply in parallel. For each record in a bundle:

1. **Ownership check** (ADR-063 section 2).
2. **Version rule**, per record id:
   - stored version below the incoming version: replace the whole record, including its child rows, **keeping the incoming version number** (the apply path stores the origin's version, WP-EDG-06);
   - equal: no change, but compare the content digest of the stored record with the incoming image. A **same version with different content** means corruption or a bug and raises `DIVERGENCE` (ADR-067);
   - stored version above the incoming: ignore it as a stale duplicate.
3. **No business logic, no side effects, no new events** (the shared apply path of ADR-061 and ADR-062).
4. **Projectors.** Derived entities (ADR-061), such as balances, are brought up to date by their registered projector inside the same transaction. A projector is a deterministic function of the applied records and contains no business rules. A projector failure fails the bundle (it is parked as `projector_failed`, a condition for the app owner).

Gaps in the version sequence are normal and harmless (full images): receiving version 5 while holding version 3 is simply an update. The receiver holds the **latest known state** of each mirror. The full history of a record stays with its owner and in the journal.

### 7. When a dependency is still missing: park, ask, repair

Closure should make a miss impossible in normal operation. It can still happen: a node restored from an old backup, a bug, a scope change racing the sender, an operator deleting a mirror. The receiver then handles it **automatically and visibly**:

1. **Park, do not poison.** The bundle fails with `ReferenceViolation`. It moves to the **parking area** with the missing ids recorded (`waiting_for`). Parking never blocks the stream: other bundles continue. Bundles that depend on a parked bundle's records (they would fail the same way) are parked behind it automatically.
2. **Ask the owner.** The receiver writes a `sys_sync_missing` audit event listing the missing records and the parked bundle. It travels to the other side in the ordinary stream (up for a node, down for the hub).
3. **The owner answers with a repair bundle**: the closure of the requested records (current versions, with their own missing references, by the same algorithm), flagged `repair`, and adds them to the destination's extras set where they are out of scope. The receiver applies it, then **retries the parked bundles** automatically.
4. **Backoff.** First attempt immediately, then 1 minute, 5 minutes, 30 minutes, 2 hours, then every 6 hours. Every attempt is an audit event.
5. **Escalation.** If a requested record does not exist at the owner (it never did, or was removed illegally), the bundle moves to **quarantine** (`QUARANTINED`, red, ADR-067) with a plain explanation, because that is an integrity problem a person must look at. Parked bundles still waiting at the escalation deadline (default 24 hours) raise `MISSING_DEP` at amber, then red.
6. **No silent skip.** Nothing leaves parking except by being applied or by an explicit, audited operator decision.

Parking and pull-on-miss are the **same repair mechanism** that handles a rebuilt node, a restored hub and scope backfills. They are not a special case for rare errors.

### 8. Coalesced catch-up keeps closure

A node that returns after a long absence receives coalesced images (the latest version of each record rather than every version, ADR-066). Coalescing can reorder a record after something that references it. **Rule:** coalesced catch-up is delivered as bundles built by this ADR's algorithm, and the planner orders them by **dependency** (targets before the records that reference them, cycles together), not by their original position. The planner may merge bundles into one transaction. ADR-066 specifies the planner. The correctness of the dependency order is this ADR's property.

### 9. What the platform does not decide

- **Which values a record copies from its references at the time of posting** (for example a rate or tax percentage denormalised into an invoice) is an ERP rule. The platform records, but does not interpret, the master-data position a document was made under (the sync stamp, ADR-065).
- **When two records are the same real-world thing** (duplicate parties) is the Foundation layer's identity question (ADR-063).

### Worked cases (illustrations, not ERP design)

| Case | What happens |
| --- | --- |
| A branch, offline for a day, registers a new party (handover record) and posts a sale to it | Two source transactions, in order. The hub applies the party (provisional) and then the sale. If the party's bundle is somehow missing, the sale parks, the hub asks the branch, the branch sends a repair bundle, and the sale applies |
| The hub creates an item and a transfer to a branch that references it, and the branch has not yet received the item | Item event precedes the transfer in the stream (causal order). In the same pass the branch gets the item first, or in the same bundle. No gap |
| A transfer to branch B references a warehouse record owned by branch A (out of B's scope) | The warehouse arrives as an extra and joins B's extras set. Later changes to that warehouse keep reaching B |
| A branch is restored from last night's backup | Records it acknowledged but no longer has fail as missing dependencies. They are parked, requested and repaired. Reconciliation (ADR-067) also catches records nobody references |
| A hub master is referenced by a new branch invoice, then the master is disabled | The master is never hard-deleted (ADR-063), so the reference stays valid. `disabled` flows down like any version |

## Consequences

- **WP-EDG-06** (apply path and quarantine) becomes the receiver in section 6 and 7: version rule, parking, projectors, repair retry. Quarantine is narrowed to integrity and ownership problems that need a person.
- **WP-STO-08** (relay) and **WP-JRN-09** (stream pull API) expose `transaction` and `ordinal` in filtered views, and the session cache for extras.
- **WP-SYN-03 and WP-SYN-04** implement the sender closure and the receiver loop.
- **Metadata** (WP-MET) must expose `references(image)` from the catalog, including child rows and dynamic-link target lists, and the `allowed_targets` declaration.
- **Document engine** (WP-DOC-04): the apply path stores a given version and runs projectors. It never runs lifecycle hooks.
- **Test requirements** (WP-SYN-11), as property tests over random reference graphs:
  - closure completeness: applying any bundle to a node holding only its own extras set never raises `ReferenceViolation`;
  - convergence: any causal-order-preserving delivery with duplicates, drops of non-root records and replays ends with identical state and no human involvement;
  - the dependency fuzz: randomly deleting any single mirror record on a node is repaired by the loop within the backoff schedule.

## Open questions for the architecture owner

1. **Closure caps** (500 records or 4 MB per bundle, 5,000 hard limit): reasonable starting values, or do you expect larger?
2. **Extras never expire** while the destination exists. Proposed because the alternative needs reference counting. Confirm the trade-off of keeping a small number of out-of-scope mirrors.
3. **Receiver digest comparison on equal versions** adds a hash per duplicate record. Proposed: always on in v1, with a sampling option if measurement shows a cost.

## Alternatives considered

- **Rely on quarantine and retry for dependencies.** Rejected: it makes the normal case a failure. The park and repair loop is kept only as the safety net.
- **Receiver-driven pull for every dependency.** Rejected as the main path: a round trip per missing record is slow for a branch on a poor link, and unnecessary when the sender can include what is missing. It remains the repair path.
- **Disabling foreign-key checks in the apply path.** Rejected: the database's structural guarantee (ADR-053) is exactly the safeguard that catches a closure bug. The platform keeps it.
- **Per-record delivery ledger at the sender.** Rejected: a table the size of the data. The acknowledged position plus a small extras set expresses the same knowledge, because the stream is ordered.
- **Sending the whole transitive closure every time.** Rejected: it multiplies bandwidth on slow branch links. Sending only what the destination lacks does the job.
