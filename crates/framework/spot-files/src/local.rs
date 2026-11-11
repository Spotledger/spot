//! [`LocalFileStore`]: the default provider for desktop and single-server installs.
//!
//! `put` writes `root/<site>/<key>`: the key's directory segments become real directories
//! and its last segment becomes the object file name with a `~` suffix (see below).
//! `<site>` is a validated [`SiteId`] (safe as a directory name); `<key>` is a validated
//! [`FileKey`], so no key can resolve outside its site directory — `..` segments, absolute
//! paths and hidden names are unrepresentable. Debug builds additionally assert
//! canonical-path containment at runtime.
//!
//! # Coexisting keys (encoded object names)
//!
//! Any set of valid keys can coexist as in object stores, including a key that is a
//! directory of another: `a`, `a/b` and `a/b/c` all live at once (lead review of #423; the
//! S3 and Azure providers of WP-PLT-10.T2 treat keys as opaque and do the same). A plain
//! layout cannot offer this — `a` would have to be a file and a directory at once — so the
//! object file is named after the key's last segment plus `~`. `~` is outside the key
//! charset (`a-z 0-9 . _ -`), so an encoded name can never be a key segment, never a
//! directory another key created, and the encoding is one-to-one: `b~` decodes back to the
//! key's last segment unambiguously. Every operation encodes; `walk`, behind `list`,
//! decodes.
//!
//! # Atomic writes
//!
//! A `put` streams into a hidden temporary file in the target directory, `fsync`s it, then
//! commits it: `rename` when overwriting (an atomic replace on both platforms — Windows
//! runs it with `MOVEFILE_REPLACE_EXISTING`) or `hard_link` when `overwrite = false` (the
//! link fails with `AlreadyExists` from the OS instead of racing a separate existence
//! check). Readers therefore see the old content or the new, never a partial file; any
//! failure before the commit removes the temporary.
//!
//! `overwrite = false` relies on hard links; on a filesystem without them (exFAT, FAT32,
//! some SMB shares) such a `put` fails with `files.io` — documented for the lead review of
//! #423, to revisit if such installs turn out to matter.
//!
//! # Metadata: sidecar for the content type, hash derived on read
//!
//! The content type cannot be derived from the bytes, so it lives in a hidden sidecar
//! (`.<object name>.meta`) next to the object; the sidecar name starts with `.`, which no
//! valid key segment can, so it can never collide with an object and listings skip hidden
//! names. The SHA-256, however, is **derived**: `head` and `get` stream the content file
//! through SHA-256 when answering. Why deriving instead of caching the hash in the sidecar:
//! two independent files cannot be committed atomically as a pair, so a racing overwrite
//! can interleave another writer's sidecar between its content commit and leave a *stale
//! hash beside new bytes* — a state the contract forbids. Deriving the hash from the
//! content file makes disagreement between metadata and bytes unrepresentable. The cost is
//! one re-read per `head`; local installs hold modest attachments, and if profiling ever
//! says it matters the memoisation belongs in spot-cache, not in a second copy on disk.
//!
//! # Crash window
//!
//! Content temp → fsync → commit content (the point of no return) → commit sidecar
//! atomically. If the process dies between the two commits, the object exists but has no
//! (or, after an overwrite, stale) content type and reads as absent until the next put to
//! the same key lands, which rewrites the sidecar. Size and hash always describe the real
//! bytes because they come from the file itself.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use spot_core::SiteId;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};

use crate::error::FileError;
use crate::key::{FileKey, FileKeyPrefix};
use crate::store::{
    Body, FileFuture, FileMeta, FileResult, FileStore, PutOptions, StoredFile, stream_hashed,
};

/// Monotonic per-process counter mixed into temporary file names (no RNG dependency is
/// allowed in this crate; pid + sequence is collision-free for stores on one machine).
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// File bytes on the local disk: `root/<site>/<key>`.
#[derive(Debug, Clone)]
pub struct LocalFileStore {
    root: PathBuf,
}

/// The character every stored object's file name ends with: `~` is outside the key charset
/// (`a-z 0-9 . _ -`), so an encoded object name can never equal a key segment, and a
/// key `a` never collides with a directory an object `a/b` needs. See the module docs.
const OBJECT_SUFFIX: char = '~';

impl LocalFileStore {
    /// Opens the store rooted at `root`. Directories are created on demand; nothing is
    /// read at construction, so "after a restart" is just another call to `new`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The configured root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn site_dir(&self, site: &SiteId) -> PathBuf {
        self.root.join(site.as_str())
    }

    /// The encoded object file name for `key`: its last segment plus [`OBJECT_SUFFIX`].
    /// One-to-one (keys never contain `~`); `walk` strips the suffix again.
    fn object_name(key: &FileKey) -> String {
        let rest = key.rest();
        let last = match rest.rsplit_once('/') {
            Some((_, last)) => last,
            None => rest,
        };
        format!("{last}{OBJECT_SUFFIX}")
    }

    fn content_path(&self, site: &SiteId, key: &FileKey) -> PathBuf {
        // The key's directory segments, then the encoded object name — so a key and a key
        // it is a directory of (for example `a` and `a/b`) can both exist (module docs).
        // The full key maps under the site directory; `rest()` alone would drop the
        // visibility segment.
        let dir = Path::new(key.as_str()).parent().unwrap_or(Path::new(""));
        self.site_dir(site).join(dir).join(Self::object_name(key))
    }

    fn sidecar_path(target: &Path) -> FileResult<PathBuf> {
        let name = target.file_name().ok_or(FileError::InvalidKey {
            reason: "the key has no file name",
        })?;
        let name = name.to_str().ok_or(FileError::io("non-UTF-8 file name"))?;
        Ok(target.with_file_name(format!(".{name}.meta")))
    }

    fn temp_name(kind: &str) -> String {
        format!(
            ".tmp.{kind}.{}.{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Debug-build containment assertion: a valid key's path is under the site dir by
    /// construction; verify there are no parent/root components and that the site
    /// directory really resolves under the root (catches a symlinked site dir).
    async fn assert_contained(&self, site: &SiteId, key: &FileKey) -> FileResult<()> {
        if !cfg!(debug_assertions) {
            return Ok(());
        }
        let rel = Path::new(key.as_str());
        if rel.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(FileError::InvalidKey {
                reason: "a key resolved outside its site directory",
            });
        }
        let root_canon = match tokio::fs::canonicalize(&self.root).await {
            Ok(p) => p,
            // Nothing is stored yet: containment holds trivially.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(FileError::io(format!("canonicalising root: {e}"))),
        };
        let site_canon = match tokio::fs::canonicalize(self.site_dir(site)).await {
            Ok(p) => p,
            // The site has no directory yet: nothing can have escaped it, and the actual
            // operation reports NotFound.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(FileError::io(format!("canonicalising site dir: {e}"))),
        };
        if !site_canon.starts_with(&root_canon) {
            return Err(FileError::io("site directory escapes the store root"));
        }
        // The same defence for the object's directory: a symlink planted inside the site
        // directory would canonicalise outside it. A fresh key has no directory yet; then
        // containment holds trivially.
        let object_dir = self.content_path(site, key);
        let Some(object_dir) = object_dir.parent() else {
            return Ok(());
        };
        match tokio::fs::canonicalize(object_dir).await {
            Ok(dir_canon) => {
                if !dir_canon.starts_with(&site_canon) {
                    return Err(FileError::io("object directory escapes the site directory"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(FileError::io(format!("canonicalising object dir: {e}"))),
        }
        Ok(())
    }

    /// Reads the sidecar's content type. `Ok(None)` when absent: the object is then
    /// invisible (the sidecar is what makes a committed object describable).
    async fn read_sidecar(&self, site: &SiteId, key: &FileKey) -> FileResult<Option<String>> {
        let path = Self::sidecar_path(&self.content_path(site, key))?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|e| FileError::io(format!("sidecar not UTF-8: {e}")))?;
                let mut lines = text.splitn(2, '\n');
                match (lines.next(), lines.next()) {
                    (Some("spot-files v1"), Some(content_type)) => Ok(Some(
                        content_type.trim_end_matches(['\n', '\r']).to_string(),
                    )),
                    _ => Err(FileError::io(format!("corrupt sidecar for key {key}"))),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Opens the content file once and derives everything about the object from that one
    /// open handle: content type from the sidecar, size, mtime and SHA-256 streamed through
    /// the handle (hash-derivation rationale in the module docs). Returns the handle rewound
    /// to its start, so `get` can hand the very file it has just described to the caller. A
    /// second open would instead race a concurrent overwrite and attach new bytes to old
    /// metadata — the exact failure the race probe in `tests/local.rs` watches for (lead
    /// review of #423).
    async fn open_described(
        &self,
        site: &SiteId,
        key: &FileKey,
    ) -> FileResult<Option<(FileMeta, tokio::fs::File)>> {
        let Some(content_type) = self.read_sidecar(site, key).await? else {
            // The sidecar is what makes a committed object describable.
            return Ok(None);
        };
        let mut file = match tokio::fs::File::open(self.content_path(site, key)).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A delete or a sidecar-repair race removed the content between the two
                // commits: the object reads as absent.
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let stat = file.metadata().await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        // Back to the beginning: the caller receives the handle expecting the body.
        file.seek(SeekFrom::Start(0)).await?;
        let modified = stat
            .modified()
            .map(time::OffsetDateTime::from)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        Ok(Some((
            FileMeta {
                key: key.clone(),
                size: stat.len(),
                sha256: hasher.finalize().into(),
                content_type,
                modified,
            },
            file,
        )))
    }

    /// Metadata for `key`: size and mtime from the open file, content type from the
    /// sidecar, SHA-256 streamed from the content file (see module docs for the choice).
    /// The handle is dropped; only the description is returned.
    async fn meta_of(&self, site: &SiteId, key: &FileKey) -> FileResult<Option<FileMeta>> {
        Ok(self
            .open_described(site, key)
            .await?
            .map(|(meta, _unused)| meta))
    }

    /// Collects object keys (relative, `/`-joined, decoded) under `dir`. Files whose name
    /// ends with [`OBJECT_SUFFIX`] are objects and lose exactly that character; hidden
    /// names — temp and sidecar files — and any other stray file are skipped, as are
    /// hidden directories (none are created).
    fn walk<'a>(
        &'a self,
        dir: &'a Path,
        rel: &'a str,
        out: &'a mut Vec<String>,
    ) -> FileFuture<'a, FileResult<()>> {
        Box::pin(async move {
            let mut entries = tokio::fs::read_dir(dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let text = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                let path = entry.path();
                if entry.file_type().await?.is_dir() {
                    if !name.starts_with('.') {
                        self.walk(&path, &text, out).await?;
                    }
                } else if !name.starts_with('.') && name.ends_with(OBJECT_SUFFIX) {
                    // The suffix is at the end and appears nowhere else in a valid key, so
                    // stripping one character decodes the name (module docs).
                    out.push(text[..text.len() - 1].to_owned());
                }
            }
            Ok(())
        })
    }
}

impl FileStore for LocalFileStore {
    fn put<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
        mut body: Body,
        opts: PutOptions,
    ) -> FileFuture<'a, FileResult<StoredFile>> {
        Box::pin(async move {
            let target = self.content_path(site, &key);
            let dir = target
                .parent()
                .ok_or(FileError::InvalidKey {
                    reason: "the key has no parent directory",
                })?
                .to_path_buf();
            tokio::fs::create_dir_all(&dir).await?;
            self.assert_contained(site, &key).await?;

            // Cheap pre-check for a friendlier error before streaming; the real guarantee
            // is the OS-level hard_link below, so a racing put still loses cleanly. The
            // sidecar marks a committed object, not the path: a directory can exist here
            // because another key's object lives inside it, and that directory is not this
            // key's object (and the encoded target name of this key cannot be a directory).
            if !opts.overwrite && self.read_sidecar(site, &key).await?.is_some() {
                return Err(FileError::AlreadyExists {
                    key: key.as_str().to_string(),
                });
            }

            let temp = dir.join(Self::temp_name("f"));
            let (digest, size) = {
                let mut temp_file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp)
                    .await
                    .map_err(|e| FileError::io(format!("opening temp file: {e}")))?;
                match stream_hashed(&mut temp_file, &mut body, opts.max_bytes).await {
                    Ok((hash, size)) => {
                        if let Err(e) = temp_file.sync_all().await {
                            let _ = tokio::fs::remove_file(&temp).await;
                            return Err(FileError::io(format!("syncing temp: {e}")));
                        }
                        drop(temp_file);
                        (hash.0, size)
                    }
                    Err(e) => {
                        // TooLarge, a stream failure or a write error: the partial temp
                        // is removed, so nothing is left behind.
                        let _ = tokio::fs::remove_file(&temp).await;
                        return Err(e);
                    }
                }
            };

            // Commit the content: rename replaces atomically (Windows runs it with
            // MOVEFILE_REPLACE_EXISTING), hard_link refuses an existing target.
            let committed = if opts.overwrite {
                tokio::fs::rename(&temp, &target)
                    .await
                    .map_err(|e| FileError::io(format!("committing {key}: {e}")))
            } else {
                match tokio::fs::hard_link(&temp, &target).await {
                    Ok(()) => {
                        let _ = tokio::fs::remove_file(&temp).await;
                        Ok(())
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        Err(FileError::AlreadyExists {
                            key: key.as_str().to_string(),
                        })
                    }
                    Err(e) => Err(FileError::io(format!("committing {key}: {e}"))),
                }
            };
            if let Err(e) = committed {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(e);
            }

            // Sidecar second (content type only), written atomically over the old one.
            // Crash window documented in the module header.
            let sidecar = Self::sidecar_path(&target)?;
            let sidecar_temp = dir.join(Self::temp_name("m"));
            let sidecar_body = format!("spot-files v1\n{}\n", opts.content_type);
            write_atomic(&sidecar_temp, &sidecar, sidecar_body.as_bytes()).await?;

            tracing::debug!(site = %site, key = %key, size, "stored file");
            Ok(StoredFile {
                key,
                size,
                // The hash of the bytes this put wrote, computed while streaming into the
                // temp file that was renamed into place; `head` re-derives the same value
                // from the stored file.
                sha256: digest,
                content_type: opts.content_type,
            })
        })
    }

    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<(FileMeta, Body)>> {
        Box::pin(async move {
            self.assert_contained(site, &key).await?;
            // One open: the metadata (size, digest) and the returned body come from the
            // same handle, so a concurrent overwrite can never attach new bytes to the
            // old metadata (lead review of #423; race probe in tests/local.rs).
            let Some((meta, file)) = self.open_described(site, &key).await? else {
                return Err(FileError::NotFound {
                    key: key.as_str().to_string(),
                });
            };
            Ok((meta, Box::pin(file) as Body))
        })
    }

    fn head<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<Option<FileMeta>>> {
        Box::pin(async move {
            self.assert_contained(site, &key).await?;
            self.meta_of(site, &key).await
        })
    }

    fn delete<'a>(&'a self, site: &'a SiteId, key: FileKey) -> FileFuture<'a, FileResult<bool>> {
        Box::pin(async move {
            self.assert_contained(site, &key).await?;
            let target = self.content_path(site, &key);
            match tokio::fs::remove_file(&target).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e.into()),
            }
            let sidecar = Self::sidecar_path(&target)?;
            let _ = tokio::fs::remove_file(&sidecar).await;
            let site_dir = self.site_dir(site);
            prune_empty_dirs_up_to(&target, &site_dir).await;
            tracing::debug!(site = %site, key = %key, "deleted file");
            Ok(true)
        })
    }

    fn list<'a>(
        &'a self,
        site: &'a SiteId,
        prefix: &'a FileKeyPrefix,
        after: Option<&'a FileKey>,
        limit: usize,
    ) -> FileFuture<'a, FileResult<Vec<FileMeta>>> {
        Box::pin(async move {
            let site_dir = self.site_dir(site);
            if !tokio::fs::try_exists(&site_dir)
                .await
                .map_err(|e| FileError::io(format!("checking site dir: {e}")))?
            {
                return Ok(Vec::new());
            }
            let mut rel_keys = Vec::new();
            self.walk(&site_dir, "", &mut rel_keys).await?;
            rel_keys.sort();
            let mut out = Vec::new();
            for rel in rel_keys {
                let Ok((_, key)) = FileKey::parse(&rel) else {
                    continue; // stray files that are not keys are skipped
                };
                if !key.starts_with(prefix) {
                    continue;
                }
                if let Some(cursor) = after
                    && key.as_str() <= cursor.as_str()
                {
                    continue;
                }
                if out.len() == limit {
                    break;
                }
                if let Some(meta) = self.meta_of(site, &key).await? {
                    out.push(meta);
                }
            }
            Ok(out)
        })
    }
}

/// Writes `content` to `temp` with fsync, then renames over `final_path`.
async fn write_atomic(temp: &Path, final_path: &Path, content: &[u8]) -> FileResult<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .await
        .map_err(|e| FileError::io(format!("opening sidecar temp: {e}")))?;
    let result = async {
        file.write_all(content).await?;
        file.sync_all().await
    }
    .await;
    drop(file);
    if let Err(e) = result {
        let _ = tokio::fs::remove_file(temp).await;
        return Err(FileError::io(format!("writing sidecar: {e}")));
    }
    tokio::fs::rename(temp, final_path)
        .await
        .map_err(|e| FileError::io(format!("committing sidecar: {e}")))
}

/// After an object is removed, delete its empty parent chain back to (but never past)
/// the site directory `stop`. Best effort: every error is ignored, per the brief. A
/// sidecar left by a crash between commits counts as content, so a parent is never
/// pruned while metadata or data still lives in it.
async fn prune_empty_dirs_up_to(content: &Path, stop: &Path) {
    let mut current = content.parent().map(PathBuf::from);
    while let Some(dir) = current {
        if dir == stop || !dir.starts_with(stop) {
            break;
        }
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(_) => break,
        };
        match entries.next_entry().await {
            // Content (including a crashed put's leftovers) counts: keep the directory.
            Ok(Some(_)) => break,
            // Unreadable: the prune is best effort — stay conservative, keep the directory.
            Err(_) => break,
            // Empty: remove it, then consider its parent.
            Ok(None) => {}
        }
        // Empty: remove it, then consider its parent.
        if tokio::fs::remove_dir(&dir).await.is_err() {
            break;
        }
        current = dir.parent().map(PathBuf::from);
    }
}
