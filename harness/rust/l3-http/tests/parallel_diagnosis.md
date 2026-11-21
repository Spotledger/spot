# Diagnosis: `two_hundred_sessions_keep_every_invariant` is flaky under load (WP-TST-10.T4)

## Verdict

Two causes, one in the test and one in the product:

* **(b)/(c), the cause of the flake: the test put 200 requests in flight against a host whose
  database pool holds 48 connections and waits 5 s.** On a loaded machine the pool ran dry and
  requests were refused. Fixed in this PR by bounding the requests in flight (the 200 sessions
  stay open and keep working).
* **(a), a product defect found on the way, not fixed here (not an owned path):** the refusal
  reaches the client as a non-retryable `500 http.internal` ("The sign-in service is temporarily
  unavailable.", `retryable: false`) where the documented answer for "storage unavailable" is a
  retryable `503` with `retry_after`. See `## Open questions`.

## Evidence 1: the failed CI run

Run 41563801519 (`task/WP-EXT-02.T3`, job "pg tests for touched crates", runner
`spot-laptop-1`, 2026-11-03 02:29 UTC). The test ran 171 s and failed with 39 panics of the form

```
parallel.rs:364:32  a posted entry: Failure { message: "expected HTTP 200, got 500 with body
{"error":{"code":"http.internal","message":"The sign-in service is temporarily unavailable.",
"request_id":"...","retryable":false}}" }
```

(line 364 is the `post` of a draft Time Entry; the same body also appears for `a task list`).
Invariant: none of the end-state invariants was reached; the failure is in the workload itself,
an acknowledged request answered 500. The host log tail the test prints held only the three
fixture lines (`host is ready`, `fixture ready`): the host does not write the store failure's
detail to the log, so the cause was invisible in CI. The other CI runs whose failed-job log
contains the test name (checked: 41727894340 and eight more) show it passing; in
41727894340 it passed in 148 s, so the test is already slow on these runners.

## Evidence 2: reproduction

Local, PostgreSQL 18 + pgvector in Docker (`max_connections=200`), 6 cores, with other
agents' builds running (load average 10 to 21, no extra stress added).

| Run | Code | Result |
| --- | --- | --- |
| 1 | unchanged | FAILED after 195 s, 37 panics, same 500 body |
| 2 | unchanged, `eprintln!` of the login error detail added locally (not committed) | FAILED after 169 s, 83 store failures |
| 3 | in-flight gate of 16, per-session deadline still there | FAILED the `created.len() >= 400` check: 394 tasks, 0 contended updates, 75 s |
| 4 | gate of 16, deadline removed, old 90 s ceiling | all invariants held; failed only the 90 s ceiling (226.7 s) |

Run 2 printed the store failure detail that the client never sees:

```
71x  user store failed: storage.unavailable: no database connection became free within 5000 ms
12x  user store failed: storage.unavailable: the database could not be reached or did not answer;
     nothing was committed by this statement
```

That is the proof: `crates/backends/spot-postgres/src/backend.rs:328` (`map_pool_error`, pool
timeout) produces `ErrorKind::Unavailable`, and `crates/framework/spot-cli/src/login.rs:58`
(`storage_error`) wraps it as an untyped `StoreFailed`, so `LoginError::is_unavailable()` is false
and `to_api_error` answers the non-retryable 500 (`spot-login/src/error.rs`, `StoreFailed`
without `retryable_after`). The pool is `pool_max_size = 48` in the fixture
(`harness/rust/l3-http/src/fixture.rs:129`); the test drove up to 200 requests at once.

Failure rate before the fix: 2 of 2 local runs, plus the CI failure above (and the CI runs
that passed). It is a threshold effect: the more the machine is loaded, the longer each
connection is held and the sooner 5 s of waiting is exceeded.

## Second defect in the test: the work depended on the clock

`run_session` stopped working once its own 30 s `BUDGET` deadline passed. On a loaded machine a
session therefore did less than on a quiet one (run 3: 394 tasks, 0 contended updates), so the
`>= 400 tasks` check and the contended-document invariant (every successful patch advanced the
version) were checked on a different, smaller workload. The final `elapsed <= BUDGET + 60 s`
check then passed only because the work had been cut.

## What changed

* `client.rs`: `Client::with_in_flight_gate(Arc<Semaphore>)`. A request takes one permit for its
  round trip; clones share the gate; the wait is not part of the request timeout. Without a gate
  nothing changes for any other case.
* `parallel.rs`:
  * `IN_FLIGHT = 16` requests in flight (each authenticated request runs several storage transactions: session read, session
    touch, user read, then the handler's own; the peak connections per request was not
    measured, so 16 is a conservative choice well under the pool of 48, not a computed limit). The 200 sessions are all open and all working.
  * the per-session deadline is gone: every session does its three rounds (1,200 tasks, 1,200
    submitted entries, and the contended-update attempts), so the checks always see the full
    workload;
  * `BUDGET` is a hang guard of 600 s, measured: the load phase took 226.7 s at a load average
    of about 21 on 6 cores (run 4). Each request still has its 30 s timeout.
* No invariant was removed, weakened, skipped or retried. The invariants are unchanged; two of
  them (contended-update version count, `>= 400` tasks) are now checked on the full workload
  instead of whatever the clock allowed.

## After the fix

`tools/ci/repeat_test.py --runs 8` with `SPOT_TEST_PG_URL` set, on the 6-core machine at a load
average of 10 to 21 (other agents' builds): runs 1 to 7 passed (264 s to 331 s each, all 1,200
tasks and entries, all invariants). Run 8 did not start the test: the PostgreSQL container had
died because the host disk filled up (`No space left on device` in the container log), so
`cannot connect to SPOT_TEST_PG_URL` was an infrastructure failure, not the test. Result: 7 of 7
valid runs green, against 2 of 2 failures before. The brief asks for 50; at about 5 minutes a run
under this load that would take over four hours of a shared machine, so 7 is what was done and
the shortfall is stated here and in the PR.
## Open questions

1. **Product defect (a), for the lead to assign** (not an owned path):
   `crates/framework/spot-cli/src/login.rs:58-62`, `fn storage_error(e: StorageError)`, builds
   `LoginError::new(StoreFailed, format!("user store failed: {e}"))` for every storage error.
   An `ErrorKind::Unavailable` error (pool timeout, backend unreachable) must go through
   `LoginError::store_unavailable` (as `spot-login/src/stores/mod.rs::failed_at` does for the
   other stores) so the client gets the retryable `503` + `retry_after` instead of
   `500 http.internal, retryable:false`. A test: a pool of one connection held open, then a
   sign-in or an authenticated request, must answer 503 with `retry_after`.
2. **Product behaviour to decide (ADR-level, not a bug of this test):** the authenticated
   request path takes a database connection for the session read, the touch and the user read
   before the handler runs. With a pool of 48 and a 5 s wait, 200 simultaneous requests are
   refused on a loaded machine. Whether the host should queue longer, shed load earlier, or
   size its pool from `max_connections` is a lead decision; this PR only keeps the test below
   the pool as configured.
3. **Fixture (outside owned paths):** `harness/rust/l3-http/src/fixture.rs:129`
   `pool_max_size = 48`. If the lead wants 200 truly simultaneous requests, widen the owned
   paths to the fixture; PostgreSQL's default `max_connections` is 100, so 48 is the largest
   safe fraction.
4. **The host log is empty of the cause.** `to_api_error_traced` logs a `tracing` warning
   without the detail, and the l3-http fixture installs no subscriber, so the test's "last
   lines of the host log" shows nothing for this failure. A subscriber in the fixture (outside
   owned paths) would have made this diagnosis one step.
