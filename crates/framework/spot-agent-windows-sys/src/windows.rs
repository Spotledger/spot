//! The thin Windows implementations of the two traits, gated on `#[cfg(windows)]` so the
//! crate builds and unit-tests on Linux. Nothing here uses `unsafe`: the Volume Shadow
//! Copy service is driven through `vssadmin` and the certificate stores through `certutil`,
//! both as child processes with argument vectors (never a shell string).
//!
//! These implementations are deliberately small. They are compiled only on Windows; the
//! Linux CI host neither compiles nor runs them, and the PR lists that they were not
//! compile-checked on the author's host (the same statement WP-ADM-05 part 1 makes).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::cert_dates::parse_certutil_dump_validity;
use crate::certificates::{
    BindOutcome, CertStore, CertificateInfo, CertificateStore, ChainStatus, Expiry, HttpsBinding,
    Thumbprint,
};
use crate::error::{WindowsSysError, WindowsSysResult};
use crate::vss::{ShadowCopy, ShadowCopyId, VolumeId, VolumeSet, VssApi, WriterSet};

/// Runs a tool with an argument vector and returns its standard output on success.
fn run_tool(tool: &str, args: &[&str]) -> WindowsSysResult<String> {
    let output = Command::new(tool)
        .args(args)
        .output()
        .map_err(|error| WindowsSysError::io(format!("cannot start {tool}: {error}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(WindowsSysError::io(format!("{tool} failed: {stderr}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A line's value after `key`, trimmed; `None` when the key is absent.
fn value_after(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(key))
        .map(|rest| rest.trim().to_owned())
}

/// The real Volume Shadow Copy service, driven through `vssadmin`. It tracks the shadows
/// it created in memory (this agent's own view) and asks `vssadmin` to create and delete
/// them; the read-only device path of a shadow is returned directly, without a symlink.
#[derive(Debug, Default)]
pub struct WindowsVss {
    created: Mutex<BTreeMap<String, ShadowCopy>>,
    devices: Mutex<BTreeMap<String, String>>,
}

impl WindowsVss {
    /// A real VSS client.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_created(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ShadowCopy>> {
        self.created
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_devices(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        self.devices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl VssApi for WindowsVss {
    fn create(
        &self,
        volumes: &VolumeSet,
        _writers: &WriterSet,
        at_unix_ms: u64,
    ) -> WindowsSysResult<ShadowCopy> {
        let volume = &volumes.as_slice()[0];
        let argument = format!("/for={}", volume.as_str());
        let output = run_tool("vssadmin", &["create", "shadow", &argument])?;
        let id =
            value_after(&output, "Shadow Copy ID:").ok_or_else(|| WindowsSysError::VssFailed {
                reason: "vssadmin reported no shadow copy id".to_owned(),
            })?;
        if let Some(device) = value_after(&output, "Shadow Copy Volume Name:") {
            self.lock_devices().insert(id.clone(), device);
        }
        let shadow = ShadowCopy {
            id: ShadowCopyId::new(id.clone()),
            volumes: volumes.as_slice().to_vec(),
            created_at_unix_ms: at_unix_ms,
        };
        self.lock_created().insert(id, shadow.clone());
        Ok(shadow)
    }

    fn list(&self) -> WindowsSysResult<Vec<ShadowCopy>> {
        Ok(self.lock_created().values().cloned().collect())
    }

    fn delete(&self, id: &ShadowCopyId) -> WindowsSysResult<()> {
        let argument = format!("/shadow={}", id.as_str());
        run_tool("vssadmin", &["delete", "shadows", &argument, "/quiet"])?;
        self.lock_created().remove(id.as_str());
        self.lock_devices().remove(id.as_str());
        Ok(())
    }

    fn expose_read_only(&self, id: &ShadowCopyId, original: &Path) -> WindowsSysResult<PathBuf> {
        if !self.lock_created().contains_key(id.as_str()) {
            return Err(WindowsSysError::ShadowNotFound {
                id: id.as_str().to_owned(),
            });
        }
        let device = self
            .lock_devices()
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| WindowsSysError::VssFailed {
                reason: "the shadow copy has no device name".to_owned(),
            })?;
        let relative = relative_within_volume(original);
        Ok(PathBuf::from(format!("{device}\\{relative}")))
    }

    fn unexpose(&self, _id: &ShadowCopyId) -> WindowsSysResult<()> {
        // The device path is inherently read-only; nothing was exposed to undo.
        Ok(())
    }

    fn volume_of(&self, path: &Path) -> WindowsSysResult<VolumeId> {
        let text = path.to_string_lossy();
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (Some(letter), Some(':')) if letter.is_ascii_alphabetic() => {
                VolumeId::new(&format!("{letter}:\\"))
            }
            _ => Err(WindowsSysError::VssFailed {
                reason: "the path names no drive-letter volume".to_owned(),
            }),
        }
    }
}

/// `path` relative to its volume root, with forward slashes normalised to backslashes.
fn relative_within_volume(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    match text.find('\\') {
        Some(index) => text[index + 1..].to_owned(),
        None => String::new(),
    }
}

/// The real LocalMachine certificate store, driven through `certutil`. Output parsing is
/// best-effort: the fields it cannot read are reported honestly (an empty subject, an
/// open validity window) rather than guessed.
#[derive(Debug, Default)]
pub struct WindowsCertStore {
    _private: (),
}

impl WindowsCertStore {
    /// A real certificate store client.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl CertificateStore for WindowsCertStore {
    fn list(&self, store: CertStore) -> WindowsSysResult<Vec<CertificateInfo>> {
        let output = run_tool("certutil", &["-store", store.as_str()])?;
        Ok(parse_store_listing(store, &output))
    }

    fn import(
        &self,
        store: CertStore,
        certificate: &[u8],
        private_key: Option<&[u8]>,
    ) -> WindowsSysResult<Thumbprint> {
        let file = std::env::temp_dir().join(format!(
            "spot-agent-windows-sys-cert-{}.bin",
            std::process::id()
        ));
        std::fs::write(&file, certificate)?;
        let path = file.to_string_lossy().into_owned();
        // The thumbprint is read from the file itself, never guessed from the store listing.
        let outcome: WindowsSysResult<Thumbprint> = (|| {
            let dump = run_tool("certutil", &["-dump", &path])?;
            let hash = value_after(&dump, "Cert Hash(sha1):").ok_or_else(|| {
                WindowsSysError::io("certutil reported no thumbprint for the certificate")
            })?;
            let thumbprint = Thumbprint::parse(&hash)?;
            if private_key.is_some() {
                run_tool("certutil", &["-importPFX", "-f", store.as_str(), &path])?;
            } else {
                run_tool("certutil", &["-addstore", "-f", store.as_str(), &path])?;
            }
            Ok(thumbprint)
        })();
        let _ = std::fs::remove_file(&file);
        outcome
    }

    fn remove(&self, store: CertStore, thumbprint: &Thumbprint) -> WindowsSysResult<()> {
        // Removing an absent certificate is a no-op; any other failure is reported.
        let present = self
            .list(store)?
            .iter()
            .any(|info| info.thumbprint == *thumbprint);
        if !present {
            return Ok(());
        }
        run_tool(
            "certutil",
            &["-delstore", store.as_str(), thumbprint.as_str()],
        )
        .map(|_| ())
    }

    fn export_public(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
    ) -> WindowsSysResult<Vec<u8>> {
        let output = run_tool("certutil", &["-store", store.as_str(), thumbprint.as_str()])?;
        match pem_block(&output) {
            Some(pem) => Ok(pem.into_bytes()),
            None => Err(WindowsSysError::CertificateNotFound {
                store: store.as_str().to_owned(),
                thumbprint: thumbprint.as_str().to_owned(),
            }),
        }
    }

    fn check_expiry(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
        now_unix: i64,
    ) -> WindowsSysResult<Expiry> {
        let (not_before, not_after) = self.validity_window(store, thumbprint)?;
        if now_unix < not_before {
            Ok(Expiry::NotYetValid)
        } else if now_unix > not_after {
            Ok(Expiry::Expired)
        } else {
            Ok(Expiry::Valid)
        }
    }

    fn check_chain(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
    ) -> WindowsSysResult<ChainStatus> {
        let output = run_tool(
            "certutil",
            &["-verify", "-store", store.as_str(), thumbprint.as_str()],
        )?;
        if output.to_ascii_lowercase().contains("certificate is valid") {
            Ok(ChainStatus::Valid)
        } else {
            Ok(ChainStatus::Untrusted {
                reason: "certutil did not report the chain as valid".to_owned(),
            })
        }
    }

    fn bind_https(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
        binding: &HttpsBinding,
        now_unix: i64,
    ) -> WindowsSysResult<BindOutcome> {
        // The store is the source of truth for validity; the binding itself is recorded by
        // the host (the HTTP listener reads it). Validate before accepting.
        let (not_before, not_after) = self.validity_window(store, thumbprint)?;
        if now_unix < not_before {
            return Err(WindowsSysError::CertificateNotValid {
                thumbprint: thumbprint.as_str().to_owned(),
                reason: "not yet valid".to_owned(),
            });
        }
        if now_unix > not_after {
            return Err(WindowsSysError::CertificateNotValid {
                thumbprint: thumbprint.as_str().to_owned(),
                reason: "expired".to_owned(),
            });
        }
        if let ChainStatus::Untrusted { reason } = self.check_chain(store, thumbprint)? {
            return Err(WindowsSysError::ChainInvalid {
                thumbprint: thumbprint.as_str().to_owned(),
                reason,
            });
        }
        let _ = binding;
        let expires_within_days = days_until_expiry_if_near(not_after, now_unix);
        if let Some(days_remaining) = expires_within_days {
            tracing::warn!(
                thumbprint = thumbprint.as_str(),
                days_remaining,
                "certificate bound to an HTTPS endpoint is within 30 days of expiry"
            );
        }
        Ok(BindOutcome {
            expires_within_days,
        })
    }

    fn bound_https(&self, _binding: &HttpsBinding) -> WindowsSysResult<Option<Thumbprint>> {
        // The host owns the HTTP bindings; this store only validates certificates.
        Ok(None)
    }
}

impl WindowsCertStore {
    /// The certificate's validity window (`NotBefore`, `NotAfter`) in whole seconds since
    /// the Unix epoch (UTC). Tries the locale-independent PowerShell source first
    /// (`Get-ChildItem Cert:\...` formatted with `.ToString("o")`, immune to the machine's
    /// locale); falls back to parsing `certutil -dump`'s locale-dependent text with
    /// [`parse_certutil_dump_validity`] when PowerShell is unavailable or fails to run.
    ///
    /// # Errors
    /// [`WindowsSysError::CertificateNotFound`] when the certificate is absent from
    /// `store`; [`WindowsSysError::io`] when neither source yields a parseable window.
    fn validity_window(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
    ) -> WindowsSysResult<(i64, i64)> {
        if let Some(result) = powershell_validity_window(store, thumbprint) {
            return result;
        }
        let dump = self.certutil_dump(store, thumbprint)?;
        parse_certutil_dump_validity(&dump)
    }

    /// `certutil -dump` of the certificate's exported PEM, through a short-lived temp file.
    fn certutil_dump(&self, store: CertStore, thumbprint: &Thumbprint) -> WindowsSysResult<String> {
        let pem = self.export_public(store, thumbprint)?;
        let file = std::env::temp_dir().join(format!(
            "spot-agent-windows-sys-cert-dump-{}.bin",
            std::process::id()
        ));
        std::fs::write(&file, &pem)?;
        let path = file.to_string_lossy().into_owned();
        let outcome = run_tool("certutil", &["-dump", &path]);
        let _ = std::fs::remove_file(&file);
        outcome
    }
}

/// The certificate's validity window read from PowerShell's `Get-ChildItem Cert:\...`,
/// formatted with `.ToString("o")` (ISO 8601, locale-independent). `None` when PowerShell
/// cannot be run at all (not installed, or the shell itself failed to start) — the caller
/// then falls back to `certutil`. `Some(Err(..))` when PowerShell ran but the certificate
/// could not be read (e.g. it is absent), which the caller should NOT mask with a fallback.
fn powershell_validity_window(
    store: CertStore,
    thumbprint: &Thumbprint,
) -> Option<WindowsSysResult<(i64, i64)>> {
    let store_name = match store {
        CertStore::Root => "Root",
        CertStore::Ca => "CA",
        CertStore::My => "My",
    };
    let path = format!("Cert:\\LocalMachine\\{store_name}\\{}", thumbprint.as_str());
    let script = format!(
        "$c = Get-Item -Path '{path}' -ErrorAction SilentlyContinue; \
         if ($null -eq $c) {{ exit 1 }}; \
         Write-Output ($c.NotBefore.ToUniversalTime().ToString('o') + '|' + \
         $c.NotAfter.ToUniversalTime().ToString('o'))"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    if !output.status.success() {
        return Some(Err(WindowsSysError::CertificateNotFound {
            store: store.as_str().to_owned(),
            thumbprint: thumbprint.as_str().to_owned(),
        }));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let Some((not_before, not_after)) = text.split_once('|') else {
        return Some(Err(WindowsSysError::io(
            "powershell did not report NotBefore|NotAfter for the certificate",
        )));
    };
    Some((|| {
        Ok((
            crate::cert_dates::parse_certificate_datetime(not_before)?,
            crate::cert_dates::parse_certificate_datetime(not_after)?,
        ))
    })())
}

/// Whole days from `now_unix` to `not_after_unix` when that is 30 days or fewer; mirrors
/// [`crate::certificates`]'s own helper so both the fake and the real store warn the same
/// way. Integer arithmetic only.
fn days_until_expiry_if_near(not_after_unix: i64, now_unix: i64) -> Option<i64> {
    let remaining_seconds = not_after_unix.saturating_sub(now_unix);
    if remaining_seconds < 0 {
        return None;
    }
    let remaining_days = remaining_seconds / 86_400;
    if remaining_days <= 30 {
        Some(remaining_days)
    } else {
        None
    }
}

/// Parses the `certutil -store` listing into certificate records. Best-effort: unknown
/// fields are left at their honest defaults.
fn parse_store_listing(store: CertStore, text: &str) -> Vec<CertificateInfo> {
    let mut infos = Vec::new();
    for block in text.split("================") {
        let Some(hash) = value_after(block, "Cert Hash(sha1):") else {
            continue;
        };
        let Ok(thumbprint) = Thumbprint::parse(&hash) else {
            continue;
        };
        let subject = value_after(block, "Subject:").unwrap_or_default();
        let issuer = value_after(block, "Issuer:").unwrap_or_default();
        let has_private_key = block.contains("Private Key");
        infos.push(CertificateInfo {
            thumbprint,
            store,
            subject,
            issuer,
            not_before_unix: 0,
            not_after_unix: i64::MAX,
            has_private_key,
        });
    }
    infos
}

/// The first PEM certificate block in `text`.
fn pem_block(text: &str) -> Option<String> {
    let begin = text.find("-----BEGIN CERTIFICATE-----")?;
    let end = text[begin..].find("-----END CERTIFICATE-----")? + begin;
    Some(text[begin..end + "-----END CERTIFICATE-----".len()].to_owned())
}
