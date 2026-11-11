# tools/lints/features

Keeps the `testing` feature of the framework crates out of product builds. A framework crate may
offer a `testing` feature (fakes, helpers, and for `spot-document` the `SecretCaller::Test`
caller). Only a test turns it on: a `[dev-dependencies]` entry, or a crate's dev-dependency on
itself. Run by the `gates` job of `.github/workflows/ci.yml` (every `tools/lints/*/run.sh` is run
there).

```sh
bash tools/lints/features/run.sh                                 # exit 0 clean, 1 finding, 2 unreadable
python3 -m unittest discover -s tools/lints/features/tests -v
```

## What fails

1. A normal or build dependency of a workspace package that enables `testing` on a framework
   crate: by name, by a feature that turns it on, or through the crate's default features.
2. A product binary whose normal and build closure has `testing` enabled on a framework crate,
   however it got there. The gate asks cargo (`cargo tree -p <binary crate> -e normal,build
   --target all`) for the resolved features, so forwarded features are seen.
3. The unified product build: cargo resolves one feature set per crate for everything built in
   one invocation, so a binary that never asks for `testing` still gets it when a neighbour does.
   The gate resolves all product binaries together (`cargo tree -p a -p b ... -e normal,build
   --target all`) and reports every binary that uses a framework crate that has `testing` on
   there, naming the binaries that turn it on.
4. `non_product` or `[[allow_edge]]` has an entry that `baseline.toml` does not list, or a
   `non_product` entry has no reason (see below).
5. Anything unreadable: a manifest cargo cannot parse, a cargo tree line that does not parse, a
   missing or malformed `baseline.toml` or `rules.toml`. The gate fails closed (exit 1 or 2).

A product binary is any workspace member with a binary target, wherever it lives (`apps/`,
`crates/backends/`, `crates/apps/`, `harness/`, `tools/`), that `rules.toml` does not list under
`[non_product]`. A new binary crate is checked by default.

## Build shapes

Modelled: the product release build, that is every product binary built together
(`cargo build --release -p <each product binary>`) with normal and build dependencies, for all
target platforms, with the default feature selection. Not modelled: the workspace test build
(`cargo test --workspace` and `cargo build --workspace --all-targets` unify dev-dependencies, so
`testing` is on there by design), `--all-features` and `--features` flags on the command line,
and a product built together with the non-product binaries (they are test fakes and tools).
Building a product with extra `-p` packages outside the product set can therefore still unify
features that this gate does not see.

## The non_product list and the baseline

`[non_product.packages]` in `rules.toml` is a table of `name = "reason"` (one line each). It is a
bypass, so `baseline.toml` records the committed names and counts, and the gate fails when
`non_product` or `[[allow_edge]]` has an entry the baseline does not list. Adding an entry thus
needs a visible change to `baseline.toml`, and **a baseline change is a lead-reviewed change**.
The lists may only shrink: removing an entry passes, and the baseline follows only through the
explicit command (run it in the same change that removes the entry):

```sh
python3 tools/lints/features/features_lint.py --regenerate-baseline
```

## The allow list

`[[allow_edge]]` in `rules.toml` holds the edges that existed when the gate shipped (none now).
It may only shrink: an entry whose edge is gone fails the gate until it is removed. The fix for an
entry is to move the dependency to `[dev-dependencies]`, or to give the dependent crate its own
`testing` feature that forwards to the dependency's.
