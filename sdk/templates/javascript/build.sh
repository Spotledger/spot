#!/usr/bin/env sh
# Builds the JavaScript sample-app guest into out/component.wasm.
#
# Two routes: the default StarlingMonkey backend, and the QuickJS backend with stubbed WASI.
# StarlingMonkey imports WASI 0.2 (wasi:io/poll), which the product host does not provide, so the
# harness records that and the conformance run can fall back to QuickJS. `--disable-features`
# builds a "pure" component with no WASI at all; it is tried first because it is the only route
# that can link on the no-WASI host.
set -eu
cd "$(dirname "$0")"
mkdir -p out
if [ ! -d node_modules/@bytecodealliance/jco ]; then
  npm install --no-audit --no-fund
fi
npx --no-install jco componentize guest.js --wit wit -n test-world \
  --disable all \
  -o out/component.wasm
