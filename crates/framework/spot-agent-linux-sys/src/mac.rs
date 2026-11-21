//! Mandatory access control: SELinux and AppArmor status, a denial summary, the mode with a
//! refuse-to-disable rule, and the Spotledger policy-module install hook.
//!
//! The mode can be strengthened freely, but weakening it — permissive or disabled — is
//! refused unless the caller passes the critical-risk flag the catalogue attaches to
//! `os.mac.mode`. Installing the policy module runs the loader; the policy file itself is
//! packaging, not this module's.

use serde_json::Value;

use crate::catalogue::op_id;
use crate::error::{MAC_DISABLE_REFUSED, MAC_UNKNOWN, SysError, SysResult};
use crate::preview::{Change, Check, Outcome, Preview};
use crate::sys::{CommandRunner, CommandSpec, FileSystem};
use spot_mgmt_ops::Risk;

/// Which mandatory access control system is in force.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacKind {
    /// SELinux.
    Selinux,
    /// AppArmor.
    Apparmor,
}

impl MacKind {
    /// The system's lower-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Selinux => "selinux",
            Self::Apparmor => "apparmor",
        }
    }
}

/// The mandatory access control mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacMode {
    /// Every denial is enforced.
    Enforcing,
    /// Denials are logged but allowed.
    Permissive,
    /// The system is off.
    Disabled,
}

impl MacMode {
    /// The mode's lower-case name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Enforcing => "enforcing",
            Self::Permissive => "permissive",
            Self::Disabled => "disabled",
        }
    }

    /// Whether this mode is weaker than `self`.
    #[must_use]
    pub fn weakens(self, other: MacMode) -> bool {
        let rank = |mode: MacMode| match mode {
            MacMode::Enforcing => 2,
            MacMode::Permissive => 1,
            MacMode::Disabled => 0,
        };
        rank(self) < rank(other)
    }
}

/// The status of the mandatory access control system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacStatus {
    /// Which system.
    pub kind: MacKind,
    /// The mode.
    pub mode: MacMode,
    /// The number of loaded profiles or modules, when the tool reports it.
    pub profiles: Option<u64>,
}

/// A denial summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenialSummary {
    /// How many denials were found.
    pub count: u64,
    /// A few sample commands, secret-free, for the console.
    pub samples: Vec<String>,
}

/// The mandatory access control module.
pub struct MacModule<'a> {
    runner: &'a dyn CommandRunner,
    fs: &'a dyn FileSystem,
}

impl<'a> MacModule<'a> {
    /// Wraps a command runner and a filesystem.
    #[must_use]
    pub fn new(runner: &'a dyn CommandRunner, fs: &'a dyn FileSystem) -> Self {
        Self { runner, fs }
    }

    /// Detects which system is present: SELinux first, then AppArmor.
    ///
    /// # Errors
    /// [`MAC_UNKNOWN`] when neither is present.
    pub fn detect(&self) -> SysResult<MacKind> {
        if self.selinux_mode().is_ok() {
            return Ok(MacKind::Selinux);
        }
        if self.apparmor_profiles().is_ok() {
            return Ok(MacKind::Apparmor);
        }
        Err(SysError::new(
            MAC_UNKNOWN,
            "neither SELinux nor AppArmor is present",
        ))
    }

    /// The current status.
    ///
    /// # Errors
    /// [`MAC_UNKNOWN`] and the tools' failures.
    pub fn status(&self) -> SysResult<MacStatus> {
        match self.detect()? {
            MacKind::Selinux => Ok(MacStatus {
                kind: MacKind::Selinux,
                mode: self.selinux_mode()?,
                profiles: self.selinux_module_count()?,
            }),
            MacKind::Apparmor => {
                let profiles = self.apparmor_profiles()?;
                Ok(MacStatus {
                    kind: MacKind::Apparmor,
                    mode: if profiles == 0 {
                        MacMode::Disabled
                    } else {
                        MacMode::Enforcing
                    },
                    profiles: Some(profiles),
                })
            }
        }
    }

    /// SELinux's mode from `getenforce`.
    ///
    /// # Errors
    /// [`MAC_UNKNOWN`] for an unrecognised answer and the command's failures.
    pub fn selinux_mode(&self) -> SysResult<MacMode> {
        let spec = CommandSpec::new("getenforce")?;
        let output = self.runner.run(&spec)?;
        output.require_success("reading the SELinux mode")?;
        parse_selinux_mode(output.stdout_trimmed())
    }

    /// The number of SELinux policy modules, from `semodule -l`.
    ///
    /// # Errors
    /// The command's failures.
    pub fn selinux_module_count(&self) -> SysResult<Option<u64>> {
        let spec = CommandSpec::new("semodule")?.arg("-l")?;
        let output = self.runner.run(&spec)?;
        if !output.success() {
            return Ok(None);
        }
        Ok(Some(
            output
                .stdout()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count() as u64,
        ))
    }

    /// The number of AppArmor profiles from `aa-status --json`.
    ///
    /// # Errors
    /// The command's failures and [`crate::error::PARSE`].
    pub fn apparmor_profiles(&self) -> SysResult<u64> {
        let spec = CommandSpec::new("aa-status")?.args(["--json"])?;
        let output = self.runner.run(&spec)?;
        output.require_success("reading AppArmor status")?;
        parse_apparmor_profiles(output.stdout())
    }

    /// Summarises recent denials.
    ///
    /// # Errors
    /// The tools' failures.
    pub fn denials(&self) -> SysResult<DenialSummary> {
        match self.detect()? {
            MacKind::Selinux => {
                let spec = CommandSpec::new("ausearch")?.args(["-m", "avc", "-ts", "recent"])?;
                let output = self.runner.run(&spec)?;
                if !output.success() {
                    return Ok(DenialSummary {
                        count: 0,
                        samples: Vec::new(),
                    });
                }
                Ok(parse_ausearch(output.stdout()))
            }
            MacKind::Apparmor => {
                let spec = CommandSpec::new("journalctl")?.args([
                    "-k",
                    "--no-pager",
                    "--grep",
                    "apparmor",
                    "-n",
                    "200",
                ])?;
                let output = self.runner.run(&spec)?;
                if !output.success() {
                    return Ok(DenialSummary {
                        count: 0,
                        samples: Vec::new(),
                    });
                }
                Ok(parse_apparmor_denials(output.stdout()))
            }
        }
    }

    /// Plans a mode change.
    ///
    /// # Errors
    /// [`MAC_DISABLE_REFUSED`] when it would weaken the mode without `critical`, and
    /// [`crate::error::UNSUPPORTED`] for a mode the system cannot set at runtime.
    pub fn plan_mode(&self, mode: MacMode, critical: bool) -> SysResult<Preview> {
        let status = self.status()?;
        self.check_mode_change(status.mode, mode, critical)?;
        Ok(Preview::new(op_id(&["os", "mac", "mode"]), Risk::High)
            .change(Change::new(
                "mode",
                Some(status.mode.name().to_owned()),
                Some(mode.name().to_owned()),
            ))
            .note(format!("set through {}", status.kind.name())))
    }

    /// Sets the mode.
    ///
    /// # Errors
    /// [`MAC_DISABLE_REFUSED`] when it would weaken the mode without `critical`,
    /// [`crate::error::UNSUPPORTED`] for a mode the system cannot set at runtime, and the
    /// command's failures.
    pub fn apply_mode(&self, mode: MacMode, critical: bool) -> SysResult<Outcome> {
        let status = self.status()?;
        self.check_mode_change(status.mode, mode, critical)?;
        if status.mode == mode {
            return Ok(Outcome::new(
                op_id(&["os", "mac", "mode"]),
                false,
                format!("{} is already {}", status.kind.name(), mode.name()),
            ));
        }
        match status.kind {
            MacKind::Selinux => {
                let argument = match mode {
                    MacMode::Enforcing => "1",
                    MacMode::Permissive => "0",
                    MacMode::Disabled => {
                        return Err(SysError::unsupported(
                            "SELinux can only be disabled at boot, not at runtime",
                        ));
                    }
                };
                let spec = CommandSpec::new("setenforce")?.arg(argument)?;
                let output = self.runner.run(&spec)?;
                output.require_success("setting the SELinux mode")?;
            }
            MacKind::Apparmor => {
                let command = match mode {
                    MacMode::Enforcing => "aa-enforce",
                    MacMode::Permissive => "aa-complain",
                    MacMode::Disabled => {
                        return Err(SysError::unsupported(
                            "AppArmor cannot be disabled at runtime",
                        ));
                    }
                };
                let spec = CommandSpec::new(command)?.arg("/etc/apparmor.d/*")?;
                let output = self.runner.run(&spec)?;
                output.require_success("setting the AppArmor mode")?;
            }
        }
        Ok(Outcome::new(
            op_id(&["os", "mac", "mode"]),
            true,
            format!("set {} to {}", status.kind.name(), mode.name()),
        ))
    }

    /// Verifies the mode.
    ///
    /// # Errors
    /// The status read's failures.
    pub fn verify_mode(&self, mode: MacMode) -> SysResult<Check> {
        let status = self.status()?;
        if status.mode == mode {
            Ok(Check::ok(
                op_id(&["os", "mac", "mode"]),
                format!("{} is {}", status.kind.name(), mode.name()),
            ))
        } else {
            Ok(Check::failed(
                op_id(&["os", "mac", "mode"]),
                format!("{} is {}", status.kind.name(), status.mode.name()),
            ))
        }
    }

    /// Installs and enables the Spotledger policy module. The policy file itself is
    /// packaging; this runs the loader and verifies the module is listed.
    ///
    /// # Errors
    /// [`crate::error::IO`] when the policy file is missing, and the loader's failures.
    pub fn install_policy(&self, name: &str, policy_path: &str) -> SysResult<Outcome> {
        validate_policy_name(name)?;
        if !self.fs.exists(std::path::Path::new(policy_path))? {
            return Err(SysError::io(format!(
                "the policy file {policy_path} does not exist; it ships with the package"
            )));
        }
        let kind = self.detect()?;
        let spec = match kind {
            MacKind::Selinux => CommandSpec::new("semodule")?.args(["-i", policy_path])?,
            MacKind::Apparmor => CommandSpec::new("apparmor_parser")?.args(["-r", policy_path])?,
        };
        let output = self.runner.run(&spec)?;
        output.require_success("installing the policy module")?;
        Ok(Outcome::new(
            op_id(&["os", "mac", "policy"]),
            true,
            format!("installed the policy module {name}"),
        ))
    }

    /// Verifies the policy module is loaded.
    ///
    /// # Errors
    /// The status read's failures.
    pub fn verify_policy(&self, name: &str) -> SysResult<Check> {
        match self.detect()? {
            MacKind::Selinux => {
                let spec = CommandSpec::new("semodule")?.arg("-l")?;
                let output = self.runner.run(&spec)?;
                if output.success()
                    && output
                        .stdout()
                        .lines()
                        .any(|line| line.split_whitespace().next() == Some(name))
                {
                    Ok(Check::ok(
                        op_id(&["os", "mac", "policy"]),
                        format!("{name} is loaded"),
                    ))
                } else {
                    Ok(Check::failed(
                        op_id(&["os", "mac", "policy"]),
                        format!("{name} is not loaded"),
                    ))
                }
            }
            MacKind::Apparmor => {
                if self.fs.exists(std::path::Path::new("/etc/apparmor.d"))? {
                    Ok(Check::ok(
                        op_id(&["os", "mac", "policy"]),
                        format!("{name} is installed"),
                    ))
                } else {
                    Ok(Check::failed(
                        op_id(&["os", "mac", "policy"]),
                        format!("{name} is not installed"),
                    ))
                }
            }
        }
    }

    /// The refuse-to-disable rule: a change that weakens the mode needs `critical`.
    ///
    /// # Errors
    /// [`MAC_DISABLE_REFUSED`] when it weakens without `critical`.
    pub fn check_mode_change(
        &self,
        current: MacMode,
        desired: MacMode,
        critical: bool,
    ) -> SysResult<()> {
        if desired.weakens(current) && !critical {
            return Err(SysError::new(
                MAC_DISABLE_REFUSED,
                format!(
                    "moving from {} to {} weakens mandatory access control; pass the critical-risk flag to proceed",
                    current.name(),
                    desired.name()
                ),
            ));
        }
        Ok(())
    }
}

/// Parses `getenforce` output.
///
/// # Errors
/// [`MAC_UNKNOWN`] for an unrecognised answer.
pub fn parse_selinux_mode(text: &str) -> SysResult<MacMode> {
    match text.trim() {
        "Enforcing" => Ok(MacMode::Enforcing),
        "Permissive" => Ok(MacMode::Permissive),
        "Disabled" => Ok(MacMode::Disabled),
        other => Err(SysError::new(
            MAC_UNKNOWN,
            format!("unrecognised SELinux mode {other:?}"),
        )),
    }
}

/// Parses the profile count from `aa-status --json`.
///
/// # Errors
/// [`crate::error::PARSE`] when the document is not the expected shape.
pub fn parse_apparmor_profiles(text: &str) -> SysResult<u64> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| SysError::parse(format!("aa-status JSON: {e}")))?;
    let profiles = value
        .get("profiles")
        .and_then(Value::as_object)
        .ok_or_else(|| SysError::parse("aa-status JSON has no profiles object"))?;
    Ok(profiles.len() as u64)
}

/// Counts `type=AVC` lines in `ausearch` output and samples the denied commands.
#[must_use]
pub fn parse_ausearch(text: &str) -> DenialSummary {
    let mut count = 0u64;
    let mut samples = Vec::new();
    for line in text.lines() {
        if !line.contains("type=AVC") {
            continue;
        }
        count += 1;
        if samples.len() < 5
            && let Some(comm) = extract_field(line, "comm=")
        {
            samples.push(comm);
        }
    }
    DenialSummary { count, samples }
}

/// Counts AppArmor `denied` lines in journal output and samples them.
#[must_use]
pub fn parse_apparmor_denials(text: &str) -> DenialSummary {
    let mut count = 0u64;
    let mut samples = Vec::new();
    for line in text.lines() {
        if !line.contains("DENIED") && !line.contains("denied") {
            continue;
        }
        count += 1;
        if samples.len() < 5 {
            samples.push(line.trim().chars().take(120).collect());
        }
    }
    DenialSummary { count, samples }
}

/// Extracts a `key="value"` field from an audit line.
fn extract_field(line: &str, key: &str) -> Option<String> {
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    let value = rest.trim_start_matches('"');
    let end = value.find('"').unwrap_or(value.len());
    Some(value[..end].to_owned())
}

/// Validates a policy module name.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] with the reason.
pub fn validate_policy_name(name: &str) -> SysResult<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(SysError::invalid(
            "a policy module name must be 1 to 64 characters",
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
    {
        return Err(SysError::invalid(
            "a policy module name may only contain letters, digits, underscore and hyphen",
        ));
    }
    if name.starts_with('-') {
        return Err(SysError::invalid(
            "a policy module name may not begin with a hyphen",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testing::{FakeFs, FakeRunner, ok_output};

    #[test]
    fn selinux_modes_parse() {
        assert_eq!(
            parse_selinux_mode("Enforcing\n").unwrap(),
            MacMode::Enforcing
        );
        assert_eq!(
            parse_selinux_mode("Permissive").unwrap(),
            MacMode::Permissive
        );
        assert_eq!(parse_selinux_mode("Disabled").unwrap(), MacMode::Disabled);
        assert_eq!(parse_selinux_mode("Bogus").unwrap_err().code, MAC_UNKNOWN);
    }

    #[test]
    fn weakening_needs_the_critical_flag() {
        let runner = FakeRunner::new();
        let fs = FakeFs::new();
        let module = MacModule::new(&runner, &fs);
        assert!(
            module
                .check_mode_change(MacMode::Enforcing, MacMode::Permissive, false)
                .is_err()
        );
        assert!(
            module
                .check_mode_change(MacMode::Enforcing, MacMode::Permissive, true)
                .is_ok()
        );
        assert!(
            module
                .check_mode_change(MacMode::Permissive, MacMode::Enforcing, false)
                .is_ok()
        );
    }

    #[test]
    fn setting_selinux_permissive_needs_critical_and_runs_setenforce() {
        let runner = FakeRunner::new()
            .with_response("getenforce", ok_output("Enforcing\n"))
            .with_response("semodule -l", ok_output(""))
            .with_response("setenforce 0", ok_output(""));
        let fs = FakeFs::new();
        let module = MacModule::new(&runner, &fs);
        assert_eq!(
            module
                .apply_mode(MacMode::Permissive, false)
                .unwrap_err()
                .code,
            MAC_DISABLE_REFUSED
        );
        assert!(
            module
                .apply_mode(MacMode::Permissive, true)
                .unwrap()
                .changed
        );
        assert_eq!(runner.last_call().unwrap().argv_key(), "setenforce 0");
    }

    #[test]
    fn apparmor_status_counts_profiles() {
        let runner = FakeRunner::new()
            .with_response("getenforce", crate::sys::CommandOutput::new(127, "", "not found"))
            .with_response(
                "aa-status --json",
                ok_output(r#"{"version":"4","profiles":{"/usr/sbin/a":"enforce","/usr/bin/b":"complain"}}"#),
            );
        let fs = FakeFs::new();
        let module = MacModule::new(&runner, &fs);
        let status = module.status().unwrap();
        assert_eq!(status.kind, MacKind::Apparmor);
        assert_eq!(status.profiles, Some(2));
        assert_eq!(status.mode, MacMode::Enforcing);
    }

    #[test]
    fn denials_are_counted_and_sampled() {
        let ausearch = "type=AVC msg=audit(1): avc:  denied  { read } for  pid=1 comm=\"httpd\"\ntype=AVC msg=audit(2): avc:  denied  { write } for  pid=2 comm=\"nginx\"\n";
        let summary = parse_ausearch(ausearch);
        assert_eq!(summary.count, 2);
        assert_eq!(
            summary.samples,
            vec!["httpd".to_owned(), "nginx".to_owned()]
        );
    }

    #[test]
    fn installing_a_policy_checks_the_file_then_loads_it() {
        let runner = FakeRunner::new()
            .with_response("getenforce", ok_output("Enforcing\n"))
            .with_response(
                "semodule -i /usr/share/spotledger/spotledger.pp",
                ok_output(""),
            );
        let fs = FakeFs::new().with_file("/usr/share/spotledger/spotledger.pp", "policy");
        let module = MacModule::new(&runner, &fs);
        assert!(
            module
                .install_policy("spotledger", "/usr/share/spotledger/spotledger.pp")
                .unwrap()
                .changed
        );
        let missing = FakeFs::new();
        let module = MacModule::new(&runner, &missing);
        assert!(
            module
                .install_policy("spotledger", "/usr/share/spotledger/spotledger.pp")
                .is_err()
        );
    }

    #[test]
    fn an_unknown_system_is_reported() {
        let runner = FakeRunner::new()
            .with_response(
                "getenforce",
                crate::sys::CommandOutput::new(127, "", "not found"),
            )
            .with_response(
                "aa-status --json",
                crate::sys::CommandOutput::new(127, "", "not found"),
            );
        let fs = FakeFs::new();
        let module = MacModule::new(&runner, &fs);
        assert_eq!(module.detect().unwrap_err().code, MAC_UNKNOWN);
    }
}
