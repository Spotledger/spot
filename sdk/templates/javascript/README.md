# JavaScript guest template

The sample app in JavaScript, built with
[jco](https://github.com/bytecodealliance/jco) `componentize` (ComponentizeJS) into a WebAssembly
component.

## Build

```sh
npm install
./build.sh          # or: make build
```

`build.sh` runs

```sh
jco componentize guest.js --wit wit -n test-world \
  --disable-features stdio random clocks http fetch-event \
  -o out/component.wasm
```

`--disable-features` builds a **"pure component"** with no WASI dependence at all, which is the
only ComponentizeJS route that can link on the product host. Without it, the StarlingMonkey
backend imports `wasi:io/poll` and is refused at link time; the QuickJS backend with stubbed WASI
(`--backend qjs --backend-qjs-stub-wasi`) runs but uses the Component Model async builtin
`task.cancel`, which the host does not enable.

## Result shape

The WIT export `on-event` is the ES module export `onEvent`; the world's
`result<string, error>` is the ComponentizeJS tagged form `{ tag: 'ok', val }` /
`{ tag: 'err', val }`.

## Status

**Built and passing here.** `jco componentize ... --disable all` produced an 11,986,024 B "pure
component" that imports only the world's `log`/`now`/`get-doc` (no WASI) and passes all **7/7**
contract cases through `spot-wasm` (SHA-256 `b62a0188…`). `--disable all` is the only
ComponentizeJS route that links on the no-WASI host:

- the default StarlingMonkey backend imports `wasi:io/poll` and is refused at link time;
- the QuickJS backend with stubbed WASI (`--backend qjs --backend-qjs-stub-wasi`) runs but uses
  the Component Model async builtin `task.cancel`, which the host does not enable.

The `wasi.dev` table lists jco components as WASI 0.2 with 0.3 experimental; the world is
synchronous, so WASI 0.3 async is not exercised. See `MATRIX.md`.
