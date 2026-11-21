//! WP-ADM-03.T2 tests: handler registry gating, the privilege templates (hostile input
//! table), the helper (unknown operation, redaction), the runner's routing, timeouts that
//! kill the process group, output caps, the sudoers generator, path confinement and the
//! Windows pipe protocol. A fake child runner stands in for the privileged executor.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use spot_agent::config::AgentConfig;
#[cfg(target_os = "linux")]
use spot_agent::handlers::{HandlerCtx, NAMESPACES, build_registry_with};
use spot_agent::privilege::HELPER_PATH;
use spot_agent::privilege::exec::{
    ChildResult, ChildRunner, ChildSpec, SpawnFailed, StdChildRunner,
};
use spot_agent::privilege::helper::{HelperReply, run_helper};
use spot_agent::privilege::protocol::{decode_request, pipe_sddl};
use spot_agent::privilege::runner::{Denied, PrivilegedRunner};
use spot_agent::privilege::sudoers::sudoers_dropin;
#[cfg(unix)]
use spot_agent::privilege::templates::{CERT_DEST_ROOT, confine};
use spot_agent::privilege::templates::{Kind, Refused, TEMPLATES, find, route, validate};
#[cfg(target_os = "linux")]
use spot_mgmt_ops::OpId;

/// A fake privileged executor: records specs, answers with a canned result.
struct FakeChild {
    calls: Mutex<Vec<ChildSpec>>,
    reply: Mutex<Option<ChildResult>>,
}

impl FakeChild {
    fn new(reply: ChildResult) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            reply: Mutex::new(Some(reply)),
        })
    }
    fn calls(&self) -> Vec<ChildSpec> {
        self.calls.lock().unwrap().clone()
    }
}

impl ChildRunner for FakeChild {
    fn run(&self, spec: &ChildSpec) -> Result<ChildResult, SpawnFailed> {
        self.calls.lock().unwrap().push(spec.clone());
        Ok(self.reply.lock().unwrap().clone().unwrap())
    }
}

fn ok_result(stdout: &str) -> ChildResult {
    ChildResult {
        status: 0,
        stdout: stdout.to_owned(),
        stderr: String::new(),
        timed_out: false,
        truncated: false,
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_owned()).collect()
}

#[cfg(target_os = "linux")]
fn ctx() -> HandlerCtx {
    let system = spot_agent_linux::testing::FakeSystem::fedora();
    HandlerCtx {
        linux_runner: system.runner(),
        linux_fs: system.filesystem(),
        sys_runner: Arc::new(spot_agent_linux_sys::testing::FakeRunner::new()),
        sys_fs: Arc::new(spot_agent_linux_sys::testing::FakeFs::new()),
        clock: Arc::new(spot_core::SystemClock::new()),
        state_dir: std::env::temp_dir().join("spot-agent-test-handlers"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_namespace_that_is_not_enabled_is_not_registered_at_all() {
    assert!(NAMESPACES.contains(&"os"));
    let none = AgentConfig::parse("gateway = \"https://g.example.test\"\n").unwrap();
    assert!(build_registry_with(&none, &ctx()).is_empty());
    let other = AgentConfig::parse(
        "gateway = \"https://g.example.test\"\nallowed_modules = [\"hardening\"]\n",
    )
    .unwrap();
    // The hardening namespace has handlers of its own now (WP-ADM-10.T2); enabling it adds none
    // of the `os` ones.
    let hardening = build_registry_with(&other, &ctx());
    assert!(!hardening.contains(&OpId::new("os.service.status").unwrap()));
    assert!(!hardening.contains(&OpId::new("os.sysctl.set").unwrap()));
    let os =
        AgentConfig::parse("gateway = \"https://g.example.test\"\nallowed_modules = [\"os\"]\n")
            .unwrap();
    let registry = build_registry_with(&os, &ctx());
    assert!(!registry.is_empty());
    assert!(registry.contains(&OpId::new("os.service.status").unwrap()));
    assert!(registry.contains(&OpId::new("os.sysctl.set").unwrap()));
}

#[cfg(unix)]
const HOSTILE: &[&str] = &[
    ";rm -rf /",
    "a;b",
    "a b",
    "a\nb",
    "a\rb",
    "$(id)",
    "`id`",
    "${IFS}",
    "-rf",
    "--option=x",
    "-",
    "a|b",
    "a&&b",
    "a||b",
    "a>b",
    "a<b",
    "a'b",
    "a\"b",
    "a\\b",
    "../../etc/passwd",
    "..",
    "",
    "x\0y",
    "a*b",
    "a?b",
    "~root",
    "a\tb",
    "nginx.service\n--now",
];

// The path templates name POSIX locations (CERT_DEST_ROOT), which Windows cannot canonicalise.
#[cfg(unix)]
#[test]
fn every_template_rejects_shell_metacharacters_newlines_and_option_like_values() {
    for template in TEMPLATES {
        let slots = template.slots();
        for (slot_index, _) in slots.iter().enumerate() {
            for hostile in HOSTILE {
                // A valid value in every other slot.
                let params: Vec<String> = slots
                    .iter()
                    .enumerate()
                    .map(|(i, kind)| {
                        if i == slot_index {
                            (*hostile).to_owned()
                        } else {
                            valid_value(*kind)
                        }
                    })
                    .collect();
                assert_eq!(
                    template.build(&params),
                    Err(Refused::BadParam(slot_index)),
                    "{} slot {slot_index} must reject {hostile:?}",
                    template.name
                );
            }
        }
    }
}

fn valid_value(kind: Kind) -> String {
    match kind {
        Kind::Unit => "nginx.service".to_owned(),
        Kind::Account => "alice".to_owned(),
        Kind::Int { min, .. } => min.to_string(),
        Kind::Enum(set) => set[0].to_owned(),
        Kind::ServiceName => "Spooler".to_owned(),
        Kind::Path { root } => format!("{root}/x.crt"),
    }
}

#[test]
fn valid_values_build_argv_from_the_template_only() {
    let t = find("service.restart").unwrap();
    assert_eq!(
        t.build(&s(&["nginx.service"])).unwrap(),
        s(&["--no-pager", "restart", "nginx.service"])
    );
    assert_eq!(t.build(&s(&[])), Err(Refused::ParamCount));
    assert_eq!(
        t.build(&s(&["a.service", "b.service"])),
        Err(Refused::ParamCount)
    );
    assert_eq!(validate(Kind::Account, "alice-1").unwrap(), "alice-1");
    assert!(validate(Kind::Account, "Alice").is_err());
    assert!(validate(Kind::Int { min: 1, max: 10 }, "11").is_err());
    assert!(validate(Kind::Int { min: 1, max: 10 }, "+5").is_err());
    assert!(
        validate(Kind::Unit, "nginx").is_err(),
        "a unit needs an allowed suffix"
    );
}

#[test]
fn an_unknown_operation_is_refused_by_the_helper_even_if_the_agent_asks() {
    let child = FakeChild::new(ok_result(""));
    let (code, out) = run_helper(&s(&["run", "shell.exec"]), "{\"params\":[\"id\"]}", &*child);
    assert_eq!(code, 3);
    let reply: HelperReply = serde_json::from_str(out.trim()).unwrap();
    assert!(!reply.ok);
    assert_eq!(
        reply.code.as_deref(),
        Some("agent.privilege_unknown_operation")
    );
    // Extra stdin fields, wrong shapes and wrong counts are refused too; nothing ran.
    for bad in [
        "{\"params\":[],\"argv\":[\"x\"]}",
        "not json",
        "{\"params\":[\"a.service\",\"b\"]}",
        "{\"params\":[\"-x.service\"]}",
    ] {
        let (code, _) = run_helper(&s(&["run", "service.restart"]), bad, &*child);
        assert_eq!(code, 3, "{bad}");
    }
    assert!(child.calls().is_empty());
    // Unknown subcommands and flags exit 2.
    assert_eq!(run_helper(&s(&["--exec", "id"]), "", &*child).0, 2);
    assert_eq!(run_helper(&s(&[]), "", &*child).0, 2);
}

#[test]
fn the_helper_runs_the_template_argv_and_redacts_output() {
    let child = FakeChild::new(ok_result("unit failed, password=hunter2hunter2 was used\n"));
    let (code, out) = run_helper(
        &s(&["run", "service.restart"]),
        "{\"params\":[\"nginx.service\"]}",
        &*child,
    );
    assert_eq!(code, 0);
    let reply: HelperReply = serde_json::from_str(out.trim()).unwrap();
    assert!(reply.ok);
    assert!(!reply.stdout.contains("hunter2hunter2"));
    let call = &child.calls()[0];
    assert_eq!(call.program, "/usr/bin/systemctl");
    assert_eq!(call.args, s(&["--no-pager", "restart", "nginx.service"]));
    assert_eq!(call.timeout_secs, 60);
}

#[test]
fn the_runner_routes_templates_to_the_helper_reads_directly_and_refuses_the_rest() {
    let helper_reply = serde_json::to_string(&HelperReply {
        ok: true,
        code: None,
        status: 0,
        timed_out: false,
        truncated: false,
        stdout: "ok".to_owned(),
        stderr: String::new(),
    })
    .unwrap();
    let child = FakeChild::new(ok_result(&helper_reply));
    let runner = PrivilegedRunner::new(child.clone());
    let out = runner
        .execute(
            "systemctl",
            &s(&["--no-pager", "restart", "nginx.service"]),
            30,
            1000,
        )
        .unwrap();
    assert_eq!(out.stdout, "ok");
    let call = &child.calls()[0];
    assert_eq!(call.program, "/usr/bin/sudo");
    assert_eq!(call.args, s(&["-n", HELPER_PATH, "run", "service.restart"]));
    assert_eq!(
        String::from_utf8(call.stdin.clone().unwrap())
            .unwrap()
            .trim(),
        "{\"params\":[\"nginx.service\"]}"
    );
    // A read-only command runs directly as the agent user.
    runner
        .execute(
            "systemctl",
            &s(&["--no-pager", "show", "nginx.service"]),
            30,
            1000,
        )
        .unwrap();
    assert_eq!(child.calls()[1].program, "/usr/bin/systemctl");
    // Arbitrary commands, hostile parameters and shells are refused; nothing more ran.
    for (program, args) in [
        ("rm", s(&["-rf", "/"])),
        ("sh", s(&["-c", "id"])),
        ("/bin/sh", s(&[])),
        (
            "systemctl",
            s(&["--no-pager", "restart", "x.service;reboot"]),
        ),
        ("systemctl", s(&["--no-pager", "edit", "x.service"])),
        ("/tmp/systemctl", s(&["--no-pager", "show", "x"])),
    ] {
        assert_eq!(
            runner.execute(program, &args, 30, 1000).unwrap_err(),
            Denied::NotAllowed,
            "{program}"
        );
    }
    assert_eq!(child.calls().len(), 2);
    assert!(route("usermod", &s(&["-L", "alice"])).is_some());
    assert!(route("usermod", &s(&["-L", "alice", "-G", "wheel"])).is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn a_timed_out_child_is_killed_with_its_process_group() {
    let started = std::time::Instant::now();
    let result = StdChildRunner
        .run(&ChildSpec {
            program: "/bin/sh".to_owned(),
            args: s(&["-c", "sleep 60 & echo $!; wait"]),
            timeout_secs: 1,
            output_cap: 4096,
            stdin: None,
        })
        .unwrap();
    assert!(result.timed_out);
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    let pid: u32 = result.stdout.trim().parse().unwrap();
    let alive = |pid: u32| {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
            && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .unwrap_or_default()
                .contains(") Z")
    };
    let mut gone = false;
    for _ in 0..50 {
        if !alive(pid) {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(gone, "the grandchild must die with the group");
}

#[cfg(target_os = "linux")]
#[test]
fn output_is_capped() {
    let result = StdChildRunner
        .run(&ChildSpec {
            program: "/bin/sh".to_owned(),
            args: s(&["-c", "head -c 100000 /dev/zero | tr '\\0' a"]),
            timeout_secs: 20,
            output_cap: 1000,
            stdin: None,
        })
        .unwrap();
    assert!(result.truncated);
    assert!(result.stdout.len() <= 1000);
    assert_eq!(result.status, 0);
}

#[test]
fn the_sudoers_output_allows_exactly_the_template_names_and_no_wildcard() {
    let text = sudoers_dropin("spot-agent", HELPER_PATH).unwrap();
    assert!(!text.contains('*'), "no wildcard");
    assert_eq!(text.matches(HELPER_PATH).count(), TEMPLATES.len());
    for t in TEMPLATES {
        assert!(
            text.contains(&format!("{HELPER_PATH} run {}", t.name)),
            "{}",
            t.name
        );
    }
    let mut paths: Vec<&str> = text
        .split_whitespace()
        .filter(|w| w.starts_with('/'))
        .collect();
    paths.sort_unstable();
    paths.dedup();
    assert_eq!(paths, vec![HELPER_PATH], "no other path is named");
    assert!(sudoers_dropin("root; ALL", HELPER_PATH).is_err());
    assert!(sudoers_dropin("spot-agent", "/tmp/evil").is_err());
    let (code, out) = run_helper(
        &s(&["--print-sudoers", "--user", "spot-agent"]),
        "",
        &StdChildRunner,
    );
    assert_eq!((code, out), (0, text));
}

#[cfg(unix)]
#[test]
fn paths_are_canonicalised_and_confined_to_the_allowed_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.crt"), "x").unwrap();
    let root_s = root.display().to_string();
    assert!(confine(&format!("{root_s}/a.crt"), &root_s).is_some());
    assert!(
        confine(&format!("{root_s}/new.crt"), &root_s).is_some(),
        "a new file under the root"
    );
    assert!(confine(&format!("{root_s}/../a.crt"), &root_s).is_none());
    assert!(confine(&format!("{root_s}/sub/../a.crt"), &root_s).is_none());
    assert!(
        confine(&root_s, &root_s).is_none(),
        "the root itself is not a file"
    );
    assert!(confine("/etc/passwd", &root_s).is_none());
    assert!(confine("relative/a.crt", &root_s).is_none());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc", root.join("escape")).unwrap();
        assert!(
            confine(&format!("{root_s}/escape/passwd"), &root_s).is_none(),
            "a symlink out of the root"
        );
    }
    assert!(
        validate(
            Kind::Path {
                root: CERT_DEST_ROOT
            },
            "/etc/shadow"
        )
        .is_err()
    );
}

#[test]
fn the_windows_pipe_protocol_is_validated_and_the_sddl_admits_only_the_agent() {
    let sddl = pipe_sddl("S-1-5-21-111-222-333-1001").unwrap();
    assert_eq!(
        sddl,
        "D:P(D;;GA;;;NU)(A;;GRGW;;;S-1-5-21-111-222-333-1001)(A;;GA;;;SY)"
    );
    assert!(!sddl.contains("WD") && !sddl.contains("BU"));
    assert!(pipe_sddl("S-1-5-21)(A;;GA;;;WD").is_err());
    assert!(pipe_sddl("").is_err());
    let (program, argv) =
        decode_request("{\"op\":\"service.start\",\"params\":[\"Spooler\"]}").unwrap();
    assert!(program.ends_with("sc.exe"));
    assert_eq!(argv, s(&["start", "Spooler"]));
    for bad in [
        "{\"op\":\"service.start\",\"params\":[\"Spooler & calc\"]}",
        "{\"op\":\"format.disk\",\"params\":[]}",
        "{\"op\":\"service.start\",\"params\":[\"a\"],\"extra\":1}",
        "garbage",
    ] {
        assert!(decode_request(bad).is_err(), "{bad}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn the_catalogue_is_the_union_of_the_compiled_handler_sets() {
    let catalogue = spot_agent::handlers::build_catalogue().unwrap();
    assert!(
        catalogue
            .get(&OpId::new("os.service.restart").unwrap())
            .is_some()
    );
    assert!(
        catalogue
            .get(&OpId::new("os.sysctl.set").unwrap())
            .is_some()
    );
}
