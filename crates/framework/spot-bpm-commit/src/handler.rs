//! The Commit node kind handler.
//!
//! A commit point is a node of kind `commit`. The engine's handler contract is synchronous and
//! pure (it never touches storage), so the handler only validates the node and reports it done
//! with a `commit_point` marker output; the release of the held effects is
//! [`CommitService::commit`](crate::CommitService::commit), which the host calls when the node
//! completes. Keeping the two apart means a replayed node (the engine's crash recovery) re-runs a
//! handler with no side effect, and the release itself is idempotent under the hold.

use serde_json::{Value as Json, json};
use spot_bpm_engine::{NodeCtx, NodeHandler, NodeKind, NodeOutcome};

use crate::error::{CommitError, Result};

/// The node kind token a commit point carries.
pub const KIND_COMMIT: &str = "commit";
/// The longest label a commit point may carry.
pub const MAX_LABEL_LEN: usize = 128;

/// The execution kind the handler is registered under.
#[must_use]
pub fn commit_kind() -> NodeKind {
    NodeKind::Unknown(KIND_COMMIT.to_owned())
}

/// Checks a commit node's config: absent, or an object whose only key is an optional printable
/// `label` of at most [`MAX_LABEL_LEN`] characters.
///
/// # Errors
/// [`CommitError::InvalidConfig`] otherwise.
pub fn validate_config(node: &str, config: &Json) -> Result<()> {
    let bad = |detail: &str| CommitError::InvalidConfig {
        node: node.to_owned(),
        detail: detail.to_owned(),
    };
    let object = match config {
        Json::Null => return Ok(()),
        Json::Object(object) => object,
        _ => return Err(bad("the config must be an object")),
    };
    for (key, value) in object {
        if key != "label" {
            return Err(bad("only `label` is allowed"));
        }
        match value.as_str() {
            Some(label)
                if label.chars().count() <= MAX_LABEL_LEN
                    && !label.chars().any(char::is_control) => {}
            _ => return Err(bad("`label` must be short printable text")),
        }
    }
    Ok(())
}

/// The handler for the Commit node kind.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommitHandler;

impl NodeHandler for CommitHandler {
    fn kind(&self) -> NodeKind {
        commit_kind()
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        match validate_config(ctx.node.as_str(), ctx.config) {
            Ok(()) => NodeOutcome::Done {
                output: Some(json!({ "commit_point": true })),
                branches: ctx.outgoing_conditions(),
            },
            Err(error) => NodeOutcome::fail(error.code(), "the commit point's config is invalid"),
        }
    }
}
