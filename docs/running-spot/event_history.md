# Event history from the journal (`[services.event_history]`)

A consumer of the event stream whose checkpoint is older than the outbox retention would get `410 eventstream.resync_required`. When the tenant has a journal the history is still there, and the stream can catch the consumer up from it and then continue on the live outbox (ADR-061 sections 5 to 7, contract 0.16.0).

It is on whenever `[services.journal]` has a target. Without a journal nothing is provided and older checkpoints answer `410` exactly as before.

```toml
[services.event_history]
enabled = true   # false: history off while the journal stays on
```

## What it provides

The site's `JournalReader` over the same folder, layout and key provider the journal service ships to:

- sealed segments are served only after their bytes hash to the signed manifest's summary and every block verifies; otherwise `history_unavailable` and nothing of that range;
- positions beyond the manifest come from the unsealed tail chunks (versions 1 and 2, decrypted with the tenant journal key);
- a position in a generation a fork abandoned serves its valid part and then continues in the next generation; a position after the fork point is `resync_required`;
- the seam to the live outbox fails closed (`check_seam`): if the outbox was trimmed during the call, the answer is `resync_required`, never a silent jump;
- personal fields come back as stored (sealed, or the erased placeholder): the stream never reveals them, and the read filter and the field guard run on journal-served events exactly as on live ones.

The reader is per site and its store refuses any other tenant's prefix: a consumer of one site cannot read another site's journal.

## Selectors and `served_from`

`spot-http`'s `event_history` port file holds the rules the routes apply (`classes` default `domain` and `audit`, telemetry never a class, `entities` only with `state`, otherwise `422 eventstream.spec_invalid`) and the `served_from` forms (`served_from` in a pull, the `X-Served-From` header and the `served_from` SSE frame of a stream).

**Wiring state.** The consumer routes and the stream engine's host service (WP-INT-02.T4) have to call these helpers and read the `dyn JournalHistory` port; they are not part of this task. Until they do, `classes`, `entities` and `served_from` are not on the wire and older checkpoints stay `410`.
