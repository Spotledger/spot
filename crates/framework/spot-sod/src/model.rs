//! The segregation-of-duties model: duties, roles, rules over them, assignments (ADR-070
//! sections 2 and 3) with their scope, and the role catalogue, all serde and TOML loadable.
//!
//! A **duty** is a named, reviewable bundle of grants matching part of a business process
//! (ADR-070 section 2, rule 1); it is an opaque name here — this crate never looks inside a
//! duty. A **role** is a named set of duties (plus direct grants), expanded through a
//! [`RoleCatalogue`]. A rule says: no user may hold at least one member of side A **and** at
//! least one member of side B — in the same scope ([`ScopeMode::SameScope`]) or in any scope
//! ([`ScopeMode::AnyScope`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::SodError;

/// A named duty: the unit segregation-of-duties rules reason about (ADR-070 section 2, rule
/// 1), for example `Maintain suppliers` or `Approve payments`.
///
/// Duties are opaque names here. Non-empty and trimmed on construction.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Duty(String);

impl Duty {
    /// The duty named `text`, trimmed; `None` when it is empty or blank.
    #[must_use]
    pub fn new(text: &str) -> Option<Duty> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(Duty(trimmed.to_owned()))
        }
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A named role (ADR-070 section 2, rule 2): a set of duties, expanded to them through a
/// [`RoleCatalogue`]. Non-empty and trimmed on construction.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Role(String);

impl Role {
    /// The role named `text`, trimmed; `None` when it is empty or blank.
    #[must_use]
    pub fn new(text: &str) -> Option<Role> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(Role(trimmed.to_owned()))
        }
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How serious one violation of a rule is. Ordered from [`Severity::Low`] (least) to
/// [`Severity::Critical`] (most), so a sort by severity descending is a plain sort by this
/// enum reversed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// A hygiene finding; the review notes it.
    Low,
    /// A real separation concern; the review should resolve it.
    Medium,
    /// A separation concern flagged with emphasis at grant time.
    High,
    /// A breach an auditor would treat as a finding; granting one needs an override.
    Critical,
}

impl Severity {
    /// The stable text form used in reports and reason text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    /// Parses the snake_case text form (`as_str`'s inverse).
    #[must_use]
    pub fn parse(text: &str) -> Option<Severity> {
        match text {
            "low" => Some(Severity::Low),
            "medium" => Some(Severity::Medium),
            "high" => Some(Severity::High),
            "critical" => Some(Severity::Critical),
            _ => None,
        }
    }
}

/// Which scopes a rule compares.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMode {
    /// The conflict counts only when the duties are held in the same scope (the same
    /// company, say). Two assignments in different scopes never conflict under this rule.
    #[default]
    SameScope,
    /// The conflict counts whatever the scopes: two unscoped holds conflict, an unscoped and
    /// a scoped hold conflict too.
    AnyScope,
}

/// One side of a [`SodRule`]: the duties that satisfy it, plus the roles a rule may name
/// **for convenience**.
///
/// A rule is violated when the user holds **at least one duty from every side**. A side may
/// also name roles directly ("over roles for convenience"): a user **assigned** such a role
/// satisfies that side through the role itself, even when the catalogue does not expand the
/// role to the side's duties.
///
/// In TOML a side is `duties = [...]`, `roles = [...]` or both.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Side {
    /// The duties of this side: the rule is violated when the user holds at least one duty
    /// of every side.
    #[serde(default)]
    pub duties: Vec<Duty>,
    /// Roles that satisfy this side for convenience: a user assigned such a role holds the
    /// side even when the catalogue does not expand the role to these duties.
    #[serde(default)]
    pub roles: Vec<Role>,
}

impl Side {
    /// Whether the side names no duty and no role.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.duties.is_empty() && self.roles.is_empty()
    }

    /// Whether the side names the duty `duty` in its own duty list (a role named on the side
    /// counts for itself; expanding it is the catalogue's job, not the side's).
    #[must_use]
    pub fn names_duty(&self, duty: &Duty) -> bool {
        self.duties.contains(duty)
    }

    /// Whether the side names the role `role` directly.
    #[must_use]
    pub fn names_role(&self, role: &Role) -> bool {
        self.roles.contains(role)
    }
}

/// What one satisfying hold is: a duty of the side's duty list, or a role of the side's role
/// list held by assignment.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    /// A duty of the side's duty list.
    Duty {
        /// The duty held.
        duty: Duty,
    },
    /// A role of the side's role list, held by assignment.
    Role {
        /// The role held.
        role: Role,
    },
}

/// How the user came to hold what satisfies a side: assigned directly, or through a role
/// assignment the catalogue expands.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// The duty (or role) is assigned to the user directly.
    Direct,
    /// The duty comes through the role `role`, assigned to the user and expanded by the
    /// catalogue.
    ThroughRole {
        /// The role assignment the duty came through.
        role: Role,
    },
}

/// One entry of a [`Conflict`]'s `held`: what satisfies one side, held how, in which scope.
///
/// The reason is part of the result, never reconstructed later (the brief, point 2). One
/// entry per satisfied side; for a two-sided rule, two entries.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct HeldDuty {
    /// Which side (0-based, in the rule's `sides` order) this hold satisfies.
    pub side: usize,
    /// What the hold is: the side duty held, or the side role held.
    pub hold: Hold,
    /// How the user holds it: directly, or through which role.
    pub via: Via,
    /// The scope the satisfying assignment carries.
    pub scope: Option<String>,
}

/// One segregation-of-duties rule: no user may hold at least one member of side one **and**
/// at least one member of side two, within the rule's [`ScopeMode`] (ADR-070 section 8, rule
/// 1: "no user may hold duty X and duty Y in the same scope").
///
/// Rules sit over duties and, for convenience, over roles (a side may name roles directly).
/// More than two sides are allowed: a violation satisfies **every** side. The loader rejects
/// fewer than two sides, an empty side, a duty shared between sides, a duplicate id and an
/// unknown severity — the rule id rides in every error.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SodRule {
    /// The stable rule id, for example `sod.purchase-approval` — the id an override names.
    pub id: String,
    /// A short human title, shown in the access review.
    pub title: String,
    /// What separating the duties protects, shown wherever the rule is reported.
    pub description: String,
    /// How serious one violation is.
    pub severity: Severity,
    /// The sides: at least two, none empty, no duty on two sides.
    pub sides: Vec<Side>,
    /// Whether the conflicting duties must share a scope.
    #[serde(default)]
    pub scope: ScopeMode,
}

/// A violation of one [`SodRule`] by one user: which rule, how severe, in which scope, and
/// how every side is held.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Conflict {
    /// The id of the rule violated.
    pub rule_id: String,
    /// The severity of the rule violated (a copy, so a consumer never looks it up).
    pub severity: Severity,
    /// The user holding the conflicting combination.
    pub user: String,
    /// The scope the conflict sits in: the shared scope for a `same_scope` rule, `None` for
    /// an `any_scope` rule.
    pub scope: Option<String>,
    /// How every side is held, one entry per side, in the rule's side order.
    pub held: Vec<HeldDuty>,
    /// Whether an active override already covers this (rule, user, scope). A covered
    /// conflict is still reported — the access review must show it — but flagged.
    pub overridden: bool,
}

/// The loaded, checked rule set the analysis evaluates. Load with [`RuleSet::from_toml`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawRules")]
pub struct RuleSet {
    /// Every rule, sorted by id, so equal sets serialise and analyse the same.
    pub rules: Vec<SodRule>,
}

impl RuleSet {
    /// The rule named `id`, when the set holds it.
    #[must_use]
    pub fn rule(&self, id: &str) -> Option<&SodRule> {
        self.rules.iter().find(|r| r.id == id)
    }

    /// The rules, sorted by id.
    #[must_use]
    pub fn as_slice(&self) -> &[SodRule] {
        &self.rules
    }

    /// Validates a rule-set TOML text.
    ///
    /// # Errors
    ///
    /// [`SodError`] carrying a stable `sod.*` code and the rule id: parse failures (bad TOML
    /// or an unknown key), a duplicate rule id, fewer than two sides, an empty side, a duty
    /// on two sides, an unknown severity, or a blank id/title/description/duty/role.
    pub fn from_toml(text: &str) -> Result<RuleSet, SodError> {
        if text.trim().is_empty() {
            return Ok(RuleSet { rules: vec![] });
        }
        let raw: RawRules = toml::from_str(text).map_err(|e| SodError::Parse {
            at: "rules".to_owned(),
            detail: e.to_string(),
        })?;
        RuleSet::build(raw.rules)
    }

    /// Validates every row, catches duplicate ids across rows, sorts by id.
    fn build(rows: Vec<Row>) -> Result<RuleSet, SodError> {
        let mut rules: Vec<SodRule> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for row in rows {
            let rule = validate_rule(row)?;
            if !seen.insert(rule.id.clone()) {
                return Err(SodError::Duplicate {
                    rule: rule.id.clone(),
                    detail: "the rule id is already in the set".to_owned(),
                });
            }
            rules.push(rule);
        }
        rules.sort();
        Ok(RuleSet { rules })
    }
}

impl TryFrom<RawRules> for RuleSet {
    type Error = SodError;

    fn try_from(raw: RawRules) -> Result<Self, Self::Error> {
        RuleSet::build(raw.rules)
    }
}

/// The TOML file shape of the rule set, before validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRules {
    /// The `[[rules]]` entries.
    rules: Vec<Row>,
}

/// The TOML row shape the loader reads before a [`SodRule`] validates: `severity` and
/// `scope` as text, so an unknown value is this crate's error naming the rule id, not a
/// generic serde message.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct Row {
    id: String,
    title: String,
    description: String,
    severity: String,
    sides: Vec<RawSide>,
    #[serde(default)]
    scope: Option<String>,
}

/// The raw side row the loader reads.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct RawSide {
    #[serde(default)]
    duties: Vec<String>,
    #[serde(default)]
    roles: Vec<String>,
}

/// Validates one raw row into a [`SodRule`]; the first problem found is returned.
fn validate_rule(row: Row) -> Result<SodRule, SodError> {
    let id = row.id.trim().to_owned();
    if id.is_empty() {
        return Err(SodError::Name {
            rule: "(rule)".to_owned(),
            detail: "the rule `id` must not be empty".to_owned(),
        });
    }
    if row.title.trim().is_empty() {
        return Err(SodError::Name {
            rule: id.clone(),
            detail: "the rule `title` must not be empty".to_owned(),
        });
    }
    if row.description.trim().is_empty() {
        return Err(SodError::Name {
            rule: id.clone(),
            detail: "the rule `description` must not be empty".to_owned(),
        });
    }
    let severity_text = row.severity.trim();
    let Some(severity) = Severity::parse(severity_text) else {
        return Err(SodError::Severity {
            rule: id.clone(),
            detail: format!(
                "`{severity_text}` is not a severity: use low, medium, high or critical"
            ),
        });
    };
    let scope = match row.scope.as_deref().map(str::trim) {
        None | Some("") | Some("same_scope") => ScopeMode::SameScope,
        Some("any_scope") => ScopeMode::AnyScope,
        Some(other) => {
            return Err(SodError::Parse {
                at: id.clone(),
                detail: format!("`{other}` is not a scope mode: use same_scope or any_scope"),
            });
        }
    };
    if row.sides.len() < 2 {
        return Err(SodError::SideCount {
            rule: id.clone(),
            detail: format!(
                "a rule needs at least two sides; this one has {}",
                row.sides.len()
            ),
        });
    }
    let mut sides: Vec<Side> = Vec::new();
    for (n, raw) in row.sides.into_iter().enumerate() {
        let mut side = Side::default();
        for text in &raw.duties {
            let Some(duty) = Duty::new(text) else {
                return Err(SodError::Name {
                    rule: id.clone(),
                    detail: format!("side {n} names an empty duty"),
                });
            };
            side.duties.push(duty);
        }
        for text in &raw.roles {
            let Some(role) = Role::new(text) else {
                return Err(SodError::Name {
                    rule: id.clone(),
                    detail: format!("side {n} names an empty role"),
                });
            };
            side.roles.push(role);
        }
        if side.duties.is_empty() {
            return Err(SodError::SideEmpty {
                rule: id.clone(),
                detail: format!(
                    "side {n} names no duty: a side is a set of duties (one or more); \
                     roles ride on top, they do not replace them"
                ),
            });
        }
        sides.push(side);
    }
    // No duty on two sides of the same rule: a user holding that duty holds both sides, so
    // the rule could never separate anything.
    for (i, side) in sides.iter().enumerate() {
        for duty in &side.duties {
            for (j, other) in sides.iter().enumerate() {
                if i != j && other.names_duty(duty) {
                    return Err(SodError::Overlap {
                        rule: id.clone(),
                        detail: format!("duty `{}` is on sides {i} and {j}", duty.as_str()),
                    });
                }
            }
        }
    }
    Ok(SodRule {
        id,
        title: row.title.trim().to_owned(),
        description: row.description.trim().to_owned(),
        severity,
        sides,
        scope,
    })
}

/// The role catalogue: which duties each role expands to (ADR-070 section 2, rule 2).
///
/// Keys are role names, values their duties. A role missing from the catalogue expands to
/// **nothing**: an assignment naming it holds only the role itself, which still satisfies a
/// side when the rule names the role directly. Load with [`RoleCatalogue::from_toml`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCatalogue {
    /// Role name to its duties, sorted by role name with duties sorted inside (`BTreeMap`
    /// keeps role order; `new` and the loader sort the duty lists), so equal catalogues
    /// serialise the same.
    pub roles: BTreeMap<Role, Vec<Duty>>,
}

impl RoleCatalogue {
    /// A catalogue from role-to-duties entries, deduplicated and sorted.
    #[must_use]
    pub fn new(entries: impl IntoIterator<Item = (Role, Vec<Duty>)>) -> RoleCatalogue {
        let map: BTreeMap<Role, std::collections::BTreeSet<Duty>> = entries
            .into_iter()
            .map(|(role, duties)| (role, duties.into_iter().collect()))
            .collect();
        RoleCatalogue {
            roles: map
                .into_iter()
                .map(|(role, duties)| (role, duties.into_iter().collect()))
                .collect(),
        }
    }

    /// The duties `role` expands to, empty when the catalogue does not know the role.
    #[must_use]
    pub fn duties_of(&self, role: &Role) -> Vec<Duty> {
        self.roles.get(role).cloned().unwrap_or_default()
    }

    /// Whether the catalogue knows the role.
    #[must_use]
    pub fn knows(&self, role: &Role) -> bool {
        self.roles.contains_key(role)
    }

    /// Every (duty, role) pair of the catalogue, sorted; the expansion the analysis uses.
    #[must_use]
    pub fn pairs(&self) -> Vec<(Duty, Role)> {
        self.roles
            .iter()
            .flat_map(|(role, duties)| duties.iter().map(move |duty| (duty.clone(), role.clone())))
            .collect()
    }

    /// Validates a catalogue TOML text: `[[roles]]` rows, each a `name` plus `duties`.
    ///
    /// # Errors
    ///
    /// [`SodError`] with a stable `sod.*` code: parse failures, a blank role name, a blank
    /// duty, or a role named twice.
    pub fn from_toml(text: &str) -> Result<RoleCatalogue, SodError> {
        if text.trim().is_empty() {
            return Ok(RoleCatalogue::default());
        }
        let raw: RawCatalogue = toml::from_str(text).map_err(|e| SodError::Parse {
            at: "catalogue".to_owned(),
            detail: e.to_string(),
        })?;
        let mut map = BTreeMap::new();
        for (i, row) in raw.roles.into_iter().enumerate() {
            let Some(role) = Role::new(&row.name) else {
                return Err(SodError::Name {
                    rule: format!("catalogue.roles[{i}]"),
                    detail: "a catalogue `name` must not be empty".to_owned(),
                });
            };
            let mut duties: Vec<Duty> = Vec::new();
            for text in row.duties {
                let Some(duty) = Duty::new(&text) else {
                    return Err(SodError::Name {
                        rule: format!("catalogue.roles[{i}]"),
                        detail: format!("role `{}` names an empty duty", role.as_str()),
                    });
                };
                duties.push(duty);
            }
            duties.sort();
            duties.dedup();
            if map.insert(role.clone(), duties).is_some() {
                return Err(SodError::Duplicate {
                    rule: format!("catalogue.roles[{i}]"),
                    detail: format!("role `{}` is named twice in the catalogue", role.as_str()),
                });
            }
        }
        Ok(RoleCatalogue { roles: map })
    }
}

/// The TOML file shape of the role catalogue, before validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalogue {
    /// The `[[roles]]` entries.
    roles: Vec<RawCatalogueRole>,
}

/// The raw row shape of one catalogue role.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct RawCatalogueRole {
    name: String,
    #[serde(default)]
    duties: Vec<String>,
}

/// A duty or a role an assignment grants: the TOML discriminates by which key is present,
/// the Rust side carries the choice as an enum.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    /// The assignment grants the duty directly.
    Duty(Duty),
    /// The assignment grants a role; the catalogue expands it to duties.
    Role(Role),
}

/// One assignment of a duty or a role to a user with its own scope (ADR-070 section 3, rule
/// 1: the unit of access is the assignment, and scope belongs to it, never to the user).
///
/// In TOML an assignment is `{ user, duty, scope }` or `{ user, role, scope }`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "AssignmentRow")]
pub struct Assignment {
    /// Who holds the grant.
    pub user: String,
    /// The duty held directly or the role held (expanded through the catalogue).
    pub grant: Grant,
    /// The assignment's scope value, when the assignment is scoped at all.
    #[serde(default)]
    pub scope: Option<String>,
}

/// The raw assignment row: `duty` **or** `role`, with `user` and the optional `scope`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct AssignmentRow {
    user: String,
    #[serde(default)]
    duty: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

impl TryFrom<AssignmentRow> for Assignment {
    type Error = SodError;

    fn try_from(row: AssignmentRow) -> Result<Self, Self::Error> {
        let user = row.user.trim().to_owned();
        if user.is_empty() {
            return Err(SodError::Name {
                rule: "(assignment)".to_owned(),
                detail: "an assignment `user` must not be empty".to_owned(),
            });
        }
        let duty = row.duty.as_deref().map(str::trim).filter(|d| !d.is_empty());
        let role = row.role.as_deref().map(str::trim).filter(|r| !r.is_empty());
        let grant = match (duty, role) {
            (Some(d), None) => Grant::Duty(Duty(d.to_owned())),
            (None, Some(r)) => Grant::Role(Role(r.to_owned())),
            (Some(_), Some(_)) => {
                return Err(SodError::Parse {
                    at: "(assignment)".to_owned(),
                    detail: format!("user `{user}`: name a duty or a role, not both"),
                });
            }
            (None, None) => {
                return Err(SodError::Parse {
                    at: "(assignment)".to_owned(),
                    detail: format!("user `{user}`: name a duty or a role"),
                });
            }
        };
        let scope = row.scope.filter(|s| !s.trim().is_empty());
        Ok(Assignment { user, grant, scope })
    }
}

impl Assignment {
    /// A direct duty assignment.
    #[must_use]
    pub fn duty(user: &str, duty: &str, scope: Option<&str>) -> Assignment {
        Assignment {
            user: user.to_owned(),
            grant: Grant::Duty(Duty(duty.trim().to_owned())),
            scope: scope.map(str::to_owned),
        }
    }

    /// A role assignment.
    #[must_use]
    pub fn role(user: &str, role: &str, scope: Option<&str>) -> Assignment {
        Assignment {
            user: user.to_owned(),
            grant: Grant::Role(Role(role.trim().to_owned())),
            scope: scope.map(str::to_owned),
        }
    }
}
