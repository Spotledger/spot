# spot-sdk-conformance

The contract kit of the guest SDK conformance harness (WP-STR-07.T1). A bin-only crate that
loads a guest component the way `spot-wasm` loads an app, checks the world contract, and runs the
sample-app cases from `../cases.json`, emitting one JSON report.

## Build

```sh
cargo build --release
# target/release/spot-sdk-conformance
```

It is a **detached** crate (its own `[workspace]` table), like
`crates/apps/spot-sample-app/tests/app-validate`: the runner is a tool, not a workspace member,
so it stays out of the root workspace's dependency and lint graph. It depends on `spot-wasm` by
path and therefore links `wasmtime`.

## Use

```sh
target/release/spot-sdk-conformance \
  --component sdk/templates/rust/out/component.wasm \
  --cases tools/sdk-conformance/cases.json \
  --language rust \
  --out /tmp/report.json
```

The report is also printed to stdout. The process exits `0` for every component outcome (a
blocked component is a result, not a failure); `2` is a usage error.

## What it does

1. **Parse and validate** the component (`WasmHost::compile`).
2. **Check the export** `on-event` is present.
3. **Link with the granted capabilities** (`WasmHost::instantiate`): it grants read access to
   `time_entry`, so the world's `get-doc` links; `log` and `now` carry no authority. A component
   that imports anything else — any WASI interface, an unknown function — is refused with
   `wasm.capability_denied`, naming the import, and the report carries
   `blocked: {step: "instantiate", reason: ...}`.
4. **Run each case** as one `on-event` call under `Limits::default()`, comparing the world's
   `result<string, error>`: `ok` is the returned string, `err` is the app's error record.

Time is never read from the wall clock: `Imports::now` returns a fixed invocation time
(`1_700_000_000_000_000` µs), exactly as the host passes one in.
