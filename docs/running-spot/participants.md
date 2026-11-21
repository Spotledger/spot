Part of [Running Spot](../running-spot.md).

# The document audit

The audit participant is installed first in the participant order on every site
(`services::participants`), with a field masker built from the metadata **and the permission
decision** the site serves. Both are rebuilt for every metadata generation: at `spot reload` and at
every customisation swap, the participants (and with them the masker) are installed **before** the
new metadata, so no write is audited with an older mask than the metadata it runs under. A swap that
fails puts the previous participants back.

## What is audited, and what is not

Audited: every stored version of every DocType the site serves (create, update, post, cancel,
delete, method calls and corrections), written as a `sys_doc_audit` outbox event **in the write's
transaction**; and every refused operation, written as one `denied` event after the rollback in a
transaction of its own (class and code, never a message or a value).

Not audited: reads; the audit's own DocTypes; **replica apply** and **import**, where participants
do not run (they carry their own receive-side checks: the owner check on a replica record, the
import's own record).

## The masked form

A protected field (a protection group, or an unknown DocType) is written as a **digest**, never as
its value, in the outbox event and therefore in the chain. The masker is fail-closed: a DocType it
does not know masks every field.

## Lanes and the chain appenders

Events are chained by lane appenders: `[audit] doc_lanes` independent hash chains (default 4), a
document always in the same lane. A required service, `docaudit`, asks the job queue for one
`platform:doc_audit_append_<lane>` job per lane every `[audit] doc_append_poll_ms` (default 1000);
the job drains up to `[audit] doc_append_batch` events (default 200) from the lane's bookmark.
The queue's key and the appender's event index make a repeat, a restart and a second node harmless:
an event is chained once. Events older than the outbox retention are still chained if the appender
ran before the prune.

**The lane count is fixed for the life of the site.** Changing it needs a new `doc_lane_epoch` and a
migration of the lane heads, which this release does not provide; the appender refuses a stored
checkpoint that disagrees. Checkpoints are not signed yet: no key is configured (an open decision).

## Tables, the guard and start-up

`spot migrate` plans the chain tables and the four appender tables
(`spot_audit_bookmark`, `spot_audit_event_index`, `spot_audit_doc_index`,
`spot_audit_site_checkpoint`) and then installs the application role, its grants and the guard
triggers (`spot_audit_store::install_guard`). Start-up checks the tables **and**
`spot_audit_store::verify`; a site whose chain tables exist without the guard does not start and
says to run `spot migrate`.

## Readiness

`GET /api/health/ready`, line `participants:<site>`: `docaudit: on; <n> lane(s), pending <k>,
oldest <s> s; denials dropped <d>, not recorded <r>`. Warn only: a lagging appender is reported
as `(lagging)` and never pulls the node out of rotation.
