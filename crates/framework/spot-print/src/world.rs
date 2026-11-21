//! A Typst `World` over memory only: the rendered markup, the asset map and
//! the font set. No filesystem, network or package access.

use std::collections::BTreeMap;
use std::sync::Mutex;

use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, Source, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

use crate::fonts::FontSet;

/// Named binary assets (logo, QR image, ...) a template may reference by
/// relative path, e.g. `#image("logo.svg")`.
#[derive(Debug, Clone, Default)]
pub struct AssetMap {
    files: BTreeMap<String, Bytes>,
}

impl AssetMap {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) an asset; builder style. A leading `/` is ignored.
    #[must_use]
    pub fn with(mut self, name: &str, bytes: impl Into<Vec<u8>>) -> Self {
        self.files.insert(
            name.trim_start_matches('/').to_owned(),
            Bytes::new(bytes.into()),
        );
        self
    }

    pub(crate) fn get(&self, name: &str) -> Option<Bytes> {
        self.files.get(name).cloned()
    }
}

/// Fonts and the standard library, built once per renderer and shared by every
/// render.
pub(crate) struct Shared {
    pub(crate) library: LazyHash<Library>,
    pub(crate) book: LazyHash<FontBook>,
    pub(crate) fonts: Vec<Font>,
}

impl Shared {
    pub(crate) fn new(set: &FontSet) -> Self {
        let fonts = set.faces().to_vec();
        Self {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(FontBook::from_fonts(&fonts)),
            fonts,
        }
    }
}

/// One render's world.
pub(crate) struct PrintWorld<'a> {
    shared: &'a Shared,
    main: Source,
    assets: &'a AssetMap,
    today: Datetime,
    violations: Mutex<Vec<String>>,
}

impl<'a> PrintWorld<'a> {
    pub(crate) fn new(
        shared: &'a Shared,
        markup: String,
        assets: &'a AssetMap,
        today: Datetime,
    ) -> Self {
        Self {
            shared,
            main: Source::detached(markup),
            assets,
            today,
            violations: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn main_source(&self) -> &Source {
        &self.main
    }

    /// The first path a document asked for and was refused, if any.
    pub(crate) fn first_violation(&self) -> Option<String> {
        match self.violations.lock() {
            Ok(list) => list.first().cloned(),
            Err(poisoned) => poisoned.into_inner().first().cloned(),
        }
    }

    fn refuse(&self, id: FileId) -> FileError {
        let rooted = id.get();
        let name = match rooted.root() {
            VirtualRoot::Package(spec) => {
                format!("{spec}{}", rooted.vpath().get_with_slash())
            }
            VirtualRoot::Project => rooted.vpath().get_with_slash().to_owned(),
        };
        let error = FileError::NotFound(name.clone().into());
        match self.violations.lock() {
            Ok(mut list) => list.push(name),
            Err(poisoned) => poisoned.into_inner().push(name),
        }
        error
    }

    fn asset(&self, id: FileId) -> Option<Bytes> {
        let rooted = id.get();
        if !matches!(rooted.root(), VirtualRoot::Project) {
            return None;
        }
        self.assets.get(rooted.vpath().get_without_slash())
    }
}

impl World for PrintWorld<'_> {
    fn library(&self) -> &LazyHash<Library> {
        &self.shared.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.shared.book
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() {
            return Ok(self.main.clone());
        }
        match self.asset(id) {
            Some(bytes) => match String::from_utf8(bytes.to_vec()) {
                Ok(text) => Ok(Source::new(id, text)),
                Err(_) => Err(FileError::InvalidUtf8),
            },
            None => Err(self.refuse(id)),
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        self.asset(id).ok_or_else(|| self.refuse(id))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.shared.fonts.get(index).cloned()
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        // The document date is an input, so output never depends on the clock.
        // A requested UTC offset is ignored for the same reason.
        let _ = offset;
        Some(self.today)
    }
}
