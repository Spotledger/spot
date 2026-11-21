//! The `rules` section: one expression file per rule.
//!
//! A rule's expression is parsed with `spot-expr` at check time, so a bundle that carries a
//! rule that does not parse is refused when it is packed, never at apply time.

use std::collections::BTreeMap;

use crate::error::{CustomBundleError, codes};
use crate::registry::{
    Change, ChangeAction, Files, Problem, Risk, Section, SiteReader, SiteWriter,
};

/// The `rules` section.
pub struct RulesSection;

/// The file carrying a rule's expression.
fn expression_file(name: &str) -> String {
    format!("{name}.expr")
}

impl Section for RulesSection {
    fn kind(&self) -> &'static str {
        "rules"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn risk(&self, action: ChangeAction) -> Risk {
        match action {
            ChangeAction::Add => Risk::Medium,
            ChangeAction::Update => Risk::Medium,
            ChangeAction::Remove => Risk::Medium,
        }
    }

    fn check(&self, files: &Files) -> Vec<Problem> {
        let mut problems = Vec::new();
        for (path, bytes) in files {
            let Some(name) = path.strip_suffix(".expr") else {
                problems.push(Problem {
                    code: codes::INVALID_RULES.to_owned(),
                    item: path.clone(),
                    message: "unexpected file in the rules section (only `.expr` files are \
                              allowed)"
                        .to_owned(),
                });
                continue;
            };
            let text = match std::str::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    problems.push(Problem {
                        code: codes::INVALID_RULES.to_owned(),
                        item: path.clone(),
                        message: "the expression is not valid UTF-8".to_owned(),
                    });
                    continue;
                }
            };
            if let Err(err) = spot_expr::parse(text) {
                problems.push(Problem {
                    code: codes::INVALID_RULES.to_owned(),
                    item: name.to_owned(),
                    message: format!("the expression does not parse: {err}"),
                });
            }
        }
        problems
    }

    fn capture(&self, site: &dyn SiteReader) -> Files {
        let mut rules = site.rules();
        rules.retain(|rule| {
            !site.is_system_managed(self.kind(), &rule.name)
                && !site.is_secret(self.kind(), &rule.name)
        });
        rules.sort_by(|a, b| a.name.cmp(&b.name));
        let mut files = Files::new();
        for rule in rules {
            files.insert(expression_file(&rule.name), rule.expression.into_bytes());
        }
        files
    }

    fn items(&self, files: &Files) -> BTreeMap<String, Vec<u8>> {
        let mut items = BTreeMap::new();
        for (path, bytes) in files {
            if let Some(name) = path.strip_suffix(".expr") {
                items.insert(name.to_owned(), bytes.clone());
            }
        }
        items
    }

    fn apply(
        &self,
        change: &Change,
        files: &Files,
        site: &mut dyn SiteWriter,
    ) -> Result<(), CustomBundleError> {
        match change.action {
            ChangeAction::Remove => site.remove_rule(&change.key),
            _ => {
                let expression = files.get(&expression_file(&change.key)).ok_or_else(|| {
                    CustomBundleError::about(
                        codes::APPLY_FAILED,
                        &change.key,
                        "the rule's expression is not in the bundle",
                    )
                })?;
                let expression = String::from_utf8(expression.clone()).map_err(|_| {
                    CustomBundleError::about(
                        codes::APPLY_FAILED,
                        &change.key,
                        "the rule's expression is not valid UTF-8",
                    )
                })?;
                site.upsert_rule(&change.key, &expression)
            }
        }
    }
}
