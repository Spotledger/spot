Part of [Running Spot](../running-spot.md).

# TypeScript declarations: `spot types`

An app author writes TypeScript against documents. `spot types` writes the declarations of those
documents (the platform's, the app's and the site's own Custom Fields) so an editor and `tsc`
check a script against the fields that really exist.

```sh
spot types generate --site demo --app myapp          # writes ./types/spot.d.ts and tsconfig.spot.json
spot types check    --site demo --app myapp          # exit 0 when they are current, 2 when stale
```

Both commands read the site's **effective metadata** through the loader `spot serve` uses: the
apps the site has installed, with every recorded runtime change (Custom Fields) applied. They read
files under the configuration root and nothing else: no database connection is opened and no
document, secret or permission is read or written.

## `generate`

```
spot types generate --site S [--app A ...] [--doctype D ...] [--out DIR] [--json]
```

- **Selection.** `--app A` declares the DocTypes of app `A` and everything they reference (child
  tables and link targets, transitively). `--doctype D` (a storage name such as `sales_invoice`)
  does the same for one DocType. Both may be repeated and combined. With neither, every DocType of
  the site is declared. An app that is not installed on the site, or a DocType the site does not
  have, is a usage error (exit 1) and writes nothing.
- **Output.** `DIR/spot.d.ts` and `DIR/tsconfig.spot.json`. `DIR` defaults to `<app directory>/types`
  when exactly one `--app` is given and to `./types` otherwise.
- **Atomic and quiet.** Each file is written to a temporary file in the same directory and renamed
  over the target, so an editor or a parallel run never sees half a file. A file whose content is
  already right is not touched, its modification time included: the command prints `unchanged`.
  Otherwise it prints `created` or `updated`.
- **Names that cannot be declared.** A DocType that maps onto no valid TypeScript name, or two that
  map onto one interface (`a1` and `a_1` both become `A1`), are reported together, one line each,
  with exit 3 and nothing written.

Example output:

```
types/spot.d.ts: created (12 interfaces, sha256 3f9a0c1d22b7)
types/tsconfig.spot.json: created
```

With `--json`: `{"out": "types", "interfaces": 12, "fingerprint": "...", "files": [{"file": "spot.d.ts", "status": "created"}, ...]}`.

## The editor

Point your script project at the generated configuration with one line in its own `tsconfig.json`:

```json
{ "extends": "./types/tsconfig.spot.json" }
```

`tsconfig.spot.json` includes `spot.d.ts`, which exports one interface per DocType
(`import type { Project } from "./types/spot.d.ts"`) and a `spot` namespace with `getDoc`.
Decimals are strings, `Secret` fields are not declared (a script never receives their value) and
no declaration carries a default value.

## A new Custom Field

The loop:

1. add the Custom Field (through the customisation API; no restart is needed);
2. run `spot types generate --site S --app A`;
3. the editor sees the field.

The command holds no state: it reads the site's recorded changes afresh on every run, so the field
is in the output as soon as the API has recorded it. Property overrides and DocTypes created at
runtime are not yet part of that record (only Custom Fields are), so they do not appear until the
metadata service records them.

## `check` (CI)

```
spot types check --site S [same selection] [--out DIR] [--json]
```

Exit 0 when both files in `DIR` equal what `generate` would write. Exit 2 when either is missing or
stale, printing which one, the DocTypes whose declarations changed (names, not content) and the
command to run:

```
the types in types are stale:
  spot.d.ts differs; changed DocTypes: Project
run: spot types generate --site demo --app myapp --out types
```

`check` writes nothing. Run it in CI after committing the generated files so a metadata change
cannot land without its declarations.

## Guarantees and limits

- **Deterministic.** DocTypes are sorted by name and fields keep their definition order; the same
  metadata gives the same bytes on any machine.
- **User text is escaped.** Labels and descriptions of fields come from users. The generator closes
  any `*/` inside a comment and escapes string literals; a label with a newline or a backtick stays
  inside its comment, and `check` never reads such text as a DocType name.
- **Output path.** A symlinked output directory or file is refused (exit 2); nothing is written
  outside the chosen directory. An unwritable directory is a clean exit 3.
- **Dev loop only.** `--dev` refreshing the file live is a separate task (WP-DX-02.T2).

Exit codes: 0 done, 1 usage, 2 refused (stale in `check`, a symlink), 3 failed.
