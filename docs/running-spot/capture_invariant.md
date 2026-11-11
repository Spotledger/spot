# The capture invariant (`[services.capture_invariant]`)

The journal is only recoverable if every committed write left a `doc_state` event. The document engine writes that event, and storage's own **change feed** is an independent witness that it did. The capture invariant compares the two continuously. A write that bypassed the engine (a direct SQL update, a loader that skipped it) leaves a feed entry with no event, and the service raises the red condition `CAPTURE_MISMATCH` within a few minutes, not months later when a rebuild comes up short (ADR-061, section 8).

The service is **optional**: it never stops the site, and it never writes to the change feed or the outbox (it reads inside a read-only snapshot, which the database enforces).

## Settings

```toml
[services.capture_invariant]
enabled = true           # off by default: run `spot migrate` first (it creates the watermark table)
interval_seconds = 60     # seconds between passes; never below 10
lag_seconds = 30          # seconds a position must have been published before it is judged
window = 50000            # document versions the checker tracks at once; at least 1
page_size = 2000          # entries read per statement; at least 1
```

An unknown key, a window or page size of 0, is refused when the site starts.

## The table

`spot_capture_watermark` holds one row per stream (`change_feed`, `doc_state`): the generation and the last position settled. `spot migrate` creates it. A site that has not run `spot migrate` starts with this service degraded and says so; the service never creates the table.

## What a pass does

1. Asks the backend to publish both logs. The storage feed gives a committed entry its position when a reader asks, and nothing else in a running host asks for the change feed, so each pass does. This is the storage's own publisher acting on its own tables; the service writes nothing.
2. Reads the DocTypes the engine excludes from capture (derived or local-only data, each with an owner and a reason). The list is read on **every** pass, so a newly excluded DocType is not reported. If the engine cannot be reached the pass fails; it never judges with an empty list.
3. Reads the change feed and the `doc_state` events in one read-only `REPEATABLE READ` snapshot, from the watermark, a page at a time, and compares them. Memory is bounded by `window` and `page_size`.
4. Raises `CAPTURE_MISMATCH` once per record for each **settled** finding of these kinds:
   - a feed entry with no event of the same document, version and operation (a missed capture);
   - an event with no feed entry (an orphan);
   - a feed entry and an event of the same document at different versions.

   The condition's subject is `<source>:<entity>/<id>` and its parameters are `source` (`change_feed` or `doc_state`) and `record` (`entity/id`). It carries identity only, never a value.
5. Clears the condition of a record a later pass finds matched.
6. Saves the watermark.

### Settled, deferred and the lag

The two sides of one write are published by two statements, so they reach their positions at slightly different instants. A finding is **settled** only when every position it names was published at least `lag_seconds` before the snapshot. A younger finding is *deferred* and judged by a later pass. Set `lag_seconds` above the longest gap between the two publications; 30 seconds is generous for the default publisher.

### The watermark is held below an open finding

The watermark advances to the newest settled position of each stream, **held back below the earliest open finding**. A missed write therefore keeps the watermark behind it: every pass (and every pass after a restart) finds it again, and it clears only when a later pass finds the entry matched. Nothing is lost by restarting the host. The cost is that, while a finding stays open, each pass re-reads from the finding forward; fix or recapture the record.

### The window

An entry that leaves the checker's window unmatched is an `OutOfWindow` finding. It raises `CAPTURE_MISMATCH` **only when `window` is larger than the measured lag**, the number of entries published in the last `lag_seconds`: then the window certainly held the counterpart, so the entry is a real miss. When the window is not larger than the measured lag, the entry may simply be too early, so it is counted and logged at warn level with the numbers and raises nothing. Size `window` above the busiest `lag_seconds` of the site.

Duplicate events (two events for one document version) and the deferred and out-of-window counts appear in the warn log line as counts. They raise nothing.

## Readiness

`CaptureInvariantService::readiness_text()` answers

```
capture_invariant: ok (watermark g1:s48210, 0 findings open)
```

After **three passes in a row** that could not read the streams it says `capture_invariant: degraded (...)` with the last error code, not `failed`: an unreadable check never marks the site not ready. The first good pass resets it.

## CI and the harness

`CaptureGate::run_once(&pool)` (re-exported from `spot_cli::services::capture_invariant`) compares the whole feed with the whole journal with no lag, for a test that has stopped writing. A non-empty report is a failing assertion.

## Errors

Stable codes are in `crates/framework/spot-errors/catalogue/capture_invariant_pg.toml` (`capture_invariant_pg.read_failed`, `.not_read_only`, `.corrupt`, `.watermark_failed`, `.checker_refused`, `.options_invalid`). Messages never carry SQL, an id or a value.

## Known limit

A derived exclusion declared by the engine carries no projector name, so the service cannot tell the checker which projector rebuilds it. The checker's "derived without projector" finding is therefore counted by the service and raises nothing.
