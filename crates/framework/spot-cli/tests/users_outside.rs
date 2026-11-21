//! The portal rule at the host's own call sites (ADR-083 Barrier 3, WP-IAM-11.T16): an
//! invitation or an activation cannot give a portal user a desk role. Real PostgreSQL (skipped
//! when none is configured).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use spot_cli::onetime::Issue;
use spot_cli::users;
use spot_storage_contract::{
    CmpOp, DocumentStore, Expr, Isolation, Name, Query, QueryExecutor, QueryPath, RecordId,
    RowLock, Selection, StorageBackend, Transaction, TxOptions, Value,
};
use std::sync::Arc;
use support::Env;

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";
const DESK_ROLE: &str = "Tenant Administrator";

fn name(text: &str) -> Name {
    Name::new(text).unwrap()
}

/// Opens a transaction on the site's own backend, as the operator commands do.
async fn open(env: &Env) -> impl Transaction {
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    let meta = spot_cli::apps::serving(&env.root, &loaded, site).unwrap();
    let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
    let catalog = Arc::new(spot_cli::schema::catalog(&meta).unwrap());
    backend
        .begin(
            catalog,
            TxOptions::new(name("spot_cli_test"), Isolation::ReadCommitted),
        )
        .await
        .unwrap()
}

async fn user_id(tx: &mut impl QueryExecutor, user_name: &str) -> Option<RecordId> {
    let alias = name("t");
    let mut query = Query::from(&name("user"), &alias, QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::field(&alias, &name("id")),
        alias: name("v"),
    }];
    query.filter = Some(Expr::Cmp(
        Box::new(Expr::field(&alias, &name("user_name"))),
        CmpOp::Eq,
        Box::new(Expr::Value(Value::Text(user_name.to_owned()))),
    ));
    query.limit = Some(1);
    let rows = tx.query(&query).await.unwrap();
    rows.rows.into_iter().flatten().find_map(|v| match v {
        Value::Id(id) => Some(id),
        _ => None,
    })
}

/// Changes a stored user's `user_type` directly in storage.
async fn set_user_type(env: &Env, user_name: &str, user_type: &str) {
    let mut tx = open(env).await;
    let id = user_id(&mut tx, user_name).await.unwrap();
    let doc = tx
        .load(&name("user"), id, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    let mut data = doc.data;
    data.fields
        .insert(name("user_type"), Value::Text(user_type.to_owned()));
    tx.update(&data, doc.version).await.unwrap();
    tx.commit().await.unwrap();
}

/// The stored `state` of a user, or `None` when there is no such user.
async fn state_of(env: &Env, user_name: &str) -> Option<String> {
    let mut tx = open(env).await;
    let id = user_id(&mut tx, user_name).await?;
    let doc = tx
        .load(&name("user"), id, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    let state = doc.data.fields.iter().find_map(|(n, v)| match v {
        Value::Text(t) if n.as_str() == "state" => Some(t.clone()),
        _ => None,
    });
    tx.rollback().await.unwrap();
    state
}

fn issue() -> Issue {
    Issue::system()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_internal_invitation_with_a_desk_role_works_as_before() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    users::add_user(
        &env.root,
        &env.site,
        "ama@example.com",
        None,
        &[DESK_ROLE.into()],
    )
    .await
    .unwrap();
    assert_eq!(
        state_of(&env, "ama@example.com").await.as_deref(),
        Some("invited")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_portal_invitation_with_a_desk_role_is_refused_and_leaves_nothing() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let refused = users::add_user_typed(
        &env.root,
        &env.site,
        "kofi@example.com",
        None,
        &[DESK_ROLE.into()],
        "portal",
        &issue(),
    )
    .await;
    let Err(e) = refused else {
        panic!("a portal user must not be invited with a desk role");
    };
    let text = e.to_string();
    assert!(text.contains(DESK_ROLE), "{text}");
    assert!(text.contains("duty"), "{text}");
    assert!(!text.contains("kofi"), "the e-mail must not appear: {text}");
    // Rolled back: no user, so the same address can be invited as an internal user.
    assert_eq!(state_of(&env, "kofi@example.com").await, None);
    users::add_user(&env.root, &env.site, "kofi@example.com", None, &[])
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_portal_invitation_without_roles_is_accepted_and_activates() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    users::add_user_typed(
        &env.root,
        &env.site,
        "efua@example.com",
        None,
        &[],
        "portal",
        &issue(),
    )
    .await
    .unwrap();
    let password = spot_auth::Password::new(PASSWORD);
    users::set_password(&env.root, &env.site, "efua@example.com", &password)
        .await
        .unwrap();
    assert_eq!(
        state_of(&env, "efua@example.com").await.as_deref(),
        Some("active")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_portal_user_holding_a_desk_role_is_not_activated() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    users::add_user(
        &env.root,
        &env.site,
        "yaw@example.com",
        None,
        &[DESK_ROLE.into()],
    )
    .await
    .unwrap();
    // The first portal invitation of the future reaches the host like this: a portal user
    // whose stored assignment is a desk role.
    set_user_type(&env, "yaw@example.com", "portal").await;
    let password = spot_auth::Password::new(PASSWORD);
    let refused = users::set_password(&env.root, &env.site, "yaw@example.com", &password).await;
    let Err(e) = refused else {
        panic!("a portal user with a desk role must not be activated");
    };
    let text = e.to_string();
    assert!(text.contains(DESK_ROLE), "{text}");
    assert!(!text.contains("yaw"), "{text}");
    assert_eq!(
        state_of(&env, "yaw@example.com").await.as_deref(),
        Some("invited")
    );
    // An internal user is unaffected: the operator fixes the type and retries.
    set_user_type(&env, "yaw@example.com", "internal").await;
    users::set_password(&env.root, &env.site, "yaw@example.com", &password)
        .await
        .unwrap();
    assert_eq!(
        state_of(&env, "yaw@example.com").await.as_deref(),
        Some("active")
    );
}
