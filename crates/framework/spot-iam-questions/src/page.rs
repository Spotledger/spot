//! Deterministic paging for the four questions.
//!
//! A question's full answer is sorted on a total key and then cut into pages of at most
//! [`MAX_PAGE`] rows. Every page of the same question names the same rows in the same order,
//! so a large result is walked page by page without repeats or gaps, whatever order the read
//! model happened to hand its data in.

use serde::{Deserialize, Serialize};

use crate::error::QuestionsError;

/// The default page size when a caller does not name one.
pub const DEFAULT_PAGE: usize = 50;

/// The largest page a caller may ask for: a question never returns an unbounded answer.
pub const MAX_PAGE: usize = 500;

/// Which slice of a sorted answer a caller wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Page {
    /// How many sorted rows to skip.
    #[serde(default)]
    pub offset: usize,
    /// How many sorted rows to return; at least 1 and at most [`MAX_PAGE`].
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    DEFAULT_PAGE
}

impl Default for Page {
    fn default() -> Self {
        Page {
            offset: 0,
            limit: DEFAULT_PAGE,
        }
    }
}

impl Page {
    /// The first page: offset 0, [`DEFAULT_PAGE`] rows.
    #[must_use]
    pub fn first() -> Page {
        Page::default()
    }

    /// A page at `offset` with `limit` rows.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::PageInvalid`] when the limit is zero or above [`MAX_PAGE`].
    pub fn new(offset: usize, limit: usize) -> Result<Page, QuestionsError> {
        if limit == 0 || limit > MAX_PAGE {
            return Err(QuestionsError::PageInvalid {
                subject: "(page)".to_owned(),
                detail: format!("limit must be between 1 and {MAX_PAGE}, not {limit}"),
            });
        }
        Ok(Page { offset, limit })
    }
}

/// One page of a sorted answer, with just enough to walk the rest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Paged<T> {
    /// The rows on this page, in the answer's deterministic order.
    pub items: Vec<T>,
    /// How many rows the whole answer has.
    pub total: usize,
    /// Where this page starts.
    pub offset: usize,
    /// This page's size.
    pub limit: usize,
    /// The offset of the next page, when one is left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<usize>,
}

impl<T> Paged<T> {
    /// Cuts a fully sorted answer into the page `page` asks for.
    #[must_use]
    pub fn of(items: Vec<T>, page: Page) -> Paged<T> {
        let page = Page {
            offset: page.offset,
            limit: page.limit.clamp(1, MAX_PAGE),
        };
        let total = items.len();
        let start = page.offset.min(total);
        let end = start.saturating_add(page.limit).min(total);
        let mut rows = items;
        let _ = rows.split_off(end);
        let mut rows = rows.split_off(start);
        rows.shrink_to_fit();
        let next = (end < total).then_some(end);
        Paged {
            items: rows,
            total,
            offset: page.offset,
            limit: page.limit,
            next,
        }
    }

    /// Whether the page carries no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn pages_walk_a_sorted_answer_without_gaps() {
        let all: Vec<usize> = (0..10).collect();
        let first = Paged::of(all.clone(), Page::new(0, 4).expect("page"));
        assert_eq!(first.items, vec![0, 1, 2, 3]);
        assert_eq!(first.total, 10);
        assert_eq!(first.next, Some(4));
        let second = Paged::of(all.clone(), Page::new(4, 4).expect("page"));
        assert_eq!(second.items, vec![4, 5, 6, 7]);
        let last = Paged::of(all, Page::new(8, 4).expect("page"));
        assert_eq!(last.items, vec![8, 9]);
        assert_eq!(last.next, None);
    }

    #[test]
    fn a_limit_of_zero_or_above_the_max_is_refused() {
        assert!(Page::new(0, 0).is_err());
        assert!(Page::new(0, MAX_PAGE + 1).is_err());
    }
}
