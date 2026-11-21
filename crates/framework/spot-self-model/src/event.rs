//! Incremental system events (`apply_event`), the vocabulary WP-AI-02 consumes.
//!
//! T1 defines the event enum. Every event carries the cause that produced it and the evidence
//! record that proves it, so an applied event can explain itself exactly like a source-built
//! node can.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::SelfModelError;
use crate::evidence::{ChangeKind, EvidenceRef, HistoryEntry};
use crate::kind::{EdgeKind, NodeKind};
use crate::model::{Edge, Node, NodeRef, SelfModel};

/// One system event that moves the self-model forward.
///
/// `at` is unix microseconds supplied by the caller; the crate reads no clock.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemEvent {
    /// A node was added by a source upgrade or a customisation.
    NodeAdded {
        /// The node that appeared.
        node: NodeRef,
        /// Non-sensitive attributes it starts with.
        attributes: BTreeMap<String, String>,
        /// The cause that added it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A node disappeared.
    NodeRemoved {
        /// The node that disappeared.
        node: NodeRef,
        /// The cause that removed it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A node's attribute changed.
    NodeChanged {
        /// The node that changed.
        node: NodeRef,
        /// The change, in one non-sensitive sentence.
        change: String,
        /// The cause that changed it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// An app was installed or upgraded.
    AppInstalled {
        /// The app's name.
        app: String,
        /// The installed version.
        version: String,
        /// The migration or install run that installed it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// An app was uninstalled.
    AppUninstalled {
        /// The app's name.
        app: String,
        /// The cause that removed it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A seam was provided or replaced.
    SeamChanged {
        /// The seam name, as `<app>:<seam>`.
        seam: String,
        /// Its major version.
        version: u32,
        /// The change, in one non-sensitive sentence.
        change: String,
        /// The cause that changed it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A pipeline step's plan or order changed.
    PipelineStepChanged {
        /// The DocType whose hook runs the step.
        doctype: String,
        /// The hook point's name.
        hook_point: String,
        /// The step's name.
        step: String,
        /// The change, in one non-sensitive sentence.
        change: String,
        /// The cause that changed it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A migration run was recorded.
    MigrationRecorded {
        /// The run id.
        run: String,
        /// Its outcome.
        outcome: String,
        /// The change, in one non-sensitive sentence.
        change: String,
        /// The cause that recorded it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A patch was applied.
    PatchApplied {
        /// The patch id.
        patch: String,
        /// The app that declares it.
        app: String,
        /// What it does, in one sentence.
        intent: String,
        /// The migration run that applied it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A permission or assignment changed.
    PermissionChanged {
        /// The role.
        role: String,
        /// The DocType.
        doctype: String,
        /// The field, when the permission is on a field.
        field: Option<String>,
        /// The right's text form.
        right: String,
        /// What kind of change it was.
        change: ChangeKind,
        /// The cause that changed it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A job failed.
    JobFailed {
        /// The job's name.
        job: String,
        /// Why it failed, in one non-sensitive sentence.
        reason: String,
        /// The setting, migration or signal that caused it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A setting changed.
    SettingChanged {
        /// The setting key.
        key: String,
        /// Its scope's canonical text.
        scope: String,
        /// The change, in one non-sensitive sentence.
        change: String,
        /// The cause that changed it.
        cause: NodeRef,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
    /// A runtime health signal was observed.
    HealthChanged {
        /// The signal's name.
        signal: String,
        /// Its status (`healthy`, `degraded`, `failing`).
        status: String,
        /// The node it observed.
        target: NodeRef,
        /// The setting or change that caused it, when known.
        cause: Option<NodeRef>,
        /// Unix microseconds.
        at: i64,
        /// The proving record.
        evidence: EvidenceRef,
    },
}

impl SystemEvent {
    /// The event's subject, the node it is about.
    #[must_use]
    pub fn subject(&self) -> NodeRef {
        match self {
            Self::NodeAdded { node, .. }
            | Self::NodeRemoved { node, .. }
            | Self::NodeChanged { node, .. } => node.clone(),
            Self::AppInstalled { app, .. } | Self::AppUninstalled { app, .. } => {
                NodeRef::new(NodeKind::App, app)
            }
            Self::SeamChanged { seam, version, .. } => {
                NodeRef::new(NodeKind::Seam, format!("{seam}@{version}"))
            }
            Self::PipelineStepChanged {
                doctype,
                hook_point,
                step,
                ..
            } => pipeline_step_ref(doctype, hook_point, step),
            Self::MigrationRecorded { run, .. } => NodeRef::new(NodeKind::Migration, run),
            Self::PatchApplied { patch, .. } => NodeRef::new(NodeKind::Patch, patch),
            Self::PermissionChanged {
                role,
                doctype,
                field,
                right,
                ..
            } => permission_ref(role, doctype, field.as_deref(), right),
            Self::JobFailed { job, .. } => NodeRef::new(NodeKind::Job, job),
            Self::SettingChanged { key, .. } => NodeRef::new(NodeKind::Setting, key),
            Self::HealthChanged { signal, .. } => NodeRef::new(NodeKind::HealthSignal, signal),
        }
    }

    /// Unix microseconds the event happened.
    #[must_use]
    pub fn at(&self) -> i64 {
        match self {
            Self::NodeAdded { at, .. }
            | Self::NodeRemoved { at, .. }
            | Self::NodeChanged { at, .. }
            | Self::AppInstalled { at, .. }
            | Self::AppUninstalled { at, .. }
            | Self::SeamChanged { at, .. }
            | Self::PipelineStepChanged { at, .. }
            | Self::MigrationRecorded { at, .. }
            | Self::PatchApplied { at, .. }
            | Self::PermissionChanged { at, .. }
            | Self::JobFailed { at, .. }
            | Self::SettingChanged { at, .. }
            | Self::HealthChanged { at, .. } => *at,
        }
    }

    /// The proving record.
    #[must_use]
    pub fn evidence(&self) -> &EvidenceRef {
        match self {
            Self::NodeAdded { evidence, .. }
            | Self::NodeRemoved { evidence, .. }
            | Self::NodeChanged { evidence, .. }
            | Self::AppInstalled { evidence, .. }
            | Self::AppUninstalled { evidence, .. }
            | Self::SeamChanged { evidence, .. }
            | Self::PipelineStepChanged { evidence, .. }
            | Self::MigrationRecorded { evidence, .. }
            | Self::PatchApplied { evidence, .. }
            | Self::PermissionChanged { evidence, .. }
            | Self::JobFailed { evidence, .. }
            | Self::SettingChanged { evidence, .. }
            | Self::HealthChanged { evidence, .. } => evidence,
        }
    }
}

/// The canonical reference of a pipeline step.
#[must_use]
pub fn pipeline_step_ref(doctype: &str, hook_point: &str, step: &str) -> NodeRef {
    NodeRef::new(
        NodeKind::PipelineStep,
        format!("{doctype}.{hook_point}.{step}"),
    )
}

/// The canonical reference of a permission.
#[must_use]
pub fn permission_ref(role: &str, doctype: &str, field: Option<&str>, right: &str) -> NodeRef {
    let target = match field {
        Some(field) => format!("{doctype}.{field}"),
        None => doctype.to_owned(),
    };
    NodeRef::new(NodeKind::Permission, format!("{role}:{target}:{right}"))
}

pub(crate) fn apply(model: &mut SelfModel, event: SystemEvent) -> Result<u64, SelfModelError> {
    if !event.evidence().is_complete() {
        return Err(SelfModelError::EventInvalid {
            reason: "the event carries no evidence record".to_owned(),
        });
    }
    let at = event.at();
    let evidence = event.evidence().clone();
    match event {
        SystemEvent::NodeAdded {
            node,
            attributes,
            cause,
            ..
        } => {
            ensure_node(model, &node, at, &evidence, &attributes)?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            history(
                model,
                &node,
                at,
                ChangeKind::Created,
                format!("{} was added", node.key()),
                evidence,
            );
        }
        SystemEvent::NodeRemoved { node, cause, .. } => {
            ensure_node(model, &node, at, &evidence, &BTreeMap::new())?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            model.set_attribute(&node.id(), "removed", "true");
            history(
                model,
                &node,
                at,
                ChangeKind::Removed,
                format!("{} was removed", node.key()),
                evidence,
            );
        }
        SystemEvent::NodeChanged {
            node,
            change,
            cause,
            ..
        } => {
            ensure_node(model, &node, at, &evidence, &BTreeMap::new())?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            model.set_attribute(&node.id(), "last_change", change.clone());
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            history(model, &node, at, ChangeKind::Updated, change, evidence);
        }
        SystemEvent::AppInstalled {
            app,
            version,
            cause,
            ..
        } => {
            let node = NodeRef::new(NodeKind::App, app);
            let attributes = BTreeMap::from([("version".to_owned(), version)]);
            ensure_node(model, &node, at, &evidence, &attributes)?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::InstalledBy, &evidence, at)?;
            history(
                model,
                &node,
                at,
                ChangeKind::Promoted,
                "the app was installed".to_owned(),
                evidence,
            );
        }
        SystemEvent::AppUninstalled { app, cause, .. } => {
            let node = NodeRef::new(NodeKind::App, app);
            ensure_node(model, &node, at, &evidence, &BTreeMap::new())?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            model.set_attribute(&node.id(), "removed", "true");
            history(
                model,
                &node,
                at,
                ChangeKind::Removed,
                "the app was uninstalled".to_owned(),
                evidence,
            );
        }
        SystemEvent::SeamChanged {
            seam,
            version,
            change,
            cause,
            ..
        } => {
            let node = NodeRef::new(NodeKind::Seam, format!("{seam}@{version}"));
            let attributes = BTreeMap::from([
                ("seam".to_owned(), seam),
                ("version".to_owned(), version.to_string()),
            ]);
            ensure_node(model, &node, at, &evidence, &attributes)?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            history(model, &node, at, ChangeKind::Updated, change, evidence);
        }
        SystemEvent::PipelineStepChanged {
            doctype,
            hook_point,
            step,
            change,
            cause,
            ..
        } => {
            let node = pipeline_step_ref(&doctype, &hook_point, &step);
            let attributes = BTreeMap::from([
                ("doctype".to_owned(), doctype),
                ("hook_point".to_owned(), hook_point),
                ("step".to_owned(), step),
            ]);
            ensure_node(model, &node, at, &evidence, &attributes)?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            history(model, &node, at, ChangeKind::Updated, change, evidence);
        }
        SystemEvent::MigrationRecorded {
            run,
            outcome,
            change,
            cause,
            ..
        } => {
            let node = NodeRef::new(NodeKind::Migration, run);
            let attributes = BTreeMap::from([("outcome".to_owned(), outcome)]);
            ensure_node(model, &node, at, &evidence, &attributes)?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            history(model, &node, at, ChangeKind::Observed, change, evidence);
        }
        SystemEvent::PatchApplied {
            patch,
            app,
            intent,
            cause,
            ..
        } => {
            let node = NodeRef::new(NodeKind::Patch, patch);
            let attributes = BTreeMap::from([
                ("app".to_owned(), app),
                ("intent".to_owned(), intent.clone()),
            ]);
            ensure_node(model, &node, at, &evidence, &attributes)?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            history(model, &node, at, ChangeKind::Updated, intent, evidence);
        }
        SystemEvent::PermissionChanged {
            role,
            doctype,
            field,
            right,
            change,
            cause,
            ..
        } => {
            let node = permission_ref(&role, &doctype, field.as_deref(), &right);
            let mut attributes = BTreeMap::from([
                ("role".to_owned(), role),
                ("doctype".to_owned(), doctype),
                ("right".to_owned(), right),
            ]);
            if let Some(field) = field {
                attributes.insert("field".to_owned(), field);
            }
            ensure_node(model, &node, at, &evidence, &attributes)?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            history(
                model,
                &node,
                at,
                change,
                format!("the permission changed: {}", change.as_str()),
                evidence,
            );
        }
        SystemEvent::JobFailed {
            job, reason, cause, ..
        } => {
            let node = NodeRef::new(NodeKind::Job, job);
            ensure_node(model, &node, at, &evidence, &BTreeMap::new())?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            history(model, &node, at, ChangeKind::Failed, reason, evidence);
        }
        SystemEvent::SettingChanged {
            key,
            scope,
            change,
            cause,
            ..
        } => {
            let node = NodeRef::new(NodeKind::Setting, key);
            let attributes = BTreeMap::from([("scope".to_owned(), scope)]);
            ensure_node(model, &node, at, &evidence, &attributes)?;
            ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            history(model, &node, at, ChangeKind::Updated, change, evidence);
        }
        SystemEvent::HealthChanged {
            signal,
            status,
            target,
            cause,
            ..
        } => {
            let node = NodeRef::new(NodeKind::HealthSignal, signal);
            let attributes = BTreeMap::from([("status".to_owned(), status.clone())]);
            ensure_node(model, &node, at, &evidence, &attributes)?;
            ensure_node(model, &target, at, &evidence, &BTreeMap::new())?;
            cause_edge(model, &node, &target, EdgeKind::ObservedOn, &evidence, at)?;
            if let Some(cause) = cause {
                ensure_node(model, &cause, at, &evidence, &BTreeMap::new())?;
                cause_edge(model, &node, &cause, EdgeKind::ChangedBy, &evidence, at)?;
            }
            let change = match status.as_str() {
                "healthy" | "green" => ChangeKind::Recovered,
                "degraded" | "amber" => ChangeKind::Observed,
                _ => ChangeKind::Failed,
            };
            history(
                model,
                &node,
                at,
                change,
                format!("the health signal reported {status}"),
                evidence,
            );
        }
    }
    Ok(model.next_epoch())
}

/// Ensures the node exists, merging attributes the snapshot does not already hold.
fn ensure_node(
    model: &mut SelfModel,
    reference: &NodeRef,
    _at: i64,
    evidence: &EvidenceRef,
    attributes: &BTreeMap<String, String>,
) -> Result<(), SelfModelError> {
    let mut node = Node::new(
        reference.kind,
        reference.name.clone(),
        vec![evidence.clone()],
    )?;
    for (key, value) in attributes {
        node = node.with_attribute(key.clone(), value.clone());
    }
    model.ensure_node(node);
    Ok(())
}

/// Adds a directed edge and rejects a relation whose endpoints are absent.
fn cause_edge(
    model: &mut SelfModel,
    from: &NodeRef,
    to: &NodeRef,
    kind: EdgeKind,
    evidence: &EvidenceRef,
    at: i64,
) -> Result<(), SelfModelError> {
    let edge = Edge::new(from.id(), to.id(), kind, Some(at), vec![evidence.clone()])?;
    model.insert_edge(edge);
    Ok(())
}

fn history(
    model: &mut SelfModel,
    node: &NodeRef,
    at: i64,
    change: ChangeKind,
    detail: String,
    evidence: EvidenceRef,
) {
    model.push_history(HistoryEntry::new(node.id(), at, change, detail, evidence));
}
