# tools/docsite

Builds the Spotledger contract documentation site: a deterministic, offline
static site (plain HTML/CSS, no JavaScript) rendering the contracts in
`contracts/` — the native OpenAPI reference, the realtime protocol with the
message schemas injected, one overview page per contract, the rustdoc trees
of the documented crates, the ADRs and the per-contract changelogs.

## Commands

```sh
python tools/docsite/build.py --out out/docsite          # generate the site
python tools/docsite/build.py --out out/docsite --check  # …and verify it
```

`--out` may be anywhere; an existing build in that directory (marked by
`.spot-docsite`) is replaced on the next run, and a directory without the
marker is refused for safety. `--check` re-verifies every internal link and
anchor after generating and exits non-zero on any problem. `--skip-rustdoc`
reuses an existing `target/doc` rustdoc instead of running `cargo doc`
(what the tests do); `--commit`/`--root` exist for determinism and tests.

## Dependencies

`requirements.txt` pins the three third-party packages (PyYAML, Jinja2,
markdown-it-py). CI's stock Python runs no pip step, so `bootstrap.py`
probes the imports first and, when pins are missing, walks a fallback
ladder: create a throwaway venv under `RUNNER_TEMP`/`TMPDIR`, install the
pins there and re-exec the same command through it
(`SPOT_DOCSITE_REEXEC` guards against re-exec loops); if the venv route
fails, fall back to `pip install --user` into the current interpreter and
re-probe in place; if both fail, exit with every attempted path and the
captured pip output, so the CI log says why. Run the tests the same way
(`python -m unittest discover -s tools/docsite/tests`); the bootstrap
applies automatically.

The build never writes under `contracts/` or elsewhere in the repository;
its output is only the `--out` tree. Pages are written in a deterministic
order and contain no timestamps, so two builds of the same tree are
byte-identical.

Gotcha: a local `.git/info/exclude` pattern (`*.txt`) silently keeps
`requirements.txt` out of the tree — `git add -f` it after editing, and
check `git ls-files tools/docsite` before pushing.