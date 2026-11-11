//! App source: the installer's installed-app state and the parsed app manifests.

use spot_installer::SiteState;
use spot_manifest::AppManifest;

use crate::SelfModelError;
use crate::adapters::{pipeline_key, push_edge, push_node};
use crate::evidence::{EvidenceRef, EvidenceSource};
use crate::kind::{EdgeKind, NodeKind, SourceKind};
use crate::model::{Edge, Node, NodeRef, SourceAdapter, SourceData, node_id};

/// Builds app, patch, schedule, seam and pipeline-step nodes from the installer's
/// [`SiteState`] and the manifests the installed apps shipped.
#[derive(Clone, Debug, Default)]
pub struct AppsAdapter {
    site: SiteState,
    manifests: Vec<AppManifest>,
}

impl AppsAdapter {
    /// An adapter over the installed apps.
    #[must_use]
    pub fn new(site: SiteState) -> Self {
        Self {
            site,
            manifests: Vec::new(),
        }
    }

    /// Adds the parsed manifests of the installed apps.
    #[must_use]
    pub fn with_manifests(mut self, manifests: Vec<AppManifest>) -> Self {
        self.manifests = manifests;
        self
    }
}

impl SourceAdapter for AppsAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Apps
    }

    fn epoch(&self) -> u64 {
        u64::try_from(self.site.apps.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::try_from(self.manifests.len()).unwrap_or(u64::MAX))
    }

    fn snapshot(&self) -> Result<SourceData, SelfModelError> {
        let mut data = SourceData::new();
        for installed in &self.site.apps {
            let evidence = EvidenceRef::untimed(
                EvidenceSource::Installer,
                format!("installed:{}@{}", installed.name, installed.version),
            );
            let app = Node::new(
                NodeKind::App,
                installed.name.clone(),
                vec![evidence.clone()],
            )?
            .with_attribute("version", installed.version.to_string())
            .with_attribute("order", installed.order.to_string())
            .with_attribute("digest", installed.digest.to_string())
            .with_attribute("installed", "true");
            let app_ref = app.reference();
            push_node(&mut data.nodes, app);

            for dependency in &installed.depends {
                let dependency_name = dependency.app.as_str().to_owned();
                push_node(
                    &mut data.nodes,
                    Node::new(
                        NodeKind::App,
                        dependency_name.clone(),
                        vec![EvidenceRef::untimed(
                            EvidenceSource::Manifest,
                            format!("depends:{}:{}", installed.name, dependency_name),
                        )],
                    )?
                    .with_attribute("version_req", dependency.version.to_string()),
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        app_ref.id(),
                        node_id(NodeKind::App, &dependency_name),
                        EdgeKind::References,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
            }

            for patch in &installed.applied_patches {
                let patch_ref = NodeRef::new(NodeKind::Patch, patch.clone());
                push_node(
                    &mut data.nodes,
                    Node::new(
                        NodeKind::Patch,
                        patch.clone(),
                        vec![EvidenceRef::untimed(
                            EvidenceSource::Installer,
                            format!("applied-patch:{}:{patch}", installed.name),
                        )],
                    )?
                    .with_app(installed.name.clone())
                    .with_attribute("app", installed.name.clone()),
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        app_ref.id(),
                        patch_ref.id(),
                        EdgeKind::Defines,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
            }
        }

        for manifest in &self.manifests {
            let app_name = manifest.app.name.as_str().to_owned();
            let evidence = EvidenceRef::untimed(
                EvidenceSource::Manifest,
                format!("manifest:{}@{}", app_name, manifest.app.version),
            );
            push_node(
                &mut data.nodes,
                Node::new(NodeKind::App, app_name.clone(), vec![evidence.clone()])?
                    .with_attribute("version", manifest.app.version.to_string())
                    .with_attribute("title", manifest.app.title.clone()),
            );
            let app_ref = NodeRef::new(NodeKind::App, app_name.clone());

            for event in &manifest.events {
                let hook_point = event.event.name();
                let step_name = pipeline_key(&event.doctype, &hook_point, &event.step.to_string());
                let step_ref = NodeRef::new(NodeKind::PipelineStep, step_name.clone());
                push_node(
                    &mut data.nodes,
                    Node::new(NodeKind::PipelineStep, step_name, vec![evidence.clone()])?
                        .with_app(app_name.clone())
                        .with_attribute("doctype", event.doctype.clone())
                        .with_attribute("hook_point", event.event.name())
                        .with_attribute("step", event.step.to_string())
                        .with_attribute("priority", event.priority.to_string()),
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        app_ref.id(),
                        step_ref.id(),
                        EdgeKind::Defines,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
            }

            for schedule in &manifest.schedules {
                let name = format!("{app_name}:{}", schedule.name);
                let schedule_ref = NodeRef::new(NodeKind::Schedule, name.clone());
                push_node(
                    &mut data.nodes,
                    Node::new(NodeKind::Schedule, name, vec![evidence.clone()])?
                        .with_app(app_name.clone())
                        .with_attribute("cron", schedule.cron.as_str())
                        .with_attribute("step", schedule.step.to_string())
                        .with_attribute("queue", format!("{:?}", schedule.queue)),
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        app_ref.id(),
                        schedule_ref.id(),
                        EdgeKind::Defines,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
            }

            for seam in &manifest.seams {
                let name = format!("{}@{}", seam.name, seam.version);
                let seam_ref = NodeRef::new(NodeKind::Seam, name.clone());
                let mut node = Node::new(NodeKind::Seam, name, vec![evidence.clone()])?
                    .with_attribute("seam", seam.name.clone())
                    .with_attribute("version", seam.version.to_string())
                    .with_attribute("kind", format!("{:?}", seam.kind));
                if let Some(reason) = &seam.reason {
                    node = node.with_attribute("reason", reason.clone());
                }
                push_node(&mut data.nodes, node);
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        app_ref.id(),
                        seam_ref.id(),
                        EdgeKind::Defines,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
            }

            for patch in &manifest.patches {
                let patch_ref = NodeRef::new(NodeKind::Patch, patch.id.clone());
                push_node(
                    &mut data.nodes,
                    Node::new(NodeKind::Patch, patch.id.clone(), vec![evidence.clone()])?
                        .with_app(app_name.clone())
                        .with_attribute("app", app_name.clone())
                        .with_attribute("patch_intent", format!("{:?}", patch.intent))
                        .with_attribute("reversible", patch.reversible.to_string())
                        .with_attribute("idempotent", patch.idempotent.to_string()),
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        app_ref.id(),
                        patch_ref.id(),
                        EdgeKind::Defines,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
            }
        }

        Ok(data)
    }
}
