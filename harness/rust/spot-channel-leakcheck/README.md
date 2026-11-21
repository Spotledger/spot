# spot-channel-leakcheck

End-to-end proof (WP-SEC-04.T3, ADR-070 section 12 rule 6) that each **real producer** of data
calls the field guard. `spot-field-guard` proves its own component with a matrix; this crate
proves the producers *use* it, so a new producer or a refactor that skips the guard turns the
build red.

Each test builds a tenant whose DocType has protected fields in groups the acting principal
lacks (`read_masked`, no grant, and an explicit `deny`, plus a derived field that inherits two
groups), plants the leak `Corpus` (whole, embedded in a longer text, split across child rows,
unicode lookalikes), drives the producer through its **public entry point**, serialises
everything that leaves and calls `assert_no_leak`. A positive control in every test checks that
a principal who *may* see the value does, so a clean result cannot be an empty one.

## Channels

| Channel | Test | Producer | State |
| --- | --- | --- | --- |
| `Export` | `export_never_leaks` (`tests/export.rs`) | `spot-bulk-data` (`ExportPlan`, `ExportStream`, CSV and NDJSON, child parts) | covered |
| `Report` | `report_never_leaks` (`tests/report.rs`) | `spot-reports` (`Compiler`, `ReportRunner::run`) | covered; one finding, see below |
| `Version` | `version_history_never_leaks` (`tests/version.rs`) | `spot-doc-audit` (`build_event`, `LaneAppender`, `DocAudit::history`, `GuardMasker::redact_diff`) | covered |
| `ListColumn` | `list_column_never_leaks` (`tests/list_column.rs`) | `spot-api-services` (`EngineResources::list` over the real `Engine`, `PermissionAuthorizer` and `with_field_guard`) | covered |
| `LinkTitle` | `link_title_never_leaks` (`tests/link_title.rs`) | `spot-api-services` (`EngineLookup::search`, the platform link seam, over the real `Engine`) | covered |
| `Search` | `search_never_leaks` (`tests/search.rs`) | `spot-retrieval` (`Indexer::pump_once` with `with_protection`, `Retriever::search` with `with_search_guard`) | covered |
| `Notification` | `notification_never_leaks` (`tests/notification.rs`) | `spot-realtime-bridge` (`FeedPump::pump_once` with `with_notification_guard`, `recipient_names`) | covered |
| `Journal` | `journal_never_leaks` (`tests/journal.rs`) | `spot-journal-relay` (`Relay::append_batch`, `seal_now`, `open_views`, `JournalViewer::view_line`: names only, never a value) | covered |
| `ApiScim` | `api_scim_never_leaks` (`tests/api_scim.rs`) | `spot-iam-scim` (`handle_guarded`: single resource, list, `PATCH` and `POST` answers, filter and `sortBy` refusal) | covered |
| `Print` | `print_never_leaks` (`tests/print.rs`) | `spot-http` (`GuardedPrintPort::render`: loads as the caller, `guard_document` with `Channel::Print`, then renders; PDF bytes and HTML) | covered |

Two more producers of `Export` are covered by extra files of the same channel (found by the coverage test's source scan): `tests/export_incremental.rs` (`spot-bulk-data` incremental extraction: a protected-only change, a field that became protected, a right revoked before the extract, tombstones) and `tests/event_stream.rs` (`spot-event-stream` data subscription: JSON frames, SSE frames, resuming after a revocation, the audit records).

Other files: `tests/inference.rs` (a filter, sort, group or aggregate on a protected field is
refused, with a 100-sample timing check), `tests/system.rs` (jobs, sync and migrations: manifest,
trace and audit records carry no value, a job exporting as the user is masked),
`tests/negative.rs` (a producer that skips the guard fails the harness, naming the label and
encoding and never the value), `tests/coverage.rs` (the guard below).

## The coverage guard

`CHANNELS` in `src/lib.rs` maps every `Channel` to `covered(test, producer)` or
`pending(reason, task)`. `tests/coverage.rs` matches `Channel` exhaustively (a new variant does
not compile until it is listed), checks each registered test exists by name in `tests/`, checks
a covered producer depends on `spot-field-guard`, and prints the pending list.

## Adding a channel

1. Add the variant to `spot_field_guard::Channel` (its `need` and `ALL`); the coverage test stops
   compiling until you list it in `listed` and in `CHANNELS`.
2. Write `tests/<channel>.rs` on the shared world in `tests/common/mod.rs`: build the producer
   over `core()` (guard, engine, metadata), plant `corpus()`, run its public entry point as each of
   `PRINCIPALS`, serialise every output, `assert_no_leak(&corpus.without(&visible_labels(..)), ..)`
   and a positive control with `find_leaks`.
3. Register it with `covered("test_name", "producer-crate")` (or `pending(..)` while the producer
   does not exist).
4. If the producer leaks, commit the test `#[ignore = "leaks: <task>"]` and report it first.

## Why a separate crate

It depends on many producers (storage fake, bulk data, reports, audit). **No production crate
may depend on it.** It is dev and test code only and contains no wall-clock read; the one
`Instant` use in `tests/inference.rs` measures elapsed time between two monotonic readings.
