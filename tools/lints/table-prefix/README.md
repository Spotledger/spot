# tools/lints/table-prefix

Object-name gate for the backend crates: every table, index, constraint,
sequence, view, type and foreign-key reference a backend crate creates must
carry the `spot_` prefix. Run by the `gates` job of `.github/workflows/ci.yml`
(every `tools/lints/*/run.sh` is executed there) and recorded as the
`lint-table-prefix` gate in `tools/ci/matrix/gates.toml`.

```sh
bash tools/lints/table-prefix/run.sh                  # the gate (exit 0 clean, 1 finding, 2 unreadable input)
python -m unittest discover -s tools/lints/table-prefix/tests -v
```

## Why

`spot_pg_migrate` refuses, as a destructive change, every live table whose name
does not start with `spot_` and that the catalogue does not know
(`crates/backends/spot-pg-migrate/src/plan.rs`, `live.rs`; the whole rule is
described in that crate's own docs). The migrator reads one schema —
`current_schema()` — and treats anything unprefixed in it as an object that
belongs to somebody else, so it will not drop, rename or extend it.

Some backend crates (eight when the gate shipped) still create unprefixed objects. The first time a host runs
`spot migrate`, `install-app` or runtime customise against such a crate, the
migrator sees `sso_replay` (not `spot_sso_replay`) and refuses the plan, which
blocks every later migration of that site. The gate stops any further crate from
joining them; one small rename task per crate (listed in `allow/`) clears each one,
and the allow-list may only shrink.

Naming is also how ownership stays visible: ADR-45 keeps SQL inside the storage
crate that owns the tables, and every one of those crates writes into the same
PostgreSQL database. The `spot_` prefix is the one signal, visible from `psql`
alone, that says which crate owns a table, an index or a constraint — and which
objects the platform may manage.

## What it checks

Every `*.rs` file under `crates/backends/*/src/`, string literals only. Case
insensitive, after an optional quoted `schema.` qualifier and after stripping
double quotes:

| Statement | Name read |
| --- | --- |
| `CREATE [UNLOGGED] TABLE [IF NOT EXISTS] <name>` | table |
| `CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS] <name> [ON <table>]` | index |
| `CREATE SEQUENCE [IF NOT EXISTS] <name>` | sequence |
| `CREATE [OR REPLACE] [MATERIALIZED] VIEW [IF NOT EXISTS] <name>` | view |
| `CREATE TYPE <name>` | type |
| `[ADD] CONSTRAINT <name>` | constraint |
| `REFERENCES <name>` | foreign-key target |
| `ALTER TABLE <table> RENAME TO <new>` | the new table name |

Not read at all:

- **Comments**: `//`, `///`, `//!` and `/* … */` (nested blocks included). A doc
  comment that shows a bad `CREATE TABLE` is documentation, not DDL.
- **Test code**: `tests/`, `benches/` and `examples/` directories, and any
  `#[cfg(test)]` item inside `src/`. Test fixtures name scratch tables freely,
  and a `#[cfg(test)]` assertion string never reaches a database. This is why
  `spot-postgres/src/schema/ddl.rs`'s `mod tests` (which asserts on a generated
  `CREATE TABLE "party"`) is not a finding.
- **`CREATE TEMP TABLE` / `CREATE TEMPORARY TABLE`**: session-scratch tables are
  not part of the site schema, so they never reach the migrator.
- **`DROP CONSTRAINT <name>`**: it removes an object instead of creating one.

A `CONSTRAINT` or `REFERENCES` token only counts inside a `CREATE TABLE` /
`ALTER TABLE` statement, so an English sentence in a literal — "the constraint
violation" — is not read as DDL.

Exit codes: `0` clean, `1` a finding or an allow-list problem, `2` an input that
cannot be read (unparseable TOML, unreadable file). The gate never reads the
network and uses only the standard library.

## How to read a finding

```
TABLE-PREFIX: crates/backends/spot-sso-pg/src/schema.rs:31: table `sso_replay` does not start with `spot_`
TABLE-PREFIX: crates/backends/spot-sso-pg/src/schema.rs:41: index `sso_replay_expires_idx` does not start with `spot_`
```

`file:line` is the line of the SQL text (a multi-line literal reports the line
the name sits on). Rename the object to `spot_<name>` — and every index,
constraint and foreign key that names it — then re-run the gate. When the last
finding of a crate is gone, delete that crate's allow file (the gate fails on a
stale entry, so it cannot be left behind).

The summary line names what the gate could not call either way:

```
TABLE-PREFIX: allowed (rename pending): 59 finding(s) in 8 crate(s): spot-audit-store (2), …
TABLE-PREFIX: clean: 97 prefixed, 59 allowed pending rename, 14 in a control schema, 19 unchecked, of 189 object name(s) in 223 file(s) under crates/backends/*/src/
```

## The allow-list

`allow/<crate>.toml`, one file per crate, three fields and nothing else:

```toml
crate = "spot-sso-pg"
reason = "Creates `sso_replay`, its `sso_replay_keys` constraint and the `sso_replay_expires_idx` index."
rename_task = "WP-IAM-05.T7"
```

A finding whose file is under a listed crate is *allowed* and counted, not
failed. The list only shrinks. The gate fails when:

- a finding is in a crate with no allow file;
- an allow file names a crate that has no findings (stale: the rename landed, so
  delete the file);
- an allow file is missing `rename_task` or `reason` (or leaves one blank), or
  carries an unknown field — a typo such as `rename_taks` must not pass;
- an allow file names a crate directory that does not exist under
  `crates/backends/`, or its `crate` does not match its file name.

A new allow file therefore needs a lead-approved brief: `tools/lints/table-prefix/`
is an owned path, and only a task that owns it may add an entry.

### Retiring an allow entry

1. Finish the rename in the crate (the `rename_task` in the file).
2. `bash tools/lints/table-prefix/run.sh` — the crate's name is gone from the
   `allowed (rename pending)` line.
3. Delete `allow/<crate>.toml`. Leaving it is a failure (`stale entry`).
4. Update the crate count in this README and in the merge-matrix fixtures if it
   changes what the gate prints.

## The `spot_control` exemption

Objects in the control schemas are exempt, because the site-database migrator
never looks at them: it reads `current_schema()` only
(`crates/backends/spot-pg-migrate/src/live.rs`). The exemption is one constant
in `table_prefix_lint.py`:

```python
CONTROL_SCHEMAS = ("spot_control", "spot_migration_control")
```

- `spot_control` is the tenant/control schema (`spot_control.tenant`,
  `spot_control.tenant_journal`, `spot_control.link_registry`).
- `spot_migration_control` is the migration-apply crate's own schema; the first
  clean scan found it, and it is exempt for the same reason.

An index is exempt when the table it is `ON` lives in one of those schemas, even
when the index name itself is unprefixed (`link_registry_a ON
spot_control.link_registry`). A schema placeholder (`CREATE TABLE
{}.spot_bi_probe`) leaves the object checked: an unknown schema with a prefixed
name satisfies the prefix rule anyway.

Anything else — a `bi.v1_party` in a `bi` schema, a bare `party` in the site
schema — must be prefixed.

## Unchecked names: format placeholders

A name that is a Rust format placeholder (`{TABLE}`, `{name}`, `{}`,
`{REGISTRY_TABLE}`) cannot be judged from the source, so it is counted as
*unchecked* and printed with its `file:line`; it is never a failure:

```
TABLE-PREFIX: unchecked: 19 object name(s) are format placeholders, not read:
TABLE-PREFIX: unchecked crates/backends/spot-postgres/src/sequences/mod.rs:150: table name `{TABLE}`
```

The count exists so a reviewer can see what the gate could not read. A growing
number is not a gate failure, but it is a review signal: each entry is a name
that no automated check covers.

## A dynamic table name: expose it as a `const`

If a crate builds a statement with a runtime table name, the gate cannot read
it and will count it *unchecked*. Make the name a constant the gate can see:

```rust
/// The one table this crate creates.
pub const TABLE: &str = "spot_sequence";
```

and either interpolate the constant into the SQL text at compile time
(`concat!`, a literal `format!` argument naming the constant) or, better, keep
one literal `CREATE TABLE spot_sequence (…)` in the module and use `TABLE`
everywhere else. `{TABLE}` still counts as unchecked — a placeholder is never a
failure — but a reviewer can read the constant next to it, and a `const` whose
value starts with `spot_` documents the name in one place.

If the crate must build a name from parts (a per-entity table, a shard suffix),
prefix the format string literally: `"CREATE TABLE spot_{entity} (…)"` is read
as a prefixed name, because the literal text after `TABLE` starts with `spot_`.
`format!("CREATE TABLE {table}")` is not readable, so it lands in the unchecked
list.

## Files

| File | What it is |
| --- | --- |
| `table_prefix_lint.py` | The scanner: literal tokenizer, statement patterns, allow-list, exit codes. |
| `run.sh` | The gate entry point CI runs (`tools/lints/*/run.sh`). |
| `allow/<crate>.toml` | One allow entry per crate still to be renamed. |
| `tests/test_table_prefix_lint.py` | Unit tests over fixture trees (statement kinds, quoting, comments, placeholders, `tests/` skipped, allow-list failures, the real tree). |
