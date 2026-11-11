Part of [Running Spot](../running-spot.md).

# Document nodes

The Document node (`crates/framework/spot-bpm-documents`, hosted here) is a Process step that
creates, updates, posts or cancels a document. It never writes a record itself: it asks the
document engine's own lifecycle (`create`, `update`, `post`, `cancel`) to do it, exactly as a
user's click would, so every permission check, validation, field guard, audit entry and
numbering decision runs the same way for a node as for a person (ADR-49, ADR-080 §1). This
module wires that handler to the site's own running document engine and PostgreSQL database,
and registers it with the process engine's `HandlerBuilder` port (`services::process`, a
smaller `ORDER` than this module's `1110`).

## Service principals

A Document node may run as the Case's own actor (a plain user id), or it may name a
`principal` to act with a different identity's rights. A named principal must be:

1. **registered** in `[services.process_documents]` (see below);
2. **on the allow-list** the registered entry declares, for the author of the process version
   the Case is running (an author exempt from the allow-list check if the version is the
   platform's own — exactly the rule the publish guard below applies);
3. successfully **audited** before the node acts.

Any failure of the three is `bpm_documents.principal_not_registered` or
`bpm_documents.principal_not_permitted`, fail closed, never a silent fall back to the Case's
initiator or a system actor.

**The principal's rights are its own**, never the initiator's: this module resolves a
registered principal's name to the platform **user** named in its settings entry and runs the
lifecycle operation as that user (`Actor::User`), so the document engine's normal role-based
permission and field-guard checks decide exactly as they would for that user acting directly.
A node can never write a field its principal's user may not write.

## Configuration

```toml
[services.process_documents]
enabled = true            # default; false makes the module inert (no handler, no port)

[[services.process_documents.principals]]
name = "billing_bot"               # the name a node's `principal` names
user = "<record id>"               # the platform user whose role grants the principal runs with
grants = ["platform", "user:<record id>", "process:<process id>"]
```

`grants` lists who may use this principal: `"platform"` (a process version the platform itself
published), `"user:<id>"` (a version a named user published) or `"process:<id>"` (a version
another process published). A malformed `user` id, an unparseable grant, or a duplicate `name`
refuses the site at start-up, naming the entry.

## The Case file

Every document a node touches is recorded as one edge `case -> document` in the business
graph (`spot_bpm_documents::CaseFile`, the `bpm_case_file` edge type), carrying the node, the
operation and the opaque receipt. The document engine does not yet expose a transaction-bound
participant seam for this (WP-DOC-04.T3; see the crate's own module docs), and this host's own
participant table (`services::participants`) is built once in `host.rs` before any module
registers, with no seam to add one either — so the edge is written **immediately after** the
lifecycle call commits, on its own PostgreSQL connection, never inside the same transaction.
A crash between the two leaves a document with no edge yet; see Idempotency below for how a
replay still lands on exactly one document. This is an interim, awaiting the lead's decision
(release question of brief item 3): the edge joins the document transaction only when the
participant order exposes a place for it.

## Idempotency without the engine's own replay

The document engine does not yet replay a repeated command id end to end (WP-DOC-04.T8). Before
calling the engine for a node attempt, this module asks the durable Case-file edge store
(`CaseFileEdgeStore::edge_exists`) whether `(case, document, node, operation)` already
committed. When it has — the write committed, but the process crashed before the node recorded
its outcome — the node settles from the document's current state instead of calling the engine
again, so a restart never creates a second document. The other crash window (the document
committed, the edge not yet written) finds no edge, so a `create_from` calls the engine with
the same id derived from the node attempt's key; the engine answers `document.duplicate`, and
the node then loads that derived document as the same actor and settles on it (writing the
missing edge) instead of failing. If no document with that id exists, the duplicate was a
unique-field collision and the refusal stands. `update`, `submit` and `cancel` in that window
meet a stale version instead, until WP-DOC-04.T8 lands. Once WP-DOC-04.T8 lands the engine answers
the repeat itself; this check stays as a cheap first line.

## The publish-time guard

`PublishGuard` is provided as a port (consumed by a smaller-`ORDER` module than this one has —
a larger one, rather: the publish path, WP-BPM-09.T4, not yet hosted). Once wired, publishing a
process version with a service-principal Document node will require the platform permission
`process.publish_service_principal` and every named principal's allow-list to cover the
version's author, checked **at publish**, not only at run time.

## Health and metrics

The hosted service (`process_documents`) is `Level::Optional`: a Document node that cannot run
degrades the processes that use one, never the whole site (a process with no Document node is
unaffected). Its readiness check only confirms the process engine itself has finished starting.

## Known limitations

- **The Case-file edge is not transactional with the document** (see above): a crash between
  the two is covered by the idempotency pre-check, but an operator reading the Case file
  immediately after a crash and before the node resumes would see the document without its
  edge for a moment.
- **Service principals are a static list**, read from site configuration, not a dynamic IAM
  directory: `spot-iam-directory` carries no service-principal concept today. The
  `ServicePrincipals`/`CaseAuthors`/`ServicePrincipalAudit` ports this module builds are the
  crate's own injected seams; a future IAM-backed directory can replace `ConfiguredPrincipals`
  without touching the handler.
- **The service-principal audit trail** is a `bpm_principal_use` business-graph edge from the
  Case to a node keyed by `(case, node, principal)`, read back the same way the Case file is;
  the site has no durable audit chain yet (`services::participants`' own known limitation), so
  this is the audit surface until one exists.

## Fail-closed identity

A node acts as a registered service principal's backing user or as the Case's own user-id
actor, nothing else. A node with neither (a trigger- or timer-started Run has no acting user,
and names no principal) is refused with `bpm_documents.principal_invalid` and the engine is
never called; the platform actor is never built here. Failure reasons carry the stable code and
refused field paths only, never the engine's free-text message or any field value. Service-
principal use is audited before the node acts, so an engine refusal still leaves who tried;
a refusal by the principal policy itself (not registered, not permitted) is returned as the
node failure but is not written to the audit edge by `spot-bpm-documents`.
