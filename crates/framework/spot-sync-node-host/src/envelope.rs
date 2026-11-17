//! How a queued bundle travels as one protocol record.
//!
//! The hub's per-node queue numbers every bundle with a **serve sequence** (WP-SYN-11.T2). A
//! pulled batch carries one protocol [`Record`] per queued bundle: the record's `seq` is the
//! serve sequence (contiguous, so the batch framing and the wire's gap detection apply
//! unchanged), its `name` is `bundle/<serve seq>`, its `version` is 1, and its `payload` is the
//! JSON of an [`Envelope`]: the tenant the bundle belongs to, the serve sequence of the greatest
//! bundle the queue still held below it (`after`) and the closure [`Bundle`] itself.
//!
//! The edge acknowledges the **serve-sequence prefix** (`NodeIntake::served_prefix`) by asking
//! for `from_seq = prefix + 1` in its next pull, which is exactly the protocol's own checkpoint
//! rule; the hub trims its queue by it. This is the join the hub half (WP-RTB-02.T4) encodes
//! with [`encode`]; both halves use this one module so the two cannot drift.

use serde::{Deserialize, Serialize};
use spot_sync_closure::Bundle;
use spot_sync_node::ServeStamp;
use spot_sync_protocol::{Record, RecordName};

use crate::error::{Error, Result};

/// The envelope version this crate reads and writes.
pub const ENVELOPE_VERSION: u32 = 1;

/// The payload of one served record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// The envelope version; anything but [`ENVELOPE_VERSION`] is refused (held by the caller).
    pub v: u32,
    /// The tenant the bundle belongs to. A node applies only its own tenant's bundles.
    pub tenant: String,
    /// The serve sequence of the greatest bundle the hub's queue still held below this one when
    /// it served it, or 0.
    pub after: u64,
    /// The bundle: applied by the node in one transaction.
    pub bundle: Bundle,
}

/// One decoded served bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Served {
    /// The serve stamp (`seq` is the record's sequence).
    pub stamp: ServeStamp,
    /// The tenant tag.
    pub tenant: String,
    /// The bundle.
    pub bundle: Bundle,
}

/// The record name of the bundle at serve sequence `seq`.
pub fn record_name(seq: u64) -> String {
    format!("bundle/{seq}")
}

/// Builds the record that carries `bundle` at serve sequence `seq` (the hub's side of the join).
///
/// # Errors
///
/// `sync_node_host.envelope_invalid` when the bundle does not encode or exceeds the record
/// payload limit.
pub fn encode(seq: u64, tenant: &str, after: u64, bundle: &Bundle) -> Result<Record> {
    let envelope = Envelope {
        v: ENVELOPE_VERSION,
        tenant: tenant.to_owned(),
        after,
        bundle: bundle.clone(),
    };
    let payload = serde_json::to_string(&envelope)
        .map_err(|_| Error::envelope_invalid("a bundle did not encode"))?;
    let name = RecordName::new(&record_name(seq))
        .map_err(|_| Error::envelope_invalid("a record name did not build"))?;
    Record::new(name, 1, seq, payload).map_err(|error| Error::envelope_invalid(error.to_string()))
}

/// Reads a served record.
///
/// The record's own sequence is the stamp's `seq`; the envelope's `after` must be below it.
///
/// # Errors
///
/// `sync_node_host.envelope_invalid` for a payload that is not an envelope, a version this node
/// does not know, an `after` not below the sequence, or an empty tenant. The error names the
/// sequence only, never the payload.
pub fn decode(record: &Record) -> Result<Served> {
    let envelope: Envelope = serde_json::from_str(&record.payload).map_err(|_| {
        Error::envelope_invalid(format!("the record at {} is not an envelope", record.seq))
    })?;
    if envelope.v != ENVELOPE_VERSION {
        return Err(Error::envelope_invalid(format!(
            "the record at {} has envelope version {}, which this node does not read",
            record.seq, envelope.v
        )));
    }
    if envelope.tenant.is_empty() || envelope.after >= record.seq {
        return Err(Error::envelope_invalid(format!(
            "the record at {} has an impossible tenant or `after`",
            record.seq
        )));
    }
    Ok(Served {
        stamp: ServeStamp {
            seq: record.seq,
            after: envelope.after,
        },
        tenant: envelope.tenant,
        bundle: envelope.bundle,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_sync_closure::BundleKind;

    use super::*;

    fn bundle() -> Bundle {
        Bundle {
            kind: BundleKind::Root,
            records: Vec::new(),
            bytes: 0,
        }
    }

    #[test]
    fn a_bundle_round_trips_through_a_record() {
        let record = encode(7, "acme", 5, &bundle()).unwrap();
        assert_eq!(record.seq, 7);
        let served = decode(&record).unwrap();
        assert_eq!(served.stamp, ServeStamp { seq: 7, after: 5 });
        assert_eq!(served.tenant, "acme");
        assert_eq!(served.bundle, bundle());
    }

    #[test]
    fn a_foreign_version_or_a_bad_after_is_refused_without_the_payload() {
        let mut record = encode(3, "acme", 2, &bundle()).unwrap();
        record = Record::new(
            RecordName::new("bundle/3").unwrap(),
            1,
            3,
            record.payload.replace("\"v\":1", "\"v\":2"),
        )
        .unwrap();
        let error = decode(&record).unwrap_err();
        assert_eq!(error.code, crate::error::ENVELOPE_INVALID);
        assert!(!error.message.contains("acme"));
        let bad_after = encode(3, "acme", 3, &bundle()).unwrap();
        assert!(decode(&bad_after).is_err());
        let junk = Record::new(RecordName::new("x").unwrap(), 1, 1, "not json").unwrap();
        assert!(decode(&junk).is_err());
    }
}
