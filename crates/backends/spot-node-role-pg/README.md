# spot-node-role-pg

The PostgreSQL store of the node role a site first started as (WP-RTB-03.T5).

- **Table:** `spot_node_role_state`, one row per site: `site` (key), `role` (text), `hub_fingerprint` (nullable pin), `updated_at` (Unix seconds, given by the caller).
- **API:** `PgNodeRoleStore::new(pool)`, `read(site)`, `upsert(site, role, hub_fingerprint, updated_at_unix)`. The store does not decide whether a role change is allowed; the caller does.
- **Schema:** `schema::DDL` is carried by `spot-cli`'s `extra_tables()` and applied by `spot migrate`. The host refuses to start with the table missing (`node_role_pg.table_missing`).
- **Errors:** fixed text, codes `node_role_pg.unavailable` and `node_role_pg.table_missing`; no driver text, site or fingerprint.
- **`testing` feature:** `drop_table_for_tests` and `create_table_for_tests`. Off by default.
- **Tenancy:** the table lives in the site's own database, keyed by site; there is no cross-site read.

PG tests need `SPOT_TEST_PG_URL`; they skip (printing `SKIPPED:`) when it is unset, and fail if `SPOT_REQUIRE_PG=1`.
