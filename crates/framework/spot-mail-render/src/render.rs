//! [`MailRenderer`]: a named template and a context become a [`RenderedMail`].

use spot_mail::{Mailbox, OutgoingMessage};
use spot_template::{Context, OutputKind, TemplateEngine, TemplateError};

use crate::error::RenderError;
use crate::html_text::html_to_text;
use crate::names::{is_locale, is_template_name};
use crate::source::{MailSource, Refusal, TemplateRoots};

/// The longest rendered subject, in characters.
pub const MAX_SUBJECT_CHARS: usize = 255;
/// The largest rendered body, in bytes: 1 MiB.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// The locale tried after the requested one.
pub const FALLBACK_LOCALE: &str = "en";

/// A rendered mail: a subject and at least one body, before any sender or recipient is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMail {
    /// The template that was rendered.
    pub template: String,
    /// The locale of the body files that were used; `None` when the template without a locale
    /// was used.
    pub locale: Option<String>,
    /// The subject: one line, at most 255 characters.
    pub subject: String,
    /// The plain-text body, when the template has one.
    pub text: Option<String>,
    /// The HTML body, when the template has one.
    pub html: Option<String>,
}

impl RenderedMail {
    /// Makes an outgoing message. The caller supplies the sender and the recipients: a template
    /// never chooses them. When only an HTML body exists a plain-text alternative is derived
    /// (see [`crate::html_to_text`]); when only text exists there is no HTML part.
    ///
    /// # Errors
    /// [`RenderError::InvalidMessage`] when the message does not validate (for example no
    /// recipient). The error carries no address.
    pub fn into_message(
        self,
        from: Mailbox,
        to: Vec<Mailbox>,
    ) -> Result<OutgoingMessage, RenderError> {
        let mut message = OutgoingMessage::new(from, self.subject);
        message.to = to;
        message.text = match (self.text, &self.html) {
            (Some(text), _) => Some(text),
            (None, Some(html)) => Some(html_to_text(html)),
            (None, None) => None,
        };
        message.html = self.html;
        message
            .validate()
            .map_err(|_| RenderError::InvalidMessage)?;
        Ok(message)
    }
}

/// Renders named mail templates from the template roots.
///
/// Compiled templates are cached by the engine under `(file, content version)`: a changed file
/// renders its new content on the next call, with no restart.
pub struct MailRenderer {
    roots: TemplateRoots,
    engine: TemplateEngine<MailSource>,
}

/// One located part: the file name and the locale it was found under.
struct Located {
    file: String,
    locale: Option<String>,
}

impl MailRenderer {
    /// A renderer over `roots`, searched in order (the site's folder first, then the apps').
    #[must_use]
    pub fn new(roots: TemplateRoots) -> Self {
        let engine = TemplateEngine::new(MailSource {
            roots: roots.clone(),
        })
        .with_max_output_bytes(MAX_BODY_BYTES);
        Self { roots, engine }
    }

    /// Renders template `name` for `locale` with `context`.
    ///
    /// A missing locale file falls back to `en`, then to the file without a locale; each part
    /// (subject, text body, HTML body) falls back on its own.
    ///
    /// # Errors
    /// See [`RenderError`]; every variant names the template and, where known, the file and the
    /// line, never source text or a context value.
    pub fn render(
        &self,
        name: &str,
        locale: Option<&str>,
        context: &Context,
    ) -> Result<RenderedMail, RenderError> {
        if !is_template_name(name) {
            return Err(RenderError::InvalidName { what: "name" });
        }
        if let Some(locale) = locale
            && !is_locale(locale)
        {
            return Err(RenderError::InvalidName { what: "locale" });
        }
        let dir = self
            .roots
            .template_dir(name)
            .ok_or_else(|| RenderError::TemplateNotFound {
                template: name.to_owned(),
            })?;
        let subject_file = self.locate(name, &dir, "subject", "txt", locale)?;
        let text_file = self.locate(name, &dir, "body", "txt", locale)?;
        let html_file = self.locate(name, &dir, "body", "html", locale)?;
        let subject_file = subject_file.ok_or_else(|| RenderError::TemplateIncomplete {
            template: name.to_owned(),
            missing: "subject",
        })?;
        if text_file.is_none() && html_file.is_none() {
            return Err(RenderError::TemplateIncomplete {
                template: name.to_owned(),
                missing: "body",
            });
        }

        let subject = self.subject(name, &subject_file.file, context)?;
        let text = match &text_file {
            Some(found) => Some(self.part(name, &found.file, context)?),
            None => None,
        };
        let html = match &html_file {
            Some(found) => Some(self.part(name, &found.file, context)?),
            None => None,
        };
        let chosen = text_file
            .iter()
            .chain(html_file.iter())
            .chain(std::iter::once(&subject_file))
            .find_map(|found| found.locale.clone());
        Ok(RenderedMail {
            template: name.to_owned(),
            locale: chosen,
            subject,
            text,
            html,
        })
    }

    /// Finds `<stem>[.<locale>].<ext>`: the requested locale, then `en`, then no locale.
    fn locate(
        &self,
        template: &str,
        dir: &std::path::Path,
        stem: &str,
        ext: &str,
        locale: Option<&str>,
    ) -> Result<Option<Located>, RenderError> {
        let mut candidates: Vec<Option<&str>> = Vec::new();
        if let Some(locale) = locale {
            candidates.push(Some(locale));
        }
        if locale != Some(FALLBACK_LOCALE) {
            candidates.push(Some(FALLBACK_LOCALE));
        }
        candidates.push(None);
        for candidate in candidates {
            let file = match candidate {
                Some(tag) => format!("{stem}.{tag}.{ext}"),
                None => format!("{stem}.{ext}"),
            };
            match self.roots.probe(dir, &file) {
                Ok(true) => {
                    return Ok(Some(Located {
                        file,
                        locale: candidate.map(str::to_owned),
                    }));
                }
                Ok(false) => {}
                Err(refusal) => {
                    return Err(RenderError::FileRefused {
                        template: template.to_owned(),
                        file,
                        reason: refusal_reason(refusal),
                    });
                }
            }
        }
        Ok(None)
    }

    fn part(&self, template: &str, file: &str, context: &Context) -> Result<String, RenderError> {
        let engine_name = format!("{template}/{file}");
        self.engine
            .render(&engine_name, context)
            .map_err(|error| map_error(error, template, file))
    }

    fn subject(
        &self,
        template: &str,
        file: &str,
        context: &Context,
    ) -> Result<String, RenderError> {
        debug_assert_eq!(OutputKind::from_name(file), OutputKind::Text);
        let rendered = self.part(template, file, context)?;
        // The file's own trailing line break is not part of the subject.
        let subject = rendered.trim_end_matches(['\r', '\n']);
        let invalid = |reason: &'static str| RenderError::SubjectInvalid {
            template: template.to_owned(),
            reason,
        };
        if subject.contains(['\r', '\n']) {
            return Err(invalid("line_break"));
        }
        if subject.trim().is_empty() {
            return Err(invalid("empty"));
        }
        if subject.chars().count() > MAX_SUBJECT_CHARS {
            return Err(invalid("too_long"));
        }
        Ok(subject.to_owned())
    }
}

fn refusal_reason(refusal: Refusal) -> &'static str {
    refusal.reason()
}

/// The template and file an engine error names (`<template>/<file>`), falling back to the ones
/// being rendered.
fn names_of(engine_name: &str, template: &str, file: &str) -> (String, String) {
    match engine_name.split_once('/') {
        Some((t, f)) => (t.to_owned(), f.to_owned()),
        None => (template.to_owned(), file.to_owned()),
    }
}

fn map_error(error: TemplateError, template: &str, file: &str) -> RenderError {
    let failed = |name: &str, line: usize, kind: &'static str| {
        let (template, file) = names_of(name, template, file);
        RenderError::RenderFailed {
            template,
            file,
            line,
            kind,
        }
    };
    match error {
        TemplateError::NotFound { .. } => RenderError::TemplateNotFound {
            template: template.to_owned(),
        },
        TemplateError::Syntax { name, line, .. } => failed(&name, line, "syntax"),
        TemplateError::Undefined {
            name,
            line,
            variable,
            ..
        } => {
            let (template, file) = names_of(&name, template, file);
            RenderError::UndefinedVariable {
                template,
                file,
                line,
                variable,
            }
        }
        TemplateError::FuelExhausted { name, line } => failed(&name, line, "fuel"),
        TemplateError::RecursionLimitExceeded { name, line } => failed(&name, line, "recursion"),
        TemplateError::OutputLimitExceeded { limit, .. } => RenderError::OutputTooLarge {
            template: template.to_owned(),
            file: file.to_owned(),
            limit,
        },
        TemplateError::NotComputable { message } => RenderError::NotComputable {
            template: template.to_owned(),
            file: file.to_owned(),
            message,
        },
        TemplateError::InvalidInput { .. }
        | TemplateError::UnknownCurrency { .. }
        | TemplateError::UnknownLocale { .. }
        | TemplateError::InvalidDateFormat { .. }
        | TemplateError::Filter { .. } => failed(&format!("{template}/{file}"), 0, "filter"),
        TemplateError::Render { name, line, .. } => failed(&name, line, "render"),
        _ => failed(&format!("{template}/{file}"), 0, "render"),
    }
}
