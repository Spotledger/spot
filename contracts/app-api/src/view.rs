//! Typed views over the dynamic document (ADR-05).
//!
//! The typed-view generator (WP-MET-04) turns each standard DocType definition into a newtype
//! over [`Document`] implementing [`DocView`], with one getter and setter per standard field.
//! Getters read through the dynamic document, so a view never holds a copy of the data, and a
//! custom field added at runtime stays reachable through [`DocView::doc`]. Generated code looks
//! like this:
//!
//! ```
//! use spot_app_api::prelude::*;
//! use spot_app_api::{AppResult, DocView, Document};
//!
//! pub struct SalesInvoice(Document);
//!
//! impl DocView for SalesInvoice {
//!     const DOCTYPE: &'static str = "sales_invoice";
//!     fn wrap(doc: Document) -> Self { Self(doc) }
//!     fn doc(&self) -> &Document { &self.0 }
//!     fn doc_mut(&mut self) -> &mut Document { &mut self.0 }
//!     fn into_doc(self) -> Document { self.0 }
//! }
//!
//! impl SalesInvoice {
//!     pub fn grand_total(&self) -> AppResult<Option<Decimal>> { self.0.decimal("grand_total") }
//! }
//! ```

use crate::document::Document;
use crate::error::{AppError, AppResult, ErrorClass};

/// A typed view of one DocType.
pub trait DocView: Sized {
    /// The DocType's storage name.
    const DOCTYPE: &'static str;

    /// Wraps a document without checking its DocType; use [`DocView::from_doc`].
    fn wrap(doc: Document) -> Self;

    /// The underlying dynamic document.
    fn doc(&self) -> &Document;

    /// The underlying dynamic document, mutably.
    fn doc_mut(&mut self) -> &mut Document;

    /// Unwraps the document.
    fn into_doc(self) -> Document;

    /// Wraps a document, failing if it is of another DocType.
    fn from_doc(doc: Document) -> AppResult<Self> {
        if doc.doctype().as_str() != Self::DOCTYPE {
            return Err(AppError::new(
                ErrorClass::Internal,
                "document.wrong_doctype",
                format!(
                    "expected a {} document, got {}",
                    Self::DOCTYPE,
                    doc.doctype()
                ),
            ));
        }
        Ok(Self::wrap(doc))
    }
}
