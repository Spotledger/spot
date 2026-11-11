//! Compilation: parse, resolve against the effective metadata, check safety and stratification,
//! and hash.
//!
//! [`RuleSet::compile`] validates a program in one pass and reports **every** problem at once:
//! unknown predicates, arity mismatches, unsafe (non range-restricted) rules, guards that read
//! an unbound name, and programs that cannot be stratified. A [`Compiled`] program carries a
//! SHA-256 content hash over its resolved content, which is the key the registry swaps on.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha2::{Digest, Sha256};
use spot_expr::Expr;
use spot_graph_model::GraphModel;
use spot_meta::{EffectiveMeta, FieldType};

use crate::ast::{self, AggFunc, HeadArg, Literal, Term};
use crate::datum::Datum;
use crate::error::{
    ARITY, NOT_A_FIELD, NOT_STRATIFIED, PREDICATE_CONFLICT, Problem, RuleGraphError,
    UNKNOWN_PREDICATE, UNKNOWN_VARIABLE, UNSAFE,
};
use crate::parse;

/// A rule set's source text and parsed program.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleSet {
    program: ast::Program,
    source: String,
}

impl RuleSet {
    /// Parses rule text.
    ///
    /// # Errors
    ///
    /// [`RuleGraphError::Invalid`] with one [`Problem`] per parse fault (code
    /// [`PARSE`](crate::error::PARSE)).
    pub fn parse(text: &str) -> Result<Self, RuleGraphError> {
        let program = parse::parse(text)?;
        Ok(Self {
            program,
            source: text.to_owned(),
        })
    }

    /// Parses and compiles rule text against the effective metadata, deriving the graph model
    /// from the same metadata.
    ///
    /// # Errors
    ///
    /// [`RuleGraphError::Invalid`] with every problem found.
    pub fn compile(text: &str, meta: &EffectiveMeta) -> Result<Compiled, RuleGraphError> {
        let model = GraphModel::derive(meta);
        Self::compile_with_model(text, meta, &model)
    }

    /// Parses and compiles rule text against the effective metadata and a pre-built graph model
    /// (the caller may have derived it with relation specs).
    ///
    /// # Errors
    ///
    /// [`RuleGraphError::Invalid`] with every problem found.
    pub fn compile_with_model(
        text: &str,
        meta: &EffectiveMeta,
        model: &GraphModel,
    ) -> Result<Compiled, RuleGraphError> {
        Self::parse(text)?.compile_parsed(meta, model)
    }

    /// The source text.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The parsed rules, in source order.
    #[must_use]
    pub fn rules(&self) -> &[ast::Rule] {
        &self.program.rules
    }

    fn compile_parsed(
        &self,
        meta: &EffectiveMeta,
        model: &GraphModel,
    ) -> Result<Compiled, RuleGraphError> {
        let mut problems: Vec<Problem> = Vec::new();
        let mut predicates: BTreeMap<String, Predicate> = BTreeMap::new();

        // Heads define the derived predicates (their arity is fixed by the first definition).
        for rule in &self.program.rules {
            let name = &rule.head.predicate;
            let arity = rule.head.args.len();
            if let Class::Field { doctype, field } = classify(meta, model, name) {
                problems.push(Problem::new(
                    PREDICATE_CONFLICT,
                    rule.head.span.line,
                    rule.head.span.column,
                    format!("`{name}` is the field `{field}` of `{doctype}`; a derived predicate cannot take its name"),
                ));
                continue;
            }
            if model.edge(name).is_some() {
                problems.push(Problem::new(
                    PREDICATE_CONFLICT,
                    rule.head.span.line,
                    rule.head.span.column,
                    format!("`{name}` is an edge type; a derived predicate cannot take its name"),
                ));
                continue;
            }
            match predicates.get(name) {
                Some(existing) if existing.arity != arity => problems.push(Problem::new(
                    ARITY,
                    rule.head.span.line,
                    rule.head.span.column,
                    format!(
                        "`{name}` is defined with {arity} argument(s) here and {} elsewhere",
                        existing.arity
                    ),
                )),
                Some(_) => {}
                None => {
                    predicates.insert(
                        name.clone(),
                        Predicate {
                            name: name.clone(),
                            arity,
                            kind: PredKind::Derived,
                            stratum: 0,
                        },
                    );
                }
            }
        }

        // Resolve every body literal.
        let mut resolved: Vec<RRule> = Vec::with_capacity(self.program.rules.len());
        for rule in &self.program.rules {
            let mut positives = Vec::new();
            let mut negatives = Vec::new();
            let mut guards = Vec::new();
            for literal in &rule.body {
                match literal {
                    Literal::Atom {
                        predicate,
                        args,
                        span,
                    } => {
                        resolve_atom(
                            &mut predicates,
                            meta,
                            model,
                            predicate,
                            args.len(),
                            *span,
                            &mut problems,
                        );
                        positives.push(RAtom {
                            pred: predicate.clone(),
                            terms: args.iter().map(resolve_term).collect(),
                            span: *span,
                        });
                    }
                    Literal::Neg {
                        predicate,
                        args,
                        span,
                    } => {
                        resolve_atom(
                            &mut predicates,
                            meta,
                            model,
                            predicate,
                            args.len(),
                            *span,
                            &mut problems,
                        );
                        negatives.push(RAtom {
                            pred: predicate.clone(),
                            terms: args.iter().map(resolve_term).collect(),
                            span: *span,
                        });
                    }
                    Literal::Guard {
                        source, expr, span, ..
                    } => {
                        let (vars, _) = parse::guard_paths(expr);
                        guards.push(RGuard {
                            expr: expr.clone(),
                            vars,
                            source: source.clone(),
                            span: *span,
                        });
                    }
                }
            }
            let aggregate = rule
                .head
                .args
                .iter()
                .any(|arg| matches!(arg, HeadArg::Agg { .. }));
            let head = RHead {
                pred: rule.head.predicate.clone(),
                args: rule
                    .head
                    .args
                    .iter()
                    .map(|arg| match arg {
                        HeadArg::Var(var) => RHeadArg::Var(var.clone()),
                        HeadArg::Const(value) => RHeadArg::Const(value.to_datum()),
                        HeadArg::Agg { func, var, .. } => RHeadArg::Agg(*func, var.clone()),
                    })
                    .collect(),
            };
            resolved.push(RRule {
                head,
                positives,
                negatives,
                guards,
                aggregate,
            });
        }

        // Safety: every variable in the head, a negated atom or a guard must be bound by a
        // positive body atom. This is the range restriction that, with a finite fact source,
        // makes the least fixed point finite.
        for rule in &resolved {
            let bound: BTreeSet<String> = rule
                .positives
                .iter()
                .flat_map(|atom| atom.terms.iter())
                .filter_map(|term| match term {
                    RTerm::Var(var) => Some(var.clone()),
                    RTerm::Anon | RTerm::Const(_) => None,
                })
                .collect();
            for arg in &rule.head.args {
                let (var, what) = match arg {
                    RHeadArg::Var(var) => (var, "the head"),
                    RHeadArg::Agg(_, var) => (var, "an aggregate"),
                    RHeadArg::Const(_) => continue,
                };
                if !bound.contains(var) {
                    problems.push(Problem::plain(
                        UNSAFE,
                        format!("variable `{var}` in {what} is not bound by a positive body atom"),
                    ));
                }
            }
            for neg in &rule.negatives {
                for term in &neg.terms {
                    if let RTerm::Var(var) = term
                        && !bound.contains(var)
                    {
                        problems.push(Problem::new(
                            UNSAFE,
                            neg.span.line,
                            neg.span.column,
                            format!("variable `{var}` in a negated atom is not bound by a positive body atom"),
                        ));
                    }
                }
            }
            for guard in &rule.guards {
                for var in &guard.vars {
                    if !bound.contains(var) {
                        problems.push(Problem::new(
                            UNKNOWN_VARIABLE,
                            guard.span.line,
                            guard.span.column,
                            format!("the guard reads `{var}`, which is not a bound variable"),
                        ));
                    }
                }
            }
        }

        // Stratification: the head of a rule is at least as high as each positive body predicate,
        // and strictly higher through a negated atom or across an aggregate rule.
        let names: Vec<String> = predicates.keys().cloned().collect();
        let index_of: BTreeMap<&str, usize> = names
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_str(), i))
            .collect();
        let mut edges: Vec<(usize, usize, usize)> = Vec::new();
        for rule in &resolved {
            let Some(&head) = index_of.get(rule.head.pred.as_str()) else {
                continue;
            };
            let weight = usize::from(rule.aggregate);
            for atom in &rule.positives {
                if let Some(&body) = index_of.get(atom.pred.as_str()) {
                    edges.push((head, body, weight));
                }
            }
            for atom in &rule.negatives {
                if let Some(&body) = index_of.get(atom.pred.as_str()) {
                    edges.push((head, body, 1));
                }
            }
        }
        let count = names.len();
        let mut stratum = vec![0usize; count];
        let mut changed = true;
        let mut iterations = 0usize;
        while changed {
            changed = false;
            for &(head, body, weight) in &edges {
                let want = stratum[body].saturating_add(weight);
                if stratum[head] < want {
                    stratum[head] = want;
                    changed = true;
                }
            }
            iterations += 1;
            if iterations > count {
                break;
            }
        }
        if changed {
            problems.push(Problem::plain(
                NOT_STRATIFIED,
                "the program is not stratifiable: a cycle runs through negation or through an aggregate",
            ));
        }

        if !problems.is_empty() {
            return Err(RuleGraphError::Invalid(problems));
        }

        for (i, name) in names.iter().enumerate() {
            if let Some(predicate) = predicates.get_mut(name) {
                predicate.stratum = stratum[i];
            }
        }
        let max_stratum = stratum.iter().copied().max().unwrap_or(0);
        let mut strata: Vec<Vec<usize>> = vec![Vec::new(); max_stratum + 1];
        for (rule_index, rule) in resolved.iter().enumerate() {
            let level = index_of
                .get(rule.head.pred.as_str())
                .map_or(0, |&i| stratum[i]);
            strata[level].push(rule_index);
        }

        let hash = ContentHash::of(&predicates, &resolved);
        Ok(Compiled {
            source: self.source.clone(),
            predicates,
            rules: resolved,
            strata,
            hash,
            meta_version: meta.version(),
        })
    }
}

/// The class of a name in the metadata: an edge type, a readable field, a link/table field (an
/// edge, not a field predicate), or nothing.
enum Class {
    Edge,
    Field { doctype: String, field: String },
    EdgeField,
    Unknown,
}

fn classify(meta: &EffectiveMeta, model: &GraphModel, name: &str) -> Class {
    if model.edge(name).is_some() {
        return Class::Edge;
    }
    let Some((doctype, field)) = name.rsplit_once('.') else {
        return Class::Unknown;
    };
    let Some(definition) = meta.definition(doctype) else {
        return Class::Unknown;
    };
    let Some(spec) = definition.fields.iter().find(|f| f.name == field) else {
        return Class::Unknown;
    };
    if is_edge_field(spec.ty) {
        Class::EdgeField
    } else {
        Class::Field {
            doctype: doctype.to_owned(),
            field: field.to_owned(),
        }
    }
}

/// A field that is an edge, never a readable field predicate.
fn is_edge_field(ty: FieldType) -> bool {
    matches!(
        ty,
        FieldType::Link | FieldType::DynamicLink | FieldType::Table | FieldType::TableMultiselect
    )
}

fn resolve_atom(
    predicates: &mut BTreeMap<String, Predicate>,
    meta: &EffectiveMeta,
    model: &GraphModel,
    name: &str,
    arity: usize,
    span: ast::Span,
    problems: &mut Vec<Problem>,
) {
    if let Some(existing) = predicates.get(name) {
        if existing.arity != arity {
            problems.push(Problem::new(
                ARITY,
                span.line,
                span.column,
                format!("`{name}` takes {} argument(s), not {arity}", existing.arity),
            ));
        }
        return;
    }
    let predicate = match classify(meta, model, name) {
        Class::Edge => Predicate {
            name: name.to_owned(),
            arity: 2,
            kind: PredKind::Edge,
            stratum: 0,
        },
        Class::Field { doctype, field } => Predicate {
            name: name.to_owned(),
            arity: 2,
            kind: PredKind::Field { doctype, field },
            stratum: 0,
        },
        Class::EdgeField => {
            problems.push(Problem::new(
                NOT_A_FIELD,
                span.line,
                span.column,
                format!("`{name}` is a link or table field; it is an edge type, not a readable field predicate"),
            ));
            return;
        }
        Class::Unknown => {
            problems.push(Problem::new(
                UNKNOWN_PREDICATE,
                span.line,
                span.column,
                format!("`{name}` is not an edge type, a metadata field or a derived predicate"),
            ));
            return;
        }
    };
    if predicate.arity != arity {
        problems.push(Problem::new(
            ARITY,
            span.line,
            span.column,
            format!(
                "`{name}` takes {} argument(s), not {arity}",
                predicate.arity
            ),
        ));
    }
    predicates.insert(name.to_owned(), predicate);
}

fn resolve_term(term: &Term) -> RTerm {
    match term {
        Term::Var(var) => RTerm::Var(var.clone()),
        Term::Anon => RTerm::Anon,
        Term::Const(value) => RTerm::Const(value.to_datum()),
    }
}

/// A compiled, validated rule program.
#[derive(Clone, Debug, PartialEq)]
pub struct Compiled {
    source: String,
    predicates: BTreeMap<String, Predicate>,
    pub(crate) rules: Vec<RRule>,
    pub(crate) strata: Vec<Vec<usize>>,
    hash: ContentHash,
    meta_version: u64,
}

impl Compiled {
    /// The source text the program was compiled from.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The content hash of the compiled program.
    #[must_use]
    pub fn hash(&self) -> &ContentHash {
        &self.hash
    }

    /// The metadata version the program was compiled against.
    ///
    /// Informational only: it is recorded for logs and diagnostics and never
    /// decides whether a rule set is current — [`Registry::reload`] compares
    /// [`Compiled::hash`] because two different metadata sets can carry the
    /// same version (every fresh `spot_meta::Registry` reports 1).
    ///
    /// [`Registry::reload`]: crate::Registry::reload
    #[must_use]
    pub fn meta_version(&self) -> u64 {
        self.meta_version
    }

    /// The predicate named `name`, if the program knows it.
    #[must_use]
    pub fn predicate(&self, name: &str) -> Option<&Predicate> {
        self.predicates.get(name)
    }

    /// Every predicate, in name order.
    pub fn predicates(&self) -> impl Iterator<Item = &Predicate> {
        self.predicates.values()
    }

    /// The number of strata the program is evaluated in.
    #[must_use]
    pub fn strata_count(&self) -> usize {
        self.strata.len()
    }

    /// The number of rules.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// A predicate the program knows: a base edge or field, or a derived predicate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Predicate {
    /// The predicate name.
    pub name: String,
    /// How many arguments it takes.
    pub arity: usize,
    /// Where it comes from.
    pub kind: PredKind,
    /// The stratum it is evaluated in.
    pub stratum: usize,
}

/// Where a predicate comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredKind {
    /// An edge type of the graph model.
    Edge,
    /// A readable metadata field.
    Field {
        /// The DocType.
        doctype: String,
        /// The field.
        field: String,
    },
    /// A predicate defined by a rule.
    Derived,
}

impl PredKind {
    /// The kind's stable tag, for a content hash.
    #[must_use]
    pub fn tag(&self) -> String {
        match self {
            Self::Edge => "edge".to_owned(),
            Self::Field { doctype, field } => format!("field:{doctype}:{field}"),
            Self::Derived => "derived".to_owned(),
        }
    }
}

/// The SHA-256 content hash of a compiled program, 64 lowercase hex characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(String);

impl ContentHash {
    fn of(predicates: &BTreeMap<String, Predicate>, rules: &[RRule]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"spot-rule-graph-v1\n");
        for (name, predicate) in predicates {
            hasher.update(
                format!(
                    "pred\t{name}\t{}\t{}\t{}\n",
                    predicate.arity,
                    predicate.kind.tag(),
                    predicate.stratum
                )
                .as_bytes(),
            );
        }
        let mut texts: Vec<String> = rules.iter().map(canonical_rule).collect();
        texts.sort();
        for text in texts {
            hasher.update(text.as_bytes());
            hasher.update(b"\n");
        }
        Self(hex(&hasher.finalize()))
    }

    /// The 64 lowercase hex characters.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn canonical_rule(rule: &RRule) -> String {
    let head = rule
        .head
        .args
        .iter()
        .map(canonical_head_arg)
        .collect::<Vec<_>>()
        .join(",");
    let mut body: Vec<String> = Vec::new();
    body.extend(rule.positives.iter().map(canonical_atom));
    body.extend(
        rule.negatives
            .iter()
            .map(|atom| format!("!{}", canonical_atom(atom))),
    );
    body.extend(
        rule.guards
            .iter()
            .map(|guard| format!("where({})", guard.source)),
    );
    body.sort();
    format!("rule\t{}\t{head}\t{}", rule.head.pred, body.join(","))
}

fn canonical_head_arg(arg: &RHeadArg) -> String {
    match arg {
        RHeadArg::Var(var) => format!("v:{var}"),
        RHeadArg::Const(value) => format!("c:{}", value.canonical()),
        RHeadArg::Agg(func, var) => format!("a:{}:{var}", func.as_str()),
    }
}

fn canonical_atom(atom: &RAtom) -> String {
    let terms = atom
        .terms
        .iter()
        .map(|term| match term {
            RTerm::Var(var) => format!("v:{var}"),
            RTerm::Anon => "_".to_owned(),
            RTerm::Const(value) => format!("c:{}", value.canonical()),
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{}({terms})", atom.pred)
}

/// A resolved term.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RTerm {
    /// A variable.
    Var(String),
    /// The anonymous variable.
    Anon,
    /// A constant.
    Const(Datum),
}

/// A resolved atom.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RAtom {
    pub(crate) pred: String,
    pub(crate) terms: Vec<RTerm>,
    pub(crate) span: ast::Span,
}

/// A resolved guard.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RGuard {
    pub(crate) expr: Expr,
    pub(crate) vars: Vec<String>,
    pub(crate) source: String,
    pub(crate) span: ast::Span,
}

/// A resolved head argument.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RHeadArg {
    /// A grouping variable.
    Var(String),
    /// A grouping constant.
    Const(Datum),
    /// An aggregate over a variable.
    Agg(AggFunc, String),
}

/// A resolved head.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RHead {
    pub(crate) pred: String,
    pub(crate) args: Vec<RHeadArg>,
}

/// A resolved rule.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RRule {
    pub(crate) head: RHead,
    pub(crate) positives: Vec<RAtom>,
    pub(crate) negatives: Vec<RAtom>,
    pub(crate) guards: Vec<RGuard>,
    pub(crate) aggregate: bool,
}
