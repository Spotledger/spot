//! WP-PRD-01.T30: `spot console`, an administrator's session over a site's documents as a named
//! user. Real PostgreSQL through `SPOT_TEST_PG_URL` (skipped with a printed reason when it is
//! unset, never faked); the command runs as a process, so what is checked is what an operator
//! gets: exit codes, output and the audit chain.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{Env, ok, run};
use serde_json::Value as Json;
use spot_core::SiteId;
use spot_migration_apply::gate::AdmissionGate;
use spot_migration_apply_pg::{PgAdmissionGate, PgApplySchema};
use spot_postgres::PgBackend;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";

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

fn password_file(env: &Env, name: &str, password: &str, mode: u32) -> PathBuf {
    let path = env.root.0.join(name);
    std::fs::write(&path, format!("{password}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let _ = mode;
    path
}

/// `spot console` as `user`, with the statements of `exec` and `extra` options.
fn console(env: &Env, user: &str, file: &PathBuf, exec: &[&str], extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_spot"));
    command
        .arg("--root")
        .arg(&env.root.0)
        .args([
            "console",
            "--site",
            &env.site,
            "--user",
            user,
            "--password-file",
        ])
        .arg(file)
        .args(extra)
        .env("XDG_STATE_HOME", env.root.0.join("state"))
        .env("USER", "opsuser")
        .stdin(std::process::Stdio::null());
    for statement in exec {
        command.args(["--exec", statement]);
    }
    command.output().unwrap()
}

fn lines(output: &Output) -> Vec<Json> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn pg_of(env: &Env) -> spot_postgres::PgConfig {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    loaded.admin.clone().with_dbname(&site.database)
}

async fn backend(env: &Env) -> PgBackend {
    PgBackend::connect(pg_of(env)).await.unwrap()
}

/// The chain records of the site whose text contains `needle`.
async fn chain_records(env: &Env, needle: &str) -> Vec<String> {
    let backend = backend(env).await;
    let client = backend.pool().get().await.unwrap();
    client
        .query(
            "SELECT record FROM spot_audit_chain WHERE site = $1 AND record LIKE $2 ORDER BY seq",
            &[&format!("identity:{}", env.site), &format!("%{needle}%")],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect()
}

async fn gate(env: &Env) -> PgAdmissionGate {
    PgAdmissionGate::new(
        backend(env).await.pool(),
        SiteId::new(&env.site).unwrap(),
        Arc::new(spot_core::SystemClock::new()),
        Duration::from_millis(1),
    )
}

async fn prepare_gate(env: &Env) {
    PgApplySchema::new(backend(env).await.pool())
        .prepare()
        .await
        .unwrap();
}

struct Fixture {
    env: Env,
    admin: PathBuf,
    manager: PathBuf,
    viewer: PathBuf,
}

/// A site with the sample app, an administrator, a Project Manager and a Viewer, and the two
/// roles assigned through the console itself.
async fn fixture() -> Option<Fixture> {
    let env = Env::new("").await?;
    env.site();
    env.set_password(&env.site, "administrator", PASSWORD);
    for email in ["manager@example.com", "viewer@example.com"] {
        ok(&env.root.0, &["add-user", &env.site, email]);
        env.set_password(&env.site, email, PASSWORD);
    }
    let admin = password_file(&env, "admin.pw", PASSWORD, 0o600);
    let manager = password_file(&env, "manager.pw", PASSWORD, 0o600);
    let viewer = password_file(&env, "viewer.pw", PASSWORD, 0o600);
    prepare_gate(&env).await;
    let fixture = Fixture {
        env,
        admin,
        manager,
        viewer,
    };
    for (email, role) in [
        ("manager@example.com", "Project Manager"),
        ("viewer@example.com", "Viewer"),
    ] {
        let user = fixture.one(&format!("list user where email = \"{email}\" fields id"));
        let role_id = fixture.one(&format!(
            "create role {{\"role_name\": \"{role}\", \"origin\": \"reference\"}}"
        ));
        let made = console(
            &fixture.env,
            "administrator",
            &fixture.admin,
            &[&format!(
                "create role_assignment {{\"user\": \"{}\", \"role\": \"{}\", \"source\": \"manual\", \"state\": \"active\", \"reason\": \"console test\"}}",
                user["id"].as_str().unwrap(),
                role_id["id"].as_str().unwrap()
            )],
            &[],
        );
        assert_eq!(code(&made), 0, "{}", text(&made));
    }
    Some(fixture)
}

impl Fixture {
    /// The first JSON line of one administrator statement.
    fn one(&self, statement: &str) -> Json {
        let output = console(&self.env, "administrator", &self.admin, &[statement], &[]);
        assert_eq!(code(&output), 0, "{statement}: {}", text(&output));
        lines(&output).remove(0)
    }

    fn manager(&self, statements: &[&str]) -> Output {
        console(
            &self.env,
            "manager@example.com",
            &self.manager,
            statements,
            &[],
        )
    }

    fn viewer(&self, statements: &[&str]) -> Output {
        console(
            &self.env,
            "viewer@example.com",
            &self.viewer,
            statements,
            &[],
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unauthenticated_or_wrong_password_session_is_refused_and_audited() {
    let Some(f) = fixture().await else { return };
    let wrong = password_file(&f.env, "wrong.pw", "not-the-password-0-Wrong", 0o600);
    let before = chain_records(&f.env, "sign_in_failed").await.len();
    let denied = console(&f.env, "manager@example.com", &wrong, &["whoami"], &[]);
    assert_eq!(code(&denied), 2, "{}", text(&denied));
    let unknown = console(&f.env, "nobody@example.com", &wrong, &["whoami"], &[]);
    assert_eq!(code(&unknown), 2);
    // A wrong password and an unknown user are told apart by nothing the operator sees.
    assert_eq!(text(&denied), text(&unknown));
    assert!(lines(&denied).is_empty() && !text(&denied).contains("not-the-password"));
    assert!(chain_records(&f.env, "sign_in_failed").await.len() > before);
    // The password file must be private.
    let open = password_file(&f.env, "open.pw", PASSWORD, 0o644);
    let output = console(&f.env, "manager@example.com", &open, &["whoami"], &[]);
    assert_eq!(code(&output), 2);
    assert!(text(&output).contains("password file"), "{}", text(&output));
    assert!(!text(&output).contains(PASSWORD));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_without_write_cannot_update_and_hidden_fields_stay_hidden() {
    let Some(f) = fixture().await else { return };
    let owner = f.one("whoami")["user"].as_str().unwrap().to_owned();
    let project = f.one(&format!(
        "create project {{\"code\": \"P-1\", \"title\": \"Alpha\", \"status\": \"Planning\", \"owner\": \"{owner}\"}}"
    ));
    let project_id = project["id"].as_str().unwrap().to_owned();
    let task = f.one(&format!(
        "create task {{\"subject\": \"Wire it\", \"project\": \"{project_id}\", \"details\": \"CONFIDENTIAL-NOTE\"}}"
    ));
    let task_id = task["id"].as_str().unwrap().to_owned();

    // The viewer reads but cannot write: denied, not an empty success, and nothing changed.
    let output = f.viewer(&[&format!(
        "update project {project_id} {{\"title\": \"Changed\"}}"
    )]);
    assert_eq!(code(&output), 2, "{}", text(&output));
    assert!(
        text(&output).contains("permission.denied"),
        "{}",
        text(&output)
    );
    assert!(!text(&output).contains("Changed"));
    let after = f.viewer(&[&format!("get project {project_id}")]);
    assert_eq!(lines(&after)[0]["title"], "Alpha");

    // The confidential field is absent for the viewer, in `get` and in `list`, and present for
    // a role the grants open it to.
    let get = f.viewer(&[&format!("get task {task_id}")]);
    assert_eq!(code(&get), 0, "{}", text(&get));
    assert!(!text(&get).contains("CONFIDENTIAL-NOTE") && lines(&get)[0].get("details").is_none());
    let list = f.viewer(&["list task"]);
    assert_eq!(code(&list), 0, "{}", text(&list));
    assert!(
        !text(&list).contains("CONFIDENTIAL-NOTE"),
        "{}",
        text(&list)
    );
    let manager = f.manager(&[&format!("get task {task_id}")]);
    assert!(
        text(&manager).contains("CONFIDENTIAL-NOTE"),
        "{}",
        text(&manager)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_stops_at_the_first_failure_and_keep_going_continues() {
    let Some(f) = fixture().await else { return };
    let missing = "get project 00000000-0000-7000-8000-000000000000";
    let stopped = f.manager(&[missing, "whoami"]);
    assert_eq!(code(&stopped), 3, "{}", text(&stopped));
    assert!(lines(&stopped).is_empty(), "{}", text(&stopped));
    let going = console(
        &f.env,
        "manager@example.com",
        &f.manager,
        &[missing, "whoami"],
        &["--keep-going"],
    );
    assert_eq!(code(&going), 3, "{}", text(&going));
    assert_eq!(lines(&going)[0]["channel"], "console");
    // A file works the same way, with comments and blank lines.
    let script = f.env.root.0.join("script.txt");
    std::fs::write(&script, "# a comment\n\nwhoami\nwhoami\n").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_spot"));
    let output = command
        .arg("--root")
        .arg(&f.env.root.0)
        .args([
            "console",
            "--site",
            &f.env.site,
            "--user",
            "manager@example.com",
            "--password-file",
        ])
        .arg(&f.manager)
        .arg("--file")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(code(&output), 0, "{}", text(&output));
    assert_eq!(lines(&output).len(), 2);
    // The statement cap is checked before anything is opened.
    let big = f.env.root.0.join("big.txt");
    std::fs::write(&big, "whoami\n".repeat(10_001)).unwrap();
    let capped = run(
        &f.env.root.0,
        &[
            "console",
            "--site",
            &f.env.site,
            "--user",
            "x@example.com",
            "--file",
            big.to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(code(&capped), 1, "{}", text(&capped));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutations_are_audited_with_the_console_channel_and_errors_hold_no_values() {
    let Some(f) = fixture().await else { return };
    let made = f.manager(&["create label {\"name\": \"Beta\", \"colour\": \"#112233\"}"]);
    assert_eq!(code(&made), 0, "{}", text(&made));
    let id = lines(&made)[0]["id"].as_str().unwrap().to_owned();
    let changed = f.manager(&[&format!("update label {id} {{\"name\": \"Beta two\"}}")]);
    assert_eq!(code(&changed), 0, "{}", text(&changed));
    let gone = f.manager(&[&format!("delete label {id}")]);
    assert_eq!(code(&gone), 0, "{}", text(&gone));
    let whoami = f.manager(&["whoami"]);
    let user = lines(&whoami)[0]["user"].as_str().unwrap().to_owned();
    for kind in ["console_create", "console_update", "console_delete"] {
        let found = chain_records(&f.env, kind).await;
        let mine: Vec<_> = found.iter().filter(|r| r.contains(user.as_str())).collect();
        assert!(!mine.is_empty(), "no {kind} record for the manager");
        for record in mine {
            assert!(
                record.replace('\\', "").contains("\"path\":\"console\""),
                "{record}"
            );
            // The OS account, from the kernel: the `USER=opsuser` the child was given is ignored.
            let me =
                String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
            assert!(record.contains(me.trim()), "{record}");
            assert!(
                !record.contains("opsuser"),
                "USER reached the trail: {record}"
            );
            assert!(
                !record.contains("Beta") && !record.contains("#112233"),
                "a value reached the trail: {record}"
            );
        }
    }
    // The trail is still a valid chain.
    let verify = ok(&f.env.root.0, &["audit", "verify", "--site", &f.env.site]);
    assert!(verify.contains(": ok"), "{verify}");
    // An error never carries what the statement said.
    let bad = f.manager(&["create label {\"name\": \"SECRET-CODE-77\", \"colour\": 12345}"]);
    assert_ne!(code(&bad), 0);
    assert!(
        !text(&bad).contains("SECRET-CODE-77") && !text(&bad).contains("12345"),
        "{}",
        text(&bad)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_the_user_cannot_see_answers_not_found_whether_or_not_it_exists() {
    let Some(f) = fixture().await else { return };
    let existing = f.one("list role_assignment limit 1")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let real = f.viewer(&[&format!("delete role_assignment {existing}")]);
    let absent = f.viewer(&["delete role_assignment 00000000-0000-7000-8000-000000000000"]);
    assert_eq!(code(&real), code(&absent));
    assert!(
        text(&real).contains("document.not_found"),
        "{}",
        text(&real)
    );
    assert_eq!(text(&real), text(&absent));
    // And it is still there.
    assert!(!lines(&f.one_list("list role_assignment limit 1")).is_empty());
}

impl Fixture {
    fn one_list(&self, statement: &str) -> Output {
        console(&self.env, "administrator", &self.admin, &[statement], &[])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_are_refused_while_the_maintenance_gate_is_closed_or_unreadable() {
    let Some(env) = Env::new("").await else {
        return;
    };
    env.site();
    env.set_password(&env.site, "administrator", PASSWORD);
    let admin = password_file(&env, "admin.pw", PASSWORD, 0o600);
    let write = "create role {\"role_name\": \"Gate Probe\", \"origin\": \"reference\"}";
    // No control schema: the gate cannot be read, so writes are refused and reads still work.
    let output = console(&env, "administrator", &admin, &[write], &[]);
    assert_eq!(code(&output), 2, "{}", text(&output));
    assert!(
        text(&output).contains("server.maintenance"),
        "{}",
        text(&output)
    );
    let read = console(&env, "administrator", &admin, &["list role limit 1"], &[]);
    assert_eq!(code(&read), 0, "{}", text(&read));

    prepare_gate(&env).await;
    let open = console(&env, "administrator", &admin, &[write], &[]);
    assert_eq!(code(&open), 0, "{}", text(&open));

    let gate = gate(&env).await;
    gate.close("a test window", 1_800_000_000_000_000)
        .await
        .unwrap();
    let closed = console(
        &env,
        "administrator",
        &admin,
        &["create role {\"role_name\": \"Second Probe\", \"origin\": \"reference\"}"],
        &[],
    );
    assert_eq!(code(&closed), 2, "{}", text(&closed));
    assert!(
        text(&closed).contains("server.maintenance"),
        "{}",
        text(&closed)
    );
    let listed = console(
        &env,
        "administrator",
        &admin,
        &["list role fields role_name"],
        &[],
    );
    assert_eq!(code(&listed), 0, "{}", text(&listed));
    assert!(
        !text(&listed).contains("Second Probe"),
        "a refused write must change nothing"
    );
    gate.open(1_800_000_000_000_001).await.unwrap();
    let reopened = console(
        &env,
        "administrator",
        &admin,
        &["create role {\"role_name\": \"Second Probe\", \"origin\": \"reference\"}"],
        &[],
    );
    assert_eq!(code(&reopened), 0, "{}", text(&reopened));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_is_off_by_default_the_table_format_prints_and_the_session_has_no_back_door() {
    let Some(f) = fixture().await else { return };
    let plain = f.manager(&["whoami"]);
    assert_eq!(code(&plain), 0, "{}", text(&plain));
    assert!(
        !f.env.root.0.join("state").exists(),
        "no history file unless asked for"
    );
    let with = console(
        &f.env,
        "manager@example.com",
        &f.manager,
        &["whoami"],
        &["--history", "--format", "table"],
    );
    assert_eq!(code(&with), 0, "{}", text(&with));
    let history = f.env.root.0.join("state/spot/console-history");
    assert!(
        std::fs::read_to_string(&history)
            .unwrap()
            .contains("whoami")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&history).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let table = console(
        &f.env,
        "administrator",
        &f.admin,
        &["list role fields role_name limit 2"],
        &["--format", "table"],
    );
    assert!(
        text(&table).starts_with("id ") && text(&table).contains("role_name"),
        "{}",
        text(&table)
    );
    // The module offers no privileged actor and no raw SQL: grep it.
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands/console.rs"),
    )
    .unwrap();
    let production: String = source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for banned in [
        ["Actor::", "System"].concat(),
        ["as-", "system"].concat(),
        ["as_", "system"].concat(),
        ["batch_", "execute"].concat(),
        ["tokio_", "postgres"].concat(),
        ["SELECT", " "].concat(),
        ["INSERT", " INTO"].concat(),
        [".unw", "rap()"].concat(),
        [".exp", "ect("].concat(),
    ] {
        assert!(
            !production.contains(&banned),
            "console.rs contains `{banned}`"
        );
    }
}

#[test]
fn a_statement_that_does_not_parse_is_refused_before_anything_opens_and_echoes_nothing() {
    let root = std::env::temp_dir();
    let bad = [
        "",
        "drop table SECRET-A",
        "get Task SECRET-A",
        "get task",
        "get task a b",
        "list task limit 5000",
        "list task where SECRET-A ~ 1",
        "list task where \"SECRET-A",
        "create task",
        "create task {\"x\": \"SECRET-A\"",
        "update task a",
        "call a.b",
        "delete task ../SECRET-A",
    ];
    for statement in bad {
        let args = [
            "console",
            "--site",
            "x",
            "--user",
            "y@example.com",
            "--exec",
            statement,
        ];
        let output = run(&root, &args, None);
        assert_eq!(code(&output), 1, "{statement:?}: {}", text(&output));
        assert!(
            !text(&output).contains("SECRET-A"),
            "{statement:?} was echoed"
        );
    }
    // One bad statement in a script stops the whole script before it starts.
    let script = [
        "console",
        "--site",
        "x",
        "--user",
        "y@example.com",
        "--exec",
        "whoami",
        "--exec",
        "nope",
    ];
    assert_eq!(code(&run(&root, &script, None)), 1);
}
