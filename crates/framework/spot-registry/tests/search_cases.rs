#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::*;
use spot_registry::*;
fn catalogue() -> Setup {
    let s = Setup::policy(RegistryPolicy {
        capabilities: spot_bundle::Policy {
            allow_email: true,
            ..Default::default()
        },
        ..Default::default()
    });
    s.publish("payroll", "1.0.0", "requires_host=\"^0.1\"", 12);
    s.publish("payroll", "2.0.0-alpha.1", "requires_host=\"^0.1\"", 12);
    s.publish("payroll_tools", "1.0.0", "", 12);
    s.publish("hr_tools","1.0.0","requires_host=\">=0.1, <0.2\"\n[capabilities]\nfiles=\"read\"\nnetwork=[\"example.test:443\"]\ndoctypes={read=[\"Document\"],write=[\"Document\"]}",13);
    let m = manifest("finance", "1.0.0", "[capabilities]\nemail=true")
        .replace("title = \"finance\"", "title = \"Ledger Studio\"");
    publish(
        &s.registry,
        spot_bundle::BundleBuilder::new(m, COMPONENT)
            .build()
            .unwrap(),
        13,
    )
    .unwrap();
    s.publish("remote", "1.0.0", "", 12);
    s.publish("beta", "1.0.0-alpha", "", 12);
    let archived = s.publish("archive", "1.0.0", "", 12);
    let hidden = s.publish("hidden", "1.0.0", "", 13);
    let p12 = s.caller(12);
    let p13 = s.caller(13);
    for (name, desc, cat, caller) in [
        ("payroll", "Salary management", "hr", &p12),
        ("payroll_tools", "Salary toolkit", "hr", &p12),
        ("hr_tools", "Staff records", "hr", &p13),
        ("finance", "Payroll ledger salary", "accounting", &p13),
        ("remote", "Network deployment", "operations", &p12),
    ] {
        s.registry
            .describe(&app(name), desc.into(), cat.into(), caller, NOW)
            .unwrap();
    }
    s.registry
        .yank(
            &archived.app,
            &archived.version,
            "obsolete".into(),
            &p12,
            NOW,
        )
        .unwrap();
    s.registry
        .withdraw(&hidden.app, &hidden.version, "malware".into(), &p13, NOW)
        .unwrap();
    s
}
fn names(s: &Setup, q: SearchQuery) -> Vec<String> {
    s.registry
        .search(&q, NOW)
        .unwrap()
        .releases
        .iter()
        .map(|r| r.app.as_str().to_owned())
        .collect()
}
macro_rules! search_case {
    ($name:ident, $query:expr, [$($expected:expr),*]) => {
        #[test] fn $name() { let s=catalogue(); let q: SearchQuery=$query; assert_eq!(names(&s,q),vec![$($expected.to_owned()),*] as Vec<String>); }
    };
}
search_case!(
    case01_blank_deterministic,
    SearchQuery::default(),
    ["finance", "hr_tools", "payroll", "payroll_tools", "remote"]
);
search_case!(
    case02_exact_before_prefix_and_overlap,
    SearchQuery {
        text: "payroll".into(),
        ..Default::default()
    },
    ["payroll", "payroll_tools", "finance"]
);
search_case!(
    case03_name_prefix,
    SearchQuery {
        text: "pay".into(),
        ..Default::default()
    },
    ["payroll", "payroll_tools"]
);
search_case!(
    case04_case_insensitive,
    SearchQuery {
        text: "PAYROLL".into(),
        ..Default::default()
    },
    ["payroll", "payroll_tools", "finance"]
);
search_case!(
    case05_underscore_tokenization,
    SearchQuery {
        text: "tools".into(),
        ..Default::default()
    },
    ["hr_tools", "payroll_tools"]
);
search_case!(
    case06_title_token,
    SearchQuery {
        text: "studio".into(),
        ..Default::default()
    },
    ["finance"]
);
search_case!(
    case07_description_token,
    SearchQuery {
        text: "staff".into(),
        ..Default::default()
    },
    ["hr_tools"]
);
search_case!(
    case08_overlap_rank,
    SearchQuery {
        text: "payroll salary".into(),
        ..Default::default()
    },
    ["finance", "payroll", "payroll_tools"]
);
search_case!(
    case09_description_overlap_tie,
    SearchQuery {
        text: "salary".into(),
        ..Default::default()
    },
    ["finance", "payroll", "payroll_tools"]
);
search_case!(
    case10_punctuation,
    SearchQuery {
        text: "studio!!!".into(),
        ..Default::default()
    },
    ["finance"]
);
search_case!(
    case11_publisher12,
    SearchQuery {
        publisher: Some("publisher12".into()),
        ..Default::default()
    },
    ["payroll", "payroll_tools", "remote"]
);
search_case!(
    case12_publisher13,
    SearchQuery {
        publisher: Some("publisher13".into()),
        ..Default::default()
    },
    ["finance", "hr_tools"]
);
search_case!(
    case13_category,
    SearchQuery {
        category: Some("hr".into()),
        ..Default::default()
    },
    ["hr_tools", "payroll", "payroll_tools"]
);
search_case!(
    case14_category_case_insensitive,
    SearchQuery {
        category: Some("HR".into()),
        ..Default::default()
    },
    ["hr_tools", "payroll", "payroll_tools"]
);
search_case!(
    case15_combined_category_publisher,
    SearchQuery {
        publisher: Some("publisher12".into()),
        category: Some("hr".into()),
        ..Default::default()
    },
    ["payroll", "payroll_tools"]
);
search_case!(
    case16_host_compatible,
    SearchQuery {
        compatible_host: Some(version("0.1.5")),
        ..Default::default()
    },
    ["finance", "hr_tools", "payroll", "payroll_tools", "remote"]
);
search_case!(
    case17_host_incompatible,
    SearchQuery {
        compatible_host: Some(version("2.0.0")),
        ..Default::default()
    },
    ["finance", "payroll_tools", "remote"]
);
search_case!(
    case18_network_capability,
    SearchQuery {
        capability: Some("network".into()),
        ..Default::default()
    },
    ["hr_tools"]
);
search_case!(
    case19_email_capability,
    SearchQuery {
        capability: Some("email".into()),
        ..Default::default()
    },
    ["finance"]
);
search_case!(
    case20_files_capability,
    SearchQuery {
        capability: Some("files".into()),
        ..Default::default()
    },
    ["hr_tools"]
);
search_case!(
    case21_read_capability,
    SearchQuery {
        capability: Some("doctypes.read".into()),
        ..Default::default()
    },
    ["hr_tools"]
);
search_case!(
    case22_write_capability,
    SearchQuery {
        capability: Some("doctypes.write".into()),
        ..Default::default()
    },
    ["hr_tools"]
);
search_case!(
    case23_unknown_capability,
    SearchQuery {
        capability: Some("unknown".into()),
        ..Default::default()
    },
    []
);
search_case!(
    case24_no_match,
    SearchQuery {
        text: "nonexistent".into(),
        ..Default::default()
    },
    []
);
search_case!(
    case25_yanked_hidden,
    SearchQuery {
        text: "archive".into(),
        ..Default::default()
    },
    []
);
search_case!(
    case26_withdrawn_hidden,
    SearchQuery {
        text: "hidden".into(),
        ..Default::default()
    },
    []
);
search_case!(
    case27_pre_hidden,
    SearchQuery {
        text: "beta".into(),
        ..Default::default()
    },
    []
);
search_case!(
    case28_pre_explicit,
    SearchQuery {
        text: "beta".into(),
        visibility: Visibility {
            include_pre: true,
            include_inactive: false
        },
        ..Default::default()
    },
    ["beta"]
);
search_case!(
    case29_all_filters,
    SearchQuery {
        text: "staff".into(),
        publisher: Some("publisher13".into()),
        category: Some("hr".into()),
        compatible_host: Some(version("0.1.0")),
        capability: Some("files".into()),
        ..Default::default()
    },
    ["hr_tools"]
);
search_case!(
    case30_first_page,
    SearchQuery {
        limit: 2,
        ..Default::default()
    },
    ["finance", "hr_tools"]
);
#[test]
fn pagination_snapshot_query_binding_and_no_duplicates() {
    let s = catalogue();
    let mut q = SearchQuery {
        limit: 2,
        ..Default::default()
    };
    let mut all = vec![];
    loop {
        let page = s.registry.search(&q, NOW).unwrap();
        all.extend(page.releases.iter().map(|r| r.app.as_str().to_owned()));
        q.cursor = page.next_cursor;
        if q.cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        all,
        vec!["finance", "hr_tools", "payroll", "payroll_tools", "remote"]
    );
    let first = s
        .registry
        .search(
            &SearchQuery {
                limit: 2,
                ..Default::default()
            },
            NOW,
        )
        .unwrap();
    let cursor = first.next_cursor.unwrap();
    assert_code(
        s.registry.search(
            &SearchQuery {
                cursor: Some(cursor.clone()),
                limit: 2,
                text: "changed".into(),
                ..Default::default()
            },
            NOW,
        ),
        "registry.invalid_cursor",
    );
    assert_code(
        s.registry.search(
            &SearchQuery {
                cursor: Some("garbage".into()),
                ..Default::default()
            },
            NOW,
        ),
        "registry.invalid_cursor",
    );
    s.publish("another", "1.0.0", "", 12);
    assert_code(
        s.registry.search(
            &SearchQuery {
                cursor: Some(cursor),
                limit: 2,
                ..Default::default()
            },
            NOW,
        ),
        "registry.invalid_cursor",
    );
}
#[test]
fn search_visibility_authorization_and_latest_prerelease() {
    let s = catalogue();
    let mut q = SearchQuery {
        visibility: Visibility {
            include_pre: true,
            include_inactive: true,
        },
        ..Default::default()
    };
    assert_code(s.registry.search(&q, NOW), "registry.unauthorized");
    q.caller = Some(s.caller(12));
    assert_eq!(
        names(&s, q.clone()),
        vec![
            "archive",
            "beta",
            "finance",
            "hr_tools",
            "payroll",
            "payroll_tools",
            "remote"
        ]
    );
    q.caller = Some(s.caller(13));
    assert_eq!(
        names(&s, q),
        vec![
            "beta",
            "finance",
            "hidden",
            "hr_tools",
            "payroll",
            "payroll_tools",
            "remote"
        ]
    );
    let stable = s
        .registry
        .search(
            &SearchQuery {
                text: "payroll".into(),
                ..Default::default()
            },
            NOW,
        )
        .unwrap();
    assert_eq!(stable.releases[0].version, version("1.0.0"));
    let pre = s
        .registry
        .search(
            &SearchQuery {
                text: "payroll".into(),
                visibility: Visibility {
                    include_pre: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            NOW,
        )
        .unwrap();
    assert_eq!(pre.releases[0].version, version("2.0.0-alpha.1"));
}
#[test]
fn invalid_page_sizes_refused() {
    let s = catalogue();
    for limit in [0, 101, usize::MAX] {
        assert_code(
            s.registry.search(
                &SearchQuery {
                    limit,
                    ..Default::default()
                },
                NOW,
            ),
            "registry.invalid_query",
        );
    }
}
