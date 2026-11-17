//! The sync form of a published process version.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, INVALID_RECORD, Result};

/// How a version relates to the ones before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatClass {
    /// Existing Cases and new Cases can use it freely.
    Compatible,
    /// Breaking: a node starts new Cases on it only when it declares it accepts breaking versions.
    Breaking,
}

/// One published version of a process.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    /// The tenant the process belongs to.
    pub tenant: String,
    /// The process name.
    pub process: String,
    /// The version (at least 1).
    pub version: u64,
    /// SHA-256 (hex) of `payload`.
    pub definition_hash: String,
    /// The lowest engine version that can run it.
    pub min_engine_version: u64,
    /// The compatibility class.
    pub compat_class: CompatClass,
    /// The canonical definition JSON (never logged).
    pub payload: String,
}

impl std::fmt::Debug for ProcessRecord {
    /// Never shows the payload.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRecord")
            .field("process", &self.process)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// The record name: `process:<process>@<version>`, so the intake's `(name, version)` idempotency
/// makes a republish a no-op.
pub fn record_name(process: &str, version: u64) -> String {
    format!("process:{process}@{version}")
}

/// The SHA-256 of a definition, lower-case hex.
pub fn hash_definition(payload: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(payload.as_bytes()) {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

impl ProcessRecord {
    /// Builds a record, computing the hash of `payload`.
    ///
    /// # Errors
    ///
    /// `sync_process.invalid_record` for an empty tenant or process name, a zero version, a name
    /// holding `@`, or a payload whose `process` field does not equal `process`.
    pub fn new(
        tenant: &str,
        process: &str,
        version: u64,
        min_engine_version: u64,
        compat_class: CompatClass,
        payload: &str,
    ) -> Result<Self> {
        let record = Self {
            tenant: tenant.to_owned(),
            process: process.to_owned(),
            version,
            definition_hash: hash_definition(payload),
            min_engine_version,
            compat_class,
            payload: payload.to_owned(),
        };
        record.check_shape()?;
        Ok(record)
    }

    /// The record name.
    pub fn name(&self) -> String {
        record_name(&self.process, self.version)
    }

    /// Shape checks that need no store: names, version, and the payload naming the same process.
    ///
    /// # Errors
    ///
    /// `sync_process.invalid_record`.
    pub fn check_shape(&self) -> Result<()> {
        let bad = |why: &str| Err(Error::new(INVALID_RECORD, why.to_owned()));
        if self.tenant.is_empty() || self.process.is_empty() || self.process.contains('@') {
            return bad("a process record needs a tenant and a process name without `@`");
        }
        if self.version == 0 {
            return bad("a process version is at least 1");
        }
        let named = serde_json::from_str::<serde_json::Value>(&self.payload)
            .ok()
            .and_then(|v| v.get("process").and_then(|p| p.as_str().map(str::to_owned)));
        if named.as_deref() != Some(self.process.as_str()) {
            return bad("the definition does not name the process the record names");
        }
        Ok(())
    }

    /// Whether the stored hash equals the hash of the payload.
    pub fn hash_ok(&self) -> bool {
        self.definition_hash == hash_definition(&self.payload)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    pub fn payload(process: &str) -> String {
        format!(r#"{{"process":"{process}","steps":[]}}"#)
    }

    #[test]
    fn the_name_and_hash_are_stable() {
        let record = ProcessRecord::new(
            "acme",
            "order",
            3,
            1,
            CompatClass::Compatible,
            &payload("order"),
        )
        .unwrap();
        assert_eq!(record.name(), "process:order@3");
        assert!(record.hash_ok());
        assert!(!format!("{record:?}").contains("steps"));
    }

    #[test]
    fn a_payload_for_another_process_is_refused() {
        let error = ProcessRecord::new(
            "acme",
            "order",
            1,
            1,
            CompatClass::Compatible,
            &payload("refund"),
        )
        .unwrap_err();
        assert_eq!(error.code, INVALID_RECORD);
        assert!(
            ProcessRecord::new(
                "acme",
                "order",
                0,
                1,
                CompatClass::Compatible,
                &payload("order")
            )
            .is_err()
        );
        assert!(
            ProcessRecord::new(
                "acme",
                "a@b",
                1,
                1,
                CompatClass::Compatible,
                &payload("a@b")
            )
            .is_err()
        );
    }
}
