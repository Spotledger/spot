//! The template source: files under `<root>/mail/<name>/`, searched in root order.
//!
//! A root is a folder that holds a `mail` folder: the site's template folder first, then each
//! installed app's `templates` folder. The first root with a folder of the template's name wins,
//! so a site overrides an app. Every name is validated before any read; a link, anything that is
//! not a regular file and a file over 256 KiB are refused. Each load reads the file again and
//! versions it by a hash of its bytes, so the engine recompiles exactly when the content changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_template::{TemplateSource, Version};

use crate::names::{is_file_name, is_template_name};

/// The largest template file, in bytes: 256 KiB.
pub const MAX_TEMPLATE_BYTES: u64 = 256 * 1024;

/// The folder under a root that holds the mail templates.
pub(crate) const MAIL_DIR: &str = "mail";

/// Why a template file was not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The path does not exist (not an error: the next candidate is tried).
    Missing,
    /// A symbolic link.
    Link,
    /// A directory, a device or another special file.
    NotAFile,
    /// Over [`MAX_TEMPLATE_BYTES`].
    TooLarge,
    /// The bytes are not UTF-8.
    NotUtf8,
    /// Any other read failure.
    Unreadable,
}

impl Refusal {
    /// The fixed reason word of [`crate::RenderError::FileRefused`].
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Missing | Self::Unreadable => "unreadable",
            Self::Link => "link",
            Self::NotAFile => "not_a_file",
            Self::TooLarge => "too_large",
            Self::NotUtf8 => "not_utf8",
        }
    }
}

/// The template roots, in priority order.
#[derive(Debug, Clone, Default)]
pub struct TemplateRoots {
    roots: Arc<Vec<PathBuf>>,
}

impl TemplateRoots {
    /// The roots in the order they are searched: the site's template folder first, then each
    /// app's `templates` folder.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots: Arc::new(roots),
        }
    }

    /// The roots, in search order.
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The folder of template `name` in the first root that has one, or `None`. The name must
    /// already be validated; a root whose `mail/<name>` is a link or a file is skipped.
    pub(crate) fn template_dir(&self, name: &str) -> Option<PathBuf> {
        if !is_template_name(name) {
            return None;
        }
        self.roots
            .iter()
            .map(|root| root.join(MAIL_DIR).join(name))
            .find(|dir| is_plain_dir(dir))
    }

    /// Reads `file` of template `name`, applying every refusal rule. `Ok` carries the text and
    /// its content version.
    pub(crate) fn read(&self, name: &str, file: &str) -> Result<(String, Version), Refusal> {
        if !is_file_name(file) {
            return Err(Refusal::Missing);
        }
        let dir = self.template_dir(name).ok_or(Refusal::Missing)?;
        read_plain_file(&dir.join(file))
    }

    /// Whether `file` of template `name` exists as a usable file: `Ok(true)`; absent:
    /// `Ok(false)`; a refused file is an `Err`.
    pub(crate) fn probe(&self, dir: &Path, file: &str) -> Result<bool, Refusal> {
        match check_plain_file(&dir.join(file)) {
            Ok(()) => Ok(true),
            Err(Refusal::Missing) => Ok(false),
            Err(other) => Err(other),
        }
    }
}

fn is_plain_dir(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type().is_dir(),
        Err(_) => false,
    }
}

/// Metadata checks of one file, without reading it.
fn check_plain_file(path: &Path) -> Result<(), Refusal> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Refusal::Missing);
        }
        Err(_) => return Err(Refusal::Unreadable),
    };
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Err(Refusal::Link);
    }
    if !kind.is_file() {
        return Err(Refusal::NotAFile);
    }
    if meta.len() > MAX_TEMPLATE_BYTES {
        return Err(Refusal::TooLarge);
    }
    Ok(())
}

fn read_plain_file(path: &Path) -> Result<(String, Version), Refusal> {
    check_plain_file(path)?;
    let bytes = fs::read(path).map_err(|_| Refusal::Unreadable)?;
    // The file may have grown between the check and the read.
    if bytes.len() as u64 > MAX_TEMPLATE_BYTES {
        return Err(Refusal::TooLarge);
    }
    let version = Version::new(fnv1a(&bytes));
    let text = String::from_utf8(bytes).map_err(|_| Refusal::NotUtf8)?;
    Ok((text, version))
}

/// FNV-1a over the bytes: a stable content version (the std hasher is randomised per process).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The engine's view: names are `<template>/<file>`; anything else is not found.
pub(crate) struct MailSource {
    pub(crate) roots: TemplateRoots,
}

impl TemplateSource for MailSource {
    fn load(&self, name: &str) -> Option<(String, Version)> {
        let (template, file) = name.split_once('/')?;
        self.roots.read(template, file).ok()
    }
}
