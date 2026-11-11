//! Running one case on one driver.

use std::collections::BTreeMap;
use std::fmt;

use spot_storage_contract::{
    ChangeOp, ChildRow, DocumentData, FeedPosition, Fields, Name, RecordId, Rows, StorageError,
    Value, Version,
};

use crate::cases::{Case, Op};
use crate::catalog::{LINE_FIELDS, item_fields};
use crate::driver::Driver;
use crate::dsl::{build_query, words};
use crate::values::{Ids, canon, parse_value};

/// What a backend did for a case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    /// These rows, as canonical value text.
    Rows(Vec<Vec<String>>),
    /// A write that succeeded.
    Ok,
    /// This error kind (`invalid_value`).
    Error(String),
    /// The backend is not available in this environment.
    Skipped,
    /// The case itself is wrong: its setup or a command failed. Never a backend verdict.
    BrokenCase(String),
    /// A query gave different rows inside and outside a transaction.
    Inconsistent(String),
}

impl fmt::Display for Observed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Observed::Rows(rows) => {
                if rows.is_empty() {
                    return f.write_str("no rows");
                }
                for (i, r) in rows.iter().enumerate() {
                    if i > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "[{}]", r.join(", "))?;
                }
                Ok(())
            }
            Observed::Ok => f.write_str("ok"),
            Observed::Error(k) => write!(f, "error {k}"),
            Observed::Skipped => f.write_str("skipped"),
            Observed::BrokenCase(m) => write!(f, "broken case: {m}"),
            Observed::Inconsistent(m) => write!(f, "inconsistent: {m}"),
        }
    }
}

/// The error kind of `e` without its `storage.` prefix: what a case expects.
pub fn error_kind(e: &StorageError) -> String {
    e.kind.code().trim_start_matches("storage.").to_owned()
}

struct DocState {
    id: RecordId,
    fields: Fields,
    children: Vec<ChildRow>,
    version: Option<Version>,
    /// The document was deleted in this case. Its id is kept so a later command can name it
    /// (`update h` after `delete h` must reach the backend with the right id and see its answer,
    /// SEM-HLD-02); it is never inserted again.
    deleted: bool,
}

struct Session<'a, D: Driver> {
    driver: &'a D,
    ids: &'a mut Ids,
    docs: BTreeMap<String, DocState>,
    pending: Vec<String>,
    max_writes: Option<u32>,
    child_counter: u32,
    /// The fields `item` declares in this case's catalogue, filled with null when a setup line
    /// omits them.
    declared: &'static [&'static str],
}

type CmdResult<T> = Result<T, CmdError>;

enum CmdError {
    Broken(String),
    Storage(StorageError),
}

impl From<String> for CmdError {
    fn from(s: String) -> Self {
        CmdError::Broken(s)
    }
}

impl From<&str> for CmdError {
    fn from(s: &str) -> Self {
        CmdError::Broken(s.to_owned())
    }
}

impl From<StorageError> for CmdError {
    fn from(e: StorageError) -> Self {
        CmdError::Storage(e)
    }
}

fn name(s: &str) -> Result<Name, String> {
    Name::new(s).map_err(|e| e.to_string())
}

impl<D: Driver> Session<'_, D> {
    fn assign(
        &mut self,
        fields: &mut Fields,
        declared: &[&str],
        pairs: &[(String, String)],
        strict: bool,
    ) -> Result<(), String> {
        for (k, v) in pairs {
            if !declared.contains(&k.as_str()) {
                return Err(format!("`{k}` is not a declared field"));
            }
            fields.insert(name(k)?, parse_value(v, self.ids)?);
        }
        if !strict {
            for d in declared {
                fields.entry(name(d)?).or_insert(Value::Null);
            }
        }
        Ok(())
    }

    fn data(&self, doc: &str) -> Result<DocumentData, String> {
        let st = self
            .docs
            .get(doc)
            .ok_or_else(|| format!("unknown document `{doc}`"))?;
        let mut children = BTreeMap::new();
        children.insert(name("lines")?, st.children.clone());
        Ok(DocumentData {
            entity: name("item")?,
            id: st.id,
            fields: st.fields.clone(),
            children,
        })
    }

    async fn flush(&mut self) -> CmdResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let names = std::mem::take(&mut self.pending);
        let docs = names
            .iter()
            // A document deleted after it was inserted in this case is not written again.
            .filter(|n| !self.docs.get(*n).is_some_and(|s| s.deleted))
            .map(|n| self.data(n))
            .collect::<Result<Vec<_>, _>>()?;
        self.driver.insert(&docs, self.max_writes).await?;
        for n in &names {
            if let Some(st) = self.docs.get_mut(n) {
                st.version = Some(Version::FIRST);
            }
        }
        Ok(())
    }

    /// Runs one command line.
    async fn command(&mut self, line: &str) -> CmdResult<()> {
        let ws = words(line)?;
        let Some((first, rest)) = ws.split_first() else {
            return Err(CmdError::Broken("empty command".into()));
        };
        let (verb, target, args): (&str, Option<&String>, &[String]) = match first.as_str() {
            "insert" | "update" | "delete" | "clear" => {
                (first.as_str(), rest.first(), rest.get(1..).unwrap_or(&[]))
            }
            "flush" => ("flush", None, rest),
            _ if first.contains('.') => ("child", Some(first), rest),
            _ => ("insert", Some(first), rest),
        };
        let mut strict = false;
        let mut expected: Option<u64> = None;
        let mut pairs = Vec::new();
        for a in args {
            if a == "@strict" {
                strict = true;
            } else if let Some(v) = a.strip_prefix("@expected=") {
                expected = Some(v.parse().map_err(|_| format!("bad {a}"))?);
            } else {
                let (k, v) = a
                    .split_once('=')
                    .ok_or_else(|| format!("`{a}` is not field=value"))?;
                pairs.push((k.to_owned(), v.to_owned()));
            }
        }
        match verb {
            "flush" => self.flush().await,
            "insert" => {
                let n = target.ok_or("insert needs a document name")?.clone();
                let id = self.ids.get(&n)?;
                let mut fields = Fields::new();
                self.assign(&mut fields, self.declared, &pairs, strict)?;
                self.docs.insert(
                    n.clone(),
                    DocState {
                        id,
                        fields,
                        children: Vec::new(),
                        version: None,
                        deleted: false,
                    },
                );
                self.pending.push(n);
                Ok(())
            }
            "child" => {
                let t = target.ok_or("child row needs doc.table")?;
                let (doc, table) = t.split_once('.').ok_or("child row needs doc.table")?;
                if table != "lines" {
                    return Err(CmdError::Broken(format!("no child table `{table}`")));
                }
                self.child_counter += 1;
                let id = self
                    .ids
                    .get(&format!("{doc}.{table}.{}", self.child_counter))?;
                let mut fields = Fields::new();
                self.assign(&mut fields, &LINE_FIELDS, &pairs, strict)?;
                let st = self
                    .docs
                    .get_mut(doc)
                    .ok_or_else(|| format!("unknown document `{doc}`"))?;
                st.children.push(ChildRow { id, fields });
                Ok(())
            }
            "clear" => {
                let t = target.ok_or("clear needs doc.table")?;
                let doc = t.split('.').next().unwrap_or(t);
                let st = self
                    .docs
                    .get_mut(doc)
                    .ok_or_else(|| format!("unknown document `{doc}`"))?;
                st.children.clear();
                Ok(())
            }
            "update" => {
                self.flush().await?;
                let n = target.ok_or("update needs a document name")?.clone();
                let mut changes = Fields::new();
                self.assign(&mut changes, self.declared, &pairs, true)?;
                let st = self
                    .docs
                    .get_mut(&n)
                    .ok_or_else(|| format!("unknown document `{n}`"))?;
                st.fields.extend(changes);
                let version = expected
                    .map(Version)
                    .or(st.version)
                    .ok_or("document has no version yet")?;
                let data = self.data(&n)?;
                let v = self.driver.update(&data, version, self.max_writes).await?;
                if let Some(st) = self.docs.get_mut(&n) {
                    st.version = Some(v);
                }
                Ok(())
            }
            "delete" => {
                self.flush().await?;
                let n = target.ok_or("delete needs a document name")?.clone();
                let st = self
                    .docs
                    .get_mut(&n)
                    .ok_or_else(|| format!("unknown document `{n}`"))?;
                let version = expected
                    .map(Version)
                    .or(st.version)
                    .ok_or("no version yet")?;
                let id = st.id;
                self.driver
                    .delete(&name("item")?, id, version, self.max_writes)
                    .await?;
                // The id stays: a later command may name the deleted document (its version is
                // gone, so an `update` can only be told the expected version explicitly).
                if let Some(st) = self.docs.get_mut(&n) {
                    st.version = None;
                    st.deleted = true;
                }
                Ok(())
            }
            other => Err(CmdError::Broken(format!("unknown command `{other}`"))),
        }
    }
}

fn canon_rows(rows: &Rows, ids: &Ids) -> Vec<Vec<String>> {
    rows.rows
        .iter()
        .map(|r| r.iter().map(|v| canon(v, ids)).collect())
        .collect()
}

/// Runs `case` on `driver`.
pub async fn execute<D: Driver>(case: &Case, driver: &D) -> Observed {
    let mut ids = Ids::new();
    let mut session = Session {
        driver,
        ids: &mut ids,
        docs: BTreeMap::new(),
        pending: Vec::new(),
        max_writes: case.max_writes,
        child_counter: 0,
        declared: item_fields(case.catalog.as_deref()),
    };
    for line in &case.setup {
        match session.command(line).await {
            Ok(()) => {}
            Err(CmdError::Broken(m)) => {
                return Observed::BrokenCase(format!("setup `{line}`: {m}"));
            }
            Err(CmdError::Storage(e)) => {
                return Observed::BrokenCase(format!("setup `{line}` failed: {e}"));
            }
        }
    }
    if let Err(e) = session.flush().await {
        return match e {
            CmdError::Broken(m) => Observed::BrokenCase(format!("setup: {m}")),
            CmdError::Storage(e) => Observed::BrokenCase(format!("setup failed: {e}")),
        };
    }
    match case.op {
        // `Prepare` is the runner's: it asserts what `prepare` does with the catalog and runs no
        // command, so reaching `execute` with it is a broken case, not a backend verdict.
        Op::Prepare => Observed::BrokenCase("a prepare case runs no commands".into()),
        Op::Changes => {
            // The `write` lines are buffered, never committed on their own: they are inserted
            // and deleted in one transaction below, which must leave no change-feed entry
            // (SEM-HLD-03).
            for line in &case.write {
                if let Err(e) = session.command(line).await {
                    return match e {
                        CmdError::Broken(m) => Observed::BrokenCase(format!("write `{line}`: {m}")),
                        CmdError::Storage(e) => Observed::Error(error_kind(&e)),
                    };
                }
            }
            let names = std::mem::take(&mut session.pending);
            let docs = match names
                .iter()
                .map(|n| session.data(n))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(d) => d,
                Err(m) => return Observed::BrokenCase(m),
            };
            if let Err(e) = session
                .driver
                .insert_then_delete(&docs, session.max_writes)
                .await
            {
                return Observed::Error(error_kind(&e));
            }
            match session.driver.changes(FeedPosition::START, 1_000).await {
                Ok(changes) => Observed::Rows(
                    changes
                        .iter()
                        .map(|c| {
                            let op = match c.op {
                                ChangeOp::Insert => "insert",
                                ChangeOp::Update => "update",
                                ChangeOp::Delete => "delete",
                            };
                            vec![
                                canon(&Value::Text(op.to_owned()), session.ids),
                                canon(&Value::Text(c.document.entity.to_string()), session.ids),
                            ]
                        })
                        .collect(),
                ),
                Err(e) => Observed::Error(error_kind(&e)),
            }
        }
        Op::Write => {
            let mut r = Ok(());
            for line in &case.write {
                r = session.command(line).await;
                if r.is_err() {
                    break;
                }
            }
            if r.is_ok() {
                r = session.flush().await;
            }
            match r {
                Ok(()) => Observed::Ok,
                Err(CmdError::Storage(e)) => Observed::Error(error_kind(&e)),
                Err(CmdError::Broken(m)) => Observed::BrokenCase(m),
            }
        }
        Op::Query => {
            let q = match build_query(&case.query, session.ids) {
                Ok(q) => q,
                Err(m) => return Observed::BrokenCase(format!("query: {m}")),
            };
            let in_tx = session.driver.query(&q).await;
            let outside = session.driver.read(&q).await;
            match (in_tx, outside) {
                (Ok(a), Ok(b)) => {
                    let (a, b) = (canon_rows(&a, session.ids), canon_rows(&b, session.ids));
                    if a == b {
                        Observed::Rows(a)
                    } else {
                        Observed::Inconsistent(format!(
                            "in a transaction {}, outside {}",
                            Observed::Rows(a),
                            Observed::Rows(b)
                        ))
                    }
                }
                (Err(a), Err(b)) if a.kind == b.kind => Observed::Error(error_kind(&a)),
                (a, b) => Observed::Inconsistent(format!(
                    "in a transaction {}, outside {}",
                    a.map_or_else(|e| format!("error {}", error_kind(&e)), |_| "rows".into()),
                    b.map_or_else(|e| format!("error {}", error_kind(&e)), |_| "rows".into()),
                )),
            }
        }
        Op::Load | Op::Children => {
            let doc = case.doc.as_deref().unwrap_or_default();
            let Some(st) = session.docs.get(doc) else {
                return Observed::BrokenCase(format!("unknown document `{doc}`"));
            };
            let id = st.id;
            let item = match name("item") {
                Ok(n) => n,
                Err(m) => return Observed::BrokenCase(m),
            };
            let loaded = match session.driver.load(&item, id).await {
                Ok(l) => l,
                Err(e) => return Observed::Error(error_kind(&e)),
            };
            let Some(d) = loaded else {
                return Observed::Rows(vec![]);
            };
            let pick = |fields: &Fields, extra: &[(&str, Value)]| -> Result<Vec<String>, String> {
                case.fields
                    .iter()
                    .map(|f| {
                        if let Some((_, v)) = extra.iter().find(|(k, _)| k == f) {
                            return Ok(canon(v, session.ids));
                        }
                        let n = name(f)?;
                        fields
                            .get(&n)
                            .map(|v| canon(v, session.ids))
                            .ok_or_else(|| format!("the loaded document has no field `{f}`"))
                    })
                    .collect()
            };
            let version = Value::Int(i64::try_from(d.version.0).unwrap_or(i64::MAX));
            let result = if case.op == Op::Load {
                pick(&d.data.fields, &[("@version", version)]).map(|r| vec![r])
            } else {
                let rows = d.data.children.values().next().cloned().unwrap_or_default();
                rows.iter().map(|c| pick(&c.fields, &[])).collect()
            };
            match result {
                Ok(rows) => Observed::Rows(rows),
                Err(m) => Observed::BrokenCase(m),
            }
        }
    }
}
