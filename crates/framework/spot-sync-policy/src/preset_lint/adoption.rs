//! Lint S2 (decision of 31 Oct 2026): a rule that can never be satisfied is a defect in the
//! declaration, so it is refused when it is written.
//!
//! * a link marked `requires_adopted` whose target is a `record` entity: a record is owned by
//!   its creating node for ever and is never adopted, so the link could never be saved;
//! * a `handover` entity with `adopt = never`: a handover exists to be adopted by the hub, so a
//!   handover that is never adopted is a `record` declared under the wrong name.

use super::links::edges_of;
use super::{Ctx, Finding, codes};
use crate::preset::{AdoptMode, Preset};

pub(crate) fn lint_adoption(ctx: &Ctx<'_>, out: &mut Vec<Finding>) {
    for root in ctx.roots() {
        for edge in edges_of(ctx, root) {
            if !edge.requires_adopted {
                continue;
            }
            for target in &edge.targets {
                if ctx.preset(target) == Preset::Record {
                    out.push(Finding::new(
                        codes::LINT_REQUIRES_ADOPTED_RECORD,
                        &root.name,
                        format!(
                            "link `{}` is marked `requires_adopted` but points to record \
                             `{target}`, which is never adopted, so the link could never be \
                             saved; point it at a handover entity or remove the mark",
                            edge.path
                        ),
                        &[target],
                    ));
                }
            }
        }
        if let Some(policy) = ctx.table.get(&root.name)
            && policy.preset == Preset::Handover
            && policy.adopt_mode() == AdoptMode::Never
        {
            out.push(Finding::new(
                codes::LINT_ADOPT_NEVER,
                &root.name,
                format!(
                    "handover `{}` sets `adopt = never`, so the hub would never adopt it and \
                     it could never be used where adoption is required; use the `record` \
                     preset, or set `adopt` to `manual` or `auto`",
                    root.name
                ),
                &[],
            ));
        }
    }
}
