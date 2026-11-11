//! The set of plans for one metadata version: every DocType's compiled plan, built all at
//! once or not at all.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_meta::EffectiveMeta;

use crate::error::PlanError;
use crate::plan::DocPlan;

/// Every DocType's plan at one metadata version.
///
/// Immutable: a metadata change builds a new one, so an operation holding a `PlanSet` keeps a
/// consistent view (ADR-05). Plans live in name order; `get` hands out the shared
/// [`Arc`], which is what makes cheap "did this plan change" comparisons possible.
#[derive(Clone, Debug)]
pub struct PlanSet {
    version: u64,
    plans: BTreeMap<String, Arc<DocPlan>>,
}

impl PlanSet {
    /// Compiles the plan of every DocType in `meta`.
    ///
    /// Every problem is reported at once: one failing DocType does not hide another's
    /// problems, and a failed build produces no set.
    ///
    /// # Errors
    ///
    /// Every compile error found, each naming its DocType and field.
    pub fn build(meta: &EffectiveMeta) -> Result<Self, Vec<PlanError>> {
        let mut plans = BTreeMap::new();
        let mut errors: Vec<PlanError> = Vec::new();
        for d in meta.definitions() {
            match meta.doctype(&d.name) {
                None => errors.push(PlanError::ViewMismatch {
                    doctype: d.name.clone(),
                    message: "the metadata carries no compiled app view for this DocType".into(),
                }),
                Some(view) => match DocPlan::compile(d, &view) {
                    Ok(plan) => {
                        plans.insert(plan.name().as_str().to_owned(), Arc::new(plan));
                    }
                    Err(mut es) => errors.append(&mut es),
                },
            }
        }
        // Cross-DocType rule validation: a `fetch_from` path must end at a field the linked
        // DocType has, when both DocTypes are in this set.
        for plan in plans.values() {
            for field in plan.fields() {
                let Some(fetch) = &field.fetch_from else {
                    continue;
                };
                let Some(target) = plan
                    .links()
                    .iter()
                    .find(|(name, _)| name.as_str() == fetch.link_field)
                    .map(|(_, target)| target)
                else {
                    continue;
                };
                if let Some(target_plan) = plans.get(target.as_str())
                    && target_plan.field_ix(&fetch.target_field).is_none()
                {
                    errors.push(PlanError::FetchFromUnknownTarget {
                        doctype: plan.name().as_str().to_owned(),
                        field: field.name.as_str().to_owned(),
                        link_field: fetch.link_field.clone(),
                        target_field: fetch.target_field.clone(),
                    });
                }
            }
        }
        if errors.is_empty() {
            Ok(Self {
                version: meta.version(),
                plans,
            })
        } else {
            Err(errors)
        }
    }

    /// The metadata version the plans were built at.
    #[must_use]
    pub fn meta_version(&self) -> u64 {
        self.version
    }

    /// A DocType's plan; `None` when the metadata has no such DocType.
    #[must_use]
    pub fn get(&self, doctype: &str) -> Option<Arc<DocPlan>> {
        self.plans.get(doctype).cloned()
    }

    /// Every DocType with a plan, in name order.
    pub fn doctypes(&self) -> impl Iterator<Item = &str> {
        self.plans.keys().map(String::as_str)
    }

    /// Assembles a set from already-built plans (`PlanCache::refresh` reuses the old
    /// DocTypes' arcs into the new set this way).
    #[must_use]
    pub(crate) fn from_parts(version: u64, plans: BTreeMap<String, Arc<DocPlan>>) -> Self {
        Self { version, plans }
    }

    /// The underlying plans, in name order.
    pub(crate) fn plans(&self) -> &BTreeMap<String, Arc<DocPlan>> {
        &self.plans
    }
}
