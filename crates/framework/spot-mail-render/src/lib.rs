//! Named e-mail templates (WP-PLT-11.T14).
//!
//! An app or a site operator writes an e-mail once, as a template; a sender renders it for one
//! recipient with a context. This crate loads the template, renders it through `spot-template`
//! and hands back a [`RenderedMail`] that becomes an [`spot_mail::OutgoingMessage`].
//!
//! # Layout
//!
//! A template is a folder `<root>/mail/<name>/`:
//!
//! - `subject.txt` (one line), optionally `subject.<locale>.txt`;
//! - `body.txt` and/or `body.html` (at least one), optionally `body.<locale>.txt` and
//!   `body.<locale>.html`.
//!
//! The roots, in order: the site's template folder, then each installed app's `templates`
//! folder; the first root that has the name wins, so a site overrides an app. See
//! [`TemplateRoots`].
//!
//! # Rules
//!
//! - The subject renders as text (nothing is escaped) and must be one line; the text body as
//!   text; the HTML body with HTML auto-escaping.
//! - A missing locale falls back to `en`, then to the file without a locale; the locale used is
//!   in [`RenderedMail::locale`].
//! - A variable the context lacks is an error naming the template, file, line and variable.
//! - Money is formatted, never computed (the engine refuses arithmetic on decimals).
//! - Names are validated before any read; a link, a non-regular file and a file over 256 KiB are
//!   refused. Errors carry the template name, file and line only: never source text, a context
//!   value or a recipient.
//! - A template never chooses a sender, a recipient or a header; the caller passes them to
//!   [`RenderedMail::into_message`].
//!
//! The crate opens no connection and reads no clock or database.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod context;
pub mod error;
mod html_text;
mod names;
pub mod render;
mod source;

pub use context::context_from_json;
pub use error::RenderError;
pub use html_text::html_to_text;
pub use render::{FALLBACK_LOCALE, MAX_BODY_BYTES, MAX_SUBJECT_CHARS, MailRenderer, RenderedMail};
pub use source::{MAX_TEMPLATE_BYTES, TemplateRoots};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-mail-render";
