# Guest toolchain matrix (WP-STR-07.T1)

Research notes for the guest SDK templates, written before the templates. Every claim about a
toolchain's current behaviour cites the toolchain's own documentation, checked on **2026-10-17**
(the `docs/blueprint` date). The "tested here" rows are what the conformance harness
(`tools/sdk-conformance/`) actually observed on this machine; a language is *supported* only when
a run demonstrates it.

## The world and how a component is produced

All templates target the same world, `spotledger:test@0.1.0`, a copy of
`crates/framework/spot-wasm/wit/test-world.wit`:

```wit
world test-world {
    import log: func(level: level, message: string);
    import now: func() -> s64;
    import get-doc: func(doctype: string, id: string) -> option<string>;
    export on-event: func(event: string) -> result<string, error>;
}
```

| Language | How the component is produced from the world |
| --- | --- |
| Rust | `wit-bindgen` bindings, build for `wasm32-unknown-unknown`, `wasm-tools component new` (no WASI) |
| Go | `componentize-go --world test-world build`, bindings from `componentize-go ... bindings` (wit-bindgen-go) |
| C# | `componentize-dotnet` (`<Wit Update="wit/world.wit" World="test-world" />`), `dotnet build` |
| JavaScript | `jco componentize guest.js --wit wit -n test-world` (ComponentizeJS) |
| Python | `componentize-py -d wit -w test-world componentize app` |

## WASI 0.3 status (as of 2026-10-17)

WASI 0.3.0 shipped **2026-06-11** and 0.3.1 on **2026-08-11**; 0.3.2 was planned for 2026-10-13.
WASI 0.3 rebases WASI on the Component Model's native async primitives (`async func`,
`stream<T>`, `future<T>`) and removes `wasi:io` (`wasi.dev/releases/wasi-p3`,
`bytecodealliance.org/articles/WASI-0.3`).

The `wasi.dev` language table (`wasi.dev/languages`) is the authority:

| Language | Toolchain | Output | WASI version |
| --- | --- | --- | --- |
| Rust | `wasm32-wasip2` (Tier 2), `wasm32-wasip3` (Tier 3, nightly) | modules and components | 0.1, 0.2; **0.3 via the `wasip3` crate** |
| Go | `go-modules` / TinyGo | components | 0.2 |
| C# | `componentize-dotnet` | components | 0.2 (pre-release toolchain) |
| JavaScript/TypeScript | `jco` | components | 0.2; **0.3 experimental** (`preview3-shim`) |
| Python | `componentize-py` | components | 0.2 |

So **no guest toolchain in this matrix emits a WASI 0.3 world by default**: all target WASI 0.2,
and only Rust (nightly) and jco (experimental) reach 0.3. The sample app's world is synchronous,
so WASI 0.3 async is **not exercised** by any template; a future async world would be the place
to test it.

The product host pins **wasmtime 49.0.5** (`crates/framework/spot-wasm/Cargo.toml`) but does
**not** enable `component-model-async`, and provides **no WASI** at all.

## The matrix

Component sizes marked *(SPK-05)* are from `docs/spikes/SPK-05/REPORT.md`; the rest are measured
by this task's conformance run.

| Language | Toolchain (version) | WASI 0.3 | Component size | Start-up | Known gaps |
| --- | --- | --- | --- | --- | --- |
| **Rust** | rustc 1.98.1, wit-bindgen 0.57.1, wasm-tools 1.262.0, target `wasm32-unknown-unknown` | 0.3 via `wasm32-wasip3`/`wasip3` (nightly) | **34,680 B** | ~0.1 ms warm (SPK-05) | none for this world: the guest imports nothing |
| **Go** | Go 1.27.1+, `componentize-go` | 0.3 route: `componentizego_async` build tag for async worlds | not measured | — | not built here (no Go toolchain on the box); TinyGo's component tooling is unmaintained and only targets `wasi:cli/command` |
| **C#** | .NET SDK 10.0.401, `componentize-dotnet` 0.8.0-preview | 0.2 only (pre-release toolchain) | **2,313,435 B** (this run) | — | emits the full WASI 0.2 CLI world (`wasi:io/poll@0.2.6`, clocks, cli, filesystem, random); needs a WASI 0.2 host |
| **JavaScript** | Node.js 26.8.1, jco 1.39.0, componentize-js 0.23.0 | 0.3 experimental (`preview3-shim`) | **11,986,024 B** (this run, `--disable all`) / 1,656,508 B (QuickJS) / 12,000,130 B (StarlingMonkey) (SPK-05) | 539 µs (SPK-05) | supported here **only** with `--disable all` (a pure component); StarlingMonkey imports `wasi:io/poll@0.2.12`; QuickJS with stubbed WASI uses the Component Model async builtin `task.cancel`, which the host does not enable |
| **Python** | CPython 3.14.8, componentize-py 0.25.1 | 0.2 only | **18,506,343 B** (this run, `--stub-wasi`) | — | the CPython runtime's component has **38 core module instances**; the product host's pooling allocator caps a component at **4**, so it does not even compile |

## What this machine's conformance run found

`python tools/sdk-conformance/run.py --run --engine local`, with wasm-tools 1.262.0, jco 1.39.0,
componentize-py 0.25.1 and .NET SDK 10.0.401:

| Language | Status | Evidence |
| --- | --- | --- |
| rust | **SUPPORTED** | `out/component.wasm` (34,680 B) imports **nothing**, exports `on-event`, and passes all **7/7** contract cases through `spot-wasm` |
| javascript | **SUPPORTED** | `jco componentize --disable all` produced an 11,986,014 B "pure component" importing only `log`/`now`/`get-doc`, exporting `on-event`, passing **7/7** cases |
| csharp | **BLOCKED(instantiate, ...)** | `dotnet build` produced a 2,313,435 B component exporting `on-event`, but it imports the WASI 0.2 CLI world: `wasm.capability_denied: the app imports 'wasi:cli/environment@0.2.6', which the host does not provide` |
| python | **BLOCKED(parse, ...)** | `componentize-py ... --stub-wasi` builds an 18.5 MB component that imports only `log`/`now`/`get-doc` (no WASI), but it contains **38 core module instances** and the host's pooling allocator allows **4 per component**: `wasm.bad_component: The component transitively contains 38 core module instances, which exceeds the configured maximum of 4 in the pooling allocator` |
| go | **BLOCKED(build, ...)** | no Go toolchain on the box: `./build.sh: line 10: componentize-go: command not found`. The template follows componentize-go's documented flow |

Four of the five languages were actually built and run here (Go was not: no toolchain). This is
the honest state: the matrix claims nothing it did not observe.

### The finding that matters

The Python result is not a WASI problem — `--stub-wasi` removed the WASI imports — it is a
**host pool limit**. `WasmHost` builds its pooling allocator with
`max_core_instances_per_component(4)` (`crates/framework/spot-wasm/src/host.rs`), so a component
that instantiates many core modules (a bundled interpreter such as CPython) is refused at
compile time. Any language whose runtime is embedded as several core modules (Python today,
likely StarlingMonkey) will hit this before it hits the no-WASI rule. If the product wants to
support such guests, that cap is the thing to revisit — it is **not** in this task's owned paths,
so it is recorded under `## Open questions` in the report, not changed here.

## Sources

- `docs/spikes/SPK-05/REPORT.md` — the handle-vs-value measurements and the first toolchain
  matrix (Rust, JS, Python, C#, Go).
- `wasi.dev/languages`, `wasi.dev/releases/wasi-p3`, `wasi.dev/roadmap`.
- `bytecodealliance.org/articles/WASI-0.3` (WASI 0.3 launch).
- `github.com/bytecodealliance/componentize-go` (Go 1.27.1+, `componentizego_async`).
- `github.com/bytecodealliance/componentize-dotnet` (WASI 0.2, pre-release).
- `github.com/bytecodealliance/ComponentizeJS` and `github.com/bytecodealliance/jco`
  (StarlingMonkey, QuickJS, `disableFeatures` for a "pure component").
- `pypi.org/project/componentize-py` 0.25.1 (2026-09-12) and the componentize-py README
  (`--stub-wasi`).
- `component-model.bytecodealliance.org/language-support/building-a-simple-component/` (C#, TinyGo).
- `tinygo.org/docs/guides/webassembly/wasi/` and TinyGo v0.42.0 (2026-08-31).
