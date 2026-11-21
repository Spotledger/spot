# spot-scaffold

Scaffolding for Spotledger apps (WP-DX-01.T1): `app new`, `doctype add`, `controller add` and
`script add` generate a working starting point from templates, and — more important — **prove
every generated file is valid**. The test suite generates 24 argument combinations into
temporary directories and then runs `spot-pack`'s `check` and `pack`, loads the DocTypes
through `spot-meta` into a catalogue, loads the permission files with `spot-permissions`,
registers a generated controller against a `spot-controllers` registry and transpiles a
generated script with `spot-ts-transpile`. A template that rots when a format changes fails
the build.

```text
spot-scaffold app new acme [--with-controller] [--with-script] [--example-fields]
spot-scaffold doctype add acme "Sales Invoice" [--name sales_invoice] [--example-fields]
spot-scaffold controller add acme sales_invoice
spot-scaffold script add acme ping
```

Every leaf command takes `--dry-run`, `--force`, `--out <project>` and `--json`. Exit codes:
`0` ok, `2` usage, `3` refused.

An edit (`doctype add`, `controller add`, `script add`) rewrites the manifest and the
permission file, so it needs `--force`; a refusal writes nothing at all (the whole set is
validated before the first byte lands).

## Layout

`--out` names a **project** directory. The packable app lives in `<project>/app` — the exact
shape `spot-pack` reads (`manifest.toml`, `app.wasm`, `doctypes/`, `permissions/`,
`fixtures/`, `controllers.toml`, `scripts/`) — and the developer files (`README.md`,
`.gitignore`, `tests/`, and the optional native controller crate in `src/`) sit beside it.
That split is why a generated app packs: `spot-pack` accepts only the app directory's own
shape, and a README at the app root would be a `pack.layout` fault. Every app carries the
minimal `app.wasm` a bundle requires (`spot-bundle` refuses a bundle without a component).

## The safety layer

- `FileTree` is the only I/O (`write_to_disk`). It refuses an absolute path, `..`, a symlink on
  the path, an overwrite without `--force`, and a write that resolves outside `--out`.
- Names follow the storage rules (`[a-z][a-z0-9_]*`, at most 63 bytes) and refuse the Rust
  keywords, the SQL words the storage layer keeps, the platform prefixes `doc_`, `sys_`,
  `identity_` and `spot_`, and the reserved system and engine columns. A label such as
  `Sales Invoice` is derived to `sales_invoice`; a refusal names the rule and suggests a name.
- Edits to an existing app are additive and exact: the manifest is parsed through
  `spot-manifest`, appended to with `toml_edit` (only the table it touches), and a conflict
  names the file and the line.
- Generation is deterministic: no timestamp, username, machine path or random value reaches
  the output; the author has a documented default and the licence has none: the generated README says
  the licence is not chosen yet until `--licence <SPDX id>` is given.
