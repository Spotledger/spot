#!/usr/bin/env sh
# Builds the Go sample-app guest into out/component.wasm.
#
# componentize-go generates the wit-bindgen-go bindings for the world, tidies the module, then
# compiles the component. Requires Go 1.27.1+ and componentize-go on PATH (see go.mod's `tool`
# directive).
set -eu
cd "$(dirname "$0")"
mkdir -p out
componentize-go --world test-world bindings --format
go mod tidy
componentize-go --world test-world build
cp main.wasm out/component.wasm
