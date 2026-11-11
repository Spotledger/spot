//! Seam source: the step and seam registry's override report.

use std::collections::BTreeMap;

use spot_seams::{OverrideReport, OverrideRowKind, Registry};

use crate::SelfModelError;
use crate::adapters::{ensure_node, pipeline_key, push_edge, push_node, push_provenance_cause};
use crate::evidence::{ChangeKind, EvidenceRef, EvidenceSource, HistoryEntry, Provenance};
use crate::kind::{EdgeKind, NodeKind, SourceKind};
use crate::model::{Edge, Node, NodeRef, SourceAdapter, SourceData};

/// Builds pipeline-step, seam, app and customisation nodes from `spot-seams`'s
/// [`OverrideReport`].
///
/// The report is the registry's own admin listing (`Registry::override_report`): it names
/// every installed override, its effective order and every offered seam version. It carries no
/// time, so callers with a promotion record supply it through
/// [`SeamsAdapter::with_provenance`].
#[derive(Clone, Debug)]
pub struct SeamsAdapter {
    report: OverrideReport,
    provenance: BTreeMap<String, Provenance>,
}

impl Default for SeamsAdapter {
    fn default() -> Self {
        Self::new(OverrideReport {
            overrides: Vec::new(),
            seams: Vec::new(),
        })
    }
}

impl SeamsAdapter {
    /// An adapter over one report.
    #[must_use]
    pub fn new(report: OverrideReport) -> Self {
        Self {
            report,
            provenance: BTreeMap::new(),
        }
    }

    /// An adapter over the registry's current report.
    #[must_use]
    pub fn from_registry(registry: &Registry) -> Self {
        Self::new(registry.override_report())
    }

    /// Adds the provenance of one override.
    ///
    /// Keys are `override:pipeline:<doctype>.<hook>.<app>.<target>` and
    /// `override:seam:<name>@<version>`.
    #[must_use]
    pub fn with_provenance(mut self, key: impl Into<String>, provenance: Provenance) -> Self {
        self.provenance.insert(key.into(), provenance);
        self
    }

    fn history(
        &self,
        entries: &mut Vec<HistoryEntry>,
        target: &NodeRef,
        key: &str,
        change: ChangeKind,
        detail: String,
    ) {
        if let Some(provenance) = self.provenance.get(key) {
            entries.push(HistoryEntry::new(
                target.id(),
                provenance.at,
                change,
                detail,
                provenance.evidence(EvidenceSource::SeamRegistry),
            ));
        }
    }
}

impl SourceAdapter for SeamsAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Seams
    }

    fn epoch(&self) -> u64 {
        let overrides = u64::try_from(self.report.overrides.len()).unwrap_or(u64::MAX);
        let seams = u64::try_from(self.report.seams.len()).unwrap_or(u64::MAX);
        overrides.saturating_add(seams)
    }

    fn snapshot(&self) -> Result<SourceData, SelfModelError> {
        let mut data = SourceData::new();
        let report_evidence = EvidenceRef::untimed(EvidenceSource::SeamRegistry, "override-report");

        for row in &self.report.seams {
            let name = format!("{}@{}", row.name, row.version);
            let mut seam = Node::new(NodeKind::Seam, name.clone(), vec![report_evidence.clone()])?
                .with_app(row.provider_app.clone())
                .with_attribute("seam", row.name.clone())
                .with_attribute("version", row.version.to_string())
                .with_attribute("provider_app", row.provider_app.clone());
            if let Some(replaced_by) = &row.replaced_by {
                seam = seam.with_attribute("replaced_by", replaced_by.clone());
            }
            if !row.reason.is_empty() {
                seam = seam.with_attribute("reason", row.reason.clone());
            }
            let seam_ref = seam.reference();
            push_node(&mut data.nodes, seam);
            let provider = NodeRef::new(NodeKind::App, row.provider_app.clone());
            ensure_node(
                &mut data,
                &provider,
                &EvidenceRef::untimed(
                    EvidenceSource::SeamRegistry,
                    format!("provider:{}", row.provider_app),
                ),
            )?;
            push_edge(
                &mut data.edges,
                Edge::new(
                    provider.id(),
                    seam_ref.id(),
                    EdgeKind::Defines,
                    None,
                    vec![report_evidence.clone()],
                )?,
            );
            if let Some(replaced_by) = &row.replaced_by {
                let key = format!("override:seam:{name}");
                let evidence = provenance_evidence(
                    self.provenance.get(&key),
                    EvidenceSource::SeamRegistry,
                    &key,
                );
                let custom =
                    Node::new(NodeKind::Customisation, key.clone(), vec![evidence.clone()])?
                        .with_app(replaced_by.clone())
                        .with_attribute("kind", "seam_replacement")
                        .with_attribute("seam", name.clone())
                        .with_attribute("reason", row.reason.clone());
                let custom_ref = custom.reference();
                push_node(&mut data.nodes, custom);
                let replacer = NodeRef::new(NodeKind::App, replaced_by.clone());
                ensure_node(&mut data, &replacer, &evidence)?;
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        custom_ref.id(),
                        seam_ref.id(),
                        EdgeKind::Overrides,
                        self.provenance.get(&key).map(|entry| entry.at),
                        vec![evidence],
                    )?,
                );
                if let Some(provenance) = self.provenance.get(&key) {
                    push_provenance_cause(
                        &mut data,
                        &custom_ref,
                        provenance,
                        EvidenceSource::SeamRegistry,
                    )?;
                }
                self.history(
                    &mut data.history,
                    &seam_ref,
                    &key,
                    ChangeKind::Updated,
                    format!("seam {} was replaced", row.name),
                );
            }
        }

        for row in &self.report.overrides {
            let step_name = pipeline_key(&row.doctype, &row.hook_point, &row.target_step);
            let step_ref = NodeRef::new(NodeKind::PipelineStep, step_name.clone());
            let step_evidence = EvidenceRef::untimed(
                EvidenceSource::SeamRegistry,
                format!(
                    "override-report:{}:{}:{}",
                    row.doctype, row.hook_point, row.target_step
                ),
            );
            push_node(
                &mut data.nodes,
                Node::new(
                    NodeKind::PipelineStep,
                    step_name,
                    vec![step_evidence.clone()],
                )?
                .with_app(row.app.clone())
                .with_attribute("doctype", row.doctype.clone())
                .with_attribute("hook_point", row.hook_point.clone())
                .with_attribute("step", row.target_step.clone())
                .with_attribute("effective_order", row.effective_order.to_string())
                .with_attribute("override_kind", row_kind(row.kind)),
            );
            let app_ref = NodeRef::new(NodeKind::App, row.app.clone());
            ensure_node(&mut data, &app_ref, &step_evidence)?;

            let key = format!(
                "override:pipeline:{}.{}.{}.{}",
                row.doctype, row.hook_point, row.app, row.target_step
            );
            let evidence = provenance_evidence(
                self.provenance.get(&key),
                EvidenceSource::SeamRegistry,
                &key,
            );
            let mut custom =
                Node::new(NodeKind::Customisation, key.clone(), vec![evidence.clone()])?
                    .with_app(row.app.clone())
                    .with_attribute("kind", "pipeline_override")
                    .with_attribute("override_kind", row_kind(row.kind))
                    .with_attribute("doctype", row.doctype.clone())
                    .with_attribute("hook_point", row.hook_point.clone())
                    .with_attribute("target_step", row.target_step.clone())
                    .with_attribute("reason", row.reason.clone());
            if let Some(provenance) = self.provenance.get(&key) {
                custom = custom.with_attribute("actor", provenance.actor.clone());
            }
            let custom_ref = custom.reference();
            push_node(&mut data.nodes, custom);
            push_edge(
                &mut data.edges,
                Edge::new(
                    app_ref.id(),
                    custom_ref.id(),
                    EdgeKind::Defines,
                    None,
                    vec![step_evidence],
                )?,
            );
            push_edge(
                &mut data.edges,
                Edge::new(
                    custom_ref.id(),
                    step_ref.id(),
                    EdgeKind::Overrides,
                    self.provenance.get(&key).map(|entry| entry.at),
                    vec![evidence],
                )?,
            );
            if let Some(provenance) = self.provenance.get(&key) {
                push_provenance_cause(
                    &mut data,
                    &custom_ref,
                    provenance,
                    EvidenceSource::SeamRegistry,
                )?;
            }
            self.history(
                &mut data.history,
                &step_ref,
                &key,
                ChangeKind::Updated,
                format!(
                    "the {} override set the step's order to {}",
                    row_kind(row.kind),
                    row.effective_order
                ),
            );
        }

        Ok(data)
    }
}

fn provenance_evidence(
    provenance: Option<&Provenance>,
    source: EvidenceSource,
    key: &str,
) -> EvidenceRef {
    provenance.map_or_else(
        || EvidenceRef::untimed(source, key.to_owned()),
        |provenance| provenance.evidence(source),
    )
}

fn row_kind(kind: OverrideRowKind) -> &'static str {
    match kind {
        OverrideRowKind::Replace => "replace",
        OverrideRowKind::Wrap => "wrap",
        OverrideRowKind::Disable => "disable",
        OverrideRowKind::Reorder => "reorder",
    }
}
