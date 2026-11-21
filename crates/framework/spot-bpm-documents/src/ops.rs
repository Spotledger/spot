//! The Document node handler and the port it runs its operations through.

use std::sync::Arc;

use serde_json::Value as Json;
use spot_bpm_engine::{HandlerRegistry, NodeCtx, NodeHandler, NodeKind, NodeOutcome};

use crate::codes;
use crate::config::NodeConfig;
use crate::failure::DocumentFailure;
use crate::operation::Operation;
use crate::receipt::{DocumentReceipt, DocumentRequest};
use crate::service_principal::ServicePrincipalPolicy;

/// Where a Document node runs its operation.
///
/// The engine's [`NodeHandler`] is synchronous and runs outside any store transaction, so a
/// handler that changes a document does it through a port. [`EngineDocumentOps`](crate::EngineDocumentOps)
/// is the reference port over the document engine's public lifecycle API; a host supplies its own
/// (bound to its runtime and its transaction) in production.
pub trait DocumentOps: Send + Sync {
    /// Runs one operation and returns its receipt.
    ///
    /// # Errors
    /// A [`DocumentFailure`] naming why the operation did not happen; the handler turns it into
    /// the node's error branch.
    fn execute(&self, request: &DocumentRequest) -> Result<DocumentReceipt, DocumentFailure>;
}

/// The [`NodeHandler`] for [`NodeKind::Document`].
///
/// It reads the node's config, evaluates the field mapping with the shared expression language,
/// runs the operation through its [`DocumentOps`] port, and returns the operation's result as the
/// node output (with the opaque receipt inside it). A failure becomes
/// [`NodeOutcome::Fail`](spot_bpm_engine::NodeOutcome::Fail), so the engine retries per the node's
/// policy and then takes the node's error branch.
pub struct DocumentNodeHandler {
    ops: Arc<dyn DocumentOps>,
    service_principals: Option<ServicePrincipalPolicy>,
}

impl DocumentNodeHandler {
    /// A handler running its operations through `ops`.
    #[must_use]
    pub fn new(ops: Arc<dyn DocumentOps>) -> Self {
        Self {
            ops,
            service_principals: None,
        }
    }

    /// Lets nodes that name a `principal` act as the tenant's registered service principals,
    /// under `policy`. Without a policy such a node is refused (fail closed).
    #[must_use]
    pub fn with_service_principals(mut self, policy: ServicePrincipalPolicy) -> Self {
        self.service_principals = Some(policy);
        self
    }

    /// Registers a Document handler in an engine's registry.
    pub fn register(registry: &mut HandlerRegistry, ops: Arc<dyn DocumentOps>) {
        registry.register(Self::new(ops));
    }

    /// Runs one attempt.
    fn execute(&self, ctx: &mut NodeCtx<'_>) -> Result<NodeOutcome, DocumentFailure> {
        let config = NodeConfig::parse(ctx.config, ctx.node.as_str())?;
        if config.operation == Operation::Map {
            return self.map(&config, ctx);
        }
        let fields = config.fields(ctx)?;
        let document = if config.operation.targets_existing() {
            Some(config.target_document(ctx)?)
        } else {
            None
        };
        let expected = config.expected_version(ctx)?;
        // A node-named service principal must be a registered one the version's author may use, and
        // its use is audited before the node acts. The Case's own actor
        // must be a user id: any other text would be turned into a system actor by the port,
        // which the document engine allows to do anything, so it is refused here.
        let principal = match config.principal() {
            Some(named) => {
                ServicePrincipalPolicy::authorise(
                    self.service_principals.as_ref(),
                    named,
                    ctx.case,
                    ctx.node,
                    ctx.now,
                )?;
                Some(named.to_owned())
            }
            None => match ctx.principal {
                Some(actor) if !actor.is_empty() => {
                    if crate::ids::parse_record_id(actor).is_none() {
                        return Err(DocumentFailure::new(
                            codes::PRINCIPAL_INVALID,
                            "the Case's acting principal is not a user id, so the node will not act as the system",
                        ));
                    }
                    Some(actor.to_owned())
                }
                _ => None,
            },
        };
        let request = DocumentRequest {
            operation: config.operation,
            doctype: config.doctype.clone(),
            document,
            fields,
            expected,
            compensation: config.compensation,
            principal,
            case: ctx.case.clone(),
            node: ctx.node.clone(),
            key: ctx.key.to_string(),
            now: ctx.now,
        };
        let receipt = self.ops.execute(&request)?;
        let output = with_compensation(receipt.to_json(), config.compensation);
        ctx.set_variable(&config.output, output.clone());
        let branches = success_branches(ctx)?;
        Ok(NodeOutcome::Done {
            output: Some(output),
            branches,
        })
    }

    /// A `map` operation: evaluates the mapping into the Case's variables, touching no document.
    fn map(
        &self,
        config: &NodeConfig,
        ctx: &mut NodeCtx<'_>,
    ) -> Result<NodeOutcome, DocumentFailure> {
        let mapped = config.mapping_json(ctx)?;
        let mut object = serde_json::Map::new();
        for (field, value) in &mapped {
            object.insert(field.as_str().to_owned(), value.clone());
            ctx.set_variable(
                &format!("{}.{}", config.output, field.as_str()),
                value.clone(),
            );
        }
        let output = Json::Object(object);
        ctx.set_variable(&config.output, output.clone());
        let branches = success_branches(ctx)?;
        Ok(NodeOutcome::Done {
            output: Some(output),
            branches,
        })
    }
}

impl NodeHandler for DocumentNodeHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Document
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        match self.execute(ctx) {
            Ok(outcome) => outcome,
            Err(failure) => NodeOutcome::Fail {
                code: failure.code().to_owned(),
                reason: failure.reason().to_owned(),
            },
        }
    }
}

/// The node's outgoing conditions that hold, in edge order; the error branch is the engine's.
fn success_branches(ctx: &NodeCtx<'_>) -> Result<Vec<String>, DocumentFailure> {
    let mut branches = Vec::new();
    for edge in ctx.outgoing {
        if edge.error {
            continue;
        }
        if let Some(condition) = &edge.condition {
            match spot_bpm_engine::expr::evaluate_bool(condition, ctx.variables, ctx.now) {
                Ok(true) => branches.push(condition.clone()),
                Ok(false) => {}
                Err(detail) => {
                    return Err(DocumentFailure::new(
                        codes::EXPRESSION_INVALID,
                        format!("the condition `{condition}` could not be evaluated: {detail}"),
                    ));
                }
            }
        }
    }
    Ok(branches)
}

/// The node output with the declared compensation added, so it is persisted with the attempt and
/// available to WP-BPM-09's compensation path.
fn with_compensation(mut output: Json, compensation: Option<Operation>) -> Json {
    if let (Some(compensation), Some(object)) = (compensation, output.as_object_mut()) {
        object.insert(
            "compensation".to_owned(),
            Json::String(compensation.token().to_owned()),
        );
    }
    output
}
