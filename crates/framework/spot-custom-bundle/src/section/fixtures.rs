//! The `fixtures` section: one JSON file per DocType, an array of the site's documents.
//!
//! Documents are sorted by their canonical JSON, so a capture's bytes do not depend on the
//! order the site returned them in, and `serde_json`'s sorted object keys keep the encoding
//! deterministic.

use std::collections::BTreeMap;

use crate::error::{CustomBundleError, codes};
use crate::paths::sanitize;
use crate::registry::{
    Change, ChangeAction, Files, FixtureSet, Problem, Risk, Section, SiteReader, SiteWriter,
};

/// The `fixtures` section.
pub struct FixturesSection;

/// The file carrying a DocType's fixtures.
fn file_name(doctype: &str) -> String {
    format!("{}.json", sanitize(doctype))
}

/// The canonical bytes of one DocType's documents: a compact JSON array, sorted.
fn canonical(documents: &[serde_json::Value]) -> Vec<u8> {
    let mut documents = documents.to_vec();
    documents.sort_by(|a, b| {
        serde_json::to_vec(a)
            .unwrap_or_default()
            .cmp(&serde_json::to_vec(b).unwrap_or_default())
    });
    serde_json::to_vec(&documents).unwrap_or_default()
}

impl FixturesSection {
    /// Parses every file into a document list, reporting the first failure per file.
    fn parse_files(&self, files: &Files) -> Result<Vec<FixtureSet>, Vec<Problem>> {
        let mut sets = Vec::new();
        let mut problems = Vec::new();
        for (path, bytes) in files {
            let Some(doctype) = path.strip_suffix(".json") else {
                problems.push(Problem {
                    code: codes::INVALID_FIXTURES.to_owned(),
                    item: path.clone(),
                    message: "unexpected file in the fixtures section (only `.json` files are \
                              allowed)"
                        .to_owned(),
                });
                continue;
            };
            match serde_json::from_slice::<serde_json::Value>(bytes) {
                Ok(serde_json::Value::Array(documents)) => {
                    if documents.iter().all(serde_json::Value::is_object) {
                        sets.push(FixtureSet {
                            doctype: doctype.to_owned(),
                            documents,
                        });
                    } else {
                        problems.push(Problem {
                            code: codes::INVALID_FIXTURES.to_owned(),
                            item: path.clone(),
                            message: "every fixture must be a JSON object".to_owned(),
                        });
                    }
                }
                Ok(_) => problems.push(Problem {
                    code: codes::INVALID_FIXTURES.to_owned(),
                    item: path.clone(),
                    message: "a fixtures file must be a JSON array of documents".to_owned(),
                }),
                Err(err) => problems.push(Problem {
                    code: codes::INVALID_FIXTURES.to_owned(),
                    item: path.clone(),
                    message: format!("the fixtures file is not valid JSON: {err}"),
                }),
            }
        }
        if problems.is_empty() {
            Ok(sets)
        } else {
            Err(problems)
        }
    }
}

impl Section for FixturesSection {
    fn kind(&self) -> &'static str {
        "fixtures"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn risk(&self, action: ChangeAction) -> Risk {
        match action {
            ChangeAction::Add => Risk::Medium,
            ChangeAction::Update => Risk::Medium,
            ChangeAction::Remove => Risk::High,
        }
    }

    fn check(&self, files: &Files) -> Vec<Problem> {
        match self.parse_files(files) {
            Ok(_) => Vec::new(),
            Err(problems) => problems,
        }
    }

    fn capture(&self, site: &dyn SiteReader) -> Files {
        let mut sets = site.fixtures();
        sets.retain(|set| {
            !site.is_system_managed(self.kind(), &set.doctype)
                && !site.is_secret(self.kind(), &set.doctype)
        });
        sets.sort_by(|a, b| a.doctype.cmp(&b.doctype));
        let mut files = Files::new();
        for set in sets {
            files.insert(file_name(&set.doctype), canonical(&set.documents));
        }
        files
    }

    fn items(&self, files: &Files) -> BTreeMap<String, Vec<u8>> {
        let mut items = BTreeMap::new();
        for (path, bytes) in files {
            if let Some(doctype) = path.strip_suffix(".json")
                && let Ok(serde_json::Value::Array(documents)) =
                    serde_json::from_slice::<serde_json::Value>(bytes)
            {
                items.insert(doctype.to_owned(), canonical(&documents));
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
            ChangeAction::Remove => site.remove_fixtures(&change.key),
            _ => {
                let set = self
                    .parse_files(files)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|set| set.doctype == change.key)
                    .ok_or_else(|| {
                        CustomBundleError::about(
                            codes::APPLY_FAILED,
                            &change.key,
                            "the fixtures are not in the bundle",
                        )
                    })?;
                site.set_fixtures(&set.doctype, set.documents)
            }
        }
    }
}
