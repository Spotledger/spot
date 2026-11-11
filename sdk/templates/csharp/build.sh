#!/usr/bin/env sh
# Builds the C# sample-app guest into out/component.wasm.
#
# componentize-dotnet builds the NativeAOT-LLVM component and leaves it next to the native
# output; the script copies the `.component.wasm` it finds to out/component.wasm.
set -eu
cd "$(dirname "$0")"
mkdir -p out
dotnet build -c Release
# componentize-dotnet leaves the component under bin/Release. Older versions name it
# `<Assembly>.component.wasm`; this one publishes `<Assembly>.wasm` directly. Prefer the
# explicit component name, else the native output.
component="$(find bin/Release -name '*.component.wasm' -print -quit)"
if [ -z "$component" ]; then
  component="$(find bin/Release/net10.0/wasi-wasm/native -name '*.wasm' -print -quit)"
fi
if [ -z "$component" ]; then
  echo "componentize-dotnet produced no component under bin/Release" >&2
  exit 1
fi
cp "$component" out/component.wasm
