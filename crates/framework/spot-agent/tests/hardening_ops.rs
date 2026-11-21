//! WP-ADM-10.T2 tests: findings and drift from fixture hosts (a mounted directory), the
//! plan, the lock-out guard and the catalogue shape. Apply against the distro matrix is
//! not run here (needs the harness).

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_agent::handlers::HandlerCtx;
use spot_agent::handlers::hardening_ops::{
    HardeningHandler, HostFacts, Refused, catalogue_specs, drift, drift_series, findings,
};
use spot_agent_linux::{FileSystem, LinuxContext, LinuxError};
use spot_hardening::{Baseline, evaluate};
use spot_mgmt_ops::{ParamValue, Risk};

/// A fixture host: files under a directory, read as if it were `/`.
struct DirFs(PathBuf);

impl DirFs {
    fn path(&self, p: &str) -> PathBuf {
        self.0.join(p.trim_start_matches('/'))
    }
}

impl FileSystem for DirFs {
    fn read_to_string(&self, path: &str) -> Result<String, LinuxError> {
        std::fs::read_to_string(self.path(path)).map_err(|e| LinuxError::Io {
            path: path.to_owned(),
            detail: e.to_string(),
        })
    }
    fn exists(&self, path: &str) -> Result<bool, LinuxError> {
        Ok(self.path(path).exists())
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn weak_host(root: &Path) {
    write(
        root,
        "etc/ssh/sshd_config",
        "# comment\nPermitRootLogin yes\nPasswordAuthentication yes\n",
    );
    write(
        root,
        "etc/systemd/system/multi-user.target.wants/sshd.service",
        "",
    );
    write(root, "sys/fs/selinux/enforce", "0\n");
}

fn strong_host(root: &Path) {
    write(
        root,
        "etc/ssh/sshd_config",
        "PermitRootLogin no\nPasswordAuthentication no\n",
    );
    write(
        root,
        "etc/systemd/system/multi-user.target.wants/sshd.service",
        "",
    );
    write(root, "sys/fs/selinux/enforce", "1\n");
    write(root, "proc/sys/net/ipv4/conf/all/rp_filter", "1\n");
    write(root, "proc/sys/kernel/kptr_restrict", "1\n");
    write(root, "proc/sys/net/ipv4/tcp_syncookies", "1\n");
}

fn handler(
    root: &Path,
    state: &Path,
) -> (
    HardeningHandler,
    Arc<spot_agent_linux_sys::testing::FakeRunner>,
) {
    let system = spot_agent_linux::testing::FakeSystem::fedora();
    let sys_runner = Arc::new(spot_agent_linux_sys::testing::FakeRunner::new());
    let ctx = HandlerCtx {
        linux_runner: system.runner(),
        linux_fs: system.filesystem(),
        sys_runner: sys_runner.clone(),
        sys_fs: Arc::new(spot_agent_linux_sys::testing::FakeFs::new()),
        clock: Arc::new(spot_core::SystemClock::new()),
        state_dir: state.to_path_buf(),
    };
    let context = Arc::new(LinuxContext::new(
        ctx.linux_runner.clone(),
        ctx.linux_fs.clone(),
    ));
    (
        HardeningHandler::new(&ctx, context, Arc::new(DirFs(root.to_path_buf()))),
        sys_runner,
    )
}

#[test]
fn findings_carry_rule_severity_and_an_evidence_digest_never_contents() {
    let host = tempfile::tempdir().unwrap();
    weak_host(host.path());
    let facts = HostFacts::new(Arc::new(DirFs(host.path().to_path_buf())));
    let report = evaluate(&Baseline::linux().unwrap(), &facts);
    let found = findings(&report);
    let root_login = found
        .iter()
        .find(|f| f.rule == "ssh.root_login")
        .expect("a weak host fails ssh.root_login");
    assert_eq!(root_login.severity, "high");
    assert_eq!(root_login.status, "fail");
    assert_eq!(root_login.evidence_sha256.len(), 16);
    let text = serde_json::to_string(&found).unwrap();
    assert!(
        !text.contains("PermitRootLogin") && !text.contains("yes"),
        "digests, not contents"
    );
    assert!(found.iter().any(|f| f.rule == "mac.enforcing"));
    let d = drift(&report);
    assert!(d.high >= 1);
}

#[test]
fn a_hardened_host_has_no_ssh_or_mac_findings() {
    let host = tempfile::tempdir().unwrap();
    strong_host(host.path());
    let facts = HostFacts::new(Arc::new(DirFs(host.path().to_path_buf())));
    let report = evaluate(&Baseline::linux().unwrap(), &facts);
    let rules: Vec<String> = findings(&report).into_iter().map(|f| f.rule).collect();
    for rule in [
        "ssh.root_login",
        "ssh.password_auth",
        "mac.enforcing",
        "kernel.sysctls",
    ] {
        assert!(!rules.contains(&rule.to_owned()), "{rule}");
    }
}

#[test]
fn check_writes_the_report_and_the_drift_summary_for_the_heartbeat() {
    let host = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    weak_host(host.path());
    let (h, _) = handler(host.path(), state.path());
    let found = h.check().unwrap();
    assert!(!found.is_empty());
    assert!(state.path().join("hardening-report.json").exists());
    let series = drift_series(state.path());
    assert_eq!(series.len(), 4);
    let high: usize = series
        .iter()
        .find(|(n, _)| n == "hardening_findings_high")
        .unwrap()
        .1
        .parse()
        .unwrap();
    assert!(high >= 1);
    assert!(drift_series(Path::new("/nonexistent-dir-for-test")).is_empty());
}

#[test]
fn plan_shows_the_exact_operations_and_changes_nothing() {
    let host = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    weak_host(host.path());
    let (h, runner) = handler(host.path(), state.path());
    let lines = h.plan(Some("ssh.root_login")).unwrap();
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("ssh.root_login: os.service.disable"),
        "{lines:?}"
    );
    assert!(runner.calls().is_empty());
    // An unknown rule and a rule that already passes are refused.
    assert_eq!(
        h.plan(Some("no.such_rule")).unwrap_err(),
        Refused::UnknownRule
    );
    let strong = tempfile::tempdir().unwrap();
    strong_host(strong.path());
    let (h2, _) = handler(strong.path(), state.path());
    assert_eq!(
        h2.plan(Some("ssh.root_login")).unwrap_err(),
        Refused::NotRemediable
    );
}

#[test]
fn applying_a_lockout_rule_without_the_guard_is_refused_and_changes_nothing() {
    let host = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    weak_host(host.path());
    let (h, runner) = handler(host.path(), state.path());
    // No key session, no revert ticket.
    assert_eq!(
        h.apply("ssh.root_login", false, false, None).unwrap_err(),
        Refused::LockoutGuard
    );
    // A key session alone is not enough: the revert ticket must exist and be named.
    assert_eq!(
        h.apply("ssh.root_login", false, true, None).unwrap_err(),
        Refused::LockoutGuard
    );
    assert_eq!(
        h.apply("ssh.root_login", false, true, Some(7)).unwrap_err(),
        Refused::LockoutGuard
    );
    assert!(runner.calls().is_empty());
    // A dry run (the default) never trips the guard and never applies.
    let dry = h.apply("ssh.root_login", true, false, None);
    assert!(!matches!(dry, Err(Refused::LockoutGuard)));
}

#[test]
fn the_catalogue_declares_three_operations_and_apply_is_a_dry_run_by_default() {
    let specs = catalogue_specs();
    let ids: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        ["hardening.check", "hardening.plan", "hardening.apply"]
    );
    assert_eq!(specs[0].risk, Risk::Read);
    assert_eq!(specs[2].risk, Risk::High);
    let dry = specs[2]
        .params
        .iter()
        .find(|p| p.name == "dry_run")
        .unwrap();
    assert_eq!(dry.default, Some(ParamValue::Bool(true)));
    let rules = specs[2].params.iter().find(|p| p.name == "rules").unwrap();
    assert!(rules.required);
    let mut builder = spot_mgmt_ops::CatalogueBuilder::new();
    spot_agent::handlers::hardening_ops::register_catalogue(&mut builder);
    assert_eq!(builder.build().unwrap().len(), 3);
}
