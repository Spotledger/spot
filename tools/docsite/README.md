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

## More pages (WP-DX-04.T2)

`extras.py` adds, to the same build and under the same determinism and `--check` rules:

- **Error catalogue** (`errors/`): every `[[entry]]` of
  `crates/framework/spot-errors/catalogue/*.toml`, one index page grouped by prefix and one page per
  prefix with the entry's summary, remedy, parameters, class, since and owner file. A code defined in
  two files fails the build. The native API codes that no catalogue file defines are listed on the
  index page; `known_gaps.toml` holds today's number, and `--check` fails only when it grows.
- **Contracts beyond the first four** (`<name>.html`): every directory of `contracts/` with a
  `VERSION` file. `ui-ext` gets its schemas as property tables (with `$ref`s resolved to links; a ref
  that does not resolve fails), its examples (each checked to be well-formed JSON; the full
  validation is `contracts/ui-ext/check.py` in its own job), its errors and changelog. The other
  contracts get an overview with their version, README and operations table.
- **Connector WIT** (`wit/connector.html`): an outline read by `wit.py` (a deliberately small reader:
  it fails the build on a construct it cannot read instead of skipping it) and the file verbatim.
  The SDK template matrix of `sdk/templates/MATRIX.md` is added when that file exists.
- **Formats** (`formats/`): the app manifest format (`FORMAT.md`) and the DocType field kinds, read
  from the `FieldType` enum of `crates/framework/spot-meta/src/definition.rs`.
- **Running Spot** (`guides/`): `docs/running-spot.md` and `docs/running-spot/*.md`
  (and the project process notes when present), one page each. Links between guides are
  rewritten to the rendered pages; a link to a file that does not exist or to a markdown page the
  site does not render fails `--check`.
- **`index.json`**: a flat, sorted list of `{title, path, section}` for the in-product help.

### Adding a contract

A directory under `contracts/` with a `VERSION` file must have a page that shows `v<VERSION>`;
`--check` fails with the directory name when it has not. A contract with an `openapi.yaml` or a
`README.md` gets a generic page automatically. A contract that needs more than that (a schema
reference, an examples corpus) adds a section to `extras.py`, as `ui-ext` did.
