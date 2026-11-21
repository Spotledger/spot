//! The compile report: what a build found but did not refuse.
//!
//! A warning never stops a build and never changes what is stored. It marks a definition that
//! compiles but that a client or the author should know about, such as a control hint that can
//! never apply to the field's kind (ADR-068 amendment A2). The report is part of the effective
//! metadata, so a caller that reloads or adopts a snapshot sees the same findings.

/// One warning-level finding of a compile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// The stable code, in the `meta.*` family (`meta.control_not_applicable`).
    pub code: &'static str,
    /// The DocType the finding is about.
    pub doctype: String,
    /// The field the finding is about, when it is about one.
    pub field: Option<String>,
    /// What is wrong, and what the platform does instead.
    pub message: String,
}

impl Warning {
    /// A finding with a code, its DocType and field, and a sentence.
    pub fn new(
        code: &'static str,
        doctype: &str,
        field: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            doctype: doctype.to_owned(),
            field: field.map(str::to_owned),
            message: message.into(),
        }
    }
}

/// The findings of one build. An empty report is the common case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompileReport {
    warnings: Vec<Warning>,
}

impl CompileReport {
    /// An empty report.
    pub fn new() -> Self {
        Self::default()
    }

    /// The warnings, in the order the build found them.
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// Whether the build found nothing to report.
    pub fn is_empty(&self) -> bool {
        self.warnings.is_empty()
    }

    /// Takes the warnings of one compiled DocType.
    pub(crate) fn extend(&mut self, warnings: Vec<Warning>) {
        self.warnings.extend(warnings);
    }
}
