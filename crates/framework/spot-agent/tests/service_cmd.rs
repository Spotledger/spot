//! WP-ADM-02.T7 tests: the hardened systemd unit (rendered, parsed, planned, run against a
//! fake host), the Windows service commands against a fake SCM, the `service` and
//! `audit-verify` grammar, and `audit-verify` on a good, a tampered and a truncated log.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use spot_agent::audit::{LocalAuditLog, event_record, verify_file};
use spot_agent::cli::{Command, parse};
use spot_agent::service_cmd::{
    ServiceAction, ServiceEnv, ServiceRequest, parse_service_request, run_linux, run_windows,
};
use spot_agent::systemd::{
    self, Existing, Host, Step, UnitRefused, UnitSettings, execute, plan_install, plan_uninstall,
    render_unit,
};
use spot_agent::winsvc::Scm;

fn args(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

fn settings() -> UnitSettings {
    UnitSettings {
        binary: "/usr/local/bin/spot-agent".to_owned(),
        user: "spot-agent".to_owned(),
        state_dir: "/var/lib/spot-agent".to_owned(),
        install_root: "/opt/spot-agent".to_owned(),
        privileged_helper: false,
    }
}

fn env() -> ServiceEnv {
    ServiceEnv {
        executable: "/usr/local/bin/spot-agent".to_owned(),
        state_dir: "/var/lib/spot-agent".to_owned(),
        install_root: "/opt/spot-agent".to_owned(),
    }
}

/// A unit file as `section -> key -> values` (a key may repeat).
fn parse_unit(text: &str) -> BTreeMap<String, BTreeMap<String, Vec<String>>> {
    let mut out: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        let (key, value) = line.split_once('=').expect("key=value line");
        out.entry(section.clone())
            .or_default()
            .entry(key.to_owned())
            .or_default()
            .push(value.to_owned());
    }
    out
}

fn one(unit: &BTreeMap<String, BTreeMap<String, Vec<String>>>, key: &str) -> String {
    let values = &unit["Service"][key];
    assert_eq!(values.len(), 1, "{key} appears once");
    values[0].clone()
}

#[test]
fn the_unit_is_hardened_and_parses() {
    let text = render_unit(&settings()).unwrap();
    assert!(!text.contains('@'), "no placeholder is left");
    let unit = parse_unit(&text);
    assert_eq!(one(&unit, "User"), "spot-agent");
    assert_eq!(one(&unit, "Group"), "spot-agent");
    assert_eq!(
        one(&unit, "ExecStart"),
        "/usr/local/bin/spot-agent --state-dir /var/lib/spot-agent run"
    );
    assert_eq!(one(&unit, "NoNewPrivileges"), "yes");
    assert_eq!(one(&unit, "ProtectSystem"), "strict");
    assert_eq!(one(&unit, "ProtectHome"), "yes");
    assert_eq!(one(&unit, "PrivateTmp"), "yes");
    assert_eq!(one(&unit, "CapabilityBoundingSet"), "");
    assert_eq!(one(&unit, "RestrictSUIDSGID"), "yes");
    assert_eq!(one(&unit, "Restart"), "on-failure");
    assert_eq!(
        unit["Service"]["ReadWritePaths"],
        vec![
            "/var/lib/spot-agent".to_owned(),
            "-/opt/spot-agent".to_owned()
        ]
    );
    assert_eq!(
        unit["Install"]["WantedBy"],
        vec!["multi-user.target".to_owned()]
    );
    assert!(unit["Unit"]["After"][0].contains("network-online.target"));
}

#[test]
fn the_privileged_helper_option_relaxes_exactly_two_lines() {
    let strict = render_unit(&settings()).unwrap();
    let relaxed = render_unit(&UnitSettings {
        privileged_helper: true,
        ..settings()
    })
    .unwrap();
    let (strict, relaxed) = (parse_unit(&strict), parse_unit(&relaxed));
    assert_eq!(one(&relaxed, "NoNewPrivileges"), "no");
    assert_eq!(one(&relaxed, "RestrictSUIDSGID"), "no");
    for (key, values) in &strict["Service"] {
        if key != "NoNewPrivileges" && key != "RestrictSUIDSGID" {
            assert_eq!(&relaxed["Service"][key], values, "{key} is unchanged");
        }
    }
}

#[test]
fn unsafe_settings_are_refused() {
    let with = |change: fn(&mut UnitSettings)| {
        let mut s = settings();
        change(&mut s);
        render_unit(&s).unwrap_err()
    };
    assert_eq!(
        with(|s| s.binary = "/tmp/spot-agent".into()),
        UnitRefused::Binary
    );
    assert_eq!(
        with(|s| s.binary = "/home/alice/spot-agent".into()),
        UnitRefused::Binary
    );
    assert_eq!(
        with(|s| s.binary = "spot-agent".into()),
        UnitRefused::Binary
    );
    assert_eq!(
        with(|s| s.binary = "/opt/a b/spot-agent".into()),
        UnitRefused::Binary
    );
    assert_eq!(
        with(|s| s.binary = "/opt/../bin/x".into()),
        UnitRefused::Binary
    );
    assert_eq!(with(|s| s.binary = "/opt/%h/x".into()), UnitRefused::Binary);
    assert_eq!(with(|s| s.user = "root".into()), UnitRefused::User);
    assert_eq!(with(|s| s.user = "Bad Name".into()), UnitRefused::User);
    assert_eq!(
        with(|s| s.state_dir = "/var/lib/x y".into()),
        UnitRefused::Directory
    );
    assert_eq!(
        with(|s| s.install_root = "opt".into()),
        UnitRefused::Directory
    );
}

#[test]
fn the_install_plan_is_ordered_and_idempotent() {
    let fresh = plan_install(&settings(), &Existing::default()).unwrap();
    let words: Vec<String> = fresh
        .iter()
        .map(|step| match step {
            Step::Run(argv) => argv[..argv.len().min(2)].join(" "),
            Step::Write { path, .. } => format!("write {}", path.display()),
            Step::Remove(path) => format!("remove {}", path.display()),
        })
        .collect();
    assert_eq!(
        words,
        vec![
            "useradd --system",
            "install -d",
            "write /etc/systemd/system/spot-agent.service",
            "systemctl daemon-reload",
            "systemctl enable",
        ]
    );
    // The account is a system account without a login shell and without a home to create.
    let Step::Run(useradd) = &fresh[0] else {
        panic!("useradd first")
    };
    assert!(useradd.contains(&"--no-create-home".to_owned()));
    assert!(useradd.contains(&"/usr/sbin/nologin".to_owned()));
    // The state directory is 0700 and owned by the agent account.
    let Step::Run(dir) = &fresh[1] else {
        panic!("state dir second")
    };
    assert_eq!(&dir[..4], &["install", "-d", "-m", "0700"]);

    let unit = render_unit(&settings()).unwrap();
    let again = plan_install(
        &settings(),
        &Existing {
            unit: Some(unit),
            user_exists: true,
        },
    )
    .unwrap();
    assert!(
        again.iter().all(|s| !matches!(s, Step::Write { .. })),
        "an identical unit is not rewritten"
    );
    assert!(
        again
            .iter()
            .all(|s| !matches!(s, Step::Run(a) if a[0] == "useradd"))
    );
    let changed = plan_install(
        &settings(),
        &Existing {
            unit: Some("[Unit]\n".to_owned()),
            user_exists: true,
        },
    )
    .unwrap();
    assert!(
        changed
            .iter()
            .any(|s| matches!(s, Step::Write { mode: 0o644, .. }))
    );
}

#[test]
fn uninstall_keeps_the_account_and_the_state() {
    assert!(plan_uninstall(&Existing::default()).is_empty());
    let plan = plan_uninstall(&Existing {
        unit: Some("x".to_owned()),
        user_exists: true,
    });
    assert_eq!(plan.len(), 3);
    assert!(
        matches!(&plan[0], Step::Run(a) if a == &["systemctl", "disable", "--now", "spot-agent.service"])
    );
    assert!(
        matches!(&plan[1], Step::Remove(p) if p == Path::new("/etc/systemd/system/spot-agent.service"))
    );
    assert!(
        plan.iter()
            .all(|s| !matches!(s, Step::Run(a) if a[0] == "userdel"))
    );
}

/// A host that records what the plan does and can fail on a chosen program.
#[derive(Default)]
struct FakeHost {
    log: Mutex<Vec<String>>,
    unit: Mutex<Option<String>>,
    user: bool,
    fail_on: Option<(&'static str, i32)>,
}

impl Host for FakeHost {
    fn run(&self, argv: &[String]) -> std::io::Result<i32> {
        self.log.lock().unwrap().push(argv.join(" "));
        Ok(match self.fail_on {
            Some((program, code)) if argv.iter().any(|a| a == program) => code,
            _ => 0,
        })
    }
    fn write(&self, path: &Path, content: &str, mode: u32) -> std::io::Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("write {} {mode:o}", path.display()));
        *self.unit.lock().unwrap() = Some(content.to_owned());
        Ok(())
    }
    fn remove(&self, path: &Path) -> std::io::Result<()> {
        self.log
            .lock()
            .unwrap()
            .push(format!("remove {}", path.display()));
        *self.unit.lock().unwrap() = None;
        Ok(())
    }
    fn read_unit(&self) -> Option<String> {
        self.unit.lock().unwrap().clone()
    }
    fn user_exists(&self, _user: &str) -> bool {
        self.user
    }
}

fn request(action: ServiceAction) -> ServiceRequest {
    ServiceRequest {
        action,
        print: false,
        binary: None,
        user: None,
        privileged_helper: false,
    }
}

#[test]
fn linux_install_then_a_second_install_changes_nothing() {
    let host = FakeHost::default();
    let first = run_linux(&request(ServiceAction::Install), &env(), &host);
    assert_eq!(first.code, 0, "{first:?}");
    let written = host.unit.lock().unwrap().clone().unwrap();
    assert_eq!(written, render_unit(&settings()).unwrap());
    let calls = host.log.lock().unwrap().len();
    assert!(calls >= 5);
    // Second run: the unit exists and the account exists.
    let host2 = FakeHost {
        unit: Mutex::new(Some(written)),
        user: true,
        ..FakeHost::default()
    };
    assert_eq!(
        run_linux(&request(ServiceAction::Install), &env(), &host2).code,
        0
    );
    let log = host2.log.lock().unwrap().clone();
    assert!(
        log.iter()
            .all(|l| !l.starts_with("write") && !l.starts_with("useradd"))
    );
    assert!(
        log.iter()
            .any(|l| l == "systemctl enable spot-agent.service")
    );
}

#[test]
fn a_failing_step_stops_the_install_with_its_code() {
    let host = FakeHost {
        fail_on: Some(("daemon-reload", 4)),
        ..FakeHost::default()
    };
    let outcome = run_linux(&request(ServiceAction::Install), &env(), &host);
    assert_eq!(outcome.code, 4);
    assert!(
        !host
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("enable")),
        "nothing runs after the failure"
    );
    assert_eq!(execute(&host, &[]), 0);
}

#[test]
fn print_shows_the_plan_and_changes_nothing() {
    let host = FakeHost::default();
    let mut req = request(ServiceAction::Install);
    req.print = true;
    let outcome = run_linux(&req, &env(), &host);
    assert_eq!(outcome.code, 0);
    assert!(host.log.lock().unwrap().is_empty(), "no command, no write");
    let text = outcome.lines.join("\n");
    assert!(text.contains("useradd --system"));
    assert!(text.contains("write /etc/systemd/system/spot-agent.service (mode 0644)"));
    assert!(text.contains("NoNewPrivileges=yes"));
    assert!(text.contains("systemctl enable spot-agent.service"));
    for verb in [
        ServiceAction::Start,
        ServiceAction::Stop,
        ServiceAction::Status,
        ServiceAction::Uninstall,
    ] {
        let mut r = request(verb);
        r.print = true;
        let out = run_linux(&r, &env(), &host);
        assert_eq!(out.code, 0);
        assert!(out.lines.iter().any(|l| l.contains("systemctl")));
    }
    assert!(host.log.lock().unwrap().is_empty());
}

#[test]
fn an_unsafe_binary_is_refused_with_code_10_and_nothing_runs() {
    let host = FakeHost::default();
    let mut req = request(ServiceAction::Install);
    req.binary = Some("/tmp/spot-agent".to_owned());
    let outcome = run_linux(&req, &env(), &host);
    assert_eq!(outcome.code, 10);
    assert!(host.log.lock().unwrap().is_empty());
}

#[test]
fn linux_start_stop_status_uninstall_use_systemctl() {
    let unit = render_unit(&settings()).unwrap();
    let host = FakeHost {
        unit: Mutex::new(Some(unit)),
        user: true,
        ..FakeHost::default()
    };
    for (action, expect) in [
        (ServiceAction::Start, "systemctl start spot-agent.service"),
        (ServiceAction::Stop, "systemctl stop spot-agent.service"),
        (
            ServiceAction::Status,
            "systemctl status --no-pager spot-agent.service",
        ),
    ] {
        assert_eq!(run_linux(&request(action), &env(), &host).code, 0);
        assert_eq!(host.log.lock().unwrap().last().unwrap(), expect);
    }
    assert_eq!(
        run_linux(&request(ServiceAction::Uninstall), &env(), &host).code,
        0
    );
    assert!(host.unit.lock().unwrap().is_none());
    // Uninstalling again has nothing to do.
    let again = run_linux(&request(ServiceAction::Uninstall), &env(), &host);
    assert_eq!(
        (again.code, again.lines[0].as_str()),
        (0, "nothing to do: not installed")
    );
    // `status` of an inactive unit passes systemctl's code through.
    let inactive = FakeHost {
        fail_on: Some(("status", 3)),
        ..FakeHost::default()
    };
    assert_eq!(
        run_linux(&request(ServiceAction::Status), &env(), &inactive).code,
        3
    );
    // `service run` is the Windows entry.
    assert_eq!(
        run_linux(
            &request(ServiceAction::Run { helper: false }),
            &env(),
            &host
        )
        .code,
        2
    );
}

/// A fake `sc.exe`: services by name with their `qc` text; `create` and `config` store the
/// requested definition; one verb can be made to fail with a code.
#[derive(Default)]
struct FakeScm {
    services: Mutex<BTreeMap<String, String>>,
    calls: Mutex<Vec<Vec<String>>>,
    fail: Option<(&'static str, i32)>,
    state: &'static str,
}

impl FakeScm {
    fn verbs(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c[0].clone())
            .collect()
    }
}

impl Scm for FakeScm {
    fn sc(&self, args: &[String]) -> std::io::Result<(i32, String)> {
        self.calls.lock().unwrap().push(args.to_vec());
        let verb = args[0].as_str();
        if let Some((failing, code)) = self.fail
            && verb == failing
        {
            return Ok((code, String::new()));
        }
        let value_after = |key: &str| {
            args.iter()
                .position(|a| a == key)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        match verb {
            "qc" => Ok(match self.services.lock().unwrap().get(&args[1]) {
                Some(text) => (0, text.clone()),
                None => (1060, String::new()),
            }),
            "create" | "config" => {
                let delayed = if value_after("start=").as_deref() == Some("delayed-auto") {
                    " (DELAYED)"
                } else {
                    ""
                };
                let account = value_after("obj=").unwrap_or_else(|| "LocalSystem".to_owned());
                let text = format!(
                    "SERVICE_NAME: {}\n        START_TYPE         : 2   AUTO_START{delayed}\n        BINARY_PATH_NAME   : {}\n        SERVICE_START_NAME : {account}\n",
                    args[1],
                    value_after("binPath=").unwrap_or_default()
                );
                self.services.lock().unwrap().insert(args[1].clone(), text);
                Ok((0, String::new()))
            }
            "delete" => {
                self.services.lock().unwrap().remove(&args[1]);
                Ok((0, String::new()))
            }
            "query" => Ok((
                0,
                format!(
                    "SERVICE_NAME: {}\n        STATE              : 4  {}\n",
                    args[1], self.state
                ),
            )),
            _ => Ok((0, String::new())),
        }
    }
}

const WIN_BIN: &str = r"C:\Program Files\Spotledger\Agent\spot-agent.exe";

fn win_env() -> ServiceEnv {
    ServiceEnv {
        executable: WIN_BIN.to_owned(),
        ..env()
    }
}

#[test]
fn windows_install_creates_both_services_and_a_second_install_is_a_no_op() {
    let scm = FakeScm::default();
    let first = run_windows(&request(ServiceAction::Install), &win_env(), &scm);
    assert_eq!(first.code, 0, "{first:?}");
    assert_eq!(scm.verbs().iter().filter(|v| *v == "create").count(), 2);
    assert_eq!(scm.services.lock().unwrap().len(), 2);
    let before = scm.calls.lock().unwrap().len();
    let second = run_windows(&request(ServiceAction::Install), &win_env(), &scm);
    assert_eq!(second.code, 0);
    let after: Vec<String> = scm.calls.lock().unwrap()[before..]
        .iter()
        .map(|c| c[0].clone())
        .collect();
    assert!(after.iter().all(|v| v == "qc"), "only queries: {after:?}");
    // A changed binary path updates the service.
    let mut moved = request(ServiceAction::Install);
    moved.binary = Some(r"C:\Program Files\Other\spot-agent.exe".to_owned());
    assert_eq!(run_windows(&moved, &win_env(), &scm).code, 0);
    assert!(scm.verbs().iter().any(|v| v == "config"));
}

#[test]
fn windows_refuses_an_unsafe_path_and_reports_access_denied() {
    let scm = FakeScm::default();
    let mut req = request(ServiceAction::Install);
    req.binary = Some(r"C:\Users\alice\spot-agent.exe".to_owned());
    let out = run_windows(&req, &win_env(), &scm);
    assert_eq!(out.code, 10);
    assert!(!scm.verbs().iter().any(|v| v == "create"));
    let denied = FakeScm {
        fail: Some(("create", 5)),
        ..FakeScm::default()
    };
    assert_eq!(
        run_windows(&request(ServiceAction::Install), &win_env(), &denied).code,
        5
    );
}

#[test]
fn windows_start_stop_status_and_uninstall() {
    let scm = FakeScm::default();
    assert_eq!(
        run_windows(&request(ServiceAction::Install), &win_env(), &scm).code,
        0
    );
    assert_eq!(
        run_windows(&request(ServiceAction::Start), &win_env(), &scm).code,
        0
    );
    assert_eq!(
        run_windows(&request(ServiceAction::Stop), &win_env(), &scm).code,
        0
    );
    // Already running / already stopped are fine.
    let running = FakeScm {
        fail: Some(("start", 1056)),
        ..FakeScm::default()
    };
    assert_eq!(
        run_windows(&request(ServiceAction::Start), &win_env(), &running).code,
        0
    );
    let stopped = FakeScm {
        fail: Some(("stop", 1062)),
        ..FakeScm::default()
    };
    assert_eq!(
        run_windows(&request(ServiceAction::Stop), &win_env(), &stopped).code,
        0
    );
    // Status: running is 0, stopped is 3, not installed is 3.
    let up = FakeScm {
        state: "RUNNING",
        ..FakeScm::default()
    };
    assert_eq!(
        run_windows(&request(ServiceAction::Status), &win_env(), &up).code,
        0
    );
    let down = FakeScm {
        state: "STOPPED",
        ..FakeScm::default()
    };
    let out = run_windows(&request(ServiceAction::Status), &win_env(), &down);
    assert_eq!(
        (out.code, out.lines[0].as_str()),
        (3, "SpotledgerAgent: stopped")
    );
    let missing = FakeScm {
        fail: Some(("query", 1060)),
        ..FakeScm::default()
    };
    assert_eq!(
        run_windows(&request(ServiceAction::Status), &win_env(), &missing).code,
        3
    );
    // Uninstall removes both, a second uninstall only queries.
    assert_eq!(
        run_windows(&request(ServiceAction::Uninstall), &win_env(), &scm).code,
        0
    );
    assert!(scm.services.lock().unwrap().is_empty());
    let before = scm.calls.lock().unwrap().len();
    assert_eq!(
        run_windows(&request(ServiceAction::Uninstall), &win_env(), &scm).code,
        0
    );
    assert!(
        scm.calls.lock().unwrap()[before..]
            .iter()
            .all(|c| c[0] == "qc")
    );
}

#[test]
fn windows_print_runs_nothing() {
    let scm = FakeScm::default();
    for action in [
        ServiceAction::Install,
        ServiceAction::Uninstall,
        ServiceAction::Start,
        ServiceAction::Stop,
        ServiceAction::Status,
    ] {
        let mut req = request(action);
        req.print = true;
        let out = run_windows(&req, &win_env(), &scm);
        assert_eq!(out.code, 0);
        assert!(
            out.lines.iter().all(|l| l.starts_with("sc.exe ")),
            "{:?}",
            out.lines
        );
    }
    assert!(scm.calls.lock().unwrap().is_empty());
    let mut install = request(ServiceAction::Install);
    install.print = true;
    let text = run_windows(&install, &win_env(), &scm).lines.join("\n");
    assert!(text.contains("sc.exe create SpotledgerAgent binPath= \"\\\"C:\\Program Files"));
}

#[test]
fn the_service_and_audit_verify_grammar() {
    let ok = |text: &str| parse_service_request(&args(text)).unwrap();
    assert_eq!(ok("install").action, ServiceAction::Install);
    let full = ok("install --binary /usr/bin/spot-agent --user spot --privileged-helper --print");
    assert_eq!(full.binary.as_deref(), Some("/usr/bin/spot-agent"));
    assert_eq!(full.user.as_deref(), Some("spot"));
    assert!(full.privileged_helper && full.print);
    assert!(ok("stop --print").print);
    assert_eq!(
        ok("run --helper").action,
        ServiceAction::Run { helper: true }
    );
    for bad in [
        "",
        "frobnicate",
        "start --binary x",
        "status --user a",
        "run --print",
        "install --binary",
        "stop --helper",
    ] {
        assert!(
            parse_service_request(&args(bad)).is_err(),
            "{bad:?} must be refused"
        );
    }
    let no_file = |_: &str| None;
    let cli = parse(&args("--state-dir /x service install --print"), &no_file).unwrap();
    assert!(matches!(cli.command, Command::Service(ref r) if r.print));
    let verify = parse(
        &args("audit-verify --file /tmp/a.log --agent-id agent-1"),
        &no_file,
    )
    .unwrap();
    assert_eq!(
        verify.command,
        Command::AuditVerify {
            file: Some("/tmp/a.log".to_owned()),
            agent_id: Some("agent-1".to_owned())
        }
    );
    assert!(parse(&args("run --file x"), &no_file).is_err());
    assert!(parse(&args("audit-verify --token ABCDEFGHIJKLMNOPQR"), &no_file).is_err());
}

fn write_log(path: &Path, entries: u64) {
    let log = LocalAuditLog::open(path, "agent-1").unwrap();
    for i in 0..entries {
        spot_agent_core::ResultLog::append(
            &log,
            &event_record(
                "agent-1",
                "x.y",
                "executed",
                None,
                100 + i64::try_from(i).unwrap(),
            ),
        )
        .unwrap();
    }
}

fn run_verify(path: &Path) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_spot-agent"))
        .args(["audit-verify", "--agent-id", "agent-1", "--file"])
        .arg(path)
        .output()
        .unwrap()
}

#[test]
fn audit_verify_passes_a_good_log_and_names_the_first_bad_line() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("audit.log");
    write_log(&path, 4);
    let good = run_verify(&path);
    assert!(good.status.success());
    assert!(String::from_utf8_lossy(&good.stdout).contains("4 entries"));

    // A tampered line 3.
    let original = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
    lines[2] = lines[2].replacen("executed", "refused!", 1);
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    let bad = run_verify(&path);
    assert_eq!(bad.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("line 3"));
    assert!(verify_file(&path, "agent-1").is_err());
}

#[test]
fn audit_verify_fails_a_truncated_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.log");
    write_log(&path, 3);
    let original = std::fs::read(&path).unwrap();
    // Cut inside the last entry.
    std::fs::write(&path, &original[..original.len() - 20]).unwrap();
    let cut = run_verify(&path);
    assert_eq!(cut.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&cut.stderr).contains("line 3"));
    // Cut exactly before the final newline: the entry still parses but is incomplete.
    std::fs::write(&path, &original[..original.len() - 1]).unwrap();
    let nonl = run_verify(&path);
    assert_eq!(nonl.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&nonl.stderr).contains("line 3"));
    // A wrong agent id is a broken chain at the first line.
    std::fs::write(&path, &original).unwrap();
    assert!(verify_file(&path, "agent-2").is_err());
    assert_eq!(systemd::UNIT_NAME, "spot-agent.service");
}
