#!/usr/bin/env sh
# Builds the reference guest into out/component.wasm.
#
# Two steps: compile a core module for wasm32-unknown-unknown (no WASI), then componentize it
# with wasm-tools. The product host provides no WASI, so the module must not import any; a
# `wasm32-wasip2` std guest would, and would not link.
set -eu
cd "$(dirname "$0")"
mkdir -p out
cargo build --release --target wasm32-unknown-unknown
wasm-tools component new \
  target/wasm32-unknown-unknown/release/spot_sample_guest.wasm \
  -o out/component.wasm
