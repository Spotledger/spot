//! The `scripts` section: one `.ts` source file per script plus a `scripts.toml` index.
//!
//! The bundle carries the **source and its hash**, never the transpiled JavaScript or the
//! bytecode: both are rebuilt on apply from the source. The hash is the SHA-256 of the source
//! (what `spot-ts` records as a script's `source_hash`), so a reader can check that a script's
//! source has not changed without running it. Checking a script also re-transpiles it with
//! `spot-ts-transpile`, so a bundle never carries a script the engine could not build.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_ts_transpile::Options;

use crate::error::{CustomBundleError, codes};
use crate::registry::{
    Change, ChangeAction, Files, Problem, Risk, ScriptRecord, Section, SiteReader, SiteWriter,
};

/// The index file's name.
const INDEX: &str = "scripts.toml";

/// The `scripts` section.
pub struct ScriptsSection;

/// The `scripts.toml` index: the metadata of every script, with no source.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptsIndex {
    #[serde(default)]
    scripts: Vec<ScriptEntry>,
}

/// One script's metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptEntry {
    name: String,
    app: String,
    hook_point: String,
    hash: String,
}

/// The file carrying a script's source.
fn source_file(name: &str) -> String {
    format!("{name}.ts")
}

/// The canonical bytes of one script, for the diff.
fn item_bytes(entry: &ScriptEntry, source: &str) -> Vec<u8> {
    format!(
        "app = {}\nhook_point = {}\nhash = {}\n{}",
        entry.app, entry.hook_point, entry.hash, source
    )
    .into_bytes()
}

impl ScriptsSection {
    /// Parses the index and its sources.
    fn parse(
        &self,
        files: &Files,
    ) -> Result<(ScriptsIndex, BTreeMap<String, String>), Vec<Problem>> {
        let mut problems = Vec::new();
        let index = match files.get(INDEX) {
            None => {
                problems.push(Problem {
                    code: codes::INVALID_SCRIPTS.to_owned(),
                    item: INDEX.to_owned(),
                    message: "the scripts index is missing".to_owned(),
                });
                return Err(problems);
            }
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => match toml::from_str::<ScriptsIndex>(text) {
                    Ok(index) => index,
                    Err(err) => {
                        problems.push(Problem {
                            code: codes::INVALID_SCRIPTS.to_owned(),
                            item: INDEX.to_owned(),
                            message: format!(
                                "the index is not valid TOML: {}",
                                err.message().trim_end()
                            ),
                        });
                        return Err(problems);
                    }
                },
                Err(_) => {
                    problems.push(Problem {
                        code: codes::INVALID_SCRIPTS.to_owned(),
                        item: INDEX.to_owned(),
                        message: "the index is not valid UTF-8".to_owned(),
                    });
                    return Err(problems);
                }
            },
        };
        let mut sources = BTreeMap::new();
        for (path, bytes) in files {
            if path == INDEX {
                continue;
            }
            let Some(name) = path.strip_suffix(".ts") else {
                problems.push(Problem {
                    code: codes::INVALID_SCRIPTS.to_owned(),
                    item: path.clone(),
                    message: "unexpected file in the scripts section (only `.ts` files and \
                              `scripts.toml` are allowed)"
                        .to_owned(),
                });
                continue;
            };
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    sources.insert(name.to_owned(), text.to_owned());
                }
                Err(_) => problems.push(Problem {
                    code: codes::INVALID_SCRIPTS.to_owned(),
                    item: path.clone(),
                    message: "the source is not valid UTF-8".to_owned(),
                }),
            }
        }
        if problems.is_empty() {
            Ok((index, sources))
        } else {
            Err(problems)
        }
    }
}

/// The apps named by a scripts section's index, for compatibility attribution.
pub(crate) fn script_apps(files: &Files) -> Vec<String> {
    ScriptsSection
        .parse(files)
        .map(|(index, _)| index.scripts.into_iter().map(|entry| entry.app).collect())
        .unwrap_or_default()
}

impl Section for ScriptsSection {
    fn kind(&self) -> &'static str {
        "scripts"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn risk(&self, action: ChangeAction) -> Risk {
        match action {
            ChangeAction::Add => Risk::Low,
            ChangeAction::Update => Risk::Medium,
            ChangeAction::Remove => Risk::Medium,
        }
    }

    fn check(&self, files: &Files) -> Vec<Problem> {
        let (index, sources) = match self.parse(files) {
            Ok(parsed) => parsed,
            Err(problems) => return problems,
        };
        let mut problems = Vec::new();
        let mut seen = BTreeMap::new();
        for entry in &index.scripts {
            if seen.insert(entry.name.clone(), ()).is_some() {
                problems.push(Problem {
                    code: codes::INVALID_SCRIPTS.to_owned(),
                    item: entry.name.clone(),
                    message: "the script appears twice in the index".to_owned(),
                });
            }
            let Some(source) = sources.get(&entry.name) else {
                problems.push(Problem {
                    code: codes::INVALID_SCRIPTS.to_owned(),
                    item: source_file(&entry.name),
                    message: "the index names this script but its source is not in the bundle"
                        .to_owned(),
                });
                continue;
            };
            let computed = crate::digest::sha256_hex(source.as_bytes());
            if computed != entry.hash {
                problems.push(Problem {
                    code: codes::INVALID_SCRIPTS.to_owned(),
                    item: entry.name.clone(),
                    message: format!(
                        "the recorded hash {} does not match the source's {computed}",
                        entry.hash
                    ),
                });
                continue;
            }
            // Re-transpiling the source (never running it) catches a script the engine could
            // not build; the output is discarded.
            if let Err(diagnostics) =
                spot_ts_transpile::transpile(source, &source_file(&entry.name), &Options::default())
            {
                let first = diagnostics.first().map_or_else(
                    || "the script did not transpile".to_owned(),
                    |d| format!("{}: {}", d.code, d.message),
                );
                problems.push(Problem {
                    code: codes::INVALID_SCRIPTS.to_owned(),
                    item: entry.name.clone(),
                    message: format!("the script did not transpile: {first}"),
                });
            }
        }
        for name in sources.keys() {
            if !index.scripts.iter().any(|entry| &entry.name == name) {
                problems.push(Problem {
                    code: codes::INVALID_SCRIPTS.to_owned(),
                    item: source_file(name),
                    message: "the source is not named in the index".to_owned(),
                });
            }
        }
        problems
    }

    fn capture(&self, site: &dyn SiteReader) -> Files {
        let mut records: Vec<ScriptRecord> = site
            .scripts()
            .into_iter()
            .filter(|record| {
                !site.is_system_managed(self.kind(), &record.name)
                    && !site.is_secret(self.kind(), &record.name)
            })
            .collect();
        records.sort_by(|a, b| a.name.cmp(&b.name));
        let mut files = Files::new();
        let mut index = ScriptsIndex::default();
        for record in records {
            files.insert(source_file(&record.name), record.source.into_bytes());
            index.scripts.push(ScriptEntry {
                name: record.name,
                app: record.app,
                hook_point: record.hook_point,
                hash: record.hash,
            });
        }
        if let Ok(text) = toml::to_string_pretty(&index) {
            files.insert(INDEX.to_owned(), text.into_bytes());
        }
        files
    }

    fn items(&self, files: &Files) -> BTreeMap<String, Vec<u8>> {
        let mut items = BTreeMap::new();
        if let Ok((index, sources)) = self.parse(files) {
            for entry in index.scripts {
                let source = sources.get(&entry.name).cloned().unwrap_or_default();
                items.insert(entry.name.clone(), item_bytes(&entry, &source));
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
            ChangeAction::Remove => site.remove_script(&change.key),
            _ => {
                let (index, sources) = self.parse(files).map_err(|problems| {
                    let problem = problems.into_iter().next().unwrap_or(Problem {
                        code: codes::APPLY_FAILED.to_owned(),
                        item: change.key.clone(),
                        message: "the scripts section is invalid".to_owned(),
                    });
                    CustomBundleError::from_problem(problem)
                })?;
                let entry = index
                    .scripts
                    .into_iter()
                    .find(|entry| entry.name == change.key)
                    .ok_or_else(|| {
                        CustomBundleError::about(
                            codes::APPLY_FAILED,
                            &change.key,
                            "the script is not in the bundle's index",
                        )
                    })?;
                let source = sources.get(&change.key).cloned().unwrap_or_default();
                site.upsert_script(ScriptRecord {
                    name: entry.name,
                    app: entry.app,
                    hook_point: entry.hook_point,
                    source,
                    hash: entry.hash,
                })
            }
        }
    }
}
