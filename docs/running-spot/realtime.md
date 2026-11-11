Part of [Running Spot](../running-spot.md).

# Realtime

`GET /api/v1/realtime` upgrades to a WebSocket speaking `spot.v1`. The first message is
`auth` with a session token or an API key, checked exactly like an HTTP bearer; a user who has
not yet chosen a password is refused. Rooms are decided by the site's permission snapshot:
`doctype:<name>` needs list read on the DocType, `doc:<name>:<id>` needs read on that document
(including its owner and state rules), `user:<id>` only that user. A notification carries
identifiers, never values. A permission change never closes a
connection: when a new permission snapshot (or a removed role assignment, noticed within a few
seconds) no longer lets a principal read a room it joined, the server takes the principal out of
that room. Its connections stay open and in their other rooms (its own `user:` room included),
no further event of the room reaches them (events already queued for them included), and the
client may ask to join again and is decided afresh. A connection is closed with code 4009 only
when its **session** ends: the token expired, was revoked (sign-out, a password change) or the
user was disabled; the server asks again every few seconds.

A hook's `ctx.notify_user(user, payload)` reaches an open connection of that user, in its
`user:<id>` room, as a `notification` event (`event = "user_notice"`, the payload under `data`)
once the transaction commits; a rolled-back transaction delivers nothing and no other user hears
it. A notice carries ids and short scalars only, never a document's content. A change feed that
cannot be read degrades the site (`realtime`) instead of stopping it.
