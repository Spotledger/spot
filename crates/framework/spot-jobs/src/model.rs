//! The job model: jobs, their state, and job-related types.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_core::SiteId;
use spot_scheduler::lease::FencingToken;
use std::fmt;
use time::OffsetDateTime;

/// A job ID, unique within a site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JobId(pub u64);

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A claim: the lease that authorizes a worker to complete a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    /// The fencing token: only the current token may complete.
    pub token: FencingToken,
    /// The worker id that holds the claim.
    pub worker: String,
    /// When the claim expires; the job reverts to Retrying after this.
    pub expires_at: OffsetDateTime,
}

impl Claim {
    /// Create a new claim.
    pub fn new(token: FencingToken, worker: String, expires_at: OffsetDateTime) -> Self {
        Self {
            token,
            worker,
            expires_at,
        }
    }
}

/// The state of a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    /// Pending execution.
    Pending,
    /// Currently executing.
    Running,
    /// Successfully completed.
    Succeeded,
    /// Failed but will retry at the given time.
    Retrying {
        /// When to retry the job.
        at: OffsetDateTime,
    },
    /// Permanently failed; no more retries.
    Dead,
    /// Cancelled by user or system.
    Cancelled,
}

impl fmt::Display for JobState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobState::Pending => write!(f, "pending"),
            JobState::Running => write!(f, "running"),
            JobState::Succeeded => write!(f, "succeeded"),
            JobState::Retrying { .. } => write!(f, "retrying"),
            JobState::Dead => write!(f, "dead"),
            JobState::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A persistent job: work to run later, survive crashes, and never be lost or doubled.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// Unique job ID within the site.
    pub id: JobId,
    /// The site that owns the job.
    pub site: SiteId,
    /// The job name, app-qualified: `app:job`.
    pub name: String,
    /// Job arguments as JSON.
    pub args: Json,
    /// The queue the job belongs to.
    pub queue: String,
    /// Current state of the job.
    pub state: JobState,
    /// Current attempt number (1-indexed).
    pub attempt: u32,
    /// Maximum number of attempts before dead-lettering.
    pub max_attempts: u32,
    /// Earliest time the job may start.
    pub not_before: OffsetDateTime,
    /// Idempotency key: (site, name, key) identifies a job while pending/running/retrying.
    pub idempotency_key: Option<String>,
    /// Current lease claim, if any.
    pub claim: Option<Claim>,
    /// Last error message, if any.
    pub last_error: Option<String>,
    /// When the job was created.
    pub created_at: OffsetDateTime,
    /// Opaque context for extensions (e.g., batch run identity).
    pub context: Json,
}

impl Job {
    /// Check if the job name is valid (`app:job` format).
    pub fn is_valid_name(name: &str) -> bool {
        spot_app_api::context::valid_job_name(name)
    }
}

/// A new job to enqueue.
#[derive(Clone, Debug, PartialEq)]
pub struct NewJob {
    /// The job name, app-qualified: `app:job`.
    pub name: String,
    /// Job arguments as JSON.
    pub args: Json,
    /// The queue the job belongs to.
    pub queue: String,
    /// The site that owns the job.
    pub site: SiteId,
    /// Earliest time the job may start.
    pub not_before: OffsetDateTime,
    /// Idempotency key for deduplication.
    pub idempotency_key: Option<String>,
    /// Opaque context for extensions.
    pub context: Json,
    /// Optional event ID from the outbox (used for intake deduplication).
    pub outbox_event_id: Option<String>,
}

impl NewJob {
    /// Create a new job.
    pub fn new(
        name: String,
        args: Json,
        queue: String,
        site: SiteId,
        not_before: OffsetDateTime,
    ) -> Self {
        Self {
            name,
            args,
            queue,
            site,
            not_before,
            idempotency_key: None,
            context: Json::Null,
            outbox_event_id: None,
        }
    }

    /// Set the idempotency key.
    pub fn with_idempotency_key(mut self, key: String) -> Self {
        self.idempotency_key = Some(key);
        self
    }

    /// Set the context.
    pub fn with_context(mut self, context: Json) -> Self {
        self.context = context;
        self
    }

    /// Set the outbox event ID (for intake deduplication).
    pub fn with_outbox_event_id(mut self, id: String) -> Self {
        self.outbox_event_id = Some(id);
        self
    }
}

/// Result of attempting to insert a new job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertResult {
    /// Job was inserted successfully.
    Inserted(JobId),
    /// A job with this idempotency key already exists (pending/running/retrying).
    Duplicate(JobId),
}

/// The most serialised bytes a job's `context` may hold after a merge (64 KiB).
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024;

/// The most top-level keys a job's `context` may hold after a merge.
pub const MAX_CONTEXT_KEYS: usize = 256;

/// The context after shallow-merging `patch` into `current`, within the context limits.
///
/// A `current` that is not a JSON object (the default is `null`) is replaced by an object.
/// Both stores call this, so the limits mean the same thing everywhere: the merged object may
/// have at most [`MAX_CONTEXT_KEYS`] top-level keys and serialise (compactly) to at most
/// [`MAX_CONTEXT_BYTES`] bytes, otherwise [`Error::ContextTooLarge`](crate::Error::ContextTooLarge).
/// The error carries nothing of the content.
pub fn merge_context_value(
    current: &Json,
    patch: serde_json::Map<String, Json>,
) -> crate::Result<Json> {
    // A cheap refusal before any copy: a patch alone over a limit can never fit.
    if patch.len() > MAX_CONTEXT_KEYS {
        return Err(crate::Error::ContextTooLarge);
    }
    let mut merged = match current {
        Json::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    for (key, value) in patch {
        merged.insert(key, value);
    }
    if merged.len() > MAX_CONTEXT_KEYS {
        return Err(crate::Error::ContextTooLarge);
    }
    let merged = Json::Object(merged);
    let size = serde_json::to_vec(&merged)
        .map_err(|_| crate::Error::ContextTooLarge)?
        .len();
    if size > MAX_CONTEXT_BYTES {
        return Err(crate::Error::ContextTooLarge);
    }
    Ok(merged)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Json) -> serde_json::Map<String, Json> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn merge_is_shallow_and_replaces_a_non_object() {
        let merged = merge_context_value(&Json::Null, obj(json!({"a": {"x": 1}}))).unwrap();
        assert_eq!(merged, json!({"a": {"x": 1}}));
        let merged = merge_context_value(&merged, obj(json!({"a": {"y": 2}, "b": 1}))).unwrap();
        assert_eq!(merged, json!({"a": {"y": 2}, "b": 1}));
        assert_eq!(
            merge_context_value(&json!([1, 2]), obj(json!({"k": 1}))).unwrap(),
            json!({"k": 1})
        );
        assert_eq!(
            merge_context_value(&json!("s"), obj(json!({}))).unwrap(),
            json!({})
        );
    }

    #[test]
    fn the_key_limit_is_inclusive_and_counts_the_merged_object() {
        let keys = |n: usize| -> serde_json::Map<String, Json> {
            (0..n).map(|i| (format!("k{i}"), json!(i))).collect()
        };
        let full = merge_context_value(&Json::Null, keys(MAX_CONTEXT_KEYS)).unwrap();
        assert_eq!(
            merge_context_value(&Json::Null, keys(MAX_CONTEXT_KEYS + 1)),
            Err(crate::Error::ContextTooLarge)
        );
        assert_eq!(
            merge_context_value(&full, obj(json!({"extra": 1}))),
            Err(crate::Error::ContextTooLarge)
        );
        // An existing key does not count twice.
        assert!(merge_context_value(&full, obj(json!({"k0": "new"}))).is_ok());
    }

    #[test]
    fn the_byte_limit_is_inclusive() {
        let at = "x".repeat(MAX_CONTEXT_BYTES - 8);
        assert!(merge_context_value(&Json::Null, obj(json!({"k": at}))).is_ok());
        let over = "x".repeat(MAX_CONTEXT_BYTES - 7);
        assert_eq!(
            merge_context_value(&Json::Null, obj(json!({"k": over}))),
            Err(crate::Error::ContextTooLarge)
        );
    }
}
