//! Mapping messages to and from JSON documents (WP-INT-06.T1, brief item 5).
//!
//! An [`EdiMapping`] wraps the shared [`MappingSpec`] from `spot-connectors` — the same spec the
//! BPM integration node uses. A rule's `source` is an **EDI path** and its `target` is a JSON
//! path, so one spec drives both directions:
//!
//! - [`EdiMapping::to_document`] renders a message into a JSON view and applies the spec with
//!   `MappingSpec::apply`, exactly as a connector would;
//! - [`EdiMapping::from_document`] reads each rule's `target` out of a document and writes it
//!   back into the EDI path named by `source`, using a template message for the structure.
//!
//! The EDI path grammar is `segments.<TAG>.<occurrence>.<element>`, all indices 0-based, plus
//! `message.id` and `message.control`. A simple element is a string; a composite element is an
//! object of component index to string.

use serde_json::{Map, Value as Json};
use spot_connectors::{MappingRule, MappingSpec};

use crate::error::{EdiError, Result};
use crate::model::{Element, Message, Segment};

/// A message-to-document mapping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EdiMapping {
    /// The shared mapping spec: EDI path to JSON path.
    pub spec: MappingSpec,
}

impl EdiMapping {
    /// A mapping over `spec`.
    pub fn new(spec: MappingSpec) -> Self {
        Self { spec }
    }

    /// Renders `message` into a JSON view and applies the spec, returning the mapped document.
    pub fn to_document(&self, message: &Message, now_micros: i64) -> Result<Json> {
        let view = message_view(message);
        self.spec
            .apply(&view, now_micros)
            .map_err(|e| EdiError::Mapping {
                rule: "<spec>".to_owned(),
                reason: e.to_string(),
            })
    }

    /// Fills a copy of `template` from `doc` along the spec's rules, then recomputes the
    /// message trailer's segment count and control number.
    pub fn from_document(&self, doc: &Json, template: &Message) -> Result<Message> {
        let mut message = template.clone();
        for rule in &self.spec.rules {
            let value = read_path(doc, &rule.target).cloned();
            apply_rule(&mut message, rule, value)?;
        }
        fix_trailer(&mut message);
        Ok(message)
    }
}

/// Builds the JSON view of a message that EDI paths address.
///
/// Paths use identifier-only components so the shared expression language can read them:
/// `segments.<TAG>.s<occurrence>.e<element>`, with `c<component>` for a composite's components.
pub fn message_view(message: &Message) -> Json {
    let mut segments: Map<String, Json> = Map::new();
    for segment in message.all_segments() {
        let occurrences = segments
            .entry(segment.tag.clone())
            .or_insert_with(|| Json::Object(Map::new()));
        let Json::Object(by_occurrence) = occurrences else {
            continue;
        };
        let key = format!("s{}", by_occurrence.len());
        by_occurrence.insert(key, segment_view(segment));
    }
    let mut root = Map::new();
    root.insert(
        "message".to_owned(),
        Json::Object({
            let mut m = Map::new();
            m.insert("id".to_owned(), Json::String(message.id().to_owned()));
            m.insert(
                "control".to_owned(),
                Json::String(message.control().to_owned()),
            );
            m
        }),
    );
    root.insert("segments".to_owned(), Json::Object(segments));
    Json::Object(root)
}

/// The view of one segment: element index to value.
fn segment_view(segment: &Segment) -> Json {
    let mut elements = Map::new();
    for (index, element) in segment.elements.iter().enumerate() {
        elements.insert(format!("e{index}"), element_view(element));
    }
    Json::Object(elements)
}

/// The view of one element: a string for a simple element, an object for a composite or a
/// repetition.
fn element_view(element: &Element) -> Json {
    match element {
        Element::Simple(value) => Json::String(value.clone()),
        Element::Composite(parts) => Json::Object(
            parts
                .iter()
                .enumerate()
                .map(|(i, part)| (format!("c{i}"), Json::String(part.clone())))
                .collect(),
        ),
        Element::Repeated(items) => Json::Object(
            items
                .iter()
                .enumerate()
                .map(|(i, item)| (format!("c{i}"), element_view(item)))
                .collect(),
        ),
    }
}

/// Applies one rule to the message, writing `value` at the EDI path named by `rule.source`.
fn apply_rule(message: &mut Message, rule: &MappingRule, value: Option<Json>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let parts: Vec<&str> = rule.source.split('.').collect();
    match parts.as_slice() {
        ["message", "control"] => {
            let text = scalar_text(&value, rule)?;
            if message.header.tag == "UNH" {
                message.header.set(0, text);
            } else {
                message.header.set(1, text);
            }
            Ok(())
        }
        ["message", "id"] => {
            let text = scalar_text(&value, rule)?;
            if message.header.tag == "UNH" {
                message.header.set(1, text);
            } else {
                message.header.set(0, text);
            }
            Ok(())
        }
        ["segments", tag, occurrence, element] => {
            let occurrence = path_index(occurrence, 's').ok_or_else(|| EdiError::Mapping {
                rule: rule.target.clone(),
                reason: format!("the EDI path occurrence {occurrence:?} is not an index"),
            })?;
            let element = path_index(element, 'e').ok_or_else(|| EdiError::Mapping {
                rule: rule.target.clone(),
                reason: format!("the EDI path element {element:?} is not an index"),
            })?;
            let Some(segment) = nth_segment(message, tag, occurrence) else {
                return Err(EdiError::Mapping {
                    rule: rule.target.clone(),
                    reason: format!("the template has no occurrence {occurrence} of segment {tag}"),
                });
            };
            segment.elements_set(element, element_from_json(&value, rule)?);
            Ok(())
        }
        ["segments", tag, occurrence, element, component] => {
            let occurrence = path_index(occurrence, 's').ok_or_else(|| EdiError::Mapping {
                rule: rule.target.clone(),
                reason: format!("the EDI path occurrence {occurrence:?} is not an index"),
            })?;
            let element = path_index(element, 'e').ok_or_else(|| EdiError::Mapping {
                rule: rule.target.clone(),
                reason: format!("the EDI path element {element:?} is not an index"),
            })?;
            let component = path_index(component, 'c').ok_or_else(|| EdiError::Mapping {
                rule: rule.target.clone(),
                reason: format!("the EDI path component {component:?} is not an index"),
            })?;
            let Some(segment) = nth_segment(message, tag, occurrence) else {
                return Err(EdiError::Mapping {
                    rule: rule.target.clone(),
                    reason: format!("the template has no occurrence {occurrence} of segment {tag}"),
                });
            };
            set_component(
                segment.element_slot(element),
                component,
                scalar_text(&value, rule)?,
            );
            Ok(())
        }
        _ => Err(EdiError::Mapping {
            rule: rule.target.clone(),
            reason: format!("the EDI path {:?} is not recognised", rule.source),
        }),
    }
}

/// Turns a JSON value into an element: a string into a simple element, an object of component
/// index to string into a composite.
fn element_from_json(value: &Json, rule: &MappingRule) -> Result<Element> {
    match value {
        Json::Object(map) => {
            let mut parts: Vec<(usize, String)> = Vec::new();
            for (key, item) in map {
                let index = path_index(key, 'c').ok_or_else(|| EdiError::Mapping {
                    rule: rule.target.clone(),
                    reason: format!("the component index {key:?} is not an index"),
                })?;
                parts.push((index, scalar_text(item, rule)?));
            }
            parts.sort_by_key(|(index, _)| *index);
            Ok(Element::Composite(
                parts.into_iter().map(|(_, v)| v).collect(),
            ))
        }
        other => Ok(Element::Simple(scalar_text(other, rule)?)),
    }
}

/// Parses a path component like `s0`, `e2` or `c1` (or a bare number) into an index.
fn path_index(part: &str, prefix: char) -> Option<usize> {
    let digits = part.strip_prefix(prefix).unwrap_or(part);
    digits.parse().ok()
}

/// Renders a JSON scalar as EDI text.
fn scalar_text(value: &Json, rule: &MappingRule) -> Result<String> {
    Ok(match value {
        Json::Null => String::new(),
        Json::Bool(b) => b.to_string(),
        Json::String(s) => s.clone(),
        Json::Number(n) => n.to_string(),
        Json::Array(_) | Json::Object(_) => {
            return Err(EdiError::Mapping {
                rule: rule.target.clone(),
                reason: "the value is not a scalar".to_owned(),
            });
        }
    })
}

/// The `occurrence`-th (0-based) segment with `tag` in the message, header and trailer included.
fn nth_segment<'a>(
    message: &'a mut Message,
    tag: &str,
    occurrence: usize,
) -> Option<&'a mut Segment> {
    let mut seen = 0usize;
    let mut found: Option<&mut Segment> = None;
    for segment in message.all_segments_mut() {
        if segment.tag == tag {
            if seen == occurrence {
                found = Some(segment);
                break;
            }
            seen += 1;
        }
    }
    found
}

/// Recomputes the trailer's segment count and control number.
fn fix_trailer(message: &mut Message) {
    let count = message.all_segments().len().to_string();
    let control = message.control().to_owned();
    if let Some(trailer) = message.trailer.as_mut() {
        match trailer.tag.as_str() {
            "SE" => {
                trailer.set(0, count);
                trailer.set(1, control);
            }
            "UNT" => {
                trailer.set(0, count);
                trailer.set(1, control);
            }
            _ => {}
        }
    }
}

/// Reads a dotted path out of a JSON document.
pub fn read_path<'a>(doc: &'a Json, path: &str) -> Option<&'a Json> {
    let mut current = doc;
    for part in path.split('.') {
        if part.is_empty() {
            continue;
        }
        current = match current {
            Json::Object(map) => map.get(part)?,
            Json::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

impl Segment {
    /// Sets the element at `index`, replacing it with `element`.
    fn elements_set(&mut self, index: usize, element: Element) {
        while self.elements.len() <= index {
            self.elements.push(Element::Simple(String::new()));
        }
        self.elements[index] = element;
    }

    /// The element at `index`, creating empty elements up to it.
    fn element_slot(&mut self, index: usize) -> &mut Element {
        while self.elements.len() <= index {
            self.elements.push(Element::Simple(String::new()));
        }
        &mut self.elements[index]
    }
}

/// Sets one component of an element, turning a simple element into a composite as needed.
fn set_component(element: &mut Element, component: usize, value: String) {
    match element {
        Element::Composite(parts) => {
            while parts.len() <= component {
                parts.push(String::new());
            }
            parts[component] = value;
        }
        other => {
            let first = other.first().unwrap_or("").to_owned();
            let mut parts = vec![first];
            while parts.len() <= component {
                parts.push(String::new());
            }
            parts[component] = value;
            *other = Element::Composite(parts);
        }
    }
}

impl Message {
    /// Mutable access to every segment, header and trailer included.
    fn all_segments_mut(&mut self) -> Vec<&mut Segment> {
        let mut out = vec![&mut self.header];
        out.extend(self.segments.iter_mut());
        if let Some(trailer) = self.trailer.as_mut() {
            out.push(trailer);
        }
        out
    }
}
