//! Render goldens for both shells, including the awkward values — spaces, quotes, `$`,
//! backticks, newlines, non-ASCII — plus the secret placeholder and the boolean forms.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_mgmt_ops::{Shell, render};

#[test]
fn posix_golden_the_briefs_example() {
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![("name", common::text("nginx"))]);
    let validated = catalogue.validate(&invocation).expect("valid");
    // `wait` is filled from the default (30s) and `force` is absent (no default).
    assert_eq!(
        render(&validated, Shell::Posix),
        "spot ops server service restart --name nginx --wait 30s"
    );
    // PowerShell renders identically for values that need no quoting.
    assert_eq!(
        render(&validated, Shell::PowerShell),
        "spot ops server service restart --name nginx --wait 30s"
    );
}

#[test]
fn boolean_renders_bare_and_no_forms() {
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![
        ("name", common::text("nginx")),
        ("force", common::boolean(true)),
    ]);
    let validated = catalogue.validate(&invocation).expect("valid");
    let line = render(&validated, Shell::Posix);
    assert!(line.ends_with("--force"), "{line}");
    assert!(!line.contains("--no-force"), "{line}");

    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![
        ("name", common::text("nginx")),
        ("force", common::boolean(false)),
    ]);
    let validated = catalogue.validate(&invocation).expect("valid");
    let line = render(&validated, Shell::Posix);
    assert!(line.ends_with("--no-force"), "{line}");
}

#[test]
fn a_list_repeats_its_flag() {
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("tenant.user.create");
    invocation.params = common::params(vec![
        ("user_name", common::text("ana")),
        (
            "groups",
            common::list(vec![common::text("users"), common::text("auditors")]),
        ),
    ]);
    let validated = catalogue.validate(&invocation).expect("valid");
    assert_eq!(
        render(&validated, Shell::Posix),
        "spot ops tenant user create --user-name ana --groups users --groups auditors --quota-mb 1000"
    );
}

#[test]
fn awkward_values_are_quoted_per_shell() {
    let catalogue = common::catalogue();
    // A value with a space, a dollar, a single quote and non-ASCII — the echo operation's
    // text parameter has no pattern, so it accepts exactly the values the shells find
    // awkward (the restart fixture's unit-name pattern is deliberately narrow).
    let mut invocation = common::invocation("server.service.echo");
    invocation.params = common::params(vec![(
        "message",
        common::text("web 'prod $db `.app 日本語"),
    )]);
    let validated = catalogue.validate(&invocation).expect("valid");

    let posix = render(&validated, Shell::Posix);
    // The POSIX spelling of an embedded apostrophe is the four-character '\'' escape.
    assert!(
        posix.contains("'\\''"),
        "posix escapes the quote with '\\'' : {posix}"
    );
    assert!(
        posix.contains("$db"),
        "posix keeps $ inside single quotes: {posix}"
    );
    assert!(posix.contains("日本語"), "non-ASCII rides: {posix}");

    let pwsh = render(&validated, Shell::PowerShell);
    // PowerShell doubles the apostrophe inside its single-quoted string.
    assert!(pwsh.contains("''"), "powershell doubles the quote: {pwsh}");
    assert!(pwsh.contains("日本語"), "non-ASCII rides: {pwsh}");
}

#[test]
fn the_secret_renders_only_the_placeholder() {
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("tenant.backup.run");
    invocation.params = common::params(vec![
        ("passphrase", common::text("correct horse battery staple")),
        ("level", common::text("full")),
    ]);
    let validated = catalogue.validate(&invocation).expect("valid");
    for shell in [Shell::Posix, Shell::PowerShell] {
        let line = render(&validated, shell);
        // The placeholder token `<prompt>` rides the line quoted in both shells (its
        // angle brackets would read as redirects bare): assert the flag and the
        // placeholder value, not their incidental spacing.
        assert!(line.contains("--passphrase"), "{shell:?}: {line}");
        assert!(
            line.contains("<prompt>"),
            "the placeholder is on the line ({shell:?}): {line}"
        );
        assert!(
            !line.contains("correct horse"),
            "the value never reaches the line ({shell:?}): {line}"
        );
        assert!(line.contains("--level full"), "{shell:?}: {line}");
    }
}

#[test]
fn debug_redacts_the_secret_value() {
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("tenant.backup.run");
    invocation.params = common::params(vec![(
        "passphrase",
        common::text("correct horse battery staple"),
    )]);
    let validated = catalogue.validate(&invocation).expect("valid");
    let dumped = format!("{validated:?}");
    assert!(dumped.contains("[redacted]"), "{dumped}");
    assert!(!dumped.contains("correct horse"), "{dumped}");
    // Non-secret parameters stay readable.
    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![("name", common::text("nginx"))]);
    let validated = catalogue.validate(&invocation).expect("valid");
    let dumped = format!("{validated:?}");
    assert!(dumped.contains("nginx"), "{dumped}");
}

#[test]
fn empty_and_awkward_arguments_are_quoted_deterministically() {
    // The empty argument: quoted in both shells.
    assert_eq!(spot_mgmt_ops::quote("", Shell::Posix), "''");
    assert_eq!(spot_mgmt_ops::quote("", Shell::PowerShell), "''");
    // Plain identifiers stay bare.
    assert_eq!(spot_mgmt_ops::quote("nginx", Shell::Posix), "nginx");
    assert_eq!(spot_mgmt_ops::quote("nginx", Shell::PowerShell), "nginx");
    // The POSIX escape is literal `'\''`; PowerShell doubles the quote.
    assert_eq!(spot_mgmt_ops::quote("a'b", Shell::Posix), "'a'\\''b'");
    assert_eq!(spot_mgmt_ops::quote("a'b", Shell::PowerShell), "'a''b'");
    // Rendering twice is byte-identical.
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("tenant.user.create");
    invocation.params = common::params(vec![
        ("user_name", common::text("ana o'hara")),
        (
            "groups",
            common::list(vec![common::text("a b"), common::text("c'd")]),
        ),
    ]);
    let validated = catalogue.validate(&invocation).expect("valid");
    let once = render(&validated, Shell::Posix);
    assert_eq!(render(&validated, Shell::Posix), once);
}
