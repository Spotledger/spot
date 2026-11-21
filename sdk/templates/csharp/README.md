# C# guest template

The sample app in C#, built with
[componentize-dotnet](https://github.com/bytecodealliance/componentize-dotnet)
(`BytecodeAlliance.Componentize.DotNet.Wasm.SDK`) on .NET 10.

## Build

```sh
./build.sh          # or: make build
```

`build.sh` runs `dotnet build -c Release` and copies the `.component.wasm` componentize-dotnet
leaves under `bin/Release` to `out/component.wasm`.

## Bindings

`App.csproj` points `<Wit Update="wit/world.wit" World="test-world" />` at the world;
`WorldExportsImpl.cs` implements the generated export. The generated namespace and interface name
come from wit-bindgen; adjust them to match the files under `generated/wit` if a toolchain
version names them differently.

## Status

**Built here, blocked at instantiation.** `dotnet build` (with the
`runtime.linux-x64.Microsoft.DotNet.ILCompiler.LLVM` runtime pack) produced a 2,313,435 B
component that exports `on-event`, but it imports the **WASI 0.2 CLI world**
(`wasi:io/poll@0.2.6`, `wasi:clocks/*`, `wasi:cli/*`, `wasi:filesystem/*`, `wasi:random/*`), so
the product host refuses it at link time:

```text
BLOCKED(instantiate, wasm.capability_denied: the app imports `wasi:cli/environment@0.2.6`,
which the host does not provide)
```

This matches SPK-05 (`docs/spikes/SPK-05/REPORT.md`): componentize-dotnet targets WASI 0.2 and
cannot avoid the runtime's import set. The conformance run records the blocked step and reason.
See `MATRIX.md`.
