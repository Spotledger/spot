//! Collecting raw findings and grouping them into one item per root cause.
//!
//! Every check pushes a [`Finding`] — one category, one cause, one broken artefact and a
//! resolution. [`Collector::finish`] groups the findings that share a category, a release item
//! and an effect into a single [`ConflictItem`], so one removed field that a view, three scripts
//! and a fixture all mention is one plan item with four artefacts. The result is sorted by code,
//! then release item, then id, so a report is deterministic whatever order the checks ran in.

use std::collections::BTreeMap;

use crate::model::{ArtefactRef, Cause, ConflictCategory, ConflictItem, ConflictReport, slug};

/// One raw finding: one artefact broken by one release item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    /// The conflict category.
    pub category: ConflictCategory,
    /// The release item that broke the artefact.
    pub cause: Cause,
    /// The broken artefact.
    pub artefact: ArtefactRef,
    /// What the site owner can do.
    pub resolution: String,
}

/// Accumulates findings and turns them into a report.
#[derive(Debug, Default)]
pub(crate) struct Collector {
    findings: Vec<Finding>,
}

impl Collector {
    /// An empty collector.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records that `cause` broke `artefact` in `category`, with `resolution`.
    pub(crate) fn push(
        &mut self,
        category: ConflictCategory,
        cause: Cause,
        artefact: ArtefactRef,
        resolution: impl Into<String>,
    ) {
        self.findings.push(Finding {
            category,
            cause,
            artefact,
            resolution: resolution.into(),
        });
    }

    /// Groups the findings into items and returns the report bound to `plan_digest`.
    pub(crate) fn finish(mut self, plan_digest: impl Into<String>) -> ConflictReport {
        self.findings.sort_by(|a, b| {
            (
                a.category,
                &a.cause.release_item,
                &a.cause.effect,
                &a.artefact,
            )
                .cmp(&(
                    b.category,
                    &b.cause.release_item,
                    &b.cause.effect,
                    &b.artefact,
                ))
        });

        // Group by (category, release item, effect). BTreeMap keeps the groups in the sorted
        // order the findings arrived in, so the report is deterministic.
        let mut groups: BTreeMap<(ConflictCategory, String, String), Vec<Finding>> =
            BTreeMap::new();
        for finding in self.findings {
            groups
                .entry((
                    finding.category,
                    finding.cause.release_item.clone(),
                    finding.cause.effect.clone(),
                ))
                .or_default()
                .push(finding);
        }

        let mut items: Vec<ConflictItem> = groups
            .into_iter()
            .map(|((category, release_item, effect), findings)| {
                let mut artefacts: Vec<ArtefactRef> =
                    findings.iter().map(|f| f.artefact.clone()).collect();
                artefacts.sort();
                artefacts.dedup();
                let resolution = findings
                    .first()
                    .map(|f| f.resolution.clone())
                    .unwrap_or_default();
                let id = format!(
                    "conflict/{}/{}",
                    category.slug(),
                    slug(&format!("{release_item}|{effect}"))
                );
                ConflictItem {
                    id,
                    code: category.code(),
                    category,
                    cause: Cause::new(release_item, effect),
                    artefacts,
                    risk: category.risk(),
                    resolution,
                }
            })
            .collect();
        items.sort_by(|a, b| {
            (&a.code, &a.cause.release_item, &a.id).cmp(&(&b.code, &b.cause.release_item, &b.id))
        });

        ConflictReport {
            plan_digest: plan_digest.into(),
            items,
        }
    }
}
