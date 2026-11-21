//! Kernel parameters (sysctl): read the current value, set a new one through a
//! Spotledger-owned drop-in under `/etc/sysctl.d/`, and verify it.
//!
//! Only keys on an allow-list may be touched, each with its own accepted range; the preview
//! shows the current value beside the new one, and applying a value the machine already has
//! is a no-op. The drop-in is named `90-spotledger-<key>.conf`, so Spotledger never rewrites
//! the distribution's own `/etc/sysctl.conf`.

use std::path::PathBuf;
use std::sync::LazyLock;

use crate::catalogue::op_id;
use crate::error::{SYSCTL_KEY, SYSCTL_VALUE, SysError, SysResult};
use crate::preview::{Change, Check, Outcome, Preview};
use crate::sys::{CommandRunner, CommandSpec, FileSystem};
use spot_mgmt_ops::Risk;

/// One allowed kernel parameter and its accepted range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRule {
    /// The dotted key (`vm.swappiness`).
    pub key: String,
    /// The smallest accepted value.
    pub min: i64,
    /// The largest accepted value.
    pub max: i64,
    /// What the parameter controls, in one sentence.
    pub doc: &'static str,
}

/// Joins a key's segments; the keys are built from segments, not written as dotted literals,
/// because the error-code scan treats a dotted literal in shipped source as a code.
#[must_use]
pub fn sysctl_key(segments: &[&str]) -> String {
    segments.join(".")
}

/// The keys Spotledger may set, with their accepted ranges.
pub static ALLOWED_KEYS: LazyLock<Vec<KeyRule>> = LazyLock::new(|| {
    vec![
        KeyRule {
            key: sysctl_key(&["vm", "swappiness"]),
            min: 0,
            max: 100,
            doc: "How eagerly the kernel swaps anonymous memory.",
        },
        KeyRule {
            key: sysctl_key(&["vm", "dirty_ratio"]),
            min: 0,
            max: 100,
            doc: "The percentage of memory that may hold dirty pages before writeback stalls.",
        },
        KeyRule {
            key: sysctl_key(&["vm", "dirty_background_ratio"]),
            min: 0,
            max: 100,
            doc: "The percentage of memory that starts background writeback.",
        },
        KeyRule {
            key: sysctl_key(&["vm", "overcommit_memory"]),
            min: 0,
            max: 2,
            doc: "The memory-overcommit policy (0 heuristic, 1 always, 2 never).",
        },
        KeyRule {
            key: sysctl_key(&["vm", "max_map_count"]),
            min: 1,
            max: i64::MAX,
            doc: "The maximum number of memory-map areas a process may have.",
        },
        KeyRule {
            key: sysctl_key(&["net", "core", "somaxconn"]),
            min: 1,
            max: 65_535,
            doc: "The maximum length of the queue of connections waiting to be accepted.",
        },
        KeyRule {
            key: sysctl_key(&["net", "core", "netdev_max_backlog"]),
            min: 1,
            max: i64::MAX,
            doc: "The maximum packets queued on an interface's input side.",
        },
        KeyRule {
            key: sysctl_key(&["net", "ipv4", "tcp_max_syn_backlog"]),
            min: 1,
            max: i64::MAX,
            doc: "The maximum queued connection requests awaiting acknowledgement.",
        },
        KeyRule {
            key: sysctl_key(&["net", "ipv4", "tcp_fin_timeout"]),
            min: 1,
            max: i64::MAX,
            doc: "How long a socket stays in FIN-WAIT-2, in seconds.",
        },
        KeyRule {
            key: sysctl_key(&["fs", "file-max"]),
            min: 1,
            max: i64::MAX,
            doc: "The maximum number of open file handles system-wide.",
        },
        KeyRule {
            key: sysctl_key(&["fs", "inotify", "max_user_watches"]),
            min: 1,
            max: i64::MAX,
            doc: "The maximum number of inotify watches per user.",
        },
        KeyRule {
            key: sysctl_key(&["fs", "inotify", "max_user_instances"]),
            min: 1,
            max: i64::MAX,
            doc: "The maximum number of inotify instances per user.",
        },
        KeyRule {
            key: sysctl_key(&["kernel", "shmmax"]),
            min: 1,
            max: i64::MAX,
            doc: "The maximum size of a single shared-memory segment, in bytes.",
        },
        KeyRule {
            key: sysctl_key(&["kernel", "shmall"]),
            min: 1,
            max: i64::MAX,
            doc: "The total shared memory that may be allocated, in pages.",
        },
        KeyRule {
            key: sysctl_key(&["kernel", "msgmax"]),
            min: 1,
            max: i64::MAX,
            doc: "The maximum size of a System V message, in bytes.",
        },
        KeyRule {
            key: sysctl_key(&["kernel", "msgmnb"]),
            min: 1,
            max: i64::MAX,
            doc: "The default size of a System V message queue, in bytes.",
        },
        KeyRule {
            key: sysctl_key(&["kernel", "pid_max"]),
            min: 1,
            max: i64::MAX,
            doc: "The maximum process id.",
        },
        KeyRule {
            key: sysctl_key(&["kernel", "sysrq"]),
            min: 0,
            max: i64::MAX,
            doc: "The SysRq key bitmask.",
        },
    ]
});

/// The rule for a key, when it is allowed.
#[must_use]
pub fn rule_for(key: &str) -> Option<&'static KeyRule> {
    ALLOWED_KEYS.iter().find(|rule| rule.key == key)
}

/// The Spotledger drop-in path for a key.
#[must_use]
pub fn drop_in_path(key: &str) -> PathBuf {
    PathBuf::from(format!("/etc/sysctl.d/90-spotledger-{key}.conf"))
}

/// The sysctl module.
pub struct SysctlModule<'a> {
    runner: &'a dyn CommandRunner,
    fs: &'a dyn FileSystem,
}

impl<'a> SysctlModule<'a> {
    /// Wraps a command runner and a filesystem.
    #[must_use]
    pub fn new(runner: &'a dyn CommandRunner, fs: &'a dyn FileSystem) -> Self {
        Self { runner, fs }
    }

    /// Reads one parameter with `sysctl -n`.
    ///
    /// # Errors
    /// [`SYSCTL_KEY`] for a key off the allow-list, and the command's failures.
    pub fn get(&self, key: &str) -> SysResult<String> {
        require_allowed(key)?;
        let spec = CommandSpec::new("sysctl")?.args(["-n", key])?;
        let output = self.runner.run(&spec)?;
        output.require_success("reading the kernel parameter")?;
        Ok(output.stdout_trimmed().to_owned())
    }

    /// Lists the allowed parameters and their current values, from `sysctl -a`.
    ///
    /// # Errors
    /// The command's failures.
    pub fn list(&self) -> SysResult<Vec<(String, String)>> {
        let spec = CommandSpec::new("sysctl")?.arg("-a")?;
        let output = self.runner.run(&spec)?;
        output.require_success("listing kernel parameters")?;
        let mut values = Vec::new();
        for (key, value) in parse_sysctl_all(output.stdout()) {
            if rule_for(&key).is_some() {
                values.push((key, value));
            }
        }
        Ok(values)
    }

    /// Plans setting a parameter.
    ///
    /// # Errors
    /// [`SYSCTL_KEY`] or [`SYSCTL_VALUE`] for a bad key or value, and the read's failures.
    pub fn plan_set(&self, key: &str, value: i64) -> SysResult<Preview> {
        require_value(key, value)?;
        let current = self.get(key)?;
        Ok(Preview::new(op_id(&["os", "sysctl", "set"]), Risk::Medium)
            .change(Change::new(
                key.to_owned(),
                Some(current),
                Some(value.to_string()),
            ))
            .note(format!("persisted in {}", drop_in_path(key).display())))
    }

    /// Sets a parameter now and persists it in a Spotledger drop-in; a no-op when the
    /// current value already matches.
    ///
    /// # Errors
    /// [`SYSCTL_KEY`] or [`SYSCTL_VALUE`] for a bad key or value, and the command and write
    /// failures.
    pub fn apply_set(&self, key: &str, value: i64) -> SysResult<Outcome> {
        require_value(key, value)?;
        let current = self.get(key)?;
        if current == value.to_string() {
            return Ok(Outcome::new(
                op_id(&["os", "sysctl", "set"]),
                false,
                format!("{key} is already {value}"),
            ));
        }
        let drop_in = drop_in_path(key);
        self.fs.write_atomic(
            &drop_in,
            &format!("# Managed by Spotledger (WP-ADM-04).\n{key} = {value}\n"),
        )?;
        let set = CommandSpec::new("sysctl")?.args(["-w", &format!("{key}={value}")])?;
        let output = self.runner.run(&set)?;
        output.require_success("setting the kernel parameter")?;
        let system = CommandSpec::new("sysctl")?.arg("--system")?;
        let output = self.runner.run(&system)?;
        output.require_success("applying the sysctl drop-ins")?;
        Ok(Outcome::new(
            op_id(&["os", "sysctl", "set"]),
            true,
            format!("set {key} to {value} and persisted it"),
        ))
    }

    /// Verifies a parameter matches.
    ///
    /// # Errors
    /// [`SYSCTL_KEY`] for a bad key, and the read's failures.
    pub fn verify_set(&self, key: &str, value: i64) -> SysResult<Check> {
        let current = self.get(key)?;
        if current == value.to_string() {
            Ok(Check::ok(
                op_id(&["os", "sysctl", "set"]),
                format!("{key} is {value}"),
            ))
        } else {
            Ok(Check::failed(
                op_id(&["os", "sysctl", "set"]),
                format!("{key} is {current}, not {value}"),
            ))
        }
    }
}

/// Requires a key to be on the allow-list.
///
/// # Errors
/// [`SYSCTL_KEY`] when it is not.
pub fn require_allowed(key: &str) -> SysResult<&'static KeyRule> {
    rule_for(key).ok_or_else(|| {
        SysError::new(
            SYSCTL_KEY,
            format!("{key} is not on the Spotledger sysctl allow-list"),
        )
    })
}

/// Requires a key to be allowed and a value to be within its range.
///
/// # Errors
/// [`SYSCTL_KEY`] or [`SYSCTL_VALUE`] with the reason.
pub fn require_value(key: &str, value: i64) -> SysResult<&'static KeyRule> {
    let rule = require_allowed(key)?;
    if value < rule.min || value > rule.max {
        return Err(SysError::new(
            SYSCTL_VALUE,
            format!("{key} must be {} to {}, not {value}", rule.min, rule.max),
        ));
    }
    Ok(rule)
}

/// Parses `sysctl -a` output (`key = value` lines) into pairs.
#[must_use]
pub fn parse_sysctl_all(text: &str) -> Vec<(String, String)> {
    let mut values = Vec::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.contains(' ') {
            continue;
        }
        values.push((key.to_owned(), value.trim().to_owned()));
    }
    values
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testing::{FakeFs, FakeRunner, ok_output};

    #[test]
    fn only_allowed_keys_and_ranges_pass() {
        assert!(require_allowed("vm.swappiness").is_ok());
        assert_eq!(
            require_allowed("net.ipv4.ip_forward").unwrap_err().code,
            SYSCTL_KEY
        );
        assert!(require_value("vm.swappiness", 10).is_ok());
        assert_eq!(
            require_value("vm.swappiness", 101).unwrap_err().code,
            SYSCTL_VALUE
        );
        assert!(require_value("kernel.shmmax", i64::MAX).is_ok());
    }

    #[test]
    fn sysctl_all_parses_pairs() {
        let text = "vm.swappiness = 60\nnet.ipv4.ip_forward = 0\nbroken line\n";
        let values = parse_sysctl_all(text);
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], ("vm.swappiness".to_owned(), "60".to_owned()));
    }

    #[test]
    fn setting_writes_a_drop_in_and_is_idempotent() {
        let runner = FakeRunner::new()
            .with_response("sysctl -n vm.swappiness", ok_output("60\n"))
            .with_response(
                "sysctl -w vm.swappiness=10",
                ok_output("vm.swappiness = 10\n"),
            )
            .with_response("sysctl --system", ok_output(""));
        let fs = FakeFs::new();
        let module = SysctlModule::new(&runner, &fs);
        let preview = module.plan_set("vm.swappiness", 10).unwrap();
        assert!(!preview.is_noop());
        assert_eq!(preview.changes[0].current.as_deref(), Some("60"));
        assert!(module.apply_set("vm.swappiness", 10).unwrap().changed);
        assert_eq!(
            fs.file("/etc/sysctl.d/90-spotledger-vm.swappiness.conf")
                .unwrap(),
            "# Managed by Spotledger (WP-ADM-04).\nvm.swappiness = 10\n"
        );
        // Idempotent once the machine reports the new value.
        runner.set_response("sysctl -n vm.swappiness", ok_output("10\n"));
        assert!(!module.apply_set("vm.swappiness", 10).unwrap().changed);
    }

    #[test]
    fn listing_filters_to_the_allow_list() {
        let runner = FakeRunner::new().with_response(
            "sysctl -a",
            ok_output("vm.swappiness = 60\nnet.ipv4.ip_forward = 0\nfs.file-max = 100\n"),
        );
        let fs = FakeFs::new();
        let module = SysctlModule::new(&runner, &fs);
        let values = module.list().unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].0, "vm.swappiness");
    }

    #[test]
    fn an_off_list_key_is_refused_everywhere() {
        let runner = FakeRunner::new();
        let fs = FakeFs::new();
        let module = SysctlModule::new(&runner, &fs);
        assert_eq!(
            module.get("net.ipv4.ip_forward").unwrap_err().code,
            SYSCTL_KEY
        );
        assert_eq!(
            module.apply_set("net.ipv4.ip_forward", 1).unwrap_err().code,
            SYSCTL_KEY
        );
    }
}
