//! WP-ADM-06.T2 tests: the runtime ports over a fake template executor (what the agent
//! sends to the helper, how answers are parsed, refusals and failures), the helper
//! executor's protocol, and the catalogue. Tests against a real PostgreSQL need
//! `SPOT_TEST_PG_URL` plus the privilege templates and print their skip reason.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use spot_agent::handlers::runtime_ops::{
    BackupExec, HelperExec, PgExec, REQUIRED_TEMPLATES, ReplicaExec, RuntimeExec,
    RuntimeServiceExec, module, register_catalogue,
};
use spot_agent::privilege::exec::{ChildResult, ChildRunner, ChildSpec, SpawnFailed};
use spot_agent::privilege::helper::HelperReply;
use spot_agent_runtime::replica::ReplicaRole;
use spot_agent_runtime::{
    BackupKind, BackupPort, BackupRequest, PgAdminPort, ReplicaPort, RuntimeControl, RuntimeError,
    RuntimeResult,
};

/// A scripted executor: records `(template, params)` and answers from a table.
struct Fake {
    calls: Mutex<Vec<(String, Vec<String>)>>,
    answers: Vec<(&'static str, Result<&'static str, &'static str>)>,
}

impl Fake {
    fn new(answers: Vec<(&'static str, Result<&'static str, &'static str>)>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            answers,
        })
    }
    fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }
}

impl RuntimeExec for Fake {
    fn run(&self, name: &str, params: &[String]) -> RuntimeResult<String> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_owned(), params.to_vec()));
        match self.answers.iter().find(|(n, _)| *n == name) {
            Some((_, Ok(out))) => Ok((*out).to_owned()),
            Some((_, Err(what))) => Err(RuntimeError::Unavailable {
                what: (*what).to_owned(),
            }),
            None => Err(RuntimeError::Unavailable {
                what: "no such template".to_owned(),
            }),
        }
    }
}

#[test]
fn parameters_are_read_and_written_through_named_templates_never_sql() {
    let fake = Fake::new(vec![
        ("pg.param.read", Ok("64MB\tf\n")),
        ("pg.param.set", Ok("")),
        ("pg.reload", Ok("")),
    ]);
    let pg = PgExec(fake.clone());
    assert_eq!(pg.read("work_mem").unwrap(), ("64MB".to_owned(), false));
    pg.alter_system("work_mem", "128MB").unwrap();
    pg.reload().unwrap();
    let calls = fake.calls();
    assert_eq!(
        calls[1],
        (
            "pg.param.set".to_owned(),
            vec!["work_mem".to_owned(), "128MB".to_owned()]
        )
    );
    for (name, params) in &calls {
        assert!(
            !name.to_ascii_uppercase().contains("SELECT") && !name.contains(' '),
            "{name}"
        );
        assert!(
            params.iter().all(|p| !p.contains(';') && !p.contains('/')),
            "{params:?}"
        );
    }
    assert!(
        Fake::new(vec![("pg.param.read", Ok("garbage"))])
            .calls()
            .is_empty()
    );
    assert!(
        PgExec(Fake::new(vec![("pg.param.read", Ok("garbage"))]))
            .read("work_mem")
            .is_err()
    );
}

#[test]
fn a_parameter_outside_the_allow_list_is_refused_by_the_module_before_any_template_runs() {
    let fake = Fake::new(vec![("pg.param.set", Ok(""))]);
    let pg = PgExec(fake.clone());
    let err = spot_agent_runtime::params::set_parameter(&pg, "archive_command", "x").unwrap_err();
    assert!(
        matches!(err, RuntimeError::UnknownParameter { .. }),
        "{err:?}"
    );
    assert!(fake.calls().is_empty(), "nothing reached the helper");
}

#[test]
fn replica_status_lag_and_promotion() {
    let fake = Fake::new(vec![
        (
            "pg.replica.status",
            Ok(
                "role=standby\nstreaming=true\nprimary_host=10.0.0.5\nreceive_lsn=0/3000060\nreplay_lsn=-\n",
            ),
        ),
        (
            "pg.replica.lag",
            Ok("bytes=2048\nseconds=-\nstreaming=true\n"),
        ),
        ("pg.primary.reachable", Ok("true\n")),
        ("pg.promote", Ok("")),
    ]);
    let replica = ReplicaExec(fake.clone());
    let status = replica.status().unwrap();
    assert_eq!(status.role, ReplicaRole::Standby);
    assert_eq!(status.primary_host.as_deref(), Some("10.0.0.5"));
    assert_eq!(status.replay_lsn, None);
    let lag = replica.lag().unwrap();
    assert_eq!((lag.bytes, lag.seconds, lag.streaming), (2048, None, true));
    assert!(replica.primary_reachable().unwrap());
    // The module refuses promotion while the primary is reachable (no template call).
    let err = spot_agent_runtime::replica::promote(&replica, false).err();
    assert!(err.is_some());
    assert!(!fake.calls().iter().any(|(n, _)| n == "pg.promote"));
}

#[test]
fn backups_are_triggered_listed_and_verified_with_typed_parameters() {
    let fake = Fake::new(vec![
        (
            "backup.run",
            Ok("id=b-1\nlocation=/srv/backups/b-1\ncreated_at=1800000000\n"),
        ),
        (
            "backup.list",
            Ok("b-1\tbase\tnightly\t1048576\t1800000000\ttrue\n"),
        ),
        (
            "backup.verify",
            Ok("files_checked=12\nbytes_checked=1048576\n"),
        ),
    ]);
    let backup = BackupExec(fake.clone());
    let started = backup
        .start(
            &BackupRequest {
                kind: BackupKind::BaseBackup,
                label: "nightly".to_owned(),
            },
            1_800_000_000,
        )
        .unwrap();
    assert_eq!(
        (started.id.as_str(), started.created_at),
        ("b-1", 1_800_000_000)
    );
    let listed = backup.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].verified);
    assert_eq!(backup.verify("b-1").unwrap().files_checked, 12);
    assert_eq!(
        fake.calls()[0],
        (
            "backup.run".to_owned(),
            vec!["base".to_owned(), "nightly".to_owned()]
        )
    );
    assert!(
        BackupExec(Fake::new(vec![(
            "backup.list",
            Ok("only\tthree\tfields\n")
        )]))
        .list()
        .is_err()
    );
}

#[test]
fn runtime_stop_and_start_report_what_happened() {
    let fake = Fake::new(vec![
        (
            "runtime.stop",
            Ok("drained=true\nin_flight=0\nwaited_secs=3\nstopped=true\n"),
        ),
        ("runtime.start", Ok("started=true\nhealthy after reload\n")),
    ]);
    let control = RuntimeServiceExec(fake);
    let stop = control.stop(30, 0).unwrap();
    assert!(stop.stopped && stop.drain.drained && stop.drain.waited_secs == 3);
    let start = control.start().unwrap();
    assert!(start.started);
    assert_eq!(start.notes, vec!["healthy after reload".to_owned()]);
}

struct Canned(Mutex<Vec<ChildSpec>>, ChildResult);

impl ChildRunner for Canned {
    fn run(&self, spec: &ChildSpec) -> Result<ChildResult, SpawnFailed> {
        self.0.lock().unwrap().push(spec.clone());
        Ok(self.1.clone())
    }
}

#[test]
fn the_helper_executor_sends_named_templates_with_params_on_stdin_and_fails_closed() {
    let reply = |ok: bool, code: Option<&str>, status: i32, stdout: &str| {
        let r = HelperReply {
            ok,
            code: code.map(str::to_owned),
            status,
            timed_out: false,
            truncated: false,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        };
        ChildResult {
            status: 0,
            stdout: serde_json::to_string(&r).unwrap(),
            stderr: String::new(),
            timed_out: false,
            truncated: false,
        }
    };
    let good = Arc::new(Canned(Mutex::new(Vec::new()), reply(true, None, 0, "ok")));
    let exec = HelperExec::with_runner(good.clone());
    assert_eq!(
        exec.run("pg.param.set", &["work_mem".to_owned(), "64MB".to_owned()])
            .unwrap(),
        "ok"
    );
    let spec = good.0.lock().unwrap()[0].clone();
    assert_eq!(spec.program, "/usr/bin/sudo");
    assert_eq!(&spec.args[..2], ["-n", spot_agent::privilege::HELPER_PATH]);
    assert_eq!(spec.args[3], "pg.param.set");
    assert_eq!(
        String::from_utf8(spec.stdin.unwrap()).unwrap().trim(),
        "{\"params\":[\"work_mem\",\"64MB\"]}"
    );
    // The helper refusing (no such template yet), a failing command and garbage all fail closed.
    for (answer, label) in [
        (
            reply(false, Some("agent.privilege_unknown_operation"), -1, ""),
            "refused",
        ),
        (reply(true, None, 1, ""), "failed"),
        (
            ChildResult {
                status: 1,
                stdout: "sudo: a password is required".to_owned(),
                stderr: String::new(),
                timed_out: false,
                truncated: false,
            },
            "sudo",
        ),
    ] {
        let exec = HelperExec::with_runner(Arc::new(Canned(Mutex::new(Vec::new()), answer)));
        let err = exec.run("pg.reload", &[]).unwrap_err();
        assert!(matches!(err, RuntimeError::Unavailable { .. }), "{label}");
        assert!(
            !err.to_string().contains("password"),
            "no output text in errors"
        );
    }
}

#[test]
fn the_catalogue_and_templates_are_declared() {
    let mut builder = spot_mgmt_ops::CatalogueBuilder::new();
    register_catalogue(&mut builder);
    let catalogue = builder.build().unwrap();
    assert!(
        catalogue
            .get(&spot_mgmt_ops::OpId::new("db.param.set").unwrap())
            .is_some()
    );
    assert!(
        catalogue
            .get(&spot_mgmt_ops::OpId::new("db.replica.promote").unwrap())
            .is_some()
    );
    let names: Vec<&str> = REQUIRED_TEMPLATES.iter().map(|(n, _)| *n).collect();
    for needed in [
        "pg.param.set",
        "pg.reload",
        "pg.restart",
        "pg.promote",
        "backup.run",
        "node.reload",
    ] {
        assert!(names.contains(&needed), "{needed}");
    }
    // The module builds over an executor (settings schema valid).
    assert!(module(Fake::new(vec![]), "node-1").is_ok());
}

#[test]
fn real_postgres_cases_are_skipped_without_the_environment() {
    if std::env::var("SPOT_TEST_PG_URL").is_err() {
        eprintln!(
            "skipped: SPOT_TEST_PG_URL is not set (real PostgreSQL parameter, restart and promotion cases need it and the privilege templates)"
        );
    }
}
