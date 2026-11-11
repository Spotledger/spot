# Spotledger.Client.Realtime

The client half of [`contracts/native-api/realtime.md`](../../../../contracts/native-api/realtime.md)
v0.1: one WebSocket per signed-in session, shared by the shell, forms, lists and the
notification centre. UI-free — no Avalonia reference, no package reference, hand-written
serialisation (trim and AOT safe for the browser head).

```csharp
await using var realtime = new RealtimeClient(site);
realtime.SessionEnded += (_, ended) => shell.RunReSignIn(ended.CloseCode);

// Asked once per connection attempt: the realtime layer holds no credential of its own.
await realtime.ConnectAsync(
    _ => ValueTask.FromResult(tokens.GetTokenForSecretStore()),
    cancellationToken);

using var subscription = realtime.Join(
    $"doc:sales_invoice:{id}",
    message =>
    {
        switch (message)
        {
            case RealtimeEvent e:          View.NoteHint(e); break;
            case RealtimeResyncRequired r: View.ReloadAsync(); break;
            case RealtimeJoinError err:    View.ShowCannotOpen(err.Code); break;
        }
    });
```

## The wire, in one paragraph

`wss://<site>/api/v1/realtime` with subprotocol `spot.v1`. Every frame is one UTF-8 JSON
text object: the client sends `auth`, `join`, `leave`, `ping`; the server sends `ready`,
`joined`, `join_error`, `event`, `pong`. `auth` must be the **first** message of every
connection and is the only authentication (there is no header, because browsers cannot set
headers on a WebSocket). Frames over 64 KiB are refused. Types this version does not know
are counted and ignored, because the server may add message types.

## State machine

| State | Entered when | What the client does | Leaves to |
| --- | --- | --- | --- |
| `Closed` | before the first connect; after a normal close (1000); after a fatal protocol fault (1003/1008/1009); after `DisposeAsync` | nothing runs | `Connecting` on `ConnectAsync` |
| `Connecting` | `ConnectAsync` starts the first attempt | connects, sends `auth`, waits for `ready` (the contract's 10 s) | `Ready` on `ready`; `Reconnecting` if the attempt fails; `SignedOut` on 4001/4009 |
| `Ready` | `ready` arrived | sends the joins for every room with subscribers, then keeps sending them | `Reconnecting` on 1001/4008/a transport fault/a missed heartbeat/a slow handler; `SignedOut` on 4001/4009; `Closed` on 1000 or a fatal code |
| `Reconnecting` | a connection ended and the client will retry | waits the full-jitter backoff (1 s → 30 s), then opens a new connection, sends `auth`, and on `ready` rejoins every room with `resume_after = last seq the subscriber saw` | `Ready`; `SignedOut` if the retry answers 4001/4009; `Closed` after a fatal code |
| `SignedOut` | the server closed with 4001 (auth failed or timed out) or 4009 (session expired or revoked) | stops reconnecting and raises `SessionEnded`; holds no credential | `Connecting` after the shell signed in again and calls `ConnectAsync` |

`RealtimeConnectionState` therefore has one more member than the task brief lists
(`Closed`): a normal close and a client-bug fault are neither "signed out" nor
"reconnecting", and folding them into `SignedOut` would make the shell run a re-sign-in
prompt for a server restart. See the PR's open questions.

Close codes and what this client does with them:

| Code | Client action |
| --- | --- |
| 1000 | stop, `Closed` |
| 1001 | reconnect after the backoff, resume every room |
| 1003 / 1008 / 1009 | one redacted diagnostic, stop, `Closed` — retrying a client bug only loops |
| 4001 | `SignedOut`, `SessionEnded` |
| 4008 | reconnect and resume; the server answers the joins with `resync_required` |
| 4009 | `SignedOut`, `SessionEnded` |
| other / 1006 / no close frame | treat as a transport fault: back off and reconnect |

The client also drops the connection **itself** when it is the slow consumer (see
backpressure below) and in that case reconnects exactly as it does for 4008.

## Rooms, seq and resync

* `Join(room, handler)` subscribes; it returns a `RealtimeSubscription` whose `Dispose` is
  `Leave`. **One wire `join` per room** however many subscribers, reference counted; the
  wire `leave` follows the last subscriber.
* The contract's 200-room limit is enforced locally: the 201st distinct room throws
  `RealtimeRoomLimitException` with the room and the limit in the message, before a round
  trip could fail with `realtime.too_many_rooms`.
* `join_error` (`permission.denied`, `realtime.unknown_room`, `realtime.too_many_rooms`) is
  delivered to the room's subscribers as `RealtimeJoinError`; it is never thrown on the
  receive loop. A refused room is not retried on reconnects until a new subscriber joins.
* Per room the client tracks the last `seq` **received** and the last `seq` **delivered**.
  A gap (`seq` > last + 1) raises `RealtimeResyncRequired(room, Gap)` and the event that
  revealed the gap is **not** delivered as if nothing happened. `joined` with
  `outcome = resync_required` raises the same signal with `ResumeExpired`.
* While a room awaits its resync, later events for that room are **withheld** (and counted
  in `DroppedMessageCount`); call `RealtimeSubscription.AcknowledgeResync()` once the
  refetch finished and delivery resumes from the server's current `seq`.
* Replays after a resume are delivered in order, once: the resume point is the last event
  the *handler* saw, so an event still queued when the connection dropped is replayed by
  the server rather than lost.

## No correctness may depend on a message arriving

The contract's design rule (ADR-25) is that realtime is notification, not data sync. For a
subscriber that means:

1. **Events are hints.** `RealtimeEvent.Payload` is `{doctype, id, version}` at most, plus
   opaque JSON for `notification`. Never render data from an event; refetch the document.
2. **Always refetch on `RealtimeResyncRequired`.** It means the client knows it missed
   something (a `seq` gap) or the server no longer holds the replay (`resync_required`).
   The view is stale until it reloaded. Acknowledge after the refetch.
3. **Be correct if nothing ever arrives.** A form that only updates on events is wrong: it
   must also reload after a reconnect, on focus, and on the user's own edits.
4. **Never block the handler.** Handlers run on the captured `SynchronizationContext` (the
   UI thread when there is one), in order, per room, from a bounded queue (256 by default).
   A handler that falls further behind than that makes the client drop the connection and
   resync — the server's own slow-consumer rule, applied from this side — so post the work
   and return instead of awaiting it.
5. **A handler exception costs nothing but a counter.** It is caught, counted
   (`HandlerFaultCount`) and raised as `HandlerFaulted`; the receive loop continues.

## Backoff, heartbeat, redaction

* Reconnect delay: cap 1 s, doubling per attempt, capped at 30 s, with **full jitter** —
  uniform in `[0, cap]` — from an injected `Random`, so a crowd of clients does not
  reconnect together. Time comes from the injected `TimeProvider`; the library never reads
  the wall clock.
* Heartbeat: `ready` carries `heartbeat_seconds`. The server pings; the transport answers
  the control frame (the WebSocket stack does that), and any frame counts as activity. If
  nothing at all arrives for **2 × heartbeat_seconds**, the client treats the socket as dead
  and reconnects, resuming every room.
* Redaction: `RealtimeAuthMessage.ToString()` is `auth(token=***)`. Diagnostics carry no
  token, no URL and no payload; the token exists only in memory (dropped when the session
  ends or the client is disposed) and in the one `auth` frame. `RealtimeTokenSource` is
  asked once per connection attempt, so the shell can keep the credential in
  `SessionTokenStore` and hand out nothing more.

## The browser target

What this library does is constrained by what the browser runtime allows, and the two were
checked against the .NET 10 browser-wasm runtime pack
(`Microsoft.NETCore.App.Runtime.Mono.browser-wasm`), not assumed:

* The runtime ships a real `BrowserWebSocket` behind `System.Net.WebSockets.Client`, so
  `ClientWebSocket` works in the browser and `AddSubProtocol` is honoured — the subprotocol
  list is passed to the browser's `WebSocket` constructor and .NET validates the answer.
  That is why the contract can require `spot.v1` and still work in a page.
* The browser `WebSocket` API has **no** way to set request headers. Server-side
  authentication therefore cannot ride on a header, which is exactly why `auth` is the
  first message. This transport never sets a header and never will: doing so would make the
  desktop work and the browser silently fail.
* The subprotocol must be chosen **before** the handshake (constructor argument) and cannot
  be changed afterwards, so `ClientWebSocketRealtimeTransport` adds it before
  `ConnectAsync` and then verifies `SubProtocol` — a server that ignores it fails the
  connect instead of half-working.
* Cancellation in the browser does not always tear the page's socket down immediately, so
  the transport never relies on cancellation alone: the client also calls `CloseAsync` and
  disposes the socket on every path.
* No other difference matters to this library: framing, ping/pong and close codes behave
  the same, and nothing here uses threads, `SynchronizationContext` internals or reflection.

## Testing seams

* `IRealtimeTransport` — swap in `InMemoryRealtimeTransport.CreatePair()` (shipped here)
  to drive the client from a scripted server with no socket.
* `RealtimeClientOptions.TimeProvider` and `.Jitter` — a manual clock and a seeded
  `Random` make a 30 s backoff a function call.
* `RealtimeClientOptions.DiagnosticLog` — collects the redacted one-liners a test asserts on.
