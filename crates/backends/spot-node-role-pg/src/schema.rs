//! The DDL of the node role table.
//!
//! | Column | Holds |
//! | --- | --- |
//! | `site` | the site the role belongs to (primary key) |
//! | `role` | the role the site first started as, as text (`standalone`, `hub`, `edge`) |
//! | `hub_fingerprint` | the pinned hub fingerprint, when the role has one |
//! | `updated_at` | Unix seconds of the last write, given by the caller |

/// The table name, so a caller's "table exists" check does not repeat the literal.
pub const TABLE: &str = "spot_node_role_state";

/// One idempotent statement per object (here only the table).
pub const DDL: &str = "CREATE TABLE IF NOT EXISTS spot_node_role_state (site text PRIMARY KEY, role text NOT NULL, hub_fingerprint text, updated_at bigint NOT NULL)";
