Part of [Running Spot](../running-spot.md).

# The access-question model (`services/access_questions_model.rs`)

The four access questions (what can this user do and why, who can do this, who can see this document, what
changed between two dates) run on a read model. This module builds that model from the **live permission
model** of the site, so an answer is the permission engine's own answer and not an empty table. It is a helper
module, not a service: the access-questions service asks a `LiveModelSource` for a model per question.

## What the model contains

| Part | Source |
| --- | --- |
| Roles | every stored `role` document (origin, copied-from, duty names), plus one reference role for every role name an installed app's `permissions/grants.toml` names that has no stored record |
| Direct grants | the shipped grant entries, field grants and named app permissions, attached to the role they name; the reference role records the app that shipped it, so "System Manager may read `user`" is answered "granted directly by role System Manager from app platform" |
| Duties | the definitions in each installed app's `duties.toml`, **by name only** (see below); a duty a stored role names that no app defines exists, empty, and is noted |
| Assignments | every `role_assignment`, active or not, of every user type, with source, validity and the `scope` rows; an assignment that is not `active` is kept with a closed window, so no question counts it |
| Portal users | the `user` records whose type is `portal`, so Barrier 3 of ADR-083 applies in every answer: a desk role held by a portal user grants nothing |
| Protection | the groups and the per-field protection facts of the served metadata |
| Shares | the shares the installed apps declare |
| History | the permission-change events of the access-change journal (below) |

**Duties grant nothing here.** The site's permission engine decides by role-named grants only and does not read
a duty's rights (see `roles.md`, "What the engine reads"). The model therefore keeps a duty's name (the
questions look for the access-governance duty by name, and the portal rule reads its outside mark) and drops its
rights, so an answer never says "allowed" where the engine says "no". Readiness notes
`access_questions_model.duties_not_enforced` with the count. The `role_duty` rows store a duty's name and pinned
version only, not its rights.

## The one agreement

For every (user, DocType, right) the model's answer equals the permission engine's decision on the same
snapshot. `tests/access_questions_model.rs` pins it with a randomised comparison over forty seeded matrices (six
users, seven roles including a role with a parent, active, ended and expired assignments, windows in the past
and the future): the model against an engine built independently from a hand-written registry, and, for roles
without a parent, against the site's own engine over role-named grants.

## When the model is rebuilt

One read-only, repeatable-read transaction reads roles, assignments and users together. The built core is
cached and rebuilt when the role-snapshot version the roles service publishes changes, when the served metadata
changes (a customisation swap), or when it is older than `max_age_seconds` (5; the cross-node bound). A rebuild
runs once under a lock, so a burst of questions reads the directory once.

## The access-change journal

Permission changes are read from a hash-chained journal of their own, scope `access-changes:<site>`, in the
platform's audit store. An entry has kind `access_change`, the actor as `user:<id>` or `system`, the time in
microseconds and a payload of two values: the format version (1) and the canonical JSON of a
`spot_iam_governance::ChangeEvent`. A flow that changes access (a grant, an approval, an emergency start or end)
appends its event with `history::record`; an event over 6 KiB is written without its before and after snapshots
and flagged `trimmed`, so the entry never fails for size.

A window is read by binary search on the entry time, then a forward scan that stops one day (clock skew) after
the window. An entry the reader cannot interpret (unknown format version, not JSON, not an event) is skipped and
counted (`access_questions_model.history_skipped`), never repaired. A window that needs more than 100,000 entries
is refused (`history_too_large`) instead of answered in part.

## Failure

Fail closed, and never silent:

- the role-snapshot port exists but has no snapshot yet, the directory cannot be read, the roles do not form a
  registry, or an app file does not parse: every question refuses with the code of the failed part; the previous
  core is not served in its place;
- the journal cannot be read: only the history question refuses (`history_unavailable`); the other three keep
  answering, and readiness is degraded with that code;
- a site with no role-snapshot port rebuilds on the timer alone and notes `snapshot_unversioned`.

## What the reviewer sees in an answer

Every row carries the assignment, the role, the path of roles from the assignment's role down to the one that
defines the right, the source, the scope and the decision core's own rule. The history carries who, what, when
and why, and never a before or after snapshot.
