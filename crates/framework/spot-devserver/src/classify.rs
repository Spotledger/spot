//! Classifying a normalised event into the reload step that handles it.
//!
//! The layout the dev server watches is the app layout WP-DX-01 scaffolds: metadata under
//! `meta/` (or the DocType authoring folder `doctypes/`), views under `views/`, scripts under
//! `ts/`, components as `*.wasm`, Rust sources and `Cargo.toml`, fixtures under `fixtures/` and
//! assets under `assets/`/`public/`/`static/`.
//!
//! Classification tries the **directory** first (so a `.toml` under `views/` is a view and not
//! metadata), then the **extension** as a fallback for an unfamiliar tree. Anything else is
//! [`ChangeKind::Other`], which the pipeline reports as an unhandled change rather than
//! guessing.

use camino::{Utf8Path, Utf8PathBuf};

use crate::reload::StepName;
use crate::watch::{Op, RawEvent};

/// What kind of source a change touched.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// DocType metadata (`meta/*.toml`, `doctypes/*.toml`).
    Metadata,
    /// A view definition (`views/*.toml`).
    View,
    /// A TypeScript script (`ts/*.ts`).
    Script,
    /// A built WebAssembly component (`*.wasm`).
    Component,
    /// A Rust source or manifest that needs a project build.
    Rust,
    /// A fixture file.
    Fixture,
    /// A static asset.
    Asset,
    /// Anything else.
    Other,
}

impl ChangeKind {
    /// The lowercase name used in logs and status JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::View => "view",
            Self::Script => "script",
            Self::Component => "component",
            Self::Rust => "rust",
            Self::Fixture => "fixture",
            Self::Asset => "asset",
            Self::Other => "other",
        }
    }

    /// The step that handles this kind, or `None` for [`ChangeKind::Other`].
    #[must_use]
    pub fn step(self) -> Option<StepName> {
        match self {
            Self::Metadata => Some(StepName::Metadata),
            Self::View => Some(StepName::Views),
            Self::Script => Some(StepName::Scripts),
            Self::Component | Self::Rust => Some(StepName::Components),
            Self::Fixture => Some(StepName::Fixtures),
            Self::Asset => Some(StepName::Assets),
            Self::Other => None,
        }
    }
}

/// One classified change: the event, the path relative to the watched root, and its kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The path the event named.
    pub path: Utf8PathBuf,
    /// The path relative to the watched root (the event path when it is not under it).
    pub relative: Utf8PathBuf,
    /// What happened.
    pub op: Op,
    /// What kind of source it touched.
    pub kind: ChangeKind,
}

/// The directory names the classifier recognises.
#[derive(Clone, Debug)]
pub struct Classifier {
    /// Directories holding DocType metadata.
    pub metadata_dirs: Vec<String>,
    /// Directories holding view definitions.
    pub view_dirs: Vec<String>,
    /// Directories holding TypeScript scripts.
    pub script_dirs: Vec<String>,
    /// Directories holding fixtures.
    pub fixture_dirs: Vec<String>,
    /// Directories holding static assets.
    pub asset_dirs: Vec<String>,
}

impl Default for Classifier {
    fn default() -> Self {
        Self {
            metadata_dirs: vec!["meta".into(), "doctypes".into()],
            view_dirs: vec!["views".into()],
            script_dirs: vec!["ts".into(), "scripts".into()],
            fixture_dirs: vec!["fixtures".into()],
            asset_dirs: vec!["assets".into(), "public".into(), "static".into()],
        }
    }
}

/// Asset extensions that are not otherwise classified.
const ASSET_EXTENSIONS: &[&str] = &[
    "css", "scss", "html", "svg", "png", "jpg", "jpeg", "gif", "ico", "woff", "woff2",
];

impl Classifier {
    /// The default app layout.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Classifies `event` against `root`.
    #[must_use]
    pub fn classify(&self, event: &RawEvent, root: &Utf8Path) -> Change {
        let relative = crate::paths::relative_to(root, &event.path);
        Change {
            kind: self.kind_of(&relative),
            path: event.path.clone(),
            relative,
            op: event.op,
        }
    }

    /// The kind of `relative`, by directory first, then extension.
    #[must_use]
    pub fn kind_of(&self, relative: &Utf8Path) -> ChangeKind {
        let extension = relative
            .extension()
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        let file_name = relative.file_name().unwrap_or_default();
        let in_dirs = |dirs: &[String]| {
            relative.components().any(|component| match component {
                camino::Utf8Component::Normal(name) => dirs.iter().any(|dir| dir == name),
                _ => false,
            })
        };

        if in_dirs(&self.metadata_dirs) {
            return ChangeKind::Metadata;
        }
        if in_dirs(&self.view_dirs) {
            return ChangeKind::View;
        }
        if in_dirs(&self.script_dirs) && matches!(extension.as_str(), "ts" | "tsx" | "mts" | "cts")
        {
            return ChangeKind::Script;
        }
        if extension == "wasm" {
            return ChangeKind::Component;
        }
        if extension == "rs" || file_name == "Cargo.toml" || file_name == concat!("build", ".rs") {
            return ChangeKind::Rust;
        }
        if in_dirs(&self.fixture_dirs) {
            return ChangeKind::Fixture;
        }
        if in_dirs(&self.asset_dirs) || ASSET_EXTENSIONS.contains(&extension.as_str()) {
            return ChangeKind::Asset;
        }
        match extension.as_str() {
            "toml" => ChangeKind::Metadata,
            "ts" | "tsx" | "mts" | "cts" => ChangeKind::Script,
            _ => ChangeKind::Other,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn classify(relative: &str) -> ChangeKind {
        Classifier::new().kind_of(Utf8Path::new(relative))
    }

    #[test]
    fn directory_wins_over_extension() {
        assert_eq!(classify("views/widget/form.toml"), ChangeKind::View);
        assert_eq!(classify("meta/widget.toml"), ChangeKind::Metadata);
        assert_eq!(classify("doctypes/widget.toml"), ChangeKind::Metadata);
        assert_eq!(classify("ts/main.ts"), ChangeKind::Script);
        assert_eq!(classify("fixtures/seed.json"), ChangeKind::Fixture);
        assert_eq!(classify("assets/logo.svg"), ChangeKind::Asset);
    }

    #[test]
    fn extension_fallback_for_an_unfamiliar_tree() {
        assert_eq!(classify("widget/thing.toml"), ChangeKind::Metadata);
        assert_eq!(classify("thing.ts"), ChangeKind::Script);
        assert_eq!(classify("app.wasm"), ChangeKind::Component);
        assert_eq!(classify("src/lib.rs"), ChangeKind::Rust);
        assert_eq!(classify("Cargo.toml"), ChangeKind::Rust);
        assert_eq!(classify("logo.css"), ChangeKind::Asset);
        assert_eq!(classify("README"), ChangeKind::Other);
    }

    #[test]
    fn classify_uses_the_root_for_the_relative_path() {
        let root = Utf8Path::new("/scratch/dev");
        let event = RawEvent::new("/scratch/dev/views/form.toml", Op::Modified);
        let change = Classifier::new().classify(&event, root);
        assert_eq!(change.relative, Utf8Path::new("views/form.toml"));
        assert_eq!(change.kind, ChangeKind::View);
    }
}
