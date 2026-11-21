//! The production seams of the host: the user directory and the document creator over the
//! site's live engine, the sealed credentials, the coordinator and the store constructors.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::Document;
use spot_app_api::error::ErrorClass;
use spot_coordinator::Coordinator;
use spot_core::{Clock, SiteId};
use spot_events_contract::CommandId;
use spot_mail::Secret;
use spot_mail_intake::{CredentialError, CredentialSource};
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_secrets::{KeyProvider, Sealed, SecretBox};
use spot_storage_contract::{
    CmpOp, Expr, Isolation, Name, Query, QueryExecutor, QueryPath, RecordId, Selection,
    StorageBackend, Transaction, TxOptions, Value,
};
use std::str::FromStr;

use super::sink::{CreateError, DirectoryUnavailable, DocumentCreator, Fut, RunAs, UserDirectory};
use crate::live::SiteLive;
use crate::services::process_triggers::SealedSecret;

/// The users of the site, read through the storage layer.
pub struct LiveUsers {
    /// The site's database.
    pub backend: PgBackend,
    /// The live hub (it holds the served metadata the catalogue comes from).
    pub live: Arc<SiteLive>,
}

fn name(text: &str) -> Result<Name, DirectoryUnavailable> {
    Name::new(text).map_err(|_| DirectoryUnavailable)
}

impl UserDirectory for LiveUsers {
    fn find<'a>(
        &'a self,
        user_name: &'a str,
    ) -> Fut<'a, Result<Option<RunAs>, DirectoryUnavailable>> {
        Box::pin(async move {
            let served = self.live.served().ok_or(DirectoryUnavailable)?;
            let catalog =
                Arc::new(crate::schema::catalog(&served.meta).map_err(|_| DirectoryUnavailable)?);
            let options = TxOptions::new(name("mail_intake")?, Isolation::ReadCommitted);
            let mut tx = self
                .backend
                .begin(catalog, options)
                .await
                .map_err(|_| DirectoryUnavailable)?;
            let alias = name("t")?;
            let mut query = Query::from(&name(doctype::USER)?, &alias, QueryPath::Report);
            query.select = vec![
                Selection {
                    expr: Expr::field(&alias, &name("id")?),
                    alias: name("v")?,
                },
                Selection {
                    expr: Expr::field(&alias, &name("state")?),
                    alias: name("s")?,
                },
            ];
            query.filter = Some(Expr::Cmp(
                Box::new(Expr::field(&alias, &name("user_name")?)),
                CmpOp::Eq,
                Box::new(Expr::Value(Value::Text(user_name.to_owned()))),
            ));
            query.limit = Some(1);
            let rows = tx.query(&query).await;
            let _ = tx.rollback().await;
            let rows = rows.map_err(|_| DirectoryUnavailable)?;
            let Some(row) = rows.rows.into_iter().next() else {
                return Ok(None);
            };
            let mut cells = row.into_iter();
            let id = match cells.next() {
                Some(Value::Id(id)) => id,
                _ => return Ok(None),
            };
            // A user that is disabled or anonymised may not act; an invited one may (a service
            // user never signs in).
            let active = !matches!(
                cells.next(),
                Some(Value::Text(state)) if state == "disabled" || state == "anonymised"
            );
            Ok(Some(RunAs { id, active }))
        })
    }
}

/// Creates documents through the site's running engine, as a user.
pub struct LiveCreator {
    /// The site.
    pub site: SiteId,
    /// The live hub (it holds the running engine).
    pub live: Arc<SiteLive>,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
}

impl DocumentCreator for LiveCreator {
    fn create<'a>(
        &'a self,
        run_as: RecordId,
        doctype: &'a str,
        id: RecordId,
        command_id: String,
        fields: Vec<(String, Value)>,
    ) -> Fut<'a, Result<(), CreateError>> {
        Box::pin(async move {
            let engine = self.live.engine().ok_or(CreateError::Unavailable)?;
            let doc_name = Name::new(doctype).map_err(|_| CreateError::Invalid)?;
            let mut doc = Document::new(doc_name, id);
            for (field, value) in fields {
                let field = Name::new(&field).map_err(|_| CreateError::Invalid)?;
                doc.set(field, value);
            }
            let inv = Invocation {
                site: self.site.clone(),
                actor: Actor::User {
                    id: run_as,
                    via: None,
                },
                command_id: CommandId::new(&command_id).ok(),
                language: None,
                now: self.clock.now_utc(),
            };
            match engine.create(&inv, doc).await {
                Ok(_) => Ok(()),
                Err(error) => Err(match error.class {
                    ErrorClass::Validation
                    | ErrorClass::UnknownDocType
                    | ErrorClass::Duplicate
                    | ErrorClass::InvalidState => CreateError::Invalid,
                    ErrorClass::PermissionDenied | ErrorClass::NotFound => CreateError::Denied,
                    _ => CreateError::Unavailable,
                }),
            }
        })
    }
}

/// IMAP passwords sealed in the settings, opened with the site's key provider at poll time only.
pub struct SealedCredentials<P: KeyProvider> {
    opener: SecretBox<P>,
    entries: BTreeMap<String, SealedSecret>,
}

impl<P: KeyProvider> SealedCredentials<P> {
    /// Secrets `entries` opened with `provider`.
    pub fn new(provider: P, entries: BTreeMap<String, SealedSecret>) -> Self {
        Self {
            opener: SecretBox::new(provider),
            entries,
        }
    }
}

#[async_trait]
impl<P: KeyProvider + 'static> CredentialSource for SealedCredentials<P> {
    async fn resolve(&self, credential_ref: &str) -> Result<Secret, CredentialError> {
        let entry = self
            .entries
            .get(credential_ref)
            .ok_or(CredentialError::NotFound)?;
        let sealed = Sealed::from_str(&entry.sealed).map_err(|_| CredentialError::NotFound)?;
        let opened = self
            .opener
            .open(&sealed, entry.aad.as_bytes())
            .await
            .map_err(|_| CredentialError::Unavailable)?;
        Ok(Secret::new(opened.expose().to_owned()))
    }
}

/// A source with no secrets: the site has no `[services.secrets]` key.
pub struct NoCredentials;

#[async_trait]
impl CredentialSource for NoCredentials {
    async fn resolve(&self, _credential_ref: &str) -> Result<Secret, CredentialError> {
        Err(CredentialError::NotFound)
    }
}

/// The coordinator the site's leases live in: PostgreSQL on the cluster profile, in the process
/// otherwise.
///
/// # Errors
/// The coordinator cannot be prepared or started; the text never holds a connection string.
pub async fn coordinator(
    backend: &PgBackend,
    cluster: bool,
    clock: Arc<dyn Clock>,
    node: &str,
) -> Result<Arc<dyn Coordinator>, String> {
    if cluster {
        let coordinator = spot_coordinator_pg::PgCoordinator::new(
            backend.pool(),
            spot_coordinator_pg::PgCoordinatorConfig::default(),
        );
        coordinator
            .prepare()
            .await
            .map_err(|e| format!("the coordinator: {}", e.code))?;
        Ok(Arc::new(coordinator))
    } else {
        let node_id = spot_coordinator::NodeId::new(node.to_owned())
            .map_err(|e| format!("the node name: {}", e.code))?;
        let coordinator = spot_coordinator::InProcessCoordinator::start(
            Arc::new(spot_coordinator::InMemoryPresence::new()),
            node_id,
            clock,
        )
        .await
        .map_err(|e| format!("the coordinator: {}", e.code))?;
        Ok(coordinator)
    }
}
