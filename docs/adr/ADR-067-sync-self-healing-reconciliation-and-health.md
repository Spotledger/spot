# ADR-067: Self-healing: conditions, repair loops, reconciliation and the health surface

- **Status:** Accepted by the architecture owner, 26 Oct 2026, together with ADR-061 to ADR-067 as a group. Proposed by the lead, 2 Oct 2026. Open questions that state a proposal or starting value are accepted at that value (thresholds stay tunable); the open questions that stated none were answered on 26 Oct 2026 (ADR-062, "Open questions answered after acceptance").
- **Date:** 2 Oct 2026
- **Layer:** Platform (series ADR-062; the conditions mechanism is general, the catalogue here covers the journal and sync)
- **Builds on:** ADR-23 (reconciliation, lag dashboard), ADR-26 (the ERP knows itself), ADR-061, ADR-062 to ADR-066, WS13 (support agent), WS22 (Admin Center)
- **Raised by:** the architecture owner, 2 Oct 2026: the system should be feature rich "without boiling people's heads with complexity", a sort of **self-healing** system, like the rest of the platform.

## Context

Five ADRs define how data moves and what the system promises. None of the promises is worth much if the first sign of trouble is a user phoning head office. Real networks produce partial failures every week: a cut link, a restored backup, a bug in an apply path, a clock that drifted, a branch that was never upgraded. Each of these needs a **detector**, an **automatic response that is safe**, and, only when automation is exhausted, a **plain request to a person**.

ADR-23 already asked for daily reconciliation per branch and a lag dashboard. This ADR generalises them into one mechanism. Its measure of success is not the number of dashboards. It is that **an operator sees a single, calm status that is usually green, and when it is not, is told in plain words what happened, what the system already tried and what to do**.

## Decision

### 1. Conditions: every abnormal state has a name

A **condition** is a detectable abnormal state with a fixed definition:

| Part | Meaning |
| --- | --- |
| `id` | A stable name, such as `MISSING_DEP` |
| `subject` | What it is about: a node, a stream, an entity, a record, the tenant |
| `severity` | `info`, `amber` (the system is handling it, or it is minor) or `red` (a person is needed or damage is possible) |
| `detector` | The check that raises it and the check that clears it |
| `response` | The automatic actions, in order, with their limits (section 3) |
| `escalation` | When and how severity rises if the response does not work |
| `explanation` | A plain-language sentence template with parameters, translated like any user interface text |
| `action` | The operator commands that apply, if any (section 6) |

- A condition is **raised, handled and cleared**. It is **de-duplicated** per subject (one open `MISSING_DEP` per node), **debounced** against flapping (a condition that clears and returns within a short interval is one condition, not many) and **timestamped** at each transition.
- Every transition is an **audit event** (`sys_condition_raised`, `sys_condition_cleared`, ADR-061), so the history of a branch's troubles is part of the replayable record, and the support agent and auditors can read it.
- Conditions are **platform data**, not code scattered across services. An app may register its own (for example a projector failure) and gets the same lifecycle, display and notification.

### 2. The catalogue (journal and sync)

Defaults are starting values. Administrators tune thresholds, not behaviour.

| Condition | Raised when | Default | Automatic response | A person is needed when |
| --- | --- | --- | --- | --- |
| `NODE_SILENT` | A node has not contacted the hub for more than 3× its usual interval, or beyond the freshness limit | amber, red at `max_staleness` | Notify the node's contact by mail | The branch should be online and is not |
| `LAG_HIGH` | A node's applied prefix trails the stream beyond a threshold while connected | amber | Prioritise its catch-up (ADR-066) | Lag keeps growing (a capacity or link problem) |
| `OBLIGATION_OVERDUE` | A mandatory obligation passes its deadline (ADR-065) | amber, red after grace | Mandatory lane first, notify contact, then notify `sync_admin` | Red |
| `MISSING_DEP` | A bundle is parked for a missing dependency (ADR-064) | amber, red after 24 hours | Pull-on-miss repair with backoff | The owner does not have the record (the bundle moves to quarantine) |
| `SCHEMA_HOLD` | A record is held because the node cannot decode it (ADR-066) | amber | Release and re-request after upgrade | The node stays too old |
| `NODE_TOO_OLD` | A node is beyond the skew limit | red | Offer the upgrade channel | Always: the branch needs the new version |
| `SCHEMA_RETIRE_BLOCKED` | A major version cannot be retired because nodes still need it | amber | None | A node must be upgraded or suspended |
| `QUARANTINED` | An ownership violation, integrity problem or circuit-broken record | red | Capture the error and context | Always |
| `PROJECTOR_FAILED` | A derived-data projector failed on apply (ADR-064) | red | Retry once | An app defect |
| `DIVERGENCE` | Reconciliation finds a mismatch that persists (section 4), or equal versions with different content | amber | Repair from the owner (section 3) | The owner's own data differs (nothing to repair from) |
| `NODE_AHEAD_OF_OWNER` | A node holds a newer mirror version than the owner (ADR-066) | red | None. It never overwrites | Review and approve recovery |
| `CLOCK_SKEW` | Node clock differs from the hub (ADR-065) | amber over 2 min, red over 5 min | Use corrected time for effective dates | Red |
| `STAMP_VIOLATION` | A document was posted without a mandatory change already published (ADR-065) | amber | Create a finding in the queue | Review the finding |
| `ADOPTION_PENDING` | An adoption request has waited beyond a threshold (ADR-063) | amber | None | The branch is offline too long |
| `EXTRAS_GROWTH` | A destination's extras set grows beyond a threshold (ADR-064) | info | None | A scope or reference design problem |
| `CLOSURE_TOO_LARGE` | A bundle's closure passes the hard limit (ADR-064) | red | Refuse to send | A metadata design problem |
| `BOOTSTRAP_STALLED` | A snapshot makes no progress for a threshold | amber | Resume with backoff | It stays stalled |
| `SERIES_LOW` / `SERIES_ADVANCE` | A node's numbering range is running low, or was advanced after a restore (ADR-066) | amber / info | Allocate the next range when connected | The node is offline and nearly out |
| `HEAL_BUDGET_EXHAUSTED` | Automatic repairs for a node hit their budget (section 3) | red | Stop repairs, wait | Always |
| `CERT_EXPIRING` | A node identity certificate is near expiry | amber, red at 3 days | Renew automatically when connected | Renewal fails |
| `RPO_BREACH` | The journal's durability policy is not being met (`remote-sync` unreachable, `degrade`) (ADR-061) | red | Re-ship as soon as possible | Persisting |
| `JOURNAL_GAP` | A segment, chunk or manifest is missing, or a hash chain breaks (ADR-061) | red | Re-seal from the database where it is still held | Always |
| `JOURNAL_AUDIT_MISMATCH` | A scratch rebuild does not match the live database (ADR-061) | red | Capture the first differing record | Always |
| `CAPTURE_MISMATCH` | A change-feed entry has no matching `doc_state` (ADR-061) | red | Re-capture from the current record | A defect in the write path |
| `KEY_BACKUP_MISSING` | The journal key backup is absent or unverified | red | None | Always |

The list is a catalogue, not a ceiling. A new failure mode that is found in testing or production is **added to it with its response**.

### 3. Repair loops: desired state against observed state

Healing is built as **level-triggered reconcilers**, in the manner of control loops, not as one-shot scripts that assume nothing else happened.

- For each node and stream there is a **desired state**: the owner's records in the node's scope at the stream's position, with every obligation satisfied. There is an **observed state**: the node's applied prefix, holds, digests and versions. A reconciler computes the **difference** and chooses actions. Run it twice and the second run does nothing.
- **Actions** are a closed set: resend a bundle, send a repair bundle (ADR-064), re-bootstrap a record range (ADR-066 snapshot for a subset), release held records, re-request after upgrade, advance a numbering series, renew a certificate, retry apply, notify a contact or role. Each is idempotent.
- **Each action is audited** as `sys_heal_action` (what, why, against which condition and node, result). This is the **healing journal**. An operator or the support agent can answer "what has the system tried?" at any time.
- **Backoff and limits.** Actions retry with backoff (as in ADR-064). A **budget** bounds the work per node (default 50 repair bundles and a fraction of bandwidth per hour). Exhausting it raises `HEAL_BUDGET_EXHAUSTED` instead of looping. A **circuit breaker** stops an action that keeps failing and escalates.
- **What automation may never do** (ADR-062 rule 6), enforced in the action set itself, not by convention:
  1. edit or delete an owner's posted financial record;
  2. delete any data (mirrors are rebuilt by overwriting from the owner, and removing a mirror is an explicit operator action);
  3. cross ownership, or overwrite a mirror that is **newer** than its owner (`NODE_AHEAD_OF_OWNER`);
  4. approve a recovery, change a policy, suspend or decommission a node, or upgrade software;
  5. exceed its budget.
- Safe automatic overwriting is **repair apply**: a mirror whose version is at or below the owner's is replaced by the owner's image. Because a mirror is read-only to users (ADR-063), a mismatch means a bug or corruption, and the owner's copy is by definition correct.

### 4. Reconciliation: finding what nobody noticed

The conditions above find problems the protocol can see. Reconciliation finds **silent divergence** (a bug, a manual database edit, a lost write) by comparing state. Three levels, cheapest first.

| Level | What it compares | When | Cost |
| --- | --- | --- | --- |
| **L0** Position and acknowledgement | Applied prefix, holds, versions, obligation ledger (ADR-065) | Continuous | None beyond the protocol |
| **L1** Range digests | For each entity in scope, the **set digest** of its records: an order-independent combination of a hash of `(id, version, content digest)`, bucketed by a hash of the id so buckets are even. Mismatching buckets are drilled down to record lists | On reconnect after a long absence, after bootstrap, and hourly | Low: a few hundred bucket digests per entity, drilled down only on mismatch |
| **L2** Business fingerprints | App-registered fingerprints: counts and totals that mean something to the business (ledger by account, stock by item, document counts by day). The hook is generic. Apps define the fingerprints (ADR-23's daily list, WP-EDG-07) | Daily, off-hours | Moderate, budgeted |

**The settling rule.** The two sides are never perfectly still. A digest taken while changes are in flight can differ for good reason. So a mismatch is **reported only if it persists** across a second comparison taken after the node's applied prefix has moved past the first comparison's hub position. Anything that settles by itself is never raised.

**Direction rule.** The comparison is always against the **owner**:

- A mirror that differs from its owner's record at the same or a lower version is **repaired** from the owner (section 3). A mirror missing at the node is sent. A mirror that exists at the node but not at the owner is an **orphan** and is reported, never auto-deleted.
- A mirror **newer** than its owner is `NODE_AHEAD_OF_OWNER`, not repaired.
- A node-owned record compared at the hub is repaired by the same rules in reverse: the node's own copy is authoritative, and a differing hub copy is overwritten from it. A node-owned record the node does not have (a lost disk) is `origin_recovery` (ADR-066).
- **Up-flow fingerprints are easy:** posted branch documents are immutable, and the hub knows the branch stream position it has applied, so both sides compute over the same immutable set.

### 5. Self-checking of the journal

The journal's own conditions (`JOURNAL_GAP`, `JOURNAL_AUDIT_MISMATCH`, `CAPTURE_MISMATCH`, `RPO_BREACH`, `KEY_BACKUP_MISSING`) belong to the same mechanism, so a tenant has **one** health view for "can I recover?" and "is my network in sync?".

### 6. The health surface: calm by default

- **Three colours, one sentence.** Each node, and the tenant as a whole, has a status that is the worst of its open conditions: green, amber or red. Next to each is **one sentence in plain language**: "Branch 12 has not connected for 31 hours. The new tax rate is due there at 6 pm."
- **"Everything verified at 03:10"** is shown when the last L1 cycle passed. Calm is something the system states, not the absence of news.
- **Needs you.** One short list, for conditions the automation has exhausted or cannot handle, ordered by business impact. Each item shows: **what happened**, **what the system already tried** (from the healing journal), and **what to do**. Where an operator command applies, it is a labelled button, never an instruction to run commands. Raw logs are behind a link, not in the way.
- **Operator commands** are the manual form of the automatic actions (resend, re-verify, release, re-bootstrap a range, suspend or resume a node, approve a recovery, mark a finding reviewed). Each is permission-controlled, confirmed, audited and idempotent. None is required for normal operation.
- **Notifications are deliberate.** Red notifies the responsible roles and the node contact. Amber appears in a daily digest and on screen. Info is not pushed. A notification always carries the one-sentence explanation.
- **For machines too.** Conditions are readable through the stream pull API (audit class, ADR-061) and the native API, so an external monitoring system can subscribe. Metrics (lag, parked counts, repair rates, session durations) flow through `spot-telemetry` as telemetry, not through the journal.
- **For the support agent.** The conditions catalogue, the healing journal and the `explain` tools of ADR-063 are the **tools of the support agent** (WS13). It can read conditions, explain "why does branch 12 not have this record?" and propose operator commands. It acts within the same limits and approvals as a person, and a destructive step is never taken on its own.

### 7. Proving it works: deterministic simulation

Testing a distributed system with sleeps and a few integration tests proves little. The series is tested by a **deterministic simulator**:

- One process runs a hub and N nodes under **virtual time**, a **virtual network** and a seeded random source. A seed reproduces a run exactly.
- **Faults are injected** on schedule: dropped, duplicated, reordered and delayed messages; connection cuts at every protocol step; node crashes mid-apply and mid-snapshot; restored backups (nodes and hub); disk replacement; clock skew and drift; nodes on old versions; a hub migration epoch in progress.
- **Properties are checked after every run**, as the machine-checkable form of ADR-062's guarantees:
  - **G1:** no record is ever changed by a non-owner.
  - **G2:** after faults stop and a bounded time passes, every in-scope change is applied at every active node, or an open condition names it.
  - **G3:** no destination ever holds a partial bundle (a record without its dependencies, or a root without its satellites).
  - **G4:** the final state is identical across all seeds that differ only in delivery order and duplication.
  - **G5:** at every moment, the ledger's answer to "who has applied X?" matches the nodes' actual state.
  - **G6:** no condition stays open without an action in progress or a recorded exhaustion. Every red has an explanation and an operator action.
- The simulator runs in CI (a fast profile on every merge, a long profile nightly) and reproduces any production incident class by seed. WP-SYN-11 is built around it. The earlier real-database chaos tests of the Event Delivery report remain, for the storage layer.

## Consequences

- **WP-SYN-09** implements conditions, reconcilers, the healing journal, range digests and the fingerprint hook. **WP-SYN-10** is the health surface. **WP-SYN-11** is the simulator and its properties. **WP-EDG-07** (drift reconciliation and lag dashboard) is subsumed.
- **Platform conditions framework** (WS22): a general registry, lifecycle, de-duplication, notification and display. The journal and sync catalogues are its first users. Other subsystems (the job engine, cache, integrations) may register conditions later.
- **Metadata** (WP-MET): fingerprints register through the app manifest (WP-EXT-04), like topics.
- **Support agent** (WS13): this catalogue and journal are its sync tools. Its acceptance gains "explains and proposes a fix for a seeded sync condition".
- **Document engine and apply path**: `repair apply` is an apply-path variant with the version rule of ADR-064 and the safeguards of section 3.
- **Not changed:** ADR-23's ownership, ADR-053's contracts, the frozen crates.

## Open questions for the architecture owner

1. **Default thresholds** in section 2 (3× interval, 24 hours to red for `MISSING_DEP`, budget 50 repair bundles per hour). Start there and tune from the simulator and pilots?
2. **Automatic overwrite of mirrors** from the owner (repair apply). Confirm this is acceptable without approval, given that mirrors are read-only to users. The alternative is approval for every repair, which would bury operators.
3. **Notification channels**: mail only in v1 (via `spot-mail`), with SMS and chat later?
4. **Deterministic simulator as a gate**: should a failing simulator property block merges of sync work packages from the start?
   - **Answered by the architecture owner, 26 Oct 2026 (accepted as recommended):** Yes, from the first sync work package: a failing simulator blocks merges of sync work packages.

## Alternatives considered

- **A dashboard of metrics and logs.** Rejected as the primary surface: it is what "boiling people's heads" looks like. Conditions give a decision (green, amber, red) and a sentence. Metrics remain for engineers.
- **Alert on every anomaly.** Rejected: operators learn to ignore a stream of alerts. Amber is handled by the system and appears in a digest. Only red interrupts.
- **Manual repair tools only.** Rejected: most failures are routine, and every manual step is a chance to repair the wrong way. Automatic repairs are bounded and audited, and the manual forms exist for the rest.
- **Automatically overwriting a node that is ahead of its owner.** Rejected (ADR-066): after a hub loss, the branch may hold the only copy.
- **Full-state comparison between hub and nodes.** Rejected: too heavy. Range digests find a difference cheaply and drill down only where it exists.
- **Integration tests with real time and sleeps as the main proof.** Rejected: they are slow, flaky and cannot reproduce a failure. A seeded deterministic simulator does all three.
