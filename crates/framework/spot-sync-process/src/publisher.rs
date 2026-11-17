//! The hub side: publish a version as hub-authored master data.

use std::collections::BTreeMap;

use crate::error::{EDGE_AUTHORED, Error, HASH_MISMATCH, Result};
use crate::record::ProcessRecord;

/// Where the hub emits master records (the hub stream; the relay routes by scope keys).
pub trait HubSink {
    /// Emits `record` as hub-authored master data requiring all of `scope_keys` (empty: every
    /// subscriber of process definitions). A node whose subscription does not cover them never
    /// receives it.
    ///
    /// # Errors
    ///
    /// `sync_process.store_failed`.
    fn emit(&mut self, record: &ProcessRecord, scope_keys: &[String]) -> Result<()>;
}

/// What a publish did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Published {
    /// Emitted.
    Emitted,
    /// The same version with the same hash was published before: nothing emitted.
    NoOp,
}

/// The hub publisher.
pub struct ProcessPublisher<S: HubSink> {
    sink: S,
    tenant: String,
    published: BTreeMap<(String, u64), String>,
}

impl<S: HubSink> ProcessPublisher<S> {
    /// A publisher for `tenant`.
    pub fn new(sink: S, tenant: impl Into<String>) -> Self {
        Self {
            sink,
            tenant: tenant.into(),
            published: BTreeMap::new(),
        }
    }

    /// The sink (for tests and the host).
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// Publishes a version.
    ///
    /// # Errors
    ///
    /// `sync_process.foreign_record` for another tenant, `sync_process.hash_mismatch` when the
    /// record's hash is not its payload's or a version already published has another hash (a
    /// changed definition under the same version is a defect), `sync_process.invalid_record`.
    pub fn publish(&mut self, record: &ProcessRecord, scope_keys: &[String]) -> Result<Published> {
        record.check_shape()?;
        if record.tenant != self.tenant {
            return Err(Error::new(
                crate::error::FOREIGN_RECORD,
                "the process belongs to another tenant",
            ));
        }
        if !record.hash_ok() {
            return Err(Error::new(
                HASH_MISMATCH,
                format!(
                    "the hash of {} does not match its definition",
                    record.name()
                ),
            ));
        }
        let key = (record.process.clone(), record.version);
        match self.published.get(&key) {
            Some(hash) if *hash == record.definition_hash => return Ok(Published::NoOp),
            Some(_) => {
                return Err(Error::new(
                    HASH_MISMATCH,
                    format!(
                        "{} was published with another definition; publish a new version",
                        record.name()
                    ),
                ));
            }
            None => {}
        }
        self.sink.emit(record, scope_keys)?;
        self.published.insert(key, record.definition_hash.clone());
        Ok(Published::Emitted)
    }

    /// Checks an inbound record name pushed by a node: `process:*` is hub-authored only.
    ///
    /// # Errors
    ///
    /// `sync_process.edge_authored` for a name that starts with `process:`.
    pub fn refuse_from_node(record_name: &str) -> Result<()> {
        if record_name.starts_with("process:") {
            Err(Error::new(
                EDGE_AUTHORED,
                "process definitions flow from the hub down only",
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use crate::record::CompatClass;

    use super::*;

    #[derive(Default)]
    struct Sink(Vec<(String, Vec<String>)>);

    impl HubSink for Sink {
        fn emit(&mut self, record: &ProcessRecord, scope_keys: &[String]) -> Result<()> {
            self.0.push((record.name(), scope_keys.to_vec()));
            Ok(())
        }
    }

    fn record(version: u64, extra: &str) -> ProcessRecord {
        ProcessRecord::new(
            "acme",
            "order",
            version,
            1,
            CompatClass::Compatible,
            &format!(r#"{{"process":"order","x":"{extra}"}}"#),
        )
        .unwrap()
    }

    #[test]
    fn publish_emits_once_and_a_republish_is_a_no_op() {
        let mut publisher = ProcessPublisher::new(Sink::default(), "acme");
        let scope = vec!["branch_group:G".to_owned()];
        assert_eq!(
            publisher.publish(&record(1, "a"), &scope).unwrap(),
            Published::Emitted
        );
        assert_eq!(
            publisher.publish(&record(1, "a"), &scope).unwrap(),
            Published::NoOp
        );
        assert_eq!(publisher.sink().0.len(), 1);
        assert_eq!(publisher.sink().0[0].1, scope);
    }

    #[test]
    fn a_changed_definition_under_one_version_is_refused() {
        let mut publisher = ProcessPublisher::new(Sink::default(), "acme");
        publisher.publish(&record(1, "a"), &[]).unwrap();
        let error = publisher.publish(&record(1, "b"), &[]).unwrap_err();
        assert_eq!(error.code, HASH_MISMATCH);
        assert_eq!(publisher.sink().0.len(), 1);
    }

    #[test]
    fn another_tenant_and_a_forged_hash_are_refused() {
        let mut publisher = ProcessPublisher::new(Sink::default(), "globex");
        assert_eq!(
            publisher.publish(&record(1, "a"), &[]).unwrap_err().code,
            crate::error::FOREIGN_RECORD
        );
        let mut publisher = ProcessPublisher::new(Sink::default(), "acme");
        let mut forged = record(1, "a");
        forged.definition_hash = "00".into();
        assert_eq!(
            publisher.publish(&forged, &[]).unwrap_err().code,
            HASH_MISMATCH
        );
    }

    #[test]
    fn an_inbound_process_record_from_a_node_is_refused() {
        let error = ProcessPublisher::<Sink>::refuse_from_node("process:order@1").unwrap_err();
        assert_eq!(error.code, EDGE_AUTHORED);
        assert!(ProcessPublisher::<Sink>::refuse_from_node("Customer/c1").is_ok());
    }
}
