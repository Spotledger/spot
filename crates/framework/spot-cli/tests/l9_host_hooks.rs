//! The host hooks the L9 fault and tier profiles drive (WP-TST-15.T9): `--clock-offset` behind
//! `--test-hooks`, and the `postgres.pool_max_size` setting with its range.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::time::Duration;

use spot_cli::config::{
    MAX_CLOCK_OFFSET_SECONDS, MAX_POOL_MAX_SIZE, TEST_HOOK_REFUSED, clock_from_test_hooks, load,
};
use spot_core::Clock;

// --- the clock offset --------------------------------------------------------------------------

#[test]
fn no_offset_keeps_the_system_clock_whatever_the_flags() {
    for (hooks, env) in [(false, false), (true, false), (true, true), (false, true)] {
        assert!(clock_from_test_hooks(hooks, env, None).unwrap().is_none());
    }
}

#[test]
fn an_offset_without_the_hook_flag_is_refused_with_the_catalogued_code() {
    let error = clock_from_test_hooks(false, true, Some(30))
        .err()
        .unwrap()
        .to_string();
    assert!(error.starts_with(TEST_HOOK_REFUSED), "{error}");
    assert!(error.contains("--test-hooks"), "{error}");
}

#[test]
fn the_hook_flag_without_the_environment_opt_in_is_refused() {
    let error = clock_from_test_hooks(true, false, Some(30))
        .err()
        .unwrap()
        .to_string();
    assert!(error.starts_with(TEST_HOOK_REFUSED), "{error}");
    assert!(error.contains("SPOT_TEST_HOOKS"), "{error}");
}

#[test]
fn an_offset_beyond_a_year_is_refused_and_the_error_never_prints_the_value() {
    let too_far = MAX_CLOCK_OFFSET_SECONDS + 1;
    for seconds in [too_far, -too_far, i64::MAX, i64::MIN] {
        let error = clock_from_test_hooks(true, true, Some(seconds))
            .err()
            .unwrap()
            .to_string();
        assert!(error.starts_with(TEST_HOOK_REFUSED), "{error}");
        assert!(!error.contains(&seconds.to_string()), "{error}");
    }
    assert!(
        clock_from_test_hooks(true, true, Some(MAX_CLOCK_OFFSET_SECONDS))
            .unwrap()
            .is_some()
    );
    assert!(
        clock_from_test_hooks(true, true, Some(-MAX_CLOCK_OFFSET_SECONDS))
            .unwrap()
            .is_some()
    );
}

#[test]
fn the_offset_shifts_timestamps_and_not_the_monotonic_reading() {
    let system = spot_core::SystemClock::new();
    for seconds in [1_800_i64, -1_800] {
        let skewed = clock_from_test_hooks(true, true, Some(seconds))
            .unwrap()
            .unwrap();
        let before = system.now_utc();
        let shifted = skewed.now_utc();
        let after = system.now_utc();
        let low = before + time::Duration::seconds(seconds);
        let high = after + time::Duration::seconds(seconds);
        assert!(
            shifted >= low && shifted <= high,
            "{shifted} is not within [{low}, {high}] for an offset of {seconds} s"
        );
        // Intervals are measured by the system's own monotonic clock: it moves forward by real
        // time, whatever the offset.
        let first = skewed.monotonic();
        std::thread::sleep(Duration::from_millis(20));
        let second = skewed.monotonic();
        assert!(
            second >= first + Duration::from_millis(15),
            "{first:?} -> {second:?}"
        );
    }
}

// --- the binary --------------------------------------------------------------------------------

fn spot() -> Command {
    Command::new(env!("CARGO_BIN_EXE_spot"))
}

#[test]
fn the_binary_refuses_a_clock_offset_without_the_hook_flag_before_reading_anything() {
    // The root does not exist: a refusal that came from reading it would name the file.
    let output = spot()
        .env_remove("SPOT_TEST_HOOKS")
        .args([
            "--root",
            "/nonexistent-l9-hooks",
            "serve",
            "--clock-offset",
            "-30",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(TEST_HOOK_REFUSED), "{stderr}");
    assert!(!stderr.contains("nonexistent"), "{stderr}");
}

#[test]
fn the_binary_refuses_the_hook_flag_without_the_environment_opt_in() {
    let output = spot()
        .env_remove("SPOT_TEST_HOOKS")
        .args([
            "--root",
            "/nonexistent-l9-hooks",
            "serve",
            "--test-hooks",
            "--clock-offset",
            "30",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(TEST_HOOK_REFUSED), "{stderr}");
}

// --- the pool size -----------------------------------------------------------------------------

fn root_with(postgres: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "spot_cli_l9_hooks_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("apps/books/doctypes")).unwrap();
    std::fs::write(
        root.join("common.toml"),
        format!(
            "apps = [{:?}]\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://u:p@127.0.0.1:15432/postgres'\n{postgres}\n",
            root.join("apps/books").display().to_string()
        ),
    )
    .unwrap();
    root
}

#[test]
fn the_default_pool_size_is_unchanged() {
    let root = root_with("");
    assert_eq!(
        load(&root, false).unwrap().common.postgres.pool_max_size,
        16
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_pool_size_in_range_is_applied() {
    for size in [1, 20, 48, MAX_POOL_MAX_SIZE] {
        let root = root_with(&format!("pool_max_size = {size}"));
        assert_eq!(
            load(&root, false).unwrap().common.postgres.pool_max_size,
            size
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn a_pool_size_out_of_range_is_refused_naming_the_setting() {
    for size in [0, MAX_POOL_MAX_SIZE + 1] {
        let root = root_with(&format!("pool_max_size = {size}"));
        let error = load(&root, false).err().unwrap().to_string();
        assert!(error.contains("postgres.pool_max_size"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }
}
