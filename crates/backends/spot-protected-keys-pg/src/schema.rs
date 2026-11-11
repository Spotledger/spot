//! The DDL of the key store tables, their triggers and the role restriction.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_key_group` | one row per (tenant, group): the epoch head, moved only by compare-and-set |
//! | `spot_key_group_epoch` | one row per (tenant, group, epoch): the key **wrapped** under the tenant root key, the root key reference and the epoch it supersedes; retiring an epoch erases `wrapped` |
//! | `spot_key_node` | one row per (tenant, node): status and the node wrapping key **wrapped** under the tenant root key (erased at revocation) |
//! | `spot_key_hold` | the key currently released to a node per group, and its lease end |
//! | `spot_key_revocation` | per revoked node and group: the epoch in force at revocation and the epoch rotated to |
//! | `spot_key_event` | the append-only history: issue, rotate, release, refuse, revoke, retire, register, status |
//!
//! # Only wrapped forms
//!
//! No column can hold a plaintext key: the key columns are `wrapped bytea` with
//! `CHECK (octet_length(wrapped) >= 40)` (the AEAD nonce and tag alone are 40 bytes, so a bare
//! 256-bit key is refused by the database), and the API only accepts wrapped forms.
//!
//! # Tenancy
//!
//! `tenant` is the first column of every primary key and every foreign key; every statement
//! filters on it as parameter `$1` (see [`crate::sql`]).
//!
//! # Immutable history and keys
//!
//! * `spot_key_event`: triggers refuse `UPDATE`, `DELETE` and `TRUNCATE`, and
//!   `spot_key_restrict_role` leaves the application role with `SELECT` and `INSERT` only.
//! * `spot_key_group_epoch`: triggers refuse `DELETE` and `TRUNCATE`, and the only `UPDATE`
//!   allowed erases `wrapped` (retirement).
//!
//! # Indexes
//!
//! Every access is by primary key prefix: `(tenant, group_name, epoch)` for keys,
//! `(tenant, node)` for nodes, `(tenant, node, group_name)` for holds and revocations, and
//! `(tenant, seq)` for the history.

use tokio_postgres::Client;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x6b65_7973_7401;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`, `OR REPLACE` or guarded.
pub const DDL: &str = r"
CREATE TABLE IF NOT EXISTS spot_key_group (
    tenant        text NOT NULL CHECK (tenant <> ''),
    group_name    text NOT NULL CHECK (group_name <> ''),
    current_epoch bigint NOT NULL CHECK (current_epoch >= 1),
    PRIMARY KEY (tenant, group_name)
);

CREATE TABLE IF NOT EXISTS spot_key_group_epoch (
    tenant        text NOT NULL,
    group_name    text NOT NULL,
    epoch         bigint NOT NULL CHECK (epoch >= 1),
    supersedes    bigint,
    root_id       text NOT NULL CHECK (root_id <> ''),
    root_version  bigint NOT NULL CHECK (root_version >= 0),
    wrapped       bytea CHECK (wrapped IS NULL OR octet_length(wrapped) >= 40),
    retired_at_ms bigint,
    PRIMARY KEY (tenant, group_name, epoch),
    FOREIGN KEY (tenant, group_name) REFERENCES spot_key_group (tenant, group_name),
    CHECK ((wrapped IS NULL) = (retired_at_ms IS NOT NULL)),
    CHECK ((epoch = 1 AND supersedes IS NULL) OR supersedes = epoch - 1)
);

CREATE TABLE IF NOT EXISTS spot_key_node (
    tenant           text NOT NULL CHECK (tenant <> ''),
    node             text NOT NULL CHECK (node <> ''),
    status           text NOT NULL CHECK (status IN ('active', 'suspended', 'revoked')),
    root_id          text CHECK (root_id IS NULL OR root_id <> ''),
    root_version     bigint CHECK (root_version IS NULL OR root_version >= 0),
    wrapped          bytea CHECK (wrapped IS NULL OR octet_length(wrapped) >= 40),
    registered_at_ms bigint NOT NULL,
    revoked_at_ms    bigint,
    PRIMARY KEY (tenant, node),
    CHECK ((status = 'revoked') = (wrapped IS NULL)),
    CHECK ((status = 'revoked') = (revoked_at_ms IS NOT NULL)),
    CHECK ((wrapped IS NULL) = (root_id IS NULL) AND (wrapped IS NULL) = (root_version IS NULL))
);

CREATE TABLE IF NOT EXISTS spot_key_hold (
    tenant         text NOT NULL,
    node           text NOT NULL,
    group_name     text NOT NULL,
    epoch          bigint NOT NULL CHECK (epoch >= 1),
    lease_until_ms bigint NOT NULL,
    PRIMARY KEY (tenant, node, group_name),
    FOREIGN KEY (tenant, node) REFERENCES spot_key_node (tenant, node),
    FOREIGN KEY (tenant, group_name) REFERENCES spot_key_group (tenant, group_name)
);

CREATE TABLE IF NOT EXISTS spot_key_revocation (
    tenant         text NOT NULL,
    node           text NOT NULL,
    group_name     text NOT NULL,
    from_epoch     bigint NOT NULL CHECK (from_epoch >= 1),
    to_epoch       bigint NOT NULL,
    revoked_at_ms  bigint NOT NULL,
    PRIMARY KEY (tenant, node, group_name),
    FOREIGN KEY (tenant, node) REFERENCES spot_key_node (tenant, node),
    FOREIGN KEY (tenant, group_name) REFERENCES spot_key_group (tenant, group_name),
    CHECK (to_epoch = from_epoch + 1)
);

CREATE TABLE IF NOT EXISTS spot_key_event (
    tenant     text NOT NULL CHECK (tenant <> ''),
    seq        bigint GENERATED ALWAYS AS IDENTITY,
    kind       text NOT NULL
        CHECK (kind IN ('issue', 'rotate', 'release', 'refuse', 'revoke', 'retire', 'register', 'status')),
    group_name text,
    node       text,
    epoch      bigint,
    to_epoch   bigint,
    detail     text CHECK (detail IS NULL OR length(detail) <= 64),
    actor      text NOT NULL CHECK (actor <> '' AND length(actor) <= 128),
    at_ms      bigint NOT NULL,
    PRIMARY KEY (tenant, seq)
);

CREATE OR REPLACE FUNCTION spot_key_event_immutable()
RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    RAISE EXCEPTION 'the key history is append-only';
END
$fn$;

DROP TRIGGER IF EXISTS spot_key_event_append_only ON spot_key_event;
CREATE TRIGGER spot_key_event_append_only
    BEFORE UPDATE OR DELETE ON spot_key_event
    FOR EACH ROW EXECUTE FUNCTION spot_key_event_immutable();

DROP TRIGGER IF EXISTS spot_key_event_no_truncate ON spot_key_event;
CREATE TRIGGER spot_key_event_no_truncate
    BEFORE TRUNCATE ON spot_key_event
    FOR EACH STATEMENT EXECUTE FUNCTION spot_key_event_immutable();

CREATE OR REPLACE FUNCTION spot_key_group_epoch_guard()
RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    IF TG_OP = 'UPDATE'
       AND OLD.wrapped IS NOT NULL AND NEW.wrapped IS NULL AND NEW.retired_at_ms IS NOT NULL
       AND NEW.tenant = OLD.tenant AND NEW.group_name = OLD.group_name AND NEW.epoch = OLD.epoch
       AND NEW.supersedes IS NOT DISTINCT FROM OLD.supersedes
       AND NEW.root_id = OLD.root_id AND NEW.root_version = OLD.root_version THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'wrapped group keys are append-only; an epoch can only be retired';
END
$fn$;

DROP TRIGGER IF EXISTS spot_key_group_epoch_append_only ON spot_key_group_epoch;
CREATE TRIGGER spot_key_group_epoch_append_only
    BEFORE UPDATE OR DELETE ON spot_key_group_epoch
    FOR EACH ROW EXECUTE FUNCTION spot_key_group_epoch_guard();

DROP TRIGGER IF EXISTS spot_key_group_epoch_no_truncate ON spot_key_group_epoch;
CREATE TRIGGER spot_key_group_epoch_no_truncate
    BEFORE TRUNCATE ON spot_key_group_epoch
    FOR EACH STATEMENT EXECUTE FUNCTION spot_key_group_epoch_guard();

CREATE OR REPLACE FUNCTION spot_key_restrict_role(role_name text)
RETURNS void LANGUAGE plpgsql AS $fn$
BEGIN
    EXECUTE format('REVOKE ALL ON spot_key_group, spot_key_group_epoch, spot_key_node, spot_key_hold, spot_key_revocation, spot_key_event FROM %I', role_name);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE ON spot_key_group, spot_key_node TO %I', role_name);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE ON spot_key_group_epoch TO %I', role_name);
    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON spot_key_hold TO %I', role_name);
    EXECUTE format('GRANT SELECT, INSERT ON spot_key_revocation, spot_key_event TO %I', role_name);
END
$fn$;
";

/// Creates the tables in one transaction under a transaction-scoped advisory lock.
///
/// # Errors
/// The driver's error; the caller maps it to a fixed sentence.
pub(crate) async fn prepare(client: &mut Client) -> Result<(), tokio_postgres::Error> {
    let tx = client.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
        .await?;
    tx.batch_execute(DDL).await?;
    tx.commit().await
}
