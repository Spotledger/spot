//! What a site's connectors are: the definition each `connector` DocType row carries, the
//! directory that lists them, and the state the service keeps per connector.
//!
//! A definition holds **references**, never values: the credential is a name in the vault, written
//! once through a write-only action (see [`super::vault`]).

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The state of one connector.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Runs when scheduled or invoked.
    Enabled,
    /// An operator turned it off; it never runs.
    Disabled,
    /// It failed to load (bundle, signature, describe).
    Failed,
    /// It failed on every attempt of a run; it waits for an operator.
    DeadLetter,
}

impl State {
    /// The text stored and shown.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            State::Enabled => "enabled",
            State::Disabled => "disabled",
            State::Failed => "failed",
            State::DeadLetter => "dead_letter",
        }
    }

    /// Parses the stored text; an unknown word is `failed`, never `enabled`.
    #[must_use]
    pub fn parse(text: &str) -> State {
        match text {
            "enabled" => State::Enabled,
            "disabled" => State::Disabled,
            "dead_letter" => State::DeadLetter,
            _ => State::Failed,
        }
    }
}

/// How the host authenticates a connector's outbound requests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthDef {
    /// `bearer`, `basic` or `oauth2`.
    pub kind: String,
    /// The vault reference holding the credential.
    pub credential: String,
    /// For `basic`, the static user name.
    #[serde(default)]
    pub username: Option<String>,
}

/// A schedule: every `every_seconds`, run `operation` with `input`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleDef {
    /// The period, at least 10 seconds.
    pub every_seconds: u64,
    /// The operation to run.
    pub operation: String,
    /// Its input.
    #[serde(default)]
    pub input: serde_json::Value,
}

/// One `connector` row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorDef {
    /// The connector's name, unique on the site.
    pub name: String,
    /// The digest of the signed bundle that holds its component (64 hex digits).
    pub digest: String,
    /// The hosts it may call: `host` or `host:port`. Empty means no network at all.
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// The authentication the host injects.
    #[serde(default)]
    pub auth: Option<AuthDef>,
    /// Further references the connector may read with `secrets-get`.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// When it runs by itself.
    #[serde(default)]
    pub schedule: Option<ScheduleDef>,
    /// The field mapping (`spot-expr` rules) the operations apply, as stored text.
    #[serde(default)]
    pub mapping: Option<String>,
    /// Its state.
    #[serde(default = "enabled")]
    pub state: State,
}

fn enabled() -> State {
    State::Enabled
}

/// Lists a site's connectors and keeps their state.
pub trait ConnectorDirectory: Send + Sync {
    /// Every connector of `site`.
    fn list<'a>(&'a self, site: &'a str) -> Boxed<'a, Result<Vec<ConnectorDef>, String>>;
    /// Sets the state of one connector.
    fn set_state<'a>(
        &'a self,
        site: &'a str,
        name: &'a str,
        state: State,
    ) -> Boxed<'a, Result<(), String>>;
}

/// A boxed future.
pub type Boxed<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// An in-memory directory (tests, and a site configured without a database table).
#[derive(Default)]
pub struct MemoryDirectory {
    items: Mutex<BTreeMap<(String, String), ConnectorDef>>,
}

impl MemoryDirectory {
    /// An empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a connector of `site`.
    pub fn put(&self, site: &str, def: ConnectorDef) {
        self.items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((site.to_owned(), def.name.clone()), def);
    }

    /// The state of a connector, for tests.
    #[must_use]
    pub fn state(&self, site: &str, name: &str) -> Option<State> {
        self.items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(site.to_owned(), name.to_owned()))
            .map(|d| d.state)
    }
}

impl ConnectorDirectory for MemoryDirectory {
    fn list<'a>(&'a self, site: &'a str) -> Boxed<'a, Result<Vec<ConnectorDef>, String>> {
        Box::pin(async move {
            Ok(self
                .items
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .filter(|((s, _), _)| s == site)
                .map(|(_, d)| d.clone())
                .collect())
        })
    }

    fn set_state<'a>(
        &'a self,
        site: &'a str,
        name: &'a str,
        state: State,
    ) -> Boxed<'a, Result<(), String>> {
        Box::pin(async move {
            let mut items = self.items.lock().unwrap_or_else(|p| p.into_inner());
            match items.get_mut(&(site.to_owned(), name.to_owned())) {
                Some(def) => {
                    def.state = state;
                    Ok(())
                }
                None => Err("connector.not_found".to_owned()),
            }
        })
    }
}

/// The PostgreSQL directory: one row per connector in `spot_connector_def`, the definition as JSON
/// text and the state beside it.
pub struct PgDirectory {
    pool: deadpool_postgres::Pool,
}

impl PgDirectory {
    /// A directory over `pool`.
    #[must_use]
    pub fn new(pool: deadpool_postgres::Pool) -> Self {
        Self { pool }
    }

    /// Creates the table if it is missing.
    ///
    /// # Errors
    ///
    /// `connector.store_failed`.
    pub async fn prepare(&self) -> Result<(), String> {
        let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS spot_connector_def (\
                 site text NOT NULL, name text NOT NULL, def text NOT NULL, \
                 state text NOT NULL DEFAULT 'enabled', PRIMARY KEY (site, name))",
            )
            .await
            .map_err(|_| STORE_FAILED.to_owned())
    }
}

/// `connector.store_failed`: the connector tables cannot be read or written.
pub const STORE_FAILED: &str = "connector.store_failed";

impl ConnectorDirectory for PgDirectory {
    fn list<'a>(&'a self, site: &'a str) -> Boxed<'a, Result<Vec<ConnectorDef>, String>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
            let rows = client
                .query(
                    "SELECT def, state FROM spot_connector_def WHERE site = $1 ORDER BY name",
                    &[&site],
                )
                .await
                .map_err(|_| STORE_FAILED.to_owned())?;
            let mut out = Vec::new();
            for row in rows {
                let text: String = row.get(0);
                let state: String = row.get(1);
                // A row that does not parse is a failed connector, not a missing one.
                if let Ok(mut def) = serde_json::from_str::<ConnectorDef>(&text) {
                    def.state = State::parse(&state);
                    out.push(def);
                }
            }
            Ok(out)
        })
    }

    fn set_state<'a>(
        &'a self,
        site: &'a str,
        name: &'a str,
        state: State,
    ) -> Boxed<'a, Result<(), String>> {
        Box::pin(async move {
            let client = self.pool.get().await.map_err(|_| STORE_FAILED.to_owned())?;
            let n = client
                .execute(
                    "UPDATE spot_connector_def SET state = $3 WHERE site = $1 AND name = $2",
                    &[&site, &name, &state.as_str()],
                )
                .await
                .map_err(|_| STORE_FAILED.to_owned())?;
            if n == 1 {
                Ok(())
            } else {
                Err("connector.not_found".to_owned())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_state_word_is_failed_never_enabled() {
        assert_eq!(State::parse("enabled"), State::Enabled);
        assert_eq!(State::parse("whatever"), State::Failed);
        assert_eq!(State::parse(State::DeadLetter.as_str()), State::DeadLetter);
    }

    #[test]
    fn a_definition_has_no_field_for_a_credential_value() {
        let def: Result<ConnectorDef, _> = serde_json::from_str(
            r#"{"name":"c","digest":"d","auth":{"kind":"bearer","credential":"crm_token"}}"#,
        );
        let def = def.unwrap_or_else(|_| unreachable!());
        assert_eq!(def.auth.map(|a| a.credential), Some("crm_token".to_owned()));
    }
}
