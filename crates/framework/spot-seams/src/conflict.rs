//! Install-time conflicts (WP-EXT-03 refuses an app whose overrides or seams produce any).

use std::fmt;

/// One reason a set of apps cannot be installed together.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Conflict {
    /// A stable machine code, for example `override.duplicate_replace`.
    pub code: &'static str,
    /// A plain message that names the apps and steps or seams involved.
    pub message: String,
    /// The apps involved, sorted and without repeats.
    pub apps: Vec<String>,
}

impl Conflict {
    pub(crate) fn new(code: &'static str, message: String, apps: &[&str]) -> Self {
        let mut apps: Vec<String> = apps.iter().map(|a| (*a).to_owned()).collect();
        apps.sort();
        apps.dedup();
        Self {
            code,
            message,
            apps,
        }
    }
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Conflict {}

/// Stable conflict codes.
pub mod codes {
    /// Two controllers declare the same step name.
    pub const STEP_DUPLICATE: &str = "step.duplicate";
    /// A controller registered for one app declares a step named after another.
    pub const STEP_FOREIGN_APP: &str = "step.foreign_app";
    /// An override targets a step nobody declares.
    pub const OVERRIDE_UNKNOWN_TARGET: &str = "override.unknown_target";
    /// An override's `by` step is not declared by anybody.
    pub const OVERRIDE_UNKNOWN_BY: &str = "override.unknown_by";
    /// An override's target and `by` step belong to different DocTypes.
    pub const OVERRIDE_DOCTYPE_MISMATCH: &str = "override.doctype_mismatch";
    /// `Disable` with a `by` step, or any other kind without one.
    pub const OVERRIDE_BAD_SHAPE: &str = "override.bad_shape";
    /// The same override was declared twice.
    pub const OVERRIDE_DUPLICATE: &str = "override.duplicate";
    /// Two `Replace` overrides of one step.
    pub const OVERRIDE_DUPLICATE_REPLACE: &str = "override.duplicate_replace";
    /// Two `Wrap` overrides of one step.
    pub const OVERRIDE_DUPLICATE_WRAP: &str = "override.duplicate_wrap";
    /// `Replace` and `Wrap` of one step.
    pub const OVERRIDE_REPLACE_AND_WRAP: &str = "override.replace_and_wrap";
    /// `Disable` together with `Replace` or `Wrap` of one step.
    pub const OVERRIDE_DISABLED_AND_CHANGED: &str = "override.disabled_and_changed";
    /// Overrides that loop: A wraps B and B wraps A, or a longer chain back to its start.
    pub const OVERRIDE_CYCLE: &str = "override.cycle";
    /// An override targets a step that another override uses as its `by` step.
    pub const OVERRIDE_CHAINED: &str = "override.chained";
    /// Two apps provide the same seam at the same version, and neither replaces the other.
    pub const SEAM_DUPLICATE_PROVIDER: &str = "seam.duplicate_provider";
    /// `replace_seam` for a seam nobody provides.
    pub const SEAM_REPLACE_UNKNOWN: &str = "seam.replace_unknown";
    /// Two replacements of one seam.
    pub const SEAM_DUPLICATE_REPLACEMENT: &str = "seam.duplicate_replacement";
    /// An app needs a seam at major N, but the providers only offer other majors (WP-EXT-03).
    pub const SEAM_VERSION_MISMATCH: &str = "seam.version_mismatch";
    /// An app needs a seam that no installed app provides at any version (WP-EXT-03).
    pub const SEAM_REQUIRED_MISSING: &str = "seam.required_missing";
    /// An internal effect is not reversible by a lifecycle operation (ADR-080 6.3).
    pub const EFFECTS_INTERNAL_NOT_REVERSIBLE: &str = "effects.internal_not_reversible";
    /// An internal effect at `after_post` is reversible by `cancel`, but the DocType's
    /// `after_cancel` pipeline has no step writing the same store.
    pub const EFFECTS_NO_PAIRED_REVERSAL: &str = "effects.no_paired_reversal";
    /// An override removes the step that reverses a forward effect while the forward step stays.
    pub const EFFECTS_OVERRIDE_DROPS_REVERSAL: &str = "effects.override_drops_reversal";
    /// A step declares an effect class that is not in the known set given to the builder, or a
    /// platform builder was built without a known set at all (fail closed).
    pub const EFFECTS_UNKNOWN_CLASS: &str = "effects.unknown_class";
    /// Effects are declared for a step the controller does not declare.
    pub const EFFECTS_UNKNOWN_STEP: &str = "effects.unknown_step";
    /// The effects of one step are declared twice.
    pub const EFFECTS_DUPLICATE_STEP: &str = "effects.duplicate_step";
    /// An effect runs in the wrong phase for its kind.
    pub const EFFECTS_WRONG_PHASE: &str = "effects.wrong_phase";
    /// An internal effect names a reversal other than the pair of its operation.
    pub const EFFECTS_WRONG_REVERSAL: &str = "effects.wrong_reversal";
    /// An internal effect names no store it writes.
    pub const EFFECTS_INTERNAL_WRITES_NOTHING: &str = "effects.internal_writes_nothing";
    /// An irreversible effect gives no reason.
    pub const EFFECTS_IRREVERSIBLE_REASON: &str = "effects.irreversible_reason";
    /// Any other declaration the contract refuses.
    pub const EFFECTS_INVALID: &str = "effects.invalid";
}
