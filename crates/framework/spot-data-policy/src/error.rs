//! Typed errors with stable `data_policy.*` codes (catalogued in
//! `spot-errors/catalogue/data_policy.toml`).
//!
//! Messages name the rule, region or record that made the request fail; they never carry a
//! secret, a field value or a raw SQL string. `reason` arguments are fixed words chosen by this
//! crate, so a caller cannot smuggle data into a log through an error.

/// Stable error codes of this crate.
pub mod codes {
    /// A residency policy file or value is malformed.
    pub const INVALID_POLICY: &str = "data_policy.invalid_policy";
    /// A region identifier is malformed.
    pub const INVALID_REGION: &str = "data_policy.invalid_region";
    /// A retention rule file or a rule value is malformed.
    pub const INVALID_RULE: &str = "data_policy.invalid_rule";
    /// A legal hold file or a hold value is malformed.
    pub const INVALID_HOLD: &str = "data_policy.invalid_hold";
    /// A statutory-floor table entry is malformed.
    pub const INVALID_FLOOR: &str = "data_policy.invalid_floor";
    /// A field-classification table entry is malformed.
    pub const INVALID_CLASSIFICATION: &str = "data_policy.invalid_classification";
    /// A record handed to the planner or the store is malformed.
    pub const INVALID_RECORD: &str = "data_policy.invalid_record";
    /// The injected calendar refused a date or a period.
    pub const INVALID_CALENDAR: &str = "data_policy.invalid_calendar";
    /// A region change has no owner approval.
    pub const REGION_CHANGE_UNAPPROVED: &str = "data_policy.region_change_unapproved";
    /// A run was started again instead of resumed, or resumed without a checkpoint.
    pub const RUN_MISMATCH: &str = "data_policy.run_mismatch";
    /// The plan behind a resumed run is not the plan the run started with.
    pub const PLAN_CHANGED: &str = "data_policy.plan_changed";
    /// The retention store failed.
    pub const STORE_FAILED: &str = "data_policy.store_failed";
    /// A deletion proof does not verify or its record is still present.
    pub const PROOF_INVALID: &str = "data_policy.proof_invalid";
    /// An execution option is not valid.
    pub const INVALID_OPTION: &str = "data_policy.invalid_option";
}

/// Anything the data-policy layer can refuse to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataPolicyError {
    /// A residency policy file or value is malformed.
    #[error("the residency policy is not valid: {reason}")]
    InvalidPolicy {
        /// What is wrong (a fixed description, never a value from a record).
        reason: String,
    },
    /// A region identifier is malformed.
    #[error("the region {region} is not valid: {reason}")]
    InvalidRegion {
        /// The region that failed validation.
        region: String,
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A retention rule file or a rule value is malformed.
    #[error("retention rule {rule} is not valid: {reason}")]
    InvalidRule {
        /// The rule id, or `<file>` for a file-level problem.
        rule: String,
        /// What is wrong.
        reason: String,
    },
    /// A legal hold file or a hold value is malformed.
    #[error("legal hold {hold} is not valid: {reason}")]
    InvalidHold {
        /// The hold id, or `<file>` for a file-level problem.
        hold: String,
        /// What is wrong.
        reason: String,
    },
    /// A statutory-floor table entry is malformed.
    #[error("statutory floor {entry} is not valid: {reason}")]
    InvalidFloor {
        /// The DocType class and jurisdiction, or `<file>`.
        entry: String,
        /// What is wrong.
        reason: String,
    },
    /// A field-classification table entry is malformed.
    #[error("field classification {entry} is not valid: {reason}")]
    InvalidClassification {
        /// The DocType and field, or `<file>`.
        entry: String,
        /// What is wrong.
        reason: String,
    },
    /// A record handed to the planner or the store is malformed.
    #[error("record {record} is not valid: {reason}")]
    InvalidRecord {
        /// The record id, or `<file>`.
        record: String,
        /// What is wrong.
        reason: String,
    },
    /// The injected calendar refused a date or a period.
    #[error("the calendar {calendar} refused {detail}")]
    InvalidCalendar {
        /// The calendar's name.
        calendar: String,
        /// What it refused.
        detail: String,
    },
    /// A region change has no owner approval.
    #[error("moving a tenant from {from} to {to} needs the owner's approval")]
    RegionChangeUnapproved {
        /// The region the tenant is in.
        from: String,
        /// The requested region.
        to: String,
    },
    /// A run was started again instead of resumed, or resumed without a checkpoint.
    #[error("run {run} cannot be {operation}: {reason}")]
    RunMismatch {
        /// The run id.
        run: String,
        /// `started` or `resumed`.
        operation: &'static str,
        /// What is wrong.
        reason: &'static str,
    },
    /// The plan behind a resumed run is not the plan the run started with.
    #[error("run {run} was planned from a different policy")]
    PlanChanged {
        /// The run id.
        run: String,
    },
    /// The retention store failed.
    #[error("the retention store failed: {reason}")]
    StoreFailed {
        /// A fixed description of the failed operation.
        reason: &'static str,
    },
    /// A deletion proof does not verify or its record is still present.
    #[error("the deletion proof for {record} is not valid: {reason}")]
    ProofInvalid {
        /// The record the proof claims to have deleted.
        record: String,
        /// What is wrong.
        reason: &'static str,
    },
    /// An execution option is not valid.
    #[error("the execution option {option} is not valid: {reason}")]
    InvalidOption {
        /// The option name.
        option: &'static str,
        /// What is wrong.
        reason: &'static str,
    },
}

impl DataPolicyError {
    /// The stable code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidPolicy { .. } => codes::INVALID_POLICY,
            Self::InvalidRegion { .. } => codes::INVALID_REGION,
            Self::InvalidRule { .. } => codes::INVALID_RULE,
            Self::InvalidHold { .. } => codes::INVALID_HOLD,
            Self::InvalidFloor { .. } => codes::INVALID_FLOOR,
            Self::InvalidClassification { .. } => codes::INVALID_CLASSIFICATION,
            Self::InvalidRecord { .. } => codes::INVALID_RECORD,
            Self::InvalidCalendar { .. } => codes::INVALID_CALENDAR,
            Self::RegionChangeUnapproved { .. } => codes::REGION_CHANGE_UNAPPROVED,
            Self::RunMismatch { .. } => codes::RUN_MISMATCH,
            Self::PlanChanged { .. } => codes::PLAN_CHANGED,
            Self::StoreFailed { .. } => codes::STORE_FAILED,
            Self::ProofInvalid { .. } => codes::PROOF_INVALID,
            Self::InvalidOption { .. } => codes::INVALID_OPTION,
        }
    }
}
