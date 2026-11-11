//! The sample app's golden resolved document, and the byte-for-byte stability of the merge.

mod support;

use std::fs;
use std::path::PathBuf;

use spot_ui_ext::{
    AppContribution, Format, InstalledApp, Resolved, SiteContext, parse_document,
    parse_document_as, resolve,
};

fn sample_dir() -> PathBuf {
    support::contracts_dir().join("examples/sample-app")
}

fn sample_contribution() -> AppContribution {
    let path = sample_dir().join("ui.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    support::sample_contribution(vec![("ui.json".to_owned(), text)])
}

fn site(context: &support::Context) -> SiteContext {
    SiteContext::new(
        context
            .app_order
            .iter()
            .map(|app| {
                let version = if app == &context.app {
                    "1.0.0"
                } else {
                    "0.1.0"
                };
                InstalledApp::new(app.clone(), version)
            })
            .collect(),
        "0.1.0",
    )
}

fn sample_resolved() -> Resolved {
    resolve(&[sample_contribution()], &site(&support::context()))
        .unwrap_or_else(|findings| panic!("the sample app must resolve: {findings:?}"))
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/sample-app.resolved.json")
}

#[test]
fn the_sample_app_resolves_to_the_committed_canonical_json() {
    let resolved = sample_resolved();
    let canonical = resolved.to_canonical_json();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(golden_path(), format!("{canonical}\n"))
            .unwrap_or_else(|e| panic!("golden file: {e}"));
        return;
    }
    let text = fs::read_to_string(golden_path()).unwrap_or_else(|e| panic!("golden file: {e}"));
    assert_eq!(
        canonical,
        text.trim_end_matches('\n'),
        "the golden file is stale"
    );

    assert_eq!(resolved.requires_shell(), "0.1.0");
    assert_eq!(resolved.app_order(), vec!["sample"]);
    assert_eq!(resolved.hash().len(), 64);
    let toolbar = resolved
        .slot("doc.toolbar")
        .unwrap_or_else(|| panic!("the sample app fills doc.toolbar"));
    let ids: Vec<&str> = toolbar.items().iter().map(|item| item.id()).collect();
    assert_eq!(ids, vec!["sample.start_work", "sample.open_time"]);
    assert!(resolved.component("sample.project_health").is_some());
    for slot in resolved.slots() {
        assert_eq!(slot.hash().len(), 64, "{} has a hash", slot.name());
        assert!(slot.to_canonical_json().contains("hash"));
    }
    assert!(resolved.shell_upgrade_needed().is_none());
}

#[test]
fn the_sample_app_reads_the_same_from_json_and_toml() {
    let path = sample_dir().join("ui.json");
    let json = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let document =
        parse_document("ui.json", &json).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let toml = toml::to_string(&document).unwrap_or_else(|e| panic!("to TOML: {e}"));
    let from_toml = parse_document_as("ui.toml", &toml, Format::Toml)
        .unwrap_or_else(|e| panic!("from TOML: {e}\n{toml}"));
    assert_eq!(document, from_toml);
}

#[test]
fn shuffling_contributions_one_hundred_times_gives_the_same_bytes() {
    let apps = ["alpha", "beta", "gamma"];
    let contributions: Vec<AppContribution> = apps
        .iter()
        .map(|app| {
            support::contribution(
                app,
                "1.0.0",
                &[],
                vec![support::document(
                    app,
                    vec![support::action(app, "one", "shell.sidebar", 3)],
                )],
            )
        })
        .collect();
    let site = SiteContext::new(
        apps.iter()
            .map(|app| InstalledApp::new(*app, "1.0.0"))
            .collect(),
        "0.1.0",
    );
    let expected = resolve(&contributions, &site)
        .unwrap_or_else(|findings| panic!("{findings:?}"))
        .to_canonical_json();
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    for round in 0..100 {
        let mut order: Vec<usize> = (0..contributions.len()).collect();
        for position in (1..order.len()).rev() {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            order.swap(position, (seed >> 33) as usize % (position + 1));
        }
        let shuffled: Vec<AppContribution> = order
            .iter()
            .map(|index| contributions[*index].clone())
            .collect();
        let resolved = resolve(&shuffled, &site)
            .unwrap_or_else(|findings| panic!("round {round}: {findings:?}"));
        assert_eq!(resolved.to_canonical_json(), expected, "round {round}");
    }
}

#[test]
fn a_toml_document_with_an_unknown_key_is_a_schema_finding() {
    let toml = "contract_version = \"0.1.0\"\nrequires_shell = \"0.1.0\"\napp = \"sample\"\nonclick = \"alert(1)\"\n";
    let finding = match parse_document_as("ui.toml", toml, Format::Toml) {
        Err(finding) => finding,
        Ok(_) => panic!("the unknown key must be refused"),
    };
    assert_eq!(finding.code, spot_ui_ext::codes::SCHEMA);
    assert_eq!(finding.file, "ui.toml");
    assert!(finding.message.contains("onclick"), "{}", finding.message);
}

#[test]
fn a_different_order_hint_gives_a_different_hash() {
    let resolved = sample_resolved();
    let mut changed = sample_contribution();
    let text = &mut changed.documents[0].1;
    assert!(text.contains("\"order\": 20"));
    *text = text.replace("\"order\": 20", "\"order\": 21");
    let other = resolve(&[changed], &site(&support::context()))
        .unwrap_or_else(|findings| panic!("{findings:?}"));
    assert_ne!(resolved.hash(), other.hash());
}
