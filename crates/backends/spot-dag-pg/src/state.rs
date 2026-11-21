//! Which rows a transition changed, so the store writes back exactly those.
//!
//! The rules themselves are not here: [`spot_dag_exec::rules::apply_step`] and
//! [`spot_dag_exec::rules::already_applied`] are the single definition every store calls. This
//! module only answers the storage question that follows — which node rows the transition
//! touched — because a cancel changes no node row at all.

use spot_dag_exec::Step;

/// The node ids a step changed, so the store writes back exactly those rows.
pub(crate) fn touched(step: &Step) -> Vec<&spot_dag::NodeId> {
    match step {
        Step::Claim { node, .. } => vec![node],
        Step::Complete {
            node,
            ready,
            skipped,
            ..
        } => {
            let mut ids = Vec::with_capacity(1 + ready.len() + skipped.len());
            ids.push(node);
            ids.extend(ready.iter());
            ids.extend(skipped.iter());
            ids
        }
        Step::Cancel { .. } => Vec::new(),
    }
}
