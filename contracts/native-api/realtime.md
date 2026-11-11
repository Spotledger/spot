# Realtime protocol v0.1

Part of the native API contract (WP-CON-03, ADR-055). The message schemas are `RealtimeClientMessage` and `RealtimeServerMessage` in `openapi.yaml`; `check.py` validates the examples there and checks that every message type is documented here.

**Design rule (ADR-25):** realtime is **notification, not data sync**.

- Events are published only after the transaction that caused them commits.
- Every room join is permission-checked.
- A slow client is disconnected rather than waited for.
- After a reconnect the client refetches what it shows.

No correctness may depend on a message arriving. Clients must behave correctly if they receive no events at all; they just update later.

## Connection

1. **Upgrade.** `GET /api/v1/realtime` upgrades to a WebSocket on the site's host name. The site is resolved exactly as for HTTP, and the subprotocol is `spot.v1`. A server that does not speak `spot.v1` refuses the upgrade.
2. **Authentication.** The first client message must be `auth`, within 10 s, carrying a session token or API key. There is no header-based authentication because browsers cannot set headers on a WebSocket.
   - On success the server sends `ready`, then automatically joins the client to its own `user:<id>` room.
   - A missing, late or invalid `auth` closes the socket with **4001**.
3. **Frames.** Every frame is one UTF-8 JSON text frame with one message. Binary frames close the socket with **1003**. A frame over 64 KiB closes the socket with **1009**.

## Rooms

| Room | Who may join | Events |
| --- | --- | --- |
| `doc:<doctype>:<id>` | Users who may read that document | Changes to that document |
| `doctype:<doctype>` | Users who may read the DocType's list | Any document of that DocType created, changed or deleted |
| `user:<id>` | Only that user; joined automatically | Notifications for the user |
| `site` | Every signed-in user | Site-wide notices, for example maintenance |

- A client may be in at most **200** rooms. The join that would exceed this fails with `join_error` and `realtime.too_many_rooms`.
- Permissions are checked on join. When permission is revoked later, the server removes the client from the room within the permission cache's lifetime. It does not re-check on every event.

## Client messages

- **`auth`**: `{type, token}`. First message only.
- **`join`**: `{type, ref, room, resume_after?}`.
  - `ref` is any string up to 64 characters that the client uses to match the reply.
  - `resume_after` is the last `seq` the client saw in that room, sent when reconnecting.
  - Joining a room the client is already in replies `joined` again, with no side effects.
- **`leave`**: `{type, room}`. Leaving a room the client is not in is a no-op, and gets no reply.
- **`ping`**: `{type}`. The server answers `pong`. Clients may also rely on WebSocket ping/pong frames.

## Server messages

- **`ready`**: `{type, user, heartbeat_seconds}`. It is sent after a successful `auth`. The server sends a WebSocket ping every `heartbeat_seconds` (25 by default), and closes a connection that sends nothing, pong frames included, for 2 × `heartbeat_seconds`.
- **`joined`**: `{type, ref, room, outcome, replayed?}`. `outcome` is one of:
  - `joined`: no resume was requested.
  - `replayed`: the events after `resume_after` follow immediately, in order; `replayed` says how many.
  - `resync_required`: the server no longer holds every event after `resume_after`. The replay buffer keeps at least the last 1,000 events or 5 minutes per room. The client is in the room, and must **refetch** whatever it shows from that room before relying on events again.
- **`join_error`**: `{type, ref, room, error: {code, message}}`. Codes: `permission.denied`, `realtime.unknown_room`, `realtime.too_many_rooms`.
- **`event`**: `{type, room, seq, kind, payload, published_at}`.
  - `seq` starts at 1 and increases by one per room on one site, with no gaps as seen by a client that stays connected. A gap means messages were lost: resync.
  - There is no ordering promise **across** rooms.
  - `kind` is one of `doc_created`, `doc_updated`, `doc_deleted`, `doc_posted`, `doc_cancelled`, `doc_renamed` or `notification`.
  - For document kinds, `payload` is `{doctype, id, version}` and nothing more. The client refetches the document if it needs the data. A client that already holds `version` or later may ignore the event.
  - For `notification`, `payload` is `{event_id, event, data}` and nothing more (contract 0.8.0):
    - `event_id` is a string naming the committed event. It is stable across a redelivery: the same notice never carries two ids, and delivery is **at-least-once**, so a client deduplicates on `event_id` and ignores a repeat.
    - `event` is a short lowercase token naming the notice class. `"user_notice"` is the only value this version defines. A client that does not know the value ignores the event.
    - `data` is a JSON object of at most **2 KiB** serialised (compact JSON). Its values are scalars (string, number, boolean, null), record ids, or arrays of those; there is no object and no array inside an array below the top level. No string, key or value, is longer than **256 bytes**. The server refuses a notice outside these bounds and never sends it, whole or trimmed. `data` is plain data: a client renders a string as text, never as markup or script, and never evaluates it.
    - A `notification` is a doorbell (ADR-25), not a data path: the client refetches whatever it needs. `data` carries identifiers and short scalars, never a field value the recipient may not read.
    - **Who receives it.** A `user_notice` is published only to the addressee's own `user:<id>` room, so only that user's own connections receive it. The addressee is the user the committed event names, never a field inside `data`, so `data` cannot redirect delivery. It is never a broadcast to a document or shared room.
    - **The keys of a `user_notice` `data` object** (contract 0.10.0). All are optional scalars, and a client ignores a key it does not know: `title` and `body` (strings, shown as plain text); `severity` (`"info"`, `"warning"` or `"error"`; absent means `"info"`, and a client treats an unknown value as `"info"`); `doctype` (a DocType name) and `id` (the record's id, present only together with `doctype`), which a client may turn into a link; and `kind` (a short lowercase token naming what happened, for example `"posted"`). The same limits apply as above: 2 KiB for `data`, 256 bytes for any string.
    - **Offline and replay.** A notice raised while the user has no connection is replayed on the room only as far as the room's replay buffer reaches (`replayed`, or `resync_required` when it does not). A client that sees `resync_required`, or that was offline longer, reads `GET /api/v1/notifications?after=<event_id>` (contract 0.10.0), which returns the caller's own notices after the last `event_id` the client has, from the durable record, and refetches what it shows.
    - **Not in this version:** there is no `left` message (leaving a room still gets no reply) and no notice acknowledgement; the notice feed is read-only and keeps notices only for the notification retention period.
- **`pong`**: `{type}`.

## Close codes

| Code | Meaning | Client should |
| --- | --- | --- |
| 1000 | Normal close | Nothing |
| 1001 | Server shutting down or draining | Reconnect after 1–5 s with jitter, and resume |
| 1003 | Binary frame sent | Fix the client |
| 1008 | Protocol violation (unknown message type, invalid JSON, a message before `auth`) | Fix the client |
| 1009 | Frame too large | Fix the client |
| 4001 | Authentication failed or timed out | Sign in again, then reconnect |
| 4008 | **Slow consumer**: the client did not read fast enough, and its queue (256 messages) filled | Reconnect and resume; expect `resync_required` |
| 4009 | Session expired or revoked | Sign in again |

**Reconnecting.** Use exponential backoff from 1 s to 30 s, with full jitter. For every room, rejoin with `resume_after` set to the last `seq` seen.
