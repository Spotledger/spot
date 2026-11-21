# Go guest template

The sample app in Go, built with
[componentize-go](https://github.com/bytecodealliance/componentize-go) (Go 1.27.1+) and
wit-bindgen-go bindings.

## Build

```sh
# componentize-go, from go.mod's `tool` directive:
go install github.com/bytecodealliance/componentize-go@latest
./build.sh          # or: make build
```

`build.sh` runs

```sh
componentize-go --world test-world bindings --format
go mod tidy
componentize-go --world test-world build
cp main.wasm out/component.wasm
```

## Bindings

`main.go` implements the world's export `on-event` and registers the component. The generated
package is imported as `wit_component` and the export as `OnEvent`, with the error record
`wit_component.Error`; these names are produced by wit-bindgen-go from the world and may differ
between toolchain versions. Run `componentize-go --world test-world bindings --format` and adjust
the import and the method name to match.

## Status

**Not built here**: this machine has no Go toolchain. The template follows componentize-go's
documented flow and the `wasi.dev` language table (Go components target WASI 0.2; componentize-go
passes the `componentizego_async` build tag when the world uses async features). The conformance
run records `BLOCKED(build, ...)` until a run demonstrates it. See `MATRIX.md`.

TinyGo is the other Go route, but its component tooling is unmaintained and only targets the
`wasi:cli/command` world, so this template uses componentize-go.
