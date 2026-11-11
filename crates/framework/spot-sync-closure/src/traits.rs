//! The seams: what the catalog, the destination and the source tell the closure.

use crate::error::ClosureError;
use crate::extras::ExtrasSet;
use crate::model::{Position, RecordKey, SourceImage};
use serde_json::Value;

/// The catalog's view of references (ADR-064 section 2).
pub trait References {
    /// The declared references of one record image: fixed links in the parent and every child
    /// row, and dynamic links whose target entity is read from the record's type field. Text that
    /// merely looks like a name or number is not a reference.
    ///
    /// # Errors
    /// `ReferenceUnreadable` when a dynamic link names an entity its metadata does not allow.
    fn references(&self, entity: &str, image: &Value) -> Result<Vec<RecordKey>, ClosureError>;
}

/// What one destination is known to have (ADR-064 section 3, step 2).
pub trait DestinationKnows {
    /// The destination owns the record (it came from there).
    fn owns(&self, key: &RecordKey) -> bool;
    /// The record is in the destination's scope.
    fn in_scope(&self, key: &RecordKey) -> bool;
    /// The destination's acknowledged position: the completed prefix of what it has applied.
    /// `None` when unknown; then in-scope targets are included (rule for doubt).
    fn acked_position(&self) -> Option<Position>;
    /// The extras already delivered to the destination.
    fn extras(&self) -> &ExtrasSet;
    /// The destination cannot decode the record yet: it is held by a version-skew hold, or it is
    /// withheld behind one (ADR-066 section 5).
    ///
    /// A held record is never pulled into a closure as an extra, whatever its role, even when the
    /// destination's acknowledged position is below it: the hold exists to keep the record away
    /// from a node that cannot apply it. A dependent whose closure needs one is withheld instead
    /// (`BundlePlan::withheld_by_hold`).
    ///
    /// The default is `false`: a caller with no hold set (the node side, a repair pull) shows
    /// every record as decodable and withholds nothing.
    fn held(&self, _key: &RecordKey) -> bool {
        false
    }
}

/// The source's records.
pub trait ImageSource {
    /// The current image of a record with its latest event position, or `None` if it does not exist.
    ///
    /// # Errors
    /// `SourceFailed` when the adapter fails.
    fn image(&self, key: &RecordKey) -> Result<Option<SourceImage>, ClosureError>;
}

/// Applies the destination's `excluded_fields.down` (ADR-063) to an image.
pub trait Redactor {
    /// Remove the excluded fields of `entity` from `image`, parent and child rows.
    fn redact_down(&self, entity: &str, image: &mut Value);
}

/// A redactor that removes nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRedaction;

impl Redactor for NoRedaction {
    fn redact_down(&self, _entity: &str, _image: &mut Value) {}
}

/// The id carried by a link value: a non-empty string, or an integer rendered as text.
/// Anything else (null, empty, structure) is no reference.
#[must_use]
pub fn element_id(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => n
            .as_i64()
            .map(|i| i.to_string())
            .or_else(|| n.as_u64().map(|u| u.to_string())),
        _ => None,
    }
}
