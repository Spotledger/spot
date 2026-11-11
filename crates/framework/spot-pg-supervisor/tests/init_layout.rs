//! Layout validation and initialisation semantics against the fake bundle.

mod common;

use common::{TestEnv, must};
use spot_pg_supervisor::Listen;
use spot_pg_supervisor::error::SupervisorError;
use spot_pg_supervisor::layout::render_spot_conf;
use spot_pg_supervisor::settings::PgSettings;
use std::time::Duration;

fn supervisor(env: &TestEnv) -> spot_pg_supervisor::Supervisor {
    spot_pg_supervisor::Supervisor::new(env.layout(), env.settings(), env.config())
}

/// Missing binaries are named by `validate`.
#[test]
fn validate_missing_binaries() {
    let env = TestEnv::new();
    std::fs::remove_file(env.bin_dir.join("initdb")).ok();
    let layout = env.layout();
    match common::block_on(layout.validate()) {
        Err(SupervisorError::MissingBinaries { dir, missing }) => {
            assert_eq!(missing, vec!["initdb".to_string()]);
            assert_eq!(dir, env.bin_dir.display().to_string());
        }
        other => panic!("expected MissingBinaries, got {other:?}"),
    }
}

/// A data directory inside the bin directory is a layout error; a sibling is fine.
#[test]
fn validate_data_dir_inside_bin_dir() {
    let env = TestEnv::new();
    let mut layout = env.layout();
    layout.data_dir = env.bin_dir.join("data");
    match common::block_on(layout.validate()) {
        Err(SupervisorError::Layout(message)) => {
            assert!(message.contains("must not be inside bin_dir"), "{message}");
        }
        other => panic!("expected Layout error, got {other:?}"),
    }
}

/// A busy port fails validation with `PortBusy`.
#[test]
fn validate_busy_port() {
    let env = TestEnv::new();
    let blocker = must(std::net::TcpListener::bind(("127.0.0.1", env.port)));
    let layout = env.layout();
    match common::block_on(layout.validate()) {
        Err(SupervisorError::PortBusy { port, .. }) => {
            drop(blocker);
            assert_eq!(port, env.port);
        }
        other => {
            drop(blocker);
            panic!("expected PortBusy, got {other:?}");
        }
    }
}

/// A clean layout validates.
#[test]
fn validate_ok() {
    let env = TestEnv::new();
    let layout = env.layout();
    must(common::block_on(layout.validate()));
}

/// ADR-087: a bundled PostgreSQL without PostGIS is refused by `validate`, naming the
/// missing control file, before `initdb` could create a data directory that cannot store a
/// `geo` field.
#[test]
fn validate_requires_the_postgis_control_file() {
    let env = TestEnv::new();
    let control = env
        ._tmp
        .path()
        .join("share")
        .join("extension")
        .join("postgis.control");
    must(std::fs::remove_file(&control));
    match common::block_on(env.layout().validate()) {
        Err(SupervisorError::PostgisMissingFromBundle { missing, prefix }) => {
            assert_eq!(prefix, env._tmp.path().display().to_string());
            assert!(
                missing
                    .iter()
                    .any(|entry| entry.contains("postgis.control")),
                "the control file must be named: {missing:?}"
            );
        }
        other => panic!("expected PostgisMissingFromBundle, got {other:?}"),
    }
}

/// The library the control file names (`module_pathname = '$libdir/postgis-3'`) must be in
/// `<prefix>/lib`: shipping the control file alone would let `CREATE EXTENSION postgis`
/// fail at first start instead of at install time.
#[test]
fn validate_requires_the_postgis_library() {
    let env = TestEnv::new();
    let library = env._tmp.path().join("lib").join("postgis-3.dll");
    must(std::fs::remove_file(&library));
    match common::block_on(env.layout().validate()) {
        Err(SupervisorError::PostgisMissingFromBundle { missing, .. }) => {
            assert!(
                missing.iter().any(|entry| entry.contains("postgis-3")),
                "the library must be named: {missing:?}"
            );
        }
        other => panic!("expected PostgisMissingFromBundle, got {other:?}"),
    }
}

/// A bundle with no installation tree at all (a `bin` directory of programs and nothing
/// else) is not checked for PostGIS: such a layout cannot run `initdb`, which fails on the
/// missing `share/` files anyway, and the crate's dependants stage exactly that shape.
#[test]
fn validate_accepts_a_bundle_without_an_installation_tree() {
    let env = TestEnv::new();
    must(std::fs::remove_dir_all(env._tmp.path().join("share")));
    must(std::fs::remove_dir_all(env._tmp.path().join("lib")));
    must(common::block_on(env.layout().validate()));
}

/// `ensure_initialised` runs initdb once, is idempotent on the second call, and
/// refuses a half-initialised directory instead of deleting it.
#[test]
fn ensure_initialised_idempotent_and_refuses_half_state() {
    let env = TestEnv::new();
    let supervisor = supervisor(&env);

    let first = must(common::block_on(supervisor.ensure_initialised("pw-test-1")));
    assert!(matches!(
        first,
        spot_pg_supervisor::InitState::FreshlyInitialised
    ));
    assert!(env.data_dir.join("PG_VERSION").is_file());
    let argv_one = std::fs::read_to_string(env.data_dir.join("fake-initdb-argv.txt"));

    let second = must(common::block_on(supervisor.ensure_initialised("pw-test-1")));
    assert!(matches!(
        second,
        spot_pg_supervisor::InitState::AlreadyInitialised
    ));

    // Half-initialised: files but no PG_VERSION.
    match std::fs::remove_file(env.data_dir.join("PG_VERSION")) {
        Ok(()) => {}
        Err(error) => panic!("remove PG_VERSION: {error}"),
    }
    if let Err(error) = std::fs::write(env.data_dir.join("base"), "leftovers") {
        panic!("write base/: {error}");
    }
    match common::block_on(supervisor.ensure_initialised("pw-test-1")) {
        Err(SupervisorError::HalfInitialised { dir }) => {
            assert_eq!(dir, env.data_dir.display().to_string());
        }
        other => panic!("expected HalfInitialised, got {other:?}"),
    }
    // The directory was not deleted.
    assert!(env.data_dir.join("base").is_file());
    // The recorded first-run argv proves what initdb was called with.
    let argv_one = argv_one.unwrap_or_else(|error| panic!("fake argv file: {error}"));
    assert!(argv_one.contains("--auth-local=peer"), "{argv_one}");
    assert!(argv_one.contains("--auth=scram-sha-256"), "{argv_one}");
    assert!(argv_one.contains("--encoding=UTF8"), "{argv_one}");
    assert!(argv_one.contains("--no-instructions"), "{argv_one}");
    assert!(argv_one.contains("--pwfile="), "{argv_one}");
}

/// The password file is removed after initdb and its contents never appear on the
/// recorded command line.
#[test]
fn password_never_on_command_line_or_left_behind() {
    let env = TestEnv::new();
    let supervisor = supervisor(&env);

    must(common::block_on(
        supervisor.ensure_initialised("super-secret-pw-42"),
    ));

    // No password file survives.
    let leftovers: Vec<_> = match std::fs::read_dir(&env.data_dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains("pwfile"))
            .collect(),
        Err(error) => panic!("read data dir: {error}"),
    };
    assert!(
        leftovers.is_empty(),
        "password file was left behind: {leftovers:?}"
    );

    // The recorded command line carries only the pwfile path, never the password.
    let argv = std::fs::read_to_string(env.data_dir.join("fake-initdb-argv.txt"))
        .unwrap_or_else(|error| panic!("fake argv file: {error}"));
    assert!(
        !argv.contains("super-secret-pw-42"),
        "password leaked into the command line: {argv}"
    );
    assert!(argv.contains("--pwfile="), "{argv}");
}

/// The rendered `spot.conf` keeps the durability switches on no matter what
/// `PgSettings` does, and protected keys cannot be smuggled in through `extra_config`
/// (PostgreSQL resolves duplicate settings last-wins).
#[test]
fn spot_conf_durability_cannot_be_disabled() {
    let settings = PgSettings {
        extra_config: vec![
            "fsync = off".to_string(),
            "full_page_writes = off".to_string(),
            "shared_buffers = '4GB'".to_string(),
            "  autovacuum = off".to_string(),
        ],
        ..PgSettings::default()
    };
    let rendered = render_spot_conf(&Listen::Localhost, &settings);
    let fsync_lines: Vec<&str> = rendered
        .lines()
        .filter(|line| line.starts_with("fsync"))
        .collect();
    assert_eq!(
        fsync_lines,
        vec!["fsync = on"],
        "fsync must stay on; render was:\n{rendered}"
    );
    let fpw_lines: Vec<&str> = rendered
        .lines()
        .filter(|line| line.starts_with("full_page_writes"))
        .collect();
    assert_eq!(
        fpw_lines,
        vec!["full_page_writes = on"],
        "full_page_writes must stay on; render was:\n{rendered}"
    );
    // The protected shared_buffers override was dropped too.
    assert!(rendered.contains("shared_buffers = '128MB'"));
    assert!(!rendered.contains("shared_buffers = '4GB'"));
    // Non-protected extras survive.
    assert!(rendered.contains("autovacuum = off"));
}

/// `spot.conf` is written and included from `postgresql.conf` after initdb.
#[test]
fn spot_conf_written_and_included() {
    let env = TestEnv::new();
    let supervisor = supervisor(&env);
    must(common::block_on(supervisor.ensure_initialised("pw-3")));
    let spot = std::fs::read_to_string(env.data_dir.join("spot.conf"))
        .unwrap_or_else(|error| panic!("spot.conf: {error}"));
    assert!(spot.contains("fsync = on"));
    assert!(spot.contains(&format!("port = {}", env.port)));
    let postgres = std::fs::read_to_string(env.data_dir.join("postgresql.conf"))
        .unwrap_or_else(|error| panic!("postgresql.conf: {error}"));
    assert!(
        postgres
            .lines()
            .any(|line| line.trim() == "include = 'spot.conf'"),
        "{postgres}"
    );
    // The _stop_grace_ config is unused here; keep the link to the defaults alive.
    let _ = Duration::from_secs(0);
}
