Part of [Running Spot](../running-spot.md).

# Sandboxes: `spot sandbox`

```
spot sandbox create --from SITE --name N [--ttl 14d]
spot sandbox list
spot sandbox destroy --name N
spot sandbox reap
```

`create` snapshots the source database with `pg_dump` (read only; the source is never changed),
restores it into a new database `sandbox_<name>` and a new site, then, before it says ready:

1. the new site keeps the source's apps and locale but **none of its credentials, API keys or
   aliases**;
2. every closed field is **masked in place**: fields tagged `pii`, `financial`, `secret`,
   `health`, in a protection group, or with no classification at all (fail closed). Text becomes a
   deterministic pseudonym under a key drawn for this sandbox and kept nowhere, so equal values
   stay equal (references hold) and nothing can be reversed. Secret fields are set to NULL.
   Numeric ledger amounts and links are kept. A closed field of a type that cannot be masked in
   place (a date, a number identifier) **fails the run**;
3. every framework table is **classified** (see "What a sandbox contains"): `Empty` and `Rebuild`
   tables are truncated, `Keep` tables are copied, and a table the registry does not know is
   emptied and reported as `unclassified_table`;
4. a **leak scan** reads a sample (5,000 values per column) of every text and JSON column and
   fails when any original value of a masked field survives anywhere;
5. the **effect hold** is placed on the new site (`spot migrate-hold`): mail, webhooks, bank calls
   and edge sync are held. Nothing here releases it;
6. a record `sites/<name>/sandbox.toml` stores the source, the expiry and a banner flag.

Any failure destroys the clone (database and folder). The snapshot of unmasked data is deleted in
every case. `destroy` and `reap` (everything past its expiry, and creations unfinished for an
hour) refuse a site without a sandbox record. Run `reap` from the scheduler or cron.

Exit codes: 0 done, 1 usage, 2 refused, 3 failed (the clone is gone). Output names tables and
columns, never a value.

## What a sandbox contains

A sandbox is a restore of production with the data masked. Masking works on the **classified
fields of documents**. The platform also keeps tables that are not documents (audit, keys, nonces,
queues, mail logs, link keys, coordination, sync identity, conditions, the migration control
schema). Copied into a clone they would be wrong (a clone claiming to be the production node) or
a leak. `crates/framework/spot-sandbox/src/tables.rs` classifies every one of them:

| Treatment | What the clone does | Examples |
| --- | --- | --- |
| `Keep` | copies it | `spot_meta`, `spot_sequence`, `spot_installer_*`, `spot_bpm_definition*`, `spot_sod_rule_sets` |
| `Empty` | truncates it (identity, secret, history, queue or per-node state) | `spot_audit_*`, `spot_key_*`, `spot_signed_command_nonce`, `spot_job*`, `spot_outbox*`, `spot_mail_sent_log`, `spot_link_*`, `spot_coord_*`, `spot_node_role_state`, `spot_migration_control.*`, `spot_bpm_*` instances |
| `Rebuild` | truncates it; the clone's own services rebuild it after activation | `spot_search_doc`, `spot_change`, `spot_feed_state`, `spot_graph_edge`, `spot_embedding*`, `spot_cache*` |

**The rule for a table that is not listed:** it is emptied in the clone (fail towards less data)
and the run says `unclassified_table` with its name, so the operator sees it. It never becomes
`Keep` by silence. A crate that adds a table must classify it in `tables.rs`: the test
`tests/tables_complete.rs` in `spot-sandbox` scans the repository for `CREATE TABLE` and fails,
naming the table and the file, when one is missing, or when a classified table is no longer
created. A `Keep` of a name containing `key`, `secret`, `token`, `nonce`, `audit`, `outbox` or
`mail` needs an `allow_keep_because` reason.

**Documents are different.** A table of a DocType in the site's metadata (child tables
included) is not in the registry: its fields are masked by classification, an unclassified field
is masked, and the leak scan fails the run if an original value survives anywhere, including a
`Keep` table. The clone knows which tables are documents (the metadata's definitions); everything
else in the schema is framework state.

**A clone is never a sync node.** `spot_node_role_state`, `spot_partition_*`, `spot_range_*` and
`spot_coord_*` are emptied, the clone's site has no `[partition]`, and the effect hold keeps edge
sync held. A node's role is read from `common.toml` of the process that serves it: do not point a
hub or edge process at a sandbox site.
