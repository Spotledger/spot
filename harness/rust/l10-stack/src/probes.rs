//! The probe comparisons, free of any I/O.

use spot_l10_liveness::ProbeOutcome;

/// What the plan layer holds: the version it compiled, and the fields of the DocType the last
/// change touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanView {
    /// The metadata version the compiled plans were built from.
    pub version: u64,
    /// Whether the compiled field index of the touched DocType contains the touched field.
    pub has_field: bool,
}

/// The plan probe: the plans must be at the metadata version, and agree on the touched field.
pub fn plan_probe(
    plans: &PlanView,
    meta_version: u64,
    field_expected: Option<bool>,
) -> ProbeOutcome {
    if plans.version != meta_version {
        return ProbeOutcome::Stale {
            observed: plans.version,
            seen: meta_version,
        };
    }
    match field_expected {
        Some(want) if want != plans.has_field => ProbeOutcome::Stale {
            observed: plans.version.saturating_sub(1),
            seen: meta_version,
        },
        _ => ProbeOutcome::Live {
            version: meta_version,
        },
    }
}

/// What a dry run of the migration planner says about the current metadata's catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaView {
    /// Steps the planner would still apply.
    pub pending_steps: usize,
    /// Steps it refuses.
    pub refused_steps: usize,
}

/// The schema probe: an applied change leaves an **empty plan** (nothing left to apply).
pub fn schema_probe(schema: &SchemaView, meta_version: u64) -> ProbeOutcome {
    if schema.pending_steps == 0 && schema.refused_steps == 0 {
        ProbeOutcome::Live {
            version: meta_version,
        }
    } else {
        ProbeOutcome::Stale {
            observed: meta_version.saturating_sub(1),
            seen: meta_version,
        }
    }
}

/// What the app view layer says about the touched field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewView {
    /// The version of the metadata the view was read from.
    pub version: u64,
    /// Whether the view describes the field, when a field was touched.
    pub has_field: bool,
}

/// The view probe.
pub fn view_probe(
    view: &ViewView,
    meta_version: u64,
    field_expected: Option<bool>,
) -> ProbeOutcome {
    plan_probe(
        &PlanView {
            version: view.version,
            has_field: view.has_field,
        },
        meta_version,
        field_expected,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_at_the_version_with_the_field_is_live() {
        let p = PlanView {
            version: 4,
            has_field: true,
        };
        assert_eq!(
            plan_probe(&p, 4, Some(true)),
            ProbeOutcome::Live { version: 4 }
        );
        assert_eq!(plan_probe(&p, 4, None), ProbeOutcome::Live { version: 4 });
    }

    #[test]
    fn a_stale_plan_is_stale_whatever_the_field() {
        let p = PlanView {
            version: 3,
            has_field: true,
        };
        assert!(matches!(
            plan_probe(&p, 4, Some(true)),
            ProbeOutcome::Stale {
                observed: 3,
                seen: 4
            }
        ));
    }

    #[test]
    fn a_removed_field_still_in_the_plan_is_stale() {
        let p = PlanView {
            version: 5,
            has_field: true,
        };
        assert!(matches!(
            plan_probe(&p, 5, Some(false)),
            ProbeOutcome::Stale { .. }
        ));
    }

    #[test]
    fn a_schema_with_steps_left_or_refused_is_stale() {
        assert_eq!(
            schema_probe(
                &SchemaView {
                    pending_steps: 0,
                    refused_steps: 0
                },
                2
            ),
            ProbeOutcome::Live { version: 2 }
        );
        assert!(matches!(
            schema_probe(
                &SchemaView {
                    pending_steps: 1,
                    refused_steps: 0
                },
                2
            ),
            ProbeOutcome::Stale { .. }
        ));
        assert!(matches!(
            schema_probe(
                &SchemaView {
                    pending_steps: 0,
                    refused_steps: 1
                },
                2
            ),
            ProbeOutcome::Stale { .. }
        ));
    }

    #[test]
    fn a_view_missing_the_field_is_stale() {
        let v = ViewView {
            version: 7,
            has_field: false,
        };
        assert!(matches!(
            view_probe(&v, 7, Some(true)),
            ProbeOutcome::Stale { .. }
        ));
        assert_eq!(
            view_probe(&v, 7, Some(false)),
            ProbeOutcome::Live { version: 7 }
        );
    }
}
