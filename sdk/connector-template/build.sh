#!/usr/bin/env sh
# Builds the connector to a WebAssembly component.
#
# Requires the `wasm32-wasip2` target (`rustup target add wasm32-wasip2`). The output is a
# component under `target/wasm32-wasip2/release/` that `ConnectorHost::install` loads.
set -eu
cd "$(dirname "$0")"
cargo build --release --target wasm32-wasip2
