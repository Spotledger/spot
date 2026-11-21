# spot-ts

The embedded JavaScript engine for TypeScript scripts (WP-EXT-08.T1, ADR-054): QuickJS through
`rquickjs` (pinned `=0.14.0`), embedded in the binary. TypeScript is stripped to JavaScript on save
(WP-EXT-08.T2); this crate only ever runs plain JavaScript. The acceptance sentence of the work
package is the design: *a runaway script is killed without affecting the runtime, and startup and
per-call latency stay within budget.*

```text
ScriptEngine::new(EngineConfig { surface, .. })       cache + allow-list; cheap to clone
  .compile(js, ScriptId { app, name, version }, map)  -> Arc<Compiled>   bytecode + SHA-256 of the source
  .run(&compiled, "entry", args, bridge, Limits)      -> Result<Json, ScriptError>   (async)
  .run_blocking(...)                                  the same, for a caller that is not async
```

A script is an **ES module**; its exported functions are the entry points, each called as
`entry(args)` with the JSON given to `run`, and may be `async`. (QuickJS reads and writes bytecode for
modules only.)

## One execution

Each execution gets a thread of its own, sized for its stack allowance, a QuickJS runtime and a
context, made for it and dropped with it (`src/exec.rs`). Nothing is shared between executions,
tenants or calls: the next run sees none of the previous run's globals, pollution of built-ins or
memory. Tested: 1,000 executions, each ended by a different limit, grow resident memory by 0 KiB, and
the next run still works.

| Limit (`Limits`) | Mechanism | Error |
| --- | --- | --- |
| memory (32 MiB) | `set_memory_limit` | `script.memory_limit` |
| native stack (256 KiB) | `set_max_stack_size` | `script.stack_overflow` |
| wall clock (1 s) | the interrupt handler, every ~10,000 instructions | `script.timeout` |
| instruction budget (10^9) | the same handler, counting its ticks | `script.out_of_budget` |
| host calls (1,000) | counted in every host function | `script.too_many_host_calls` |
| output (1 MiB) | checked on the serialised result | `script.output_too_large` |

**The interrupt is uncatchable**: QuickJS-ng marks the "interrupted" exception uncatchable, so a
script that wraps a loop in `try/catch/finally` is still killed at the next tick (tested, in both
sync and async forms). Limits the engine enforces decide the error whatever the script saw: a script
that catches the host-call limit's exception is still reported as `script.too_many_host_calls`.

**The thread is sized.** QuickJS checks its stack allowance against the real native stack, so on an
ordinary 2 MiB thread an allowance above that overflows the real stack, which aborts the whole
process. The engine therefore never runs a script on a thread it did not size: the stack is the
allowance (clamped to 16 MiB) plus 1 MiB of headroom.

## What a script can reach

The global object holds the language's built-ins (`Object`, `Array`, `JSON`, `Math`, `Date`, `Map`,
`Set`, `Promise`, `RegExp`, typed arrays, ...) and `host`. A test lists every global name and checks
that none of `require`, `process`, `fetch`, `setTimeout`, `console`, `WebAssembly`, ... exists. There
is no module system beyond the script itself: a static `import` fails to compile
(`script.compile: imports are not allowed`), a dynamic `import()` and `require` fail when they run.

`host` is **frozen** and holds exactly the allow-listed functions of the engine's `HostSurface`
(each with an argument `Shape` checked before the bridge sees the call), plus `now()` and
`random()`. Three things QuickJS does have are taken away before the script runs:

- `eval` and every `Function` constructor (plain, async, generator, async generator): QuickJS cannot
  compile *anything* without its `Eval` intrinsic (the embedder needs it for the script itself), so
  they are replaced by functions that throw `TypeError`;
- `Date.now()` and `new Date()` with no argument answer with `HostBridge::now_millis()`;
- `Math.random()` answers from `HostBridge::random_seed()` (splitmix64; the same seed gives the same
  numbers). The generator hands the script 53-bit integers and the script side divides, so no floating
  point arithmetic is written in this crate.

A host function's failure is a thrown `HostError` (with a `code`) that the script may catch. A host
function that **panics** is caught: the execution ends with `script.host_panic`, the engine is fine.

## Bytecode

`compile` writes QuickJS bytecode (debug info kept, source stripped). The cache is keyed by `ScriptId`
(app, name, **version**), holds source and bytecode up to a byte cap, and evicts the least recently
used script; an unchanged script is not recompiled and a new version is a new entry.

QuickJS reads bytecode with `JS_READ_OBJ_ROM_DATA`, so the bytes must outlive the runtime, and the
only safe loader reachable from a crate that forbids `unsafe` (rquickjs' `Bundle`) takes `&'static`
data. Bytecode is therefore **interned once by content hash for the life of the process**, up to
`INTERN_BUDGET` (64 MiB). Past that a script keeps no bytecode and runs from source: slower to start,
identical in every other respect (every test runs both ways). `EngineConfig::keep_bytecode = false`
forces that mode.

## Errors

`ScriptError` has a `script.*` code on every variant (the catalogue entries are in
`spot-errors/catalogue/script.toml`). An uncaught exception is `Thrown { name, message, stack,
position }`; `position` is the original TypeScript position when a source map was given
(`SourceMap`, version 3, decoded here; WP-EXT-08.T2 produces the maps). A script signals a validation
problem by throwing `{ code, message, fields: [{ field, code, message }] }`:
`ScriptError::into_app_error` maps it to a `Validation`-class `AppError` that keeps the script's code
and the field errors; every other failure maps to class `Internal`.

## Costs

`tests/bench.rs` (`cargo test -p spot-ts --test bench -- --nocapture`), mean of 300 calls on the
development workstation. Every call includes creating the thread, the QuickJS runtime and context,
and running the prelude. The tests assert only loose ceilings (50 ms cold, 20 ms a trivial call).

| | Debug build | Release build |
| --- | --- | --- |
| First call (cold: first runtime, first thread) | 1.8 ms | 0.59 ms |
| Trivial call, from bytecode | 1.05 ms | 0.40 ms |
| Trivial call, from source | 1.10 ms | 0.36 ms |
| A call with ten host calls, from bytecode | 1.38 ms | 0.39 ms |
| `compile` of a small script | 17 µs | 5 µs |

(The scripts here are tiny, so bytecode saves little; it matters for large scripts.)
