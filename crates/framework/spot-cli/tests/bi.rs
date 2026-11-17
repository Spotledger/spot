//! WP-PRD-07.T2: `spot bi` (plan, apply, list, password, revoke). The plan tests need no database
//! (the site root is built by hand). The rest run against real PostgreSQL through
//! `SPOT_TEST_PG_URL` and print one `SKIPPED` line without it, never faking the server: the
//! point is what the BI role can and cannot do, and only a server can say.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use common::{Env, Root, copy_dir, repo, run};
use spot_app_api::prelude::RecordId;

const POLICY: &str =
    "expose = [\"task\"]\n[entity.task]\nposted_only = false\ninclude = [\"subject\"]\n";

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

/// The sample app with the same two fixture patches `Env` makes, in a root with one site and no
/// database: enough for `spot bi plan`, which reads metadata and the policy only.
fn offline(policy: Option<&str>) -> Root {
    let root = Root(std::env::temp_dir().join(format!("spot_bi_t2_{}", RecordId::new_v7())));
    copy_dir(&repo().join("apps/sample"), &root.0.join("sample"));
    let task = root.0.join("sample/doctypes/task.toml");
    let patched = std::fs::read_to_string(&task).unwrap().replace(
        "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"",
        "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"\nprotection_group = \"confidential\"",
    );
    let patched = patched.replace(
        "title_field = \"subject\"",
        "title_field = \"subject\"\n\n[[protection_groups]]\nname = \"confidential\"\nlabel = \"Confidential\"\ntrace = true",
    );
    std::fs::write(&task, patched).unwrap();
    std::fs::write(
        root.0.join("common.toml"),
        format!(
            "apps = [{:?}, 'sample']\n[server]\nbind = ['127.0.0.1:0']\nshutdown_grace = 2\n[postgres]\nadmin_url = 'postgres://u:p@127.0.0.1:1/postgres'\n[trust]\nrequire_signed = false\n",
            repo().join("apps/platform").display().to_string()
        ),
    )
    .unwrap();
    let site = root.0.join("sites/s1");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(site.join("site.toml"), "site = 's1'\ndatabase = 's1db'\n").unwrap();
    if let Some(policy) = policy {
        std::fs::write(site.join("bi.toml"), policy).unwrap();
    }
    root
}

#[test]
fn without_a_policy_nothing_is_exposed() {
    let root = offline(None);
    let out = run(&root.0, &["bi", "plan", "--site", "s1"], None);
    let said = text(&out);
    assert_eq!(code(&out), 0, "{said}");
    assert!(said.contains("nothing is exposed"), "{said}");
    assert!(!said.contains("bi.v1_"), "{said}");
}

#[test]
fn the_policy_decides_and_the_metadata_can_only_close_columns() {
    let root = offline(Some(POLICY));
    let out = run(&root.0, &["bi", "plan", "--site", "s1"], None);
    let said = text(&out);
    assert_eq!(code(&out), 0, "{said}");
    assert!(said.contains("bi.v1_task (task)"), "{said}");
    assert!(said.contains("    subject text"), "{said}");
    // Left out, with the reason: protected, and not in the include list.
    assert!(
        said.contains("task.details: protected (confidential)"),
        "{said}"
    );
    assert!(
        said.contains("task.assignee: not in the policy's include list"),
        "{said}"
    );
    assert!(
        !said.contains("    details "),
        "a protected column reached a view: {said}"
    );
}

#[test]
fn an_unclassified_column_is_closed_unless_the_include_list_names_it() {
    // No include list: every unclassified column is closed by default.
    let policy = "expose = [\"task\"]\n[entity.task]\nposted_only = false\n";
    let root = offline(Some(policy));
    let out = run(&root.0, &["bi", "plan", "--site", "s1"], None);
    let said = text(&out);
    assert_eq!(code(&out), 0, "{said}");
    assert!(said.contains("task.subject: unclassified"), "{said}");
    assert!(!said.contains("    subject text"), "{said}");
}

#[test]
fn a_policy_that_asks_for_a_protected_column_is_refused_and_names_it() {
    let policy = "expose = [\"task\"]\n[entity.task]\nposted_only = false\ninclude = [\"subject\", \"details\"]\n";
    let root = offline(Some(policy));
    let out = run(&root.0, &["bi", "plan", "--site", "s1"], None);
    assert_eq!(code(&out), 2, "{}", text(&out));
    assert!(text(&out).contains("bi.protected_column"), "{}", text(&out));
}

#[test]
fn an_unknown_site_and_an_unusable_site_name_are_refused() {
    let root = offline(Some(POLICY));
    let out = run(&root.0, &["bi", "plan", "--site", "nope"], None);
    assert_eq!(code(&out), 1, "{}", text(&out));
    // A role name must fit PostgreSQL's 63 bytes: a long site name is refused, not shortened.
    let long = "s".repeat(60);
    std::fs::create_dir_all(root.0.join("sites").join(&long)).unwrap();
    std::fs::write(
        root.0.join("sites").join(&long).join("site.toml"),
        format!("site = '{long}'\ndatabase = 'longdb'\n"),
    )
    .unwrap();
    let out = run(&root.0, &["bi", "plan", "--site", &long], None);
    assert_eq!(code(&out), 2, "{}", text(&out));
    assert!(
        text(&out).contains("cannot make a role name"),
        "{}",
        text(&out)
    );
}

#[test]
fn a_missing_site_argument_is_a_usage_error() {
    let root = offline(Some(POLICY));
    let out = run(&root.0, &["bi", "plan"], None);
    assert_eq!(code(&out), 1, "{}", text(&out));
}

// --- real PostgreSQL ---------------------------------------------------------------------------------

struct Rig {
    env: Env,
    role: String,
}

impl Rig {
    async fn new() -> Option<Rig> {
        let env = Env::new("").await?;
        env.site();
        std::fs::write(env.site_dir().join("bi.toml"), POLICY).unwrap();
        let role = format!("spot_bi_{}_ro", env.site);
        Some(Rig { env, role })
    }

    fn spot(&self, args: &[&str]) -> Output {
        let mut all = vec!["bi"];
        all.extend_from_slice(args);
        all.extend_from_slice(&["--site", &self.env.site]);
        run(&self.env.root.0, &all, None)
    }

    async fn admin(&self) -> deadpool_postgres::Pool {
        let loaded = spot_cli::config::load(&self.env.root.0, false).unwrap();
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == self.env.site)
            .unwrap();
        spot_cli::schema::backend(&loaded, site)
            .await
            .unwrap()
            .pool()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // The role belongs to the cluster, not to the scratch database: drop it with the test.
        let (root, site) = (self.env.root.0.clone(), self.env.site.clone());
        let _ = run(&root, &["bi", "revoke", "--site", &site], None);
    }
}

async fn names(pool: &deadpool_postgres::Pool, sql: &str) -> Vec<String> {
    let client = pool.get().await.unwrap();
    let rows = client.query(sql, &[]).await.unwrap();
    rows.iter().map(|r| r.get::<_, String>(0)).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_makes_views_and_a_role_that_reads_only_them() {
    let Some(rig) = Rig::new().await else {
        println!("SKIPPED: no PostgreSQL (set SPOT_TEST_PG_URL)");
        return;
    };
    let pool = rig.admin().await;

    // Without --apply only the plan is shown and nothing exists.
    let dry = rig.spot(&["apply"]);
    assert_eq!(code(&dry), 0, "{}", text(&dry));
    assert!(text(&dry).contains("dry run"), "{}", text(&dry));
    assert!(
        names(
            &pool,
            "SELECT table_name::text FROM information_schema.views WHERE table_schema = 'bi'"
        )
        .await
        .is_empty()
    );

    let applied = rig.spot(&["apply", "--apply"]);
    assert_eq!(code(&applied), 0, "{}", text(&applied));
    assert!(text(&applied).contains("verified"), "{}", text(&applied));
    let views = names(&pool, "SELECT table_name::text FROM information_schema.views WHERE table_schema = 'bi' ORDER BY 1").await;
    assert!(views.contains(&"v1_task".to_owned()), "{views:?}");

    // Secret and protected columns are absent from the view.
    let columns = names(&pool, "SELECT column_name::text FROM information_schema.columns WHERE table_schema = 'bi' AND table_name = 'v1_task'").await;
    assert!(columns.contains(&"subject".to_owned()), "{columns:?}");
    assert!(!columns.contains(&"details".to_owned()), "{columns:?}");

    // The role: read-only by default, a statement timeout, and its own settings (a group role's
    // settings are not inherited by its members).
    let config = names(
        &pool,
        &format!(
            "SELECT array_to_string(rolconfig, ',') FROM pg_roles WHERE rolname = '{}'",
            rig.role
        ),
    )
    .await;
    let config = config.join(",");
    assert!(
        config.contains("default_transaction_read_only=on"),
        "{config}"
    );
    assert!(config.contains("statement_timeout=60s"), "{config}");

    // As the role: the views yes; the base table, a write and DDL no.
    let client = pool.get().await.unwrap();
    client
        .batch_execute(&format!("SET ROLE \"{}\"", rig.role))
        .await
        .unwrap();
    client.query("SELECT * FROM bi.v1_task", &[]).await.unwrap();
    let denied = |e: tokio_postgres::Error| e.code().map(|c| c.code().to_owned());
    let base = client
        .query("SELECT * FROM public.task", &[])
        .await
        .unwrap_err();
    assert_eq!(denied(base).as_deref(), Some("42501"));
    let write = client
        .batch_execute("SET default_transaction_read_only = off; DELETE FROM bi.v1_task")
        .await
        .unwrap_err();
    assert!(matches!(denied(write).as_deref(), Some("42501" | "42809")));
    client.batch_execute("RESET ROLE").await.unwrap();
    drop(client);

    // Re-applying is idempotent: nothing created, nothing replaced.
    let again = rig.spot(&["apply", "--apply"]);
    assert_eq!(code(&again), 0, "{}", text(&again));
    assert!(
        text(&again).contains("0 created, 0 replaced"),
        "{}",
        text(&again)
    );

    // list shows the view and the role.
    let list = text(&rig.spot(&["list"]));
    assert!(
        list.contains("v1_task") && list.contains("present"),
        "{list}"
    );

    // revoke drops the role, not the views.
    let revoked = rig.spot(&["revoke"]);
    assert_eq!(code(&revoked), 0, "{}", text(&revoked));
    assert!(
        names(
            &pool,
            &format!(
                "SELECT rolname::text FROM pg_roles WHERE rolname = '{}'",
                rig.role
            )
        )
        .await
        .is_empty()
    );
    assert!(
        !names(
            &pool,
            "SELECT table_name::text FROM information_schema.views WHERE table_schema = 'bi'"
        )
        .await
        .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_password_goes_to_a_private_file_and_never_to_a_pipe() {
    use std::os::unix::fs::PermissionsExt;
    let Some(rig) = Rig::new().await else {
        println!("SKIPPED: no PostgreSQL (set SPOT_TEST_PG_URL)");
        return;
    };
    assert_eq!(code(&rig.spot(&["apply", "--apply"])), 0);

    // Standard output is a pipe here: without --out nothing is printed and nothing is set.
    let refused = rig.spot(&["password"]);
    assert_eq!(code(&refused), 2, "{}", text(&refused));
    assert!(text(&refused).contains("--out"), "{}", text(&refused));

    let file = rig.env.root.0.join("bi-password.txt");
    let out = file.display().to_string();
    let done = rig.spot(&["password", "--out", &out]);
    assert_eq!(code(&done), 0, "{}", text(&done));
    let secret = std::fs::read_to_string(&file).unwrap();
    let secret = secret.trim();
    assert!(
        secret.len() >= 32,
        "a password of {} characters",
        secret.len()
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        !text(&done).contains(secret),
        "the password reached standard output"
    );

    // An existing file is never overwritten, and the password is not changed behind it.
    let second = rig.spot(&["password", "--out", &out]);
    assert_eq!(code(&second), 2, "{}", text(&second));
    assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), secret);

    // Signing in as the role: read-only, with the timeout. Servers that do not allow password
    // logins for it (pg_hba) are skipped, said aloud.
    let loaded = spot_cli::config::load(&rig.env.root.0, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == rig.env.site)
        .unwrap();
    let mut config = loaded.admin.clone().with_dbname(&site.database);
    config.user = rig.role.clone();
    config.password = secret.to_owned();
    let Ok(backend) = spot_postgres::PgBackend::connect(config).await else {
        println!("SKIPPED: the server does not allow a password login for the BI role");
        return;
    };
    let pool = backend.pool();
    let client = pool.get().await.unwrap();
    let shown = client
        .query_one("SHOW default_transaction_read_only", &[])
        .await
        .unwrap();
    assert_eq!(shown.get::<_, String>(0), "on");
    let timeout = client
        .query_one("SHOW statement_timeout", &[])
        .await
        .unwrap();
    assert_eq!(timeout.get::<_, String>(0), "1min");
    assert!(
        client
            .batch_execute("CREATE TABLE bi_escape (x int)")
            .await
            .is_err()
    );
    assert!(
        client
            .query("SELECT * FROM public.task", &[])
            .await
            .is_err()
    );
}
