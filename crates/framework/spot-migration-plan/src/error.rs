//! Typed errors for `spot-migration-plan` (stable `migplan.*` codes).
//!
//! The plan engine is pure: the only failures are a malformed rule table or cost model, and a
//! plan that cannot be computed from the inputs. Callers match on [`PlanError::code`], never on
//! the message.

/// Stable codes of this crate (`migplan.*`, catalogued in `migplan.toml`).
pub mod codes {
    /// The risk rule table or a site policy did not parse.
    pub const BAD_RISK_TABLE: &str = "migplan.bad_risk_table";
    /// The cost model did not parse.
    pub const BAD_COST_MODEL: &str = "migplan.bad_cost_model";
    /// The inputs cannot produce a plan (for example two DocTypes share a storage name).
    pub const BAD_INPUT: &str = "migplan.bad_input";
    /// Two traversal indexes map to one name.
    pub const TRAVERSAL_NAME_CLASH: &str = "migplan.traversal_name_clash";
}

/// Why a plan call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// A risk rule table or policy is not valid TOML, or names a risk that is not
    /// `additive`, `needs_review` or `destructive`.
    #[error("{}: {message}", codes::BAD_RISK_TABLE)]
    BadRiskTable {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// A cost model is not valid TOML, or a rate is missing.
    #[error("{}: {message}", codes::BAD_COST_MODEL)]
    BadCostModel {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// The inputs cannot produce a plan.
    #[error("{}: {message}", codes::BAD_INPUT)]
    BadInput {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// Two traversable links (or a link and a reserved catalogue index) map to one index name.
    #[error(
        "{}: index name `{name}` of {first} clashes with {second}",
        codes::TRAVERSAL_NAME_CLASH
    )]
    TraversalNameClash {
        /// The clashing index name.
        name: String,
        /// The link whose name clashes, as `DocType.field`.
        first: String,
        /// What it clashes with: another `DocType.field`, or the catalogue's own index.
        second: String,
    },
}

impl PlanError {
    /// The stable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRiskTable { .. } => codes::BAD_RISK_TABLE,
            Self::BadCostModel { .. } => codes::BAD_COST_MODEL,
            Self::BadInput { .. } => codes::BAD_INPUT,
            Self::TraversalNameClash { .. } => codes::TRAVERSAL_NAME_CLASH,
        }
    }
}
