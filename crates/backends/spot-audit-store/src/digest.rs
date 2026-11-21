//! The human review digest: what a person reads to review a range of the audit chain.
//!
//! [`digest`] counts a range's entries by kind and by actor class, lists every refusal and every
//! entry the producer did **not** mark masked, and carries the range's verification status. It
//! renders as plain text ([`Digest::to_text`]) and as JSON ([`Digest::to_json`]) for the support
//! console.

use std::collections::BTreeMap;

use spot_audit_chain::{AuditStore, PayloadValue, Verifier};

use crate::error::{Error, Result};

/// One entry the digest calls out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigestEntry {
    /// The entry's sequence number.
    pub seq: u64,
    /// The record kind.
    pub kind: String,
    /// The actor.
    pub actor: String,
    /// The decision code, for a decision record.
    pub code: Option<String>,
}

/// Whether the range verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyStatus {
    /// The range and its checkpoint verified.
    Ok,
    /// The chain is broken at this `seq`.
    Broken {
        /// The sequence number at fault.
        seq: u64,
    },
}

impl VerifyStatus {
    /// A short name for rendering.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            VerifyStatus::Ok => "verified".to_owned(),
            VerifyStatus::Broken { seq } => format!("broken at seq {seq}"),
        }
    }
}

/// A review digest of one range of one site's chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    /// The site.
    pub site: String,
    /// First sequence number included.
    pub from: u64,
    /// Last sequence number included.
    pub to: u64,
    /// Entries in the range.
    pub total: u64,
    /// Entries per record kind.
    pub by_kind: BTreeMap<String, u64>,
    /// Entries per actor class (`system`, `user`, `agent`).
    pub by_actor_class: BTreeMap<String, u64>,
    /// Every refused decision in the range.
    pub refusals: Vec<DigestEntry>,
    /// Every entry the producer did not mark masked.
    pub unmasked: Vec<DigestEntry>,
    /// Whether the range verified.
    pub verify: VerifyStatus,
}

impl Digest {
    /// The digest as plain text, for a person.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "Audit digest for `{}` seq {}..={}: {} entries ({})\n",
            self.site,
            self.from,
            self.to,
            self.total,
            self.verify.label()
        ));
        out.push_str("by kind:\n");
        for (kind, count) in &self.by_kind {
            out.push_str(&format!("  {kind}: {count}\n"));
        }
        out.push_str("by actor:\n");
        for (class, count) in &self.by_actor_class {
            out.push_str(&format!("  {class}: {count}\n"));
        }
        out.push_str(&format!("refusals: {}\n", self.refusals.len()));
        for entry in &self.refusals {
            out.push_str(&format!(
                "  seq {} {} {} {}\n",
                entry.seq,
                entry.kind,
                entry.actor,
                entry.code.as_deref().unwrap_or("-")
            ));
        }
        out.push_str(&format!("unmasked entries: {}\n", self.unmasked.len()));
        for entry in &self.unmasked {
            out.push_str(&format!(
                "  seq {} {} {}\n",
                entry.seq, entry.kind, entry.actor
            ));
        }
        out
    }

    /// The digest as JSON, for the support console.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "site": self.site,
            "from": self.from,
            "to": self.to,
            "total": self.total,
            "by_kind": self.by_kind,
            "by_actor_class": self.by_actor_class,
            "refusals": self.refusals.iter().map(entry_json).collect::<Vec<_>>(),
            "unmasked": self.unmasked.iter().map(entry_json).collect::<Vec<_>>(),
            "verify": match self.verify {
                VerifyStatus::Ok => serde_json::json!({"ok": true}),
                VerifyStatus::Broken { seq } => serde_json::json!({"ok": false, "seq": seq}),
            },
        })
    }
}

fn entry_json(entry: &DigestEntry) -> serde_json::Value {
    serde_json::json!({
        "seq": entry.seq,
        "kind": entry.kind,
        "actor": entry.actor,
        "code": entry.code,
    })
}

/// Builds the digest of `since..=until` for `site`, verifying the range with `verifier`.
///
/// # Errors
///
/// A store error.
pub async fn digest(
    store: &dyn AuditStore,
    verifier: &Verifier<'_>,
    site: &str,
    since: u64,
    until: u64,
) -> Result<Digest> {
    let entries = store.range(site, since, until).await.map_err(Error::from)?;
    let mut by_kind = BTreeMap::new();
    let mut by_actor_class = BTreeMap::new();
    let mut refusals = Vec::new();
    let mut unmasked = Vec::new();
    for entry in &entries {
        *by_kind.entry(entry.record.kind.clone()).or_insert(0) += 1;
        let class = entry
            .record
            .actor
            .split(':')
            .next()
            .unwrap_or("unknown")
            .to_owned();
        *by_actor_class.entry(class).or_insert(0) += 1;
        let code = decision_code(&entry.record.payload);
        if let Some(code) = &code
            && code != "agent.allow"
        {
            refusals.push(DigestEntry {
                seq: entry.seq,
                kind: entry.record.kind.clone(),
                actor: entry.record.actor.clone(),
                code: Some(code.clone()),
            });
        }
        if !entry.record.masked {
            unmasked.push(DigestEntry {
                seq: entry.seq,
                kind: entry.record.kind.clone(),
                actor: entry.record.actor.clone(),
                code,
            });
        }
    }
    let report = verifier
        .verify_range(store, site, since.max(1), until)
        .await
        .map_err(Error::from)?;
    let verify = report
        .failures
        .first()
        .map_or(VerifyStatus::Ok, |f| VerifyStatus::Broken { seq: f.seq });
    Ok(Digest {
        site: site.to_owned(),
        from: since,
        to: until,
        total: u64::try_from(entries.len()).unwrap_or(u64::MAX),
        by_kind,
        by_actor_class,
        refusals,
        unmasked,
        verify,
    })
}

/// The `decision` payload value of a decision record, if it has one.
fn decision_code(payload: &spot_audit_chain::Payload) -> Option<String> {
    match payload.get("decision") {
        Some(PayloadValue::Text(code)) => Some(code.clone()),
        _ => None,
    }
}
