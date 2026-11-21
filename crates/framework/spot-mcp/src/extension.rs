//! Extension tools: tools a hosting service adds to `tools/list` (contract `mcp.md`, section 5).
//!
//! The generated tools come from metadata; an **extension tool** comes from a service that
//! lives in the host (retrieval, agent memory, the remedy host). The seam is one trait,
//! [`ExtensionTool`], and one registry, [`ExtensionRegistry`], that the host installs tools into
//! and the [`Server`](crate::server::Server) reads on every request, so a service that stops can
//! uninstall its tool and the tool is gone from the next `tools/list` with no restart.
//!
//! # The rule (contract 0.27.0, section 5, "Extension tools")
//!
//! - **Name.** Lower snake case, 1 to 64 characters, never `query` or `job_status`, never
//!   beginning with a generated prefix (`search_`, `get_`, `create_`, `update_`, `call_`). A name
//!   that breaks the rule, or that is already installed, is refused at install time
//!   ([`InstallError`]); a collision is never resolved by a suffix.
//! - **Listing.** [`ExtensionTool::spec`] answers `None` for a principal that may not call the
//!   tool, and then it is not listed and a call is the unknown-tool refusal of any other name.
//! - **Identity.** [`ExtensionTool::call`] receives the calling principal and nothing else about
//!   the request: no raw message, no credential, no other principal.
//! - **Limits and audit.** The server applies its own rate limit, argument schema check,
//!   deadline and audit record around the call exactly as for a generated tool. The audit record
//!   names the tool and never carries an argument value. A tool that [`writes`](ExtensionSpec::writes)
//!   is write-capable: its intent is recorded before it runs and a failed audit write refuses it.
//! - **Failure.** A tool that returns an error, panics or never returns becomes a tool error
//!   (the error carries the service's own code, a fixed sentence and no data), a deadline error
//!   or an internal error. It never becomes a transport failure and never poisons the server.
//!
//! The tier gate of a generated write tool (a write needs T2) is **not** applied to an extension
//! tool: an extension gates itself, by listing only for an agent whose tier allows it
//! ([`ExtensionTool::spec`]) and by having its service check the tier again when it runs.

use std::collections::BTreeMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll};

use serde_json::Value as Json;

use crate::BoxFuture;
use crate::error::{INTERNAL_ERROR, INVALID_PARAMS, McpError};
use crate::principal::McpPrincipal;

/// The longest extension tool name.
pub const MAX_NAME_LEN: usize = 64;

/// Name prefixes the generated tools use; an extension name never begins with one.
pub const GENERATED_PREFIXES: [&str; 5] = ["search_", "get_", "create_", "update_", "call_"];

/// Names of generated tools that have no prefix; an extension never takes one.
pub const GENERATED_NAMES: [&str; 2] = ["query", "job_status"];

/// What a tool looks like in `tools/list` for one principal.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionSpec {
    /// A human-readable title; the tool's name when absent.
    pub title: Option<String>,
    /// What the tool does, for the model reading the list. Never contains data.
    pub description: String,
    /// The JSON Schema of the arguments. The server validates every call against it.
    pub input_schema: Json,
    /// Whether the tool can change data (the audit rule of a write-capable tool applies).
    pub writes: bool,
}

impl ExtensionSpec {
    /// A read-only tool with no title.
    #[must_use]
    pub fn new(description: impl Into<String>, input_schema: Json) -> Self {
        Self {
            title: None,
            description: description.into(),
            input_schema,
            writes: false,
        }
    }

    /// Gives the tool a title.
    #[must_use]
    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Marks the tool write-capable.
    #[must_use]
    pub fn writing(mut self) -> Self {
        self.writes = true;
        self
    }
}

/// How an [`ExtensionError`] reaches the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultClass {
    /// The service refused the call (a permission, a validation, a precondition): the caller can
    /// change something and try again.
    Refused,
    /// The service could not do it (a store or a provider failed, the service is stopping).
    Failed,
}

/// Why an extension tool did not answer. The code is the service's own catalogue code and the
/// message a fixed sentence: neither ever carries an argument, a document value, a key or a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionError {
    /// The stable code (`retrieval.*`, `agent_memory.*`, `remedies.*`, `remedies_host.*`, or a
    /// native code such as `permission.denied`).
    pub code: &'static str,
    /// A fixed, user-safe sentence.
    pub message: String,
    /// How the refusal is classed on the wire.
    pub class: FaultClass,
}

impl ExtensionError {
    /// A refusal: the caller may fix it and retry.
    #[must_use]
    pub fn refused(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            class: FaultClass::Refused,
        }
    }

    /// A failure of the service or of something it depends on.
    #[must_use]
    pub fn failed(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            class: FaultClass::Failed,
        }
    }
}

impl From<ExtensionError> for McpError {
    fn from(error: ExtensionError) -> Self {
        let jsonrpc = match error.class {
            FaultClass::Refused => INVALID_PARAMS,
            FaultClass::Failed => INTERNAL_ERROR,
        };
        McpError::new(jsonrpc, error.code, error.message)
    }
}

/// A tool a hosting service adds to `tools/list`.
pub trait ExtensionTool: Send + Sync {
    /// The tool's stable name (see the module documentation for the rule).
    fn name(&self) -> &str;

    /// The tool as `principal` sees it, or `None` when the principal may not call it (then it is
    /// not listed, and a call is refused as an unknown tool). Called on every `tools/list` and
    /// `tools/call`: nothing about the principal is cached.
    fn spec(&self, principal: &McpPrincipal) -> Option<ExtensionSpec>;

    /// Runs one call as `principal` with arguments already validated against the spec's schema.
    /// A tool that must not run for this principal answers an [`ExtensionError`].
    fn call<'a>(
        &'a self,
        principal: &'a McpPrincipal,
        arguments: &'a Json,
    ) -> BoxFuture<'a, Result<Json, ExtensionError>>;
}

/// Why a tool could not be installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    /// The name is empty, too long, not lower snake case, `query`, `job_status` or begins with a
    /// generated prefix.
    BadName,
    /// A tool with this name is already installed.
    Duplicate,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InstallError::BadName => {
                "the tool name is not allowed (lower snake case, at most 64 characters, \
                 not `query` or `job_status`, no generated prefix)"
            }
            InstallError::Duplicate => "a tool with this name is already installed",
        })
    }
}

impl std::error::Error for InstallError {}

/// Whether `name` may be the name of an extension tool.
#[must_use]
pub fn name_allowed(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    let mut chars = name.chars();
    let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_lowercase());
    if !starts_with_letter
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return false;
    }
    if GENERATED_NAMES.contains(&name) {
        return false;
    }
    !GENERATED_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// The installed extension tools, shared between the host (which installs and uninstalls) and
/// the server (which reads on every request).
#[derive(Default)]
pub struct ExtensionRegistry {
    tools: RwLock<BTreeMap<String, Arc<dyn ExtensionTool>>>,
}

impl ExtensionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, Arc<dyn ExtensionTool>>> {
        self.tools.read().unwrap_or_else(|p| p.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<String, Arc<dyn ExtensionTool>>> {
        self.tools.write().unwrap_or_else(|p| p.into_inner())
    }

    /// Installs `tool`.
    ///
    /// # Errors
    ///
    /// [`InstallError::BadName`] when the name breaks the rule and [`InstallError::Duplicate`]
    /// when a tool of that name is installed. The registry is unchanged on an error.
    pub fn install(&self, tool: Arc<dyn ExtensionTool>) -> Result<(), InstallError> {
        let name = tool.name().to_owned();
        if !name_allowed(&name) {
            return Err(InstallError::BadName);
        }
        let mut tools = self.write();
        if tools.contains_key(&name) {
            return Err(InstallError::Duplicate);
        }
        tools.insert(name, tool);
        Ok(())
    }

    /// Removes the tool called `name`; whether one was installed. A call already running keeps
    /// the tool it started with.
    pub fn uninstall(&self, name: &str) -> bool {
        self.write().remove(name).is_some()
    }

    /// The installed tools, in name order.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Arc<dyn ExtensionTool>> {
        self.read().values().cloned().collect()
    }

    /// The installed tool called `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn ExtensionTool>> {
        self.read().get(name).cloned()
    }

    /// How many tools are installed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether no tool is installed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }
}

/// `tool.spec(principal)`, with a panic in the tool read as "not listed".
pub(crate) fn spec_of(tool: &dyn ExtensionTool, principal: &McpPrincipal) -> Option<ExtensionSpec> {
    catch_unwind(AssertUnwindSafe(|| tool.spec(principal)))
        .ok()
        .flatten()
}

/// Runs `tool.call`, turning a panic (while building the future or while polling it) into an
/// internal tool error instead of unwinding through the server.
pub(crate) async fn guarded_call(
    tool: &dyn ExtensionTool,
    principal: &McpPrincipal,
    arguments: &Json,
) -> Result<Json, McpError> {
    let Ok(future) = catch_unwind(AssertUnwindSafe(|| tool.call(principal, arguments))) else {
        return Err(panicked());
    };
    match (Guarded {
        inner: Some(future),
    })
    .await
    {
        Some(result) => result.map_err(McpError::from),
        None => Err(panicked()),
    }
}

fn panicked() -> McpError {
    McpError::internal("the tool failed")
}

/// A future that answers `None` instead of unwinding when the wrapped future panics.
struct Guarded<'a, T> {
    inner: Option<BoxFuture<'a, T>>,
}

impl<T> Future for Guarded<'_, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let Some(future) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => {
                this.inner = None;
                Poll::Ready(Some(value))
            }
            Err(_) => {
                // A panicked future is never polled again.
                this.inner = None;
                Poll::Ready(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn names_follow_the_rule() {
        for good in ["retrieve", "recall_memory", "remedy_dry_run", "a1"] {
            assert!(name_allowed(good), "{good}");
        }
        for bad in [
            "",
            "Retrieve",
            "re-trieve",
            "1retrieve",
            "_retrieve",
            "query",
            "job_status",
            "search_customer",
            "get_x",
            "create_x_draft",
            "update_x",
            "call_app_mod_fn",
        ] {
            assert!(!name_allowed(bad), "{bad}");
        }
        assert!(!name_allowed(&"a".repeat(MAX_NAME_LEN + 1)));
        assert!(name_allowed(&"a".repeat(MAX_NAME_LEN)));
    }

    #[test]
    fn an_error_keeps_its_code_and_class() {
        let refused = McpError::from(ExtensionError::refused("remedies.refused", "no"));
        assert_eq!(refused.kind, "remedies.refused");
        assert_eq!(refused.code, INVALID_PARAMS);
        let failed = McpError::from(ExtensionError::failed("retrieval.store_failed", "down"));
        assert_eq!(failed.kind, "retrieval.store_failed");
        assert_eq!(failed.code, INTERNAL_ERROR);
    }
}
