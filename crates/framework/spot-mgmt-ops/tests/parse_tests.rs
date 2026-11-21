//! The parser: golden parses of what render writes, and every refusal shape.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_mgmt_ops::ParseError;
use spot_mgmt_ops::{Invocation, ParamValue};

/// Parses argv against the catalogue (the function lives at `parse::parse`; re-exported at
/// the crate root as `parse` — a module name, so call it fully qualified).
fn parse_line(
    catalogue: &spot_mgmt_ops::Catalogue,
    args: &[String],
) -> Result<Invocation, ParseError> {
    spot_mgmt_ops::parse::parse(catalogue, args)
}

#[test]
fn the_briefs_example_command_parses_to_the_typed_invocation() {
    let catalogue = common::catalogue();
    let args: Vec<String> = [
        "spot", "ops", "server", "service", "restart", "--name", "nginx", "--wait", "30s",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let invocation = parse_line(&catalogue, &args).expect("parses");
    assert_eq!(invocation.op.as_str(), "server.service.restart");
    assert_eq!(
        invocation.params.get("name"),
        Some(&ParamValue::Text("nginx".to_owned()))
    );
    assert_eq!(
        invocation.params.get("wait"),
        Some(&ParamValue::Duration(30))
    );
}

#[test]
fn booleans_flags_and_lists_parse() {
    let catalogue = common::catalogue();
    let args: Vec<String> = [
        "spot", "ops", "server", "service", "restart", "--name", "web", "--force", "--wait", "5s",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let invocation = parse_line(&catalogue, &args).expect("parses");
    assert_eq!(
        invocation.params.get("force"),
        Some(&ParamValue::Bool(true))
    );

    let args: Vec<String> = [
        "spot",
        "ops",
        "server",
        "service",
        "restart",
        "--name",
        "web",
        "--no-force",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let invocation = parse_line(&catalogue, &args).expect("parses");
    assert_eq!(
        invocation.params.get("force"),
        Some(&ParamValue::Bool(false))
    );

    let args: Vec<String> = [
        "spot",
        "ops",
        "tenant",
        "user",
        "create",
        "--user-name",
        "ana",
        "--groups",
        "users",
        "--groups",
        "auditors",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let invocation = parse_line(&catalogue, &args).expect("parses");
    assert_eq!(
        invocation.params.get("groups"),
        Some(&ParamValue::List(vec![
            ParamValue::Text("users".to_owned()),
            ParamValue::Text("auditors".to_owned()),
        ]))
    );
}

#[test]
fn refusals_carry_their_codes() {
    let catalogue = common::catalogue();

    let args: Vec<String> = vec!["ls".to_owned(), "-l".to_owned()];
    let error = parse_line(&catalogue, &args).expect_err("not an ops command");
    assert_eq!(error.code(), "ops.parse_not_ops");

    let args: Vec<String> = vec!["spot".to_owned(), "ops".to_owned(), "web".to_owned()];
    let error = parse_line(&catalogue, &args).expect_err("one segment only");
    assert_eq!(error.code(), "ops.parse_bad_path");

    let args: Vec<String> = ["spot", "ops", "cluster", "node", "eject"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let error = parse_line(&catalogue, &args).expect_err("unknown op");
    assert_eq!(error.code(), "ops.unknown_op");

    let args: Vec<String> = [
        "spot", "ops", "server", "service", "restart", "--nam", "web",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let error = parse_line(&catalogue, &args).expect_err("unknown param");
    assert_eq!(error.code(), "ops.unknown_param");

    let args: Vec<String> = ["spot", "ops", "server", "service", "restart", "--name"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let error = parse_line(&catalogue, &args).expect_err("missing value");
    assert_eq!(error.code(), "ops.parse_bad_flag");

    let args: Vec<String> = [
        "spot", "ops", "server", "service", "restart", "--wait", "30",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let error = parse_line(&catalogue, &args).expect_err("no s suffix");
    assert_eq!(error.code(), "ops.parse_bad_value");

    let args: Vec<String> = [
        "spot", "ops", "server", "service", "restart", "--wait", "fast",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let error = parse_line(&catalogue, &args).expect_err("not a number");
    assert_eq!(error.code(), "ops.parse_bad_value");

    let args: Vec<String> = [
        "spot", "ops", "server", "service", "restart", "--name", "web", "--name", "db",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let error = parse_line(&catalogue, &args).expect_err("duplicate non-list param");
    assert_eq!(error.code(), "ops.parse_duplicate");
}

#[test]
fn quoted_things_arrive_as_one_argument_from_a_real_shell() {
    // The property test proves the round trip; this test proves the pipeline end to end
    // with a REAL shell: render, quote, `sh -c 'printf ...'`, read the argv back, parse.
    if cfg!(windows) {
        return; // `sh -c` does not exist on Windows; the PowerShell test runs there.
    }
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("server.service.echo");
    invocation.params = common::params(vec![(
        "message",
        common::text("web 'prod $db `.app 日本語"),
    )]);
    let validated = catalogue.validate(&invocation).expect("valid");
    let line = spot_mgmt_ops::render(&validated, spot_mgmt_ops::Shell::Posix);
    // The shell itself answers with the argv it saw: one `printf '%s\n'` per argument,
    // bracketed with sentinel lines so an empty-argument edge cannot hide.
    let script = format!(
        "printf '%s\\n' spot-argv-start {} spot-argv-end",
        &line["spot ops ".len()..]
    );
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .output()
        .expect("sh -c runs");
    assert!(output.status.success(), "sh rejected the line: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 argv");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.first(), Some(&"spot-argv-start"), "{stdout}");
    assert_eq!(lines.last(), Some(&"spot-argv-end"), "{stdout}");
    let argv: Vec<String> = lines[1..lines.len() - 1]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    // printf printed one argument per line INCLUDING `spot` and `ops`: re-attach the
    // prefix the parser demands (the shell saw the full line; the sentinel brackets the
    // whole thing).
    let mut full_argv = vec!["spot".to_owned(), "ops".to_owned()];
    full_argv.extend(argv.clone());
    let reparsed = parse_line(&catalogue, &full_argv).expect("the shell gave clean argv");
    assert_eq!(
        reparsed.params.get("message").cloned(),
        Some(ParamValue::Text("web 'prod $db `.app 日本語".to_owned())),
        "the full awkward value survived the real shell: {argv:?}"
    );
}

#[test]
fn parse_then_validate_full_pipeline_round_trip() {
    let catalogue = common::catalogue();
    let args: Vec<String> = [
        "spot", "ops", "server", "service", "restart", "--name", "nginx",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let invocation = parse_line(&catalogue, &args).expect("parses");
    let validated = catalogue.validate(&invocation).expect("valid");
    // The typed round trip: render the same line the console would show.
    assert_eq!(
        spot_mgmt_ops::render(&validated, spot_mgmt_ops::Shell::Posix),
        "spot ops server service restart --name nginx --wait 30s"
    );
    // And the parsed (not validated) invocation carries no defaults yet.
    assert!(!invocation.params.contains_key("wait"));
}

#[test]
fn an_invalid_enum_choice_parses_but_fails_validation() {
    // Parse is structural, validation judges content: a bad enum choice parses.
    let catalogue = common::catalogue();
    let args: Vec<String> = [
        "spot",
        "ops",
        "tenant",
        "backup",
        "run",
        "--passphrase",
        "x",
        "--level",
        "monthly",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    let invocation = parse_line(&catalogue, &args).expect("parses structurally");
    let errors = catalogue.validate(&invocation).expect_err("not a choice");
    assert_eq!(errors[0].code(), "ops.not_in_enum");
}

#[test]
fn the_placeholder_value_round_trips_through_parse() {
    // A rendered secret line parses back with the placeholder as the value: the parser
    // reads exactly what render writes. The real value was never on the line.
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("tenant.backup.run");
    invocation.params = common::params(vec![
        ("passphrase", common::text("correct horse battery staple")),
        ("level", common::text("full")),
    ]);
    let validated = catalogue.validate(&invocation).expect("valid");
    let line = spot_mgmt_ops::render(&validated, spot_mgmt_ops::Shell::Posix);
    // Split under the same quoting rules the line was written with: the placeholder (and
    // any other value) sits inside quotes when it needs them, so a blind space-split is
    // NOT the parse's own contract — the CLI splits with its shell.
    let args: Vec<String> = split_posix(&line);
    let reparsed: Invocation = parse_line(&catalogue, &args).expect("the placeholder parses");
    assert_eq!(
        reparsed.params.get("passphrase"),
        Some(&ParamValue::Text("<prompt>".to_owned())),
        "the parser reads the placeholder as the value: the line never carried the secret"
    );
    // `<prompt>` is legal text, so validation passes it — the CLI knows a rendered line's
    // placeholder means prompt-at-execution; the shape contract holds either way.
    let validated_again = catalogue
        .validate(&reparsed)
        .expect("placeholder validates");
    assert_eq!(validated_again.params.len(), 2);
}

/// The minimal POSIX word-splitter of a rendered line (its inverse); the tests use it
/// where a real shell would (the shell round-trip tests cover `sh -c` itself).
fn split_posix(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '\'' => in_quotes = false,
                '\\' if chars.peek() == Some(&'\'') => {
                    chars.next();
                    current.push('\'');
                }
                other => current.push(other),
            }
            continue;
        }
        match c {
            ' ' => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            '\'' => in_quotes = true,
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

#[test]
fn a_true_shell_round_trips_every_argument_of_a_line_with_newlines() {
    // The brief: a value with spaces, quotes, `$`, backticks, newlines or non-ASCII must
    // survive a real shell. Newlines split `lines()`, so bracket every argument instead.
    if cfg!(windows) {
        return;
    }
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("server.service.echo");
    invocation.params = common::params(vec![("message", common::text("line1\nline2"))]);
    let validated = catalogue.validate(&invocation).expect("valid");
    let line = spot_mgmt_ops::render(&validated, spot_mgmt_ops::Shell::Posix);
    let script = format!(
        "printf '[%s]' spot-argv-start {} spot-argv-end",
        &line["spot ops ".len()..]
    );
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .output()
        .expect("sh -c runs");
    assert!(output.status.success(), "sh rejected the line: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("utf-8");
    // Every argument came out bracketed exactly once, newlines intact inside the brackets.
    // The rendered argument arrives as its literal content (the shell removes the quotes).
    assert_eq!(
        stdout, "[spot-argv-start][server][service][echo][--message][line1\nline2][spot-argv-end]",
        "the newline survived inside its single argument"
    );
    // And the whole pipeline: the shell's argv parses back to the same value. The printf
    // brackets isolate every argument verbatim (newlines included); the two sentinels at
    // the ends drop out, everything between them is the operation line's argv, and the
    // `spot ops` prefix is prepended for the parser.
    let pieces: Vec<&str> = stdout
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split("][")
        .collect();
    let argv: Vec<String> = pieces[1..pieces.len() - 1]
        .iter()
        .map(|piece| (*piece).to_owned())
        .collect();
    let mut full_argv = vec!["spot".to_owned(), "ops".to_owned()];
    full_argv.extend(argv.clone());
    let reparsed = parse_line(&catalogue, &full_argv).expect("the shell gave clean argv");
    assert_eq!(
        reparsed.params.get("message").cloned(),
        Some(ParamValue::Text("line1\nline2".to_owned())),
        "the newline survived the round trip: {argv:?}"
    );
}

/// Locates a PowerShell executable, or `None` when the machine has none (the brief allows
/// the skip). On Windows the stock shell is `powershell.exe`; the cross-platform `pwsh`
/// is preferred wherever it exists.
fn find_powershell() -> Option<std::path::PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["powershell.exe", "pwsh.exe"]
    } else {
        &["pwsh", "powershell"]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        names
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[test]
fn a_true_power_shell_round_trips_its_arguments_when_powershell_is_present() {
    // The brief demands the same real-shell proof for PowerShell as for `sh -c`. A value
    // with a single quote (doubled inside the quoting), spaces, `$`, backticks and
    // non-ASCII must survive `powershell -Command` with its arguments intact. (Embedded
    // newlines are the POSIX test's case; every line here must stay one line.)
    let Some(powershell) = find_powershell() else {
        return; // skip: no PowerShell on this machine
    };
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("server.service.echo");
    invocation.params = common::params(vec![(
        "message",
        common::text("it's \"mixed\" $stuff `tick 日本語"),
    )]);
    let validated = catalogue.validate(&invocation).expect("valid");
    let line = spot_mgmt_ops::render(&validated, spot_mgmt_ops::Shell::PowerShell);

    // A param-less stub named exactly like the first token of the rendered line: with no
    // parameter block there are no parameter names to match, so every token — dash-prefixed
    // flags included — lands verbatim in `$args` and is echoed one bracket per line.
    let script = format!(
        "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
         function spot {{ foreach ($a in $args) {{ Write-Output (\"[\" + $a + \"]\") }} }}; {}",
        line
    );
    let output = std::process::Command::new(&powershell)
        .arg("-NoProfile")
        .arg("-Command")
        .arg(&script)
        .output()
        .expect("powershell runs");
    assert!(
        output.status.success(),
        "powershell rejected the line: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8");

    // One bracketed token per output line; the value must arrive exactly as rendered.
    let pieces: Vec<String> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            assert!(
                line.starts_with('[') && line.ends_with(']'),
                "every token arrives bracketed, got {line:?}"
            );
            line[1..line.len() - 1].to_owned()
        })
        .collect();
    // The stub `spot` was the command itself, so `$args` starts at `ops`.
    assert_eq!(
        pieces,
        vec![
            "ops".to_owned(),
            "server".to_owned(),
            "service".to_owned(),
            "echo".to_owned(),
            "--message".to_owned(),
            "it's \"mixed\" $stuff `tick 日本語".to_owned(),
        ],
        "every token survived PowerShell's parser verbatim"
    );

    // And the whole pipeline: PowerShell's argv parses back to the same typed value.
    let mut full_argv = vec!["spot".to_owned(), "ops".to_owned()];
    full_argv.extend(pieces.into_iter().skip(1)); // drop the `ops` the stub swallowed
    let reparsed = parse_line(&catalogue, &full_argv).expect("the shell gave clean argv");
    assert_eq!(
        reparsed.params.get("message").cloned(),
        Some(ParamValue::Text(
            "it's \"mixed\" $stuff `tick 日本語".to_owned()
        )),
        "the value survived the PowerShell round trip"
    );
}

#[test]
fn validation_errors_carry_params_so_errors_name_their_param() {
    let catalogue = common::catalogue();
    let mut invocation = common::invocation("server.service.restart");
    invocation.params = common::params(vec![("wrong", common::text("x"))]);
    let errors = catalogue.validate(&invocation).expect_err("unknown");
    assert_eq!(errors[0].param(), Some("wrong"));
}
