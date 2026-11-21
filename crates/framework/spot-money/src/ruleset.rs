//! Named rounding rules and where they apply: a [`RuleSet`] read from TOML (ADR-12: rounding
//! rules are data, declared by the app that needs them).
//!
//! ```toml
//! default = "money2"            # optional: the rule when nothing else matches
//!
//! [rules.money2]                # named rules: scale, mode, optional step
//! scale = 2
//! mode = "half_up"
//!
//! [rules.cash5]
//! scale = 2
//! mode = "half_up"
//! step = "0.05"                 # decimals are strings, never binary numbers
//!
//! [currency]                    # one table per kind: key -> rule name
//! "*" = "money2"                # `"*"` is the kind's default
//! CHF = "cash5"
//!
//! [tax_rule]
//! "vat-reduced" = "money2"
//! ```
//!
//! # Kinds and the fallback order
//!
//! A rule is looked up by `(kind, key)`; the kinds are `currency` (the key is a currency code),
//! `tax_rule`, `price`, `quantity` and `line_total` (any key the app chooses). The first match
//! wins:
//!
//! 1. the rule bound to exactly `(kind, key)`;
//! 2. the rule bound to `(kind, "*")`, the kind's default;
//! 3. the top-level `default`;
//! 4. otherwise [`MoneyError::NoRule`] (`money.no_rule`): nothing is ever guessed.
//!
//! [`RuleSet::explain`] returns the rule and says which step matched.
//!
//! # Errors
//!
//! A bad file reports **every** problem at once ([`RuleSetError`]), each with a `money.*` code and
//! the path of the offending entry.

use std::collections::BTreeMap;
use std::fmt;

use toml::Value;

use crate::currency::Currency;
use crate::error::{MoneyError, Result};
use crate::rounding::{Mode, RoundingRule, parse_decimal};

/// What a rule is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A currency; the key is the currency code.
    Currency,
    /// A tax rule; the key is its name.
    TaxRule,
    /// A unit price.
    Price,
    /// A quantity.
    Quantity,
    /// A line total.
    LineTotal,
}

impl Kind {
    /// Every kind.
    pub const ALL: [Kind; 5] = [
        Kind::Currency,
        Kind::TaxRule,
        Kind::Price,
        Kind::Quantity,
        Kind::LineTotal,
    ];

    /// The name used as a TOML table.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Currency => "currency",
            Kind::TaxRule => "tax_rule",
            Kind::Price => "price",
            Kind::Quantity => "quantity",
            Kind::LineTotal => "line_total",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The key that binds a kind's default rule.
pub const KIND_DEFAULT: &str = "*";

/// Which step of the fallback order produced a rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Bound to exactly the `(kind, key)` asked for.
    Exact,
    /// The kind's `"*"` default.
    KindDefault,
    /// The top-level `default`.
    GlobalDefault,
}

/// Which rule applied and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Explanation {
    /// The rule's name in the set.
    pub rule_name: String,
    /// The rule.
    pub rule: RoundingRule,
    /// The step of the fallback order that matched.
    pub source: Source,
    /// A sentence saying all of the above.
    pub text: String,
}

/// Named rules and the bindings from `(kind, key)` to a rule name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleSet {
    rules: BTreeMap<String, RoundingRule>,
    default: Option<String>,
    bindings: BTreeMap<(Kind, String), String>,
}

/// One problem found in a rule file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The stable `money.*` code.
    pub code: &'static str,
    /// Where: a dotted path such as `rules.cash5.step` or `currency.CHF`.
    pub path: String,
    /// What is wrong, in plain words.
    pub message: String,
}

/// Every problem found in a rule file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSetError {
    /// The problems, in file order within each section; never empty.
    pub problems: Vec<Problem>,
}

impl fmt::Display for RuleSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} problem(s) in the rounding rules:",
            self.problems.len()
        )?;
        for p in &self.problems {
            writeln!(f)?;
            write!(f, "  {}: {} ({})", p.code, p.message, p.path)?;
        }
        Ok(())
    }
}

impl std::error::Error for RuleSetError {}

impl RuleSetError {
    /// The codes of every problem, in order.
    pub fn codes(&self) -> Vec<&'static str> {
        self.problems.iter().map(|p| p.code).collect()
    }
}

const SECTIONS: [&str; 7] = [
    "default",
    "rules",
    "currency",
    "tax_rule",
    "price",
    "quantity",
    "line_total",
];

fn valid_rule_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.chars().count() <= 64 && !key.chars().any(char::is_control)
}

impl RuleSet {
    /// An empty set: every lookup is `money.no_rule` until rules are added.
    pub fn new() -> RuleSet {
        RuleSet::default()
    }

    /// Adds (or replaces) the named rule.
    pub fn with_rule(mut self, name: &str, rule: RoundingRule) -> Result<RuleSet> {
        if !valid_rule_name(name) {
            return Err(MoneyError::BadStep(format!("`{name}` is not a rule name")));
        }
        self.rules.insert(name.to_owned(), rule);
        Ok(self)
    }

    /// Binds `(kind, key)` to the named rule (use [`KIND_DEFAULT`] as the key for the kind's
    /// default). The rule must exist.
    pub fn bind(mut self, kind: Kind, key: &str, rule_name: &str) -> Result<RuleSet> {
        if !self.rules.contains_key(rule_name) {
            return Err(MoneyError::NoRule {
                kind: "rule".into(),
                key: rule_name.to_owned(),
            });
        }
        self.bindings
            .insert((kind, key.to_owned()), rule_name.to_owned());
        Ok(self)
    }

    /// Sets the top-level default rule. The rule must exist.
    pub fn with_default(mut self, rule_name: &str) -> Result<RuleSet> {
        if !self.rules.contains_key(rule_name) {
            return Err(MoneyError::NoRule {
                kind: "rule".into(),
                key: rule_name.to_owned(),
            });
        }
        self.default = Some(rule_name.to_owned());
        Ok(self)
    }

    /// The rule that applies to `(kind, key)`, by the fallback order in the module documentation.
    pub fn lookup(&self, kind: Kind, key: &str) -> Result<&RoundingRule> {
        let (name, _) = self.resolve(kind, key)?;
        self.rules.get(name).ok_or_else(|| MoneyError::NoRule {
            kind: kind.to_string(),
            key: key.to_owned(),
        })
    }

    /// The rule that applies to `currency`.
    pub fn for_currency(&self, currency: &Currency) -> Result<&RoundingRule> {
        self.lookup(Kind::Currency, currency.code())
    }

    /// Which rule applies to `(kind, key)` and why.
    pub fn explain(&self, kind: Kind, key: &str) -> Result<Explanation> {
        let (name, source) = self.resolve(kind, key)?;
        let rule = *self.rules.get(name).ok_or_else(|| MoneyError::NoRule {
            kind: kind.to_string(),
            key: key.to_owned(),
        })?;
        let text = match source {
            Source::Exact => format!("{kind} `{key}` is bound to rule `{name}` ({rule})"),
            Source::KindDefault => format!(
                "{kind} `{key}` has no rule of its own; the {kind} default `{KIND_DEFAULT}` is rule `{name}` ({rule})"
            ),
            Source::GlobalDefault => format!(
                "{kind} `{key}` has no rule and {kind} has no default; the top-level default is rule `{name}` ({rule})"
            ),
        };
        Ok(Explanation {
            rule_name: name.to_owned(),
            rule,
            source,
            text,
        })
    }

    fn resolve(&self, kind: Kind, key: &str) -> Result<(&str, Source)> {
        let bound = |k: &str| self.bindings.get(&(kind, k.to_owned())).map(String::as_str);
        if let Some(name) = bound(key) {
            return Ok((name, Source::Exact));
        }
        if let Some(name) = bound(KIND_DEFAULT) {
            return Ok((name, Source::KindDefault));
        }
        if let Some(name) = self.default.as_deref() {
            return Ok((name, Source::GlobalDefault));
        }
        Err(MoneyError::NoRule {
            kind: kind.to_string(),
            key: key.to_owned(),
        })
    }

    /// The named rules.
    pub fn rules(&self) -> impl Iterator<Item = (&str, &RoundingRule)> {
        self.rules.iter().map(|(n, r)| (n.as_str(), r))
    }

    /// Parses a rule file, reporting every problem at once.
    pub fn from_toml(text: &str) -> std::result::Result<RuleSet, RuleSetError> {
        let table: toml::Table = match toml::from_str(text) {
            Ok(table) => table,
            Err(e) => {
                return Err(RuleSetError {
                    problems: vec![Problem {
                        code: "money.syntax",
                        path: String::new(),
                        message: e.message().to_owned(),
                    }],
                });
            }
        };
        let mut problems = Vec::new();
        let mut set = RuleSet::new();
        let mut note = |code: &'static str, path: String, message: String| {
            problems.push(Problem {
                code,
                path,
                message,
            })
        };

        for key in table.keys() {
            if !SECTIONS.contains(&key.as_str()) {
                note(
                    "money.unknown_key",
                    key.clone(),
                    format!(
                        "`{key}` is not a section (expected one of {})",
                        SECTIONS.join(", ")
                    ),
                );
            }
        }
        if let Some(value) = table.get("rules") {
            match value.as_table() {
                None => note(
                    "money.bad_type",
                    "rules".into(),
                    "`rules` must be a table of named rules".into(),
                ),
                Some(rules) => {
                    for (name, value) in rules {
                        if !valid_rule_name(name) {
                            note(
                                "money.bad_name",
                                format!("rules.{name}"),
                                format!(
                                    "`{name}` is not a rule name (letters, digits, `_`, `-`, `.`; at most 40)"
                                ),
                            );
                        }
                        if let Some(rule) = read_rule(name, value, &mut note) {
                            set.rules.insert(name.clone(), rule);
                        }
                    }
                }
            }
        }
        // A rule that failed to parse is already reported; do not also call it unknown.
        let defined = |name: &str| {
            table
                .get("rules")
                .and_then(Value::as_table)
                .is_some_and(|t| t.contains_key(name))
        };
        if let Some(value) = table.get("default") {
            match value.as_str() {
                None => note(
                    "money.bad_type",
                    "default".into(),
                    "`default` must be a rule name".into(),
                ),
                Some(name) if !defined(name) => note(
                    "money.unknown_rule",
                    "default".into(),
                    format!("`default` names rule `{name}`, which is not defined"),
                ),
                Some(name) => set.default = Some(name.to_owned()),
            }
        }
        for kind in Kind::ALL {
            let Some(value) = table.get(kind.as_str()) else {
                continue;
            };
            let Some(entries) = value.as_table() else {
                note(
                    "money.bad_type",
                    kind.to_string(),
                    format!(
                        "`{kind}` must be a table of key = rule name pairs, the name written as a string"
                    ),
                );
                continue;
            };
            for (key, value) in entries {
                let path = format!("{kind}.{key}");
                let key_ok = if kind == Kind::Currency && key != KIND_DEFAULT {
                    Currency::new(key).is_ok()
                } else {
                    valid_key(key)
                };
                if !key_ok {
                    let what = if kind == Kind::Currency {
                        "a currency code (three uppercase letters) or `*`"
                    } else {
                        "a key (1 to 64 characters, no control characters)"
                    };
                    note(
                        if kind == Kind::Currency {
                            "money.bad_currency"
                        } else {
                            "money.bad_key"
                        },
                        path.clone(),
                        format!("`{key}` is not {what}"),
                    );
                }
                match value.as_str() {
                    None => note(
                        "money.bad_type",
                        path,
                        "a binding must be a rule name written as a string".into(),
                    ),
                    Some(name) if !defined(name) => note(
                        "money.unknown_rule",
                        path,
                        format!("names rule `{name}`, which is not defined"),
                    ),
                    Some(name) if key_ok => {
                        set.bindings.insert((kind, key.clone()), name.to_owned());
                    }
                    Some(_) => {}
                }
            }
        }
        if problems.is_empty() {
            Ok(set)
        } else {
            Err(RuleSetError { problems })
        }
    }
}

/// Reads one `[rules.<name>]` table, reporting every problem in it.
fn read_rule(
    name: &str,
    value: &Value,
    note: &mut impl FnMut(&'static str, String, String),
) -> Option<RoundingRule> {
    let path = |field: &str| format!("rules.{name}.{field}");
    let Some(table) = value.as_table() else {
        note(
            "money.bad_type",
            format!("rules.{name}"),
            "a rule must be a table with scale, mode and optionally step".into(),
        );
        return None;
    };
    let mut ok = true;
    for key in table.keys() {
        if !["scale", "mode", "step"].contains(&key.as_str()) {
            note(
                "money.unknown_key",
                path(key),
                format!("`{key}` is not a rule field (scale, mode, step)"),
            );
            ok = false;
        }
    }
    let scale = match table.get("scale") {
        None => {
            note(
                "money.missing_field",
                path("scale"),
                "a rule needs a scale".into(),
            );
            ok = false;
            None
        }
        Some(Value::Integer(n)) if (0..=28).contains(n) => u32::try_from(*n).ok(),
        Some(Value::Integer(n)) => {
            note(
                "money.bad_scale",
                path("scale"),
                format!("scale {n} is outside 0 to 28"),
            );
            ok = false;
            None
        }
        Some(_) => {
            note(
                "money.bad_type",
                path("scale"),
                "scale must be a whole number".into(),
            );
            ok = false;
            None
        }
    };
    let mode = match table.get("mode") {
        None => {
            note(
                "money.missing_field",
                path("mode"),
                "a rule needs a mode".into(),
            );
            ok = false;
            None
        }
        Some(Value::String(m)) => match Mode::parse(m) {
            Some(mode) => Some(mode),
            None => {
                let names: Vec<&str> = Mode::ALL.iter().map(|m| m.as_str()).collect();
                note(
                    "money.bad_mode",
                    path("mode"),
                    format!("`{m}` is not a mode (one of {})", names.join(", ")),
                );
                ok = false;
                None
            }
        },
        Some(_) => {
            note(
                "money.bad_type",
                path("mode"),
                "mode must be a string".into(),
            );
            ok = false;
            None
        }
    };
    let step = match table.get("step") {
        None => None,
        Some(Value::String(s)) => match parse_decimal(s) {
            Ok(d) => Some(d),
            Err(_) => {
                note(
                    "money.bad_step",
                    path("step"),
                    format!("`{s}` is not an exact decimal"),
                );
                ok = false;
                None
            }
        },
        Some(Value::Integer(n)) => Some(rust_decimal::Decimal::from(*n)),
        Some(_) => {
            note(
                "money.bad_type",
                path("step"),
                "write the step as a string, for example '0.05'".into(),
            );
            ok = false;
            None
        }
    };
    let (scale, mode) = (scale?, mode?);
    if !ok {
        return None;
    }
    match RoundingRule::new(scale, mode, step) {
        Ok(rule) => Some(rule),
        Err(e) => {
            note(e.code(), path("step"), e.to_string());
            None
        }
    }
}
