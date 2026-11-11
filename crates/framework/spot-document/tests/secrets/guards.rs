//! The secret path cannot be used to get around the rest of the engine: `create_with_secrets`
//! is authorised like any create, and a hook's repository query cannot name a sealed column or
//! a blind-index column.

use std::sync::Mutex;

use crate::support::*;
use spot_app_api::{DocTypeDef, Document};
use spot_document::engine::{AllowAllAuthorizer, Authorizer, QueryPlan, WriteChange};
use spot_permissions::{Action, Decision, DocFacts, Principal};
use spot_storage_contract::{Expr, Name, Nulls, Order, OrderDir, Query, QueryPath, Selection};

fn denied() -> AppError {
    AppError::new(
        ErrorClass::PermissionDenied,
        "permission.denied",
        "Creating is not allowed.",
    )
}

/// An authorizer that allows everything except creating: the check a create must pass, so the
/// test can tell whether a path runs it. (It is not "unrestricted", so the engine consults it.)
struct NoCreates(AllowAllAuthorizer);

impl Authorizer for NoCreates {
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> BoxFuture<'a, Principal> {
        self.0.principal_for(inv)
    }

    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        facts: &'a DocFacts,
    ) -> BoxFuture<'a, Decision> {
        self.0.decide(principal, action, facts)
    }

    fn authorize<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>> {
        if action == Action::Op(Operation::Create) {
            return Box::pin(async { Err(denied()) });
        }
        self.0.authorize(principal, action, doc)
    }

    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
        changes: &'a [WriteChange],
    ) -> BoxFuture<'a, AppResult<()>> {
        self.0.authorize_write(principal, action, doc, changes)
    }

    fn readable<'a>(
        &'a self,
        principal: &'a Principal,
        doc: Document,
        def: &'a DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>> {
        self.0.readable(principal, doc, def)
    }

    fn readable_query<'a>(
        &'a self,
        principal: &'a Principal,
        query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Option<QueryPlan>>> {
        self.0.readable_query(principal, query)
    }
}

/// What the probe step saw: the result of each query it ran, by label.
type Seen = Arc<Mutex<Vec<(String, Result<usize, String>)>>>;

/// A controller whose after-create step runs one repository query per probe.
struct QueryProbe {
    seen: Seen,
}

fn account_query() -> (Query, Name, Name) {
    let entity = n("account");
    let alias = n("d");
    let id = n("id");
    let mut q = Query::from(&entity, &alias, QueryPath::Transactional);
    q.select = vec![Selection {
        expr: Expr::field(&alias, &id),
        alias: id.clone(),
    }];
    (q, alias, id)
}

fn probes() -> Vec<(&'static str, Query)> {
    let col = |name: &str| (n("d"), n(name));
    let sel = |name: &str| {
        let (mut q, alias, _) = account_query();
        let c = col(name);
        q.select = vec![Selection {
            expr: Expr::field(&alias, &c.1),
            alias: c.1.clone(),
        }];
        q
    };
    let mut out = vec![
        ("control: an ordinary column", sel("label")),
        ("select sealed", sel("password__sealed")),
        ("select token sealed", sel("token__sealed")),
        ("select bidx", sel("token__bidx")),
    ];
    let (mut q, alias, _) = account_query();
    q.filter =
        Some(Expr::field(&alias, &n("api_key__bidx")).eq(Expr::value(Value::Bytes(vec![1]))));
    out.push(("filter on bidx", q));
    let (mut q, alias, _) = account_query();
    q.order_by = vec![Order {
        expr: Expr::field(&alias, &n("token__sealed")),
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    }];
    out.push(("order by sealed", q));
    let (mut q, alias, _) = account_query();
    q.group_by = vec![Expr::field(&alias, &n("token__bidx"))];
    out.push(("group by bidx", q));
    let (mut q, alias, _) = account_query();
    q.filter = Some(Expr::Not(Box::new(Expr::IsNull(Box::new(Expr::field(
        &alias,
        &n("password__sealed"),
    ))))));
    out.push(("filter nested in not/is-null", q));
    out
}

impl Controller for QueryProbe {
    fn doctype(&self) -> &str {
        "account"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("probe:query_secret_columns").unwrap(),
            point: HookPoint::of(Phase::After, Target::Op(Operation::Create)),
            priority: 0,
            budget: Duration::from_secs(5),
        }]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            for (label, query) in probes() {
                let outcome = ctx
                    .query(&query)
                    .await
                    .map(|rows| rows.rows.len())
                    .map_err(|e| e.code.clone());
                self.seen.lock().unwrap().push((label.to_owned(), outcome));
            }
            Ok(())
        })
    }
}

async fn engine(
    controller: Option<Arc<dyn Controller>>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> Engine<FakeBackend> {
    let mut builder = RegistryBuilder::new();
    if let Some(c) = controller {
        builder.add_controller("probe", c);
    }
    let mut engine = Engine::new(
        FakeBackend::new(),
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_secrets(Arc::new(SecretBox::new(keyring())));
    if let Some(a) = authorizer {
        engine = engine.with_authorizer(a);
    }
    engine.prepare().await.unwrap();
    engine
}

#[tokio::test]
async fn a_repository_query_cannot_name_a_sealed_or_blind_index_column() {
    let seen: Seen = Arc::default();
    let engine = engine(Some(Arc::new(QueryProbe { seen: seen.clone() })), None).await;
    let access = engine.secret_access(SecretCaller::Test, &inv());
    // The record is created with both kinds of secret, so there is something to read.
    let id = RecordId::new_v7();
    let mut doc = spot_app_api::Document::new(n("account"), id);
    doc.set(n("label"), "probe");
    engine
        .create_with_secrets(
            &access,
            doc,
            vec![("token".into(), s(TOKEN)), ("password".into(), s(PASSWORD))],
        )
        .await
        .unwrap();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), probes().len(), "every probe ran: {seen:?}");
    for (label, outcome) in seen {
        if label.starts_with("control") {
            assert!(outcome.is_ok(), "{label}: {outcome:?}");
        } else {
            assert_eq!(outcome, Err("permission.denied".to_owned()), "{label}");
        }
    }
}

#[tokio::test]
async fn create_with_secrets_is_authorised_like_a_create() {
    let engine = engine(None, Some(Arc::new(NoCreates(AllowAllAuthorizer)))).await;
    let access = engine.secret_access(SecretCaller::Test, &inv());
    // Control: the ordinary create is what the authorizer refuses.
    let ordinary = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    assert_eq!(
        code_of(engine.create(&inv(), ordinary).await),
        "permission.denied"
    );
    // A create with secrets takes the same gate, so the capability does not get round it.
    let id = RecordId::new_v7();
    let doc = spot_app_api::Document::new(n("account"), id);
    assert_eq!(
        code_of(
            engine
                .create_with_secrets(&access, doc, vec![("token".into(), s(TOKEN))])
                .await
        ),
        "permission.denied"
    );
    assert!(
        engine.peek("account", id).await.unwrap().is_none(),
        "nothing was created"
    );
}

#[tokio::test]
async fn create_with_secrets_with_no_secret_is_refused() {
    let engine = engine(None, None).await;
    let access = engine.secret_access(SecretCaller::Test, &inv());
    let id = RecordId::new_v7();
    let doc = spot_app_api::Document::new(n("account"), id);
    assert_eq!(
        code_of(engine.create_with_secrets(&access, doc, vec![]).await),
        "secretfield.empty_value",
        "an empty list would be a plain create under the capability alone"
    );
    assert!(engine.peek("account", id).await.unwrap().is_none());
    // A DocType with no secret field cannot be created through it either.
    let line = spot_app_api::Document::new(n("crate_box"), RecordId::new_v7());
    assert_eq!(
        code_of(
            engine
                .create_with_secrets(&access, line, vec![("x".into(), s("y"))])
                .await
        ),
        "secretfield.unknown_field"
    );
}

#[tokio::test]
async fn the_audit_state_is_recorded_only_for_a_call_that_worked() {
    let engine = engine(None, None).await;
    let access = engine.secret_access(SecretCaller::Test, &inv());
    let ghost = RecordId::new_v7();
    // A failed set records no state (it did not set anything).
    let _ = engine
        .set_secret(
            &access,
            "account",
            ghost,
            "password",
            s("x"),
            Version::FIRST,
        )
        .await;
    let last = engine.secret_audit_log().pop().unwrap();
    assert_eq!(
        (last.op, last.state, last.error.as_deref()),
        (SecretOp::Set, "<none>", Some("document.not_found"))
    );
    // A failed open records no state either.
    let _ = engine
        .open_secret(&access, "account", ghost, "password")
        .await;
    assert_eq!(engine.secret_audit_log().pop().unwrap().state, "<none>");
    // An open of a secret that was never set is "unset", and one that was set is "set".
    let mut doc = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    doc.set(n("label"), "audit");
    let id = doc.id();
    engine
        .create_with_secrets(&access, doc, vec![("token".into(), s(TOKEN))])
        .await
        .unwrap();
    engine
        .open_secret(&access, "account", id, "password")
        .await
        .unwrap();
    assert_eq!(engine.secret_audit_log().pop().unwrap().state, "<unset>");
    engine
        .open_secret(&access, "account", id, "token")
        .await
        .unwrap();
    assert_eq!(engine.secret_audit_log().pop().unwrap().state, "<set>");
}
