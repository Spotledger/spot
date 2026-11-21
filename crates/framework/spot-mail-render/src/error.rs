//! The renderer's errors: a stable code and the template name, file and line, nothing else.
//!
//! A message never carries template source, a context value, a recipient or a header. The one
//! piece of free text is [`RenderError::NotComputable`], which is the engine's own sentence
//! about the rejected operation (it names an operation, never a value).

use std::fmt;

/// The code of a template name, locale or file name that is not a plain name.
pub const INVALID_NAME: &str = "mail_render.invalid_name";
/// The code of a template no root has.
pub const TEMPLATE_NOT_FOUND: &str = "mail_render.template_not_found";
/// The code of a template without a subject or without any body.
pub const TEMPLATE_INCOMPLETE: &str = "mail_render.template_incomplete";
/// The code of a template file that is a link, not a regular file, or too large.
pub const FILE_REFUSED: &str = "mail_render.file_refused";
/// The code of a variable the context lacks.
pub const UNDEFINED_VARIABLE: &str = "mail_render.undefined_variable";
/// The code of a template that tried to compute money.
pub const NOT_COMPUTABLE: &str = "mail_render.not_computable";
/// The code of any other failure while rendering.
pub const RENDER_FAILED: &str = "mail_render.render_failed";
/// The code of a subject that is not one short line.
pub const SUBJECT_INVALID: &str = "mail_render.subject_invalid";
/// The code of a body that is larger than the limit.
pub const OUTPUT_TOO_LARGE: &str = "mail_render.output_too_large";
/// The code of a context that cannot be turned into template data.
pub const INVALID_CONTEXT: &str = "mail_render.invalid_context";
/// The code of a rendered mail that does not make a valid message.
pub const INVALID_MESSAGE: &str = "mail_render.invalid_message";

/// Why a mail could not be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// A template name, locale or file name is not a plain name (a parent step, a separator,
    /// too long, a character outside the allowed set).
    InvalidName {
        /// What was refused: `name`, `locale` or `file`. The refused text is not repeated.
        what: &'static str,
    },
    /// No template root has a template of this name.
    TemplateNotFound {
        /// The template name.
        template: String,
    },
    /// The template lacks a part every mail needs.
    TemplateIncomplete {
        /// The template name.
        template: String,
        /// `subject` or `body`.
        missing: &'static str,
    },
    /// A template file is a link, is not a regular file, is over 256 KiB or is not UTF-8.
    FileRefused {
        /// The template name.
        template: String,
        /// The file name inside the template's folder.
        file: String,
        /// `link`, `not_a_file`, `too_large`, `not_utf8` or `unreadable`.
        reason: &'static str,
    },
    /// The context lacks a variable the template uses (strict undefined).
    UndefinedVariable {
        /// The template name.
        template: String,
        /// The file name inside the template's folder.
        file: String,
        /// The line of the template file, 1-based; 0 when unknown.
        line: usize,
        /// The variable (or attribute chain) that was missing.
        variable: String,
    },
    /// The template tried to compute with money; amounts can only be formatted.
    NotComputable {
        /// The template name.
        template: String,
        /// The file name inside the template's folder.
        file: String,
        /// The engine's sentence about the rejected operation.
        message: String,
    },
    /// Any other failure while rendering.
    RenderFailed {
        /// The template name.
        template: String,
        /// The file name inside the template's folder.
        file: String,
        /// The line of the template file, 1-based; 0 when unknown.
        line: usize,
        /// `syntax`, `fuel`, `recursion`, `filter` or `render`.
        kind: &'static str,
    },
    /// The rendered subject is empty, longer than 255 characters or holds a line break.
    SubjectInvalid {
        /// The template name.
        template: String,
        /// `empty`, `too_long` or `line_break`.
        reason: &'static str,
    },
    /// A rendered part is larger than its limit.
    OutputTooLarge {
        /// The template name.
        template: String,
        /// The file name inside the template's folder.
        file: String,
        /// The limit in bytes.
        limit: usize,
    },
    /// A JSON context could not be turned into template data.
    InvalidContext {
        /// A fixed reason: `not_an_object`, `non_integer_number`, `bad_decimal`, `bad_date`
        /// or `bad_datetime`.
        reason: &'static str,
    },
    /// The rendered mail is not a valid message (no recipient, a bad header).
    InvalidMessage,
}

impl RenderError {
    /// The stable catalogue code (`mail_render.*`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => INVALID_NAME,
            Self::TemplateNotFound { .. } => TEMPLATE_NOT_FOUND,
            Self::TemplateIncomplete { .. } => TEMPLATE_INCOMPLETE,
            Self::FileRefused { .. } => FILE_REFUSED,
            Self::UndefinedVariable { .. } => UNDEFINED_VARIABLE,
            Self::NotComputable { .. } => NOT_COMPUTABLE,
            Self::RenderFailed { .. } => RENDER_FAILED,
            Self::SubjectInvalid { .. } => SUBJECT_INVALID,
            Self::OutputTooLarge { .. } => OUTPUT_TOO_LARGE,
            Self::InvalidContext { .. } => INVALID_CONTEXT,
            Self::InvalidMessage => INVALID_MESSAGE,
        }
    }
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.code())?;
        match self {
            Self::InvalidName { what } => write!(f, "the {what} is not a plain name"),
            Self::TemplateNotFound { template } => write!(f, "template {template} not found"),
            Self::TemplateIncomplete { template, missing } => {
                write!(f, "template {template} has no {missing}")
            }
            Self::FileRefused {
                template,
                file,
                reason,
            } => write!(f, "template {template}, file {file} refused ({reason})"),
            Self::UndefinedVariable {
                template,
                file,
                line,
                variable,
            } => write!(
                f,
                "template {template}, file {file}, line {line}: the context has no `{variable}`"
            ),
            Self::NotComputable {
                template,
                file,
                message,
            } => write!(f, "template {template}, file {file}: {message}"),
            Self::RenderFailed {
                template,
                file,
                line,
                kind,
            } => write!(
                f,
                "template {template}, file {file}, line {line}: {kind} error"
            ),
            Self::SubjectInvalid { template, reason } => {
                write!(f, "template {template}: the subject is invalid ({reason})")
            }
            Self::OutputTooLarge {
                template,
                file,
                limit,
            } => write!(
                f,
                "template {template}, file {file}: output is over {limit} bytes"
            ),
            Self::InvalidContext { reason } => write!(f, "the context is invalid ({reason})"),
            Self::InvalidMessage => write!(f, "the rendered mail is not a valid message"),
        }
    }
}

impl std::error::Error for RenderError {}
