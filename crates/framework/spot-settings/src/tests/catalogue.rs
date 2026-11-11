//! The compiled-in seed catalogue and the TOML loader's robustness.

use proptest::prelude::*;

use crate::builder::RegistryBuilder;
use crate::catalogue::{SEED_FILE_COUNT, load_seed_catalogue, parse_spec};
use crate::tests::{build, text_spec};

#[test]
fn the_seed_catalogue_has_ten_settings_that_build() {
    let specs = load_seed_catalogue().unwrap();
    assert_eq!(specs.len(), 10);
    assert_eq!(SEED_FILE_COUNT, 10);
    let keys: Vec<&str> = specs.iter().map(|spec| spec.key.as_str()).collect();
    for expected in [
        "runtime.http.listen_address",
        "runtime.session.idle_timeout",
        "runtime.jobs.worker_concurrency",
        "runtime.logging.level",
        "tenant.backup.retention_days",
        "runtime.smtp.host",
        "tenant.default_currency",
        "company.fiscal_year_start",
        "os.kernel.version",
        "runtime.smtp.password",
    ] {
        assert!(keys.contains(&expected), "seed catalogue lacks {expected}");
    }
    // Building proves every default is valid and the secret carries none.
    let registry = build(specs);
    assert_eq!(registry.len(), 10);
    assert_eq!(registry.get("runtime.smtp.password").unwrap().default, None);
}

#[test]
fn a_spec_round_trips_through_toml() {
    let spec = text_spec(
        "runtime.http.address",
        "0.0.0.0:8080",
        vec![crate::model::Scope::Fleet],
    );
    let text = toml::to_string_pretty(&spec).unwrap();
    let parsed = parse_spec(&text).unwrap();
    assert_eq!(parsed, spec);
}

#[test]
fn a_malformed_spec_is_a_reported_error_not_a_panic() {
    let report = parse_spec("this is not toml = = =").unwrap_err();
    assert!(report.has_code("settings.catalogue_invalid"));
}

#[test]
fn the_seed_files_are_the_owned_settings_not_duplicated_in_code() {
    // The catalogue is compiled in; a spec registered twice would fail the build.
    let specs = load_seed_catalogue().unwrap();
    let registry = RegistryBuilder::new()
        .register_all(specs.clone())
        .build()
        .unwrap();
    assert_eq!(registry.len(), specs.len());
}

proptest! {
    #[test]
    fn parse_spec_never_panics_on_arbitrary_text(text in "\\PC{0,300}") {
        // Any outcome is fine; the point is that no input panics.
        let _ = parse_spec(&text);
    }

    #[test]
    fn parse_spec_never_panics_on_toml_shaped_junk(text in "[-a-z0-9_ =\\.\\[\\]\"]{0,300}") {
        let _ = parse_spec(&text);
    }
}
