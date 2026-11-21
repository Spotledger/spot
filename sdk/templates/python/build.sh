#!/usr/bin/env sh
# Builds the Python sample-app guest into out/component.wasm.
#
# `--stub-wasi` replaces the WASI 0.2 imports the CPython runtime would otherwise declare with
# trapping stubs, so the component can link on the product host, which provides no WASI. If the
# flag is not supported by the pinned componentize-py, the build step fails and the harness
# records BLOCKED(build, ...).
set -eu
cd "$(dirname "$0")"
mkdir -p out
componentize-py -d wit -w test-world componentize --stub-wasi app -o out/component.wasm
