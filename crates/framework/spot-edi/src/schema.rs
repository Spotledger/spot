//! The lightweight schema layer (WP-INT-06.T1, brief item 4).
//!
//! A schema is data: an ordered list of segment rules, each with a minimum and maximum number
//! of occurrences and, for every element position, a name, a type and its own minimum and
//! maximum. Composite elements carry component rules. The definitions are our own minimal
//! ones — `schemas/x12/*.toml` and `schemas/edifact/*.toml` — written from the public structure
//! of the documents, never copied from a licensed implementation guide.
//!
//! [`validate`] walks a message's segments against a schema and reports **every** problem as a
//! [`Violation`]: a required segment that is absent, a segment that is not allowed where it
//! appears, a segment that occurs too often, and an element that is missing, too long or of the
//! wrong type. Matching is ordered and greedy (a rule is closed when the next segment cannot use
//! it), which is enough for the acknowledgement-style checks the adapter needs.

use serde::{Deserialize, Serialize};

use crate::delimiters::Syntax;
use crate::error::codes::{
    SCHEMA_BAD_ELEMENT, SCHEMA_MISSING_SEGMENT, SCHEMA_TOO_MANY, SCHEMA_UNEXPECTED_SEGMENT,
};
use crate::error::{EdiError, Result};
use crate::model::{Element, Message, Segment};
use crate::report::{Position, Violation};

/// The type an element's text must satisfy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElementKind {
    /// Any text.
    Text,
    /// ASCII letters and digits only.
    Identifier,
    /// An optionally signed whole number.
    Numeric,
    /// An optionally signed decimal number.
    Decimal,
    /// A six- or eight-digit date.
    Date,
    /// A four- to eight-digit time.
    Time,
    /// One of a fixed list of values.
    Enum,
}

impl ElementKind {
    /// The name used in messages and schema files.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Identifier => "identifier",
            Self::Numeric => "numeric",
            Self::Decimal => "decimal",
            Self::Date => "date",
            Self::Time => "time",
            Self::Enum => "enum",
        }
    }
}

/// One element position's rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementRule {
    /// The element's name, for reports.
    pub name: String,
    /// The type its text must satisfy.
    pub kind: ElementKind,
    /// The minimum number of values (1 means required, 0 means optional).
    #[serde(default)]
    pub min: u32,
    /// The maximum number of values; 0 means no maximum.
    #[serde(default)]
    pub max: u32,
    /// The allowed values, for [`ElementKind::Enum`].
    #[serde(default)]
    pub values: Vec<String>,
    /// The component rules, for a composite element.
    #[serde(default)]
    pub component: Vec<ElementRule>,
}

/// One segment position's rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRule {
    /// The segment tag (`BEG`, `N1`, ...).
    pub tag: String,
    /// The minimum number of occurrences (1 means required).
    #[serde(default)]
    pub min: u32,
    /// The maximum number of occurrences; 0 means no maximum.
    #[serde(default)]
    pub max: u32,
    /// The element rules, by position.
    #[serde(default)]
    pub element: Vec<ElementRule>,
}

/// A message definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    /// The schema's id (`x12_850`, `edifact_orders`, ...).
    pub id: String,
    /// The syntax it describes.
    pub syntax: Syntax,
    /// The version it was written against.
    pub version: String,
    /// A short human title.
    #[serde(default)]
    pub title: String,
    /// The X12 functional group code or EDIFACT message type, when known.
    #[serde(default)]
    pub functional_group: Option<String>,
    /// The ordered segment rules.
    #[serde(default, rename = "segment")]
    pub segments: Vec<SegmentRule>,
}

impl Schema {
    /// Parses a schema from TOML text.
    pub fn from_toml(id: &str, text: &str) -> Result<Self> {
        let schema: Schema = toml::from_str(text).map_err(|e| EdiError::SchemaBad {
            id: id.to_owned(),
            reason: e.to_string(),
        })?;
        schema.check()?;
        Ok(schema)
    }

    /// Checks the schema's own consistency.
    pub fn check(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(EdiError::SchemaBad {
                id: self.id.clone(),
                reason: "the id is empty".into(),
            });
        }
        for segment in &self.segments {
            if segment.tag.trim().is_empty() {
                return Err(EdiError::SchemaBad {
                    id: self.id.clone(),
                    reason: "a segment rule has an empty tag".into(),
                });
            }
            if segment.max != 0 && segment.min > segment.max {
                return Err(EdiError::SchemaBad {
                    id: self.id.clone(),
                    reason: format!(
                        "segment {} has min {} over max {}",
                        segment.tag, segment.min, segment.max
                    ),
                });
            }
            for element in &segment.element {
                check_element_rule(&self.id, element)?;
            }
        }
        Ok(())
    }

    /// The rule for `tag`, when the schema names it.
    pub fn rule_for(&self, tag: &str) -> Option<&SegmentRule> {
        self.segments.iter().find(|r| r.tag == tag)
    }

    /// Loads an embedded schema by id.
    pub fn load(id: &str) -> Result<Self> {
        match embedded_source(id) {
            Some((id, text)) => Schema::from_toml(id, text),
            None => Err(EdiError::SchemaUnknown { id: id.to_owned() }),
        }
    }

    /// The ids of every embedded schema.
    pub fn embedded_ids() -> Vec<&'static str> {
        EMBEDDED.iter().map(|(id, _)| *id).collect()
    }
}

fn check_element_rule(id: &str, rule: &ElementRule) -> Result<()> {
    if rule.max != 0 && rule.min > rule.max {
        return Err(EdiError::SchemaBad {
            id: id.to_owned(),
            reason: format!(
                "element {} has min {} over max {}",
                rule.name, rule.min, rule.max
            ),
        });
    }
    if rule.kind == ElementKind::Enum && rule.values.is_empty() {
        return Err(EdiError::SchemaBad {
            id: id.to_owned(),
            reason: format!("element {} is an enum with no values", rule.name),
        });
    }
    for component in &rule.component {
        check_element_rule(id, component)?;
    }
    Ok(())
}

/// The embedded schema documents, compiled into the binary.
static EMBEDDED: &[(&str, &str)] = &[
    ("x12_850", include_str!("../schemas/x12/850.toml")),
    ("x12_810", include_str!("../schemas/x12/810.toml")),
    ("x12_856", include_str!("../schemas/x12/856.toml")),
    ("x12_997", include_str!("../schemas/x12/997.toml")),
    (
        "edifact_orders",
        include_str!("../schemas/edifact/orders.toml"),
    ),
    (
        "edifact_invoic",
        include_str!("../schemas/edifact/invoic.toml"),
    ),
    (
        "edifact_desadv",
        include_str!("../schemas/edifact/desadv.toml"),
    ),
    (
        "edifact_contrl",
        include_str!("../schemas/edifact/contrl.toml"),
    ),
];

fn embedded_source(id: &str) -> Option<(&'static str, &'static str)> {
    EMBEDDED.iter().copied().find(|(name, _)| *name == id)
}

/// Validates `message` against `schema`, reporting every violation (empty when it conforms).
///
/// Positions are relative to the message (its header is segment 0).
pub fn validate(message: &Message, schema: &Schema) -> Vec<Violation> {
    let segments = message.all_segments();
    validate_segments(&segments, schema)
}

/// Validates a segment list against `schema`; positions are 0-based indices into `segments`.
///
/// Matching is ordered: a segment is taken by the nearest rule at or after the current one whose
/// tag matches and whose maximum is not yet reached; when no forward rule fits, a **loop** back
/// to an earlier repeating rule is allowed (an `HL` loop, say). A segment that fits no rule is
/// reported as unexpected, or as too many when a rule of that tag is already full.
pub fn validate_segments(segments: &[&Segment], schema: &Schema) -> Vec<Violation> {
    let mut out = Vec::new();
    let rules = &schema.segments;
    let mut current = 0usize;
    let mut counts = vec![0u64; rules.len()];
    for (index, segment) in segments.iter().enumerate() {
        let mut target: Option<(usize, bool)> = None;
        for r in current..rules.len() {
            if rules[r].tag == segment.tag && counts[r] < effective_max(rules[r].max) {
                target = Some((r, false));
                break;
            }
        }
        if target.is_none() {
            for r in (0..current).rev() {
                if rules[r].tag == segment.tag && counts[r] < effective_max(rules[r].max) {
                    target = Some((r, true));
                    break;
                }
            }
        }
        match target {
            Some((r, backward)) => {
                if !backward {
                    for k in current..r {
                        if counts[k] < u64::from(rules[k].min) {
                            out.push(Violation::new(
                                SCHEMA_MISSING_SEGMENT,
                                Position::segment(index, segment),
                                format!(
                                    "segment {} must appear at least {} time(s) before {}",
                                    rules[k].tag, rules[k].min, segment.tag
                                ),
                            ));
                        }
                    }
                    current = r;
                } else {
                    current = r + 1;
                }
                counts[r] += 1;
                check_elements(index, segment, &rules[r], &mut out);
            }
            None => {
                let position = Position::segment(index, segment);
                match (current..rules.len()).find(|&r| rules[r].tag == segment.tag) {
                    Some(r) => out.push(Violation::new(
                        SCHEMA_TOO_MANY,
                        position,
                        format!(
                            "segment {} occurs more than {} time(s)",
                            segment.tag, rules[r].max
                        ),
                    )),
                    None => out.push(Violation::new(
                        SCHEMA_UNEXPECTED_SEGMENT,
                        position,
                        format!("segment {} is not allowed here", segment.tag),
                    )),
                }
            }
        }
    }
    for k in current..rules.len() {
        if counts[k] < u64::from(rules[k].min) {
            let position = match segments.last() {
                Some(segment) => Position {
                    segment: segments.len(),
                    tag: rules[k].tag.clone(),
                    element: None,
                    component: None,
                    offset: segment.offset,
                },
                None => Position {
                    segment: 0,
                    tag: rules[k].tag.clone(),
                    element: None,
                    component: None,
                    offset: 0,
                },
            };
            out.push(Violation::new(
                SCHEMA_MISSING_SEGMENT,
                position,
                format!(
                    "segment {} must appear at least {} time(s)",
                    rules[k].tag, rules[k].min
                ),
            ));
        }
    }
    out
}

/// Validates every message of `interchange` against `schema`, concatenating the violations.
pub fn validate_interchange(
    interchange: &crate::model::Interchange,
    schema: &Schema,
) -> Vec<Violation> {
    let mut out = Vec::new();
    for message in interchange.messages() {
        out.extend(validate(&message, schema));
    }
    out
}

fn effective_max(max: u32) -> u64 {
    if max == 0 { u64::MAX } else { u64::from(max) }
}

/// Checks one segment's elements against its rule.
fn check_elements(index: usize, segment: &Segment, rule: &SegmentRule, out: &mut Vec<Violation>) {
    let count = segment.elements.len().max(rule.element.len());
    for position in 0..count {
        let Some(element_rule) = rule.element.get(position) else {
            continue;
        };
        let element = segment.elements.get(position);
        let value = element.and_then(Element::first).unwrap_or("");
        if value.is_empty() {
            if element_rule.min > 0 {
                out.push(Violation::new(
                    SCHEMA_BAD_ELEMENT,
                    Position::element(index, segment, position),
                    format!(
                        "segment {} requires element {} ({})",
                        rule.tag,
                        position + 1,
                        element_rule.name
                    ),
                ));
            }
            continue;
        }
        if element_rule.component.is_empty() {
            if !kind_ok(element_rule, value) {
                out.push(Violation::new(
                    SCHEMA_BAD_ELEMENT,
                    Position::element(index, segment, position),
                    format!(
                        "element {} ({}) is {:?}, not {}",
                        position + 1,
                        element_rule.name,
                        value,
                        element_rule.kind.name()
                    ),
                ));
            }
        } else {
            check_components(index, segment, position, element, element_rule, out);
        }
    }
}

/// Checks a composite element's components.
fn check_components(
    index: usize,
    segment: &Segment,
    position: usize,
    element: Option<&Element>,
    rule: &ElementRule,
    out: &mut Vec<Violation>,
) {
    let components = element.map(Element::components).unwrap_or_default();
    let count = components.len().max(rule.component.len());
    for component in 0..count {
        let Some(component_rule) = rule.component.get(component) else {
            continue;
        };
        let value = components.get(component).copied().unwrap_or("");
        if value.is_empty() {
            if component_rule.min > 0 {
                out.push(Violation::new(
                    SCHEMA_BAD_ELEMENT,
                    Position::component(index, segment, position, component),
                    format!(
                        "element {} ({}) requires component {} ({})",
                        position + 1,
                        rule.name,
                        component + 1,
                        component_rule.name
                    ),
                ));
            }
            continue;
        }
        if !kind_ok(component_rule, value) {
            out.push(Violation::new(
                SCHEMA_BAD_ELEMENT,
                Position::component(index, segment, position, component),
                format!(
                    "component {} ({}) is {:?}, not {}",
                    component + 1,
                    component_rule.name,
                    value,
                    component_rule.kind.name()
                ),
            ));
        }
    }
}

/// Whether `value` satisfies `rule`'s type.
fn kind_ok(rule: &ElementRule, value: &str) -> bool {
    match rule.kind {
        ElementKind::Text => true,
        ElementKind::Identifier => is_identifier(value),
        ElementKind::Numeric => is_integer(value),
        ElementKind::Decimal => is_decimal(value),
        ElementKind::Date => is_digits(value, 6, 8),
        ElementKind::Time => is_digits(value, 4, 8),
        ElementKind::Enum => rule.values.iter().any(|allowed| allowed == value),
    }
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

fn is_integer(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn is_decimal(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    let mut parts = digits.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some() {
        return false;
    }
    let whole_ok = !whole.is_empty() && whole.chars().all(|c| c.is_ascii_digit());
    match fraction {
        None => whole_ok,
        Some(frac) => {
            let frac_ok = !frac.is_empty() && frac.chars().all(|c| c.is_ascii_digit());
            frac_ok && (whole_ok || whole.is_empty())
        }
    }
}

fn is_digits(value: &str, min: usize, max: usize) -> bool {
    value.len() >= min && value.len() <= max && value.chars().all(|c| c.is_ascii_digit())
}
