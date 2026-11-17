//! The sync_hub_grants module's settings: parsed with unknown keys refused, the policy built
//! from them, illegal keys named.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_cli::services::sync_hub_grants::Settings;

fn parse(text: &str) -> Result<Settings, toml::de::Error> {
    toml::from_str(text)
}

#[test]
fn a_full_table_builds_a_policy() {
    let settings = parse(
        r#"
default_max = 1000
min_interval_secs = 60
max_series = 8
series_max = { INV = 5000 }
entitle = [{ node = "edge-khi#branch_07", series = "INV" }]
"#,
    )
    .unwrap();
    let policy = settings.build().unwrap();
    assert_eq!(settings.entitle.len(), 1);
    drop(policy);
}

#[test]
fn an_empty_table_entitles_nothing_and_builds() {
    let settings = parse("").unwrap();
    assert!(settings.entitle.is_empty());
    assert!(settings.build().is_ok());
}

#[test]
fn unknown_keys_are_refused() {
    assert!(parse("entitled = []").is_err());
    assert!(parse("entitle = [{ node = \"a\", series = \"S\", extra = 1 }]").is_err());
}

#[test]
fn an_illegal_series_or_node_is_named() {
    let bad_series = parse("entitle = [{ node = \"edge-1#b\", series = \"\" }]").unwrap();
    let error = bad_series.build().unwrap_err();
    assert!(error.contains("series key"), "{error}");
    let bad_node = parse("entitle = [{ node = \"\", series = \"INV\" }]").unwrap();
    assert!(bad_node.build().unwrap_err().contains("node id"));
    let bad_max = parse("series_max = { \"\" = 5 }").unwrap();
    assert!(bad_max.build().is_err());
}
