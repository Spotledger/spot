//! `[services.process_effects]`: the typed settings of the module.

use serde::Deserialize;

/// The default supervisor role names: who may start the undo of another person's request.
pub const DEFAULT_SUPERVISOR_ROLES: [&str; 2] = ["Process Supervisor", "System Manager"];

/// `[services.process_effects]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all.
    pub enabled: bool,
    /// How often the lease holder looks for undos to resume, seconds (5 to 3600).
    pub tick_secs: u64,
    /// How long a running undo may go untouched before another pass resumes it, seconds (30 to
    /// 3600).
    pub stale_secs: u64,
    /// How many interrupted undos one pass resumes at most (1 to 100).
    pub max_resume_per_tick: u32,
    /// The role names that may start, resume, decide or abandon the undo of another person's
    /// request. The person who started the request always may.
    pub supervisor_roles: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_secs: 30,
            stale_secs: 120,
            max_resume_per_tick: 10,
            supervisor_roles: DEFAULT_SUPERVISOR_ROLES
                .iter()
                .map(|role| (*role).to_owned())
                .collect(),
        }
    }
}

impl Settings {
    /// Checks every range; the error is the key that is wrong, never its value.
    ///
    /// # Errors
    /// The name of the offending key.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(5..=3600).contains(&self.tick_secs) {
            return Err("tick_secs");
        }
        if !(30..=3600).contains(&self.stale_secs) {
            return Err("stale_secs");
        }
        if !(1..=100).contains(&self.max_resume_per_tick) {
            return Err("max_resume_per_tick");
        }
        if self.supervisor_roles.is_empty()
            || self.supervisor_roles.iter().any(|r| r.trim().is_empty())
        {
            return Err("supervisor_roles");
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_unknown_keys_are_refused() {
        assert!(Settings::default().validate().is_ok());
        let bad: Result<Settings, _> = toml::from_str("tick = 5");
        assert!(bad.is_err());
    }

    #[test]
    fn ranges_are_enforced() {
        let s = Settings {
            tick_secs: 4,
            ..Settings::default()
        };
        assert_eq!(s.validate(), Err("tick_secs"));
        let s = Settings {
            stale_secs: 10,
            ..Settings::default()
        };
        assert_eq!(s.validate(), Err("stale_secs"));
        let s = Settings {
            supervisor_roles: vec![" ".to_owned()],
            ..Settings::default()
        };
        assert_eq!(s.validate(), Err("supervisor_roles"));
    }
}
