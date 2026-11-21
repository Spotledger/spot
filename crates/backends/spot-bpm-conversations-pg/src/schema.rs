//! The DDL of the conversation tables, and its indexes.
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_bpm_conversation` | one row per conversation: identity, version, the facts the indexes need (case, open, waiting) and the whole record as `body` |
//! | `spot_bpm_conversation_rejection` | the bounded memory of what a message was rejected with, per link |
//!
//! `body` is the source of truth a read decodes; the other columns are the same facts spelled
//! for the indexes, written from the same value in the same statement. A read checks that they
//! agree and refuses a row where they do not (a corrupt row is an error, never skipped).
//!
//! # Tenancy
//!
//! Both tables have `tenant` as the first column of their primary key, so two tenants can hold
//! the same link, conversation name and message id.
//!
//! # Indexes
//!
//! * primary key `(tenant, link_id, conversation)`: `load`, `save`, the idempotent `create`.
//! * `spot_bpm_conversation_case_idx (tenant, case_id)` partial on records with a case: `of_case`.
//! * `spot_bpm_conversation_open_idx (tenant, link_id)` partial on open records: `open_on`.
//! * `spot_bpm_conversation_waiting_idx (tenant, link_id, conversation)` partial on records with
//!   a wait: `waiting`.
//! * `spot_bpm_conversation_rejection` primary key `(tenant, link_id, message_id)`.

use tokio_postgres::Client;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x6270_6d63_6f01;

/// The largest record body: 1 MiB, measured as PostgreSQL measures it, in bytes of the text of
/// the stored jsonb (`octet_length(body::text)`). That text carries a space after every `:` and
/// `,`, so it is longer than the same body's compact JSON; the store measures the same way
/// (`codec::pg_text_len`), so a body at or under the cap is stored, one byte over is refused with
/// `TooLong`, and the table `CHECK` below agrees to the byte. `MAX_EVENTS` messages of ordinary
/// size fit with room; the `CHECK` also refuses an over-cap body should anything bypass the
/// store.
pub const MAX_BODY_BYTES: usize = 1_048_576;

/// The idempotent DDL. Every statement is `IF NOT EXISTS`.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_bpm_conversation (
    tenant       text NOT NULL CHECK (tenant <> ''),
    link_id      text NOT NULL CHECK (link_id <> ''),
    conversation text NOT NULL CHECK (conversation <> ''),
    version      bigint NOT NULL CHECK (version >= 1),
    case_id      text,
    open         boolean NOT NULL,
    waiting      boolean NOT NULL,
    body         jsonb NOT NULL CHECK (
        jsonb_typeof(body) = 'object' AND octet_length(body::text) <= 1048576
    ),
    PRIMARY KEY (tenant, link_id, conversation)
);

CREATE INDEX IF NOT EXISTS spot_bpm_conversation_case_idx
    ON spot_bpm_conversation (tenant, case_id) WHERE case_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS spot_bpm_conversation_open_idx
    ON spot_bpm_conversation (tenant, link_id) WHERE open;

CREATE INDEX IF NOT EXISTS spot_bpm_conversation_waiting_idx
    ON spot_bpm_conversation (tenant, link_id, conversation) WHERE waiting;

CREATE TABLE IF NOT EXISTS spot_bpm_conversation_rejection (
    tenant     text NOT NULL CHECK (tenant <> ''),
    link_id    text NOT NULL CHECK (link_id <> ''),
    message_id text NOT NULL CHECK (message_id <> ''),
    code       text NOT NULL CHECK (code <> ''),
    PRIMARY KEY (tenant, link_id, message_id)
);

CREATE INDEX IF NOT EXISTS spot_bpm_conversation_rejection_age_idx
    ON spot_bpm_conversation_rejection (tenant, message_id, link_id);
"#;

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
