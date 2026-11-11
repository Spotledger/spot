//! Script source: the TypeScript script index and the fields its `.d.ts` check resolved.

use spot_ts::ScriptId;

use crate::SelfModelError;
use crate::adapters::{pipeline_key, push_edge, push_node};
use crate::evidence::{ChangeKind, EvidenceRef, EvidenceSource, HistoryEntry};
use crate::kind::{EdgeKind, NodeKind, SourceKind};
use crate::model::{Edge, Node, NodeRef, SourceAdapter, SourceData, node_id};

/// One indexed script and what it refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptRecord {
    /// The script's stable `spot-ts` id (app, name, version).
    pub id: ScriptId,
    /// The DocType whose hook the script serves.
    pub doctype: String,
    /// The hook point it runs at, when it does (the ADR-51 name, e.g. `before_save`).
    pub hook: Option<String>,
    /// The pipeline step's name; defaults to the script's name.
    pub step: Option<String>,
    /// The fields the `.d.ts` check says the script names.
    pub referenced_fields: Vec<String>,
    /// Unix microseconds of the save or promotion, when known.
    pub at: Option<i64>,
    /// The proving record; defaults to the script index id.
    pub evidence: Option<EvidenceRef>,
    /// The app whose install or upgrade shipped the script.
    pub installed_by: Option<String>,
}

impl ScriptRecord {
    /// A script record from a `spot-ts` id.
    #[must_use]
    pub fn new(id: ScriptId, doctype: impl Into<String>) -> Self {
        Self {
            id,
            doctype: doctype.into(),
            hook: None,
            step: None,
            referenced_fields: Vec::new(),
            at: None,
            evidence: None,
            installed_by: None,
        }
    }

    /// Sets the hook point the script runs at.
    #[must_use]
    pub fn hooks_into(mut self, hook: impl Into<String>) -> Self {
        self.hook = Some(hook.into());
        self
    }

    /// Sets the pipeline step's name (defaults to the script's name).
    #[must_use]
    pub fn as_step(mut self, step: impl Into<String>) -> Self {
        self.step = Some(step.into());
        self
    }

    /// Sets the fields the `.d.ts` check resolved.
    #[must_use]
    pub fn references(mut self, fields: Vec<String>) -> Self {
        self.referenced_fields = fields;
        self
    }

    /// Sets the save/promotion time.
    #[must_use]
    pub fn at(mut self, at: i64) -> Self {
        self.at = Some(at);
        self
    }

    /// Sets the proving record.
    #[must_use]
    pub fn evidence(mut self, evidence: EvidenceRef) -> Self {
        self.evidence = Some(evidence);
        self
    }

    /// Names the app whose install shipped the script.
    #[must_use]
    pub fn installed_by(mut self, app: impl Into<String>) -> Self {
        self.installed_by = Some(app.into());
        self
    }
}

/// Builds script, field, step and app nodes from the script index.
#[derive(Clone, Debug, Default)]
pub struct ScriptsAdapter {
    scripts: Vec<ScriptRecord>,
}

impl ScriptsAdapter {
    /// An adapter with no scripts yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one indexed script.
    #[must_use]
    pub fn with_script(mut self, script: ScriptRecord) -> Self {
        self.scripts.push(script);
        self
    }
}

impl SourceAdapter for ScriptsAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Scripts
    }

    fn epoch(&self) -> u64 {
        u64::try_from(self.scripts.len()).unwrap_or(u64::MAX)
    }

    fn snapshot(&self) -> Result<SourceData, SelfModelError> {
        let mut data = SourceData::new();
        for script in &self.scripts {
            let id = format!("{}:{}", script.id.app, script.id.name);
            let evidence = script.evidence.clone().unwrap_or_else(|| {
                EvidenceRef::untimed(
                    EvidenceSource::ScriptIndex,
                    format!("script:{id}@{}", script.id.version),
                )
            });
            if !evidence.is_complete() {
                return Err(SelfModelError::EvidenceMissing { what: id });
            }
            let mut node = Node::new(NodeKind::Script, id.clone(), vec![evidence.clone()])?
                .with_app(script.id.app.clone())
                .with_attribute("version", script.id.version.to_string())
                .with_attribute("doctype", script.doctype.clone());
            if let Some(hook) = &script.hook {
                node = node.with_attribute("hook", hook.clone());
            }
            if let Some(step) = &script.step {
                node = node.with_attribute("step", step.clone());
            }
            let script_ref = node.reference();
            push_node(&mut data.nodes, node);

            let app_ref = NodeRef::new(NodeKind::App, script.id.app.clone());
            push_node(
                &mut data.nodes,
                Node::new(
                    NodeKind::App,
                    script.id.app.clone(),
                    vec![EvidenceRef::untimed(
                        EvidenceSource::ScriptIndex,
                        format!("app:{}", script.id.app),
                    )],
                )?,
            );
            push_edge(
                &mut data.edges,
                Edge::new(
                    app_ref.id(),
                    script_ref.id(),
                    EdgeKind::Defines,
                    script.at,
                    vec![evidence.clone()],
                )?,
            );

            if let Some(app) = &script.installed_by {
                let installed_by = NodeRef::new(NodeKind::App, app.clone());
                push_node(
                    &mut data.nodes,
                    Node::new(
                        NodeKind::App,
                        app.clone(),
                        vec![EvidenceRef::untimed(
                            EvidenceSource::ScriptIndex,
                            format!("shipped-by:{id}"),
                        )],
                    )?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        script_ref.id(),
                        installed_by.id(),
                        EdgeKind::ChangedBy,
                        script.at,
                        vec![evidence.clone()],
                    )?,
                );
            }

            for field in &script.referenced_fields {
                let dts = EvidenceRef::at(
                    EvidenceSource::DtsCheck,
                    format!("dts:{id}:{field}"),
                    script.at.unwrap_or(0),
                );
                let field_ref = NodeRef::new(NodeKind::Field, field.clone());
                push_node(
                    &mut data.nodes,
                    Node::new(NodeKind::Field, field.clone(), vec![dts.clone()])?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        script_ref.id(),
                        field_ref.id(),
                        EdgeKind::References,
                        script.at,
                        vec![dts],
                    )?,
                );
            }

            if let Some(hook) = &script.hook {
                let step_name = script.step.clone().unwrap_or_else(|| id.clone());
                let step_ref = NodeRef::new(
                    NodeKind::PipelineStep,
                    pipeline_key(&script.doctype, hook, &step_name),
                );
                push_node(
                    &mut data.nodes,
                    Node::new(
                        NodeKind::PipelineStep,
                        step_ref.name.clone(),
                        vec![evidence.clone()],
                    )?
                    .with_attribute("doctype", script.doctype.clone())
                    .with_attribute("hook_point", hook.clone())
                    .with_attribute("step", step_name),
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        script_ref.id(),
                        step_ref.id(),
                        EdgeKind::HooksInto,
                        script.at,
                        vec![evidence.clone()],
                    )?,
                );
                // The step runs the script, so the app that installed the step is also the
                // app that installed the script.
                if let Some(app) = &script.installed_by {
                    push_edge(
                        &mut data.edges,
                        Edge::new(
                            node_id(NodeKind::App, app),
                            step_ref.id(),
                            EdgeKind::Defines,
                            script.at,
                            vec![evidence.clone()],
                        )?,
                    );
                }
            }

            if let Some(at) = script.at {
                data.history.push(HistoryEntry::new(
                    script_ref.id(),
                    at,
                    ChangeKind::Updated,
                    format!("script {id} was indexed"),
                    evidence,
                ));
            }
        }
        Ok(data)
    }
}
