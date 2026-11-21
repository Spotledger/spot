# spot-bulk-actions

One lifecycle action applied to many **existing** documents: post twenty drafts, cancel a
batch of wrong entries, set one field on a hundred documents, delete a pile of drafts, call
one whitelisted method on each. New rows are the import engine's job (`spot-import`).

```rust,ignore
let outcome = apply_bulk(&service, &inv, &BulkAction::Post, targets, &options, &NoProgress, &cancel).await?;
```

## The contract

- **No batch transaction.** The engine runs one transaction per call and has no
  multi-document transaction. `apply_bulk` makes one call per document. A failure half-way
  leaves the earlier documents changed. **Partial success is the normal result**, not an
  error: look at `BulkOutcome` counts and at each `BulkResult`.
- **Results are in the order of the request**, whatever order the calls finished in.
- **A failure of one document never aborts the batch.** Not found, not readable, stale
  version, wrong state, permission refusal and validation errors are each a per-document
  `Failed { code, class, message }` carrying the engine's own code, class and message. The
  crate adds no text that could hold a document value. A document the caller cannot read and
  a document that does not exist produce the engine's same answer, so a batch never reveals
  that an unreadable document exists.
- **Whole-request refusals** are a `BulkError` before any call: `bulk_actions.empty_batch`,
  `too_many_targets`, `bad_label`, `bad_concurrency`, `action_not_allowed`.
- **Bounds.** At most `concurrency` (1 to 16, default 4) calls in flight; at most
  `max_targets` (default 10,000) documents; after `max_failures` failed documents (default 100,
  0 means unlimited) no new document is started. Documents already in flight when the limit is
  hit finish, so with concurrency `c` at most `c - 1` calls start after the failure that
  reached the limit. The rest are `NotRun` and `stopped = MaxFailures`.
- **Cancellation** (`CancellationToken`) stops starting documents, lets the ones in flight
  finish and marks the rest `NotRun`, `stopped = Cancelled`.
- **Duplicates** in one request collapse: the first runs, the rest are `Skipped(Duplicate)`.
- **Already done** (`skip_already_done`, default true): posting a posted document or cancelling
  a cancelled one is `Skipped`, not `Failed`.

## Target versions

`BulkTarget.expected = None` means "act on whatever is there": the document is loaded first and
its current version is used (a user's selection, not an edit session). `Some(v)` sends `v`; a
stale version is a per-document `document.stale_version` failure.

## Re-running a batch

Each call uses a copy of the invocation whose command id derives from `batch_label`, the action
and the document id (`bulk:<label>:<action>:<id>`, or `bulkh:` plus a SHA-256 when that would
not be a valid command id). The engine's command ledger returns the first result for a command
id it has already committed, so re-running the same request after a crash or a retry changes
nothing twice. Reuse the label to re-run, use a new label for a new batch. The limit: this holds
only while the ledger keeps the id. After the ledger forgets it, the call runs again. An
update with a different patch under the same label returns the first patch's result: the label
is the batch's identity, so change it with the patch.

## Dry run

`dry_run = true` loads each target (read only), never calls a write method, and reports
`WouldSucceed`, `Skipped` or `Failed` (not found, wrong state, stale `expected`). It cannot
know what validation or a hook would say on the real call, nor whether the caller may write
(the service has no probe): do not promise a user that the real run will pass.

## What a UI should show

Counts first ("48 posted, 2 skipped, 3 failed, 0 not run"), then the failures grouped by code
with the document links, and, when `stopped` is set, which rule stopped it and how many
documents remain. Offer "retry the failed ones" as a new batch built from the failed ids (with
a new label), never a silent retry.

## Not here

The HTTP route `POST /api/v1/resources/{doctype}/bulk` is a native API contract change
(a lead task), then a hosting task on the route registry. Selecting targets by filter: the
caller resolves a filter to ids through the permission-filtered list. Batches too large for one
request: a host can run `apply_bulk` inside a job once the route exists.
