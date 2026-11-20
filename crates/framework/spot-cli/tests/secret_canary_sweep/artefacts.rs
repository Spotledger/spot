//! The artefacts of the secret canary sweep (WP-SEC-03.T15): everything the site can produce,
//! kept as named byte strings, and the collectors that make the ones that are not just a
//! response body (a directory tree, a ZIP, a `pg_dump`).
//!
//! An artefact is named by where it came from (`export:ndjson`, `files:sites/<site>/site.toml`),
//! never by what it holds: the name is all a failure prints.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::canary::{Canary, Hit, scan};
use crate::common::Reply;

/// Files above this size are listed but not read: the sweep reads documents and logs, not data
/// files of gigabytes.
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// The named artefacts collected so far.
#[derive(Default)]
pub struct Artefacts {
    items: Vec<(String, Vec<u8>)>,
}

impl Artefacts {
    pub fn add(&mut self, name: impl Into<String>, bytes: Vec<u8>) {
        self.items.push((name.into(), bytes));
    }

    /// A response as the client saw it: the status, the headers and the body.
    pub fn add_reply(&mut self, name: &str, reply: &Reply) {
        let mut bytes = format!("HTTP {}\n", reply.status).into_bytes();
        for (key, value) in &reply.headers {
            bytes.extend_from_slice(format!("{key}: {value}\n").as_bytes());
        }
        bytes.extend_from_slice(&reply.bytes);
        self.add(name, bytes);
    }

    /// Every file under `dir` as `<prefix>/<relative path>`, never following a symbolic link and
    /// skipping the paths in `skip` (the key file itself).
    pub fn add_tree(&mut self, prefix: &str, dir: &Path, skip: &[PathBuf]) {
        let mut found = Vec::new();
        walk(dir, dir, skip, &mut found);
        found.sort();
        for (relative, path) in found {
            if let Ok(bytes) = std::fs::read(&path) {
                self.add(format!("{prefix}/{relative}"), bytes);
            }
        }
    }

    /// Names that were collected with no bytes at all, from `required`, plus the names of
    /// `required` that were never collected: a collector that silently returned nothing must
    /// fail the sweep, not pass it.
    pub fn missing(&self, required: &[&str]) -> Vec<String> {
        required
            .iter()
            .filter(|want| {
                !self
                    .items
                    .iter()
                    .any(|(name, bytes)| name == *want && !bytes.is_empty())
            })
            .map(|want| (*want).to_owned())
            .collect()
    }

    pub fn names(&self) -> Vec<(&str, usize)> {
        self.items
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.len()))
            .collect()
    }

    /// Every canary found in every artefact, in every form.
    pub fn sweep(&self, canaries: &[Canary]) -> Vec<Hit> {
        self.items
            .iter()
            .flat_map(|(name, bytes)| scan(name, bytes, canaries))
            .collect()
    }
}

fn walk(root: &Path, dir: &Path, skip: &[PathBuf], out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() || skip.iter().any(|s| s == &path) {
            continue;
        }
        if meta.is_dir() {
            walk(root, &path, skip, out);
        } else if meta.is_file() && meta.len() <= MAX_FILE_BYTES {
            let relative = path
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            out.push((relative, path));
        }
    }
}

/// The entries of the ZIP at `path`, as `(entry-<n>, bytes)`: the names are not read, so a
/// hostile entry name cannot reach a failure message.
pub fn zip_entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut out = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        out.push((format!("entry-{index}"), bytes));
    }
    out
}

/// A plain-format `pg_dump` of `database` on the server `url` names, run with the client tools
/// in `bin`. The password reaches the tool through its environment only. `Err` carries the exit
/// code, never the tool's output.
pub fn pg_dump(bin: &Path, url: &str, database: &str) -> Result<Vec<u8>, String> {
    let url = url::Url::parse(url).map_err(|_| "SPOT_TEST_PG_URL is not a URL".to_owned())?;
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())
        .ok_or("SPOT_TEST_PG_URL names no TCP host".to_owned())?
        .to_owned();
    let mut command = Command::new(bin.join("pg_dump"));
    command
        .arg("--host")
        .arg(&host)
        .arg("--port")
        .arg(url.port().unwrap_or(5432).to_string())
        .arg("--username")
        .arg(url.username())
        .arg("--no-owner")
        .arg("--no-privileges")
        .arg("--format=plain")
        .arg("--dbname")
        .arg(database);
    if let Some(password) = url.password() {
        command.env("PGPASSWORD", password);
    }
    let output = command
        .output()
        .map_err(|_| "pg_dump could not be started".to_owned())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!("pg_dump exited with {:?}", output.status.code()))
    }
}

/// The password of the server URL, when it is long enough to be a canary (a short one would
/// match ordinary text).
pub fn pg_password(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    url.password().filter(|p| p.len() >= 12).map(str::to_owned)
}
