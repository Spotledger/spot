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
3. tables that hold copies (`spot_search*`, `spot_embedding*`, `spot_cache*`,
   `spot_mail_sent_log`) are emptied;
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
