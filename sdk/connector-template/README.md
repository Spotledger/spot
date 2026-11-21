# spot-connector-template

The starting point for a Spotledger **connector**: a Rust guest compiled to a WebAssembly
component that exports the `connector` world (`wit/connector.wit`) and runs on
`spot-connectors::ConnectorHost` under the same sandbox as an app — no network, no filesystem,
no clock of its own, stopped by fuel, a wall-clock deadline and a memory cap.

This directory is a template, not a workspace member. Copy it, rename the crate, and fill in
`src/lib.rs`.

## What is here

| Path | What |
| --- | --- |
| `src/lib.rs` | The guest skeleton: the `wit-bindgen` bindings and the `Guest` impl to fill in |
| `wit/connector.wit` | A copy of the world (keep it in step with `spot-connectors`) |
| `connector.toml` | The connector manifest the host reads |
| `build.sh` | Builds the component to `target/wasm32-wasip2/release/` |
| `examples/echo-http/` | A complete sample connector: `echo` and a REST `fetch` |

## Building

The component toolchain is **not** part of the platform workspace: a connector is a
`cdylib` built for `wasm32-wasip2` and componentised with `wit-bindgen`. Build with:

```sh
rustup target add wasm32-wasip2
./build.sh
```

`build.sh` runs `cargo build --release --target wasm32-wasip2`. If the toolchain is not
available, the component cannot be built here; the host tests in `spot-connectors` run the
same world against hand-written WebAssembly text, so the host itself is always proven without
the guest toolchain. When you have a toolchain, point `ConnectorHost::install` at the built
`.wasm`.

## Testing without a network

`spot-connectors` ships a test kit (`spot-connector-testkit` module under the `testkit`
feature): `MockHttp` records every outbound request and answers from recorded exchanges, and
`check_mapping` runs a `MappingSpec` against a sample. A connector's own tests drive the guest
against it:

```rust
use spot_connectors::{ConnectorConfig, ConnectorHost, ConnectorId, ConnectorRuntime, Vault};
use spot_connectors::testkit::MockHttp;
```

## The world

A connector exports:

- `describe() -> ConnectorInfo` — name, version, operations (JSON Schema in and out) and the
  auth kinds it supports.
- `invoke(op, input, ctx) -> result<output, error>` — one operation. `input` and `output` are
  JSON; `ctx` carries the tenant, the actor, the invocation time and the deadline.
- `webhook(request) -> result<response, error>` — an inbound request.

and imports `log`, `http-request` (through the egress policy), `secrets-get` (only a reference
it declared), `kv-get` and `kv-set`.

A connector **never** holds a token for `bearer`, `basic` or `oauth2`: the host injects the
declared credential into the outbound request. Never log or return a secret.
