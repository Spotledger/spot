//! Opt-in telemetry: aggregate counters only.
//!
//! What is collected: feature usage counts, error code frequencies and performance buckets, all
//! keyed by short names the code defines (never free text, never a document value), plus one
//! install-random id. The payload carries a schema version. [`Telemetry::pending_payload`] is
//! the "what we send" inspector and returns the exact bytes [`Telemetry::upload`] hands to the
//! [`Uploader`]; the uploader sits behind the egress policy, so a blocked destination simply
//! fails and the counters stay queued.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::consent::ConsentState;
use crate::error::{SupportError, codes};

/// The payload schema version.
pub const SCHEMA_VERSION: u32 = 1;
/// The upper bounds (milliseconds) of the performance buckets; the last bucket is open-ended.
pub const BUCKET_BOUNDS_MS: [u64; 6] = [10, 50, 100, 500, 1_000, 5_000];
/// The most distinct names one counter family holds.
pub const MAX_NAMES: usize = 512;
/// The longest counter name.
pub const MAX_NAME_LEN: usize = 64;

/// Sends bytes somewhere. The host implements it over its egress-controlled HTTP client.
pub trait Uploader {
    /// Uploads one batch.
    ///
    /// # Errors
    /// Any failure; the batch stays queued.
    fn upload(&self, kind: &str, body: &[u8]) -> Result<(), String>;
}

/// The random id of this installation. It identifies nothing but the installation; the host
/// supplies the random bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallId(String);

impl InstallId {
    /// An id from 16 random bytes.
    #[must_use]
    pub fn from_random(bytes: [u8; 16]) -> Self {
        Self(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// The id as lower-case hex.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One telemetry batch: exactly what is sent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryBatch {
    /// The payload schema version.
    pub schema_version: u32,
    /// The installation's random id.
    pub install_id: String,
    /// When the counting period began.
    pub period_start: u64,
    /// When the payload was built.
    pub period_end: u64,
    /// Feature name to use count.
    pub feature_usage: BTreeMap<String, u64>,
    /// Error code to how often it was shown.
    pub error_codes: BTreeMap<String, u64>,
    /// Timed operation to counts per bucket (one more than [`BUCKET_BOUNDS_MS`]).
    pub performance: BTreeMap<String, Vec<u64>>,
}

#[derive(Default)]
struct Counters {
    since: u64,
    feature_usage: BTreeMap<String, u64>,
    error_codes: BTreeMap<String, u64>,
    performance: BTreeMap<String, Vec<u64>>,
}

/// The counters and the upload logic.
pub struct Telemetry {
    install_id: InstallId,
    counters: Mutex<Counters>,
}

fn check_name(name: &str) -> Result<(), SupportError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(SupportError::new(
            codes::NAME_INVALID,
            "a counter name is lower-case letters, digits, underscores and dots, at most 64 long",
        ))
    }
}

impl Telemetry {
    /// Telemetry for this installation, counting from `now`.
    #[must_use]
    pub fn new(install_id: InstallId, now: u64) -> Self {
        Self {
            install_id,
            counters: Mutex::new(Counters {
                since: now,
                ..Counters::default()
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Counters> {
        self.counters.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts one use of a feature. Does nothing unless telemetry is `on`.
    ///
    /// # Errors
    /// `name_invalid` for a name that is not a plain identifier.
    pub fn record_feature(&self, consent: &ConsentState, name: &str) -> Result<(), SupportError> {
        self.bump(consent, name, |c| &mut c.feature_usage)
    }

    /// Counts one display of an error code. Does nothing unless telemetry is `on`.
    ///
    /// # Errors
    /// `name_invalid` for a name that is not a plain identifier.
    pub fn record_error(&self, consent: &ConsentState, code: &str) -> Result<(), SupportError> {
        self.bump(consent, code, |c| &mut c.error_codes)
    }

    fn bump(
        &self,
        consent: &ConsentState,
        name: &str,
        pick: fn(&mut Counters) -> &mut BTreeMap<String, u64>,
    ) -> Result<(), SupportError> {
        check_name(name)?;
        if !consent.may_send_telemetry() {
            return Ok(());
        }
        let mut guard = self.lock();
        let map = pick(&mut guard);
        if map.len() < MAX_NAMES || map.contains_key(name) {
            let n = map.entry(name.to_owned()).or_insert(0);
            *n = n.saturating_add(1);
        }
        Ok(())
    }

    /// Counts one timed operation in its bucket. Does nothing unless telemetry is `on`.
    ///
    /// # Errors
    /// `name_invalid` for a name that is not a plain identifier.
    pub fn record_duration_ms(
        &self,
        consent: &ConsentState,
        name: &str,
        millis: u64,
    ) -> Result<(), SupportError> {
        check_name(name)?;
        if !consent.may_send_telemetry() {
            return Ok(());
        }
        let bucket = BUCKET_BOUNDS_MS
            .iter()
            .position(|bound| millis <= *bound)
            .unwrap_or(BUCKET_BOUNDS_MS.len());
        let mut guard = self.lock();
        if guard.performance.len() < MAX_NAMES || guard.performance.contains_key(name) {
            let slots = guard
                .performance
                .entry(name.to_owned())
                .or_insert_with(|| vec![0; BUCKET_BOUNDS_MS.len() + 1]);
            if let Some(slot) = slots.get_mut(bucket) {
                *slot = slot.saturating_add(1);
            }
        }
        Ok(())
    }

    /// The "what we send" inspector: the exact bytes the next upload carries, or `None` when
    /// telemetry is not `on` or nothing was counted.
    #[must_use]
    pub fn pending_payload(&self, consent: &ConsentState, now: u64) -> Option<Vec<u8>> {
        if !consent.may_send_telemetry() {
            return None;
        }
        let guard = self.lock();
        if guard.feature_usage.is_empty()
            && guard.error_codes.is_empty()
            && guard.performance.is_empty()
        {
            return None;
        }
        let batch = TelemetryBatch {
            schema_version: SCHEMA_VERSION,
            install_id: self.install_id.as_str().to_owned(),
            period_start: guard.since,
            period_end: now,
            feature_usage: guard.feature_usage.clone(),
            error_codes: guard.error_codes.clone(),
            performance: guard.performance.clone(),
        };
        serde_json::to_vec_pretty(&batch).ok()
    }

    /// Uploads the pending payload. On success the counters restart at `now`; on failure
    /// nothing is lost. Returns whether anything was sent.
    ///
    /// # Errors
    /// `upload_failed` when the uploader refuses.
    pub fn upload(
        &self,
        consent: &ConsentState,
        uploader: &dyn Uploader,
        now: u64,
    ) -> Result<bool, SupportError> {
        let Some(body) = self.pending_payload(consent, now) else {
            return Ok(false);
        };
        uploader.upload("telemetry", &body).map_err(|_| {
            SupportError::new(codes::UPLOAD_FAILED, "the uploader reported a failure")
        })?;
        *self.lock() = Counters {
            since: now,
            ..Counters::default()
        };
        Ok(true)
    }
}
