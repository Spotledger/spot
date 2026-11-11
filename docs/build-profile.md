# Dev and test build profile (WP-PIP-04.T2)

Debug info dominates the size of `target/debug`. The workspace `Cargo.toml` therefore sets, for `dev` and `test`:

```toml
[profile.dev]
debug = "line-tables-only"
[profile.dev.package."*"]
debug = false
[profile.test]
debug = "line-tables-only"
[profile.test.package."*"]
debug = false
```

`[profile.release]`, lints, members and dependencies are untouched. This is candidate B of the brief.

## What you keep and what you lose

- Kept: function names and `file:line` in panic backtraces (`RUST_BACKTRACE=1`), in test failures and in profilers.
- Lost: local variables and types in lldb/gdb, in our crates and in dependencies. To debug interactively, opt in locally with `CARGO_PROFILE_DEV_DEBUG=2` (and `CARGO_PROFILE_TEST_DEBUG=2`); this rebuilds into the same target dir, so prefer a separate `CARGO_TARGET_DIR` for debugging sessions.

## Measurements (Linux, 2026-10-21)

Tool: `python tools/ci/measure_profile.py --profile-set <label> [--env K=V]`. Crates `spot-errors,spot-expr` (a leaf crate and a crate with `proptest`, `serde`, `time`, `rust_decimal`), fresh throwaway target dir, `-j 3`, deleted afterwards. Toolchain 1.98.1.

| profile set | target/debug MiB | vs baseline | clean build s | incremental s | test --no-run s |
| --- | ---: | ---: | ---: | ---: | ---: |
| baseline (debug = 2) | 972 | 100% | 112.6 | 13.6 | 157.7 |
| A: line-tables-only | 730 | 75% | 41.9 | 4.0 | 71.6 |
| B: A + `package."*"` debug = false | 574 | 59% | 96.5 | 8.6 | 99.4 |
| C: B + `split-debuginfo = "unpacked"` | 576 (14 in `.dwo`) | 59% | 59.0 | 3.6 | 37.7 |

Reading the numbers:

- **Sizes are reliable; the times are not.** Seven implementers were building on the same machine during the runs, so wall times vary by a factor of two between runs of the same work (the baseline ran first under the heaviest load). The only safe time conclusion is that no candidate is slower than the baseline by anywhere near the 10% limit; do not quote the table as a speed-up.
- B saves 41% on this pair. The saving grows with the number of third-party crates in the dependency graph, so the whole-workspace saving is expected to be larger (the pair has a small dependency tree). The first agent to run `measure_profile.py` on a larger crate set should add a row here.
- C adds nothing in size on this pair (the `.dwo` files are tiny once only line tables remain) and cannot be set on Windows (MSVC uses PDB), so it is not adopted. D (`opt-level = 1` for dependencies) was not measured and is not adopted.

## Regression checks

| check | result |
| --- | --- |
| Panic backtrace from a deliberate failing test in a throwaway crate (`RUST_BACKTRACE=1`, profile B) shows function names and file:line | pass (`bt::boom at ./src/lib.rs:2:37`, `bt::fails at ./src/lib.rs:4:14`) |
| `cargo test -p spot-errors -p spot-expr` under profile B | pass (33 tests, plus doc tests) |
| `cargo build --workspace --all-targets`, `cargo test --workspace --no-run`, `cargo clippy --workspace --all-targets -- -D warnings` | run by CI on the PR (not run locally: a workspace build is tens of GB and the disk was short) |
| Windows (`rust` job on the Windows runner) | required green on the PR before merge; no local Windows numbers yet |
| WASM spikes (`spk` crates) | the release and wasm profiles are unchanged; no spike test asserts on debug-info content in the profile tables touched (to be confirmed by the PR CI) |
| `cargo-llvm-cov` | coverage uses the `test` profile and instrumentation, not DWARF variable info; to be confirmed by the PR CI if a coverage job exists |

## Adopting it

1. After this merges, every existing build in a shared target dir rebuilds once: the profile changes the fingerprint of every unit, so old `debug` artifacts become dead weight rather than being reused.
2. When nothing is building, the supervisor wipes the old artifacts of the shared target dir once (never while a build holds the cargo lock).
3. With the profile each target dir is roughly 40% smaller, so give each agent its own `CARGO_TARGET_DIR` (for example `shared-target-<agent>`) instead of one shared dir. Builds then no longer serialise on cargo's directory lock; the price is a first build per agent, and the sum of the dirs must still fit on disk, so wipe an idle agent's dir when disk is short.
