//! Permission source: the permission snapshot summary and assignment expiry records.

use spot_permissions::{FieldTarget, Snapshot};

use crate::SelfModelError;
use crate::adapters::{push_edge, push_node};
use crate::evidence::{ChangeKind, EvidenceRef, EvidenceSource, HistoryEntry};
use crate::kind::{EdgeKind, NodeKind, SourceKind};
use crate::model::{Edge, Node, NodeRef, SourceAdapter, SourceData, node_id};

/// One permission or assignment change that the snapshot alone cannot time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentRecord {
    /// The role.
    pub role: String,
    /// The DocType.
    pub doctype: String,
    /// The field, when the permission is on a field.
    pub field: Option<String>,
    /// The right's text form.
    pub right: String,
    /// The principal whose assignment changed.
    pub principal: String,
    /// Unix microseconds of the change.
    pub at: i64,
    /// Unix microseconds the assignment expires, when it does.
    pub expires_at: Option<i64>,
    /// What kind of change it was (`granted`, `expired`, `revoked`).
    pub change: ChangeKind,
    /// The proving record.
    pub evidence: EvidenceRef,
}

/// Builds role, duty, permission and field nodes from `spot-permissions`'s [`Snapshot`].
#[derive(Clone, Debug)]
pub struct PermissionsAdapter {
    snapshot: Snapshot,
    assignments: Vec<AssignmentRecord>,
}

impl PermissionsAdapter {
    /// An adapter over one permission snapshot.
    #[must_use]
    pub fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            assignments: Vec::new(),
        }
    }

    /// Adds one timed assignment change.
    #[must_use]
    pub fn with_assignment(mut self, assignment: AssignmentRecord) -> Self {
        self.assignments.push(assignment);
        self
    }
}

impl SourceAdapter for PermissionsAdapter {
    fn source(&self) -> SourceKind {
        SourceKind::Permissions
    }

    fn epoch(&self) -> u64 {
        u64::try_from(self.snapshot.grants().len())
            .unwrap_or(u64::MAX)
            .saturating_add(u64::try_from(self.snapshot.field_grants().len()).unwrap_or(u64::MAX))
    }

    fn snapshot(&self) -> Result<SourceData, SelfModelError> {
        let mut data = SourceData::new();
        for grant in self.snapshot.grants() {
            let role = grant.role.as_str();
            let evidence = EvidenceRef::untimed(
                EvidenceSource::PermissionSnapshot,
                format!("grant:{role}:{}", grant.doctype),
            );
            push_node(&mut data.nodes, role_node(role, &evidence)?);
            push_node(
                &mut data.nodes,
                Node::new(
                    NodeKind::DocType,
                    grant.doctype.clone(),
                    vec![evidence.clone()],
                )?,
            );
            for right in grant.rights.as_slice() {
                let permission =
                    permission_node(role, &grant.doctype, None, right.as_str(), &evidence)?;
                let permission_ref = permission.reference();
                push_node(&mut data.nodes, permission);
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        permission_ref.id(),
                        node_id(NodeKind::Role, role),
                        EdgeKind::GrantedBy,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        permission_ref.id(),
                        node_id(NodeKind::DocType, &grant.doctype),
                        EdgeKind::Governs,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
            }
        }

        for grant in self.snapshot.field_grants() {
            let role = grant.role.as_str();
            let evidence = EvidenceRef::untimed(
                EvidenceSource::PermissionSnapshot,
                format!("field-grant:{role}:{}", grant.doctype),
            );
            push_node(&mut data.nodes, role_node(role, &evidence)?);
            let (target, field_part) = match &grant.target {
                FieldTarget::Field(path) => {
                    let (node_name, field) = qualify_field(&grant.doctype, path);
                    (NodeRef::new(NodeKind::Field, node_name), Some(field))
                }
                FieldTarget::Group(group) => (
                    NodeRef::new(NodeKind::Duty, format!("group:{}", group.0)),
                    None,
                ),
            };
            push_node(
                &mut data.nodes,
                Node::new(target.kind, target.name.clone(), vec![evidence.clone()])?,
            );
            for right in &grant.access {
                let permission = match &grant.target {
                    FieldTarget::Field(_) => permission_node(
                        role,
                        &grant.doctype,
                        field_part.as_deref(),
                        right.as_str(),
                        &evidence,
                    )?,
                    FieldTarget::Group(group) => group_permission_node(
                        role,
                        &grant.doctype,
                        &group.0,
                        right.as_str(),
                        &evidence,
                    )?,
                };
                let permission_ref = permission.reference();
                push_node(&mut data.nodes, permission);
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        permission_ref.id(),
                        node_id(NodeKind::Role, role),
                        EdgeKind::GrantedBy,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
                push_edge(
                    &mut data.edges,
                    Edge::new(
                        permission_ref.id(),
                        target.id(),
                        EdgeKind::Governs,
                        None,
                        vec![evidence.clone()],
                    )?,
                );
            }
        }

        for assignment in &self.assignments {
            if !assignment.evidence.is_complete() {
                return Err(SelfModelError::EvidenceMissing {
                    what: format!(
                        "assignment:{}:{}:{}",
                        assignment.principal, assignment.role, assignment.right
                    ),
                });
            }
            let permission_ref = crate::event::permission_ref(
                &assignment.role,
                &assignment.doctype,
                assignment.field.as_deref(),
                &assignment.right,
            );
            let evidence = assignment.evidence.clone();
            let mut permission = Node::new(
                NodeKind::Permission,
                permission_ref.name.clone(),
                vec![evidence.clone()],
            )?
            .with_attribute("role", assignment.role.clone())
            .with_attribute("doctype", assignment.doctype.clone())
            .with_attribute("right", assignment.right.clone());
            if let Some(field) = &assignment.field {
                permission = permission.with_attribute("field", field.clone());
            }
            push_node(&mut data.nodes, permission);
            let target = match &assignment.field {
                Some(field) => format!("{}.{field}", assignment.doctype),
                None => assignment.doctype.clone(),
            };
            let duty_name = format!(
                "assignment:{}:{}:{target}:{}",
                assignment.principal, assignment.role, assignment.right
            );
            let duty_ref = NodeRef::new(NodeKind::Duty, duty_name.clone());
            let mut duty = Node::new(NodeKind::Duty, duty_name, vec![evidence.clone()])?
                .with_attribute("principal", assignment.principal.clone())
                .with_attribute("role", assignment.role.clone())
                .with_attribute("status", assignment.change.as_str());
            if let Some(expires_at) = assignment.expires_at {
                duty = duty.with_attribute("expires_at", expires_at.to_string());
            }
            push_node(&mut data.nodes, duty);
            push_edge(
                &mut data.edges,
                Edge::new(
                    duty_ref.id(),
                    permission_ref.id(),
                    EdgeKind::Affects,
                    Some(assignment.at),
                    vec![evidence.clone()],
                )?,
            );
            data.history.push(HistoryEntry::new(
                permission_ref.id(),
                assignment.at,
                assignment.change,
                format!(
                    "the assignment for {} was {}",
                    assignment.principal,
                    assignment.change.as_str()
                ),
                assignment.evidence.clone(),
            ));
        }

        Ok(data)
    }
}

fn role_node(role: &str, evidence: &EvidenceRef) -> Result<Node, SelfModelError> {
    Node::new(
        NodeKind::Role,
        role.to_owned(),
        vec![EvidenceRef::untimed(
            evidence.source,
            format!("role:{role}"),
        )],
    )
}

/// The Field node name and the field part of the permission key.
///
/// A grant may name a field as `total` or as `sales_invoice.total`; the Field node is always
/// qualified (`sales_invoice.total`) and the permission key always carries the bare path,
/// matching [`crate::event::permission_ref`].
fn qualify_field(doctype: &str, path: &str) -> (String, String) {
    let prefix = format!("{doctype}.");
    match path.strip_prefix(&prefix) {
        Some(field) => (path.to_owned(), field.to_owned()),
        None => (format!("{doctype}.{path}"), path.to_owned()),
    }
}

fn permission_node(
    role: &str,
    doctype: &str,
    field: Option<&str>,
    right: &str,
    evidence: &EvidenceRef,
) -> Result<Node, SelfModelError> {
    let reference = crate::event::permission_ref(role, doctype, field, right);
    let mut node = Node::new(NodeKind::Permission, reference.name, vec![evidence.clone()])?
        .with_attribute("role", role.to_owned())
        .with_attribute("doctype", doctype.to_owned())
        .with_attribute("right", right.to_owned());
    if let Some(field) = field {
        node = node.with_attribute("field", field.to_owned());
    }
    Ok(node)
}

/// A permission that applies through a protection group, distinct from the DocType-level
/// permission of the same role and right.
fn group_permission_node(
    role: &str,
    doctype: &str,
    group: &str,
    right: &str,
    evidence: &EvidenceRef,
) -> Result<Node, SelfModelError> {
    Ok(Node::new(
        NodeKind::Permission,
        format!("{role}:{doctype}:group:{group}:{right}"),
        vec![evidence.clone()],
    )?
    .with_attribute("role", role.to_owned())
    .with_attribute("doctype", doctype.to_owned())
    .with_attribute("group", group.to_owned())
    .with_attribute("right", right.to_owned()))
}
