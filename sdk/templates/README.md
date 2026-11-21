# Guest SDK templates

Starting points for writing a Spotledger **app guest** in a language other than Rust. Each
template is the *same sample app* over `crates/framework/spot-wasm/wit/test-world.wit`, built
into a WebAssembly component, and proven by the conformance harness in
`tools/sdk-conformance/`. Rust is the reference (`rust/`); the other languages are supported only
as far as a conformance run demonstrates.

| Directory | Language | Toolchain |
| --- | --- | --- |
| `rust/` | Rust | `wasm32-unknown-unknown` + `wasm-tools component new` (the reference) |
| `go/` | Go | `componentize-go` (Go 1.27.1+) |
| `csharp/` | C# | `componentize-dotnet` (.NET 10) |
| `javascript/` | JavaScript | `jco componentize` (ComponentizeJS) |
| `python/` | Python | `componentize-py` |

`MATRIX.md` records each toolchain's versions, WASI 0.3 status, component size and known gaps.

## The sample app (the contract every template implements)

The same behaviour as `crates/apps/spot-sample-app`'s Time Entry controller, reduced to one
DocType, one lifecycle step and one script-callable function. It is deliberately small: the
point of the templates is the *toolchain*, not the app.

The world is `spotledger:test@0.1.0` (`wit/world.wit`, a copy of
`crates/framework/spot-wasm/wit/test-world.wit`):

```wit
world test-world {
    import log: func(level: level, message: string);
    import now: func() -> s64;
    import get-doc: func(doctype: string, id: string) -> option<string>;
    export on-event: func(event: string) -> result<string, error>;
}
```

The `event` string is a JSON envelope; the result is the world's `result<string, error>`.

| Event | Behaviour |
| --- | --- |
| `{"op":"step","name":"sample:check_hours","hours":"2.5"}` | ok — hours inside 0.25–24 (inclusive) |
| `{"op":"step","name":"sample:check_hours","hours":"25"}` | err `sample.hours_out_of_range` |
| `{"op":"call","name":"sample:double_hours","hours":"2.5"}` | ok — `{"value":"5.00"}` |
| any other op/name | err `sample.unknown_op` |

Hours are parsed to hundredths and formatted back with **integer arithmetic only** (ADR-12: no
floating point for quantities). The exact cases are `tools/sdk-conformance/cases.json`, the
single source of truth the harness and the Rust contract kit share.

## Layout of a template

```
<lang>/
  README.md       what the template is, how to build and run it
  wit/world.wit   the world (a copy of spot-wasm's test-world.wit)
  <sources>       the sample app
  build.sh        produces out/component.wasm
  Makefile        `make build` / `make check` / `make clean`
```

A build must leave the component at `out/component.wasm` (the harness's `images.toml` names it).

## Adding a language

1. **Research it first.** Add a row to `MATRIX.md`: the toolchain and version, its WASI 0.3
   status, how it produces a component from the world, the component size, the start-up cost and
   the known gaps. Check the toolchain's own current documentation; state the version you tested.
2. **Write the template.** Copy the nearest template and implement the sample app. Keep the
   integer-only decimal handling. `wit/world.wit` is a copy of `spot-wasm`'s world — keep it in
   step (the harness never reads the copy, the guest build does).
3. **Make `build.sh` produce `out/component.wasm`** and a `Makefile` with `build`, `check` and
   `clean` targets.
4. **Add an entry to `tools/sdk-conformance/images.toml`.** Pin the image: an `@sha256:` digest
   or an explicit version tag (never `latest`). Give the `provision` command (what installs the
   toolchain the image does not ship) and the `build` command.
5. **Run it.** `python tools/sdk-conformance/run.py --run --lang <lang>`. If the component does
   not link on the no-WASI host, the run records `BLOCKED(<step>, <reason>)` — write that step
   and reason into `MATRIX.md` rather than claiming support.
6. **Record the result.** A language is *supported* only once a conformance run shows it building
   and passing every case. Never claim support the run did not demonstrate.

## Why most languages are blocked

The product host (`crates/framework/spot-wasm`) provides **no WASI** and no ambient authority. A
guest that imports any WASI interface is refused at link time with `wasm.capability_denied`. Rust
avoids WASI by building for `wasm32-unknown-unknown` and componentizing without an adapter;
several other toolchains emit the WASI 0.2 CLI world for their runtime's plumbing (SPK-05,
`docs/spikes/SPK-05/REPORT.md`). Some toolchains offer a stub or "pure component" route that
removes the WASI imports; where they do, the template uses it.
