# sdk-conformance — the guest SDK conformance harness (WP-STR-07.T1)

Proves the acceptance sentence of WP-STR-07: **the same sample app builds and passes the
contract tests in each supported language.** WASI 0.3 and Component Model toolchains mature at
different speeds, so the deliverable is a *matrix* that honestly shows which languages work
today, plus clear `BLOCKED(<step>, <reason>)` entries for the rest.

## Shape

```
run.py            the harness: --list, --run, --json, --markdown, --engine
images.toml       the pinned build image and commands per language
cases.json        the sample-app contract cases (shared with the Rust kit)
matrix.py         the matrix model: statuses, the overall verdict, JSON and Markdown
images.py         images.toml parsing and the pinning rule
contract.py       cases.json parsing and validation
install-wasm-tools.sh   pins the wasm-tools release a container installs
tests/            unit tests (stdlib only, no containers, no Rust)
runner/           the Rust contract kit `spot-sdk-conformance` (uses spot-wasm)
```

## Use

```sh
python tools/sdk-conformance/run.py --list               # the configured matrix, nothing built
python tools/sdk-conformance/run.py --run                # build and test every language
python tools/sdk-conformance/run.py --run --lang python  # one language
python tools/sdk-conformance/run.py --run --engine local --lang rust
python tools/sdk-conformance/run.py --run --dry-run      # print the container commands
python tools/sdk-conformance/run.py --run --json matrix.json --markdown MATRIX-RESULTS.md
```

`--engine auto` uses the first of `podman`/`docker` on `PATH`. `--engine local` builds on the
host (the toolchain must already be installed) — the escape hatch for a developer with a
toolchain and no container.

The harness is standard-library-only. The only heavy dependency is the Rust contract kit, which
links `spot-wasm` (and `wasmtime`); build it once:

```sh
cd tools/sdk-conformance/runner && cargo build --release
```

## The contract kit

`runner/` is a bin-only, **detached** crate (`[workspace]`, like
`crates/apps/spot-sample-app/tests/app-validate`) so it never enters the root workspace's
dependency or lint graph, and the work package's owned paths stay `sdk/templates/` and
`tools/sdk-conformance/`. It loads the built component the way `spot-wasm` loads an app:

1. **parse and validate** (`WasmHost::compile`);
2. **link with the granted capabilities** (`WasmHost::instantiate`) — a component importing
   anything the host does not provide (any WASI interface, an unknown function) is refused with
   `wasm.capability_denied`, naming the import;
3. **run the cases** from `cases.json`, one `on-event` call each, compared by the world's
   `result<string, error>` shape.

It exits 0 for every component outcome (a blocked component is a *result*, not a failure) and
writes one JSON report, which `run.py` maps onto a matrix row.

## How a language is recorded

| Situation | Row |
| --- | --- |
| image not pinned | `BLOCKED(image, ...)` |
| no container engine | `BLOCKED(engine, ...)` |
| image pull failed | `BLOCKED(pull, ...)` |
| build command failed | `BLOCKED(build, <last line>)` |
| build produced no component | `BLOCKED(component, ...)` |
| contract kit not built | `BLOCKED(runner, ...)` |
| component does not parse/export/instantiate | `BLOCKED(<parse\|export\|instantiate>, <reason>)` |
| component ran but a case disagreed | `FAILED` |
| component ran and every case passed | `SUPPORTED` |

The overall matrix **passes** when the reference language (`rust`) is `SUPPORTED`, every
`SUPPORTED` row ran at least one case and passed them all, and every `BLOCKED` row names a step
and a reason. A `FAILED` row fails the matrix. `--list` rows are `NOT_RUN` and do not fail it.

## Tests

```sh
python -m unittest discover -s tools/sdk-conformance/tests -v
```

They cover the matrix logic (status parsing and rendering, the overall verdict, JSON and Markdown
round trips), the image-config parsing and pinning rule, the case validation, and the harness's
command construction and report mapping. No containers and no Rust are needed.
