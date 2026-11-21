//! Walking an image along a field path, rebuilding it as we go.
//!
//! The walk is a pure function of the tree: it clones, it never mutates in place, and a path
//! that is absent from a record leaves that record exactly as it was. A record that has no
//! child rows, and a row that does not carry the field, are both ordinary cases: a personal
//! field is protected where it exists and nowhere else.
//!
//! Every visited node carries its *concrete* path: the spec's spelling with the element index
//! written out where an array was walked (`items.0.note`, not `items.*.note`). The concrete
//! path is what the associated data binds, so an envelope cannot move to another row either.

use serde_json::Value;

use crate::error::PiiError;
use crate::spec::Step;

/// The concrete path of the node being visited: `items.0.note`, `contact.email`.
#[derive(Debug, Default, Clone)]
pub(crate) struct NodePath {
    segments: Vec<String>,
}

impl NodePath {
    /// The path with element indices written out.
    pub(crate) fn concrete(&self) -> String {
        self.segments.join(".")
    }

    fn depth(&self) -> usize {
        self.segments.len()
    }

    fn push(&mut self, segment: String) {
        self.segments.push(segment);
    }

    fn truncate(&mut self, depth: usize) {
        self.segments.truncate(depth);
    }
}

/// Rebuilds `node` with `visit` applied to every value `steps` reaches.
///
/// `visit` receives the value and the node's path. When `steps` is exhausted the current node
/// is handed to `visit`; an object member that is absent, or an array position that is not an
/// array, is skipped (the subtree is cloned unchanged). An array is walked element by element
/// in order, so the row count and the row order survive by construction.
pub(crate) fn map_at<F>(
    node: &Value,
    steps: &[Step],
    path: &mut NodePath,
    visit: &mut F,
) -> Result<Value, PiiError>
where
    F: FnMut(&Value, &NodePath) -> Result<Value, PiiError>,
{
    let Some((step, rest)) = steps.split_first() else {
        return visit(node, path);
    };
    match step {
        Step::Key(name) => {
            let Value::Object(map) = node else {
                return Ok(node.clone());
            };
            let Some(child) = map.get(name) else {
                return Ok(node.clone());
            };
            let depth = path.depth();
            path.push(name.clone());
            let mapped = map_at(child, rest, path, visit)?;
            path.truncate(depth);
            let mut out = map.clone();
            out.insert(name.clone(), mapped);
            Ok(Value::Object(out))
        }
        Step::All => {
            let Value::Array(items) = node else {
                return Ok(node.clone());
            };
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let depth = path.depth();
                path.push(index.to_string());
                let mapped = map_at(item, rest, path, visit)?;
                path.truncate(depth);
                out.push(mapped);
            }
            Ok(Value::Array(out))
        }
    }
}
