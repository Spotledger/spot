//! Certificates: install a CA or leaf into the system trust store, list and remove it,
//! generate a certificate-signing request, and check expiry and chain.
//!
//! The trust-store paths and the refresh command differ by distribution family: the
//! RHEL-compatible family uses `/etc/pki/ca-trust/source/anchors` with `update-ca-trust`,
//! and the Debian family (Ubuntu) uses `/usr/local/share/ca-certificates` with
//! `update-ca-certificates`. Expiry is checked with `openssl x509 -checkend`, which never
//! makes this module parse a date itself, and the chain with `openssl verify`.

use std::path::PathBuf;

use crate::catalogue::op_id;
use crate::error::{CERT_CHAIN, CERT_INVALID, CERT_MISSING, SysError, SysResult};
use crate::preview::{Change, Check, Outcome, Preview};
use crate::sys::{CommandRunner, CommandSpec, FileSystem};
use spot_mgmt_ops::Risk;

/// The distribution family that decides the trust-store layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistroFamily {
    /// Fedora, RHEL and RHEL-compatible distributions.
    Rhel,
    /// Debian and Ubuntu.
    Debian,
}

impl DistroFamily {
    /// The family's lower-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Rhel => "rhel",
            Self::Debian => "debian",
        }
    }

    /// The directory a CA is installed into.
    #[must_use]
    pub fn anchor_dir(self) -> &'static str {
        match self {
            Self::Rhel => "/etc/pki/ca-trust/source/anchors",
            Self::Debian => "/usr/local/share/ca-certificates",
        }
    }

    /// The combined CA bundle the chain check verifies against.
    #[must_use]
    pub fn ca_bundle(self) -> &'static str {
        match self {
            Self::Rhel => "/etc/pki/tls/certs/ca-bundle.crt",
            Self::Debian => "/etc/ssl/certs/ca-certificates.crt",
        }
    }
}

/// One certificate present in the trust store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateInfo {
    /// The file's base name without extension.
    pub name: String,
    /// The full path.
    pub path: String,
}

/// A certificate-signing request to generate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsrRequest {
    /// The distinguished name (`/CN=example.com`).
    pub subject: String,
    /// Where the private key is written (a secret; never read back).
    pub key_path: String,
    /// Where the request is written.
    pub csr_path: String,
    /// The RSA key size in bits.
    pub key_bits: u32,
}

impl CsrRequest {
    /// Validates the subject, the paths and the key size.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] with the reason.
    pub fn validate(&self) -> SysResult<()> {
        if self.subject.is_empty() || self.subject.starts_with('-') {
            return Err(SysError::invalid(
                "the subject must be a non-empty distinguished name",
            ));
        }
        if !self.subject.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '=' | '.' | '_' | '-' | '*' | ' ' | '@')
        }) {
            return Err(SysError::invalid(
                "the subject contains an unexpected character",
            ));
        }
        validate_absolute(&self.key_path, "the key path")?;
        validate_absolute(&self.csr_path, "the request path")?;
        if !(2048..=8192).contains(&self.key_bits) {
            return Err(SysError::invalid("the key size must be 2048 to 8192 bits"));
        }
        Ok(())
    }
}

/// The certificates module.
pub struct CertificatesModule<'a> {
    runner: &'a dyn CommandRunner,
    fs: &'a dyn FileSystem,
}

impl<'a> CertificatesModule<'a> {
    /// Wraps a command runner and a filesystem.
    #[must_use]
    pub fn new(runner: &'a dyn CommandRunner, fs: &'a dyn FileSystem) -> Self {
        Self { runner, fs }
    }

    /// The distribution family, read from `/etc/os-release`.
    ///
    /// # Errors
    /// [`crate::error::IO`] when the file is unreadable and [`crate::error::UNSUPPORTED`] for
    /// an unknown family.
    pub fn family(&self) -> SysResult<DistroFamily> {
        let text = self.fs.read_to_string(&PathBuf::from("/etc/os-release"))?;
        parse_family(&text)
    }

    /// The path a certificate named `name` is installed at.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] for a bad name, and the family's failures.
    pub fn anchor_path(&self, name: &str) -> SysResult<PathBuf> {
        validate_cert_name(name)?;
        let family = self.family()?;
        Ok(PathBuf::from(format!("{}/{name}.crt", family.anchor_dir())))
    }

    /// Lists the certificates in the trust store.
    ///
    /// # Errors
    /// The family's failures and [`crate::error::IO`] when the directory cannot be listed.
    pub fn list(&self) -> SysResult<Vec<CertificateInfo>> {
        let family = self.family()?;
        let dir = PathBuf::from(family.anchor_dir());
        if !self.fs.exists(&dir)? {
            return Ok(Vec::new());
        }
        let mut certificates = Vec::new();
        for entry in self.fs.list_dir(&dir)? {
            if let Some(name) = entry.strip_suffix(".crt") {
                certificates.push(CertificateInfo {
                    name: name.to_owned(),
                    path: format!("{}/{entry}", family.anchor_dir()),
                });
            }
        }
        Ok(certificates)
    }

    /// Whether a certificate is present.
    ///
    /// # Errors
    /// The family's failures.
    pub fn exists(&self, name: &str) -> SysResult<bool> {
        self.fs.exists(&self.anchor_path(name)?)
    }

    /// Plans installing a CA or leaf certificate.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] for a bad name or empty PEM, and the family's failures.
    pub fn plan_install(&self, name: &str, pem: &str) -> SysResult<Preview> {
        validate_cert_name(name)?;
        if !pem.contains("-----BEGIN CERTIFICATE-----") {
            return Err(SysError::new(
                CERT_INVALID,
                "the certificate is not a PEM certificate",
            ));
        }
        let exists = self.exists(name)?;
        Ok(
            Preview::new(op_id(&["os", "cert", "install"]), Risk::Medium)
                .change(Change::new(
                    name.to_owned(),
                    Some(if exists { "present" } else { "absent" }.to_owned()),
                    Some("present".to_owned()),
                ))
                .note(format!("installed into {}", self.family()?.anchor_dir())),
        )
    }

    /// Installs the certificate and refreshes the trust store; a no-op when it is present.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] for a bad name, and the write and refresh failures.
    pub fn apply_install(&self, name: &str, pem: &str) -> SysResult<Outcome> {
        validate_cert_name(name)?;
        if !pem.contains("-----BEGIN CERTIFICATE-----") {
            return Err(SysError::new(
                CERT_INVALID,
                "the certificate is not a PEM certificate",
            ));
        }
        let path = self.anchor_path(name)?;
        let already = self.fs.exists(&path)?;
        if already {
            let current = self.fs.read_to_string(&path)?;
            if current == pem {
                return Ok(Outcome::new(
                    op_id(&["os", "cert", "install"]),
                    false,
                    format!("{name} is already installed"),
                ));
            }
        }
        self.fs.write_atomic(&path, pem)?;
        self.refresh()?;
        Ok(Outcome::new(
            op_id(&["os", "cert", "install"]),
            true,
            format!("installed {name} into {}", path.display()),
        ))
    }

    /// Verifies a certificate is installed.
    ///
    /// # Errors
    /// The family's failures.
    pub fn verify_install(&self, name: &str) -> SysResult<Check> {
        if self.exists(name)? {
            Ok(Check::ok(
                op_id(&["os", "cert", "install"]),
                format!("{name} is in the trust store"),
            ))
        } else {
            Ok(Check::failed(
                op_id(&["os", "cert", "install"]),
                format!("{name} is missing from the trust store"),
            ))
        }
    }

    /// Plans removing a certificate.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] for a bad name, and the family's failures.
    pub fn plan_remove(&self, name: &str) -> SysResult<Preview> {
        let exists = self.exists(name)?;
        Ok(Preview::new(op_id(&["os", "cert", "remove"]), Risk::Medium)
            .change(Change::new(
                name.to_owned(),
                Some(if exists { "present" } else { "absent" }.to_owned()),
                Some("absent".to_owned()),
            ))
            .destructive(true))
    }

    /// Removes the certificate and refreshes the trust store; a no-op when it is gone.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] for a bad name, and the remove and refresh failures.
    pub fn apply_remove(&self, name: &str) -> SysResult<Outcome> {
        let path = self.anchor_path(name)?;
        if !self.fs.exists(&path)? {
            return Ok(Outcome::new(
                op_id(&["os", "cert", "remove"]),
                false,
                format!("{name} is already absent"),
            ));
        }
        self.fs.remove_file(&path)?;
        self.refresh()?;
        Ok(Outcome::new(
            op_id(&["os", "cert", "remove"]),
            true,
            format!("removed {name} from the trust store"),
        ))
    }

    /// Verifies a certificate is gone.
    ///
    /// # Errors
    /// The family's failures.
    pub fn verify_remove(&self, name: &str) -> SysResult<Check> {
        if self.exists(name)? {
            Ok(Check::failed(
                op_id(&["os", "cert", "remove"]),
                format!("{name} is still in the trust store"),
            ))
        } else {
            Ok(Check::ok(
                op_id(&["os", "cert", "remove"]),
                format!("{name} is gone"),
            ))
        }
    }

    /// Generates a certificate-signing request.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] for a bad request, and the `openssl` failures.
    pub fn apply_csr(&self, request: &CsrRequest) -> SysResult<Outcome> {
        request.validate()?;
        let spec = CommandSpec::new("openssl")?.args([
            "req",
            "-new",
            "-newkey",
            &format!("rsa:{}", request.key_bits),
            "-nodes",
            "-keyout",
            &request.key_path,
            "-out",
            &request.csr_path,
            "-subj",
            &request.subject,
        ])?;
        let output = self.runner.run(&spec)?;
        output.require_success("generating the certificate-signing request")?;
        Ok(Outcome::new(
            op_id(&["os", "cert", "csr"]),
            true,
            format!("wrote a signing request to {}", request.csr_path),
        ))
    }

    /// Verifies the signing request was written.
    ///
    /// # Errors
    /// [`crate::error::IO`] when the file cannot be inspected.
    pub fn verify_csr(&self, request: &CsrRequest) -> SysResult<Check> {
        if self.fs.exists(&PathBuf::from(&request.csr_path))? {
            Ok(Check::ok(
                op_id(&["os", "cert", "csr"]),
                format!("{} is present", request.csr_path),
            ))
        } else {
            Ok(Check::failed(
                op_id(&["os", "cert", "csr"]),
                format!("{} is missing", request.csr_path),
            ))
        }
    }

    /// Checks a certificate's expiry with `openssl x509 -checkend <within_secs>`: it passes
    /// when the certificate stays valid for at least `within_secs`.
    ///
    /// # Errors
    /// [`CERT_MISSING`] when the certificate is absent, and the command's failures.
    pub fn check_expiry(&self, name: &str, within_secs: u64) -> SysResult<Check> {
        let path = self.anchor_path(name)?;
        if !self.fs.exists(&path)? {
            return Err(SysError::new(
                CERT_MISSING,
                format!("{name} is not in the trust store"),
            ));
        }
        let spec = CommandSpec::new("openssl")?.args([
            "x509",
            "-in",
            &path.display().to_string(),
            "-noout",
            "-checkend",
            &within_secs.to_string(),
        ])?;
        let output = self.runner.run(&spec)?;
        if output.success() {
            Ok(Check::ok(
                op_id(&["os", "cert", "check"]),
                format!("{name} stays valid for at least {within_secs}s"),
            ))
        } else {
            Ok(Check::failed(
                op_id(&["os", "cert", "check"]),
                format!("{name} expires within {within_secs}s"),
            ))
        }
    }

    /// Checks a certificate's chain against the trust store's CA bundle.
    ///
    /// # Errors
    /// The family's failures, [`crate::error::IO`] when the file cannot be read, and
    /// [`CERT_CHAIN`] when `openssl verify` refuses it.
    pub fn check_chain(&self, cert_path: &str) -> SysResult<Check> {
        validate_absolute(cert_path, "the certificate path")?;
        if !self.fs.exists(&PathBuf::from(cert_path))? {
            return Err(SysError::new(
                CERT_MISSING,
                format!("{cert_path} does not exist"),
            ));
        }
        let family = self.family()?;
        let spec = CommandSpec::new("openssl")?.args([
            "verify",
            "-CAfile",
            family.ca_bundle(),
            cert_path,
        ])?;
        let output = self.runner.run(&spec)?;
        if output.success() {
            Ok(Check::ok(
                op_id(&["os", "cert", "check"]),
                format!("{cert_path} verifies against the trust store"),
            ))
        } else {
            Err(SysError::new(
                CERT_CHAIN,
                format!("{cert_path} does not verify against the trust store"),
            ))
        }
    }

    /// Refreshes the trust store with the family's command.
    ///
    /// # Errors
    /// The refresh command's failures.
    pub fn refresh(&self) -> SysResult<()> {
        let spec = match self.family()? {
            DistroFamily::Rhel => CommandSpec::new("update-ca-trust")?.arg("extract")?,
            DistroFamily::Debian => CommandSpec::new("update-ca-certificates")?,
        };
        let output = self.runner.run(&spec)?;
        output.require_success("refreshing the trust store")
    }
}

/// Parses the distribution family from `/etc/os-release`.
///
/// # Errors
/// [`crate::error::UNSUPPORTED`] for a family this module does not know.
pub fn parse_family(text: &str) -> SysResult<DistroFamily> {
    let mut id = String::new();
    let mut like = String::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_ascii_lowercase();
        match key.trim() {
            "ID" => id = value,
            "ID_LIKE" => like = value,
            _ => {}
        }
    }
    let combined = format!("{id} {like}");
    if combined.contains("debian") || combined.contains("ubuntu") {
        return Ok(DistroFamily::Debian);
    }
    if combined.contains("rhel")
        || combined.contains("fedora")
        || combined.contains("centos")
        || combined.contains("rocky")
        || combined.contains("alma")
        || combined.contains("oracle")
    {
        return Ok(DistroFamily::Rhel);
    }
    Err(SysError::unsupported(format!(
        "the distribution {id:?} is not a supported family"
    )))
}

/// Validates a certificate's base name: `[A-Za-z0-9][A-Za-z0-9._-]*`.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] with the reason.
pub fn validate_cert_name(name: &str) -> SysResult<()> {
    if name.is_empty() || name.len() > 255 {
        return Err(SysError::invalid(
            "a certificate name must be 1 to 255 characters",
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(SysError::invalid(
            "a certificate name may only contain letters, digits, dot, underscore and hyphen",
        ));
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err(SysError::invalid(
            "a certificate name may not begin with a dot or a hyphen",
        ));
    }
    Ok(())
}

/// Validates an absolute path with no `..`.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] with the reason.
pub fn validate_absolute(path: &str, what: &str) -> SysResult<()> {
    if !path.starts_with('/') {
        return Err(SysError::invalid(format!(
            "{what} must be an absolute path"
        )));
    }
    if path.contains("..") || path.contains('\0') {
        return Err(SysError::invalid(format!("{what} may not contain '..'")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testing::{FakeFs, FakeRunner, ok_output};

    const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

    fn fedora() -> FakeFs {
        FakeFs::new().with_file("/etc/os-release", "ID=fedora\nID_LIKE=\"rhel fedora\"\n")
    }

    fn ubuntu() -> FakeFs {
        FakeFs::new().with_file("/etc/os-release", "ID=ubuntu\nID_LIKE=debian\n")
    }

    #[test]
    fn families_decide_paths_and_commands() {
        assert_eq!(parse_family("ID=ubuntu\n").unwrap(), DistroFamily::Debian);
        assert_eq!(parse_family("ID=\"rocky\"\n").unwrap(), DistroFamily::Rhel);
        assert!(parse_family("ID=plan9\n").is_err());
        assert_eq!(
            DistroFamily::Rhel.anchor_dir(),
            "/etc/pki/ca-trust/source/anchors"
        );
        assert_eq!(
            DistroFamily::Debian.anchor_dir(),
            "/usr/local/share/ca-certificates"
        );
    }

    #[test]
    fn installing_on_fedora_writes_and_refreshes() {
        let runner = FakeRunner::new().with_response("update-ca-trust extract", ok_output(""));
        let fs = fedora();
        let module = CertificatesModule::new(&runner, &fs);
        let outcome = module.apply_install("spot-root", PEM).unwrap();
        assert!(outcome.changed);
        assert_eq!(
            fs.file("/etc/pki/ca-trust/source/anchors/spot-root.crt"),
            Some(PEM.to_owned())
        );
        assert_eq!(
            runner.last_call().unwrap().argv_key(),
            "update-ca-trust extract"
        );
        // Idempotent: the same PEM changes nothing.
        assert!(!module.apply_install("spot-root", PEM).unwrap().changed);
    }

    #[test]
    fn installing_on_ubuntu_uses_its_paths() {
        let runner = FakeRunner::new().with_response("update-ca-certificates", ok_output(""));
        let fs = ubuntu();
        let module = CertificatesModule::new(&runner, &fs);
        module.apply_install("spot-root", PEM).unwrap();
        assert!(fs.has_file("/usr/local/share/ca-certificates/spot-root.crt"));
    }

    #[test]
    fn a_bad_name_or_pem_is_refused() {
        let runner = FakeRunner::new();
        let fs = fedora();
        let module = CertificatesModule::new(&runner, &fs);
        assert!(module.plan_install("-rf", PEM).is_err());
        assert_eq!(
            module
                .plan_install("ok", "not a certificate")
                .unwrap_err()
                .code,
            CERT_INVALID
        );
    }

    #[test]
    fn removing_is_idempotent() {
        let runner = FakeRunner::new().with_response("update-ca-trust extract", ok_output(""));
        let fs = fedora();
        let module = CertificatesModule::new(&runner, &fs);
        assert!(!module.apply_remove("nothing").unwrap().changed);
        module.apply_install("spot-root", PEM).unwrap();
        assert!(module.apply_remove("spot-root").unwrap().changed);
        assert!(!fs.has_file("/etc/pki/ca-trust/source/anchors/spot-root.crt"));
    }

    #[test]
    fn a_csr_builds_the_expected_argv() {
        let runner = FakeRunner::new().with_response(
            "openssl req -new -newkey rsa:2048 -nodes -keyout /root/spot.key -out /root/spot.csr -subj /CN=spot.example",
            ok_output(""),
        );
        let fs = fedora();
        let module = CertificatesModule::new(&runner, &fs);
        let request = CsrRequest {
            subject: "/CN=spot.example".to_owned(),
            key_path: "/root/spot.key".to_owned(),
            csr_path: "/root/spot.csr".to_owned(),
            key_bits: 2048,
        };
        assert!(module.apply_csr(&request).unwrap().changed);
        assert_eq!(
            runner.last_call().unwrap().argv_key(),
            "openssl req -new -newkey rsa:2048 -nodes -keyout /root/spot.key -out /root/spot.csr -subj /CN=spot.example"
        );
    }

    #[test]
    fn expiry_and_chain_use_openssl() {
        let runner = FakeRunner::new()
            .with_response(
                "openssl x509 -in /etc/pki/ca-trust/source/anchors/spot-root.crt -noout -checkend 0",
                ok_output(""),
            )
            .with_response(
                "openssl x509 -in /etc/pki/ca-trust/source/anchors/spot-root.crt -noout -checkend 86400",
                crate::sys::CommandOutput::new(1, "", "certificate will expire"),
            )
            .with_response(
                "openssl verify -CAfile /etc/pki/tls/certs/ca-bundle.crt /etc/pki/ca-trust/source/anchors/spot-root.crt",
                ok_output(""),
            );
        let fs = fedora().with_file("/etc/pki/ca-trust/source/anchors/spot-root.crt", PEM);
        let module = CertificatesModule::new(&runner, &fs);
        assert!(module.check_expiry("spot-root", 0).unwrap().ok);
        assert!(!module.check_expiry("spot-root", 86400).unwrap().ok);
        assert!(
            module
                .check_chain("/etc/pki/ca-trust/source/anchors/spot-root.crt")
                .unwrap()
                .ok
        );
    }

    #[test]
    fn checking_a_missing_certificate_is_an_error() {
        let runner = FakeRunner::new();
        let fs = fedora();
        let module = CertificatesModule::new(&runner, &fs);
        assert_eq!(
            module.check_expiry("nope", 0).unwrap_err().code,
            CERT_MISSING
        );
    }
}
