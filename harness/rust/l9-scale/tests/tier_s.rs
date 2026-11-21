//! The `tier_s` profile (WP-TST-15.T3): the README S-tier floor, 10 simulated users on the
//! desktop package configuration under a 4 GB memory cap and 2 CPUs.
//!
//! Needs `SPOT_TEST_PG_URL`; without it the PostgreSQL test prints why and returns, never
//! faked. Scale: `L9_DURATION_SCALE` (a decimal, default `0.05`: 30 s of a 600 s steady phase).
//!
//! The cap is real or the run is not a tier S run. `L9_REQUIRE_CAP` selects what is checked
//! before any load starts:
//!
//! * unset: no cap is claimed; the run is informational (verdicts printed, nothing enforced
//!   except that no request answered 5xx).
//! * `1`: the process's cgroup (v2) must show `memory.max` of at most 4 GiB and `cpu.max` of
//!   at most 2 CPUs, on the cgroup or an ancestor. A profile that silently runs uncapped is a
//!   false pass, so a missing or larger limit fails the test before the load.
//! * `vm`: the machine itself is the cap (the Windows VM runner with 4 GB): on Linux the
//!   total memory must be at most 4 GiB plus 10 percent; elsewhere it is taken on trust and
//!   said so.
//!
//! With a cap in effect the budgets of `budgets/tier_s.toml` are enforced, and so are the
//! footprint (host plus PostgreSQL, under 1 GB at the peak) and zero swap use.
//! `L9_DISK_THROTTLED=1` marks the second, throttled-disk pass: it runs as `tier_s_hdd`,
//! whose budgets are informational (printed, never failing).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use l9_scale::budget::Budgets;
use l9_scale::launcher::{self, LaunchOptions};
use l9_scale::profile::{Op, Profile, parse_decimal};
use l9_scale::{Mode, RunOptions, execute};

const SCALE_ENV: &str = "L9_DURATION_SCALE";
const CAP_ENV: &str = "L9_REQUIRE_CAP";
const THROTTLED_ENV: &str = "L9_DISK_THROTTLED";
const DEFAULT_PERMILLE: u64 = 50;
const GIB_BYTES: u64 = 1024 * 1024 * 1024;
const MEMORY_CAP_BYTES: u64 = 4 * GIB_BYTES;
const CPU_CAP_MILLI: u64 = 2_000;
/// Host plus PostgreSQL under 1 GB (the README's "runtime plus database under about 1 GB").
const FOOTPRINT_LIMIT_KB: u64 = 1024 * 1024;
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Process names that make up the footprint: the host and the PostgreSQL server (and its
/// backends, which carry the same name).
const FOOTPRINT_COMMS: [&str; 2] = ["spot", "postgres"];

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn load_profile() -> Profile {
    Profile::load(&crate_dir().join("profiles/tier_s.toml")).unwrap()
}

fn load_budgets() -> Budgets {
    Budgets::load(&crate_dir().join("budgets/tier_s.toml")).unwrap()
}

fn scale_permille() -> u64 {
    std::env::var(SCALE_ENV)
        .ok()
        .and_then(|v| parse_decimal(&v, 3))
        .map_or(DEFAULT_PERMILLE, |n| n.clamp(1, 1000))
}

fn scale_durations(profile: &mut Profile, permille: u64) {
    let scale = |seconds: u64| {
        if seconds == 0 {
            0
        } else {
            (seconds.saturating_mul(permille) / 1000).max(1)
        }
    };
    profile.ramp_seconds = scale(profile.ramp_seconds);
    profile.steady_seconds = scale(profile.steady_seconds);
    profile.sample_every_seconds = profile
        .sample_every_seconds
        .min(profile.steady_seconds.div_ceil(2).max(1));
}

// --- cgroup (v2) limits -----------------------------------------------------------------------

/// The cgroup v2 path in the text of `/proc/<pid>/cgroup` (`0::/user.slice/x.scope`).
fn cgroup_path(proc_cgroup: &str) -> Option<String> {
    proc_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_owned())
}

/// `memory.max`: `Some(bytes)`, or `None` for `max` (no limit) or text that is not a number.
fn parse_memory_max(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// `cpu.max` (`<quota> <period>` or `max <period>`) as thousandths of a CPU: `200000 100000`
/// is 2000; `None` for `max` (no limit) or malformed text.
fn parse_cpu_max(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let quota: u64 = parts.next()?.parse().ok()?;
    let period: u64 = parts.next()?.parse().ok()?;
    if period == 0 {
        return None;
    }
    u64::try_from(u128::from(quota) * 1000 / u128::from(period)).ok()
}

/// The tightest limits on `path` and its ancestors.
#[derive(Debug, PartialEq, Eq)]
struct Cap {
    memory_bytes: Option<u64>,
    cpu_milli: Option<u64>,
}

fn tighter(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// Reads the limits of the cgroup `path` and every ancestor below the cgroup root.
fn read_cap(path: &str) -> Cap {
    let mut cap = Cap {
        memory_bytes: None,
        cpu_milli: None,
    };
    let mut current = path.trim_end_matches('/').to_owned();
    loop {
        let dir = format!("{CGROUP_ROOT}{current}");
        let memory = std::fs::read_to_string(format!("{dir}/memory.max"))
            .ok()
            .and_then(|t| parse_memory_max(&t));
        let cpu = std::fs::read_to_string(format!("{dir}/cpu.max"))
            .ok()
            .and_then(|t| parse_cpu_max(&t));
        cap.memory_bytes = tighter(cap.memory_bytes, memory);
        cap.cpu_milli = tighter(cap.cpu_milli, cpu);
        match current.rfind('/') {
            Some(0) | None => return cap,
            Some(at) => current.truncate(at),
        }
    }
}

/// Why `cap` does not meet the tier S floor, or `None` when it does.
fn cap_shortfall(cap: &Cap) -> Option<String> {
    match cap.memory_bytes {
        None => return Some("memory.max is not set (no memory cap)".to_owned()),
        Some(bytes) if bytes > MEMORY_CAP_BYTES => {
            return Some(format!("memory.max is {bytes} bytes, above 4 GiB"));
        }
        Some(_) => {}
    }
    match cap.cpu_milli {
        None => Some("cpu.max is not set (no CPU limit)".to_owned()),
        Some(milli) if milli > CPU_CAP_MILLI => {
            Some(format!("cpu.max allows {milli} milli-CPUs, above 2000"))
        }
        Some(_) => None,
    }
}

/// How the run's cap is established.
#[derive(Debug, PartialEq, Eq)]
enum CapMode {
    /// No cap is claimed.
    None,
    /// A cgroup limit must be in effect.
    Cgroup,
    /// The machine is the cap.
    Vm,
}

fn cap_mode() -> CapMode {
    match std::env::var(CAP_ENV).ok().as_deref().map(str::trim) {
        Some("1") => CapMode::Cgroup,
        Some("vm") => CapMode::Vm,
        _ => CapMode::None,
    }
}

/// Checks the cap before any load starts. Panics (fails the test) when a claimed cap is not
/// observable: that run would be a false pass.
fn verify_cap(mode: &CapMode) {
    match mode {
        CapMode::None => println!(
            "l9 tier_s: no cap claimed ({CAP_ENV} unset): informational run, budgets not enforced"
        ),
        CapMode::Cgroup => {
            let own = std::fs::read_to_string("/proc/self/cgroup")
                .unwrap_or_else(|e| panic!("{CAP_ENV}=1 but /proc/self/cgroup is unreadable: {e}"));
            let path = cgroup_path(&own)
                .unwrap_or_else(|| panic!("{CAP_ENV}=1 but this is not a cgroup v2 host"));
            let cap = read_cap(&path);
            if let Some(why) = cap_shortfall(&cap) {
                panic!(
                    "{CAP_ENV}=1 but the cap is not in effect ({why}) for cgroup {path}: \
                     refusing to run a tier S profile uncapped"
                );
            }
            println!("l9 tier_s: cap verified for cgroup {path}: {cap:?}");
        }
        CapMode::Vm => match std::fs::read_to_string("/proc/meminfo") {
            Ok(meminfo) => {
                let total_kb = meminfo
                    .lines()
                    .find_map(|l| kb_field(l, "MemTotal:"))
                    .unwrap_or(u64::MAX);
                assert!(
                    total_kb <= MEMORY_CAP_BYTES / 1024 * 11 / 10,
                    "{CAP_ENV}=vm but this machine has {total_kb} kB of RAM, more than 4 GiB"
                );
                println!("l9 tier_s: VM cap verified: MemTotal {total_kb} kB");
            }
            Err(_) => println!(
                "l9 tier_s: {CAP_ENV}=vm on a host without /proc: the VM size (4 GB) is taken on \
                 trust and recorded in the baseline file"
            ),
        },
    }
}

// --- footprint ------------------------------------------------------------------------------------

/// The value of a `Name:   123 kB` line, in kB.
fn kb_field(line: &str, name: &str) -> Option<u64> {
    line.strip_prefix(name)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Footprint {
    pss_kb: u64,
    swap_kb: u64,
}

/// The proportional set size and swap of the host and PostgreSQL processes in this process's
/// cgroup (on a capped run that is the scope holding both). `None` where `/proc` is missing.
fn footprint() -> Option<Footprint> {
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let mut total = Footprint::default();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let base = format!("/proc/{pid}");
        let Ok(comm) = std::fs::read_to_string(format!("{base}/comm")) else {
            continue;
        };
        if !FOOTPRINT_COMMS.contains(&comm.trim()) {
            continue;
        }
        if std::fs::read_to_string(format!("{base}/cgroup"))
            .ok()
            .as_deref()
            != Some(own.as_str())
        {
            continue;
        }
        let rollup = std::fs::read_to_string(format!("{base}/smaps_rollup")).unwrap_or_default();
        let status = std::fs::read_to_string(format!("{base}/status")).unwrap_or_default();
        let pss = rollup
            .lines()
            .find_map(|l| kb_field(l, "Pss:"))
            .or_else(|| status.lines().find_map(|l| kb_field(l, "VmRSS:")))
            .unwrap_or(0);
        let swap = status
            .lines()
            .find_map(|l| kb_field(l, "VmSwap:"))
            .unwrap_or(0);
        total.pss_kb = total.pss_kb.saturating_add(pss);
        total.swap_kb = total.swap_kb.saturating_add(swap);
    }
    Some(total)
}

/// Samples the footprint every second until `stop` is set; the peaks seen and the number of
/// samples.
async fn watch_footprint(stop: Arc<AtomicBool>) -> (Footprint, u64) {
    let mut peak = Footprint::default();
    let mut samples = 0;
    loop {
        let last = stop.load(Ordering::SeqCst);
        if let Some(now) = footprint() {
            peak.pss_kb = peak.pss_kb.max(now.pss_kb);
            peak.swap_kb = peak.swap_kb.max(now.swap_kb);
            samples += 1;
        }
        if last {
            return (peak, samples);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// --- the run ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tier_s_on_the_desktop_configuration_under_the_cap() {
    let mode = cap_mode();
    let throttled = std::env::var(THROTTLED_ENV).is_ok_and(|v| v.trim() == "1");
    let mut profile = load_profile();
    scale_durations(&mut profile, scale_permille());
    if throttled {
        profile.name = "tier_s_hdd".to_owned();
    }
    // Real or not at all: a claimed cap that is not in effect fails here, before any load.
    verify_cap(&mode);

    let run_id = launcher::random_hex(12);
    let out = std::env::temp_dir().join(format!("l9_tier_s_report_{run_id}"));
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = tokio::spawn(watch_footprint(stop.clone()));
    let options = RunOptions {
        profile: profile.clone(),
        budgets: load_budgets(),
        mode: Mode::Launch(LaunchOptions {
            repo: launcher::default_repo(),
            spot_bin: None,
            users: profile.sessions,
            run_id: run_id.clone(),
            keep: false,
        }),
        run_id,
        out_dir: Some(out.clone()),
    };
    let result = execute(options).await;
    stop.store(true, Ordering::SeqCst);
    let (peak, samples) = watcher.await.expect("the footprint watcher panicked");
    let Some(finished) = result.unwrap_or_else(|e| panic!("the tier_s run failed to start: {e}"))
    else {
        println!("SKIPPED: the l9-scale tier_s run needs SPOT_TEST_PG_URL");
        return;
    };
    let report = finished.report;
    println!("{}", report.to_markdown());
    println!(
        "l9 tier_s: footprint peak {} kB (limit {FOOTPRINT_LIMIT_KB} kB), swap peak {} kB, \
         {samples} samples",
        peak.pss_kb, peak.swap_kb
    );

    // What holds on every machine.
    for operation in &report.operations {
        assert_eq!(
            operation.server_errors,
            0,
            "{} answered a 5xx:\n{}",
            operation.name,
            report.to_markdown()
        );
    }
    assert!(
        report
            .operations
            .iter()
            .any(|o| o.name == Op::CreateSubmit.name() && o.count > 0),
        "no posting ran"
    );
    assert!(!report.budgets.is_empty(), "no budget was evaluated");

    // What holds only with a cap in effect, and not on the informational throttled pass.
    let enforce = mode != CapMode::None && !throttled;
    if enforce {
        assert!(report.passed, "budgets breached:\n{}", report.to_markdown());
        assert_eq!(
            report.shed_arrivals, 0,
            "ten sessions could not take the arrival rate"
        );
        if samples == 0 {
            println!("l9 tier_s: no /proc on this host: footprint and swap not measured");
        } else {
            assert!(
                peak.pss_kb > 0,
                "the footprint sampler found no host or PostgreSQL process in this cgroup"
            );
            assert!(
                peak.pss_kb < FOOTPRINT_LIMIT_KB,
                "host plus PostgreSQL peaked at {} kB, above the 1 GB budget",
                peak.pss_kb
            );
            assert_eq!(peak.swap_kb, 0, "the host or PostgreSQL used swap");
        }
    } else {
        for verdict in report.budgets.iter().filter(|v| !v.passed) {
            println!(
                "l9 tier_s (informational): {} {} limit {} measured {}",
                verdict.operation, verdict.metric, verdict.limit, verdict.measured
            );
        }
    }
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn the_tier_s_profile_is_ten_sessions_and_about_one_posting_per_second() {
    let profile = load_profile();
    assert_eq!(profile.name, "tier_s");
    assert_eq!(profile.tier, "S");
    assert_eq!(profile.sessions, 10);
    let posting_milli = profile.target_milli_tps() * u64::from(profile.mix.create_submit)
        / profile.mix.total().max(1);
    assert_eq!(posting_milli, 1_000, "light posting is 1 tx/s");
    assert_eq!(profile.mix.update, 0);
}

#[test]
fn the_tier_s_budgets_are_the_readme_numbers_and_the_hdd_pass_has_its_own_lines() {
    let budgets = load_budgets();
    let line = |profile: &str, op: &str| {
        budgets
            .for_profile(profile)
            .into_iter()
            .find(|b| b.operation == op)
            .unwrap_or_else(|| panic!("no {profile} budget for {op}"))
            .clone()
    };
    for profile in ["tier_s", "tier_s_hdd"] {
        assert_eq!(line(profile, "create_submit").p95_ms, Some(500));
        assert_eq!(line(profile, "open_form").p95_ms, Some(300));
        assert_eq!(line(profile, "all").max_error_ppm, Some(0));
    }
}

#[test]
fn cgroup_text_is_parsed_and_a_missing_or_loose_cap_is_a_shortfall() {
    assert_eq!(
        cgroup_path("0::/user.slice/user-1000.slice/run-x.scope\n").as_deref(),
        Some("/user.slice/user-1000.slice/run-x.scope")
    );
    assert_eq!(cgroup_path("12:cpu:/x\n"), None);
    assert_eq!(parse_memory_max("4294967296\n"), Some(4 * GIB_BYTES));
    assert_eq!(parse_memory_max("max\n"), None);
    assert_eq!(parse_cpu_max("200000 100000\n"), Some(2_000));
    assert_eq!(parse_cpu_max("150000 100000"), Some(1_500));
    assert_eq!(parse_cpu_max("max 100000"), None);
    assert_eq!(parse_cpu_max("100 0"), None);
    assert_eq!(tighter(Some(5), Some(3)), Some(3));
    assert_eq!(tighter(None, Some(3)), Some(3));
    assert_eq!(tighter(Some(5), None), Some(5));

    let good = Cap {
        memory_bytes: Some(4 * GIB_BYTES),
        cpu_milli: Some(2_000),
    };
    assert_eq!(cap_shortfall(&good), None);
    let no_memory = Cap {
        memory_bytes: None,
        cpu_milli: Some(2_000),
    };
    assert!(cap_shortfall(&no_memory).unwrap().contains("memory"));
    let loose_memory = Cap {
        memory_bytes: Some(8 * GIB_BYTES),
        cpu_milli: Some(2_000),
    };
    assert!(cap_shortfall(&loose_memory).unwrap().contains("4 GiB"));
    let loose_cpu = Cap {
        memory_bytes: Some(GIB_BYTES),
        cpu_milli: Some(4_000),
    };
    assert!(cap_shortfall(&loose_cpu).unwrap().contains("2000"));
    let no_cpu = Cap {
        memory_bytes: Some(GIB_BYTES),
        cpu_milli: None,
    };
    assert!(cap_shortfall(&no_cpu).unwrap().contains("cpu"));
}

#[test]
fn proc_lines_and_duration_scaling_are_integer_arithmetic() {
    assert_eq!(kb_field("Pss:             1234 kB", "Pss:"), Some(1234));
    assert_eq!(kb_field("VmSwap:\t       0 kB", "VmSwap:"), Some(0));
    assert_eq!(kb_field("Rss: 5 kB", "Pss:"), None);
    let mut profile = load_profile();
    scale_durations(&mut profile, 50);
    assert_eq!(profile.steady_seconds, 30);
    assert_eq!(profile.ramp_seconds, 1);
    assert!(profile.sample_every_seconds >= 1);
}
