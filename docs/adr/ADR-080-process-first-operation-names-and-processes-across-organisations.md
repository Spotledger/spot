# ADR-080: Process-first operation, the user-facing name of a run, and processes across organisations

- **Status:** Accepted by the architecture owner, 17 Oct 2026 (decisions recorded below; the work packages in Consequences were added to the plan the same day).
- **Date:** 17 Oct 2026
- **Layer:** Platform (extends ADR-49, the Business Process Manager, which stays in force)
- **Builds on:** ADR-49, ADR-058 (as amended on 17 Oct 2026), ADR-070, ADR-073, ADR-051, ADR-062
- **Raised by:** the architecture owner, 17 Oct 2026. In an ERP everything should be a process, adding an employee or an item as much as a purchase. A government agency, a school or a hospital will run processes unrelated to a trading business. A process may involve several organisations, including systems outside the platform and tenants on separate servers that are disconnected from each other. Users should not see the word "Case".

## Context

ADR-49 made the Process the only workflow concept. It settled that a step changes a record only through the record's own lifecycle, that governance is configurable and off by default, and that one durable engine serves both long business processes and integration flows. Three things were left open:

1. **Where the logic lives, and whether "everything is a process" means the process owns it.** Without an answer, every module will make its own choice.
2. **How a person starts and feeds a process.** ADR-49 has triggers and decision steps, but no step in which a person enters the data that creates a record, so "a new employee" can only be a plain form followed by a process, not a process from the first click.
3. **Processes that cross organisations.** A process runs in one tenant and one database. Calling another system is an integration step. Nothing lets two organisations, or two tenants on disconnected servers, run the two halves of one agreed process. ADR-058 had no message exchange, no peers that are not tenants and no carrier for servers that cannot reach each other; it is amended alongside this ADR.

Wording follows the layer rule: this ADR speaks of records, steps, participants and messages, and names no business document.

## Decision

### 1. Division of responsibility

1. **The record type owns its own logic and lifecycle.** Validation, calculation, posting side effects, permissions and the record's states belong to the type, in every channel (native API, import, script, replication, process step). A process step never writes a state; it asks the type's lifecycle operation, so every rule runs exactly as on a user's click (ADR-49, unchanged).
2. **The process owns who, when and in what order.** It decides which person or system acts next, what is waiting for what, what happens on timeout, and which records are created along the way.
3. **Why the logic does not move into processes.** (a) A record arrives by import, API, script or sync, and none of those passes through a process; (b) a replicated record is stored without re-running logic (ADR-062), so rules held in a process would be skipped there; (c) a running request stays on its process version while the posting rules must be the same for every record of a type; (d) several processes may touch one record type, so no single process can own it.

### 2. Process-first operation

1. **A mode per record type and scope.** Each record type has an entry mode, set per organisation unit scope (company, branch, group of units) and held as a setting: `free` (default, as today), `governed` (a record may be created, or move through governed transitions, only inside a running request; ADR-49 governance, WP-BPM-08), or `process_only` (as `governed`, and the generic "New" action is replaced by "Start" on the process that creates it). Changing a mode is audited and takes effect for records created after its effective instant.
2. **Entry step.** A new node kind, the **form step**, asks a named person or role to enter or review a set of fields of a record. Its output is an ordinary **draft record** created or updated through the type's lifecycle (so every validation and permission applies), flagged as owned by the request. Completing the step and saving the draft commit together. The record is submitted by a later document step, as today. The same step can collect plain request data that creates no record (a leave request that is itself the only record).
3. **"New" becomes "Start", for everyone.** In `process_only` scopes there is **no bypass and no standing outside-process permission** (decided 17 Oct 2026): the desk's New action, list "add" buttons, the API's create endpoint, imports and scripts all start the entry process of that type, as the calling principal. Replication is not creation: a record created on another node is stored as it arrived (ADR-062) and the process ran where it was created. The outside-process permission with a reason (ADR-49) remains only for `governed` mode. A bulk import is one request that owns many records through a batch step with item outcomes (ADR-071), not one request per row.
4. **Applies to any record type.** An employee, a catalogue item, a student or a patient is as eligible as a purchase. The platform makes no assumption that a record has financial effect.

### 3. The user-facing name of a run

1. The internal and API term stays `case` (events, contracts, tables, error codes). Users never need to see it.
2. Every screen, notification, e-mail and report takes the name from a **label pair** (`bpm.run.singular`, `bpm.run.plural`) in the localisation catalogue (ADR-073). The shipped default is **"Request"** ("Start a request", "My requests", "Request 123 is waiting for approval"). A pack or an organisation may override the label: "Application", "Admission", "File", "Claim".
3. The label is configuration data, not code: changing it needs no release and the designer previews it.

### 4. Processes belong to packs and organisations

1. A process, its form steps, its contracts (section 5) and its templates are **data** delivered in app packs or authored in the designer. They travel in the customisation bundle (WP-ALM-01) like other site changes.
2. A tenant installs only the packs it needs. A school and a hospital share the engine and nothing else, unless they choose to. No process pack may assume that another pack is installed unless it declares the dependency in its manifest.
3. Packs for a sector (the ERP, a school, a clinic, an agency) are the job of the layers above the platform, when their stages open (ADR-060).

### 5. Processes across organisations

**5.1 Orchestration inside, choreography between.** A process always runs inside one tenant, under that organisation's rules and permissions. When several organisations are involved, each runs **its own half** and the halves agree on a **process contract**. There is no central engine owned by one party and no distributed transaction.

**5.2 The process contract.** A versioned, signed document that every participant holds:

- the **participants** by role (for example "requester", "approver", "supplier") and, for each, whether it is a platform tenant or an external system;
- the **messages**: topic, schema version, direction (which role sends to which), the fields that may cross (nothing else crosses, ADR-058 rule 6);
- the **order rules**: which message may follow which, which end a conversation, and the time limit of each wait;
- the **outcome states** of the whole conversation.

The contract is data, validated by a checker, diffable between versions, and carried in bundles. A change to a contract is a new version; a running conversation stays on the version it started with (ADR-49 rule for versions).

**5.3 Two new step kinds.** `send message` (contract message, payload mapped from the request's variables, validated against the contract before it leaves) and `wait for message` (a contract message, with a time limit and what to do on timeout). Both carry the **conversation id**, created by the first send or the first receive, so every reply is correlated to one request on each side. A received message starts a request on the receiving side (a new trigger kind: message arrived) or resumes a waiting one.

**5.4 Carrier.** Messages travel over a **tenant link** with a message topic (ADR-058 rules 8 to 14): `direct` or `pull` between platform tenants, `external` through a connector or signed webhook for a peer that is not a tenant. A send is allowed only while a grant covers that message topic and direction. Every state of delivery (queued, sent, acknowledged, applied, rejected, expired) is visible on the sending request, so a user can see "waiting for the supplier's server" instead of a silent stall.

**5.5 Disconnected and slow peers.** Delivery is asynchronous and at least once; the receiving side applies each message once. A wait time limit may be shorter than the connection, in which case the process takes its timeout or escalation branch like any other wait. A participant being unreachable is a condition shown on the request and on the health surface, not an error that loses the request. Nothing needs both sides online at the same time.

**5.6 Conformance.** The engine checks every outgoing message against the contract before sending and every incoming message on arrival: an unknown message, a message not allowed after the previous one, a schema outside the accepted range, or one from the wrong participant is rejected with a reason code the sender receives, and the incoming one goes to the quarantine queue for a person. A conversation that cannot continue ends in the contract's declared outcome state, or in a `needs attention` state with the history.

**5.7 Authority and privacy.** No participant writes another's records. A message is information; the receiver decides what to do and does it through its own lifecycle. Only contract fields leave a tenant, and the sender's residency policy applies. Credentials for external peers are references to the vault.

**5.8 More than two parties.** A contract may have any number of roles. Each pair of participants that exchange messages needs its own link and grant. A participant that must forward information sends it as a new message on the next link; the platform does not relay.

**5.9 Cancellation and failure.** If a participant cancels, it sends the contract's cancellation message; the other side's request takes its declared compensation path (WP-BPM-09), such as a return step. A rejected message stops the sender at the step's error branch.

**5.10 The designer.** The flow designer (WP-BPM-07) shows the other participants as labelled lanes whose insides are hidden, with the contract's messages as the only connections to them. It edits the contract in a side panel (shared with the other participants as an exported file or over a link), checks the local half against the contract, and offers a **partner simulator** that plays the other lanes from the contract so a flow can be tested with no partner connected.

### 6. Side effects, undo and going back

A process moves through steps and each step may change records and trigger side effects: balance rows written by a posting, stock-like movements, outbound messages. The process must know about them, or it cannot go back. This section settles where those effects live and how the process learns of and reverses them.

**6.1 Where effects live (already true, now a rule).** Effects are **pipeline steps** (ADR-51) contributed by apps through the seam registry, not code inside the record type's own controller. Internal effects (writes to append-only balance and movement stores) run in the `after_` phase of the lifecycle operation, inside its transaction. External effects (mail, bank and connector calls, webhooks, fiscal transmission, messages to other organisations) are announced in the `_committed` phase through the outbox with an **effect class** (`spot-effect-hold`). So effect logic is separate from record logic, visible in the seam registry and `explain`, and replaceable by an app. A lint rejects a controller that performs an external effect directly.

**6.2 What is missing today.** A step has no machine-readable statement of what it writes or how it is reversed. A lifecycle operation returns no record of the effects it caused. The effect hold exists only for a migration. The engine has no undo model beyond compensating when an upstream record is cancelled (WP-BPM-09).

**6.3 Effect declaration.** Every effect-bearing pipeline step declares, in its registration, `effects`: for each, `kind` (`internal` or `external`), `class`, what it writes (store or channel names), and its **reversal**:
- *reversible*: undone by a named lifecycle operation of the same record (post is undone by cancel; amend by correct). An internal effect must be reversible. The cancel pipeline contains the paired reversing step, and a conformance kit checks the pairing: post then cancel leaves every declared append-only store net zero, and nothing is deleted.
- *compensable*: cannot be undone, but a named compensating action exists (send a correction, void a request).
- *irreversible*: nothing can undo it (an issued fiscal transmission, a message already acted on). It is a **point of no return**.

**6.4 Receipts.** Every lifecycle operation returns an **operation receipt**: the operation, the record id and version, and one entry per effect (`step`, `class`, `kind`, opaque store ids, count, digest, `state`: applied, held, released, delivered or failed). The engine stores receipts in the request's record file. It treats the ids as opaque and never reads the balance or movement stores; it understands classes and reversibility only. A receipt carrying an effect class the step did not declare is a conformance failure in the test kit and a logged event in production.

**6.5 Rewind and undo are different.** *Rewind* sends the request back to an earlier step to revise (a rejected approval returns to the form step); the effects already produced stay, and the revision goes through update or correct on the same record. *Undo* sends the request back **and** reverses effects. The designer makes the difference explicit on each backward edge.

**6.6 Undo is a compensation sequence.** The engine reverses effects in reverse dependency order, taken from the record file (a record is reversed after the records that depend on it), through the declared reversal operations of the lifecycle: every record type's own rules decide whether the reversal is allowed. Each reversal is idempotent and journaled. When a reversal is refused (dependants outside this request, a closed period, a rule of the type), the sequence stops in a `needs decision` state that lists the blockers and the **alternatives the record type declares** (for example a reversing record dated in the current period), and waits for a person. Undo never forces anything and never deletes a balance or movement row (the hard floor of WS13 and ADR-49).

**6.7 External effects wait for commit points.** A process may mark **commit points**. External effects produced in the region before a commit point are **held** in a hold scope owned by the request (a generalisation of the migration hold) and released at the commit point; if the request is undone or cancelled first, they are discarded and nothing left the building. Effect classes that must never be held (fiscal transmission, ADR-43) are irreversible and mark a point of no return at the step that causes them. A message sent to another organisation is an external effect of class `link_message`; it is compensable only by the contract's cancellation message (section 5.9).

**6.8 Checks at design time and in tests.** `validate` reports: an undo edge that passes an irreversible effect without a declared compensation; a step whose record type has effects with no declared reversal; a commit point with nothing to hold; a rewind that would create a second record where the revision should update the first. A test run in the designer lists the effects each step produced, as receipts, so the analyst sees them before going live.

**6.9 What the record type keeps.** It owns whether a reversal is allowed and how. The process asks and reports; it never reaches into a store, and it never decides that a ledger-like row may be removed.

**6.10 Contract status.** The effect declarations, receipt types and `HookContext::report_effect` of sections 6.1 to 6.4 are in `contracts/app-api` 0.5.0 (additive; see its CHANGELOG). Effects ride beside `StepDecl` through `Controller::step_effects`, so every 0.4.0 controller still compiles. The engine's collection of reported effects into a receipt is not part of the contract and follows in later tasks.

### 7. What this does not decide

- Which sector packs exist (ADR-060 keeps those stages closed).
- Any ordering guarantee across links: ordering is per conversation only.
- Shared durable state between participants. There is none; each side's request is its own record of the conversation.

## Consequences

- **ADR-49** keeps all its rules. Governance (WP-BPM-08) gains the `process_only` mode and the entry step, and BPM user-facing text takes the label pair.
- **ADR-058** is amended in the same change (rules 8 to 14, work packages WP-CTL-09 to 11). **None of the tenant-link machinery is built or planned today**, so cross-organisation processes (section 5) cannot run until WP-CTL-09 and WP-CTL-10 exist. Single-tenant processes are unaffected and can proceed now.
- **WP-EDG-06** (replication apply path): the apply path records delivery notices for `notify` topics. No change to its rule of running no business logic.
- **Work packages** (added to the blueprint and the plan on 17 Oct 2026):

  | ID | Work package | Depends on |
  | --- | --- | --- |
  | WP-BPM-12 | Process contracts and conversations: contract model, checker and versioning; `send message` and `wait for message` steps; message-arrived trigger; correlation by conversation id; conformance checks; delivery states on the request | BPM-02, BPM-03, CTL-10 |
  | WP-BPM-13 | Process-first entry: the form step and its draft-record semantics; `process_only` mode and the "Start" replacement for "New"; the run label pair | BPM-04, BPM-05, BPM-08, UI-03 |
  | WP-BPM-14 | Designer support for participants, contracts and the partner simulator (Avalonia) | BPM-07, BPM-12 |
  | WP-EXT-11 | Effect declarations on pipeline steps and manifests (section 6.3), the paired-reversal conformance kit, the lint against effects in controllers | EXT-02, EXT-04, DOC-04 |
  | WP-DOC-12 | Operation receipts returned by every lifecycle operation (section 6.4) | DOC-04, EXT-11 |
  | WP-BPM-15 | Effects, receipts and undo in the engine: record file with receipts, rewind and undo, compensation sequence with blockers and alternatives, request-scoped hold and commit points, design-time checks and designer display | BPM-02, BPM-04, BPM-09, DOC-12, MIG-08 |
  | WP-CTL-09 to 11 | Tenant link registry, link exchange, external peers (ADR-058) | as listed in ADR-058 |

- **Existing briefs amended on acceptance:** WP-BPM-01.T1 (node kinds `form`, `send_message`, `wait_message`; participants in the model), WP-BPM-03.T1 (message-arrived trigger), WP-BPM-05.T1, WP-BPM-07.T1/T2 and WP-BPM-11.T1 (label pair instead of the literal "Case" in user-visible text), WP-BPM-08.T1 (the `process_only` mode, with no bypass permission in that mode), WP-BPM-04.T1 (the document step returns the operation receipt; compensation hooks become the declared reversal), WP-BPM-09.T1 (cancellation compensation reuses the undo sequence), WP-MIG-08 follow-up (hold scope generalised from migration to request), WP-EDG-06.T1 (delivery notice for `notify` topics). Until then, WP-BPM-01.T1 keeps its node kinds extensible, so nothing built now has to be undone.
- **Limits.** Two organisations get eventual consistency and an audit trail on each side, not a shared ledger. A conversation whose peer never answers ends by timeout or escalation, not by magic.

## Decisions of the architecture owner (17 Oct 2026)

1. **Default label:** "Request", kept.
2. **Mode granularity:** a sensible default is used for now (a mode per record type, narrowed by organisation unit scope where a setting asks for it). To be revisited after the architecture owner has modelled real processes.
3. **Bypass in `process_only`:** none. Internally everything is a process (section 2.3).
4. **Who signs a process contract:** an implementation detail, decided when the first cross-organisation process needs it. Until then a contract has one author and participants accept a version.
5. **Message retention bound (ADR-058):** configurable per tenant, default 30 days, range 1 day to 1 year.
6. **Ciphertext relay (ADR-058 rule 11):** deferred. Kept in the TODO list for later.

## Still open

1. **Timing.** Single-tenant process-first operation (sections 1 to 4) can start with the current engine briefs. Cross-organisation processes (section 5) wait for the link work packages. Effects and undo (section 6) need WP-EXT-11 and WP-DOC-12 first and should be in place before the engine is used for anything that posts to balance stores.
2. **Batch start.** The exact shape of "one request owns many records" for imports (section 2.3), which touches ADR-071.
