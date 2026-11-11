# Idle connections: does one node hold 10,000?

WP-PLT-01 acceptance says "10k idle connections per node". `harness/rust/spot-idle-connections`
(WP-PLT-01.T2) proves or disproves that with numbers. It starts the real `spot-http` server (request
stack, site resolution, health endpoints, accept loop, graceful shutdown) with the realtime
WebSocket route and a fake service route, in the same process on `127.0.0.1:0`, opens N idle
connections and measures what happens to the process and to the requests of the busy clients.

## How to run

```bash
# Every CI run: N = 200, proves the harness itself (counts, budget comparison, report).
cargo test -p spot-idle-connections

# The 10,000 run (about 90 seconds). Needs the descriptor limit below. The report goes to stdout.
ulimit -n 65536        # in the shell that runs cargo; see "Descriptor limits"
cargo test -p spot-idle-connections --test idle_10k -- --ignored --nocapture
```

The ignored test runs every night in the `10,000 idle connections` step of the `rust` job in
`.github/workflows/nightly.yml` (Linux runners), after `ulimit -n 65536`, with `SPOT_IDLE_REQUIRE=1`:
a skipped run (for example a descriptor limit that is too low) then fails the test and the nightly
is red. Without `SPOT_IDLE_REQUIRE=1` (the default) a skip prints `SKIPPED:` and passes, so a
developer without the limit still gets a message. The runner user's hard limit must be at least
65536 (`LimitNOFILE=65536` in the runner unit, or an `/etc/security/limits.d` entry); check it with
`su - gh-runner -c 'ulimit -Hn'`. Environment overrides, all optional: `SPOT_IDLE_CONNECTIONS` (default 10000), `SPOT_IDLE_HOLD_SECONDS` (30),
`SPOT_IDLE_BUSY_CLIENTS` (50), `SPOT_IDLE_BUSY_RPS` (100) and `SPOT_IDLE_MIX`
(`http1,http2,websocket` in percent, default `50,25,25`).

## What a run does

1. Starts the server and records memory, descriptors and the number of live tokio tasks.
2. **Empty-server baseline.** 50 busy clients, each on its own HTTP/1.1 connection, send 100
   requests per second in total for the hold time (30 s) to the fake service. Their latency is
   recorded.
3. **Opens the idle connections** (256 connects in flight at a time): plain HTTP/1.1 keep-alive
   connections (one request to `/api/health/live`, then idle), HTTP/2 connections (prior knowledge,
   one request, then idle) and WebSocket connections that authenticated with the in-process token
   verifier and joined a document room (1,000 rooms, so about ten connections share one at 10,000).
4. **Loaded phase.** The same busy load for the hold time while every idle connection is held. The
   harness answers the server's WebSocket pings from one task for all connections, so none is reaped
   for idleness.
5. **Shutdown**, the way a deployment does on `SIGTERM`: the WebSocket token is cancelled and the
   HTTP server's shutdown signal released. The harness answers the closing handshakes (HTTP/2
   `GOAWAY` ping, WebSocket close) as real clients would, and times how long until the server counts
   no open connection and no WebSocket session.
6. Judges the run (below) and returns the report.

The clients are written over a plain `TcpStream` (no HTTP, HTTP/2 or WebSocket client library): an
idle connection costs the client one socket and no task, so the process's resident size is dominated
by the server.

## Descriptor limits

Both ends of every connection live in the harness process, so a run needs
`2 x (connections + busy clients) + 512` descriptors: 20,612 for 10,000. The harness reads the soft
limit (`RLIMIT_NOFILE`) and **skips with a message, opening nothing, when it is below that**; it
never raises it.

| System | How the harness reads the limit | What to do |
| --- | --- | --- |
| Linux | `/proc/self/limits`, "Max open files" | `ulimit -n 65536` in the shell that runs cargo, or `LimitNOFILE=` in the unit that runs it. The runner user's limit is in the CI runner notes. |
| macOS | `sh -c 'ulimit -n'` | `ulimit -n 65536`; the system cap may need `launchctl limit maxfiles`. |
| Windows | none (no per-process descriptor limit applies) | nothing; the ephemeral port range (about 16,000 by default) bounds how many loopback connections one source address can hold, so for 10,000 widen it with `netsh int ipv4 set dynamicport tcp start=10000 num=55000`. |

## Measurements

| Report field | How it is measured |
| --- | --- |
| `memory.*` | Resident size of the process: `VmRSS` in `/proc/self/status` on Linux, `ps -o rss=` on macOS, `WorkingSet64` through PowerShell on Windows. Taken with the server up and no idle connection (baseline), right after the last connection is established, and at the end of the hold. `growth_bytes` is the larger growth over baseline. The process holds the server and the clients, so this is an upper bound on the server's cost. |
| `descriptors.*` | Entries in `/proc/self/fd` (Linux only; `null` elsewhere). |
| `accept.time_to_last_ms` | From the first connect until the last connection is established and has completed its first exchange. |
| `accept.accept_errors` | The server only logs accept failures (debug event `accept failed; backing off`); the harness installs one tracing layer that counts exactly those events. `accept_errors_counted` says the counter was live. |
| `accept.server_open_connections` | `ServerHandle::open_connections()`: every connection, upgraded WebSockets included (each session holds the connection's lease until it ends). |
| `accept.server_ws_sessions` | The realtime bus's subscription count. It equals the WebSocket part of `server_open_connections` (`tests/shutdown.rs` asserts it). |
| `latency.*` | Per-request time of the busy clients, whole microseconds, nearest-rank percentiles on the sorted samples (no floating point). |
| `shutdown.*` | Time until `open_connections()` is zero and the bus has no subscription; idle client sockets still open afterwards; live tokio tasks (`RuntimeMetrics::num_alive_tasks`) before the server started, with everything held, and after shutdown. |

The harness measures with monotonic clocks because latency is what it measures; it reads no wall
clock.

## Budgets

The ceilings are in `harness/rust/spot-idle-connections/budgets.toml`, which the tests read.

| Budget | Default | Why |
| --- | --- | --- |
| Idle HTTP/1.1 or HTTP/2 connection | 100 KiB above baseline | A parked connection should cost a connection state and a few buffers, not a request's worth of memory. 100 KiB is five times what was measured, enough to absorb a debug build and the allocator without letting a per-connection buffer leak in unnoticed. 10,000 of them stay near a gigabyte at the ceiling. |
| Idle WebSocket (authenticated, in a room) | 200 KiB above baseline | A WebSocket keeps a session task, a bounded event queue and room membership, so it costs more than an HTTP connection. Twice the HTTP ceiling. |
| Busy-client p99 with idle connections | at most 2 x the empty-server p99 + 20 ms | The claim is that idle connections do not degrade busy ones. A multiple absorbs noise on a loaded runner; the 20 ms floor stops a very fast empty server (a p99 of 2 ms) from making the ceiling unreachable. |
| Accept errors | 0 | An accept error means the kernel backlog or a limit was hit. |
| Client connect or handshake failures, failed busy requests | 0 | Every requested connection must be established and every busy request answered. |
| Tasks alive after shutdown beyond those before | 0 | Nothing may outlive the server. |
| Shutdown | every connection closed within the grace period (plus 500 ms of scheduling tolerance), every client socket closed | The accept loop aborts what is left at the grace; a close that needs the abort is a defect in the drain. |

Memory is judged per kind: the ceiling for a mix is `(HTTP connections x 100 KiB) + (WebSockets x
200 KiB)`.

## Reading the report

`passed` is the conjunction of `checks`; each check has a stable `name` (`connections`,
`accept_errors`, `memory`, `latency`, `busy_errors`, `shutdown`, `tasks`) and a `detail` sentence
with the numbers. `memory.per_connection_bytes` is the growth divided by all connections;
`latency.p99_ceiling_us` is the limit the loaded p99 was held to. A run that did not start prints
`{"skipped": "<reason>"}` and the test passes without having proved anything: read the output.

## Measured (2026-10-30, debug test profile, one 8-core workstation)

Defaults (10,000 connections: 5,000 HTTP/1.1, 2,500 HTTP/2, 2,500 WebSocket; 50 busy clients at 100
requests per second; 30 s hold): all budgets met.

| | Value |
| --- | --- |
| Memory growth | about 511 MiB, 51 KiB per connection |
| Descriptors held | 20,011 |
| Time to establish the last connection | 7 to 23 s (varies with load on the machine) |
| Accept errors | 0 |
| Busy p99, empty / loaded | 9.1 ms / 6.9 ms in the best run, 22.9 ms / 25.0 ms in the noisiest |
| Shutdown | everything closed in 1.9 s (the WebSocket close linger is 1 s) |
| Tasks after shutdown | 0 leaked |

Per kind (3,000 connections of one kind): HTTP/1.1 about 20 KiB, HTTP/2 about 24 KiB, WebSocket about
139 KiB per connection above baseline. The WebSocket figure is within its ceiling but is the
largest cost and the one to watch.

## Findings for the lead

- Fixed by WP-PLT-01.T3: `ServerHandle::open_connections()` counts WebSockets. `spot-http` gives each
  connection a `ConnectionLease` (a clone in every request's extensions); the realtime upgrade handler
  keeps a clone in the session task, so the count drops when the session ends. A node can export
  `open_connections()` as its connection gauge. The grace abort ends the connection tasks; a session
  ends on the realtime shutdown token.
- Fixed by WP-PLT-01.T3: HTTP/2 graceful shutdown no longer waits for the client to answer hyper's
  `GOAWAY` ping. hyper has no knob to bound that wait, so a connection with no request in flight is
  closed 100 ms after shutdown starts (time for the `GOAWAY` to be written), and one with a request in
  flight when that request, response body included, ends; the grace abort bounds the rest.
- Fixed by WP-PLT-01.T3: the WebSocket close linger (1 s, so a peer that stopped reading still finds
  the close code) ends as soon as the peer answers the close or hangs up, so shutting down many
  well-behaved sockets no longer takes a second each in sequence; a peer that never answers still gets
  the full linger.
