//! The rule corpus: every `tests/corpus/*.toml` gives deterministic results.
//!
//! A corpus file names a metadata fixture, a rule program, its facts (edges and fields), a query
//! and the exact expected rows. The harness compiles the program, evaluates it and compares the
//! rows to the golden output, in order.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod support;

use serde::Deserialize;
use spot_rule_graph::{Datum, Engine, MemoryFacts, Query, RuleSet, Tuple};
use spot_storage_contract::{NodeRef, Value};

use support::{meta, rec};

#[derive(Debug, Deserialize)]
struct Corpus {
    name: String,
    meta: String,
    query: String,
    rules: String,
    #[serde(default)]
    edge: Vec<EdgeFact>,
    #[serde(default)]
    field: Vec<FieldFact>,
    expect: Vec<Expect>,
}

#[derive(Debug, Clone, Deserialize)]
struct Ref {
    entity: String,
    id: u8,
}

impl Ref {
    fn node(&self) -> NodeRef {
        rec(&self.entity, self.id)
    }
}

#[derive(Debug, Deserialize)]
struct EdgeFact {
    #[serde(rename = "type")]
    edge_type: String,
    from: Ref,
    to: Ref,
}

#[derive(Debug, Deserialize)]
struct FieldFact {
    doctype: String,
    field: String,
    record: Ref,
    value: ValueRepr,
}

#[derive(Debug, Deserialize)]
struct Expect {
    row: Vec<ValueRepr>,
}

/// A typed value in the corpus, one key per variant (no reliance on TOML number semantics).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
enum ValueRepr {
    Record { record: Ref },
    Int { int: i64 },
    Decimal { decimal: String },
    Text { text: String },
    Bool { bool: bool },
    Null { null: bool },
}

impl ValueRepr {
    fn datum(&self) -> Datum {
        match self {
            Self::Record { record } => Datum::Record(record.node()),
            Self::Int { int } => Datum::Int(*int),
            Self::Decimal { decimal } => Datum::Decimal(
                decimal
                    .parse()
                    .unwrap_or_else(|e| panic!("corpus decimal `{decimal}`: {e}")),
            ),
            Self::Text { text } => Datum::Text(text.clone()),
            Self::Bool { bool } => Datum::Bool(*bool),
            Self::Null { .. } => Datum::Null,
        }
    }

    fn storage(&self) -> Value {
        match self {
            Self::Record { record } => Value::Id(record.node().id),
            Self::Int { int } => Value::Int(*int),
            Self::Decimal { decimal } => Value::Decimal(
                decimal
                    .parse()
                    .unwrap_or_else(|e| panic!("corpus decimal `{decimal}`: {e}")),
            ),
            Self::Text { text } => Value::Text(text.clone()),
            Self::Bool { bool } => Value::Bool(*bool),
            Self::Null { .. } => Value::Null,
        }
    }
}

#[test]
fn corpus_golden_outputs() {
    let directory = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(directory)
        .unwrap_or_else(|e| panic!("corpus directory: {e}"))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "the corpus has at least one file");

    for path in files {
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let corpus: Corpus =
            toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let meta = meta(&corpus.meta);
        let compiled = RuleSet::compile(&corpus.rules, &meta)
            .unwrap_or_else(|e| panic!("corpus `{}` compiles: {e:?}", corpus.name));

        let mut facts = MemoryFacts::new();
        for edge in &corpus.edge {
            facts.add_edge(&edge.edge_type, edge.from.node(), edge.to.node());
        }
        for field in &corpus.field {
            facts.add_field(
                &field.doctype,
                &field.field,
                field.record.node(),
                field.value.storage(),
            );
        }

        let rows = Engine::new()
            .evaluate(&compiled, &facts, &Query::new(&corpus.query))
            .unwrap_or_else(|e| panic!("corpus `{}` evaluates: {e}", corpus.name));
        let expected: Vec<Tuple> = corpus
            .expect
            .iter()
            .map(|expect| expect.row.iter().map(ValueRepr::datum).collect())
            .collect();
        assert_eq!(rows.rows, expected, "corpus `{}`", corpus.name);
    }
}
