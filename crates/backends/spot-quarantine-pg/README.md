# spot-quarantine-pg

The PostgreSQL `QuarantineStore` (WP-EDG-06.T4, ADR-062). See `src/lib.rs` for the tables and the
rules the store keeps.

## Constraints

`prepare()` adds the size and closure CHECK constraints `NOT VALID` (so an older table with older
rows still upgrades) and then runs `VALIDATE CONSTRAINT` on each, in the same transaction. After it
returns they are `convalidated`. It is idempotent. A table that holds a row breaking a constraint
makes `prepare()` fail with `quarantine_pg.migration_failed` (the message carries the SQLSTATE, not
the row); fix or remove the row, then start again.

## Roles: keep the intake away from the state column

Triggers stop a closed item changing and stop any change to an item's identity, but a database role
that can `UPDATE` the `state` column can still move an open item to `released` or `discarded`
without the audited, permission-checked path. Run the two kinds of work as two roles:

| Role | Used by | Grants on `spot_quarantine_item` | Grants on `spot_quarantine_payload` |
| --- | --- | --- | --- |
| feeder | the sync intake that only inserts and reads | `SELECT`, `INSERT` | `SELECT`, `INSERT` |
| operator | the admin operations and the retry schedule that close or retry items | `SELECT`, `INSERT`, `UPDATE (reason_code, detail, attempts, state, closed_by, closed_at)` | `SELECT`, `INSERT` |

Neither role needs `DELETE` or `TRUNCATE` (triggers refuse both anyway), and the feeder must not be
the table owner (an owner can always update every column). This crate never revokes anything from a
role it does not own: the grants are yours to apply.

`check_state_grant(&pool)` (or `check_state_grant_on(&client)`) asks the database whether the role
a connection runs as can update `state`. It returns a `StateGrant`: `is_all_clear()` is true for a
role that cannot, and `warning()` gives the one line a host prints at start-up for a role that can.
Run it with the pool the intake uses.

## Tests

The integration tests need a PostgreSQL server and skip cleanly when `SPOT_TEST_PG_URL` is unset.
