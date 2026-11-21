//! Unit tests of `extension.install`: every check, the order, the hash and app safety.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;

fn req(name: &str) -> ExtensionRequest {
    ExtensionRequest::new(name)
}

fn checked(name: &str) -> Checked {
    check_request(&req(name)).unwrap()
}

/// Facts for a plain trusted-extension case: the role holds the privilege, the server offers
/// `version`, nothing is installed.
fn facts(version: Option<&str>) -> Facts {
    Facts {
        server_major: 16,
        superuser: false,
        create_on_database: true,
        create_on_schema: Some(true),
        trusted: Some(true),
        live: LiveExtension {
            name: "pg_trgm".into(),
            available_version: version.map(str::to_owned),
            installed_version: None,
        },
    }
}

// ---- check 1 ----

#[test]
fn the_allow_list_is_the_four_extensions_and_nothing_else() {
    assert_eq!(
        ALLOWED_EXTENSIONS,
        ["postgis", "btree_gist", "pg_trgm", "unaccent"]
    );
    for name in ALLOWED_EXTENSIONS {
        assert_eq!(checked(name).name, name);
    }
}

#[test]
fn an_off_list_name_is_refused_with_its_own_code() {
    for name in [
        "hstore",
        "dblink",
        "file_fdw",
        "plpython3u",
        "POSTGIS",
        "postgis ",
        "",
    ] {
        let e = check_request(&req(name)).unwrap_err();
        assert_eq!(e.code(), codes::EXTENSION_NOT_ALLOWED, "{name:?}");
    }
}

#[test]
fn a_hostile_name_never_reaches_an_error_message() {
    let evil = "postgis\"; DROP TABLE x; --";
    let e = check_request(&req(evil)).unwrap_err();
    assert_eq!(e.code(), codes::EXTENSION_NOT_ALLOWED);
    let text = e.to_string();
    assert!(!text.contains("DROP") && !text.contains('"'), "{text}");
    assert_eq!(display_name(evil), "<invalid>");
    // A plain identifier that is merely off the list is echoed: it carries no injection.
    assert_eq!(display_name("hstore"), "hstore");
}

#[test]
fn schema_and_version_bound_are_validated() {
    let mut r = req("pg_trgm");
    r.schema = Some("tenant_1".into());
    assert_eq!(check_request(&r).unwrap().schema, "tenant_1");
    for bad in [
        "Public",
        "a b",
        "a\"b",
        "pg_catalog",
        "pg_temp",
        "1x",
        "",
        "a;b",
    ] {
        r.schema = Some(bad.into());
        let e = check_request(&r).unwrap_err();
        assert_eq!(e.code(), codes::REFUSED, "{bad:?}");
    }
    r.schema = None;
    assert_eq!(check_request(&r).unwrap().schema, "public");
    for bad in ["", "1..2", "1.x", "1.2;", "v1"] {
        r.min_version = Some(bad.into());
        let e = check_request(&r).unwrap_err();
        assert_eq!(e.code(), codes::EXTENSION_VERSION, "{bad:?}");
    }
    r.min_version = Some("1.6".into());
    assert_eq!(check_request(&r).unwrap().min_version, "1.6");
    r.min_version = None;
    assert_eq!(
        check_request(&r).unwrap().min_version,
        default_min_version("pg_trgm")
    );
}

// ---- check 2 (idempotence) ----

#[test]
fn present_at_an_acceptable_version_is_a_no_op_that_needs_no_privilege() {
    let mut f = facts(Some("1.6"));
    f.live.installed_version = Some("1.6".into());
    f.create_on_database = false;
    let v = evaluate(&checked("pg_trgm"), &f).unwrap();
    assert!(v.already_present && !v.privilege_held);
    assert_eq!(v.version, "1.6");
}

// ---- check 3 (privilege) ----

#[test]
fn privilege_follows_superuser_trust_and_create() {
    let mut f = facts(Some("1.6"));
    assert!(privilege_held("pg_trgm", &f), "trusted + CREATE");
    f.create_on_database = false;
    assert!(!privilege_held("pg_trgm", &f), "no CREATE on the database");
    f.create_on_database = true;
    f.create_on_schema = Some(false);
    assert!(!privilege_held("pg_trgm", &f), "no CREATE on the schema");
    f.create_on_schema = Some(true);
    f.trusted = Some(false);
    assert!(
        !privilege_held("pg_trgm", &f),
        "not trusted needs a superuser"
    );
    f.superuser = true;
    assert!(privilege_held("pg_trgm", &f), "a superuser may always");
}

#[test]
fn expected_trust_is_the_oracle_for_the_server_flag() {
    assert!(!expected_trusted("postgis"));
    for n in ["btree_gist", "pg_trgm", "unaccent"] {
        assert!(expected_trusted(n), "{n}");
    }
    // With no flag from the server the hard-coded expectation decides.
    let mut f = facts(Some("3.4"));
    f.trusted = None;
    assert!(!privilege_held("postgis", &f));
    assert!(privilege_held("pg_trgm", &f));
}

#[test]
fn missing_privilege_names_the_extension_and_the_exact_statement() {
    let mut f = facts(Some("3.4"));
    f.trusted = Some(false);
    let e = evaluate(&checked("postgis"), &f).unwrap_err();
    assert_eq!(e.code(), codes::EXTENSION_PRIVILEGE_MISSING);
    let text = e.to_string();
    assert!(
        text.contains("postgis") && text.contains("CREATE EXTENSION postgis;"),
        "{text}"
    );
    let mut r = req("postgis");
    r.schema = Some("gis".into());
    let e = evaluate(&check_request(&r).unwrap(), &f).unwrap_err();
    assert!(
        e.to_string()
            .contains("CREATE EXTENSION postgis SCHEMA gis;")
    );
}

// ---- check 4 (version) ----

#[test]
fn version_failures_name_the_cause() {
    let c = checked("pg_trgm");
    let e = evaluate(&c, &facts(Some("1.0"))).unwrap_err();
    assert_eq!(e.code(), codes::EXTENSION_VERSION);
    let e = evaluate(&c, &facts(None)).unwrap_err();
    assert_eq!(e.code(), codes::EXTENSION_UNAVAILABLE);
    let mut f = facts(Some("1.6"));
    f.server_major = 12;
    assert_eq!(
        evaluate(&c, &f).unwrap_err().code(),
        codes::EXTENSION_VERSION
    );
    let mut f = facts(Some("1.6"));
    f.live.installed_version = Some("1.0".into());
    assert_eq!(
        evaluate(&c, &f).unwrap_err().code(),
        codes::EXTENSION_VERSION
    );
    let v = evaluate(&c, &facts(Some("1.6"))).unwrap();
    assert_eq!((v.version.as_str(), v.already_present), ("1.6", false));
}

// ---- the order ----

#[test]
fn which_error_wins_when_several_conditions_hold() {
    // (name, schema, facts tweak) -> expected code
    struct Case {
        name: &'static str,
        schema: Option<&'static str>,
        tweak: fn(&mut Facts),
        want: &'static str,
    }
    let cases = [
        // Off-list beats everything, including a bad schema.
        Case {
            name: "hstore",
            schema: Some("Bad Schema"),
            tweak: |f| f.create_on_database = false,
            want: codes::EXTENSION_NOT_ALLOWED,
        },
        // Privilege beats version and availability.
        Case {
            name: "pg_trgm",
            schema: None,
            tweak: |f| {
                f.create_on_database = false;
                f.live.available_version = Some("0.1".into());
            },
            want: codes::EXTENSION_PRIVILEGE_MISSING,
        },
        Case {
            name: "pg_trgm",
            schema: None,
            tweak: |f| {
                f.create_on_database = false;
                f.live.available_version = None;
                f.server_major = 11;
            },
            want: codes::EXTENSION_PRIVILEGE_MISSING,
        },
        // With the privilege, an old server beats a missing offer.
        Case {
            name: "pg_trgm",
            schema: None,
            tweak: |f| {
                f.live.available_version = None;
                f.server_major = 11;
            },
            want: codes::EXTENSION_VERSION,
        },
        Case {
            name: "pg_trgm",
            schema: None,
            tweak: |f| f.live.available_version = None,
            want: codes::EXTENSION_UNAVAILABLE,
        },
        // A missing schema is refused before the privilege is judged.
        Case {
            name: "pg_trgm",
            schema: Some("nowhere"),
            tweak: |f| {
                f.create_on_schema = None;
                f.create_on_database = false;
            },
            want: codes::REFUSED,
        },
    ];
    for (i, c) in cases.iter().enumerate() {
        let mut r = req(c.name);
        r.schema = c.schema.map(str::to_owned);
        let mut f = facts(Some("1.6"));
        (c.tweak)(&mut f);
        let got = check_request(&r)
            .and_then(|checked| evaluate(&checked, &f))
            .unwrap_err();
        assert_eq!(got.code(), c.want, "case {i}: {got}");
    }
}

// ---- the plan hash ----

#[test]
fn the_plan_hash_is_stable_and_follows_request_and_state() {
    let c = checked("pg_trgm");
    let f = facts(Some("1.6"));
    let v = evaluate(&c, &f).unwrap();
    let h = plan_hash(&c, &f, &v);
    assert_eq!(h, plan_hash(&c, &f, &v), "stable");
    assert_eq!(h.len(), 64);
    // Pinned: a change here means the hashing changed and approvals in flight break.
    assert_eq!(
        h, "aac6070c4aeff6321280b865526611804d5131658328d7250ee4d308e11a56e2",
        "update the pin only on purpose"
    );
    let mut f2 = f.clone();
    f2.live.available_version = Some("1.7".into());
    let v2 = evaluate(&c, &f2).unwrap();
    assert_ne!(h, plan_hash(&c, &f2, &v2), "state moved");
    let mut f3 = f.clone();
    f3.live.installed_version = Some("1.6".into());
    let v3 = evaluate(&c, &f3).unwrap();
    assert_ne!(h, plan_hash(&c, &f3, &v3), "installed meanwhile");
    let mut r = req("pg_trgm");
    r.schema = Some("other".into());
    let c2 = check_request(&r).unwrap();
    assert_ne!(h, plan_hash(&c2, &f, &evaluate(&c2, &f).unwrap()), "schema");
}

// ---- the descriptor ----

#[test]
fn the_descriptor_is_dangerous_and_signed() {
    assert_eq!(EXTENSION_INSTALL.name, concat!("extension", ".", "install"));
    assert_eq!(EXTENSION_INSTALL.risk, OperationRisk::Dangerous);
    const { assert!(EXTENSION_INSTALL.requires_administrator_signature) };
    let names: Vec<_> = EXTENSION_INSTALL
        .parameters
        .iter()
        .map(|p| p.name)
        .collect();
    assert_eq!(names, ["name", "min_version", "schema", "plan_hash"]);
    assert!(EXTENSION_INSTALL.parameters[0].required);
}

// ---- app safety ----

#[test]
fn only_this_module_can_create_an_extension() {
    for (file, src) in [
        ("apply.rs", include_str!("../apply.rs")),
        ("plan.rs", include_str!("../plan.rs")),
        ("ddl.rs", include_str!("../ddl.rs")),
        ("live.rs", include_str!("../live.rs")),
        ("names.rs", include_str!("../names.rs")),
        ("conn.rs", include_str!("../conn.rs")),
        ("error.rs", include_str!("../error.rs")),
    ] {
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
            .to_ascii_uppercase();
        assert!(
            !code.contains("CREATE EXTENSION"),
            "{file} must not create an extension; extension.install owns that"
        );
    }
}
