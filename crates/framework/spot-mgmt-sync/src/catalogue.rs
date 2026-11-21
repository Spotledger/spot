//! The catalogue: the eighteen `sync.*` operations, as data.
//!
//! Every range, enum and pattern lives here, not in the executor: the console builds its
//! screens and the CLI its flags from these specs, and [`crate::dispatch`] only maps the
//! validated result onto typed arguments. All operations target one tenant. No operation
//! takes a path, a URL, a shell fragment or a tenant, and none has a secret parameter: the
//! enrolment code is in the result, not the invocation.
//!
//! The three operations that destroy something have a required `confirm` parameter equal to
//! their target: `sync.node.revoke` (the node), `sync.quarantine.discard` (the item) and
//! `sync.role.change` (the new role).

use spot_mgmt_ops::{
    Catalogue, CatalogueBuilder, CatalogueError, OperationModule, OperationSpec, ParamSpec,
    ParamType, ParamValue, Risk, TargetKind,
};

use crate::args::{NodeRole, Position};
use crate::ids;

/// The module's name, used in catalogue errors.
pub const MODULE_NAME: &str = "spot-mgmt-sync";

/// A node's name: a host name, lower case.
pub const NODE_PATTERN: &str = "[a-z0-9][a-z0-9.-]{0,127}";
/// A quarantined item's id or a subscription's name or id: an identifier, never a path.
const ITEM_PATTERN: &str = "[A-Za-z0-9][A-Za-z0-9._:-]{0,127}";
/// A partition's name.
const NAME_PATTERN: &str = "[a-z][a-z0-9_]{0,63}";
/// A doctype's name.
const DOCTYPE_PATTERN: &str = "[A-Za-z][A-Za-z0-9_.-]{0,63}";
/// A reason code.
const REASON_PATTERN: &str = "[a-z][a-z0-9_]{0,63}";

/// The longest a one-time code stays valid, in hours (one week).
pub const MAX_VALID_HOURS: i64 = 168;
/// The default validity of a one-time code, in hours.
pub const DEFAULT_VALID_HOURS: i64 = 24;

fn param(
    name: &str,
    ty: ParamType,
    required: bool,
    default: Option<ParamValue>,
    doc: &str,
) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty,
        required,
        default,
        doc: doc.to_owned(),
        secret: false,
    }
}

fn text_type(max_len: usize, pattern: &str) -> ParamType {
    ParamType::Text {
        max_len,
        pattern: Some(pattern.to_owned()),
    }
}

fn text(name: &str, max_len: usize, pattern: &str, doc: &str) -> ParamSpec {
    param(name, text_type(max_len, pattern), true, None, doc)
}

fn choice(names: &[&str]) -> ParamType {
    ParamType::Enum(names.iter().map(|n| (*n).to_owned()).collect())
}

fn spec(id: &str, title: &str, summary: &str, risk: Risk, params: Vec<ParamSpec>) -> OperationSpec {
    OperationSpec {
        // Unreachable failure: every id is proven valid by `ids::tests`.
        id: ids::op_id(id).unwrap_or_else(|error| panic!("invalid operation id {id}: {error}")),
        title: title.to_owned(),
        summary: summary.to_owned(),
        params,
        risk,
        target: TargetKind::Tenant,
    }
}

fn node_param(doc: &str) -> ParamSpec {
    text("node", 128, NODE_PATTERN, doc)
}

fn confirm_param(doc: &str) -> ParamSpec {
    text("confirm", 128, ITEM_PATTERN, doc)
}

/// A read-only operation without parameters.
fn read(id: &str, title: &str, summary: &str) -> OperationSpec {
    spec(id, title, summary, Risk::Read, Vec::new())
}

/// An operation that names one node and nothing else.
fn on_node(id: &str, title: &str, summary: &str, risk: Risk) -> OperationSpec {
    spec(id, title, summary, risk, vec![node_param("The node.")])
}

/// Every operation this crate registers, in a stable order.
#[must_use]
pub fn specs() -> Vec<OperationSpec> {
    vec![
        read(
            ids::HEALTH,
            "Sync health",
            "Read the sync health: calm or not, what needs a person, and each node's lag.",
        ),
        read(
            ids::NODE_LIST,
            "List nodes",
            "List the tenant's sync nodes with their role, state and last contact.",
        ),
        spec(
            ids::NODE_SHOW,
            "Show a node",
            "Show one node: partitions, subscription, certificate expiry and lag.",
            Risk::Read,
            vec![node_param("The node to show.")],
        ),
        spec(
            ids::NODE_ENROL,
            "Enrol a node",
            "Enrol an edge node. The one-time enrolment code is shown once, in the result; it is never part of the command.",
            Risk::High,
            vec![
                node_param("The new node's name."),
                param(
                    "role",
                    choice(&[NodeRole::Edge.name()]),
                    false,
                    Some(ParamValue::Text(NodeRole::Edge.name().to_owned())),
                    "The node's role; only edge nodes are enrolled.",
                ),
                param(
                    "partitions",
                    ParamType::List(Box::new(text_type(64, NAME_PATTERN))),
                    true,
                    None,
                    "The partitions the node receives, one flag each.",
                ),
                text(
                    "subscription",
                    128,
                    ITEM_PATTERN,
                    "The name of a subscription preset or the id of a stored subscription.",
                ),
                param(
                    "valid_hours",
                    ParamType::Int {
                        min: 1,
                        max: MAX_VALID_HOURS,
                    },
                    false,
                    Some(ParamValue::Int(DEFAULT_VALID_HOURS)),
                    "How many hours the one-time code stays valid (1 to 168).",
                ),
            ],
        ),
        on_node(
            ids::NODE_SUSPEND,
            "Suspend a node",
            "Stop a node from syncing until it is resumed. Its data is kept.",
            Risk::High,
        ),
        on_node(
            ids::NODE_RESUME,
            "Resume a node",
            "Let a suspended node sync again; it catches up from where it stopped.",
            Risk::Medium,
        ),
        spec(
            ids::NODE_REVOKE,
            "Revoke a node",
            "Revoke a node's membership for good: its certificate is refused and it must be enrolled again. Cannot be undone.",
            Risk::Critical,
            vec![
                node_param("The node to revoke."),
                confirm_param("Must equal the node's name; this is irreversible."),
            ],
        ),
        on_node(
            ids::NODE_ROTATE_CERTIFICATE,
            "Rotate a node's certificate",
            "Issue the node a new certificate before the old one expires.",
            Risk::Medium,
        ),
        spec(
            ids::NODE_RESEND,
            "Resend to a node",
            "Resend the stream to a node, from a position or from its last acknowledged one.",
            Risk::Medium,
            vec![
                node_param("The node to resend to."),
                param(
                    "from",
                    text_type(32, Position::PATTERN),
                    false,
                    None,
                    "The position to resend from, as generation:seq; omit to resume after the last acknowledged one.",
                ),
            ],
        ),
        on_node(
            ids::NODE_NOTIFY_CONTACT,
            "Notify a node's contact",
            "Tell the node's contact person that the node needs attention.",
            Risk::Low,
        ),
        read(
            ids::CONDITION_LIST,
            "List sync conditions",
            "List the open sync conditions, worst first.",
        ),
        read(
            ids::QUARANTINE_LIST,
            "List quarantined items",
            "List the quarantined items with their reason codes. Shows no record content.",
        ),
        spec(
            ids::QUARANTINE_RETRY,
            "Retry a quarantined item",
            "Try to apply a quarantined item again.",
            Risk::Medium,
            vec![text(
                "item",
                128,
                ITEM_PATTERN,
                "The quarantined item's id.",
            )],
        ),
        spec(
            ids::QUARANTINE_DISCARD,
            "Discard a quarantined item",
            "Discard a quarantined item without applying it; the reason is recorded. Cannot be undone.",
            Risk::High,
            vec![
                text("item", 128, ITEM_PATTERN, "The quarantined item's id."),
                text(
                    "reason_code",
                    64,
                    REASON_PATTERN,
                    "Why it is discarded, as a short snake_case code.",
                ),
                confirm_param("Must equal the item's id; this is irreversible."),
            ],
        ),
        spec(
            ids::POLICY_SHOW,
            "Show a sync policy",
            "Show the sync preset of a doctype.",
            Risk::Read,
            vec![text(
                "doctype",
                64,
                DOCTYPE_PATTERN,
                "The doctype whose policy is shown.",
            )],
        ),
        read(
            ids::POLICY_LINT,
            "Lint sync policies",
            "Check every sync policy against the lint rules and list the findings.",
        ),
        spec(
            ids::ROLE_CHANGE,
            "Change this site's role",
            "Change this site between hub and edge. The site must be stopped; this changes who is authoritative.",
            Risk::Critical,
            vec![
                param(
                    "to",
                    choice(&NodeRole::NAMES),
                    true,
                    None,
                    "The role to change to: edge or hub.",
                ),
                confirm_param("Must equal the role named in `to`."),
            ],
        ),
        read(
            ids::BOOTSTRAP_STATUS,
            "Bootstrap status",
            "Read how far the initial bootstrap of this node has got.",
        ),
    ]
}

/// The module that registers the sync operations on the catalogue builder.
#[derive(Clone, Copy, Debug, Default)]
pub struct SyncModule;

impl OperationModule for SyncModule {
    fn name(&self) -> &str {
        MODULE_NAME
    }

    fn register(&self, builder: &mut CatalogueBuilder) {
        for operation in specs() {
            builder.add(MODULE_NAME, operation);
        }
    }
}

/// Builds the catalogue from just the sync module (the host merges it with the others').
///
/// # Errors
/// Every catalogue problem the specs have; a test asserts there are none.
pub fn build_catalogue() -> Result<Catalogue, Vec<CatalogueError>> {
    let mut builder = CatalogueBuilder::new();
    SyncModule.register(&mut builder);
    builder.build()
}
