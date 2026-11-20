//! `prepare_migration_tables` (WP-BPM-09.T8): idempotent, safe from several tasks at once, the
//! created columns pinned, and a drift test that fails when SQL in the host (or in this crate)
//! names a column of `spot_bpm_migration` or `spot_bpm_migration_audit` that the DDL lacks.
//!
//! The database tests need a server and skip without `SPOT_TEST_PG_URL` (failing under
//! `SPOT_REQUIRE_PG=1`, through `TestDb`). The parser tests need none.
//!
//! # What the drift parser understands, and what it does not
//!
//! It reads the Rust source text, extracts every string literal that starts with an SQL keyword
//! (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `WITH`, `CREATE`) and names one of the two tables as a
//! whole word, tokenises it, and collects the column names it can see: the column list of an
//! `INSERT`, the select list, the `RETURNING` list, the `ON CONFLICT (..)` list, `ORDER BY` and
//! `GROUP BY` names, and every name directly before a comparison (`=`, `<`, `>`, `IS`, `IN`),
//! which covers `SET a = ..` and `WHERE a = ..`. It does **not** understand names that appear only
//! on the right of an expression (`SET failed = failed + 1` checks the left `failed` only), SQL
//! built with `format!` from pieces, aliases of tables, or a statement kind other than the five
//! above (such a literal fails the test closed, showing its first 60 characters). `CREATE`
//! statements are skipped: they are the DDL itself.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::pool;
use spot_bpm_pg::{PgCaseStore, prepare_migration_tables};
use spot_postgres::testing::TestDb;

const HOST_SQL: &str = include_str!("../../../framework/spot-cli/src/services/process_versions.rs");
const OWN_SQL: &str = include_str!("../src/sql.rs");

const MIGRATION: &str = "spot_bpm_migration";
const AUDIT: &str = "spot_bpm_migration_audit";

/// The columns the DDL must create, in order: `(name, information_schema data type)`.
const EXPECTED_MIGRATION: &[(&str, &str)] = &[
    ("tenant", "text"),
    ("id", "text"),
    ("process", "text"),
    ("from_version", "integer"),
    ("to_version", "integer"),
    ("mapping", "jsonb"),
    ("state", "text"),
    ("cursor", "text"),
    ("moved", "bigint"),
    ("skipped", "bigint"),
    ("failed", "bigint"),
    ("started_by", "text"),
    ("idem_key", "text"),
    ("started_at", "timestamp with time zone"),
    ("ended_at", "timestamp with time zone"),
    ("lease_holder", "text"),
    ("lease_until", "timestamp with time zone"),
];

const EXPECTED_AUDIT: &[(&str, &str)] = &[
    ("tenant", "text"),
    ("migration_id", "text"),
    ("case_id", "text"),
    ("plan_digest", "text"),
    ("payload", "text"),
    ("at", "timestamp with time zone"),
];

async fn scratch(app: &str) -> Option<(TestDb, deadpool_postgres::Pool)> {
    let db = TestDb::create().await?;
    let pool = pool(&db, 10, app);
    PgCaseStore::new(pool.clone()).prepare().await.unwrap();
    Some((db, pool))
}

async fn columns(db: &TestDb, table: &str) -> Vec<(String, String)> {
    db.raw_client()
        .await
        .query(
            "SELECT column_name::text, data_type::text FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 \
             ORDER BY ordinal_position",
            &[&table],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preparing_twice_and_from_eight_tasks_at_once_is_harmless() {
    let Some((_db, pool)) = scratch("bpm-mig-schema-twice").await else {
        return;
    };
    prepare_migration_tables(&pool).await.unwrap();
    prepare_migration_tables(&pool).await.unwrap();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            prepare_migration_tables(&pool).await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_created_columns_are_exactly_the_expected_list() {
    let Some((db, pool)) = scratch("bpm-mig-schema-columns").await else {
        return;
    };
    prepare_migration_tables(&pool).await.unwrap();
    assert_eq!(columns(&db, MIGRATION).await, pairs(EXPECTED_MIGRATION));
    assert_eq!(columns(&db, AUDIT).await, pairs(EXPECTED_AUDIT));
    // The idempotency key's unique index exists and is partial.
    let index: String = db
        .raw_client()
        .await
        .query_one(
            "SELECT indexdef FROM pg_indexes WHERE indexname = 'spot_bpm_migration_idem_idx'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(index.contains("UNIQUE") && index.contains("idem_key IS NOT NULL"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_case_store_prepare_does_not_create_the_migration_tables() {
    let Some((db, _pool)) = scratch("bpm-mig-schema-optin").await else {
        return;
    };
    assert!(columns(&db, MIGRATION).await.is_empty());
    assert!(columns(&db, AUDIT).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_column_the_host_and_this_crate_name_exists_in_the_created_tables() {
    let Some((db, pool)) = scratch("bpm-mig-schema-drift").await else {
        return;
    };
    prepare_migration_tables(&pool).await.unwrap();
    let mut known: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for table in [MIGRATION, AUDIT] {
        known.insert(
            table,
            columns(&db, table).await.into_iter().map(|c| c.0).collect(),
        );
    }
    let mut problems = Vec::new();
    for (origin, source) in [("process_versions.rs", HOST_SQL), ("sql.rs", OWN_SQL)] {
        for statement in statements(source) {
            match check(&statement, &known) {
                Ok(missing) => {
                    for column in missing {
                        problems.push(format!(
                            "{origin}: `{}` names the column `{column}` the DDL lacks",
                            head(&statement)
                        ));
                    }
                }
                Err(why) => problems.push(format!("{origin}: {why}")),
            }
        }
    }
    assert!(problems.is_empty(), "drift:\n{}", problems.join("\n"));
}

// ---- the parser (no database) ----

/// The first 60 characters of a statement, for messages.
fn head(statement: &str) -> String {
    statement.chars().take(60).collect()
}

/// The string literals of `source` (comment lines dropped) with Rust's line continuations
/// removed, that begin with an SQL keyword and name one of the two tables as a whole word.
fn statements(source: &str) -> Vec<String> {
    let text: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    literals(&text)
        .into_iter()
        .filter(|literal| {
            let first = literal
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            ["SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "CREATE"].contains(&first.as_str())
                && !tables_named(literal).is_empty()
        })
        .collect()
}

/// Every string literal of Rust source, unescaped (`\"`, `\\`, and `\` + newline + indentation).
fn literals(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' && chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'\'') {
            i += 3; // the character literal '"'
            continue;
        }
        if c == '\'' && chars.get(i + 1) == Some(&'\\') && chars.get(i + 2) == Some(&'"') {
            i += 4; // the character literal '\"'
            continue;
        }
        if c == 'r' && matches!(chars.get(i + 1), Some('#' | '"')) {
            let hashes = chars[i + 1..].iter().take_while(|&&h| h == '#').count();
            if chars.get(i + 1 + hashes) == Some(&'"') {
                let start = i + 2 + hashes;
                let close: Vec<char> = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let mut j = start;
                while j < chars.len() && chars[j..].iter().take(close.len()).ne(close.iter()) {
                    j += 1;
                }
                out.push(chars[start..j.min(chars.len())].iter().collect());
                i = j + close.len();
                continue;
            }
        }
        if c == '"' {
            let mut literal = String::new();
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                if chars[j] == '\\' {
                    match chars.get(j + 1) {
                        Some('\n') => {
                            j += 2;
                            while chars.get(j).is_some_and(|w| w.is_whitespace()) {
                                j += 1;
                            }
                            continue;
                        }
                        Some(&next) => {
                            literal.push(match next {
                                'n' => '\n',
                                't' => '\t',
                                other => other,
                            });
                            j += 2;
                            continue;
                        }
                        None => break,
                    }
                }
                literal.push(chars[j]);
                j += 1;
            }
            out.push(literal);
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// The migration tables a statement names as whole words.
fn tables_named(statement: &str) -> BTreeSet<&'static str> {
    let mut found = BTreeSet::new();
    for table in [MIGRATION, AUDIT] {
        let mut from = 0;
        while let Some(at) = statement[from..].find(table) {
            let start = from + at;
            let end = start + table.len();
            let before_ok = statement[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
            let after_ok = statement[end..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
            if before_ok && after_ok {
                found.insert(table);
            }
            from = end;
        }
    }
    found
}

/// SQL tokens: words (lower case), `$n`, quoted strings (as `'`), numbers and single characters.
fn tokens(statement: &str) -> Vec<String> {
    let chars: Vec<char> = statement.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(
                chars[start..i]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase(),
            );
        } else if c == '$' || c.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            out.push(chars[start..i].iter().collect());
        } else if c == '\'' {
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                i += 1;
            }
            i += 1;
            out.push("'".to_owned());
        } else {
            out.push(c.to_string());
            i += 1;
        }
    }
    out
}

const WORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "and",
    "or",
    "not",
    "null",
    "is",
    "in",
    "set",
    "values",
    "into",
    "insert",
    "update",
    "delete",
    "on",
    "conflict",
    "do",
    "nothing",
    "returning",
    "order",
    "by",
    "group",
    "limit",
    "for",
    "skip",
    "locked",
    "as",
    "asc",
    "desc",
    "nulls",
    "first",
    "last",
    "distinct",
    "with",
    "like",
    "exists",
    "case",
    "when",
    "then",
    "else",
    "end",
    "true",
    "false",
    "now",
    "count",
    "sum",
    "min",
    "max",
    "coalesce",
    "lower",
    "upper",
    "length",
];

fn is_name(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && !WORDS.contains(&token)
}

/// The token at `i`, or `""` past either end.
fn tok(t: &[String], i: usize) -> &str {
    t.get(i).map_or("", String::as_str)
}

/// The token before `i`, or `""` at the start.
fn prev(t: &[String], i: usize) -> &str {
    i.checked_sub(1).map_or("", |j| tok(t, j))
}

/// Collects the names of a comma list starting at `from` into `names`, at the list's own depth,
/// until `stop` matches a token at depth zero or an unmatched `)`. Function calls and `AS`
/// aliases are not names.
fn collect_list(
    t: &[String],
    from: usize,
    stop: &dyn Fn(&str) -> bool,
    names: &mut BTreeSet<String>,
) {
    let mut depth = 0_i32;
    for i in from..t.len() {
        let token = tok(t, i);
        match token {
            "(" => depth += 1,
            ")" => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => {
                if depth == 0 && stop(token) {
                    break;
                }
                let alias = prev(t, i) == "as";
                if is_name(token) && tok(t, i + 1) != "(" && !alias && prev(t, i) != "." {
                    names.insert(token.to_owned());
                }
            }
        }
    }
}

/// The column names one statement shows (see the module header for what is understood).
fn column_names(statement: &str) -> Result<BTreeSet<String>, String> {
    let t = tokens(statement);
    let first = tok(&t, 0);
    if !["select", "insert", "update", "delete", "with"].contains(&first) {
        return Err(format!("cannot read the statement `{}`", head(statement)));
    }
    let mut names = BTreeSet::new();
    for i in 0..t.len() {
        match tok(&t, i) {
            "insert" if tok(&t, i + 1) == "into" && tok(&t, i + 3) == "(" => {
                collect_list(&t, i + 4, &|_| false, &mut names);
            }
            "select" => collect_list(&t, i + 1, &|token| token == "from", &mut names),
            "returning" => collect_list(&t, i + 1, &|_| false, &mut names),
            "conflict" if tok(&t, i + 1) == "(" => {
                collect_list(&t, i + 2, &|_| false, &mut names);
            }
            "order" | "group" if tok(&t, i + 1) == "by" => {
                collect_list(
                    &t,
                    i + 2,
                    &|token| matches!(token, "limit" | "for" | "returning"),
                    &mut names,
                );
            }
            _ => {}
        }
        let comparison = matches!(tok(&t, i + 1), "=" | "<" | ">" | "!" | "is" | "in");
        if comparison && is_name(tok(&t, i)) && prev(&t, i) != "." {
            names.insert(tok(&t, i).to_owned());
        }
    }
    Ok(names)
}

/// The columns `statement` names that none of the tables it names has. `Err` when the statement
/// cannot be read (the test fails closed).
fn check(statement: &str, known: &BTreeMap<&str, BTreeSet<String>>) -> Result<Vec<String>, String> {
    let first = statement
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if first == "CREATE" {
        return Ok(Vec::new());
    }
    let named = tables_named(statement);
    let mut allowed = BTreeSet::new();
    for table in &named {
        allowed.extend(known.get(table).cloned().unwrap_or_default());
    }
    let missing = column_names(statement)?
        .into_iter()
        .filter(|name| !allowed.contains(name))
        .collect();
    Ok(missing)
}

fn static_known() -> BTreeMap<&'static str, BTreeSet<String>> {
    let names = |list: &[(&str, &str)]| list.iter().map(|c| c.0.to_owned()).collect();
    BTreeMap::from([
        (MIGRATION, names(EXPECTED_MIGRATION)),
        (AUDIT, names(EXPECTED_AUDIT)),
    ])
}

#[test]
fn the_extractor_finds_the_statements_in_the_host_and_in_the_crate() {
    let host = statements(HOST_SQL);
    let own = statements(OWN_SQL);
    // The host has six runtime statements on the two tables; the crate has the audit insert and
    // the moved counter (and a CREATE is never present in either once the DDL has moved).
    assert!(host.len() >= 6, "host statements: {}", host.len());
    assert!(own.len() >= 2, "crate statements: {}", own.len());
    assert!(
        host.iter()
            .any(|s| s.starts_with("INSERT INTO spot_bpm_migration ("))
    );
    assert!(own.iter().any(|s| s.contains("spot_bpm_migration_audit")));
    let known = static_known();
    for statement in host.iter().chain(own.iter()) {
        let missing = check(statement, &known).unwrap_or_else(|why| panic!("{why}"));
        assert!(missing.is_empty(), "`{}`: {missing:?}", head(statement));
    }
}

#[test]
fn the_parser_reads_inserts_updates_selects_and_subselects() {
    let known = static_known();
    let cols = |s: &str| column_names(s).unwrap();
    let set =
        |names: &[&str]| -> BTreeSet<String> { names.iter().map(|n| (*n).to_owned()).collect() };
    assert_eq!(
        cols(
            "INSERT INTO spot_bpm_migration (tenant, id) VALUES ($1, 'x') ON CONFLICT (tenant, idem_key) WHERE idem_key IS NOT NULL DO NOTHING RETURNING id"
        ),
        set(&["tenant", "id", "idem_key"])
    );
    assert_eq!(
        cols(
            "UPDATE spot_bpm_migration SET cursor = $3, skipped = skipped + $4 WHERE tenant = $1 AND lease_holder = $6"
        ),
        set(&["cursor", "skipped", "tenant", "lease_holder"])
    );
    assert_eq!(
        cols(
            "SELECT state, count(*) AS n FROM spot_bpm_migration WHERE tenant = $1 ORDER BY started_at LIMIT 1"
        ),
        set(&["state", "tenant", "started_at"])
    );
    assert_eq!(
        cols(
            "UPDATE spot_bpm_migration SET a = $1 WHERE id = (SELECT id FROM spot_bpm_migration WHERE state = 'x' FOR UPDATE SKIP LOCKED)"
        ),
        set(&["a", "id", "state"])
    );
    // A drifted column is reported.
    assert_eq!(
        check(
            "UPDATE spot_bpm_migration SET nonexistent = 1 WHERE tenant = $1",
            &known
        )
        .unwrap(),
        ["nonexistent"]
    );
    assert_eq!(
        check(
            "INSERT INTO spot_bpm_migration_audit (tenant, retries) VALUES ($1, $2)",
            &known
        )
        .unwrap(),
        ["retries"]
    );
    // A column of the other table is not accepted unless the statement names both tables.
    assert_eq!(
        check("SELECT moved FROM spot_bpm_migration_audit", &known).unwrap(),
        ["moved"]
    );
}

#[test]
fn a_statement_it_cannot_read_fails_closed_and_ddl_is_skipped() {
    let known = static_known();
    let why = check("VACUUM spot_bpm_migration", &known).unwrap_err();
    assert!(why.contains("VACUUM spot_bpm_migration"), "{why}");
    assert_eq!(
        check(
            "CREATE TABLE IF NOT EXISTS spot_bpm_migration (a text)",
            &known
        )
        .unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn literals_handle_continuations_escapes_raw_strings_and_comments() {
    let source = "// \"not a literal\nlet a = \"SELECT x, \\\n    y FROM t\"; let q = '\"'; let b = r#\"UPDATE \"t\" SET z = 1\"#;";
    let text: String = source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let found = literals(&text);
    assert_eq!(found, ["SELECT x, y FROM t", "UPDATE \"t\" SET z = 1"]);
    assert_eq!(
        tables_named("SELECT 1 FROM spot_bpm_migration_audit, spot_bpm_migration_idem_idx"),
        BTreeSet::from([AUDIT])
    );
}
