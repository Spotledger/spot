//! Environment-specific values: `${env.NAME}` placeholders in bundle content and the variable
//! set each environment resolves them from.
//!
//! The placeholder text is part of the bundle and therefore of its content digest; the values
//! are not. That is what lets one bundle have one digest in every environment while the
//! applied values differ. `resolve` substitutes values before planning and applying;
//! `normalise` folds the target's values back to placeholders so the applied result can be
//! compared with the source digest.
//!
//! A secret is a *reference only*: the environment stores the secret's name, and resolution
//! writes the reference `secret:<name>`, never the secret's value. A bundle can therefore be
//! transported and re-captured without a credential ever entering it.

use std::collections::{BTreeMap, BTreeSet};

use spot_custom_bundle::{Bundle, Limits, SectionData};

use crate::error::{EnvironmentError, Result, codes};

/// The placeholder prefix.
pub const PLACEHOLDER_PREFIX: &str = "${env.";

/// A secret's name in the secret store. This type never holds the secret's value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretRef(String);

impl SecretRef {
    /// A reference to the secret named `name`.
    ///
    /// # Errors
    /// [`codes::INVALID_VARIABLE`] when the name is empty, longer than 63 characters or uses
    /// anything but lower-case letters, digits, `-`, `_` and `.`.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        let valid = !name.is_empty()
            && name.len() <= 63
            && name.starts_with(|c: char| c.is_ascii_lowercase())
            && name.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.')
            });
        if !valid {
            return Err(EnvironmentError::new(
                codes::INVALID_VARIABLE,
                "a secret reference is 1 to 63 characters of lower-case letters, digits, '-', '_' and '.', starting with a letter",
            ));
        }
        Ok(Self(name))
    }

    /// The secret's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }

    /// The reference written into content: `secret:<name>`.
    #[must_use]
    pub fn reference(&self) -> String {
        format!("secret:{}", self.0)
    }
}

/// What a placeholder resolves to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// A plain value, written into the content.
    Text(String),
    /// A secret, written as a reference only.
    Secret(SecretRef),
}

impl Binding {
    /// The text resolution writes.
    #[must_use]
    pub fn resolved(&self) -> String {
        match self {
            Self::Text(value) => value.clone(),
            Self::Secret(secret) => secret.reference(),
        }
    }
}

/// One environment's variable set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Variables {
    bindings: BTreeMap<String, Binding>,
}

impl Variables {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Binds `name` to a plain value.
    ///
    /// # Errors
    /// [`codes::INVALID_VARIABLE`] for a malformed name or an empty value.
    pub fn set_text(&mut self, name: &str, value: impl Into<String>) -> Result<&mut Self> {
        check_name(name)?;
        let value = value.into();
        if value.is_empty() {
            return Err(EnvironmentError::about(
                codes::INVALID_VARIABLE,
                name,
                "a variable value is not empty",
            ));
        }
        self.bindings.insert(name.to_owned(), Binding::Text(value));
        Ok(self)
    }

    /// Binds `name` to a secret reference.
    ///
    /// # Errors
    /// [`codes::INVALID_VARIABLE`] for a malformed name.
    pub fn set_secret(&mut self, name: &str, secret: SecretRef) -> Result<&mut Self> {
        check_name(name)?;
        self.bindings
            .insert(name.to_owned(), Binding::Secret(secret));
        Ok(self)
    }

    /// The binding for a name, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Binding> {
        self.bindings.get(name)
    }

    /// The names the bundle uses that this set does not bind, sorted without repeats.
    #[must_use]
    pub fn unresolved(&self, bundle: &Bundle) -> Vec<String> {
        let mut missing = BTreeSet::new();
        for files in bundle.sections().values() {
            for bytes in files.files.values() {
                if let Ok(text) = std::str::from_utf8(bytes)
                    && let Ok(found) = scan(text)
                {
                    for (name, _, _) in found {
                        if !self.bindings.contains_key(&name) {
                            missing.insert(name);
                        }
                    }
                }
            }
        }
        missing.into_iter().collect()
    }

    /// Refuses a bundle with a malformed placeholder or an unbound one, before any plan is
    /// made.
    ///
    /// # Errors
    /// [`codes::INVALID_VARIABLE`] for a malformed placeholder and
    /// [`codes::UNRESOLVED_VARIABLE`] naming the first unbound one.
    pub fn validate(&self, bundle: &Bundle) -> Result<()> {
        for files in bundle.sections().values() {
            for bytes in files.files.values() {
                let Ok(text) = std::str::from_utf8(bytes) else {
                    continue;
                };
                for (name, _, _) in scan(text)? {
                    if !self.bindings.contains_key(&name) {
                        return Err(EnvironmentError::about(
                            codes::UNRESOLVED_VARIABLE,
                            name,
                            "the environment has no value for this placeholder",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolves every placeholder in the bundle, returning a bundle with its own digest for
    /// the resolved content.
    ///
    /// # Errors
    /// [`codes::INVALID_VARIABLE`] or [`codes::UNRESOLVED_VARIABLE`] from
    /// [`Variables::validate`], or a bundle packing failure.
    pub fn resolve(&self, bundle: &Bundle, limits: &Limits) -> Result<Bundle> {
        self.validate(bundle)?;
        let sections = self.map_sections(bundle.sections(), |name| {
            self.bindings
                .get(name)
                .map(Binding::resolved)
                .ok_or_else(|| {
                    EnvironmentError::about(
                        codes::UNRESOLVED_VARIABLE,
                        name,
                        "the environment has no value for this placeholder",
                    )
                })
        })?;
        pack(bundle, sections, limits)
    }

    /// Folds every bound value back to its placeholder, so a capture from an environment can
    /// be compared with the source bundle's digest.
    ///
    /// # Errors
    /// [`codes::INVALID_VARIABLE`] when two names resolve to the same text (the fold would be
    /// ambiguous) or when packing fails.
    pub fn normalise(&self, bundle: &Bundle, limits: &Limits) -> Result<Bundle> {
        let sections = self.normalise_sections(bundle.sections())?;
        pack(bundle, sections, limits)
    }

    /// Folds the values in one section map back to placeholders.
    ///
    /// # Errors
    /// [`codes::INVALID_VARIABLE`] when two names resolve to the same text.
    pub fn normalise_sections(
        &self,
        sections: &BTreeMap<String, SectionData>,
    ) -> Result<BTreeMap<String, SectionData>> {
        let replacements = self.replacements()?;
        let mut out = sections.clone();
        for section in out.values_mut() {
            for bytes in section.files.values_mut() {
                let Ok(text) = std::str::from_utf8(bytes) else {
                    continue;
                };
                let mut folded = text.to_owned();
                for (value, placeholder) in &replacements {
                    folded = folded.replace(value, placeholder);
                }
                *bytes = folded.into_bytes();
            }
        }
        Ok(out)
    }

    /// The `(value, "${env.NAME}")` pairs to fold, longest value first, refusing two names that
    /// resolve to the same text.
    fn replacements(&self) -> Result<Vec<(String, String)>> {
        let mut seen: BTreeMap<String, &str> = BTreeMap::new();
        let mut pairs = Vec::new();
        for (name, binding) in &self.bindings {
            let value = binding.resolved();
            if let Some(other) = seen.insert(value.clone(), name)
                && other != name
            {
                return Err(EnvironmentError::about(
                    codes::INVALID_VARIABLE,
                    name,
                    "two variables resolve to the same text, so a capture cannot be folded back unambiguously",
                ));
            }
            pairs.push((value, format!("{PLACEHOLDER_PREFIX}{name}}}")));
        }
        pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(pairs)
    }

    /// Rewrites every section file through `resolve`, leaving non-UTF-8 files byte for byte.
    fn map_sections(
        &self,
        sections: &BTreeMap<String, SectionData>,
        mut resolve: impl FnMut(&str) -> Result<String>,
    ) -> Result<BTreeMap<String, SectionData>> {
        let mut out = sections.clone();
        for section in out.values_mut() {
            for bytes in section.files.values_mut() {
                let Ok(text) = std::str::from_utf8(bytes) else {
                    continue;
                };
                let found = scan(text)?;
                let mut resolved = String::with_capacity(text.len());
                let mut cursor = 0;
                for (name, start, end) in found {
                    resolved.push_str(&text[cursor..start]);
                    resolved.push_str(&resolve(&name)?);
                    cursor = end;
                }
                resolved.push_str(&text[cursor..]);
                *bytes = resolved.into_bytes();
            }
        }
        Ok(out)
    }
}

/// Packs `sections` under `bundle`'s own header.
fn pack(
    bundle: &Bundle,
    sections: BTreeMap<String, SectionData>,
    limits: &Limits,
) -> Result<Bundle> {
    Bundle::pack(bundle.header().clone(), sections, limits).map_err(|err| {
        EnvironmentError::about(
            codes::INVALID_BUNDLE,
            err.code(),
            "the variable-resolved bundle could not be packed",
        )
    })
}

/// Checks a variable name: `[A-Za-z_][A-Za-z0-9_]*`, at most 63 characters.
fn check_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = name.len() <= 63
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(EnvironmentError::about(
            codes::INVALID_VARIABLE,
            name,
            "a variable name is 1 to 63 letters, digits or '_', starting with a letter or '_'",
        ))
    }
}

/// The placeholders in one text: `(name, start, end)` in order.
///
/// # Errors
/// [`codes::INVALID_VARIABLE`] for an unterminated `${env.` or a malformed name.
fn scan(text: &str) -> Result<Vec<(String, usize, usize)>> {
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find(PLACEHOLDER_PREFIX) {
        let start = cursor + offset;
        let name_start = start + PLACEHOLDER_PREFIX.len();
        let Some(close) = text[name_start..].find('}') else {
            return Err(EnvironmentError::about(
                codes::INVALID_VARIABLE,
                &text[start..],
                "a placeholder is not closed with '}'",
            ));
        };
        let end = name_start + close + 1;
        let name = &text[name_start..end - 1];
        check_name(name).map_err(|_| {
            EnvironmentError::about(
                codes::INVALID_VARIABLE,
                name,
                "a placeholder name is 1 to 63 letters, digits or '_'",
            )
        })?;
        found.push((name.to_owned(), start, end));
        cursor = end;
    }
    Ok(found)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_placeholders_in_order() {
        let found = scan("a ${env.ONE} b ${env.TWO}").unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, "ONE");
        assert_eq!(found[1].0, "TWO");
    }

    #[test]
    fn scan_refuses_a_broken_placeholder() {
        assert!(scan("${env.}").is_err());
        assert!(scan("${env.ONE").is_err());
        assert!(scan("${env.ONE!}").is_err());
    }

    #[test]
    fn secret_refs_are_validated() {
        assert!(SecretRef::new("payments-api").is_ok());
        assert!(SecretRef::new("Payments").is_err());
        assert!(SecretRef::new("").is_err());
        assert_eq!(
            SecretRef::new("payments-api").unwrap().reference(),
            "secret:payments-api"
        );
    }

    #[test]
    fn names_are_validated() {
        assert!(check_name("API_URL").is_ok());
        assert!(check_name("_private").is_ok());
        assert!(check_name("1bad").is_err());
        assert!(check_name("has-dash").is_err());
    }
}
