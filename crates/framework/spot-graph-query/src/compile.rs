//! Compiles the query's edge selectors into storage edge specs, through the graph model.

use spot_graph_model::{EdgeSource, EdgeType, GraphModel};
use spot_storage_contract::schema::system;
use spot_storage_contract::{Direction as StorageDirection, EdgeSpec, Name};

use crate::error::{GraphQueryError, problem};
use crate::filter;
use crate::query::{Direction, GraphQuery};

/// Whether the caller may read one property of an edge type, the field-level permission of a
/// relation DocType's fields (WP-GRA-03.T4).
///
/// The implementer is bound to the caller (it holds the principal and the rules), so the answer
/// is the caller's. It is asked only for edge types that declare properties, which are the ones
/// backed by a relation DocType; an edge type that declares none has no field to protect and
/// keeps the rule "visible when both ends are". A property it refuses is **absent** from every
/// answer, and a query that names it fails exactly as if the edge type never had it.
///
/// There is **no default answer**: an implementer must decide, so a forgotten policy is a compile
/// error, not a silent leak. Every executor takes the policy as a required argument. A production
/// implementer must refuse the sensitive properties of every edge type that has a relation
/// DocType, or they are readable by everyone who can see both ends.
pub trait EdgeFieldAccess: Send + Sync {
    /// Whether the caller may read `property` of `edge_type` (the canonical edge-type name).
    fn edge_field_readable(&self, edge_type: &str, property: &Name) -> bool;
}

/// The policy that allows every property. **Not for production**: it exists for tests, demos and
/// the unrestricted [`compile`] helper. No executor uses it unless the caller passes it.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnrestrictedEdgeFields;

impl EdgeFieldAccess for UnrestrictedEdgeFields {
    fn edge_field_readable(&self, _edge_type: &str, _property: &Name) -> bool {
        true
    }
}

/// One edge type to follow in one direction, as storage sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// The canonical edge-type name, reported on the result.
    pub name: String,
    /// The storage edge spec the step compiles to.
    pub spec: EdgeSpec,
    /// The storage direction.
    pub direction: StorageDirection,
    /// The edge properties the edge type declares **and the caller may read**, in declaration
    /// order. Empty when the edge type declares none (link, table and dynamic-link edges), in
    /// which case an unknown property is found at run time, not here.
    pub properties: Vec<Name>,
    /// The declared properties the caller may not read. They are removed from every edge's
    /// properties before anything else sees them, and a query cannot name them.
    pub hidden: Vec<Name>,
    /// For a relation edge, where its records live in storage; `None` for every other edge.
    pub relation: Option<RelationStep>,
}

/// The storage mapping of a relation edge: one record of `doctype` per edge, its `from_field`
/// holding a record of `from_entity` and its `to_field` a record of `to_entity`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationStep {
    /// The relation DocType.
    pub doctype: Name,
    /// The link field holding the edge's `from` record.
    pub from_field: Name,
    /// The link field holding the edge's `to` record.
    pub to_field: Name,
    /// The DocType of the `from` record.
    pub from_entity: Name,
    /// The DocType of the `to` record.
    pub to_entity: Name,
    /// The property whose values are summed when several lines join one pair (WP-GRA-03.T9);
    /// `None` refuses repeated lines. Executors that cannot see every line (an index) must leave
    /// such a step to the database path.
    pub aggregate_sum: Option<Name>,
}

/// Compiles every edge selector of `query` to a [`Step`], in selector order, without duplicates.
///
/// A link or dynamic-link edge `Doc.field` is the link field `field` of `Doc`. A table edge
/// `Parent_contains_Child` is the `parent` column of the child rows. A relation edge is the pair of link fields of its relation DocType's records. Multi-select
/// edges are refused until their storage mapping exists.
///
/// # Errors
///
/// [`GraphQueryError::UnknownEdgeType`] with the similar names, or
/// [`GraphQueryError::UnsupportedEdge`].
///
/// **Unrestricted**: every edge property is readable. Not for production and never called by an
/// executor; use [`compile_with`] under the caller's [`EdgeFieldAccess`].
pub fn compile(model: &GraphModel, query: &GraphQuery) -> Result<Vec<Step>, GraphQueryError> {
    compile_with(model, query, &UnrestrictedEdgeFields)
}

/// Like [`compile`], under the caller's field-level access to edge properties.
///
/// # Errors
///
/// As [`compile`]. A property `access` refuses gives the same `graphq.unknown_edge_property`, with
/// the same text, as a property the edge type never declared.
pub fn compile_with(
    model: &GraphModel,
    query: &GraphQuery,
    access: &dyn EdgeFieldAccess,
) -> Result<Vec<Step>, GraphQueryError> {
    let mut steps: Vec<Step> = Vec::new();
    for selector in &query.follow {
        let edge = model
            .edge(&selector.edge)
            .ok_or_else(|| GraphQueryError::UnknownEdgeType {
                name: selector.edge.clone(),
                similar: similar_names(model, &selector.edge),
            })?;
        let (spec, relation) = spec_of(edge)?;
        let direction = storage_direction(selector.direction.unwrap_or(query.direction));
        let declared: Vec<Name> = edge
            .properties
            .iter()
            .filter_map(|p| Name::new(&p.name).ok())
            .collect();
        let (properties, hidden): (Vec<Name>, Vec<Name>) = declared
            .into_iter()
            .partition(|p| access.edge_field_readable(&edge.name, p));
        let step = Step {
            name: edge.name.clone(),
            spec,
            direction,
            properties,
            hidden,
            relation,
        };
        if !steps.contains(&step) {
            steps.push(step);
        }
    }
    check_edge_properties(query, &steps)?;
    Ok(steps)
}

/// Refuses a query that names (in the edge filter through the edge variable, or as the rolled-up
/// property) a property that an edge type it follows declares its properties without.
fn check_edge_properties(query: &GraphQuery, steps: &[Step]) -> Result<(), GraphQueryError> {
    let mut named: Vec<Name> = Vec::new();
    if let (Some(var), Some(expr)) = (&query.edge_var, &query.edge_filter) {
        named.extend(filter::edge_refs(expr, var));
    }
    named.extend(query.rollup.iter().cloned());
    let mut problems = Vec::new();
    for step in steps
        .iter()
        .filter(|s| !s.properties.is_empty() || !s.hidden.is_empty())
    {
        for property in &named {
            if !step.properties.contains(property) {
                problems.push(problem(
                    "graphq.unknown_edge_property",
                    format!(
                        "edge type `{}` has no property `{property}`; it declares {}",
                        step.name,
                        step.properties
                            .iter()
                            .map(Name::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(GraphQueryError::Invalid(problems))
    }
}

fn unsupported(edge: &EdgeType, reason: &str) -> GraphQueryError {
    GraphQueryError::UnsupportedEdge {
        name: edge.name.clone(),
        reason: reason.to_owned(),
    }
}

fn name(edge: &EdgeType, text: &str) -> Result<Name, GraphQueryError> {
    Name::new(text).map_err(|_| unsupported(edge, "its names are not storage identifiers"))
}

fn spec_of(edge: &EdgeType) -> Result<(EdgeSpec, Option<RelationStep>), GraphQueryError> {
    match edge.source {
        EdgeSource::LinkField | EdgeSource::DynamicLink => {
            let prefix = format!("{}.", edge.from);
            let field = edge
                .name
                .strip_prefix(&prefix)
                .ok_or_else(|| unsupported(edge, "its name does not name its holding field"))?;
            Ok((
                EdgeSpec {
                    entity: name(edge, &edge.from)?,
                    field: name(edge, field)?,
                },
                None,
            ))
        }
        EdgeSource::Table => {
            let child = edge
                .to_one()
                .ok_or_else(|| unsupported(edge, "a table edge needs one child DocType"))?;
            Ok((
                EdgeSpec {
                    entity: name(edge, child)?,
                    field: name(edge, system::PARENT)?,
                },
                None,
            ))
        }
        EdgeSource::TableMultiselect => Err(unsupported(
            edge,
            "multi-select edges have no storage mapping yet",
        )),
        EdgeSource::Relation => {
            let relation = edge.relation.as_ref().ok_or_else(|| {
                unsupported(edge, "the relation edge does not name its relation DocType")
            })?;
            let to = edge
                .to_one()
                .ok_or_else(|| unsupported(edge, "a relation edge needs one target DocType"))?;
            let step = RelationStep {
                doctype: name(edge, &relation.doctype)?,
                from_field: name(edge, &relation.from_field)?,
                to_field: name(edge, &relation.to_field)?,
                from_entity: name(edge, &edge.from)?,
                to_entity: name(edge, to)?,
                aggregate_sum: relation
                    .aggregate
                    .as_ref()
                    .map(|a| name(edge, &a.property))
                    .transpose()?,
            };
            // The spec names the relation records by their `from` field, so two relation edges
            // over one DocType with different ends stay different steps.
            Ok((
                EdgeSpec {
                    entity: step.doctype.clone(),
                    field: step.from_field.clone(),
                },
                Some(step),
            ))
        }
    }
}

fn storage_direction(direction: Direction) -> StorageDirection {
    match direction {
        Direction::Out => StorageDirection::Outgoing,
        Direction::In => StorageDirection::Incoming,
        Direction::Both => StorageDirection::Both,
    }
}

/// Canonical names that look like `wanted`: one contains the other, or they are within two
/// edits. At most five, sorted.
fn similar_names(model: &GraphModel, wanted: &str) -> Vec<String> {
    let mut names: Vec<String> = model
        .edges
        .keys()
        .filter(|candidate| {
            candidate.contains(wanted)
                || wanted.contains(candidate.as_str())
                || edit_distance(candidate, wanted) <= 2
        })
        .take(5)
        .cloned()
        .collect();
    names.sort();
    names
}

/// Levenshtein distance over characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut current = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != *cb);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(0)
}
