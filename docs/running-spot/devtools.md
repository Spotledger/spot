Part of [Running Spot](../running-spot.md).

# Debugging and tracing in development: `[services.devtools]`

Two tools for a developer running `spot serve --dev`: a **debug adapter** to set a breakpoint in a
TypeScript script from an editor, and a **trace viewer** that answers "what did this request do?"
with the tree of spans across HTTP, lifecycle, pipeline steps, storage and jobs. Both are
libraries (`spot-devtools`); this module hosts them under the same safety rules. They are off by
default and compiled for development only.

## Starting

```toml
[services.devtools]
enabled = true
# Optional. The values below are the defaults.
trace_capacity = 500          # traces kept per site, 10 to 10000
trace_address = "127.0.0.1:7312"
debug_address = "127.0.0.1:0"  # port 0: the system chooses; the status file names it
```

Set the **development token** in the environment, at least 16 characters, then start the host:

```sh
export SPOT_DEV_TOKEN='a-long-random-development-token'
spot serve --dev
```

The token is read from `SPOT_DEV_TOKEN` only. It is never an argument, never written to disk, and
never logged. The module **refuses to start** when:

- the site is marked `production = true` (`DebugPolicy::check_site`);
- the build is a release build (`DebugPolicy::check_build`): devtools are compiled out of production;
- a `server.bind` address or `trace_address` is not loopback;
- `debug_address` is not loopback and `allow_remote` is not `true` (with `allow_remote = true` the
  host warns on every start, and the token is still required);
- the token is missing or shorter than 16 characters.

## Attaching an editor

The debug adapter speaks the Debug Adapter Protocol over TCP on `debug_address`. It starts only when
the site's TypeScript engine provides a `DebugHost` port; without one the viewer still works and the
adapter is simply absent. The running host writes `sites/<site>/devtools.json` (mode `0600`, removed
at stop) naming the addresses. A `launch.json` for VS Code:

```json
{
  "type": "spot-ts",
  "request": "attach",
  "name": "Spot script",
  "port": 7313
}
```

Replace the port with `debug_address` from the status file. The first request of a launch must carry
the development token; a pause nobody continues is resumed after the policy's pause timeout, with a
warning. A paused script holds only its own worker. The session runs as the signed-in developer, so
a breakpoint in a script run as a user shows that user's data and no more.

## Reading a trace

```sh
spot trace <request-id> --site acme            # the tree and the layer summary
spot trace <request-id> --site acme --format json
spot trace --site acme                          # the list of traces
```

`spot trace` reads the token from `SPOT_DEV_TOKEN` (or `--token-file`) and the address from the
status file. Exit codes: 0 found, 1 usage, 3 not found or the host is not reachable. The same data is
at `GET /dev/traces` and `GET /dev/traces/{id}` (JSON, or `?format=text`) on `trace_address`, with the
token in the `X-Dev-Token` header: a production site answers `404`, a missing or wrong token `401`.
These routes are **not** part of the native API.

## What is and is not traced

Traced: the HTTP request (`http_request`), the document operation and its phases (`doc.op`,
`doc.phase`), each lifecycle step (`doc.step`), storage transactions and statements (`storage.tx`,
`storage.query`) and job attempts (`job.run`), joined into one trace per request by the request id.
Not traced: script and WASM spans until those engines emit spans under a name this module maps (see
the layer table in `services/devtools/layer.rs`); a WASM boundary report is recorded as span
attributes when the app runtime provides one.

Every attribute passes the redactor before it is stored. A password, an API key, a session token and
a field of a protected group never reach the ring, the text render or the JSON.

## Known gap

`spot-telemetry` installs one process-wide subscriber and has no way to attach a second layer after
start-up. The `CollectorLayer` of this module must therefore be installed by the host when it
initialises telemetry; until that one hook exists the collector stays empty and the viewer answers
with an empty list.
