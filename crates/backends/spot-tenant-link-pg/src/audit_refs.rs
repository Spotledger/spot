//! Audit references: the link side of the audit trail.
//!
//! The audit chain (`spot-audit-chain`) is the tamper-evident record of what happened to a link.
//! This crate does not write the chain itself; the host supplies an [`AuditSink`] over it. What
//! the tenant's own schema keeps is a **reference** to each chain record written for a link: the
//! kind, actor, instant, revision and the chain sequence number the sink returned. That is what
//! "show audit" lists, and what ties a stored link to its trail without copying the trail.
//!
//! [`RefRecordingSink`] wraps the host's sink: it appends to the chain first and records the
//! reference second, so a reference never points at a record that was not written, and a
//! failure to record the reference is an audit failure (the caller refuses the change).

use spot_audit_chain::{AuditRecord, PayloadValue};
use spot_tenant_link::{AuditSink, Error, LinkId, Result};
use uuid::Uuid;

use crate::codec::{to_i64, to_u64};
use crate::error::query_failed;
use crate::store::PgLinkStore;

/// A reference from a link to one audit-chain record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRef {
    /// The dotted audit kind, such as `tenant_link.revoked`.
    pub kind: String,
    /// `system`, `user:<id>` or `agent:<id>`.
    pub actor: String,
    /// The instant the record carries.
    pub at: u64,
    /// The handshake revision or stored version the event belongs to.
    pub revision: u64,
    /// The chain sequence number the sink returned.
    pub chain_seq: u64,
}

impl PgLinkStore {
    /// Records the reference to one chain record written for `link`.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    pub async fn record_audit_ref(&self, link: &LinkId, reference: &AuditRef) -> Result<()> {
        self.insert_audit_ref(link.as_uuid(), reference).await
    }

    /// Records the reference to a chain record that names no link: a denied `list`, which asks
    /// about none. It is kept under the nil id and read back by
    /// [`denied_audit_refs`](Self::denied_audit_refs).
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    pub(crate) async fn record_unscoped_audit_ref(&self, reference: &AuditRef) -> Result<()> {
        self.insert_audit_ref(Uuid::nil(), reference).await
    }

    /// The references to calls that were refused without naming a link (a denied `list`), in the
    /// order they were written.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    pub async fn denied_audit_refs(&self) -> Result<Vec<AuditRef>> {
        self.read_audit_refs(Uuid::nil()).await
    }

    async fn insert_audit_ref(&self, link: Uuid, reference: &AuditRef) -> Result<()> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, false).await?;
        tx.execute(
            "INSERT INTO spot_tenant_link_audit_ref (link, kind, actor, at, revision, chain_seq) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &link,
                &reference.kind,
                &reference.actor,
                &to_i64(reference.at)?,
                &to_i64(reference.revision)?,
                &to_i64(reference.chain_seq)?,
            ],
        )
        .await
        .map_err(query_failed)?;
        tx.commit().await.map_err(query_failed)
    }

    /// The references recorded for `link`, in the order they were written.
    ///
    /// # Errors
    /// [`Error::StoreFailed`].
    pub async fn audit_refs(&self, link: &LinkId) -> Result<Vec<AuditRef>> {
        self.read_audit_refs(link.as_uuid()).await
    }

    async fn read_audit_refs(&self, link: Uuid) -> Result<Vec<AuditRef>> {
        let mut conn = self.conn().await?;
        let tx = self.scoped(&mut conn, true).await?;
        let rows = tx
            .query(
                "SELECT kind, actor, at, revision, chain_seq FROM spot_tenant_link_audit_ref \
                 WHERE link = $1 ORDER BY seq",
                &[&link],
            )
            .await
            .map_err(query_failed)?;
        rows.iter()
            .map(|row| {
                Ok(AuditRef {
                    kind: row.get(0),
                    actor: row.get(1),
                    at: to_u64(row.get(2))?,
                    revision: to_u64(row.get(3))?,
                    chain_seq: to_u64(row.get(4))?,
                })
            })
            .collect()
    }
}

/// An [`AuditSink`] that appends to the host's chain and then records the reference in the
/// tenant's schema.
pub struct RefRecordingSink<'a> {
    inner: &'a dyn AuditSink,
    store: &'a PgLinkStore,
}

impl<'a> RefRecordingSink<'a> {
    /// Wraps `inner`, recording references in `store`.
    #[must_use]
    pub fn new(inner: &'a dyn AuditSink, store: &'a PgLinkStore) -> Self {
        Self { inner, store }
    }
}

impl RefRecordingSink<'_> {
    /// Appends a record that names no link to the host's chain and keeps its reference under
    /// the nil id (see [`PgLinkStore::denied_audit_refs`]).
    pub(crate) fn append_unscoped(&self, record: AuditRecord) -> Result<u64> {
        let audit_failed = || Error::AuditFailed {
            action: "record an audit reference",
        };
        let reference = AuditRef {
            kind: record.kind.clone(),
            actor: record.actor.clone(),
            at: u64::try_from(record.at).map_err(|_| audit_failed())?,
            revision: 0,
            chain_seq: 0,
        };
        let chain_seq = self.inner.append(record)?;
        let reference = AuditRef {
            chain_seq,
            ..reference
        };
        crate::blocking::block_on(
            &self.store.runtime,
            self.store.record_unscoped_audit_ref(&reference),
        )
        .map_err(|_: Error| audit_failed())?;
        Ok(chain_seq)
    }
}

impl AuditSink for RefRecordingSink<'_> {
    fn append(&self, record: AuditRecord) -> Result<u64> {
        let audit_failed = || Error::AuditFailed {
            action: "record an audit reference",
        };
        let link = record
            .subject
            .as_ref()
            .filter(|subject| subject.doctype == "tenant_link")
            .and_then(|subject| subject.id.parse::<LinkId>().ok())
            .ok_or_else(audit_failed)?;
        let revision = match record.payload.get("revision") {
            Some(PayloadValue::Int(value)) => u64::try_from(*value).unwrap_or(0),
            _ => 0,
        };
        let reference = AuditRef {
            kind: record.kind.clone(),
            actor: record.actor.clone(),
            at: u64::try_from(record.at).map_err(|_| audit_failed())?,
            revision,
            chain_seq: 0,
        };
        let chain_seq = self.inner.append(record)?;
        let reference = AuditRef {
            chain_seq,
            ..reference
        };
        crate::blocking::block_on(
            &self.store.runtime,
            self.store.record_audit_ref(&link, &reference),
        )
        .map_err(|_: Error| audit_failed())?;
        Ok(chain_seq)
    }
}
