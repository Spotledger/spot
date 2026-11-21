//! PostgreSQL parameters: a typed allow-list, validation, and reload-versus-restart.
//!
//! The console must never be able to set an arbitrary PostgreSQL parameter: that would be an
//! untyped back door around the management API. This module owns a small, typed allow-list
//! ([`PG_PARAMETERS`]) where each parameter declares its value shape, its unit and whether a
//! reload or a full restart applies it. A set is written with `ALTER SYSTEM` through
//! [`PgAdminPort`], then applied with [`PgAdminPort::reload`] or
//! [`PgAdminPort::restart`] according to the allow-list, and the choice is reported back
//! ([`ApplyMethod`]).
//!
//! The allow-list is deliberately small and grows by review: a parameter that is not here is
//! refused with [`RuntimeError::UnknownParameter`], never passed through.
//!
//! Time and the machine are arguments and injected ports; this module reads no clock and
//! touches no file.

use crate::error::{RuntimeError, RuntimeResult};

/// The shape and bounds of one allow-listed parameter's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamKind {
    /// A whole number within `[min, max]`.
    Integer {
        /// The smallest accepted value.
        min: i64,
        /// The largest accepted value.
        max: i64,
    },
    /// A PostgreSQL boolean (`on`/`off`).
    Boolean,
    /// One of a closed set of spellings.
    Choice {
        /// The accepted spellings.
        choices: &'static [&'static str],
    },
    /// A memory size in kibibytes within `[min, max]`.
    Memory {
        /// The smallest accepted size, in kibibytes.
        min_kb: u64,
        /// The largest accepted size, in kibibytes.
        max_kb: u64,
    },
    /// A duration in milliseconds within `[min, max]`.
    Duration {
        /// The smallest accepted duration, in milliseconds.
        min_ms: u64,
        /// The largest accepted duration, in milliseconds.
        max_ms: u64,
    },
    /// Free text of at most `max_len` bytes (PostgreSQL truncates or refuses longer values).
    Text {
        /// The maximum accepted length, in bytes.
        max_len: usize,
    },
}

/// The unit a parameter's value carries, shown by the console next to the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// A plain number.
    None,
    /// Kibibytes.
    Kibibytes,
    /// Milliseconds.
    Milliseconds,
    /// A PostgreSQL boolean.
    Boolean,
    /// A fixed spelling.
    Choice,
    /// Bytes of text.
    Bytes,
}

impl Unit {
    /// The short label the console prints.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "count",
            Self::Kibibytes => "kB",
            Self::Milliseconds => "ms",
            Self::Boolean => "on/off",
            Self::Choice => "choice",
            Self::Bytes => "text",
        }
    }
}

/// One allow-listed PostgreSQL parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgParameter {
    /// The PostgreSQL name, for example `max_connections`.
    pub name: &'static str,
    /// The value shape.
    pub kind: ParamKind,
    /// The unit shown by the console.
    pub unit: Unit,
    /// Whether applying a change needs a restart rather than a reload.
    pub restart_needed: bool,
    /// One sentence describing the parameter.
    pub doc: &'static str,
}

impl PgParameter {
    /// The allow-listed parameter named `name`, if any.
    #[must_use]
    pub fn find(name: &str) -> Option<&'static PgParameter> {
        PG_PARAMETERS.iter().find(|param| param.name == name)
    }
}

/// The whole allow-list, in name order.
///
/// Every entry is a parameter whose effect and bounds are safe to change through the console;
/// parameters that need expert tuning or carry credentials are deliberately absent.
pub static PG_PARAMETERS: &[PgParameter] = &[
    PgParameter {
        name: "autovacuum",
        kind: ParamKind::Boolean,
        unit: Unit::Boolean,
        restart_needed: false,
        doc: "Whether the autovacuum launcher runs.",
    },
    PgParameter {
        name: "checkpoint_timeout",
        kind: ParamKind::Duration {
            min_ms: 30_000,
            max_ms: 86_400_000,
        },
        unit: Unit::Milliseconds,
        restart_needed: false,
        doc: "How long between checkpoints.",
    },
    PgParameter {
        name: "effective_cache_size",
        kind: ParamKind::Memory {
            min_kb: 8 * 1024,
            max_kb: 1024 * 1024 * 1024,
        },
        unit: Unit::Kibibytes,
        restart_needed: false,
        doc: "The planner's estimate of the operating system's page cache.",
    },
    PgParameter {
        name: "listen_addresses",
        kind: ParamKind::Text { max_len: 256 },
        unit: Unit::Bytes,
        restart_needed: true,
        doc: "The addresses PostgreSQL listens on.",
    },
    PgParameter {
        name: "log_min_duration_statement",
        kind: ParamKind::Duration {
            min_ms: 0,
            max_ms: 86_400_000,
        },
        unit: Unit::Milliseconds,
        restart_needed: false,
        doc: "Log statements that run at least this long; 0 logs every statement.",
    },
    PgParameter {
        name: "maintenance_work_mem",
        kind: ParamKind::Memory {
            min_kb: 1024,
            max_kb: 2 * 1024 * 1024,
        },
        unit: Unit::Kibibytes,
        restart_needed: false,
        doc: "Memory used by maintenance operations such as VACUUM.",
    },
    PgParameter {
        name: "max_connections",
        kind: ParamKind::Integer {
            min: 10,
            max: 10_000,
        },
        unit: Unit::None,
        restart_needed: true,
        doc: "The maximum number of concurrent client connections.",
    },
    PgParameter {
        name: "max_wal_size",
        kind: ParamKind::Memory {
            min_kb: 32 * 1024,
            max_kb: 1024 * 1024,
        },
        unit: Unit::Kibibytes,
        restart_needed: false,
        doc: "The WAL size that triggers a checkpoint.",
    },
    PgParameter {
        name: "password_encryption",
        kind: ParamKind::Choice {
            choices: &["scram-sha-256"],
        },
        unit: Unit::Choice,
        restart_needed: false,
        doc: "The algorithm used to encrypt new passwords.",
    },
    PgParameter {
        name: "shared_buffers",
        kind: ParamKind::Memory {
            min_kb: 128 * 1024,
            max_kb: 64 * 1024 * 1024,
        },
        unit: Unit::Kibibytes,
        restart_needed: true,
        doc: "The shared memory buffer pool.",
    },
    PgParameter {
        name: "wal_level",
        kind: ParamKind::Choice {
            choices: &["minimal", "replica", "logical"],
        },
        unit: Unit::Choice,
        restart_needed: true,
        doc: "How much information is written to the WAL.",
    },
    PgParameter {
        name: "work_mem",
        kind: ParamKind::Memory {
            min_kb: 64,
            max_kb: 2 * 1024 * 1024,
        },
        unit: Unit::Kibibytes,
        restart_needed: false,
        doc: "Memory used by each sort or hash operation.",
    },
];

/// A validated parameter value, in its canonical internal unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PgValue {
    /// A whole number.
    Integer(i64),
    /// A PostgreSQL boolean.
    Boolean(bool),
    /// One of the allow-listed spellings.
    Choice(String),
    /// A size in kibibytes.
    Kibibytes(u64),
    /// A duration in milliseconds.
    Milliseconds(u64),
    /// Text.
    Text(String),
}

impl PgValue {
    /// The canonical text written by `ALTER SYSTEM`.
    #[must_use]
    pub fn canonical(&self) -> String {
        match self {
            Self::Integer(value) => value.to_string(),
            Self::Boolean(value) => {
                if *value {
                    "on".to_owned()
                } else {
                    "off".to_owned()
                }
            }
            Self::Choice(value) => value.clone(),
            Self::Kibibytes(value) => format!("{value}kB"),
            Self::Milliseconds(value) => format!("{value}ms"),
            Self::Text(value) => value.clone(),
        }
    }
}

/// How a parameter change was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyMethod {
    /// `pg_reload_conf()`: new sessions see the value, running ones may not.
    Reload,
    /// A full PostgreSQL restart.
    Restart,
}

impl ApplyMethod {
    /// A short, stable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Reload => "reload",
            Self::Restart => "restart",
        }
    }
}

/// What reading a parameter returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamReading {
    /// The allow-listed parameter.
    pub name: String,
    /// The value as PostgreSQL reports it.
    pub value: String,
    /// The unit shown by the console.
    pub unit: Unit,
    /// Whether changing this parameter needs a restart.
    pub restart_needed: bool,
    /// Whether PostgreSQL says the current value is not yet applied.
    pub pending_restart: bool,
}

/// What setting a parameter did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamSetReport {
    /// The allow-listed parameter.
    pub name: String,
    /// The canonical value written with `ALTER SYSTEM`.
    pub value: String,
    /// The unit shown by the console.
    pub unit: Unit,
    /// How the change was applied.
    pub method: ApplyMethod,
    /// Whether a restart is needed to apply the change.
    pub restart_needed: bool,
    /// A short, secret-free note for the console.
    pub note: String,
}

/// The PostgreSQL administrative seam. The real adapter runs `ALTER SYSTEM`, `pg_reload_conf()`
/// and the service restart; tests script it.
///
/// Every method is synchronous: the host agent dispatches one operation at a time and the
/// adapter owns whatever waiting the underlying tool needs.
pub trait PgAdminPort: Send + Sync {
    /// The current value of `name`, as PostgreSQL reports it, and whether a restart is pending.
    ///
    /// # Errors
    /// A backend error when the value cannot be read.
    fn read(&self, name: &str) -> RuntimeResult<(String, bool)>;

    /// Runs `ALTER SYSTEM SET name TO 'value'` (a parameterised call; the adapter builds no
    /// string and the value is already canonical).
    ///
    /// # Errors
    /// A backend error when the statement fails.
    fn alter_system(&self, name: &str, value: &str) -> RuntimeResult<()>;

    /// Reloads the configuration (`pg_reload_conf()`).
    ///
    /// # Errors
    /// A backend error when the reload fails.
    fn reload(&self) -> RuntimeResult<()>;

    /// Restarts the PostgreSQL service so a restart-only parameter takes effect.
    ///
    /// # Errors
    /// A backend error when the restart fails.
    fn restart(&self) -> RuntimeResult<()>;
}

/// Reads one allow-listed parameter.
///
/// # Errors
/// [`RuntimeError::UnknownParameter`] when `name` is not allow-listed, or the port's error.
pub fn read_parameter(pg: &dyn PgAdminPort, name: &str) -> RuntimeResult<ParamReading> {
    let parameter = PgParameter::find(name).ok_or_else(|| RuntimeError::UnknownParameter {
        name: name.to_owned(),
    })?;
    let (value, pending_restart) = pg.read(parameter.name)?;
    Ok(ParamReading {
        name: parameter.name.to_owned(),
        value,
        unit: parameter.unit,
        restart_needed: parameter.restart_needed,
        pending_restart,
    })
}

/// Validates `raw` against `parameter` and returns the value in its canonical internal unit.
///
/// # Errors
/// [`RuntimeError::ValueOutOfRange`] when the value is outside the allow-listed shape; the
/// message never repeats the refused value.
pub fn parse_value(parameter: &PgParameter, raw: &str) -> RuntimeResult<PgValue> {
    let trimmed = raw.trim();
    match &parameter.kind {
        ParamKind::Integer { min, max } => {
            let value: i64 = trimmed
                .parse()
                .map_err(|_| out_of_range(parameter.name, "must be a whole number"))?;
            if value < *min || value > *max {
                return Err(out_of_range(
                    parameter.name,
                    &format!("must be between {min} and {max}"),
                ));
            }
            Ok(PgValue::Integer(value))
        }
        ParamKind::Boolean => match trimmed.to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Ok(PgValue::Boolean(true)),
            "off" | "false" | "no" | "0" => Ok(PgValue::Boolean(false)),
            _ => Err(out_of_range(
                parameter.name,
                "must be on or off (true/false, yes/no, 1/0)",
            )),
        },
        ParamKind::Choice { choices } => {
            let lower = trimmed.to_ascii_lowercase();
            if choices.iter().any(|choice| *choice == lower) {
                Ok(PgValue::Choice(lower))
            } else {
                Err(out_of_range(
                    parameter.name,
                    &format!("must be one of {}", choices.join(", ")),
                ))
            }
        }
        ParamKind::Memory { min_kb, max_kb } => {
            let kb = parse_memory_kb(trimmed).ok_or_else(|| {
                out_of_range(parameter.name, "must be a size such as 64MB or 2GB")
            })?;
            if kb < *min_kb || kb > *max_kb {
                return Err(out_of_range(
                    parameter.name,
                    &format!("must be between {min_kb}kB and {max_kb}kB"),
                ));
            }
            Ok(PgValue::Kibibytes(kb))
        }
        ParamKind::Duration { min_ms, max_ms } => {
            let ms = parse_duration_ms(trimmed).ok_or_else(|| {
                out_of_range(
                    parameter.name,
                    "must be a duration such as 500ms, 30s or 5min",
                )
            })?;
            if ms < *min_ms || ms > *max_ms {
                return Err(out_of_range(
                    parameter.name,
                    &format!("must be between {min_ms}ms and {max_ms}ms"),
                ));
            }
            Ok(PgValue::Milliseconds(ms))
        }
        ParamKind::Text { max_len } => {
            if trimmed.len() > *max_len {
                return Err(out_of_range(
                    parameter.name,
                    &format!("must be at most {max_len} bytes"),
                ));
            }
            if trimmed.is_empty()
                || !trimmed.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'.' | b':' | b',' | b'*' | b'_' | b'-')
                })
            {
                return Err(out_of_range(
                    parameter.name,
                    "may contain only letters, digits and . : , * _ -",
                ));
            }
            Ok(PgValue::Text(trimmed.to_owned()))
        }
    }
}

/// Sets one allow-listed parameter: `ALTER SYSTEM`, then reload or restart, then report.
///
/// # Errors
/// [`RuntimeError::UnknownParameter`], [`RuntimeError::ValueOutOfRange`], or the port's
/// reload/restart error.
pub fn set_parameter(pg: &dyn PgAdminPort, name: &str, raw: &str) -> RuntimeResult<ParamSetReport> {
    let parameter = PgParameter::find(name).ok_or_else(|| RuntimeError::UnknownParameter {
        name: name.to_owned(),
    })?;
    let value = parse_value(parameter, raw)?;
    let canonical = value.canonical();
    pg.alter_system(parameter.name, &canonical)?;
    let method = if parameter.restart_needed {
        pg.restart()?;
        ApplyMethod::Restart
    } else {
        pg.reload()?;
        ApplyMethod::Reload
    };
    let note = match method {
        ApplyMethod::Reload => {
            "written with ALTER SYSTEM and applied with a configuration reload".to_owned()
        }
        ApplyMethod::Restart => {
            "written with ALTER SYSTEM and applied with a full PostgreSQL restart".to_owned()
        }
    };
    Ok(ParamSetReport {
        name: parameter.name.to_owned(),
        value: canonical,
        unit: parameter.unit,
        method,
        restart_needed: parameter.restart_needed,
        note,
    })
}

/// Parses a memory size into kibibytes.
fn parse_memory_kb(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    let (number, multiplier) = if let Some(rest) = lower.strip_suffix("kb") {
        (rest, 1_u64)
    } else if let Some(rest) = lower.strip_suffix("mb") {
        (rest, 1024_u64)
    } else if let Some(rest) = lower.strip_suffix("gb") {
        (rest, 1024_u64 * 1024)
    } else if let Some(rest) = lower.strip_suffix("tb") {
        (rest, 1024_u64 * 1024 * 1024)
    } else if let Some(rest) = lower.strip_suffix('b') {
        (rest, 0_u64)
    } else {
        (lower.as_str(), 1_u64)
    };
    let number: u64 = number.trim().parse().ok()?;
    if multiplier == 0 {
        // Bytes: whole kibibytes only, rounded down, so a value is never silently inflated.
        return Some(number / 1024);
    }
    number.checked_mul(multiplier)
}

/// Parses a duration into milliseconds.
fn parse_duration_ms(text: &str) -> Option<u64> {
    let lower = text.to_ascii_lowercase();
    let (number, multiplier) = if let Some(rest) = lower.strip_suffix("ms") {
        (rest, 1_u64)
    } else if let Some(rest) = lower.strip_suffix("min") {
        (rest, 60_000_u64)
    } else if let Some(rest) = lower.strip_suffix('s') {
        (rest, 1_000_u64)
    } else if let Some(rest) = lower.strip_suffix('h') {
        (rest, 3_600_000_u64)
    } else {
        // PostgreSQL treats a bare number as milliseconds for the duration GUCs.
        (lower.as_str(), 1_u64)
    };
    let number: u64 = number.trim().parse().ok()?;
    number.checked_mul(multiplier)
}

/// The out-of-range error for `name`.
fn out_of_range(name: &str, detail: &str) -> RuntimeError {
    RuntimeError::ValueOutOfRange {
        name: name.to_owned(),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use super::*;

    /// A scripted port that records every call.
    #[derive(Default)]
    struct FakePg {
        calls: Mutex<Vec<String>>,
        value: Mutex<String>,
        pending: Mutex<bool>,
    }

    impl FakePg {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PgAdminPort for FakePg {
        fn read(&self, _name: &str) -> RuntimeResult<(String, bool)> {
            Ok((
                self.value.lock().unwrap().clone(),
                *self.pending.lock().unwrap(),
            ))
        }

        fn alter_system(&self, name: &str, value: &str) -> RuntimeResult<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("alter {name}={value}"));
            *self.value.lock().unwrap() = value.to_owned();
            Ok(())
        }

        fn reload(&self) -> RuntimeResult<()> {
            self.calls.lock().unwrap().push("reload".to_owned());
            Ok(())
        }

        fn restart(&self) -> RuntimeResult<()> {
            self.calls.lock().unwrap().push("restart".to_owned());
            Ok(())
        }
    }

    #[test]
    fn the_allow_list_is_sorted_and_unique() {
        let mut names: Vec<&str> = PG_PARAMETERS.iter().map(|param| param.name).collect();
        let sorted = {
            let mut copy = names.clone();
            copy.sort_unstable();
            copy
        };
        assert_eq!(names, sorted);
        names.dedup();
        assert_eq!(names.len(), PG_PARAMETERS.len());
        assert!(PgParameter::find("not_a_parameter").is_none());
    }

    #[test]
    fn integers_are_bounded() {
        let parameter = PgParameter::find("max_connections").unwrap();
        assert_eq!(
            parse_value(parameter, "100").unwrap(),
            PgValue::Integer(100)
        );
        assert_eq!(
            parse_value(parameter, " 10 ").unwrap(),
            PgValue::Integer(10)
        );
        assert!(parse_value(parameter, "9").is_err());
        assert!(parse_value(parameter, "10001").is_err());
        assert!(parse_value(parameter, "many").is_err());
    }

    #[test]
    fn booleans_accept_the_documented_spellings() {
        let parameter = PgParameter::find("autovacuum").unwrap();
        for on in ["on", "true", "YES", "1"] {
            assert_eq!(parse_value(parameter, on).unwrap(), PgValue::Boolean(true));
        }
        for off in ["off", "FALSE", "no", "0"] {
            assert_eq!(
                parse_value(parameter, off).unwrap(),
                PgValue::Boolean(false)
            );
        }
        assert!(parse_value(parameter, "maybe").is_err());
    }

    #[test]
    fn choices_are_closed() {
        let parameter = PgParameter::find("wal_level").unwrap();
        assert_eq!(
            parse_value(parameter, "Logical").unwrap(),
            PgValue::Choice("logical".to_owned())
        );
        assert!(parse_value(parameter, "verbose").is_err());
    }

    #[test]
    fn memory_understands_units() {
        let parameter = PgParameter::find("work_mem").unwrap();
        assert_eq!(
            parse_value(parameter, "64MB").unwrap(),
            PgValue::Kibibytes(64 * 1024)
        );
        assert_eq!(
            parse_value(parameter, "1GB").unwrap(),
            PgValue::Kibibytes(1024 * 1024)
        );
        assert_eq!(
            parse_value(parameter, "128kB").unwrap(),
            PgValue::Kibibytes(128)
        );
        assert_eq!(
            parse_value(parameter, "1024").unwrap(),
            PgValue::Kibibytes(1024)
        );
        assert!(parse_value(parameter, "1kB").is_err());
        assert!(parse_value(parameter, "huge").is_err());
    }

    #[test]
    fn durations_understand_units() {
        let parameter = PgParameter::find("checkpoint_timeout").unwrap();
        assert_eq!(
            parse_value(parameter, "30s").unwrap(),
            PgValue::Milliseconds(30_000)
        );
        assert_eq!(
            parse_value(parameter, "5min").unwrap(),
            PgValue::Milliseconds(300_000)
        );
        assert_eq!(
            parse_value(parameter, "60000ms").unwrap(),
            PgValue::Milliseconds(60_000)
        );
        assert!(parse_value(parameter, "1s").is_err());
    }

    #[test]
    fn a_restart_parameter_restarts_and_a_reload_parameter_reloads() {
        let pg = FakePg::default();

        let reload = set_parameter(&pg, "work_mem", "64MB").unwrap();
        assert_eq!(reload.value, "65536kB");
        assert_eq!(reload.method, ApplyMethod::Reload);
        assert!(!reload.restart_needed);

        let restart = set_parameter(&pg, "shared_buffers", "256MB").unwrap();
        assert_eq!(restart.method, ApplyMethod::Restart);
        assert!(restart.restart_needed);

        assert_eq!(
            pg.calls(),
            vec![
                "alter work_mem=65536kB".to_owned(),
                "reload".to_owned(),
                "alter shared_buffers=262144kB".to_owned(),
                "restart".to_owned(),
            ]
        );
    }

    #[test]
    fn reading_reports_the_restart_hint() {
        let pg = FakePg::default();
        *pg.value.lock().unwrap() = "on".to_owned();
        *pg.pending.lock().unwrap() = true;
        let reading = read_parameter(&pg, "autovacuum").unwrap();
        assert_eq!(reading.value, "on");
        assert!(!reading.restart_needed);
        assert!(reading.pending_restart);
        assert_eq!(
            read_parameter(&pg, "nope").unwrap_err(),
            RuntimeError::UnknownParameter {
                name: "nope".to_owned()
            }
        );
    }
}
