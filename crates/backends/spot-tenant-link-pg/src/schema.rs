//! The tables: the tenant-schema set and the control registry, both created idempotently.
//!
//! # The tenant schema
//!
//! Every statement against these tables runs with the tenant's schema as the **only** entry of
//! `search_path` (set transaction-locally from a name derived from the tenant id, never from a
//! tenant-chosen string), so the SQL carries no schema name and a statement can only ever see the
//! tenant it was opened for.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_tenant_link` | one row per link: parties, local side, carrier, state, expiry, revocation, optimistic `version`, and whether the control registry is up to date |
//! | `spot_tenant_link_peer_key` | each party's **public** identity key (never a private key) |
//! | `spot_tenant_link_consent` | the consents, in order, with the step-up proof's method and instant |
//! | `spot_tenant_link_grant` | the agreed grants, in order, one per topic |
//! | `spot_tenant_link_offer` | each side's offer, kept so a later accept or grant is checked against both |
//! | `spot_tenant_link_audit_ref` | a reference from the link to each audit-chain record written for it |
//! | `spot_tenant_link_nonce` | the handshake nonces seen, so a replay is refused after a restart |
//!
//! # Upgrading the old names
//!
//! Earlier versions created the tables as `link`, `link_peer_key`, `link_consent`, `link_grant`,
//! `link_offer`, `link_audit_ref` and `link_nonce` (no `spot_` prefix, which `spot_pg_migrate`
//! refuses). [`prepare_tenant`] upgrades such a schema in **one transaction** under the advisory
//! lock: it checks every old/new pair first, then renames the tables, every constraint named after
//! them (primary keys, foreign keys, unique constraints, checks and, on PostgreSQL 18, the named
//! NOT NULL constraints), the live-pair unique index and the identity sequence of the audit
//! references. Nothing is dropped, truncated or recreated, so every row is kept byte for byte and
//! the one-live-link-per-pair unique index is in force at every instant.
//!
//! # The control registry
//!
//! `spot_control.link_registry(tenant_a, tenant_b, link_id, state)` records that a link exists,
//! who its two parties are (for routing) and whether it is live. It has no grant, topic, field,
//! key or consent column: the control plane never carries link data (ADR-058 rule 3).

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use tokio_postgres::Transaction;

use crate::error::{migration, migration_pool, migration_refused};

/// The control schema the registry lives in.
pub const CONTROL_SCHEMA: &str = "spot_control";

/// The columns of the registry table, in order. A test asserts the table has no others.
pub const REGISTRY_COLUMNS: [&str; 4] = ["tenant_a", "tenant_b", "link_id", "state"];

/// The tables of one tenant's schema, in creation order.
pub const TENANT_TABLES: [&str; 7] = [
    "spot_tenant_link",
    "spot_tenant_link_peer_key",
    "spot_tenant_link_consent",
    "spot_tenant_link_grant",
    "spot_tenant_link_offer",
    "spot_tenant_link_audit_ref",
    "spot_tenant_link_nonce",
];

const TENANT_LOCK: i64 = 0x5445_4e4c_4b01;
const REGISTRY_LOCK: i64 = 0x5445_4e4c_4b02;

/// The idempotent DDL for one tenant schema. Names are unqualified: it runs under the tenant's
/// `search_path`.
pub const TENANT_DDL: &str = "\
CREATE TABLE IF NOT EXISTS spot_tenant_link (\
    id uuid PRIMARY KEY, \
    party_a text NOT NULL, \
    party_b text NOT NULL, \
    local text NOT NULL, \
    carrier text NOT NULL CHECK (carrier IN ('direct', 'pull', 'external')), \
    state text NOT NULL \
        CHECK (state IN ('proposed', 'accepted_by_peer', 'active', 'revoked', 'expired')), \
    created bigint NOT NULL CHECK (created >= 0), \
    expires bigint CHECK (expires IS NULL OR expires > created), \
    revoked_by text, \
    revoke_reason text, \
    revoked_at bigint, \
    version bigint NOT NULL CHECK (version >= 1), \
    registry_pending boolean NOT NULL DEFAULT true, \
    CHECK (party_a <> party_b), \
    CHECK (local = party_a OR local = party_b)); \
CREATE UNIQUE INDEX IF NOT EXISTS spot_tenant_link_live_pair \
    ON spot_tenant_link (party_a, party_b) WHERE state NOT IN ('revoked', 'expired'); \
CREATE TABLE IF NOT EXISTS spot_tenant_link_peer_key (\
    link uuid NOT NULL REFERENCES spot_tenant_link (id), \
    party text NOT NULL, \
    public_key bytea NOT NULL CHECK (octet_length(public_key) = 32), \
    PRIMARY KEY (link, party)); \
CREATE TABLE IF NOT EXISTS spot_tenant_link_consent (\
    link uuid NOT NULL REFERENCES spot_tenant_link (id), \
    ord integer NOT NULL CHECK (ord >= 0), \
    party text NOT NULL, \
    actor text NOT NULL, \
    at bigint NOT NULL CHECK (at >= 0), \
    body jsonb NOT NULL, \
    PRIMARY KEY (link, ord), \
    UNIQUE (link, party), \
    UNIQUE (link, actor)); \
CREATE TABLE IF NOT EXISTS spot_tenant_link_grant (\
    link uuid NOT NULL REFERENCES spot_tenant_link (id), \
    ord integer NOT NULL CHECK (ord >= 0), \
    topic text NOT NULL, \
    direction text NOT NULL, \
    expires bigint NOT NULL CHECK (expires >= 0), \
    body jsonb NOT NULL, \
    PRIMARY KEY (link, ord), \
    UNIQUE (link, topic)); \
CREATE TABLE IF NOT EXISTS spot_tenant_link_offer (\
    link uuid NOT NULL REFERENCES spot_tenant_link (id), \
    side text NOT NULL, \
    body jsonb NOT NULL, \
    PRIMARY KEY (link, side)); \
CREATE TABLE IF NOT EXISTS spot_tenant_link_audit_ref (\
    link uuid NOT NULL, \
    seq bigint GENERATED ALWAYS AS IDENTITY, \
    kind text NOT NULL, \
    actor text NOT NULL, \
    at bigint NOT NULL, \
    revision bigint NOT NULL CHECK (revision >= 0), \
    chain_seq bigint NOT NULL CHECK (chain_seq >= 0), \
    PRIMARY KEY (link, seq)); \
CREATE TABLE IF NOT EXISTS spot_tenant_link_nonce (\
    link uuid NOT NULL, \
    nonce bytea NOT NULL CHECK (octet_length(nonce) = 16), \
    seen_at bigint NOT NULL CHECK (seen_at >= 0), \
    PRIMARY KEY (link, nonce));";

/// The idempotent DDL for the control registry.
pub const REGISTRY_DDL: &str = "\
CREATE SCHEMA IF NOT EXISTS spot_control; \
CREATE TABLE IF NOT EXISTS spot_control.link_registry (\
    tenant_a text NOT NULL, \
    tenant_b text NOT NULL, \
    link_id uuid PRIMARY KEY, \
    state text NOT NULL CHECK (state IN ('live', 'ended')), \
    CHECK (tenant_a <> tenant_b)); \
CREATE INDEX IF NOT EXISTS link_registry_a ON spot_control.link_registry (tenant_a); \
CREATE INDEX IF NOT EXISTS link_registry_b ON spot_control.link_registry (tenant_b);";

/// One table of this crate: its old unprefixed name, its new name, the columns (`name:type`) the
/// old table must have to be recognised as this crate's own, the standalone indexes (old name, new
/// name) and the identity sequences (old name, new name) it carries.
struct Table {
    old: &'static str,
    new: &'static str,
    columns: &'static [&'static str],
    indexes: &'static [(&'static str, &'static str)],
    sequences: &'static [(&'static str, &'static str)],
}

/// The crate's tables, parents first.
const TABLES: [Table; 7] = [
    Table {
        old: "link",
        new: "spot_tenant_link",
        columns: &[
            "id:uuid",
            "party_a:text",
            "party_b:text",
            "local:text",
            "carrier:text",
            "state:text",
            "created:bigint",
            "expires:bigint",
            "revoked_by:text",
            "revoke_reason:text",
            "revoked_at:bigint",
            "version:bigint",
            "registry_pending:boolean",
        ],
        indexes: &[("link_live_pair", "spot_tenant_link_live_pair")],
        sequences: &[],
    },
    Table {
        old: "link_peer_key",
        new: "spot_tenant_link_peer_key",
        columns: &["link:uuid", "party:text", "public_key:bytea"],
        indexes: &[],
        sequences: &[],
    },
    Table {
        old: "link_consent",
        new: "spot_tenant_link_consent",
        columns: &[
            "link:uuid",
            "ord:integer",
            "party:text",
            "actor:text",
            "at:bigint",
            "body:jsonb",
        ],
        indexes: &[],
        sequences: &[],
    },
    Table {
        old: "link_grant",
        new: "spot_tenant_link_grant",
        columns: &[
            "link:uuid",
            "ord:integer",
            "topic:text",
            "direction:text",
            "expires:bigint",
            "body:jsonb",
        ],
        indexes: &[],
        sequences: &[],
    },
    Table {
        old: "link_offer",
        new: "spot_tenant_link_offer",
        columns: &["link:uuid", "side:text", "body:jsonb"],
        indexes: &[],
        sequences: &[],
    },
    Table {
        old: "link_audit_ref",
        new: "spot_tenant_link_audit_ref",
        columns: &[
            "link:uuid",
            "seq:bigint",
            "kind:text",
            "actor:text",
            "at:bigint",
            "revision:bigint",
            "chain_seq:bigint",
        ],
        indexes: &[],
        sequences: &[(
            "link_audit_ref_seq_seq",
            "spot_tenant_link_audit_ref_seq_seq",
        )],
    },
    Table {
        old: "link_nonce",
        new: "spot_tenant_link_nonce",
        columns: &["link:uuid", "nonce:bytea", "seen_at:bigint"],
        indexes: &[],
        sequences: &[],
    },
];

/// A constant identifier as a quoted one (the names above are fixed ASCII; the escape is defensive).
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The `relkind` of relation `name` in `schema`, or `None` when there is no such relation.
async fn relation_kind(
    tx: &Transaction<'_>,
    schema: &str,
    name: &str,
) -> Result<Option<String>, tokio_postgres::Error> {
    let row = tx
        .query_opt(
            "SELECT c.relkind::text FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &name],
        )
        .await?;
    Ok(row.map(|r| r.get(0)))
}

/// The names of the constraints of table `table` that start with `<table>_`, sorted.
async fn constraints_named_after(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let pattern = format!("{}\\_%", table.replace('_', "\\_"));
    let mut names: Vec<String> = tx
        .query(
            "SELECT k.conname::text FROM pg_constraint k \
             JOIN pg_class c ON c.oid = k.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND k.conname LIKE $3",
            &[&schema, &table, &pattern],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    names.sort();
    Ok(names)
}

/// Whether table `table` already has a constraint called `name`.
async fn has_constraint(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
    name: &str,
) -> Result<bool, tokio_postgres::Error> {
    let row = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint k \
             JOIN pg_class c ON c.oid = k.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 AND k.conname = $3)",
            &[&schema, &table, &name],
        )
        .await?;
    Ok(row.get(0))
}

/// Whether index `name` of `schema` is an index of table `table`.
async fn index_belongs_to(
    tx: &Transaction<'_>,
    schema: &str,
    name: &str,
    table: &str,
) -> Result<bool, tokio_postgres::Error> {
    let row = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_index i \
             JOIN pg_class ic ON ic.oid = i.indexrelid \
             JOIN pg_class tc ON tc.oid = i.indrelid \
             JOIN pg_namespace n ON n.oid = tc.relnamespace \
             WHERE n.nspname = $1 AND ic.relname = $2 AND tc.relname = $3)",
            &[&schema, &name, &table],
        )
        .await?;
    Ok(row.get(0))
}

/// The names of the sequences owned by (or the identity of) a column of table `table`, sorted.
async fn owned_sequences(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let mut names: Vec<String> = tx
        .query(
            "SELECT s.relname::text FROM pg_depend d \
             JOIN pg_class s ON s.oid = d.objid AND s.relkind = 'S' \
             JOIN pg_class t ON t.oid = d.refobjid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE d.classid = 'pg_class'::regclass AND d.refclassid = 'pg_class'::regclass \
               AND d.refobjsubid > 0 AND d.deptype IN ('a', 'i') \
               AND n.nspname = $1 AND t.relname = $2",
            &[&schema, &table],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    names.sort();
    Ok(names)
}

/// The sorted `name:type` list of the columns of `table` in `schema`.
async fn columns_of(
    tx: &Transaction<'_>,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, tokio_postgres::Error> {
    let mut columns: Vec<String> = tx
        .query(
            "SELECT column_name::text || ':' || data_type::text \
             FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    columns.sort();
    Ok(columns)
}

/// `new` for `old` when `name` starts with `old`.
fn renamed(name: &str, old: &str, new: &str) -> Option<String> {
    name.strip_prefix(old).map(|rest| format!("{new}{rest}"))
}

/// One planned rename of a table and what is named after it.
struct Upgrade {
    table: &'static Table,
    constraints: Vec<(String, String)>,
}

/// Checks every old/new pair and returns what to rename, touching nothing. Refuses when both
/// names of a pair exist, when an old-named table is not this crate's, or when a new name is
/// already taken.
async fn plan_upgrade(tx: &Transaction<'_>, schema: &str) -> AppResult<Vec<Upgrade>> {
    let mut plan = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    for table in &TABLES {
        let Some(kind) = relation_kind(tx, schema, table.old)
            .await
            .map_err(migration)?
        else {
            continue;
        };
        if relation_kind(tx, schema, table.new)
            .await
            .map_err(migration)?
            .is_some()
        {
            return Err(migration_refused(format!(
                "both `{}` and `{}` exist; nothing was renamed or dropped, resolve it by hand",
                table.old, table.new
            )));
        }
        let mut wanted: Vec<String> = table.columns.iter().map(|c| (*c).to_owned()).collect();
        wanted.sort();
        let ours =
            kind == "r" && columns_of(tx, schema, table.old).await.map_err(migration)? == wanted;
        if !ours {
            return Err(migration_refused(format!(
                "the table `{}` is not the table of this crate, so it was not renamed to `{}`",
                table.old, table.new
            )));
        }
        targets.push(table.new.to_owned());
        let mut constraints = Vec::new();
        for name in constraints_named_after(tx, schema, table.old)
            .await
            .map_err(migration)?
        {
            if let Some(new_name) = renamed(&name, table.old, table.new) {
                if has_constraint(tx, schema, table.old, &new_name)
                    .await
                    .map_err(migration)?
                {
                    return Err(migration_refused(format!(
                        "both `{name}` and `{new_name}` exist on `{}`; nothing was renamed",
                        table.old
                    )));
                }
                // A constraint-backed index takes the constraint's name, so a taken relation
                // name would fail half way: it is checked with the other targets below.
                targets.push(new_name.clone());
                constraints.push((name, new_name));
            }
        }
        for &(old_index, new_index) in table.indexes {
            if relation_kind(tx, schema, old_index)
                .await
                .map_err(migration)?
                .is_none()
            {
                // Dropped by hand: the DDL below creates it under the new name.
                continue;
            }
            if !index_belongs_to(tx, schema, old_index, table.old)
                .await
                .map_err(migration)?
            {
                return Err(migration_refused(format!(
                    "the index `{old_index}` is not an index of `{}`, so nothing was renamed",
                    table.old
                )));
            }
            targets.push(new_index.to_owned());
        }
        let owned = owned_sequences(tx, schema, table.old)
            .await
            .map_err(migration)?;
        let expected: Vec<&str> = table.sequences.iter().map(|&(old, _)| old).collect();
        if !owned
            .iter()
            .map(String::as_str)
            .eq(expected.iter().copied())
        {
            return Err(migration_refused(format!(
                "the sequences of `{}` are not the ones of this crate, so nothing was renamed",
                table.old
            )));
        }
        for &(_, new_sequence) in table.sequences {
            targets.push(new_sequence.to_owned());
        }
        plan.push(Upgrade { table, constraints });
    }
    for target in targets {
        // Tables, indexes and sequences share one name space in a schema.
        if relation_kind(tx, schema, &target)
            .await
            .map_err(migration)?
            .is_some()
        {
            return Err(migration_refused(format!(
                "the name `{target}` is already taken, so nothing was renamed"
            )));
        }
    }
    Ok(plan)
}

/// Renames the old tables, their constraints, indexes and sequences, keeping every row. Runs
/// inside `prepare_tenant`'s transaction under its lock, so a failure leaves the old tables
/// exactly as they were.
async fn rename_old_tables(tx: &Transaction<'_>) -> AppResult<()> {
    let schema: Option<String> = tx
        .query_one("SELECT current_schema()::text", &[])
        .await
        .map_err(migration)?
        .try_get(0)
        .map_err(migration)?;
    let Some(schema) = schema else {
        return Err(migration_refused(
            "the tenant schema does not exist, so no table was created".to_owned(),
        ));
    };
    let plan = plan_upgrade(tx, &schema).await?;
    let q = quote(&schema);
    for upgrade in plan {
        let table = upgrade.table;
        let statement =
            |sql: String| async move { tx.batch_execute(&sql).await.map_err(migration) };
        // Lock the table first: the rename waits for in-flight statements rather than racing them.
        statement(format!(
            "LOCK TABLE {q}.{} IN ACCESS EXCLUSIVE MODE",
            quote(table.old)
        ))
        .await?;
        statement(format!(
            "ALTER TABLE {q}.{} RENAME TO {}",
            quote(table.old),
            quote(table.new)
        ))
        .await?;
        for (old_name, new_name) in &upgrade.constraints {
            statement(format!(
                "ALTER TABLE {q}.{} RENAME CONSTRAINT {} TO {}",
                quote(table.new),
                quote(old_name),
                quote(new_name)
            ))
            .await?;
        }
        for &(old_index, new_index) in table.indexes {
            statement(format!(
                "ALTER INDEX {q}.{} RENAME TO {}",
                quote(old_index),
                quote(new_index)
            ))
            .await?;
        }
        for &(old_sequence, new_sequence) in table.sequences {
            statement(format!(
                "ALTER SEQUENCE {q}.{} RENAME TO {}",
                quote(old_sequence),
                quote(new_sequence)
            ))
            .await?;
        }
    }
    Ok(())
}

/// Creates the link tables in the tenant's schema if they are missing, upgrading a schema that
/// still holds the old unprefixed names in the same transaction (every row is kept). The schema
/// itself is the tenant's, created at provisioning; a missing schema is a failure, never created
/// here.
///
/// # Errors
/// `tenant_link_pg.migration_failed` when the database cannot apply the DDL, when both the old and
/// the new name of a table exist, or when an old-named table is not this crate's (nothing is
/// touched). The messages carry names and a SQLSTATE, never a row value.
pub(crate) async fn prepare_tenant(pool: &Pool, schema: &str) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(migration_pool)?;
    let tx = conn.transaction().await.map_err(migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&TENANT_LOCK])
        .await
        .map_err(migration)?;
    tx.query_one("SELECT set_config('search_path', $1, true)", &[&schema])
        .await
        .map_err(migration)?;
    rename_old_tables(&tx).await?;
    tx.batch_execute(TENANT_DDL).await.map_err(migration)?;
    tx.commit().await.map_err(migration)
}

/// Creates the control registry if it is missing.
///
/// # Errors
/// `tenant_link_pg.migration_failed` when the database cannot apply the DDL.
pub(crate) async fn prepare_registry(pool: &Pool) -> AppResult<()> {
    let mut conn = pool.get().await.map_err(migration_pool)?;
    let tx = conn.transaction().await.map_err(migration)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&REGISTRY_LOCK])
        .await
        .map_err(migration)?;
    tx.batch_execute(REGISTRY_DDL).await.map_err(migration)?;
    tx.commit().await.map_err(migration)
}
