//! The case files: what a case says, and how they are loaded and checked.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

use crate::dsl::QuerySpec;

/// What a case does after its setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Runs `query` inside a transaction and outside one; both must agree.
    Query,
    /// Loads document `doc` and returns one row of `fields`.
    Load,
    /// Loads document `doc` and returns one row per child row, in stored order, of `fields`.
    Children,
    /// Runs the command in `write`; the outcome is `ok` or the error kind.
    Write,
    /// Prepares the catalog and nothing else: the outcome is `ok` or the error kind. This is how
    /// a case asserts what `prepare` does with a construct a backend does not serve (ADR-087
    /// sections 4.7, 5.7; SEM-GEO-04, SEM-RNG-08), which is a capability answer, not a query.
    Prepare,
    /// Inserts the documents named in `write` and deletes them again in **one** transaction,
    /// then reads the change feed from its start. Returns one row per feed entry as
    /// `[op, entity]`. This is how SEM-HLD-03 is proven: a document created and deleted in one
    /// transaction leaves no entry (ADR-053 change feed).
    Changes,
    /// Runs the graph traversal of `traversal`; the outcome is the reached nodes as
    /// `entity:name@depth<predecessor:edge` lines, or the error kind. With `write` lines, those
    /// documents are inserted in the same transaction first and the traversal must see them.
    Traverse,
}

/// Which backends a case is run on. A construct the fake does not model is tested on PostgreSQL
/// only (ADR-087 option B, sections 4.5, 5.5, 7.9), and the fake's `Unsupported` answer is
/// asserted by a case scoped to the fake; the runner skips a case on a backend outside its
/// scope and never counts that as a pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scope {
    /// Every backend that is available (the default).
    #[default]
    Both,
    /// PostgreSQL only: the construct is not modelled by the in-memory fake.
    Postgres,
    /// The in-memory fake only: the case asserts what the fake answers for a construct it does
    /// not serve.
    Fake,
}

impl Scope {
    /// Parses the `scope` key of a case.
    pub fn parse(s: &str) -> Result<Scope, String> {
        match s {
            "both" => Ok(Scope::Both),
            "postgres" => Ok(Scope::Postgres),
            "fake" => Ok(Scope::Fake),
            other => Err(format!(
                "scope must be `both`, `postgres` or `fake`, not `{other}`"
            )),
        }
    }

    /// Whether a case in this scope runs on the backend called `backend`.
    pub fn covers(self, backend: &str) -> bool {
        match self {
            Scope::Both => true,
            Scope::Postgres => backend == "postgres",
            Scope::Fake => backend == "fake",
        }
    }

    /// The scope as it is written in a case file.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Both => "both",
            Scope::Postgres => "postgres",
            Scope::Fake => "fake",
        }
    }
}

/// What a case expects.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Exactly these rows (tagged value strings).
    pub rows: Option<Vec<Vec<String>>>,
    /// This error kind, without the `storage.` prefix (`invalid_value`).
    pub error: Option<String>,
    /// A write that succeeds.
    #[serde(default)]
    pub ok: bool,
    /// No fixed answer: every backend must give what the reference (PostgreSQL) gives.
    #[serde(default)]
    pub same_as_reference: bool,
    /// The reached nodes of a traversal, in order, as `entity:name@depth` for a start node and
    /// `entity:name@depth<predecessor:edge` otherwise.
    pub nodes: Option<Vec<String>>,
    /// Compare rows as a set (for results without a defined order).
    #[serde(default)]
    pub unordered: bool,
}

/// One case.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Unique id.
    pub id: String,
    /// The rule it proves (an id from `docs/storage-semantics.md`).
    pub rule: String,
    /// One line on what is checked.
    pub title: String,
    /// Which backends run it: `both` (default), `postgres` or `fake`. A case about a construct
    /// the fake does not model is `postgres`; a case asserting the fake's `Unsupported` is
    /// `fake` (ADR-087, option B).
    pub scope: Option<String>,
    /// Extensions the case's scratch database must have created before `prepare` runs
    /// (`postgis`, `btree_gist`, `pg_trgm`, `unaccent`: the closed ADR-087 list). Stands in for
    /// the audited `extension.install` operation, which is WP-STO-12.T2. A case that asserts
    /// what happens **without** an extension leaves this empty.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// The case needs the search projection: the PostgreSQL scratch database gets `pg_trgm` and
    /// `unaccent`, and the backend is connected with the search hook installed before `prepare`
    /// (ADR-087 section 7, SEM-SCH-04). Default false: a case that asserts what happens
    /// **without** the hook leaves it out. Only a case scoped to `postgres` may ask, because the
    /// fake has no inverted index.
    #[serde(default)]
    pub needs_search: bool,
    /// Fields of `item` the harness puts in a protection group when it installs the search hook
    /// (what the permission layer does for a protected field). Needs `needs_search`. A protected
    /// field that is also declared searchable is refused by `prepare` (SEM-SCH-02).
    #[serde(default)]
    pub search_protected: Vec<String>,
    /// The tenant collation; default `en`.
    pub collation: Option<String>,
    /// Commands run before the operation; each must succeed.
    #[serde(default)]
    pub setup: Vec<String>,
    /// Write limit of the setup and write transactions.
    pub max_writes: Option<u32>,
    /// The operation.
    pub op: Op,
    /// The query, for `op = "query"`.
    #[serde(default)]
    pub query: QuerySpec,
    /// The document, for `load` and `children`.
    pub doc: Option<String>,
    /// The fields to return, for `load` and `children`.
    #[serde(default)]
    pub fields: Vec<String>,
    /// The commands, for `op = "write"`; run in order after the setup, the first failure is the
    /// outcome.
    #[serde(default)]
    pub write: Vec<String>,
    /// Commands run after the setup that **must fail**: the write is rolled back and the failure
    /// is not the verdict, the operation that follows is (a rolled-back write leaves nothing,
    /// SEM-SCH-07). A command that succeeds makes the case broken.
    #[serde(default)]
    pub try_write: Vec<String>,
    /// The traversal, for `op = "traverse"`.
    pub traversal: Option<crate::dsl::TraversalSpec>,
    /// The catalogue: `main` (default), `geo`, `range`, `search`, `search_rank` or `long`
    /// (63-character names).
    pub catalog: Option<String>,
    /// The expected outcome.
    pub expect: Expect,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseFile {
    #[serde(default)]
    case: Vec<Case>,
}

/// One accepted difference between the backends.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Divergence {
    /// The rule whose cases differ.
    pub rule: String,
    /// The one case that differs. Without it the entry covers every case of the rule, which is
    /// only right when the whole rule is in question.
    pub case: Option<String>,
    /// Why the difference is accepted, and what would end it.
    pub reason: String,
    /// Who decides or fixes it.
    pub owner: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterFile {
    #[serde(default)]
    divergence: Vec<Divergence>,
}

/// The file in `cases/` that holds the divergence register.
pub const REGISTER_FILE: &str = "divergences.toml";

/// The extensions a case may ask its scratch database to have (ADR-087 section 4.3: the closed,
/// platform-owned list). Nothing else can be installed. Mirrors `ALLOWED_EXTENSIONS` of
/// `spot-pg-migrate`'s audited `extension.install` (WP-STO-12.T2), which is the real owner of the
/// check; the harness stands in for that operation.
pub const EXTENSIONS: [&str; 4] = ["postgis", "btree_gist", "pg_trgm", "unaccent"];

/// The code the audited `extension.install` operation returns for a name outside
/// [`EXTENSIONS`] (`spot-pg-migrate`'s `codes::EXTENSION_NOT_ALLOWED`, ADR-087 section 4.3). A
/// case asserts it by naming an off-list extension and expecting this code (SEM-EXT-02).
pub const EXTENSION_NOT_ALLOWED: &str = "migrate.extension_not_allowed";

/// The first extension of `case` that is not on the allow-list, if any: the request the audited
/// `extension.install` refuses before it reaches the database.
pub fn off_list_extension(c: &Case) -> Option<&str> {
    c.extensions
        .iter()
        .map(String::as_str)
        .find(|e| !EXTENSIONS.contains(e))
}

impl Case {
    /// The scope the case runs in (ADR-087 option B).
    pub fn scope(&self) -> Result<Scope, String> {
        match self.scope.as_deref() {
            None => Ok(Scope::Both),
            Some(s) => Scope::parse(s).map_err(|m| format!("case `{}`: {m}", self.id)),
        }
    }
}

/// Parses the text of a case file.
pub fn parse_cases(text: &str) -> Result<Vec<Case>, String> {
    toml::from_str::<CaseFile>(text)
        .map(|f| f.case)
        .map_err(|e| e.to_string())
}

/// Parses the text of the divergence register.
pub fn parse_register(text: &str) -> Result<Vec<Divergence>, String> {
    toml::from_str::<RegisterFile>(text)
        .map(|f| f.divergence)
        .map_err(|e| e.to_string())
}

/// Loads every `*.toml` file in `dir` except the register, in file name order, and checks
/// that case ids are unique and each case is well formed.
pub fn load_cases(dir: &Path) -> Result<Vec<Case>, String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .filter(|p| p.file_name().is_some_and(|n| n != REGISTER_FILE))
        .collect();
    files.sort();
    let mut cases = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        let parsed = parse_cases(&text).map_err(|e| format!("{}: {e}", f.display()))?;
        cases.extend(parsed);
    }
    let mut seen = BTreeSet::new();
    for c in &cases {
        if !seen.insert(c.id.clone()) {
            return Err(format!("case id `{}` is used twice", c.id));
        }
        check_shape(c)?;
    }
    Ok(cases)
}

/// Loads the divergence register in `dir`; a missing file is an empty register.
pub fn load_register(dir: &Path) -> Result<Vec<Divergence>, String> {
    let path = dir.join(REGISTER_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_register(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Checks that a case has what its operation needs and exactly one kind of expectation.
pub fn check_shape(c: &Case) -> Result<(), String> {
    let kinds = [
        c.expect.rows.is_some(),
        c.expect.nodes.is_some(),
        c.expect.error.is_some(),
        c.expect.ok,
        c.expect.same_as_reference,
    ];
    if kinds.iter().filter(|k| **k).count() != 1 {
        return Err(format!(
            "case `{}` needs exactly one of expect.rows, expect.nodes, expect.error, expect.ok, expect.same_as_reference",
            c.id
        ));
    }
    let scope = c.scope()?;
    if c.needs_search {
        if scope != Scope::Postgres {
            return Err(format!(
                "case `{}` needs the search projection, so it must be scoped to `postgres`: the \
                 fake has no inverted index (SEM-SCH-04)",
                c.id
            ));
        }
        if !matches!(c.catalog.as_deref(), Some("search" | "search_rank")) {
            return Err(format!(
                "case `{}` needs the search projection, so its catalog must be `search` or \
                 `search_rank`",
                c.id
            ));
        }
    } else if !c.search_protected.is_empty() {
        return Err(format!(
            "case `{}` lists search_protected fields but does not set needs_search",
            c.id
        ));
    }
    for e in &c.extensions {
        if !EXTENSIONS.contains(&e.as_str())
            && c.expect.error.as_deref() != Some(EXTENSION_NOT_ALLOWED)
        {
            return Err(format!(
                "case `{}` names the extension `{e}`, which is not on the platform allow-list \
                 ({}); only a case expecting `{EXTENSION_NOT_ALLOWED}` may name one (SEM-EXT-02)",
                c.id,
                EXTENSIONS.join(", ")
            ));
        }
    }
    let ok = match c.op {
        Op::Query => !c.query.select.is_empty(),
        Op::Load | Op::Children => c.doc.is_some() && !c.fields.is_empty(),
        Op::Write => !c.write.is_empty(),
        // A prepare case writes nothing; a changes case names the documents it creates and
        // deletes in one transaction.
        Op::Prepare => true,
        Op::Changes => !c.write.is_empty(),
        Op::Traverse => c.traversal.is_some(),
    };
    if !ok {
        return Err(format!("case `{}` is missing what its `op` needs", c.id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASE: &str = r#"
[[case]]
id = "x"
rule = "SEM-SCH-01"
title = "t"
scope = "postgres"
catalog = "search"
needs_search = true
search_protected = ["code"]
try_write = ['b code="t:b"']
op = "query"
query = { select = ["code"] }
expect = { rows = [] }
"#;

    #[test]
    fn needs_search_and_try_write_parse_and_default_to_off() -> Result<(), String> {
        let cases = parse_cases(CASE)?;
        assert!(cases[0].needs_search);
        assert_eq!(cases[0].search_protected, ["code"]);
        assert_eq!(cases[0].try_write.len(), 1);
        check_shape(&cases[0])?;
        let plain = parse_cases(
            "[[case]]\nid='y'\nrule='SEM-SCH-01'\ntitle='t'\nop='query'\nquery={select=['code']}\nexpect={rows=[]}\n",
        )?;
        assert!(
            !plain[0].needs_search,
            "a case that does not ask has no hook"
        );
        assert!(plain[0].try_write.is_empty());
        Ok(())
    }

    #[test]
    fn needs_search_needs_postgres_scope_and_a_search_catalogue() -> Result<(), String> {
        let both = parse_cases(&CASE.replace("scope = \"postgres\"\n", ""))?;
        assert!(check_shape(&both[0]).is_err());
        let main = parse_cases(&CASE.replace("catalog = \"search\"\n", ""))?;
        assert!(check_shape(&main[0]).is_err());
        let orphan = parse_cases(&CASE.replace("needs_search = true\n", ""))?;
        assert!(check_shape(&orphan[0]).is_err());
        Ok(())
    }
}
