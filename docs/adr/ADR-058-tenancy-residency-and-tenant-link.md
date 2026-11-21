# ADR-058: Tenancy, data residency and the tenant link

- **Status:** Proposed by the lead, 2 Oct 2026. Amendment of 17 Oct 2026 accepted by the architecture owner; rules 1 to 7 still await his confirmation. Work packages WP-CTL-09 to 11 are in the plan.
- **Date:** 2 Oct 2026. Amended 17 Oct 2026 (rules 8 to 14, on request of the architecture owner: processes that span organisations, see ADR-080).
- **Layer:** Platform (amends ADR-34; the ERP side is a separate ADR, see "Consequences")
- **Builds on:** ADR-11, ADR-23, ADR-34, ADR-56 (event and outbox schema), SEC-05, CTL-01
- **Raised by:** gap G5 of the foundation gap report (2 Oct 2026): a group with entities in the EU, China, India and the Gulf may be legally required to keep some entities' data in-region, and ADR-34 and ADR-37 give no answer.

## Context

ADR-34 makes one tenant equal one site, one logical database, one placement. SEC-05 promises per-tenant data residency. ADR-37 and ADR-39 talk about "companies in one tenant" and about franchise links between tenants. Three questions were left open:

1. What is the unit of residency: the tenant, or something inside it?
2. How do two tenants exchange business data, for franchise links today and for groups tomorrow?
3. Where does the platform stop and the ERP start?

The gap report offered three answers: (a) entity-level placement inside one tenant, sharding by company; (b) a group of tenants with a cross-tenant protocol; (c) both.

## Options

**A. Entity-level placement (shard by company).** The platform would need a shard key that means "company". Every query filter, lock, backup, migration and cache would need to know about it. That puts an ERP concept into the platform, defeats per-tenant backup, restore and rehearsal (ADR-34, ADR-22), and breaks Citus schema-based sharding, which places whole tenants.

**B. One tenant per region, linked.** The tenant stays the unit of placement and residency. A generic link lets two tenants exchange agreed records. The platform does not know what the records mean.

**C. Both.** Needs A's shard key for the default case. Rejected for the same reason as A.

## Decision

Adopt B, with these rules.

1. **The tenant is the unit of placement and residency.** A tenant record carries a `region` chosen from a platform-defined region list (the list and its legal meaning are data owned by the control plane). The control plane never places a tenant, its backups, snapshots, read replicas, analytics mirror, search and AI-memory stores, or rehearsal clones outside its region. Moving a tenant is still a per-tenant export and import (ADR-34), and a move across regions is refused unless the owner approves a change of region.
2. **No sub-tenant placement.** The platform defines no shard key below the tenant. Nothing in the storage contract, the permission engine or the job engine may depend on a business concept to decide where data lives.
3. **A tenant link is a platform object.** It is a relationship between exactly two tenants with:
   - a handshake in which both tenant administrators consent, with peer identity keys exchanged and recorded in the control plane registry (the registry records that a link exists; it never carries link data);
   - one or more **grants**, each naming a set of exchange topics, the direction (send, receive, both) and an expiry;
   - revocation by either side at any time, effective for new exchanges immediately;
   - an audit trail on both sides.
4. **Exchange is asynchronous and local.** Records travel as versioned outbox events (ADR-11, ADR-56), at least once, applied idempotently by event id on the receiving side through the same apply and quarantine path as branch sync (WP-EDG-06). There are no cross-tenant queries, no shared tables and no distributed transactions: each side commits locally and reconciles from the events (ADR-34 stays intact).
5. **Ownership travels with the record.** An inbound record is stored in the receiver's own schema as an ordinary document carrying its origin (source tenant, source id, source version). The sender stays the owner and is the only side that may change it; the receiver may react but never edit the original (ADR-23's ownership rule, applied across tenants).
6. **The sender's residency policy decides what may leave.** Each exchange topic declares, as metadata, which fields may cross the link; the default is that nothing leaves. The receiver's region is not restricted by the platform, but a grant may require the peer to be in a given region.
7. **Apps declare what flows.** An app registers exchange topics in its manifest (ADR-17) with their schemas. The platform carries them; it does not interpret them.

### Amendment of 17 Oct 2026: links between organisations, not only between a group's tenants

Raised by the architecture owner: a process may involve several organisations, some outside the platform, and some tenants may run on separate servers that are disconnected from each other for long periods. The original rules 1 to 7 cover records that two tenants of one owner mirror to each other. They do not cover messages between organisations, peers that are not tenants, links between servers that cannot dial each other, or what a receiving tenant does when something arrives. The rules below add that, and nothing in rules 1 to 7 changes.

8. **Two kinds of exchange topic.** A **record topic** is the existing kind: the sender's record is mirrored into the receiver's schema read-only with its origin (rule 5). A **message topic** carries a **message**: an immutable, addressed envelope `{ message id, conversation id, sequence in conversation, topic, schema version, sent at, payload }` that the sender owns and the receiver stores in a framework inbox, never as an editable business record. A message is not a request to edit anything; the receiver decides what to do with it. Both kinds use the same grants, field lists (rule 6), outbox transport, and idempotent apply (rule 4).
9. **Delivery notice.** The replication apply path runs no business logic and emits no document event (WP-EDG-06, unchanged). For a topic declared `notify`, it additionally appends a **delivery notice** (topic, message or record id, conversation id, source tenant, received at) to a local notice feed in the same transaction. A notice is not a change to any record; subscribers (the process engine is the first) read it like any other feed, at least once, and must be idempotent. Without this, nothing on the receiving side could react to an arrival.
10. **Links stay pairwise.** A link joins exactly two parties (rule 3). An arrangement of three or more parties is a set of pairwise links. The platform never relays a message from one link to another; if a participant forwards, it sends a new message of its own on the other link, so each hop has its own grant, audit entry and field list.
11. **Carriers.** A link names how bytes move, because the two sides may not be able to reach each other:
    - `direct`: each side publishes an inbound endpoint; the sender dials the receiver over mutual TLS with the peer keys exchanged at the handshake.
    - `pull`: the side that cannot accept inbound connections dials the other side's outbound mailbox and fetches, long-poll style (the same edge-initiates rule as ADR-062). Either side may use `pull` independently per direction.
    - `external`: the peer is not a platform tenant (rule 12).
    A relay run by the control plane is **not** part of this decision. If one is ever added it carries only ciphertext sealed to the peer's key.
12. **Peers that are not tenants.** An `external` link has the same grants, topics, field lists and audit, but only message topics (no record mirrors). The peer's identity is a credential reference in the secrets vault (never inline), and bytes move through a connector or an inbound webhook (WP-INT-05, WP-INT-01) that signs and verifies each message. Reachability, retries and receipts are the connector's, and the platform records the same delivery states as for a tenant peer.
13. **Delivery states and honest failure.** Each message on the sender side has a state visible to its producer: `queued`, `sent`, `acknowledged` (received, stored), `applied` (receiver accepted it), `rejected` (with a reason code the receiver chose) or `expired` (past the topic's lifetime). A receiver never drops a message silently: invalid or out-of-order input goes to the quarantine queue (WP-EDG-06) and the sender is told. A message that stays `queued` or `sent` past a threshold raises a condition on the sender's health surface. The outbox keeps undelivered messages until acknowledged or expired, with a retention bound set by the tenant (a long disconnection fills the bound visibly; it does not lose messages without a condition).
14. **Ordering and versions.** Within one conversation, messages apply in sequence order; a gap parks later messages until the missing one arrives or expires (the parking rule of the apply path). Each topic declares schema versions and the range a grant accepts; a message outside the range is rejected with `link.version_unsupported`, not guessed at.

Consequently the original consent open question gains a case: when the two parties are **unrelated organisations**, there is no "one party administers both", so both consents are always required, and a grant never exceeds the topics both sides listed.

## Consequences

- **ADR-34** gains the region attribute and points here for linking. Its other rules are unchanged.
- **ADR-39a** (franchise links) and **ADR-37** ("companies in one tenant are a related group") are instances and users of this decision; their wording follows in the group-structure ADR.
- **Layers:** the Foundation and the ERP specify what they exchange over a link and how a group uses it. That is a separate ERP-layer ADR (group structure, intercompany and consolidation inputs), owed before DES-02 locks. The expected answer: a group within one region lives in one tenant with companies as legal entities; a residency-driven group runs one tenant per region and links them.
- **Work that follows if approved** (not yet in the plan; none of it exists today, so no process or app can use a link yet): 

  | ID | Work package | Depends on |
  | --- | --- | --- |
  | WP-CTL-09 | Tenant link registry and handshake: link record, peer identity keys, both-sides consent, grants per topic with direction, expiry and region requirement, revocation, audit on both sides, export field lists (rule 6) enforced at the sender | CTL-01, SEC-05, SEC-09 |
  | WP-CTL-10 | Link exchange: record and message topics over the outbox, carriers `direct` and `pull`, delivery states, delivery notice feed, inbox store, version range check, conditions for stuck messages | CTL-09, EDG-02, EDG-06 |
  | WP-CTL-11 | External peers: the `external` carrier over connectors and signed inbound webhooks, credential references, the same delivery states | CTL-10, INT-01, INT-05 |

  Plus an extension of SEC-05's acceptance to link exports. No spike: every ingredient is an established pattern (ADR spike rule, 29 Sep 2026).
- **Limits.** A link gives eventual consistency, not a joint ledger. Anything that needs a single consistent view across regions (a consolidated report, an intercompany match) works from the exchanged records on one side.

## Open questions for the architecture owner

1. **Region list.** Who maintains it and at what granularity (country, bloc, cloud region)? Proposed: the control plane owns it as versioned data, starting with the regions of your first customers.
2. **Hosted franchisee companies** (ADR-39a, in the franchisor's tenant) stay in-tenant as decided; confirm that the network-linked franchise level becomes a tenant link with a franchise grant.
3. **Consent model.** The proposal requires both tenant administrators to consent. Confirm this also holds for a group where one party administers all tenants (a one-click approve on both sides is fine; the audit stays).
4. **Message retention bound.** Decided 17 Oct 2026: configurable per tenant, default 30 days, range 1 day to 1 year.
5. **Relay.** Deferred (17 Oct 2026, TODO for later): an optional ciphertext relay run by the control plane for two parties that can reach neither each other's inbound endpoint nor a mailbox (rule 11). Not needed to start.
