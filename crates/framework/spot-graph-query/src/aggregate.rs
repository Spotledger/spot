//! Merging the lines of one pair of records into one relation edge (WP-GRA-03.T9).
//!
//! A relation edge type joins two records through one relation record. When two or more records
//! join the same pair, the edge type decides what that means: by default the pair is ambiguous
//! (`graphq.ambiguous_relation_edge`); an edge type that declares `aggregate = sum` on a numeric
//! property treats the lines as one edge whose property is their sum. [`merge_lines`] is the one
//! function every executor uses, so the database path and the index path cannot disagree.
//!
//! The lines given are the lines **the caller may read**: a line the caller cannot see is not in
//! the list, so the total is the total of what the caller can see, the same rule that hides an
//! edge to a hidden record.

use spot_storage_contract::{Name, Value};

use crate::error::GraphQueryError;
use crate::exec::{EdgeProps, exact_sum};

/// The edge of one pair: its properties as the caller may read them.
///
/// `lines` are the readable property sets of the relation records joining the pair, never empty.
/// `aggregate_sum` is the property to sum, when the edge type declares one.
///
/// - One line is the edge, unchanged.
/// - Several lines, no declaration: `graphq.ambiguous_relation_edge`.
/// - Several lines with a declaration: every other property must be equal on all lines, else
///   `graphq.ambiguous_relation_edge` naming that property. The declared property is the checked
///   sum of the lines' numbers (a null or absent value adds nothing; the property is absent when
///   no line has a number); an overflow is `graphq.edge_value_overflow`. A value that is not a
///   number (the model refuses such a declaration, so only a hand-built one) is refused as
///   ambiguous, naming the property.
///
/// # Errors
///
/// As above.
pub fn merge_lines(
    edge: &str,
    aggregate_sum: Option<&Name>,
    lines: &[EdgeProps],
) -> Result<EdgeProps, GraphQueryError> {
    let Some((first, rest)) = lines.split_first() else {
        return Ok(EdgeProps::new());
    };
    if rest.is_empty() {
        return Ok(first.clone());
    }
    let ambiguous = |property: Option<&Name>| GraphQueryError::AmbiguousRelationEdge {
        edge: edge.to_owned(),
        count: lines.len(),
        property: property.map(|p| p.as_str().to_owned()),
    };
    let Some(sum_property) = aggregate_sum else {
        return Err(ambiguous(None));
    };
    // Every other property must agree on all lines; the first difference (in name order, over
    // the union of names so a property missing on one line counts) is named.
    let mut names: Vec<&Name> = lines.iter().flat_map(|line| line.keys()).collect();
    names.sort();
    names.dedup();
    for name in names.iter().copied().filter(|n| *n != sum_property) {
        let value = first.get(name);
        if rest.iter().any(|line| line.get(name) != value) {
            return Err(ambiguous(Some(name)));
        }
    }
    let mut merged = first.clone();
    merged.remove(sum_property);
    let mut total: Option<Value> = None;
    for line in lines {
        match line.get(sum_property) {
            None | Some(Value::Null) => {}
            Some(value @ (Value::Int(_) | Value::Decimal(_))) => {
                total = Some(match total {
                    None => value.clone(),
                    Some(so_far) => exact_sum(&so_far, value).ok_or_else(|| {
                        GraphQueryError::EdgeValueOverflow {
                            edge: edge.to_owned(),
                            property: sum_property.as_str().to_owned(),
                        }
                    })?,
                });
            }
            Some(_) => return Err(ambiguous(Some(sum_property))),
        }
    }
    if let Some(total) = total {
        merged.insert(sum_property.clone(), total);
    }
    Ok(merged)
}
