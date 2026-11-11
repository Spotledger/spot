//! The routing table: task name to an ordered list of [`Route`]s, and its TOML document.
//!
//! The document is deliberately small and readable — it is what an operator edits to move
//! `summarise` from a big model to a small one:
//!
//! ```toml
//! [routes.chat]
//! chain = [
//!   { provider = "anthropic", model = "claude-sonnet-4" },
//!   { provider = "openrouter", model = "meta-llama/llama-3.1-70b" },
//! ]
//!
//! [routes.classify]
//! chain = [{ provider = "ollama", model = "llama3.2:3b" }]
//! ```
//!
//! [`RoutingTable::from_toml`] reports **every** problem it finds, not the first, as one
//! [`RouterError::ConfigInvalid`] carrying each problem as a sentence — a document with
//! three typos comes back with three lines, not one per edit-compile cycle. The problems
//! carry the `router.*` codes; the sentence names the code and the place.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::RouterError;

/// Where one provider's traffic should go: a provider id and a model the provider knows.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, Deserialize)]
pub struct Route {
    /// The provider's stable id (`anthropic`, `openrouter`, `ollama`, ...), as the router
    /// finds it in its provider map.
    pub provider: String,
    /// The model id, in that provider's own spelling.
    pub model: String,
}

impl Route {
    /// A route to `model` on `provider`.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// The ordered chain of routes one task uses: first healthy, capable route wins.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct RouteChain {
    /// The routes, in preference order.
    pub chain: Vec<Route>,
}

/// Task name to route chain. Lookups are exact; task names are case-sensitive because they
/// appear in code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoutingTable {
    routes: BTreeMap<String, Vec<Route>>,
}

/// The document as TOML carries it: `[routes.<task>] chain = [...]`.
#[derive(Debug, Deserialize)]
struct Document {
    #[serde(default)]
    routes: BTreeMap<String, RouteChain>,
}

impl RoutingTable {
    /// A table from task name to its chain; the builder for tests and programmatic setup.
    pub fn new(routes: BTreeMap<String, Vec<Route>>) -> Self {
        Self { routes }
    }

    /// Adds or replaces one task's chain; later builders keep chaining.
    pub fn with_chain(mut self, task: impl Into<String>, chain: Vec<Route>) -> Self {
        self.routes.insert(task.into(), chain);
        self
    }

    /// The chain for `task`, or `None` when the task is unknown.
    #[must_use]
    pub fn chain(&self, task: &str) -> Option<&[Route]> {
        self.routes.get(task).map(Vec::as_slice)
    }

    /// Every task name with a chain, in order.
    pub fn tasks(&self) -> impl Iterator<Item = &str> {
        self.routes.keys().map(String::as_str)
    }

    /// Whether the table knows `task`.
    #[must_use]
    pub fn knows(&self, task: &str) -> bool {
        self.routes.contains_key(task)
    }

    /// Parses the routing document, reporting **every** problem at once as one
    /// [`RouterError::ConfigInvalid`] whose `problems` carries one sentence per problem.
    ///
    /// A document that does not parse as TOML, has no routes, or names a task with an empty
    /// or missing chain is refused; a task that is fine is kept even when another is not, so
    /// the message can describe the whole document rather than the first fault in it.
    pub fn from_toml(document: &str) -> Result<Self, RouterError> {
        let mut problems: Vec<String> = Vec::new();
        let parsed: Document = match toml::from_str(document) {
            Ok(parsed) => parsed,
            Err(error) => {
                // One TOML syntax error at a time is all the parser gives; report it with its
                // position, which is what a reader needs to fix it.
                problems.push(format!(
                    "router.config_invalid: the document is not valid TOML: {error}"
                ));
                return Err(RouterError::ConfigInvalid { problems });
            }
        };

        if parsed.routes.is_empty() {
            problems.push(
                "router.config_invalid: the document declares no routes; add a [routes.<task>] \
                 table with a non-empty chain"
                    .to_owned(),
            );
        }

        for (task, chain) in &parsed.routes {
            if task.trim().is_empty() {
                problems.push(
                    "router.config_invalid: a task name is empty; task names are matched exactly"
                        .to_owned(),
                );
            }
            if chain.chain.is_empty() {
                problems.push(format!(
                    "router.config_invalid: task `{task}` has an empty chain; a task needs at \
                     least one route"
                ));
                continue;
            }
            for (index, route) in chain.chain.iter().enumerate() {
                if route.provider.trim().is_empty() {
                    problems.push(format!(
                        "router.config_invalid: task `{task}` route {index} names no provider"
                    ));
                }
                if route.model.trim().is_empty() {
                    problems.push(format!(
                        "router.config_invalid: task `{task}` route {index} names no model"
                    ));
                }
            }
            let mut seen: Vec<&Route> = Vec::new();
            for route in &chain.chain {
                if seen.contains(&route) {
                    problems.push(format!(
                        "router.config_invalid: task `{task}` lists `{}`/`{}` twice; the second \
                         is never reached",
                        route.provider, route.model
                    ));
                }
                seen.push(route);
            }
        }

        if problems.is_empty() {
            let routes = parsed
                .routes
                .into_iter()
                .map(|(task, chain)| (task, chain.chain))
                .collect();
            Ok(Self { routes })
        } else {
            Err(RouterError::ConfigInvalid { problems })
        }
    }

    /// The raw chain map; the ledger's tests and the router read it wholesale.
    #[must_use]
    pub fn all(&self) -> &BTreeMap<String, Vec<Route>> {
        &self.routes
    }
}
