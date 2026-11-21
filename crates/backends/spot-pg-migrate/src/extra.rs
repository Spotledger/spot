//! The extra-table hook: the one supported way for a table that is not a document to be created
//! by the migrator (WP-STO-12.T3).
//!
//! Some platform tables are not documents: the edge tables of `spot-edge-pg`, the audit chain
//! tables of `spot-audit-store`, `spot_mail_sent_log`. The owning crate hands the host its DDL
//! as an [`ExtraTable`] in [`crate::PlanOptions::extra_tables`], and the planner turns each one
//! that is not yet in the database into one step (`extra_table:<table>`, [`crate::Risk::Additive`],
//! one transaction), planned, reported, locked and applied like every other step.
//!
//! # Rules (fail closed)
//!
//! * Every statement is one `CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS` or
//!   `CREATE UNIQUE INDEX IF NOT EXISTS`. Nothing else is accepted: no `DROP`, `ALTER`, `GRANT`,
//!   `TRUNCATE`, `INSERT`, `UPDATE`, `DELETE`, no `CREATE INDEX CONCURRENTLY`, no second
//!   statement after a `;`, no comments, no dollar quoting, no backslash.
//! * The table name, every object a statement creates, and the table an index is built on all
//!   start with `spot_` (the rule of `tools/lints/table-prefix`, see `names.rs`) and are plain
//!   lower case identifiers, not schema qualified.
//! * `table` is the name that marks the entry as done: it is a table, or for an entry that only
//!   adds an index to a table an earlier entry made, that index. One statement must create the
//!   object it names, so after an apply the entry plans nothing.
//! * No name may be the name of a document table of the catalogue, and two entries may not share
//!   a `table`.
//!
//! A breach is a refusal with code `migrate.extra_table_invalid`; the plan is refused as a whole
//! and nothing is applied. The message names the table, never the caller's SQL.
//!
//! # Order
//!
//! The extra steps come after every document step (tables, columns, foreign keys, indexes,
//! exclusion constraints) and before the `finalise` step, and among themselves in the order
//! given: the caller orders them by dependency, a table before the entry that indexes it.

use std::collections::BTreeSet;

use crate::error::{Refusal, codes};
use crate::names::{OWNED_PREFIX, is_owned};

/// The key prefix of an extra table step: `extra_table:<table>`.
pub(crate) const STEP_PREFIX: &str = "extra_table:";

/// The longest identifier PostgreSQL keeps.
const MAX_IDENT: usize = 63;

/// One non-document table (or one index of such a table) a crate contributes to the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtraTable {
    /// The name that marks this entry done: a table, or the index when the entry only indexes
    /// a table an earlier entry made. Starts with `spot_`.
    pub table: String,
    /// The idempotent statements, run in this order in one transaction.
    pub create: Vec<String>,
}

impl ExtraTable {
    /// An entry, validated by the rules in the module docs.
    ///
    /// # Errors
    ///
    /// The refusal ([`codes::EXTRA_TABLE_INVALID`]) when a rule is broken. The planner validates
    /// again, because the fields are public.
    pub fn new(table: impl Into<String>, create: Vec<String>) -> Result<Self, Refusal> {
        let extra = Self {
            table: table.into(),
            create,
        };
        match extra.violation() {
            Some(reason) => Err(extra.refusal(reason)),
            None => Ok(extra),
        }
    }

    /// The refusal for this entry with `reason`.
    pub(crate) fn refusal(&self, reason: impl Into<String>) -> Refusal {
        Refusal {
            entity: self.table.clone(),
            field: None,
            code: codes::EXTRA_TABLE_INVALID,
            reason: format!(
                "extra table `{}` is not allowed: {}",
                self.table,
                reason.into()
            ),
        }
    }

    /// The first rule this entry breaks, ignoring the catalogue and the other entries.
    pub(crate) fn violation(&self) -> Option<String> {
        if !plain_owned_name(&self.table) {
            return Some(format!(
                "its name must be a lower case identifier starting with `{OWNED_PREFIX}`"
            ));
        }
        if self.create.is_empty() {
            return Some("it holds no statement".to_owned());
        }
        let mut creates_table_name = false;
        for statement in &self.create {
            match parse_statement(statement) {
                Ok(parsed) => creates_table_name |= parsed.created == self.table,
                Err(why) => return Some(why.to_owned()),
            }
        }
        if !creates_table_name {
            return Some("no statement creates the object its name stands for".to_owned());
        }
        None
    }

    /// Every name this entry creates, in statement order. Only meaningful for a valid entry.
    pub(crate) fn created_names(&self) -> Vec<String> {
        self.create
            .iter()
            .filter_map(|s| parse_statement(s).ok().map(|p| p.created))
            .collect()
    }
}

/// The refusals for `extras` against the names of the catalogue's tables: every rule that
/// needs more than one entry or the catalogue.
pub(crate) fn refusals(extras: &[ExtraTable], catalogue_tables: &BTreeSet<&str>) -> Vec<Refusal> {
    let mut out = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for extra in extras {
        if let Some(reason) = extra.violation() {
            out.push(extra.refusal(reason));
            continue;
        }
        if !seen.insert(extra.table.as_str()) {
            out.push(extra.refusal("another entry has the same name"));
            continue;
        }
        let collides = extra
            .created_names()
            .iter()
            .chain(extra.referenced_tables().iter())
            .any(|name| catalogue_tables.contains(name.as_str()));
        if collides {
            out.push(extra.refusal("a name collides with a document table of the catalogue"));
        }
    }
    out
}

impl ExtraTable {
    /// The tables the indexes of this entry are built on. Only meaningful for a valid entry.
    fn referenced_tables(&self) -> Vec<String> {
        self.create
            .iter()
            .filter_map(|s| parse_statement(s).ok().and_then(|p| p.on))
            .collect()
    }
}

/// A statement that passed the shape check.
struct Parsed {
    /// The table or index the statement creates.
    created: String,
    /// The table an index is built on.
    on: Option<String>,
}

/// Whether `name` is a plain, lower case, unqualified identifier that starts with `spot_`.
fn plain_owned_name(name: &str) -> bool {
    is_owned(name)
        && name.len() <= MAX_IDENT
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// One lexical token of the head of a statement.
#[derive(Debug, PartialEq, Eq)]
enum Token {
    /// An unquoted word, lower cased.
    Word(String),
    /// A double quoted identifier, unquoted.
    Quoted(String),
    /// `(`.
    Open,
    /// Anything else (an operator or a literal), not needed by the shape check.
    Other,
}

/// Checks the whole text for what can hide a second statement or change how it is read, and
/// returns the tokens of its head (the first `limit`).
fn scan(text: &str, limit: usize) -> Result<Vec<Token>, &'static str> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\\' => return Err("a backslash is not accepted"),
            b'$' => return Err("dollar quoting is not accepted"),
            b';' => return Err("a statement must not contain a semicolon"),
            b'-' if bytes.get(i + 1) == Some(&b'-') => return Err("a comment is not accepted"),
            b'/' if bytes.get(i + 1) == Some(&b'*') => return Err("a comment is not accepted"),
            b'\'' => {
                // A string literal; '' is a quote inside it.
                i += 1;
                loop {
                    match bytes.get(i) {
                        None => return Err("a string literal is not closed"),
                        Some(b'\'') if bytes.get(i + 1) == Some(&b'\'') => i += 2,
                        Some(b'\'') => break,
                        Some(_) => i += 1,
                    }
                }
                if tokens.len() < limit {
                    tokens.push(Token::Other);
                }
            }
            b'"' => {
                i += 1;
                let mut name = Vec::new();
                loop {
                    match bytes.get(i) {
                        None => return Err("a quoted identifier is not closed"),
                        Some(b'"') if bytes.get(i + 1) == Some(&b'"') => {
                            name.push(b'"');
                            i += 2;
                        }
                        Some(b'"') => break,
                        Some(c) => {
                            name.push(*c);
                            i += 1;
                        }
                    }
                }
                if tokens.len() < limit {
                    tokens.push(Token::Quoted(String::from_utf8_lossy(&name).into_owned()));
                }
            }
            b'(' => {
                if tokens.len() < limit {
                    tokens.push(Token::Open);
                }
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while bytes
                    .get(i + 1)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
                {
                    i += 1;
                }
                if tokens.len() < limit {
                    let word = String::from_utf8_lossy(&bytes[start..=i]).to_ascii_lowercase();
                    tokens.push(Token::Word(word));
                }
            }
            c if c.is_ascii_whitespace() => {}
            _ => {
                if tokens.len() < limit {
                    tokens.push(Token::Other);
                }
            }
        }
        i += 1;
    }
    Ok(tokens)
}

fn is_word(token: Option<&Token>, expected: &str) -> bool {
    matches!(token, Some(Token::Word(w)) if w == expected)
}

/// The name a token stands for, when it is an identifier.
fn name_of(token: Option<&Token>) -> Option<String> {
    match token {
        Some(Token::Word(w)) => Some(w.clone()),
        Some(Token::Quoted(q)) => Some(q.clone()),
        _ => None,
    }
}

/// Parses one statement: `CREATE TABLE IF NOT EXISTS <name> (` or
/// `CREATE [UNIQUE] INDEX IF NOT EXISTS <name> ON <table>`.
fn parse_statement(statement: &str) -> Result<Parsed, &'static str> {
    const NOT_ALLOWED: &str = "only the statement kinds table-create, index-create and unique-index-create, each with IF NOT EXISTS, are accepted";
    const NAME: &str = "a created object must be named by a lower case identifier starting with `spot_`, not schema qualified";
    let trimmed = statement.trim();
    if trimmed.is_empty() {
        return Err("a statement is empty");
    }
    let tokens = scan(trimmed, 12)?;
    let mut at = 0;
    if !is_word(tokens.get(at), "create") {
        return Err(NOT_ALLOWED);
    }
    at += 1;
    let table = match tokens.get(at) {
        Some(Token::Word(w)) if w == "table" => {
            at += 1;
            true
        }
        Some(Token::Word(w)) if w == "unique" || w == "index" => {
            if w == "unique" {
                at += 1;
            }
            if !is_word(tokens.get(at), "index") {
                return Err(NOT_ALLOWED);
            }
            at += 1;
            false
        }
        _ => return Err(NOT_ALLOWED),
    };
    if !(is_word(tokens.get(at), "if")
        && is_word(tokens.get(at + 1), "not")
        && is_word(tokens.get(at + 2), "exists"))
    {
        return Err(NOT_ALLOWED);
    }
    at += 3;
    let created = name_of(tokens.get(at)).ok_or(NAME)?;
    if !plain_owned_name(&created) {
        return Err(NAME);
    }
    at += 1;
    if table {
        if tokens.get(at) != Some(&Token::Open) {
            return Err("a table-create statement needs a column list in parentheses");
        }
        return Ok(Parsed { created, on: None });
    }
    if !is_word(tokens.get(at), "on") {
        return Err("an index must be built with ON <table>");
    }
    at += 1;
    if is_word(tokens.get(at), "only") {
        at += 1;
    }
    let on = name_of(tokens.get(at)).ok_or(NAME)?;
    if !plain_owned_name(&on) {
        return Err("an index may only be built on a table starting with `spot_`");
    }
    Ok(Parsed {
        created,
        on: Some(on),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ok(table: &str, sql: &[&str]) -> ExtraTable {
        ExtraTable::new(table, sql.iter().map(|s| (*s).to_owned()).collect()).unwrap()
    }

    fn bad(table: &str, sql: &[&str]) -> String {
        ExtraTable::new(table, sql.iter().map(|s| (*s).to_owned()).collect())
            .unwrap_err()
            .reason
    }

    #[test]
    fn a_table_and_its_index_are_accepted() {
        let t = ok(
            "spot_x",
            &[
                "CREATE TABLE IF NOT EXISTS spot_x (id bigint PRIMARY KEY, note text)",
                "create unique index if not exists \"spot_x_note\" on spot_x (note)",
            ],
        );
        assert_eq!(t.created_names(), ["spot_x", "spot_x_note"]);
        // An entry that only indexes an earlier table.
        ok(
            "spot_x_note2",
            &["CREATE INDEX IF NOT EXISTS spot_x_note2 ON spot_x (note)"],
        );
    }

    #[test]
    fn the_name_rules_are_enforced() {
        assert!(bad("x", &["CREATE TABLE IF NOT EXISTS x (a int)"]).contains("spot_"));
        assert!(bad("spot_X", &["CREATE TABLE IF NOT EXISTS spot_X (a int)"]).contains("spot_"));
        assert!(bad("spot_a", &["CREATE TABLE IF NOT EXISTS other (a int)"]).contains("spot_"));
        assert!(
            bad(
                "spot_a",
                &["CREATE TABLE IF NOT EXISTS public.spot_a (a int)"]
            )
            .contains("spot_")
        );
        // The statement creates a different object than the entry names.
        assert!(
            bad("spot_a", &["CREATE TABLE IF NOT EXISTS spot_b (a int)"])
                .contains("no statement creates")
        );
        // An index on a document-looking table.
        assert!(
            bad(
                "spot_i",
                &["CREATE INDEX IF NOT EXISTS spot_i ON party (a)"]
            )
            .contains("spot_")
        );
        assert!(bad("spot_a", &[]).contains("no statement"));
        assert!(bad("spot_a", &["  "]).contains("empty"));
    }

    #[test]
    fn only_the_three_statement_kinds_are_accepted() {
        for sql in [
            "DROP TABLE spot_a",
            "ALTER TABLE spot_a ADD COLUMN b int",
            "GRANT ALL ON spot_a TO public",
            "TRUNCATE spot_a",
            "DELETE FROM spot_a",
            "UPDATE spot_a SET b = 1",
            "INSERT INTO spot_a VALUES (1)",
            "CREATE TABLE spot_a (a int)",
            "CREATE INDEX spot_a ON spot_a (a)",
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS spot_a ON spot_a (a)",
            "CREATE TABLE IF NOT EXISTS spot_a AS SELECT 1",
            "CREATE VIEW spot_a AS SELECT 1",
            "CREATE TEMP TABLE IF NOT EXISTS spot_a (a int)",
        ] {
            assert!(!bad("spot_a", &[sql]).is_empty(), "{sql}");
        }
    }

    #[test]
    fn chaining_and_hidden_text_are_refused() {
        for sql in [
            "CREATE TABLE IF NOT EXISTS spot_a (a int); DROP TABLE spot_b",
            "CREATE TABLE IF NOT EXISTS spot_a (a int);",
            "CREATE TABLE IF NOT EXISTS spot_a (a int) -- ; DROP TABLE x",
            "CREATE TABLE IF NOT EXISTS spot_a (a int /* x */)",
            "CREATE TABLE IF NOT EXISTS spot_a (a text DEFAULT $$;$$)",
            "CREATE TABLE IF NOT EXISTS spot_a (a text DEFAULT E'\\'')",
        ] {
            assert!(!bad("spot_a", &[sql]).is_empty(), "{sql}");
        }
        // A semicolon inside a string literal or quoted identifier is not chaining.
        ok(
            "spot_a",
            &["CREATE TABLE IF NOT EXISTS spot_a (a text DEFAULT 'x;y', \"b;c\" int)"],
        );
    }

    #[test]
    fn the_catalogue_and_duplicates_are_checked() {
        let a = ok("spot_a", &["CREATE TABLE IF NOT EXISTS spot_a (a int)"]);
        let docs: BTreeSet<&str> = ["spot_a", "party"].into_iter().collect();
        let r = refusals(std::slice::from_ref(&a), &docs);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].code, codes::EXTRA_TABLE_INVALID);
        assert!(r[0].reason.contains("collides"), "{}", r[0].reason);

        let none: BTreeSet<&str> = BTreeSet::new();
        assert!(refusals(std::slice::from_ref(&a), &none).is_empty());
        let r = refusals(&[a.clone(), a], &none);
        assert_eq!(r.len(), 1);
        assert!(r[0].reason.contains("same name"));

        // Built by hand, bypassing the constructor: still refused.
        let hand = ExtraTable {
            table: "spot_h".into(),
            create: vec!["DROP TABLE spot_h".into()],
        };
        assert_eq!(refusals(&[hand], &none).len(), 1);
    }

    #[test]
    fn a_refusal_never_carries_the_callers_sql() {
        let r = ExtraTable::new("spot_a", vec!["DROP TABLE secret_value_123".into()]).unwrap_err();
        assert!(!r.reason.contains("secret_value_123"), "{}", r.reason);
        assert!(r.reason.contains("spot_a"));
    }

    // --- planner level -----------------------------------------------------------------

    use crate::live::LiveSchema;
    use crate::plan::{PlanOptions, Risk, Run, plan_with};
    use spot_storage_contract::{
        Catalog, Collation, EntityDef, EntityKind, FieldDef, FieldType, Name,
    };

    fn party() -> Catalog {
        let entity = EntityDef {
            name: Name::new("party").unwrap(),
            kind: EntityKind::Document,
            fields: vec![FieldDef {
                name: Name::new("title").unwrap(),
                ty: FieldType::Text { max_chars: None },
                nullable: false,
            }],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        };
        Catalog::new(1, Collation::new("en").unwrap(), vec![entity]).unwrap()
    }

    fn with(extras: Vec<ExtraTable>) -> PlanOptions {
        PlanOptions {
            extra_tables: extras,
            ..PlanOptions::default()
        }
    }

    fn table_a() -> ExtraTable {
        ok(
            "spot_a",
            &["CREATE TABLE IF NOT EXISTS spot_a (id bigint PRIMARY KEY)"],
        )
    }

    fn index_of_a() -> ExtraTable {
        ok(
            "spot_a_id_ix",
            &["CREATE INDEX IF NOT EXISTS spot_a_id_ix ON spot_a (id)"],
        )
    }

    #[test]
    fn extras_come_after_document_steps_and_before_finalise_in_the_order_given() {
        let options = with(vec![table_a(), index_of_a()]);
        let plan = plan_with(&LiveSchema::default(), &party(), &options);
        assert!(plan.refused.is_empty(), "{:?}", plan.refused);
        let keys: Vec<&str> = plan.steps.iter().map(|s| s.key.as_str()).collect();
        let n = keys.len();
        assert_eq!(keys[n - 1], "finalise");
        assert_eq!(keys[n - 3], "extra_table:spot_a");
        assert_eq!(keys[n - 2], "extra_table:spot_a_id_ix");
        assert!(keys[..n - 3].iter().all(|k| !k.starts_with("extra_table:")));
        assert!(keys[0].starts_with("create_table:party"), "{keys:?}");
        assert!(matches!(&plan.steps[n - 3].run, Run::Tx(sql) if sql.len() == 1));
        assert_eq!(plan.steps[n - 3].risk, Risk::Additive);
    }

    #[test]
    fn a_marker_already_in_the_database_plans_nothing() {
        let mut live = LiveSchema::default();
        live.tables.insert(
            "spot_a".into(),
            crate::live::LiveTable {
                columns: Default::default(),
                has_rows: false,
            },
        );
        let options = with(vec![table_a()]);
        let plan = plan_with(
            &live,
            &Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap(),
            &options,
        );
        assert!(plan.is_empty(), "{:?}", plan.steps);
    }

    #[test]
    fn the_hash_is_stable_and_follows_the_statements_and_their_order() {
        let h = |o: &PlanOptions| plan_with(&LiveSchema::default(), &party(), o).plan_hash;
        let base = with(vec![table_a(), index_of_a()]);
        assert_eq!(h(&base), h(&base.clone()));
        assert_ne!(h(&base), h(&PlanOptions::default()));
        let changed = with(vec![
            ok(
                "spot_a",
                &["CREATE TABLE IF NOT EXISTS spot_a (id bigint PRIMARY KEY, n int)"],
            ),
            index_of_a(),
        ]);
        assert_ne!(h(&base), h(&changed));
        let reordered = with(vec![index_of_a(), table_a()]);
        assert_ne!(h(&base), h(&reordered));
    }

    #[test]
    fn a_collision_with_a_document_table_refuses_the_whole_plan() {
        let mut entity = party().entities().next().unwrap().clone();
        entity.name = Name::new("spot_a").unwrap();
        let catalog = Catalog::new(1, Collation::new("en").unwrap(), vec![entity]).unwrap();
        let plan = plan_with(&LiveSchema::default(), &catalog, &with(vec![table_a()]));
        assert_eq!(plan.refused.len(), 1);
        assert_eq!(plan.refused[0].code, codes::EXTRA_TABLE_INVALID);
    }
}
