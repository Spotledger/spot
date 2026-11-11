# Rust guest template (the reference)

The reference implementation of the SDK conformance harness's sample app. Every other template
is compared against it. It is a Rust `cdylib` built for **`wasm32-unknown-unknown`** and
componentized with `wasm-tools component new`, so the component imports **nothing** — no WASI,
no ambient authority — and links on `crates/framework/spot-wasm`, the product host.

## Why not `wasm32-wasip2`?

`wasm32-wasip2` produces a component directly, but a guest that uses `std` (and therefore
allocates) imports the WASI 0.2 CLI world for its panic and OOM plumbing even when it never
calls it (SPK-05, `docs/spikes/SPK-05/REPORT.md`). The product host provides no WASI, so such a
guest is refused at link time with `wasm.capability_denied`. Building for
`wasm32-unknown-unknown` and componentizing without an adapter avoids the WASI import set.

## Build

```sh
rustup target add wasm32-unknown-unknown
# wasm-tools, pinned: see tools/sdk-conformance/install-wasm-tools.sh
./build.sh          # or: make build
```

`out/component.wasm` is the component. `make check` prints its world with `wasm-tools`.

## The sample app

The world is `wit/world.wit` (a copy of `crates/framework/spot-wasm/wit/test-world.wit`). The
export is `on-event(event: string) -> result<string, error>`; the event is a JSON envelope:

| Event | Behaviour |
| --- | --- |
| `{"op":"step","name":"sample:check_hours","hours":"2.5"}` | ok — the hours are inside 0.25–24 |
| `{"op":"step","name":"sample:check_hours","hours":"25"}` | err `sample.hours_out_of_range` |
| `{"op":"call","name":"sample:double_hours","hours":"2.5"}` | ok — `{"value":"5.00"}` |
| any other op/name | err `sample.unknown_op` |

Hours are parsed to hundredths and formatted back in integer arithmetic (ADR-12: no floating
point for quantities).

## Run it

```sh
tools/sdk-conformance/runner/target/release/spot-sdk-conformance \
  --component sdk/templates/rust/out/component.wasm \
  --cases tools/sdk-conformance/cases.json \
  --language rust
```
