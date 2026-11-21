//! Migration source: `spot-migration-audit` runs, their impacts, and `spot-release-meta`
//! patch declarations.

use spot_migration_audit::MigrationRun;
use spot_release_meta::PatchDecl;

use crate::SelfModelError;
use crate::adapters::{push_edge, push_node};
use crate::evidence::{ChangeKind, EvidenceRef, EvidenceSource, HistoryEntry};
use crate::kind::{EdgeKind, NodeKind, SourceKind};
use crate::model::{Edge, Node, NodeRef, SourceAdapter, SourceData, node_id};

/// One node a migration run changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationImpact {
    /// The run id.
    pub run: String,
    /// The node it changed.
    pub target: NodeRef,
    /// What changed, in one non-sensitive sentence.
    pub change: String,
    /// What kind of change it was.
    pub change_kind: ChangeKind,
}

/// One app version a migration run installed or promoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppInstallRecord {
    /// The run id.
    pub run: String,
    /// The app.
    pub app: String,
    /// The installed version.
    pub version: String,
}

/// One patch a migration run applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchRecord {
    /// The run id.
    pub run: String,
    /// The patch id.
    pub patch: String,
    /// The app that declares it.
    pub app: String,
    /// What it does, in one sentence.
    pub intent: String,
}

/// Builds migration, patch, app and affected-node entries from migration audit runs and
/// release metadata.
#[derive(Clone, Debug, Default)]
pub struct MigrationsAdapter {
    runs: Vec<MigrationRun>,
    impacts: Vec<MigrationImpact>,
    installs: Vec<AppInstallRecord>,
    patches: Vec<PatchRecord>,
    declarations: Vec<PatchDecl>,
}

impl MigrationsAdapter {
    /// An adapter with no data yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one audited run.
    #[must_use]
    pub fn with_run(mut self, run: MigrationRun) -> Self {
        self.runs.push(run);
        self
    }

    /// Adds one node impact.
    #[must_use]
    pub fn with_impact(mut self, impact: MigrationImpact) -> Self {
        self.impacts.push(impact);
        self
    }

    /// Adds one installed app version.
    #[must_use]
    pub fn with_install(mut self, install: AppInstallRecord) -> Self {
        self.installs.push(install);
        self
    }

    /// Adds one applied patch.
    #[must_use]
    pub fn with_patch(mut self, patch: PatchRecord) -> Self {
        self.patches.push(patch);
        self
    }

    /// Adds one release metadata patch declaration.
    #[must_use]
    pub fn with_declaration(mut self, declaration: PatchDecl) -> Self {
        self.declarations.push(declaration);
        self
    }
}

impl SourceAdapter for MigrationsAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Migrations
    }

    fn epoch(&self) -> u64 {
        u64::try_from(self.runs.len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::try_from(self.declarations.len()).unwrap_or(u64::MAX))
    }

    fn snapshot(&self) -> Result<SourceData, SelfModelError> {
        let mut data = SourceData::new();
        for run in &self.runs {
            let run_id = run.id.as_str().to_owned();
            let evidence = EvidenceRef::at(
                EvidenceSource::MigrationAudit,
                format!("run:{run_id}"),
                run.started,
            );
            let mut migration =
                Node::new(NodeKind::Migration, run_id.clone(), vec![evidence.clone()])?
                    .with_attribute("actor", run.actor.clone())
                    .with_attribute("outcome", run.outcome.as_str())
                    .with_attribute("started", run.started.to_string())
                    .with_attribute("plan_digest", run.plan_digest.clone())
                    .with_attribute("risk", run.plan.summary.risk.clone());
            if let Some(ended) = run.ended {
                migration = migration.with_attribute("ended", ended.to_string());
            }
            let migration_ref = migration.reference();
            push_node(&mut data.nodes, migration);

            for impact in self.impacts.iter().filter(|impact| impact.run == run_id) {
                push_node(
                    &mut data.nodes,
                    Node::new(
                        impact.target.kind,
                        impact.target.name.clone(),
                        vec![evidence.clone()],
                    )?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        migration_ref.id(),
                        impact.target.id(),
                        EdgeKind::Affects,
                        Some(run.started),
                        vec![evidence.clone()],
                    )?,
                );
                data.history.push(HistoryEntry::new(
                    impact.target.id(),
                    run.ended.unwrap_or(run.started),
                    impact.change_kind,
                    impact.change.clone(),
                    evidence.clone(),
                ));
            }

            for install in self.installs.iter().filter(|install| install.run == run_id) {
                let app_ref = NodeRef::new(NodeKind::App, install.app.clone());
                push_node(
                    &mut data.nodes,
                    Node::new(NodeKind::App, install.app.clone(), vec![evidence.clone()])?
                        .with_attribute("version", install.version.clone()),
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        app_ref.id(),
                        migration_ref.id(),
                        EdgeKind::InstalledBy,
                        Some(run.started),
                        vec![evidence.clone()],
                    )?,
                );
                data.history.push(HistoryEntry::new(
                    app_ref.id(),
                    run.ended.unwrap_or(run.started),
                    ChangeKind::Promoted,
                    format!("the app was promoted to {}", install.version),
                    evidence.clone(),
                ));
            }

            for patch in self.patches.iter().filter(|patch| patch.run == run_id) {
                let patch_ref = NodeRef::new(NodeKind::Patch, patch.patch.clone());
                push_node(
                    &mut data.nodes,
                    Node::new(NodeKind::Patch, patch.patch.clone(), vec![evidence.clone()])?
                        .with_app(patch.app.clone())
                        .with_attribute("app", patch.app.clone())
                        .with_attribute("intent", patch.intent.clone()),
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        migration_ref.id(),
                        patch_ref.id(),
                        EdgeKind::Affects,
                        Some(run.started),
                        vec![evidence.clone()],
                    )?,
                );
                data.history.push(HistoryEntry::new(
                    patch_ref.id(),
                    run.ended.unwrap_or(run.started),
                    ChangeKind::Updated,
                    patch.intent.clone(),
                    evidence.clone(),
                ));
            }
        }

        for declaration in &self.declarations {
            let evidence = EvidenceRef::untimed(
                EvidenceSource::ReleaseMetadata,
                format!("patch:{}", declaration.id),
            );
            let patch_ref = NodeRef::new(NodeKind::Patch, declaration.id.clone());
            let mut patch = Node::new(
                NodeKind::Patch,
                declaration.id.clone(),
                vec![evidence.clone()],
            )?
            .with_attribute("declared_intent", declaration.intent.clone())
            .with_attribute("class", format!("{:?}", declaration.class))
            .with_attribute("reversible", declaration.reversible.to_string())
            .with_attribute("idempotent", declaration.idempotent.to_string());
            if !declaration.doctypes.is_empty() {
                patch = patch.with_attribute("doctypes", declaration.doctypes.join(","));
            }
            push_node(&mut data.nodes, patch);
            for doctype in &declaration.doctypes {
                push_node(
                    &mut data.nodes,
                    Node::new(NodeKind::DocType, doctype.clone(), vec![evidence.clone()])?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        patch_ref.id(),
                        node_id(NodeKind::DocType, doctype),
                        EdgeKind::Affects,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
            }
        }

        Ok(data)
    }
}
