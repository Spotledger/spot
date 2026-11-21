//! The backup package: one self-describing file a target stores and a restore reads.
//!
//! Layout:
//!
//! ```text
//! b"SPOTBAK1" | u64 manifest length | manifest TOML | u32 file count |
//! (u32 path length | path | u64 data length | data) ...
//! ```
//!
//! The manifest's `digest` is the SHA-256 of everything after the manifest (the file
//! count and entries), and `size` is the sum of the data lengths. A package is built
//! in a temporary directory and renamed into place only after every byte and the
//! manifest are durable, so an interrupted build leaves no package a target lists.
//! Paths are safe relatives; a package can never write outside the directory it is
//! unpacked into.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{BackupError, Result};
use crate::manifest::{BackupManifest, MANIFEST_FILE};

/// The package magic, exactly eight bytes.
pub const MAGIC: &[u8; 8] = b"SPOTBAK1";

/// The package file extension.
pub const PACKAGE_EXTENSION: &str = "spotbak";

/// A built package on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageFile {
    path: PathBuf,
    manifest: BackupManifest,
    package_bytes: u64,
}

impl PackageFile {
    /// The package's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The manifest the package header carries.
    pub fn manifest(&self) -> &BackupManifest {
        &self.manifest
    }

    /// The whole package's byte length, integer.
    pub fn package_bytes(&self) -> u64 {
        self.package_bytes
    }

    /// Opens a reader over the whole package (including the header), for a target's
    /// `put`.
    ///
    /// # Errors
    /// [`BackupError::Io`] when it cannot be opened.
    pub fn reader(&self) -> Result<Box<dyn Read + Send>> {
        let file = File::open(&self.path)?;
        Ok(Box::new(BufReader::new(file)))
    }

    /// The whole stream's byte length and SHA-256, which is what a target's `put`
    /// checks while it copies. (The manifest's own digest covers the payload section.)
    ///
    /// # Errors
    /// [`BackupError::Io`] when the package cannot be read.
    pub fn stream_facts(&self) -> Result<(u64, String)> {
        let mut reader = BufReader::new(File::open(&self.path)?);
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            bytes = bytes.saturating_add(read as u64);
        }
        Ok((bytes, hex(&hasher.finalize())))
    }
}

/// A manifest and the package it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackagedBackup {
    /// The completed manifest (digest, size and file count filled in).
    pub manifest: BackupManifest,
    /// The package file.
    pub package: PackageFile,
}

/// One file going into a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageEntry {
    /// Path relative to the package root.
    pub path: String,
    /// File contents.
    pub bytes: Vec<u8>,
}

/// Builds a package from in-memory files.
///
/// # Errors
/// [`BackupError::ManifestInvalid`] for an unsafe path, [`BackupError::Io`] for a
/// failed write.
pub fn build_from_files(
    manifest: BackupManifest,
    mut files: Vec<PackageEntry>,
    work_dir: &Path,
) -> Result<PackagedBackup> {
    files.sort_by(|left, right| left.path.cmp(&right.path));
    for file in &files {
        if !is_safe_relative(&file.path) {
            return Err(BackupError::ManifestInvalid {
                reason: format!("the package names an unsafe path: {}", file.path),
            });
        }
    }
    let sources = files
        .into_iter()
        .map(|file| PayloadSource {
            path: file.path,
            data: PayloadData::Memory(file.bytes),
        })
        .collect();
    build_sources(manifest, sources, work_dir)
}

/// Builds a package from a directory tree (the snapshot provider's output). Files are
/// streamed into the package, never read whole into memory, so a multi-gigabyte dump
/// packages in constant memory.
///
/// # Errors
/// [`BackupError::Io`] when the tree cannot be read or the package written,
/// [`BackupError::ManifestInvalid`] for a symlink or an unsafe file name.
pub fn build_from_dir(
    manifest: BackupManifest,
    source: &Path,
    work_dir: &Path,
) -> Result<PackagedBackup> {
    let mut files = Vec::new();
    collect(source, source, &mut files, 0)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    build_sources(manifest, files, work_dir)
}

enum PayloadData {
    Memory(Vec<u8>),
    File(PathBuf),
}

struct PayloadSource {
    path: String,
    data: PayloadData,
}

const MAX_DEPTH: usize = 64;
const MAX_FILES: usize = 1_000_000;

fn collect(root: &Path, dir: &Path, files: &mut Vec<PayloadSource>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(BackupError::io(format!(
            "the snapshot tree is deeper than {MAX_DEPTH} directories"
        )));
    }
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(BackupError::ManifestInvalid {
                reason: format!("the snapshot tree contains a symlink: {}", path.display()),
            });
        }
        if meta.is_dir() {
            collect(root, &path, files, depth + 1)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| BackupError::io("a snapshot path escaped its root"))?;
        let Some(relative) = relative.to_str() else {
            return Err(BackupError::ManifestInvalid {
                reason: format!("a snapshot path is not UTF-8: {}", path.display()),
            });
        };
        if !is_safe_relative(relative) {
            return Err(BackupError::ManifestInvalid {
                reason: format!("a snapshot names an unsafe path: {relative}"),
            });
        }
        if files.len() >= MAX_FILES {
            return Err(BackupError::io(format!(
                "the snapshot holds more than {MAX_FILES} files"
            )));
        }
        files.push(PayloadSource {
            path: relative.to_owned(),
            data: PayloadData::File(path),
        });
    }
    Ok(())
}

fn build_sources(
    mut manifest: BackupManifest,
    files: Vec<PayloadSource>,
    work_dir: &Path,
) -> Result<PackagedBackup> {
    std::fs::create_dir_all(work_dir)?;

    let sequence = next_temp_sequence();
    let payload_path = work_dir.join(format!(".payload-{}-{sequence}.tmp", std::process::id()));
    let package_tmp = work_dir.join(format!(".package-{}-{sequence}.tmp", std::process::id()));
    let final_path = work_dir.join(format!(
        "{}.{PACKAGE_EXTENSION}",
        sanitize_id(&manifest.id)?
    ));

    // 1. Write the payload section and hash it while streaming each file.
    let mut payload = BufWriter::new(
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&payload_path)?,
    );
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let count =
        u32::try_from(files.len()).map_err(|_| BackupError::io("too many package files"))?;
    write_counted(&mut payload, &mut hasher, &count.to_le_bytes())?;
    for file in &files {
        let path_len = u32::try_from(file.path.len())
            .map_err(|_| BackupError::io("a package path is longer than 4 GiB"))?;
        write_counted(&mut payload, &mut hasher, &path_len.to_le_bytes())?;
        write_counted(&mut payload, &mut hasher, file.path.as_bytes())?;
        let len = match &file.data {
            PayloadData::Memory(bytes) => bytes.len() as u64,
            PayloadData::File(path) => std::fs::metadata(path)?.len(),
        };
        write_counted(&mut payload, &mut hasher, &len.to_le_bytes())?;
        match &file.data {
            PayloadData::Memory(bytes) => write_counted(&mut payload, &mut hasher, bytes)?,
            PayloadData::File(path) => {
                let mut reader = BufReader::new(File::open(path)?);
                let mut copied = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let read = reader.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    payload.write_all(&buffer[..read])?;
                    hasher.update(&buffer[..read]);
                    copied = copied.saturating_add(read as u64);
                }
                if copied != len {
                    return Err(BackupError::VerifyFailed {
                        reason: format!(
                            "{} changed while it was packaged ({copied} bytes read, {len} expected)",
                            path.display()
                        ),
                    });
                }
            }
        }
        size = size.saturating_add(len);
    }
    payload.flush()?;
    payload.get_ref().sync_all()?;
    drop(payload);

    manifest.digest = hex(&hasher.finalize());
    manifest.size = size;
    manifest.file_count = files.len() as u64;
    manifest
        .canonical_checks()
        .map_err(|_| BackupError::ManifestInvalid {
            reason: "the completed manifest failed its own checks".to_owned(),
        })?;
    let manifest_text = manifest.to_toml()?;

    // 2. Compose the final package: header, then the payload bytes.
    let mut package = BufWriter::new(
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&package_tmp)?,
    );
    package.write_all(MAGIC)?;
    package.write_all(&(manifest_text.len() as u64).to_le_bytes())?;
    package.write_all(manifest_text.as_bytes())?;
    let mut payload_reader = BufReader::new(File::open(&payload_path)?);
    std::io::copy(&mut payload_reader, &mut package)?;
    package.flush()?;
    package.get_ref().sync_all()?;
    let package_bytes = package.get_ref().metadata()?.len();
    drop(package);

    // 3. Rename into place only now: an interrupted build leaves a dot-temp file that
    //    `list` ignores and `remove` can clean.
    let _ = std::fs::remove_file(&payload_path);
    std::fs::rename(&package_tmp, &final_path)?;
    sync_dir(work_dir);

    Ok(PackagedBackup {
        package: PackageFile {
            path: final_path,
            manifest: manifest.clone(),
            package_bytes,
        },
        manifest,
    })
}

fn write_counted(writer: &mut impl Write, hasher: &mut Sha256, bytes: &[u8]) -> Result<()> {
    writer.write_all(bytes)?;
    hasher.update(bytes);
    Ok(())
}

/// Opens a package and returns the manifest its header carries, without hashing the
/// payload. Targets use it to list cheaply.
///
/// # Errors
/// [`BackupError::Io`] or [`BackupError::ManifestInvalid`].
pub fn read_manifest(path: &Path) -> Result<BackupManifest> {
    let mut reader = BufReader::new(File::open(path)?);
    let manifest_text = read_header(&mut reader)?;
    BackupManifest::from_toml(&manifest_text)
}

/// Opens a package after checking its header.
///
/// # Errors
/// [`BackupError::VerifyFailed`] for a wrong magic or an unreadable header, and the
/// manifest errors from [`BackupManifest::from_toml`].
pub fn open(path: &Path) -> Result<PackageFile> {
    let mut reader = BufReader::new(File::open(path)?);
    let manifest_text = read_header(&mut reader)?;
    let manifest = BackupManifest::from_toml(&manifest_text)?;
    let package_bytes = reader.get_ref().metadata()?.len();
    Ok(PackageFile {
        path: path.to_path_buf(),
        manifest,
        package_bytes,
    })
}

fn read_header(reader: &mut impl Read) -> Result<String> {
    let mut magic = [0u8; 8];
    reader
        .read_exact(&mut magic)
        .map_err(|error| BackupError::VerifyFailed {
            reason: format!("the package header cannot be read: {error}"),
        })?;
    if &magic != MAGIC {
        return Err(BackupError::VerifyFailed {
            reason: "the package magic is not SPOTBAK1".to_owned(),
        });
    }
    let mut length = [0u8; 8];
    reader.read_exact(&mut length)?;
    let length = u64::from_le_bytes(length);
    if length == 0 || length > 16 * 1024 * 1024 {
        return Err(BackupError::VerifyFailed {
            reason: format!("the package manifest length {length} is not plausible"),
        });
    }
    let mut text = vec![
        0u8;
        usize::try_from(length).map_err(|_| BackupError::ManifestInvalid {
            reason: "the manifest length does not fit in memory".to_owned(),
        })?
    ];
    reader.read_exact(&mut text)?;
    String::from_utf8(text).map_err(|error| BackupError::ManifestInvalid {
        reason: format!("the package manifest is not UTF-8: {error}"),
    })
}

/// Checks the manifest digest, the byte size and the package header.
///
/// # Errors
/// [`BackupError::VerifyFailed`] naming the first broken check.
pub fn verify_quick(package: &PackageFile) -> Result<()> {
    let file = File::open(&package.path)?;
    let mut reader = BufReader::new(file);
    let manifest_text = read_header(&mut reader)?;
    let manifest = BackupManifest::from_toml(&manifest_text)?;
    if manifest != *package.manifest() {
        return Err(BackupError::VerifyFailed {
            reason: "the package header and the opened manifest differ".to_owned(),
        });
    }
    if let Some(key) = manifest.encryption.as_ref() {
        let total = reader.get_ref().metadata()?.len();
        let header_len = u64::try_from(manifest_text.len())
            .unwrap_or(u64::MAX)
            .saturating_add(16);
        return verify_quick_encrypted(
            &mut reader,
            &manifest,
            key,
            total.saturating_sub(header_len),
        );
    }
    let (size, count, digest) = hash_payload(&mut reader)?;
    if size != manifest.size {
        return Err(BackupError::VerifyFailed {
            reason: format!(
                "the package holds {size} payload bytes but the manifest says {}",
                manifest.size
            ),
        });
    }
    if count != manifest.file_count {
        return Err(BackupError::VerifyFailed {
            reason: format!(
                "the package holds {count} files but the manifest says {}",
                manifest.file_count
            ),
        });
    }
    if !digest.eq_ignore_ascii_case(&manifest.digest) {
        return Err(BackupError::VerifyFailed {
            reason: format!(
                "the package digest {digest} does not match the manifest's {}",
                manifest.digest
            ),
        });
    }
    Ok(())
}

/// Unpacks `package` into `dest`. `dest` is created when missing; files already there
/// with the same relative path are overwritten. Paths are validated, so the unpack
/// can never escape `dest`.
///
/// # Errors
/// [`BackupError::VerifyFailed`] when the digest or size does not match,
/// [`BackupError::ManifestInvalid`] for an unsafe path, [`BackupError::Io`] for a
/// failed write.
pub fn unpack(package: &PackageFile, dest: &Path) -> Result<()> {
    let file = File::open(&package.path)?;
    let mut reader = BufReader::new(file);
    let manifest_text = read_header(&mut reader)?;
    let manifest = BackupManifest::from_toml(&manifest_text)?;
    if manifest.encryption.is_some() {
        return Err(BackupError::EncryptionUnavailable {
            reason: "the package is encrypted; open it with decrypt_package before unpacking"
                .to_owned(),
        });
    }
    std::fs::create_dir_all(dest)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut count = 0u64;

    let mut count_buf = [0u8; 4];
    read_counted(&mut reader, &mut hasher, &mut count_buf, "the file count")?;
    let file_count = u32::from_le_bytes(count_buf);
    for _ in 0..file_count {
        let mut len_buf = [0u8; 4];
        read_counted(&mut reader, &mut hasher, &mut len_buf, "a path length")?;
        let path_len = u32::from_le_bytes(len_buf) as usize;
        if path_len > 4096 {
            return Err(BackupError::ManifestInvalid {
                reason: format!("a package path is {path_len} bytes long"),
            });
        }
        let mut path_bytes = vec![0u8; path_len];
        read_counted(&mut reader, &mut hasher, &mut path_bytes, "a path")?;
        let path = String::from_utf8(path_bytes).map_err(|error| BackupError::ManifestInvalid {
            reason: format!("a package path is not UTF-8: {error}"),
        })?;
        if !is_safe_relative(&path) {
            return Err(BackupError::ManifestInvalid {
                reason: format!("the package names an unsafe path: {path}"),
            });
        }
        let mut len_buf = [0u8; 8];
        read_counted(&mut reader, &mut hasher, &mut len_buf, "a data length")?;
        let data_len = u64::from_le_bytes(len_buf);
        let target = dest.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = BufWriter::new(File::create(&target)?);
        copy_counted(&mut reader, &mut hasher, &mut out, data_len, &path)?;
        out.flush()?;
        size = size.saturating_add(data_len);
        count += 1;
    }

    if size != manifest.size || count != manifest.file_count {
        return Err(BackupError::VerifyFailed {
            reason: format!(
                "the unpacked payload ({count} files, {size} bytes) does not match the manifest ({} files, {} bytes)",
                manifest.file_count, manifest.size
            ),
        });
    }
    let digest = hex(&hasher.finalize());
    if !digest.eq_ignore_ascii_case(&manifest.digest) {
        return Err(BackupError::VerifyFailed {
            reason: format!(
                "the unpacked digest {digest} does not match the manifest's {}",
                manifest.digest
            ),
        });
    }
    Ok(())
}

fn read_counted(
    reader: &mut impl Read,
    hasher: &mut Sha256,
    target: &mut [u8],
    what: &str,
) -> Result<()> {
    reader
        .read_exact(target)
        .map_err(|error| BackupError::VerifyFailed {
            reason: format!("the package ended before {what}: {error}"),
        })?;
    hasher.update(target);
    Ok(())
}

fn copy_counted(
    reader: &mut impl Read,
    hasher: &mut Sha256,
    writer: &mut impl Write,
    length: u64,
    path: &str,
) -> Result<()> {
    let mut remaining = length;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let take = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| BackupError::io("a package length is not addressable"))?;
        reader
            .read_exact(&mut buffer[..take])
            .map_err(|error| BackupError::VerifyFailed {
                reason: format!("the package ended inside {path}: {error}"),
            })?;
        hasher.update(&buffer[..take]);
        writer.write_all(&buffer[..take])?;
        remaining -= take as u64;
    }
    Ok(())
}

fn hash_payload(reader: &mut impl Read) -> Result<(u64, u64, String)> {
    let mut hasher = Sha256::new();
    let mut count_buf = [0u8; 4];
    read_counted(reader, &mut hasher, &mut count_buf, "the file count")?;
    let file_count = u32::from_le_bytes(count_buf);
    let mut size = 0u64;
    for _ in 0..file_count {
        let mut len_buf = [0u8; 4];
        read_counted(reader, &mut hasher, &mut len_buf, "a path length")?;
        let path_len = u32::from_le_bytes(len_buf) as usize;
        if path_len > 4096 {
            return Err(BackupError::VerifyFailed {
                reason: format!("a package path is {path_len} bytes long"),
            });
        }
        let mut path = vec![0u8; path_len];
        read_counted(reader, &mut hasher, &mut path, "a path")?;
        let mut len_buf = [0u8; 8];
        read_counted(reader, &mut hasher, &mut len_buf, "a data length")?;
        let data_len = u64::from_le_bytes(len_buf);
        let mut remaining = data_len;
        let mut buffer = [0u8; 64 * 1024];
        while remaining > 0 {
            let take = usize::try_from(remaining.min(buffer.len() as u64))
                .map_err(|_| BackupError::io("a package length is not addressable"))?;
            read_counted(reader, &mut hasher, &mut buffer[..take], "file data")?;
            remaining -= take as u64;
        }
        size = size.saturating_add(data_len);
    }
    // Refuse trailing bytes: a package is exactly one payload section.
    let mut trailing = [0u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(BackupError::VerifyFailed {
            reason: "the package has bytes after its last file".to_owned(),
        });
    }
    Ok((size, u64::from(file_count), hex(&hasher.finalize())))
}

/// Seals the payload of a plain package (WP-PRD-04.T6).
///
/// The payload section (everything after the manifest) is encrypted with
/// [`crate::encrypt::encrypt_stream`] under a fresh data key wrapped through `provider`
/// and `key`. The package is rebuilt at the same path with a manifest whose `digest`
/// is the SHA-256 of the stored (encrypted) payload section and whose `encryption` is
/// `key`; `size` and `file_count` stay plaintext facts. The new package is composed in a
/// temporary file and renamed over the plain one, so the plaintext package is replaced
/// atomically and never stays next to its encrypted twin.
///
/// # Errors
/// The errors of [`crate::encrypt::encrypt_stream`], [`BackupError::Io`] and
/// [`BackupError::ManifestInvalid`]. On error the plain package is left untouched and
/// every temporary file is removed.
pub async fn seal_package(
    plain: &PackagedBackup,
    provider: &dyn spot_secrets::KeyProvider,
    key: &crate::model::KeyRef,
) -> Result<PackagedBackup> {
    let work_dir = work_dir_of(&plain.package.path);
    let sequence = next_temp_sequence();
    let sealed_tmp = work_dir.join(format!(".sealed-{}-{sequence}.tmp", std::process::id()));
    let package_tmp = work_dir.join(format!(".package-{}-{sequence}.tmp", std::process::id()));
    let result = seal_into(plain, provider, key, &sealed_tmp, &package_tmp).await;
    let _ = std::fs::remove_file(&sealed_tmp);
    if result.is_err() {
        let _ = std::fs::remove_file(&package_tmp);
    }
    result
}

async fn seal_into(
    plain: &PackagedBackup,
    provider: &dyn spot_secrets::KeyProvider,
    key: &crate::model::KeyRef,
    sealed_tmp: &Path,
    package_tmp: &Path,
) -> Result<PackagedBackup> {
    let mut source = File::open(&plain.package.path)?;
    read_header(&mut source)?;
    let reader = tokio::fs::File::from_std(source);
    let mut writer = tokio::fs::File::from_std(private_new(sealed_tmp)?);
    crate::encrypt::encrypt_stream(provider, key, &plain.manifest.id, reader, &mut writer).await?;
    writer.sync_all().await?;
    drop(writer);

    let (_, digest) = hash_file(sealed_tmp)?;
    let mut manifest = plain.manifest.clone();
    manifest.digest = digest;
    manifest.encryption = Some(key.clone());
    manifest
        .canonical_checks()
        .map_err(|_| BackupError::ManifestInvalid {
            reason: "the sealed manifest failed its own checks".to_owned(),
        })?;
    let package_bytes = compose_package(package_tmp, &manifest, sealed_tmp)?;
    std::fs::rename(package_tmp, &plain.package.path)?;
    sync_dir(&work_dir_of(&plain.package.path));
    Ok(PackagedBackup {
        package: PackageFile {
            path: plain.package.path.clone(),
            manifest: manifest.clone(),
            package_bytes,
        },
        manifest,
    })
}

/// Decrypts an encrypted package into a new **plain** package next to it and returns
/// the plain package; the caller removes it (it holds plaintext) when done.
///
/// The decrypted payload is checked against the manifest's `size` and `file_count`
/// (the same structural checks a plain package gets from [`verify_quick`]); the plain
/// package's digest is the SHA-256 of the decrypted payload section. Wrong key, a
/// tampered, truncated, reordered or extended payload and a retired key all fail
/// closed. Temporary files are mode 0600 on Unix and removed on error.
///
/// # Errors
/// [`BackupError::ManifestInvalid`] when the package is not encrypted, the errors of
/// [`crate::encrypt::decrypt_stream`], and [`BackupError::VerifyFailed`] when the
/// decrypted payload does not match the manifest.
pub async fn decrypt_package(
    package: &PackageFile,
    provider: &dyn spot_secrets::KeyProvider,
) -> Result<PackageFile> {
    if package.manifest.encryption.is_none() {
        return Err(BackupError::ManifestInvalid {
            reason: "the package is not encrypted".to_owned(),
        });
    }
    let work_dir = work_dir_of(&package.path);
    let sequence = next_temp_sequence();
    let payload_tmp = work_dir.join(format!(".opened-{}-{sequence}.payload", std::process::id()));
    let package_tmp = work_dir.join(format!(".opened-{}-{sequence}.tmp", std::process::id()));
    let final_path = work_dir.join(format!(
        ".opened-{}-{sequence}.{PACKAGE_EXTENSION}",
        std::process::id()
    ));
    let result = decrypt_into(package, provider, &payload_tmp, &package_tmp, &final_path).await;
    let _ = std::fs::remove_file(&payload_tmp);
    let _ = std::fs::remove_file(&package_tmp);
    if result.is_err() {
        let _ = std::fs::remove_file(&final_path);
    }
    result
}

async fn decrypt_into(
    package: &PackageFile,
    provider: &dyn spot_secrets::KeyProvider,
    payload_tmp: &Path,
    package_tmp: &Path,
    final_path: &Path,
) -> Result<PackageFile> {
    let mut source = File::open(&package.path)?;
    read_header(&mut source)?;
    let reader = tokio::fs::File::from_std(source);
    let mut writer = tokio::fs::File::from_std(private_new(payload_tmp)?);
    crate::encrypt::decrypt_stream(provider, &package.manifest.id, reader, &mut writer).await?;
    writer.sync_all().await?;
    drop(writer);

    let (_, digest) = hash_file(payload_tmp)?;
    let mut manifest = package.manifest.clone();
    manifest.digest = digest;
    manifest.encryption = None;
    let package_bytes = compose_package(package_tmp, &manifest, payload_tmp)?;
    std::fs::rename(package_tmp, final_path)?;
    let plain = PackageFile {
        path: final_path.to_path_buf(),
        manifest,
        package_bytes,
    };
    verify_quick(&plain)?;
    Ok(plain)
}

/// Quick verification of an encrypted package: the header parses within its caps and
/// names the manifest's key, the chunk framing is plausible and the SHA-256 of the
/// stored payload section matches the manifest's digest. It needs no key.
fn verify_quick_encrypted(
    reader: &mut impl Read,
    manifest: &BackupManifest,
    key: &crate::model::KeyRef,
    payload_len: u64,
) -> Result<()> {
    let mut head = Vec::new();
    (&mut *reader)
        .take(crate::encrypt::MAX_HEADER_LEN as u64)
        .read_to_end(&mut head)?;
    crate::encrypt::check_framing(&head, payload_len, key).map_err(|error| {
        BackupError::VerifyFailed {
            reason: format!(
                "the encrypted payload header is not valid: {}",
                error.reason()
            ),
        }
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&head);
    let mut seen = head.len() as u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        seen = seen.saturating_add(read as u64);
    }
    if seen != payload_len {
        return Err(BackupError::VerifyFailed {
            reason: format!(
                "the encrypted payload holds {seen} bytes but the package frames {payload_len}"
            ),
        });
    }
    let digest = hex(&hasher.finalize());
    if !digest.eq_ignore_ascii_case(&manifest.digest) {
        return Err(BackupError::VerifyFailed {
            reason: format!(
                "the encrypted payload digest {digest} does not match the manifest's {}",
                manifest.digest
            ),
        });
    }
    Ok(())
}

/// The directory a package lives in (its temporary siblings go there too).
fn work_dir_of(path: &Path) -> PathBuf {
    path.parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// Creates a new file for writing, failing if it exists; mode 0600 on Unix.
fn private_new(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

/// The byte length and SHA-256 of a whole file.
fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
    }
    Ok((bytes, hex(&hasher.finalize())))
}

/// Writes `package_tmp` as magic, manifest, then the bytes of `body`; syncs it and
/// returns its length. The caller renames it into place.
fn compose_package(package_tmp: &Path, manifest: &BackupManifest, body: &Path) -> Result<u64> {
    let manifest_text = manifest.to_toml()?;
    let mut package = BufWriter::new(private_new(package_tmp)?);
    package.write_all(MAGIC)?;
    package.write_all(&(manifest_text.len() as u64).to_le_bytes())?;
    package.write_all(manifest_text.as_bytes())?;
    let mut body_reader = BufReader::new(File::open(body)?);
    std::io::copy(&mut body_reader, &mut package)?;
    package.flush()?;
    package.get_ref().sync_all()?;
    Ok(package.get_ref().metadata()?.len())
}

/// Whether `path` is a safe relative package path (no root, no `..`, no empty part).
pub fn is_safe_relative(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return false;
    }
    let candidate = Path::new(path);
    candidate
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
}

/// Whether `text` is a lower- or upper-case SHA-256 hex string.
pub fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sanitize_id(id: &str) -> Result<String> {
    if id.is_empty()
        || id.len() > 200
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(BackupError::ManifestInvalid {
            reason: format!("the backup id {id:?} is not usable as a file name"),
        });
    }
    Ok(id.to_owned())
}

/// A process-local counter naming temporary files without randomness or a clock.
fn next_temp_sequence() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(unix)]
fn sync_dir(dir: &Path) {
    // Directory fsync makes the rename durable. It is a best effort here because some
    // filesystems (and Windows) do not allow opening a directory; the file itself was
    // already synced and the rename is atomic.
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {
    // `std` has no portable directory handle; the file is synced before the rename.
}

/// The sibling `backup.toml` a host may write next to an exported package.
pub fn manifest_path(package: &PackageFile) -> PathBuf {
    package
        .path
        .with_file_name(format!("{MANIFEST_FILE}.{}.toml", package.manifest.id))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::manifest::MANIFEST_FORMAT;
    use crate::model::{TargetRef, VerifyMode};

    fn manifest() -> BackupManifest {
        BackupManifest {
            format: MANIFEST_FORMAT.to_owned(),
            id: "acme-chain-1".to_owned(),
            tenant: "acme".to_owned(),
            created_at: "2026-10-19T12:00:00Z".to_owned(),
            platform_version: "0.1.0".to_owned(),
            snapshot_provider_version: "0.1.0".to_owned(),
            pg_major: 18,
            schema_version: 3,
            app_set: vec!["crm".to_owned()],
            digest: "0".repeat(64),
            size: 0,
            file_count: 0,
            chain_id: "chain".to_owned(),
            chain_sequence: 1,
            parent_digest: None,
            verify: VerifyMode::Quick,
            target: TargetRef::Folder {
                path: "/backups".into(),
            }
            .to_string(),
            encryption: None,
        }
    }

    #[test]
    fn builds_opens_verifies_and_unpacks() {
        let work = tempfile::tempdir().expect("work");
        let out = tempfile::tempdir().expect("out");
        let files = vec![
            PackageEntry {
                path: "data/dump.bin".to_owned(),
                bytes: b"hello".to_vec(),
            },
            PackageEntry {
                path: "snapshot.toml".to_owned(),
                bytes: b"x=1\n".to_vec(),
            },
        ];
        let built = build_from_files(manifest(), files, work.path()).expect("build");
        assert_eq!(built.manifest.size, 9);
        assert_eq!(built.manifest.file_count, 2);
        assert!(is_sha256_hex(&built.manifest.digest));
        let opened = open(&built.package.path).expect("open");
        assert_eq!(opened.manifest(), &built.manifest);
        verify_quick(&opened).expect("verify");
        unpack(&opened, out.path()).expect("unpack");
        assert_eq!(
            std::fs::read(out.path().join("data/dump.bin")).expect("read"),
            b"hello"
        );
    }

    #[test]
    fn a_changed_byte_fails_quick_verification() {
        let work = tempfile::tempdir().expect("work");
        let built = build_from_files(
            manifest(),
            vec![PackageEntry {
                path: "a".to_owned(),
                bytes: b"abc".to_vec(),
            }],
            work.path(),
        )
        .expect("build");
        let mut bytes = std::fs::read(&built.package.path).expect("read");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&built.package.path, bytes).expect("write");
        let opened = open(&built.package.path).expect("open");
        assert_eq!(
            verify_quick(&opened).expect_err("changed byte").code(),
            "backup.verify_failed"
        );
    }

    #[test]
    fn builds_from_a_directory_without_reading_it_into_memory() {
        let work = tempfile::tempdir().expect("work");
        let source = work.path().join("snapshot");
        let out = work.path().join("out");
        std::fs::create_dir_all(source.join("data")).expect("dirs");
        std::fs::write(source.join("snapshot.toml"), b"provider = \"fake\"\n").expect("write");
        std::fs::write(source.join("data/dump"), b"streamed").expect("write");
        let built = build_from_dir(manifest(), &source, work.path()).expect("build");
        assert_eq!(built.manifest.file_count, 2);
        assert_eq!(built.manifest.size, 26);
        verify_quick(&built.package).expect("verify");
        unpack(&built.package, &out).expect("unpack");
        assert_eq!(
            std::fs::read(out.join("data/dump")).expect("read"),
            b"streamed"
        );
    }

    #[test]
    fn unsafe_paths_are_refused() {
        let work = tempfile::tempdir().expect("work");
        let error = build_from_files(
            manifest(),
            vec![PackageEntry {
                path: "../escape".to_owned(),
                bytes: b"x".to_vec(),
            }],
            work.path(),
        )
        .expect_err("unsafe");
        assert_eq!(error.code(), "backup.manifest_invalid");
        assert!(!is_safe_relative("/abs"));
        assert!(!is_safe_relative("a/../../b"));
        assert!(is_safe_relative("a/b.txt"));
    }
}
