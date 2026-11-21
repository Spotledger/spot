//! Where a validation problem is, and what it is (WP-INT-06.T1, brief items 2 and 4).
//!
//! Both the envelope check and the schema check report **every** problem they find as a
//! [`Violation`] instead of stopping at the first. A violation names its stable `edi.*` code,
//! the segment it sits in (its index in the interchange, its tag and its source byte offset) and,
//! for element problems, the element and component index.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::Segment;

/// Where a violation sits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// The segment's 0-based index in the interchange (`all_segments` order).
    pub segment: usize,
    /// The segment's tag.
    pub tag: String,
    /// The element's 0-based index, when the problem is inside an element.
    pub element: Option<usize>,
    /// The component's 0-based index, when the problem is inside a composite.
    pub component: Option<usize>,
    /// The byte offset of the segment in the source document.
    pub offset: usize,
}

impl Position {
    /// The position of a whole segment at `index`.
    pub fn segment(index: usize, segment: &Segment) -> Self {
        Self {
            segment: index,
            tag: segment.tag.clone(),
            element: None,
            component: None,
            offset: segment.offset,
        }
    }

    /// The position of an element inside a segment.
    pub fn element(index: usize, segment: &Segment, element: usize) -> Self {
        Self {
            segment: index,
            tag: segment.tag.clone(),
            element: Some(element),
            component: None,
            offset: segment.offset,
        }
    }

    /// The position of a component inside an element.
    pub fn component(index: usize, segment: &Segment, element: usize, component: usize) -> Self {
        Self {
            segment: index,
            tag: segment.tag.clone(),
            element: Some(element),
            component: Some(component),
            offset: segment.offset,
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "segment {} ({})", self.segment + 1, self.tag)?;
        if let Some(element) = self.element {
            write!(f, ", element {}", element + 1)?;
        }
        if let Some(component) = self.component {
            write!(f, ", component {}", component + 1)?;
        }
        write!(f, " at byte {}", self.offset)
    }
}

/// One validation problem.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Violation {
    /// The stable `edi.*` code ([`crate::error`]).
    pub code: &'static str,
    /// Where the problem is.
    pub position: Position,
    /// A one-sentence explanation.
    pub detail: String,
}

impl Violation {
    /// Builds a violation.
    pub fn new(code: &'static str, position: Position, detail: impl Into<String>) -> Self {
        Self {
            code,
            position,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {} ({})", self.code, self.detail, self.position)
    }
}

/// Whether any of `violations` has `code`.
pub fn has_code(violations: &[Violation], code: &str) -> bool {
    violations.iter().any(|v| v.code == code)
}
