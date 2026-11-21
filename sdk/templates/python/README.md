# Python guest template

The sample app in Python, built with [componentize-py](https://pypi.org/project/componentize-py)
0.25.1 into a WebAssembly component.

## Build

```sh
python -m pip install componentize-py
./build.sh          # or: make build
```

`build.sh` runs

```sh
componentize-py -d wit -w test-world componentize --stub-wasi app -o out/component.wasm
```

`--stub-wasi` replaces the WASI 0.2 imports the CPython runtime would otherwise declare with
trapping stubs. Without it the component imports the whole WASI 0.2 CLI world and is refused at
link time on the product host, which provides no WASI. With it, the component imports only the
world's `log`/`now`/`get-doc`.

## Bindings

`componentize-py ... bindings guest_bindings` generates the bindings the app imports
(`wit_world`, `componentize_py_types`). The module handed to `componentize` must carry a class
named after the world — here `WitWorld` for `test-world` — implementing the generated
`wit_world.WitWorld` protocol's `on_event`, raising `componentize_py_types.Err(wit_world.Error)`
for the world's `result<string, error>`.

## Result on this machine

The component builds (about 18.5 MB) and imports only `log`/`now`/`get-doc`, but it contains
**38 core module instances** (the bundled CPython runtime). The product host's pooling allocator
caps a component at **4**, so `spot-wasm` refuses it at compile time:

```text
wasm.bad_component: The component transitively contains 38 core module instances, which exceeds
the configured maximum of 4 in the pooling allocator
```

The conformance harness therefore records `BLOCKED(parse, ...)`, not support. See `MATRIX.md`.
