# ADR-063: Sync policy, ownership and routing

- **Status:** Accepted by the architecture owner, 26 Oct 2026, together with ADR-061 to ADR-067 as a group. Proposed by the lead, 2 Oct 2026. Open questions that state a proposal or starting value are accepted at that value (thresholds stay tunable); the open questions that stated none were answered on 26 Oct 2026 (ADR-062, "Open questions answered after acceptance").
- **Date:** 2 Oct 2026
- **Layer:** Platform (series ADR-062; generic, no ERP document types are decided here)
- **Builds on:** ADR-23, ADR-33, ADR-34, ADR-053, ADR-056, ADR-058, ADR-061, ADR-062
- **Raised by:** the architecture owner, 2 Oct 2026: "we need to define what data at high level is allowed to sync back and forth from the head office to other locations". Routing: a head-office item or tax change must reach every branch, a branch sale must reach the head office, and some records only matter to some branches.

## Context

ADR-23 declared ownership and direction by **class** (master data down, branch transactions up, shared records, balances, cross-branch flows) and said each DocType declares a sync policy in metadata (WP-EDG-08). It left open what that policy contains, how a destination is chosen, how a node proves it may write a record, what happens to records created at a branch that later become shared, and how to keep all of it understandable to people who are not integration engineers.

Three constraints shape the answer:

- **One writer per record** (ADR-23): there is no conflict resolution, so ownership must be exact and enforced at the receiver, not assumed.
- **The platform knows no ERP concepts** (ADR-058 rule 2): "company" and "warehouse" cannot appear in platform code. Routing must work on opaque keys that apps define.
- **Simplicity** (ADR-062 rule 1 and 2): nothing syncs by accident, and declaring sync must not need a specialist.

## Decision

### 1. The sync policy of a document type

Every DocType carries a **sync policy** in its metadata. It is part of the metadata version (WP-MET), so it is versioned, migrated and reviewed like any other metadata change. It has a **preset** and a small set of optional settings.

#### Presets

| Preset | Who owns the record | Direction | Sent when | Typical use |
| --- | --- | --- | --- | --- |
| `local` | The node where it lives | Nowhere | Never | Sessions, drafts, user interface state, caches. **The default.** |
| `master` | The hub | Down, to nodes in scope | Every saved version, as the latest active state | Reference data the whole network shares |
| `record` | The node that created it | Up, to the hub | Every version from first submission on (submitted, cancelled) | Transactions that happen at a branch |
| `handover` | The creating node until the hub **adopts** it, then the hub | Up; after adoption, down to nodes in scope | Every saved version | Shared records that start life at a branch |

`record` has one optional modifier, **`relay`**: the hub passes each version on to other nodes selected by a scope key found in the record (section 3). This is how a cross-branch flow works: each leg is a separate record owned by its node, and the hub relays it to the counterparty.

Child tables carry their parent's policy and may not declare their own. Derived entities (ADR-061) may not be synced. Their destination rebuilds them from the records that do sync (ADR-064).

#### Settings

| Setting | Meaning | Default |
| --- | --- | --- |
| `scope` | Which nodes receive the record (section 3) | `all` for `master`, none otherwise |
| `owner_key` | For `record` and `handover`: the scope key the creating node must hold for the record to be valid (section 2) | None: the creating node owns it |
| `relay` | Scope selector for passing a `record` on to counterparties | None |
| `excluded_fields` | Fields that never leave the origin (`local`), or never go down (`down`), or never go up (`up`) | None |
| `adopt` | For `handover`: `manual`, `auto` or `never` | `manual` |
| `when_empty` | What a scope selector does when the field is empty: `all` or `none` | `none` |
| `obligation`, `effective_from_field`, `stale_policy` | Delivery obligations and freshness (ADR-065) | Normal, none, `allow_stamp` |
| `closure` | Dependency handling overrides (ADR-064) | Derived from link fields |

The policy has a **human reading**: the Admin Center shows it as a sentence ("Owned by Head Office. Sent to every branch. Not sent: cost price"), not as a table of flags.

### 2. Ownership: who may write a record

- **Every synced record has exactly one owner at any time,** held in two system fields set at creation and never edited by users: `origin_node` and `owner`. For `master`, `owner` is the hub. For `record`, `owner` is `origin_node`. For `handover`, `owner` is `origin_node` until adoption, then the hub.
- **Mirrors are read-only.** A node that does not own a record cannot edit, submit, cancel or delete it. The document engine refuses, with the plain reason "owned by Head Office". Local documents can still *reference* a mirror, and local reactions to a mirror can create local records.
- **The receiver enforces ownership** (ADR-23). For every incoming state event it checks:
  1. the sender is the record's owner according to the receiver's own view (for a new record, the sender is `origin_node`);
  2. if the policy declares an `owner_key`, the sender **holds the scope key value** named in the record (a warehouse is written only by the node that holds that warehouse's key). Key holdings are hub-owned data, effective-dated and audited (section 4);
  3. the entity's policy allows that direction.
  A failed check is an **ownership violation**: the event is quarantined and raises a condition for a person (ADR-067), because it means a bug, a misconfiguration or a compromised node. It is never applied and never retried automatically.
- **No hard deletes after delivery.** A synced record that has been saved at its trigger state cannot be hard-deleted. The engine offers `disable` (masters) or cancel (submitted records) instead. This keeps links valid everywhere (ADR-053: referenced master data is deactivated, never deleted) and keeps the journal's history meaningful. Local drafts of `on_submit` entities are `local` until submitted and may be deleted freely.
- **Unique natural keys.** A unique field (a human-readable number or name) on a record created at several nodes can collide at the hub. A synced `record` or `handover` entity may declare a unique field only if it is **scoped by origin** or generated from a per-node naming series (WP-EDG-03). Otherwise the metadata lint rejects it (section 6). Real-world duplicates (the same customer registered at two tills) are an identity question handled by the handover mechanism below and by the Foundation layer's rules, not by the unique constraint.

### 3. Routing: choosing destinations

Routing is **declarative and evaluated on the sender**, per record version, producing the list of destination nodes. It uses **scope keys**.

- A **scope key** is an opaque pair `(key type, value)`. Apps register key types (for example `company`, `warehouse`, `region`). The platform never interprets them.
- A **node** holds a set of scope keys, plus optional **group tags** (such as `kiosks` or `northern-region`). The hub's **node registry** (control plane data, ADR-34) lists each node, its identity, its scope keys with effective dates, its groups and its software and metadata versions.
- A **scope selector** in a policy has one of three forms:

| Selector | Meaning |
| --- | --- |
| `all` | Every edge |
| `group: <tag>` | Edges with that group tag |
| `key: {field, key_type}` | Edges holding the scope key `(key_type, <value of field in the record>)`. A field holding several values (a child table of companies) selects every holder |

- A record version's destinations are the selector's matches, plus those added by the **scope-by-reference** rule of ADR-064 (a record referenced by something a node receives must reach that node too, so that the receiver never lacks a dependency).
- **Direction up** has one destination, the hub. **Relay** (the `relay` modifier) selects further destinations at the hub with the same selectors.
- **Filtered views.** For each destination the hub exposes its stream filtered by these rules. The view uses the source's own positions, so a destination's checkpoint is a position in the source stream and gaps between delivered positions are expected (ADR-061 stream pull API). Order within the view is the source's publication order, which preserves causal order (ADR-053).
- **Re-evaluation.** When a scope changes (a node gains a company, a record's scope field changes, a group tag changes), the sender computes the difference. Newly in-scope records are **backfilled** to the destination as a bundle set (ADR-066 snapshot path). Records that left a destination's scope are **not deleted** there: they remain as mirrors, flagged `out_of_scope`, and stop receiving updates. Deleting data from a node is a deliberate administrative action, never a routing side effect.
- **Explain.** `explain(node, entity)` and `explain(record, node)` answer "what does this node receive?" and "why does or doesn't it receive this record?". They are the first support tool for routing questions.

### 4. Node registry and key assignment

- **Nodes are hub-owned data** with a lifecycle: `commissioning`, `active`, `suspended`, `decommissioned`. A node is `active` only after its identity is registered (mutual TLS, ADR-23) and its bootstrap has completed (ADR-066).
- **Scope key holdings** are effective-dated assignments (`node X holds (warehouse, W7) from D1 to D2`). The registry rejects two nodes holding the same *ownership* key at the same time. Moving ownership of a key between nodes is an **ownership migration**: an explicit, approved, audited operation that uses the handover mechanism below (hub adopts, then assigns). Its operational flow belongs to the branch lifecycle ADR (ADR-062 later list).
- **Suspending or decommissioning** a node stops its routing and rejects its connections. Its streams and the journal remain.

### 5. Handover: records that start at a branch and become shared

A `handover` record lives through three states:

| State | Owner | Visible to |
| --- | --- | --- |
| **provisional** | The creating node | The creating node and the hub |
| **adoption pending** | The creating node (still) | As above |
| **adopted** | The hub | Nodes in scope, as a `master` |

Adoption is a **two-step exchange in stream order**, so that no edit is ever lost:

1. The hub writes an *adoption request* naming the version of the record it has seen. It does not yet own it.
2. When the creating node applies the request, it **stops being the owner** (its copy becomes a mirror, read-only) and writes an *adoption acknowledgement* carrying its **final version**. Because the acknowledgement follows the node's own state events in its stream, the hub is guaranteed to have every version up to that final one when it processes it.
3. The hub then takes ownership and writes the adopted version. From here it behaves as a `master`: edits happen at the hub and flow down, scoped.

While the request is pending, the creating node may keep editing and its edits apply at the hub. If the node is offline the adoption simply stays pending, visibly (a condition at amber, ADR-067). Nothing is overwritten and nothing is blocked. `adopt: auto` makes the hub issue the request as soon as the record arrives, and `adopt: never` keeps a record node-owned for ever (a branch's own customer list, say).

**Duplicates.** The platform provides the mechanism for merging: the hub, as the owner after adoption, may mark a record `merged_into` another. The merge fact flows down as an ordinary update, and mirrors resolve through it. Posted documents keep the ids they were created with and are never rewritten. **What counts as a duplicate, and when to merge, is domain logic** for the Foundation layer (the party model) when that stage opens.

### 6. Policy lints (checked on metadata save and app install)

Inconsistent policy is rejected when it is written, with an explanation and a suggested fix.

| Lint | Rejects | Why |
| --- | --- | --- |
| L1 closed links | A synced entity with a link field to a `local` entity | The destination could never resolve it |
| L2 direction | A `master` that links to a `record` or provisional `handover` entity | Hub data would depend on branch data. Allowed only if the link is marked `requires_adopted` and the engine refuses to save the link until the target is adopted |
| L3 optional exclusions | An excluded field that is required and has no default | The destination stores full documents (ADR-053), so it needs a value for every declared field |
| L4 scope fields | A scope selector whose field is not a link, enum or key-typed field | It could not name a key |
| L5 child policy | A child table with its own policy, or a derived entity that syncs | Children follow parents. Derived data is rebuilt, not copied |
| L6 unique keys | A unique natural key on a `record` or `handover` entity that is neither origin-scoped nor from a per-node series | It would collide between nodes |
| L7 reference reach | A `master` whose referenced targets are not reachable by every node the master reaches (by scope or by reference) | A destination could receive a master it cannot complete |

### 7. Changing policy later

A policy change is a metadata change and appears in the migration plan (WP-MIG-01) with its effect:

- `local` to a synced preset: allowed. The plan shows the **backfill** it will cause (how many records go to which nodes), and the backfill runs through the same path as a new node's bootstrap (ADR-066).
- Changing a record's **owner class** once data exists (for instance `master` to `record`) is a **destructive** change. It needs an explicit ownership migration plan and approval. The platform refuses it as a routine edit.
- Narrowing `scope` leaves existing mirrors in place (section 3). Widening it backfills.
- A synced preset back to `local` stops sync. Existing mirrors remain as read-only copies and are flagged.

### 8. Between tenants

The same policy vocabulary applies to a tenant link (ADR-058), where the tenant's exchange grants take the place of scope keys: a grant names topics, direction and expiry, and the policy's `excluded_fields` double as the residency filter ("the sender's residency policy decides what may leave").

### 9. Presets for authors, switches for administrators

- **App authors** choose a preset when they declare a DocType, and the lints guide them. Most DocTypes need only `preset`.
- **Administrators** do not edit policy. They see each DocType's human reading, can change `scope` selectors from lists of the tenant's own keys and groups, and set the freshness settings of ADR-065. Anything else is a metadata change under the usual approval.
- A DocType created at runtime is `local` until someone chooses (WP-EDG-08's acceptance: a runtime DocType syncs by configuration alone).

## Consequences

- **WP-EDG-08** is superseded by WP-SYN-01 (ADR-062), which implements this ADR's policy model, presets, lints and the human reading.
- **WP-EDG-03** (partition ownership and per-branch naming series) provides `owner_key` enforcement and the series that lint L6 depends on.
- **The document engine** (WP-DOC) gains: the `origin_node` and `owner` system fields, mirror read-only enforcement, and the delete-to-disable rule for synced entities.
- **Metadata** (WP-MET) gains the policy attribute, the lints, `requires_adopted` on links and key-typed fields.
- **Control plane** (WS18) gains the node registry with effective-dated scope key holdings.
- **Foundation and ERP layers** declare their DocTypes' presets and scope selectors when their stages open. This ADR does not decide that catalogue.

## Open questions for the architecture owner

1. **`adopt` default.** Proposed: `manual`, so the hub reviews shared records created at branches. Confirm, or prefer `auto`.
2. **Out-of-scope mirrors.** Proposed: kept and flagged, never deleted by routing. A purge is an explicit administrative action. Confirm.
3. **Group tags.** Proposed: free-text tags registered in the node registry, so a tenant can group branches without a new key type. Confirm they are needed in v1.
4. **Who may change a node's scope keys?** Proposed: only a tenant administrator, with the change audited and effective-dated, never a node itself.

## Amendment 1 (26 Oct 2026, approved by the architecture owner in the review of ADR-083)

Raised by ADR-083 (consumer identity), where an order created at an edge must reach the hub and then be worked by hub staff. Two additions, both additive:

1. **Adoption can be triggered by a business-process step.** The `adopt` setting of section 1 stays and is the **default**. A business-process definition (ADR-080) may add a step, "adopt this record", that issues the adoption request of section 5 at the point the process chooses (on arrival, when payment is confirmed, after a review). Where a process names a point, that wins over the DocType setting for records of that process. The two-step exchange in stream order is unchanged; only who issues the request changes. This follows the rule that whatever the platform does at the fundamental level is configurable at the business-process level (the architecture owner, 26 Oct 2026).
2. **A published process definition is hub-owned data.** Each published version of a process definition is owned by the hub and flows down to every edge in scope, like a `master` record. A running Case stays pinned to the version it started on (ADR-071, `spot-bpm-versioning`), and the version-skew rules of ADR-066 apply. Delivered by WP-SYN-12. Before this amendment no path from the hub's process registry to an edge was found.

**Consequence for the `adopt` default question** (open question 1 below): the proposal to keep `manual` as the DocType default stands; ADR-083 chooses `auto` for consumer-caused orders through the process.

## Alternatives considered

- **Per-record sync flags set by users.** Rejected: unreviewable, easy to forget, and no way to lint.
- **Conflict resolution (last writer wins, CRDTs).** Rejected by ADR-23: ownership removes conflicts, and merging financial state is not safe.
- **Routing by a platform-known `company` or `branch` field.** Rejected: it would put ERP concepts in the platform (ADR-058). Scope keys give the same power without it.
- **Subscriptions chosen by each branch.** Rejected as the main mechanism: the hub decides what a branch needs, because a branch cannot know what it lacks. Branch-chosen subscriptions may be added later for optional data.
- **Adoption by a single hub-side write.** Rejected: it can overwrite edits a branch made offline. The two-step exchange in stream order costs one round trip and loses nothing.
