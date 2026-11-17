//! `ApprovalStore` over `spot_migration_approval` and `spot_migration_plan_author`.
//!
//! Approvals are append-only: nothing here updates or deletes a row, and the guard trigger
//! installed by `protect` refuses it for every role. A revoking decision is a new row.

use spot_migration_approval::{Approval, ApprovalError, ApprovalStore, Decision, PlanDigest};

use crate::store::{LIST_DEADLINE, PgMigrationStore, READ_DEADLINE, WRITE_DEADLINE};

const SELECT: &str = "SELECT plan_hash, approver, role, decision, comment, at, signature \
     FROM spot_migration_approval";

type Row = (String, String, String, String, String, i64, Option<String>);

/// The most approvals one read returns; more is a backend error, never a truncated answer.
const CEILING: i64 = 10_000;

fn backend(message: &str) -> ApprovalError {
    ApprovalError::Backend {
        message: message.to_owned(),
    }
}

fn failed<E>(_: spot_pg_bridge::BridgeError<E>) -> ApprovalError {
    backend("the approval store could not be read or written")
}

fn digest_of(hex: &str) -> Result<PlanDigest, ApprovalError> {
    let valid = hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid {
        return Err(backend("a stored plan digest is malformed"));
    }
    serde_json::from_value(serde_json::Value::String(hex.to_owned()))
        .map_err(|_| backend("a stored plan digest is malformed"))
}

fn decision_name(decision: Decision) -> &'static str {
    match decision {
        Decision::Approve => "approve",
        Decision::Reject => "reject",
    }
}

fn approval(row: Row) -> Result<Approval, ApprovalError> {
    let (hash, approver, role, decision, comment, at, signature) = row;
    Ok(Approval {
        plan_digest: digest_of(&hash)?,
        approver,
        role,
        decision: match decision.as_str() {
            "approve" => Decision::Approve,
            "reject" => Decision::Reject,
            _ => return Err(backend("a stored decision is unknown")),
        },
        comment,
        at,
        signature,
    })
}

impl PgMigrationStore {
    fn approvals_where(&self, digest: Option<String>) -> Result<Vec<Approval>, ApprovalError> {
        let rows: Vec<Row> = self
            .bridge
            .run(
                if digest.is_some() {
                    READ_DEADLINE
                } else {
                    LIST_DEADLINE
                },
                move |conn| {
                    Box::pin(async move {
                        let rows = match digest {
                            Some(hash) => {
                                conn.query(
                                    &format!("{SELECT} WHERE plan_hash = $1 ORDER BY seq LIMIT $2"),
                                    &[&hash, &(CEILING + 1)],
                                )
                                .await?
                            }
                            None => {
                                conn.query(
                                    &format!("{SELECT} ORDER BY seq LIMIT $1"),
                                    &[&(CEILING + 1)],
                                )
                                .await?
                            }
                        };
                        Ok::<_, tokio_postgres::Error>(
                            rows.iter()
                                .map(|r| {
                                    (
                                        r.get(0),
                                        r.get(1),
                                        r.get(2),
                                        r.get(3),
                                        r.get(4),
                                        r.get(5),
                                        r.get(6),
                                    )
                                })
                                .collect(),
                        )
                    })
                },
            )
            .map_err(failed)?;
        if i64::try_from(rows.len()).unwrap_or(i64::MAX) > CEILING {
            return Err(backend("too many approvals to read at once"));
        }
        rows.into_iter().map(approval).collect()
    }
}

impl ApprovalStore for PgMigrationStore {
    fn append(&self, approval: Approval) -> Result<(), ApprovalError> {
        let Approval {
            plan_digest,
            approver,
            role,
            decision,
            comment,
            at,
            signature,
        } = approval;
        let hash = plan_digest.as_hex().to_owned();
        let decision = decision_name(decision).to_owned();
        self.bridge
            .run(WRITE_DEADLINE, move |conn| {
                Box::pin(async move {
                    conn.execute(
                        "INSERT INTO spot_migration_approval \
                         (plan_hash, approver, role, decision, comment, at, signature) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                        &[
                            &hash, &approver, &role, &decision, &comment, &at, &signature,
                        ],
                    )
                    .await
                })
            })
            .map(|_| ())
            .map_err(failed)
    }

    fn for_digest(&self, digest: &PlanDigest) -> Result<Vec<Approval>, ApprovalError> {
        self.approvals_where(Some(digest.as_hex().to_owned()))
    }

    fn all(&self) -> Result<Vec<Approval>, ApprovalError> {
        self.approvals_where(None)
    }

    fn register_plan(&self, digest: &PlanDigest, author: &str) -> Result<(), ApprovalError> {
        let (hash, who) = (digest.as_hex().to_owned(), author.to_owned());
        let existing: Option<String> = self
            .bridge
            .run(WRITE_DEADLINE, move |conn| {
                Box::pin(async move {
                    let inserted = conn
                        .execute(
                            "INSERT INTO spot_migration_plan_author (plan_hash, author) \
                             VALUES ($1, $2) ON CONFLICT (plan_hash) DO NOTHING",
                            &[&hash, &who],
                        )
                        .await?;
                    if inserted == 1 {
                        return Ok::<_, tokio_postgres::Error>(None);
                    }
                    let row = conn
                        .query_one(
                            "SELECT author FROM spot_migration_plan_author WHERE plan_hash = $1",
                            &[&hash],
                        )
                        .await?;
                    Ok(Some(row.get::<_, String>(0)))
                })
            })
            .map_err(failed)?;
        match existing {
            None => Ok(()),
            Some(found) if found == author => Ok(()),
            Some(_) => Err(ApprovalError::AuthorConflict),
        }
    }

    fn author_of(&self, digest: &PlanDigest) -> Result<Option<String>, ApprovalError> {
        let hash = digest.as_hex().to_owned();
        self.bridge
            .run(READ_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn
                        .query_opt(
                            "SELECT author FROM spot_migration_plan_author WHERE plan_hash = $1",
                            &[&hash],
                        )
                        .await?;
                    Ok::<_, tokio_postgres::Error>(row.map(|r| r.get::<_, String>(0)))
                })
            })
            .map_err(failed)
    }
}
