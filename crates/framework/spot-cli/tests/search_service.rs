//! The search service's settings and start-up state (WP-STO-13.T6). The behaviour over a running
//! site (the projection in PostgreSQL, the G2 budgets, tenant and permission isolation) is the
//! l3-http cases `harness/rust/l3-http/src/cases/search.rs`, blocked until a DocType declares
//! `[search]` and the host installs the hook; the query itself is tested in
//! `spot-api-services/tests/search.rs` and the route shapes in `spot-http/tests/routes_search.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use spot_cli::services::search::adapters::SearchState;
use spot_cli::services::search::{REBUILD_JOB, Settings};

#[test]
fn the_defaults_are_valid_and_each_out_of_range_value_names_its_key() {
    assert_eq!(Settings::default().validate(), Ok(()));
    let cases: Vec<(&str, Settings)> = vec![
        (
            "language",
            Settings {
                language: "not a tag!".into(),
                ..Settings::default()
            },
        ),
        (
            "max_rebuild_rows_per_second",
            Settings {
                max_rebuild_rows_per_second: 0,
                ..Settings::default()
            },
        ),
        (
            "snippet_max_chars",
            Settings {
                snippet_max_chars: 19,
                ..Settings::default()
            },
        ),
        (
            "snippet_max_chars",
            Settings {
                snippet_max_chars: 201,
                ..Settings::default()
            },
        ),
        (
            "lag_bound_ms",
            Settings {
                lag_bound_ms: 0,
                ..Settings::default()
            },
        ),
    ];
    for (key, settings) in cases {
        assert_eq!(settings.validate(), Err(key), "{key}");
    }
}

#[test]
fn unknown_settings_keys_are_refused() {
    let parsed: Result<Settings, _> = toml::from_str("enabled = true\nfuzzy = 3\n");
    assert!(parsed.is_err());
    let parsed: Settings = toml::from_str("language = \"ur-PK\"\nlag_bound_ms = 100\n").unwrap();
    assert_eq!(parsed.language, "ur-PK");
    assert_eq!(parsed.lag_bound_ms, 100);
    assert!(parsed.enabled, "search is on unless a site turns it off");
}

#[test]
fn the_start_up_state_holds_the_degraded_reason_until_it_is_cleared() {
    let state = SearchState::default();
    assert_eq!(state.problem(), None);
    state.set(Some("the pg_trgm extension is missing".to_owned()));
    assert!(state.problem().unwrap().contains("pg_trgm"));
    let shared = state.clone();
    shared.set(None);
    assert_eq!(state.problem(), None, "clones share one state");
    assert_eq!(REBUILD_JOB, "platform:search.rebuild");
}
