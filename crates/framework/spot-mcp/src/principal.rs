//! Principal binding: the authenticated caller every request runs as.
//!
//! A request carries a credential — an OAuth bearer token or an API key. The host's
//! [`Authenticator`] turns it into an [`McpPrincipal`]: the permission engine's
//! [`Principal`], the events-contract [`Actor`] the invocation carries, and — for an agent —
//! its tier and identity. An API key additionally carries a [`KeyScope`], which
//! [`spot_iam_access`] proves is a subset of its owner's rights; every decision is narrowed by
//! it. There is no system principal here: the server refuses to run as one.

use spot_agent_tiers::Tier;
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{CommandId, SiteId};
use spot_iam_access::{KeyAction, KeyScope};
use spot_permissions::{Action, Decision, DocFacts, PermissionEngine, Principal};
use time::OffsetDateTime;

use crate::error::McpError;

/// The credential a request presented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    /// An OAuth bearer token (a session token).
    Bearer(String),
    /// An API key, named by its public key id and carrying its secret.
    ApiKey {
        /// The key's public id.
        id: String,
        /// The key's secret, used to verify it; never logged.
        secret: String,
    },
}

impl Credential {
    /// A bearer token.
    #[must_use]
    pub fn bearer(token: impl Into<String>) -> Self {
        Credential::Bearer(token.into())
    }

    /// An API key.
    #[must_use]
    pub fn api_key(id: impl Into<String>, secret: impl Into<String>) -> Self {
        Credential::ApiKey {
            id: id.into(),
            secret: secret.into(),
        }
    }

    /// A stable, secret-free label for logs and audit.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Credential::Bearer(_) => "bearer",
            Credential::ApiKey { .. } => "api_key",
        }
    }
}

/// The authenticated principal one request runs as.
#[derive(Clone, Debug)]
pub struct McpPrincipal {
    /// The permission engine's view of the caller.
    pub principal: Principal,
    /// The actor the invocation carries.
    pub actor: Actor,
    /// An API key's scope, when the caller authenticated with one.
    pub key: Option<KeyScope>,
    /// The API key's public id, when there is one.
    pub key_id: Option<String>,
    /// An agent's tier, when the caller is an agent.
    pub tier: Option<Tier>,
    /// An agent's identity, when the caller is an agent.
    pub agent_id: Option<String>,
    /// The request time, in unix seconds (time is an argument, never read here).
    pub now_unix: i64,
}

impl McpPrincipal {
    /// A user principal with no API-key narrowing.
    #[must_use]
    pub fn new(
        user: &str,
        actor: Actor,
        assignments: Vec<spot_permissions::AssignmentContext>,
    ) -> Self {
        Self {
            principal: Principal::user(user, assignments),
            actor,
            key: None,
            key_id: None,
            tier: None,
            agent_id: None,
            now_unix: 0,
        }
    }

    /// An agent principal: a user principal that is also an agent of `tier`.
    #[must_use]
    pub fn new_agent(
        user: &str,
        actor: Actor,
        assignments: Vec<spot_permissions::AssignmentContext>,
        agent_id: &str,
        tier: Tier,
    ) -> Self {
        Self {
            principal: Principal::user(user, assignments),
            actor,
            key: None,
            key_id: None,
            tier: Some(tier),
            agent_id: Some(agent_id.to_owned()),
            now_unix: 0,
        }
    }

    /// Sets the request time.
    #[must_use]
    pub fn at(mut self, now_unix: i64) -> Self {
        self.now_unix = now_unix;
        self
    }

    /// Narrows the principal with an API key's scope.
    #[must_use]
    pub fn with_key(mut self, id: &str, scope: KeyScope) -> Self {
        self.key_id = Some(id.to_owned());
        self.key = Some(scope);
        self
    }

    /// The permission engine's principal.
    #[must_use]
    pub fn permissions(&self) -> &Principal {
        &self.principal
    }

    /// The invocation actor.
    #[must_use]
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The user name the permission engine knows.
    #[must_use]
    pub fn user(&self) -> &str {
        &self.principal.user
    }

    /// The agent's tier, when the caller is an agent.
    #[must_use]
    pub fn tier(&self) -> Option<Tier> {
        self.tier
    }

    /// The agent's identity, when the caller is an agent.
    #[must_use]
    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    /// The API key's public id, when the caller used one.
    #[must_use]
    pub fn key_id(&self) -> Option<&str> {
        self.key_id.as_deref()
    }

    /// Whether the caller is an agent.
    #[must_use]
    pub fn is_agent(&self) -> bool {
        self.agent_id.is_some()
    }

    /// Refuses a system principal: a tool never runs as the platform.
    ///
    /// # Errors
    ///
    /// [`McpError::permission_denied`] when the principal is the system.
    pub fn assert_not_system(&self) -> Result<(), McpError> {
        if self.principal.is_system {
            return Err(McpError::permission_denied(
                "a tool call never runs as the system principal",
            ));
        }
        Ok(())
    }

    /// Whether the principal may do `action` on a document with these facts. An API key's
    /// decision is its owner's, narrowed by the key.
    #[must_use]
    pub fn may(&self, engine: &PermissionEngine, action: Action, doc: &DocFacts) -> Decision {
        match &self.key {
            Some(scope) => scope.decide(engine, &self.principal, action, doc),
            None => engine.can(&self.principal, action, doc),
        }
    }

    /// Whether the principal may do `action` on the DocType at all, asked without a document
    /// (a probe the principal created, so an `if_owner` grant counts).
    #[must_use]
    pub fn may_on(&self, engine: &PermissionEngine, action: Action, doctype: &str) -> bool {
        let doc = DocFacts {
            doctype: doctype.to_owned(),
            created_by: Some(self.principal.user.clone()),
            ..DocFacts::default()
        };
        self.may(engine, action, &doc).allowed
    }

    /// Refuses `action` on `doctype` when the principal may not do it.
    ///
    /// # Errors
    ///
    /// [`McpError::permission_denied`].
    pub fn check_on(
        &self,
        engine: &PermissionEngine,
        action: Action,
        doctype: &str,
    ) -> Result<(), McpError> {
        if self.may_on(engine, action, doctype) {
            Ok(())
        } else {
            Err(McpError::permission_denied(format!(
                "the caller may not {} `{doctype}`",
                action.as_str()
            )))
        }
    }

    /// Refuses an action the API key does not cover, if the caller used a key.
    ///
    /// # Errors
    ///
    /// [`McpError::permission_denied`].
    pub fn check_key(&self, doctype: &str, action: KeyAction) -> Result<(), McpError> {
        let Some(scope) = &self.key else {
            return Ok(());
        };
        let covered = scope.allow.iter().any(|rule| {
            (rule.doctype == "*" || rule.doctype == doctype) && rule.actions.contains(&action)
        });
        if covered {
            Ok(())
        } else {
            Err(McpError::permission_denied(
                "the API key does not allow this action",
            ))
        }
    }

    /// Builds the invocation for one call.
    #[must_use]
    pub fn invocation(
        &self,
        site: SiteId,
        command_id: Option<CommandId>,
        language: Option<String>,
        now: OffsetDateTime,
    ) -> Invocation {
        Invocation {
            site,
            actor: self.actor.clone(),
            command_id,
            language,
            now,
        }
    }

    /// The audit actor: `user:<id>` or `agent:<id>`.
    #[must_use]
    pub fn audit_actor(&self) -> String {
        match (&self.agent_id, actor_id(&self.actor)) {
            (Some(agent), _) => format!("agent:{agent}"),
            (None, Some(id)) => format!("user:{id}"),
            (None, None) => "user:unknown".to_owned(),
        }
    }
}

fn actor_id(actor: &Actor) -> Option<String> {
    match actor {
        Actor::User { id, .. } => Some(id.to_string()),
        // An MCP session is always a user's or an agent's; neither a consumer nor the platform.
        Actor::System { .. } | Actor::Consumer { .. } => None,
    }
}

/// Turns a credential into the principal it authenticates, or refuses it.
///
/// The host implements this against `spot-auth` and `spot-iam-access`; this crate never reads
/// a credential itself.
pub trait Authenticator: Send + Sync {
    /// The principal the credential authenticates, or a refusal. `now_unix` is the request
    /// time; an expired key or session is refused.
    ///
    /// # Errors
    ///
    /// [`McpError::unauthorized`] when the credential is missing, malformed or expired.
    fn authenticate(
        &self,
        credential: &Credential,
        now_unix: i64,
    ) -> Result<McpPrincipal, McpError>;
}
