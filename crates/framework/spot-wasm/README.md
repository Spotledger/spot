# spot-wasm

The WebAssembly component host (WP-EXT-05.T1). It runs app components with hard resource limits and
no ambient authority; the acceptance sentence of the work package is the design: *a sample app
installs, runs, and is killed on timeout without affecting the runtime.*

```text
WasmHost::new(config)                      one engine, one pool, one ticker thread
  .compile(bytes)          -> CompiledComponent   validated, SHA-256 content hash
  .instantiate(&c, caps)   -> AppInstance         links ONLY the imports `caps` allows
AppInstance::call(export, args, Limits, imports)  a fresh pooled instance per call
```

## Isolation, in the order a call meets it

1. **Slot.** A semaphore with one permit per pool slot (`max_concurrent_calls`, default 64). A call
   waits for a slot rather than failing; the permit is released however the call ends.
2. **Instance.** A fresh `Store` and instance from the pooling allocator's slabs. Nothing survives
   between calls (tested: a fixture that remembers a byte in memory never sees it again).
3. **Limits** ([`Limits`]), all enforced by the runtime:

   | Limit | Mechanism | Error |
   | --- | --- | --- |
   | `fuel` | `consume_fuel` | `wasm.out_of_fuel` |
   | `wall_clock` | epoch interruption; one ticker thread advances the epoch every `tick` (default 5 ms), a call is a number of ticks | `wasm.timeout` |
   | `memory_bytes`, `table_elements` | a `ResourceLimiter` that traps on growth past the limit; the pool's slab caps it from above | `wasm.memory` |
   | `host_calls` | counted in every host function | `wasm.too_many_host_calls` |
   | `output_bytes` | checked on the lifted value before it is converted to JSON | `wasm.output_too_large` |
   | the wasm stack | `max_wasm_stack` (512 KiB) | `wasm.trap` |

   A guest in a long loop also yields to the async runtime every 100,000 fuel, so it cannot starve
   other tasks on its thread (tested: eight loops on two threads, ordinary calls still finish).
   An async backstop (`wall_clock` + 250 ms) covers a call stuck where the epoch cannot reach, such
   as a slow host import.
4. **Capabilities.** The linker defines only the host imports the app was granted
   (`GrantedCapabilities`). An import the host does not provide at all (any WASI interface, an unknown
   function) or one the app lacks the grant for is `wasm.capability_denied` at link time, naming the
   import. `log` and `now` carry no authority (and `now` is the time of the invocation, never the
   machine's clock); `get-doc` needs read access to at least one DocType and is then checked per
   DocType on every call.

## The test world

`wit/test-world.wit` is the **test** world: `on-event(event: string) -> result<string, error>` and the
imports `log`, `now`, `get-doc`. It is not the product world (WP-EXT-05.T2).

The fixtures in `tests/common/mod.rs` are components written as WebAssembly text and assembled by the
`wat` crate; no `wasm32-wasip2` target or other tooling is needed. A skeleton declares the world's
types and the imports a fixture asks for, lowers them with the canonical ABI (UTF-8, a bump allocator
in a `libc` core module) and lifts `on-event`; each fixture is only the body of that core function.
`the_fixtures_have_the_shape_of_the_test_world` checks the export and import types against the WIT.
To add a fixture: write a `const` with its body (it receives `$ptr`, `$len` of the event string and
returns a pointer to the `result<string, error>` that `$ret_ok` or `$ret_err` writes).

| Fixture | What it does | Proves |
| --- | --- | --- |
| `ECHO` | returns its argument | the happy path through the canonical ABI |
| `USES_IMPORTS` | `log`, `now`, `get-doc` | imports reach the host through the granted interface |
| `READS_SUPPLIER` | `get-doc("supplier", ..)` | a granted import is still checked per DocType |
| `FAILS_ON_PURPOSE` | returns `err` | an app's own error is not a host failure (`wasm.app_error`) |
| `SPIN` | endless loop | `wasm.out_of_fuel`, `wasm.timeout` |
| `GROW_FOREVER` | `memory.grow` without end | `wasm.memory` |
| `TRAPS`, `RECURSES` | `unreachable`; endless recursion | `wasm.trap` |
| `CHATTY` | `now()` without end | `wasm.too_many_host_calls` |
| `HUGE_OUTPUT` | an 8 MB string | `wasm.output_too_large` |
| `REMEMBERS` | fails if it sees an earlier call's memory | nothing survives between calls |
| `WANTS_MAIL`, `WANTS_WASI` | import what the host does not provide | `wasm.capability_denied` |

## Calls take and return JSON

Arguments are a JSON array converted by the export's declared parameter types, and the result
likewise (`src/json.rs` has the table). `result<T, E>`: `ok` is the value, `err` is
`wasm.app_error` carrying `E` as JSON. Floating point types are not supported.

## Costs

Measured by `tests/bench.rs` (`cargo test -p spot-wasm --test bench -- --nocapture`), on the
development workstation, mean of 500 calls for the warm figure. The tests assert only loose ceilings
(debug build, shared CI machine).

| Step | Debug build | Release build |
| --- | --- | --- |
| Engine and pool build (`WasmHost::new`) | 1.6 ms | 0.8 ms |
| Compile a component (validate and codegen) | 62 ms | 5.9 ms |
| Link (`instantiate_pre`) | 74 µs | 11 µs |
| First call, a cold pool slot | 0.47 ms | 0.10 ms |
| **Warm pooled call** (a fresh instance each time; `get-doc`, `log` and `now` host calls included) | **0.25 ms** | **9.5 µs** |

Reserved address space is `max_concurrent_calls * 2 * max_memory_bytes` (64 slots: 4 GiB virtual);
resident memory is only what calls touch, and the pool decommits memory between calls.

## Not here

Choosing which events reach which app, reading manifests, persisting anything, and the product WIT
world.
