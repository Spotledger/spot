# l3-journal-sync — the journal/sync cluster test harness

The journal and sync engines (WP-JRN, WP-SYN) are proved as libraries against in-memory fakes.
What is left to prove is that they work **hosted**: two or three in-process hosts (the same
`spot_cli::start_with` assembly the `spot` binary's `serve` command runs — not separate OS
processes; see `src/node.rs`), each with its own PostgreSQL database, a journal folder and a
network between them that a test can cut and heal mid-batch. `harness/rust/l3-http` proves one
host and one site and is a shared registry of cases (24 briefs touch it); this crate is
deliberately **separate** so the tasks that add journal and sync cases do not queue behind that
crate's briefs (WP-RTB-01.T1).

```
harness/rust/l3-journal-sync/
├── src/cluster.rs       Cluster: a group of Node, built from one repository checkout
├── src/node.rs          Node: one in-process host, its database, root, journal folder and
│                        per-peer loopback proxy addresses; lifecycle (stop/kill/start/
│                        partition_from/heal); NodeSpec, Role and role_fragment (the one place
│                        a node's role becomes TOML)
├── src/proxy.rs         Proxy: one loopback TCP proxy; partition_from/heal pause and resume it
├── src/flaky_store.rs   FlakyStore<S>: a JournalStore wrapper that fails on command
├── src/tamper.rs        tamper::flip_byte / truncate / delete_object: byte-level corruption
├── src/client.rs        a small typed HTTP client (trimmed from l3-http's; no dependency on it)
├── src/helpers.rs        wait_until, read_conditions, read_audit, fingerprint
├── src/seed.rs          seed::documents: creates sample-app documents through the API
└── tests/harness_self.rs the harness proves itself; no journal or sync behaviour is exercised
```

## The model, in one paragraph

[`Cluster::new`] starts empty; [`Cluster::add_node`] starts one **in-process** host from a
[`NodeSpec`] (name, [`Role`], an `extra_config_toml` string a later task may need) over a fresh
`spot_postgres::testing::TestDb` scratch database, a temporary root and journal folder, and a
loopback API address — exactly `harness/rust/l3-http`'s fixture model (`TestDb`, a temporary
root, `spot_cli::start_with` on `127.0.0.1:0`, a drop guard, `L3_KEEP=1`), but for *several*
nodes at once instead of one site. Each node's sync side has no real listener yet (no sync
engine exists on `dev` to host): a plain echo loop stands in behind a per-peer loopback TCP
proxy, created on first use ([`Node::sync_addr_for`]). [`Node::partition_from`] pauses only the
proxy pair between the two named nodes — a third node's traffic with either of them is
untouched — and [`Node::heal`] resumes that pair (bytes already in flight sit unread while
paused, so a cut can land mid-batch). [`Node::stop`] signals the host to drain and *waits* for
it: by the time it returns, the old address is genuinely refused. [`Node::kill`] is an
**un-awaited stop**, not a forceful abort: `spot_cli::Running`/`ServerHandle` expose no abort
primitive, so the host drains the same way, only in the background and unawaited by the
caller — see [`Node::kill`]'s own doc comment for the one real, provable difference this leaves
between it and [`Node::stop`], and `## Open questions` in this task's PR for what a true crash
model (an abort primitive, or real OS-process nodes) would need. [`Node::start`] brings the same
node back over the same database and root. Dropping the `Cluster` (or a `Node`) removes its
database and root; `L3_KEEP=1` keeps them, printed, exactly like `l3-http`.

## Roles today (read before using `Role::Hub` / `Role::Edge`)

[`role_fragment`] is the one function that turns a [`NodeSpec`]'s role (and its
`extra_config_toml`) into TOML, so a later task that gives roles real meaning changes this one
function. **Today that fragment is not live configuration**: `spot_cli::config::Common` and
`Site` both reject unknown TOML keys (`#[serde(deny_unknown_fields)]`), and neither declares a
`[node]` table, so splicing the fragment into `common.toml` or `site.toml` would refuse every
node's start before any role exists to read it. [`Node::create`] therefore writes the fragment's
text to a side file, `<root>/role.toml`, which `spot_cli::config::load` never reads. The text
`role_fragment` returns is the real, uncommented fragment a later task's own config change can
splice in once that schema exists; only the *write site* is a placeholder — see `## Open
questions` in this task's PR.

## Fault injection

* **`FlakyStore<S: JournalStore>`** — wraps any store and fails deterministically: the next *n*
  calls, every call whose key starts with a given prefix, a `put` past a byte limit, or a seeded
  "fail roughly 1 in *k*" rate (`fail_rate`, driven by a fixed splitmix64 step over the seed and
  the call's sequence number — never the wall clock or an OS random source). Latency is a timer
  wait, also never a clock read.
* **`tamper::flip_byte(path, offset)`** flips one byte (XOR `0xFF`, so it is always different),
  **`tamper::truncate(path, len)`** shortens a file, **`tamper::delete_object(root, key)`**
  removes an object bypassing `JournalStore::delete`'s guard (it can remove a segment or a
  snapshot, which the guarded method refuses) — all three work directly on the files
  `spot_journal_store::FolderStore` writes.

## Read-only helpers

* **`wait_until(timeout, cond)`** polls every 50 ms with a clear timeout message.
* **`read_conditions(node)`** and **`read_audit(node, topic)`** read a node's own database
  through `row_to_json`, so this crate never has to know a later task's column types; before a
  later task creates `spot_conditions` or `spot_audit_log`, both return an empty list (read as
  `undefined_table`, SQLSTATE `42P01`), not an error.
* **`fingerprint(node)`** hashes every table in `public`'s rows (each table's rows sorted so
  row order never matters) with `spot_journal_store::etag`, so two nodes — or the same node
  before and after a change — compare equal exactly when their tables agree. It reads the
  database directly (not through the host), so it works on a stopped node too; call it with
  the node stopped for a comparison unaffected by the host's own background writes.
* **`seed::documents(node, n)`** creates `n` sample-app `project` documents as the node's
  Administrator and returns their ids.

## Adding a new area

A later task adds its own `tests/<area>.rs` next to `tests/harness_self.rs` (not inside it), so
one task's cases never queue behind another's compile or review. Use `Cluster`, `Node` and the
helpers above exactly as `tests/harness_self.rs` does; nothing here needs editing for a new area
to use them.

## Running it

```bash
SPOT_TEST_PG_URL=postgres://spot:***@127.0.0.1:5432/postgres \
cargo test -p l3-journal-sync --test harness_self -- --nocapture
```

Without `SPOT_TEST_PG_URL`, every test that needs a real host prints `SKIPPED: ...` and passes
vacuously (the repository's documented convention, the pipeline notes); the tests that
need no PostgreSQL (`role_fragment`, `FlakyStore`, `tamper`, the loopback proxy, `wait_until`)
always run. `SPOT_REQUIRE_PG=1` turns a missing database into a panic, exactly as
`spot_postgres::testing::TestDb::create` documents.

`L3_KEEP=1` keeps every node's database and root instead of removing them, printed per node.
