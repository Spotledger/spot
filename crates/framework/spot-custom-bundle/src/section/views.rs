//! The `views` section: one TOML per [`ViewLayer`](spot_views::ViewLayer).
//!
//! Only `site`, `role` and `step` layers are carried. A `standard` layer is the app's own
//! view, never a site customization, so capture drops it and apply would not know a target to
//! put it in.

use std::collections::BTreeMap;

use spot_views::{LayerKind, LayerName, ViewLayer};

use crate::error::{CustomBundleError, codes};
use crate::paths::sanitize;
use crate::registry::{
    Change, ChangeAction, Files, Problem, Risk, Section, SiteReader, SiteWriter,
};

/// The `views` section.
pub struct ViewsSection;

/// The short name of a layer kind.
fn kind_name(name: LayerName) -> &'static str {
    match name {
        LayerName::Standard => "standard",
        LayerName::Site => "site",
        LayerName::Role => "role",
        LayerName::Step => "step",
    }
}

/// The layer's scope: the role or step name, or an empty string.
fn scope(layer: &ViewLayer) -> &str {
    layer
        .kind
        .role
        .as_deref()
        .or(layer.kind.step.as_deref())
        .unwrap_or("")
}

/// The file name for a layer.
fn file_name(layer: &ViewLayer) -> String {
    let stem = format!(
        "{}.{}",
        sanitize(&layer.doctype),
        kind_name(layer.kind.kind)
    );
    match scope(layer) {
        "" => format!("{stem}.toml"),
        named => format!("{stem}.{}.toml", sanitize(named)),
    }
}

/// The stable item key of a layer.
fn item_key(layer: &ViewLayer) -> String {
    let stem = format!("{}.{}", layer.doctype, kind_name(layer.kind.kind));
    match scope(layer) {
        "" => stem,
        named => format!("{stem}.{named}"),
    }
}

/// The layer kind a key names, or `None` for a malformed key.
fn kind_of(key: &str) -> Option<(String, LayerKind)> {
    let mut parts = key.splitn(3, '.');
    let doctype = parts.next()?.to_owned();
    let name = parts.next()?;
    let named = parts.next().map(str::to_owned);
    let kind = match name {
        "site" => LayerKind::named(LayerName::Site),
        "role" => {
            let role = named?;
            LayerKind {
                kind: LayerName::Role,
                role: Some(role),
                step: None,
            }
        }
        "step" => {
            let step = named?;
            LayerKind {
                kind: LayerName::Step,
                role: None,
                step: Some(step),
            }
        }
        _ => return None,
    };
    Some((doctype, kind))
}

/// Parses a views section's files into layers. Used by apply's validation, which resolves and
/// validates every layer against the target's metadata before anything is written.
pub(crate) fn layers_from_files(files: &Files) -> Result<Vec<ViewLayer>, Vec<Problem>> {
    ViewsSection.parse_files(files)
}

impl ViewsSection {
    /// Parses every file into a layer, reporting the first failure per file.
    fn parse_files(&self, files: &Files) -> Result<Vec<ViewLayer>, Vec<Problem>> {
        let mut layers = Vec::new();
        let mut problems = Vec::new();
        for (path, bytes) in files {
            match std::str::from_utf8(bytes) {
                Ok(text) => match ViewLayer::parse(path, text) {
                    Ok(layer) => {
                        if layer.kind.kind == LayerName::Standard {
                            problems.push(Problem {
                                code: codes::INVALID_VIEWS.to_owned(),
                                item: path.clone(),
                                message: "a bundle never carries a `standard` layer".to_owned(),
                            });
                        } else {
                            layers.push(layer);
                        }
                    }
                    Err(errors) => {
                        for error in errors {
                            problems.push(Problem {
                                code: codes::INVALID_VIEWS.to_owned(),
                                item: path.clone(),
                                message: error.to_string(),
                            });
                        }
                    }
                },
                Err(_) => problems.push(Problem {
                    code: codes::INVALID_VIEWS.to_owned(),
                    item: path.clone(),
                    message: "the file is not valid UTF-8".to_owned(),
                }),
            }
        }
        if problems.is_empty() {
            Ok(layers)
        } else {
            Err(problems)
        }
    }
}

impl Section for ViewsSection {
    fn kind(&self) -> &'static str {
        "views"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn risk(&self, action: ChangeAction) -> Risk {
        match action {
            ChangeAction::Add => Risk::Low,
            ChangeAction::Update => Risk::Low,
            ChangeAction::Remove => Risk::Medium,
        }
    }

    fn check(&self, files: &Files) -> Vec<Problem> {
        let layers = match self.parse_files(files) {
            Ok(layers) => layers,
            Err(problems) => return problems,
        };
        let mut problems = Vec::new();
        let mut seen = BTreeMap::new();
        for layer in &layers {
            let key = item_key(layer);
            if seen.insert(key.clone(), ()).is_some() {
                problems.push(Problem {
                    code: codes::INVALID_VIEWS.to_owned(),
                    item: key.clone(),
                    message: "the view layer appears twice".to_owned(),
                });
            }
            let expected = file_name(layer);
            if !files.contains_key(&expected) {
                problems.push(Problem {
                    code: codes::INVALID_VIEWS.to_owned(),
                    item: expected,
                    message: format!("the layer's canonical file name is missing for {key}"),
                });
            }
        }
        problems
    }

    fn capture(&self, site: &dyn SiteReader) -> Files {
        let mut layers: Vec<ViewLayer> = site
            .view_layers()
            .into_iter()
            .filter(|layer| layer.kind.kind != LayerName::Standard)
            .filter(|layer| {
                let key = item_key(layer);
                !site.is_system_managed(self.kind(), &key) && !site.is_secret(self.kind(), &key)
            })
            .collect();
        layers.sort_by(|a, b| {
            (
                &a.doctype,
                a.kind.rank(),
                a.kind.role.as_deref().unwrap_or(""),
                a.kind.step.as_deref().unwrap_or(""),
            )
                .cmp(&(
                    &b.doctype,
                    b.kind.rank(),
                    b.kind.role.as_deref().unwrap_or(""),
                    b.kind.step.as_deref().unwrap_or(""),
                ))
        });
        let mut files = Files::new();
        for layer in layers {
            if let Ok(text) = layer.to_toml() {
                files.insert(file_name(&layer), text.into_bytes());
            }
        }
        files
    }

    fn items(&self, files: &Files) -> BTreeMap<String, Vec<u8>> {
        let mut items = BTreeMap::new();
        if let Ok(layers) = self.parse_files(files) {
            for layer in layers {
                if let Ok(text) = layer.to_toml() {
                    items.insert(item_key(&layer), text.into_bytes());
                }
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
        let Some((doctype, kind)) = kind_of(&change.key) else {
            return Err(CustomBundleError::about(
                codes::APPLY_FAILED,
                &change.key,
                "the view layer key is malformed",
            ));
        };
        match change.action {
            ChangeAction::Remove => site.remove_view_layer(&doctype, &kind),
            _ => {
                let layer = self
                    .parse_files(files)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|layer| item_key(layer) == change.key)
                    .ok_or_else(|| {
                        CustomBundleError::about(
                            codes::APPLY_FAILED,
                            &change.key,
                            "the view layer is not in the bundle's views files",
                        )
                    })?;
                site.upsert_view_layer(layer)
            }
        }
    }
}
