//! Rendering: the workspace's one template engine (`spot-template`/MiniJinja), over the
//! compiled-in template sources. No new template crate is added.

use std::sync::OnceLock;

use spot_template::{Context, MemorySource, TemplateEngine};

use crate::error::{Result, ScaffoldError};
use crate::templates::TEMPLATES;

/// The shared engine over the compiled-in templates.
#[must_use]
pub fn engine() -> &'static TemplateEngine<MemorySource> {
    static ENGINE: OnceLock<TemplateEngine<MemorySource>> = OnceLock::new();
    ENGINE.get_or_init(|| TemplateEngine::memory(TEMPLATES))
}

/// Renders one template with `context`.
///
/// # Errors
/// `scaffold.template` when the template is unknown or fails to render (a missing variable,
/// a syntax fault or a sandbox limit).
pub fn render(name: &str, context: &Context) -> Result<String> {
    engine()
        .render(name, context)
        .map_err(|e| ScaffoldError::new(crate::codes::TEMPLATE, format!("{name}: {e}")))
}

/// Escapes a string for a TOML basic string (`"..."`): backslash and double quote are
/// escaped, and control characters are dropped. Used for a title an author typed.
#[must_use]
pub fn toml_escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn every_template_compiles() {
        let engine = engine();
        for (name, _) in TEMPLATES {
            // Rendering with an empty context must fail on a missing variable, not on a
            // syntax error; the proof tests render each one with a real context.
            if let Err(spot_template::TemplateError::Syntax { .. }) =
                engine.render(name, &Context::new())
            {
                panic!("template {name} has a syntax error");
            }
        }
    }

    #[test]
    fn toml_escaping() {
        assert_eq!(toml_escape("a\"b\\c"), "a\\\"b\\\\c");
    }
}
