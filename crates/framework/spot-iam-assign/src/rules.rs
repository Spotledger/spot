//! Assignment rules: a role given to everyone whose business facts satisfy a condition.
//!
//! A rule's condition and scope values are expressions of the shared language (`spot-expr`),
//! parsed and checked once, when the [`RuleSet`] is loaded, with every problem reported at
//! once. Evaluation is total and pure: it reads only the user's [`Facts`].

use std::collections::BTreeSet;

use serde::Deserialize;
use spot_expr::{BinaryOp, Expr, UnaryOp, Value, parse};
use spot_iam_roles::{Registry, RoleName};
use spot_permissions::{AppliesTo, UserPermission};
use time::{Duration, OffsetDateTime};

use crate::error::{self as e, AssignError, AssignErrors};
use crate::facts::{Facts, FactsContext};

/// The roles that exist, and which of them only emergency access may give.
#[derive(Clone, Debug, Default)]
pub struct RoleDirectory {
    known: BTreeSet<RoleName>,
    emergency_only: BTreeSet<RoleName>,
}

impl RoleDirectory {
    /// A directory of these roles, none emergency-only.
    pub fn new(roles: impl IntoIterator<Item = RoleName>) -> Self {
        Self {
            known: roles.into_iter().collect(),
            emergency_only: BTreeSet::new(),
        }
    }

    /// Every role of a registry.
    pub fn from_registry(registry: &Registry) -> Self {
        Self::new(registry.roles().map(|r| r.name.clone()))
    }

    /// Marks roles that only emergency access may assign.
    #[must_use]
    pub fn with_emergency_only(mut self, roles: impl IntoIterator<Item = RoleName>) -> Self {
        self.emergency_only.extend(roles);
        self
    }

    pub(crate) fn check(&self, role: &RoleName, subject: &str, problems: &mut Vec<AssignError>) {
        if !self.known.contains(role) {
            problems.push(AssignError::about(
                e::UNKNOWN_ROLE,
                subject,
                format!("gives the unknown role `{}`", role.as_str()),
            ));
        } else if self.emergency_only.contains(role) {
            problems.push(AssignError::about(
                e::EMERGENCY_ROLE,
                subject,
                format!(
                    "role `{}` is emergency-only: it comes only through emergency access",
                    role.as_str()
                ),
            ));
        }
    }
}

/// One restriction of a rule's scope template. `allowed` is an expression that gives a link
/// value (text) or a list of them, for example `facts.company`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestrictionSpec {
    /// Restricts only documents of this DocType; none is every DocType.
    #[serde(default)]
    pub doctype: Option<String>,
    /// The link DocType (or select field path) the restriction is about.
    pub link: String,
    /// The allowed records, as an expression over the facts.
    pub allowed: String,
    /// Also reach the descendants of the allowed records.
    #[serde(default)]
    pub include_descendants: bool,
}

/// A rule as written.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    /// The rule's id: what every assignment's reason names (`rule:<id>`).
    pub id: String,
    /// A human name.
    pub name: String,
    /// The role it gives.
    pub role: RoleName,
    /// The condition, an expression that is true or false.
    pub when: String,
    /// The scope template; empty means the role applies everywhere.
    #[serde(default)]
    pub scope: Vec<RestrictionSpec>,
    /// A time box in seconds from the grant; none means open-ended while the condition holds.
    #[serde(default)]
    pub valid_for_secs: Option<i64>,
    /// A disabled rule ends what it gave and gives nothing new.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Lower first: the order rules are evaluated and listed in.
    #[serde(default)]
    pub priority: i32,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug)]
pub(crate) struct Restriction {
    pub(crate) doctype: Option<String>,
    pub(crate) link: String,
    pub(crate) allowed: Expr,
    pub(crate) include_descendants: bool,
}

/// A rule that passed loading.
#[derive(Clone, Debug)]
pub struct Rule {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) role: RoleName,
    pub(crate) when: Expr,
    pub(crate) scope: Vec<Restriction>,
    pub(crate) valid_for: Option<Duration>,
    pub(crate) enabled: bool,
    pub(crate) priority: i32,
}

impl Rule {
    /// The rule's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The rule's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The role it gives.
    pub fn role(&self) -> &RoleName {
        &self.role
    }

    /// Whether the rule is switched on.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The reason text of every assignment this rule gives.
    pub fn reason(&self) -> String {
        reason_of(&self.id)
    }

    /// Whether the condition holds for `facts`.
    pub(crate) fn holds(&self, facts: &Facts, now: OffsetDateTime) -> Result<bool, String> {
        let ctx = FactsContext::new(facts, now).ok_or("the instant is out of range")?;
        match self.when.eval(&ctx) {
            Ok(Value::Bool(b)) => Ok(b),
            Ok(other) => Err(format!(
                "the condition gave {}, not true or false",
                other.type_name()
            )),
            Err(err) => Err(err.to_string()),
        }
    }

    /// The scope restrictions for `facts`, for `user`. A missing value is an error, never an
    /// unrestricted grant.
    pub(crate) fn scope_for(
        &self,
        user: &str,
        facts: &Facts,
        now: OffsetDateTime,
    ) -> Result<Vec<UserPermission>, String> {
        let ctx = FactsContext::new(facts, now).ok_or("the instant is out of range")?;
        let mut out = Vec::with_capacity(self.scope.len());
        for r in &self.scope {
            let value = r.allowed.eval(&ctx).map_err(|err| err.to_string())?;
            let mut allowed = Vec::new();
            match value {
                Value::Text(t) if !t.is_empty() => allowed.push(t),
                Value::List(items) => {
                    for item in items {
                        match item {
                            Value::Text(t) if !t.is_empty() => allowed.push(t),
                            _ => {
                                return Err(format!(
                                    "the scope value for `{}` has an item that is not a record id",
                                    r.link
                                ));
                            }
                        }
                    }
                }
                _ => {
                    return Err(format!(
                        "the scope value for `{}` is missing or not a record id",
                        r.link
                    ));
                }
            }
            allowed.sort();
            allowed.dedup();
            if allowed.is_empty() {
                return Err(format!("the scope value for `{}` is empty", r.link));
            }
            out.push(UserPermission {
                user: user.to_owned(),
                doctype: r.doctype.clone(),
                field_or_link_doctype: r.link.clone(),
                allowed,
                applies_to: AppliesTo::default(),
                include_descendants: r.include_descendants,
            });
        }
        Ok(out)
    }
}

pub(crate) fn reason_of(rule_id: &str) -> String {
    format!("rule:{rule_id}")
}

/// A loaded, validated set of rules, ordered by priority, then id.
#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
}

impl RuleSet {
    /// Validates the specs and compiles them, reporting every problem at once: unknown role,
    /// a condition that cannot be boolean, a scope value that cannot be a link value, a
    /// duplicate id, and a rule that would assign an emergency-only role.
    pub fn load(specs: Vec<RuleSpec>, roles: &RoleDirectory) -> Result<RuleSet, AssignErrors> {
        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        let mut rules = Vec::new();
        for spec in specs {
            let before = problems.len();
            let id = spec.id.clone();
            if id.trim().is_empty() || id != id.trim() || id.chars().any(char::is_control) {
                problems.push(AssignError::new(
                    e::INVALID_RULE,
                    "a rule needs an id without leading or trailing spaces or control characters",
                ));
            }
            if !seen.insert(id.clone()) {
                problems.push(AssignError::about(
                    e::DUPLICATE_RULE,
                    &id,
                    "two rules have this id",
                ));
            }
            if spec.name.trim().is_empty() {
                problems.push(AssignError::about(
                    e::INVALID_RULE,
                    &id,
                    "a rule needs a name",
                ));
            }
            roles.check(&spec.role, &id, &mut problems);
            let valid_for = match spec.valid_for_secs {
                Some(secs) if secs > 0 => Some(Duration::seconds(secs)),
                Some(_) => {
                    problems.push(AssignError::about(
                        e::INVALID_RULE,
                        &id,
                        "`valid_for_secs` must be positive",
                    ));
                    None
                }
                None => None,
            };
            let when = compile(&spec.when, &id, "the condition", &mut problems);
            if let Some(expr) = &when
                && !could_be_bool(expr)
            {
                problems.push(AssignError::about(
                    e::CONDITION_NOT_BOOLEAN,
                    &id,
                    "the condition can never be true or false",
                ));
            }
            let mut scope = Vec::new();
            for r in &spec.scope {
                if r.link.trim().is_empty() {
                    problems.push(AssignError::about(
                        e::INVALID_RULE,
                        &id,
                        "a scope restriction needs a link DocType",
                    ));
                }
                if let Some(allowed) = compile(
                    &r.allowed,
                    &id,
                    &format!("the scope value for `{}`", r.link),
                    &mut problems,
                ) {
                    if !could_be_link(&allowed) {
                        problems.push(AssignError::about(
                            e::SCOPE_NOT_LINK,
                            &id,
                            format!(
                                "the scope value for `{}` can never be a record id or a list of them",
                                r.link
                            ),
                        ));
                    }
                    scope.push(Restriction {
                        doctype: r.doctype.clone(),
                        link: r.link.clone(),
                        allowed,
                        include_descendants: r.include_descendants,
                    });
                }
            }
            if problems.len() == before
                && let Some(when) = when
            {
                rules.push(Rule {
                    id,
                    name: spec.name,
                    role: spec.role,
                    when,
                    scope,
                    valid_for,
                    enabled: spec.enabled,
                    priority: spec.priority,
                });
            }
        }
        if let Some(errors) = AssignErrors::from_list(problems) {
            return Err(errors);
        }
        rules.sort_by(|a, b| (a.priority, &a.id).cmp(&(b.priority, &b.id)));
        Ok(RuleSet { rules })
    }

    /// The rules, by priority, then id.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub(crate) fn find(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }
}

fn compile(source: &str, rule: &str, what: &str, problems: &mut Vec<AssignError>) -> Option<Expr> {
    match parse(&normalise(source)) {
        Ok(expr) => Some(expr),
        Err(err) => {
            problems.push(AssignError::about(
                e::BAD_EXPRESSION,
                rule,
                format!("{what} does not parse: {err}"),
            ));
            None
        }
    }
}

/// Spells `facts.` as the language's record root `doc.`, outside text literals. The language
/// has one record root; rules name the user's facts `facts` because that is what they are.
pub(crate) fn normalise(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let mut quote: Option<char> = None;
    while let Some(&c) = chars.get(i) {
        if let Some(q) = quote {
            out.push(c);
            if c == '\\' {
                if let Some(&next) = chars.get(i + 1) {
                    out.push(next);
                    i += 1;
                }
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        let word_start = i == 0
            || chars
                .get(i - 1)
                .is_none_or(|p| !(p.is_alphanumeric() || *p == '_' || *p == '.'));
        let word: String = chars.iter().skip(i).take(5).collect();
        if word_start && word == "facts" && chars.get(i + 5) == Some(&'.') {
            out.push_str("doc");
            i += 5;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Whether the expression could give true or false (`false` only when it provably cannot).
fn could_be_bool(expr: &Expr) -> bool {
    match expr {
        Expr::Bool(_) | Expr::Path(..) | Expr::Call(..) => true,
        Expr::Unary(UnaryOp::Not, _) => true,
        Expr::Binary(op, ..) => matches!(
            op,
            BinaryOp::Or
                | BinaryOp::And
                | BinaryOp::Eq
                | BinaryOp::Ne
                | BinaryOp::Lt
                | BinaryOp::Le
                | BinaryOp::Gt
                | BinaryOp::Ge
                | BinaryOp::In
        ),
        Expr::Ternary(_, a, b) => could_be_bool(a) || could_be_bool(b),
        _ => false,
    }
}

/// Whether the expression could give text or a list of texts.
fn could_be_link(expr: &Expr) -> bool {
    match expr {
        Expr::Text(_) | Expr::List(_) | Expr::Path(..) | Expr::Call(..) => true,
        Expr::Binary(BinaryOp::Add, ..) => true,
        Expr::Ternary(_, a, b) => could_be_link(a) || could_be_link(b),
        _ => false,
    }
}
