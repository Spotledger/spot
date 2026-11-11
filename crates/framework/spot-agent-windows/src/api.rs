//! The typed seam to Windows: [`WinApi`] and the data it moves.
//!
//! Every operation this crate performs reaches the operating system through exactly one
//! method of [`WinApi`]. The trait is safe and typed — no raw handle, no command line, no
//! string built from input — so the modules that use it compile and unit-test on Linux
//! against [`crate::fake::FakeWin`], and only [`crate::real`] (behind `#[cfg(windows)]`)
//! talks to the Service Control Manager, the Windows Update Agent, `INetFwPolicy2` and
//! NetAPI.
//!
//! # Timeouts and instants
//!
//! A service call takes the timeout in whole seconds and returns
//! [`WindowsError::ServiceTimeout`] rather than blocking forever; a scheduled reboot takes
//! an absolute Unix instant, so no method of this trait reads a clock. The module, not the
//! seam, owns the "time is an argument" rule.

use crate::error::WindowsError;
use crate::secrets::Secret;

/// The state of one service, as the Service Control Manager reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    /// Not running.
    Stopped,
    /// A start is in progress.
    StartPending,
    /// A stop is in progress.
    StopPending,
    /// Running.
    Running,
    /// A resume is in progress.
    ContinuePending,
    /// A pause is in progress.
    PausePending,
    /// Paused.
    Paused,
    /// A state this crate does not model.
    Unknown,
}

impl ServiceState {
    /// The lower-case name used in plans, checks and the console.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::StartPending => "start_pending",
            Self::StopPending => "stop_pending",
            Self::Running => "running",
            Self::ContinuePending => "continue_pending",
            Self::PausePending => "pause_pending",
            Self::Paused => "paused",
            Self::Unknown => "unknown",
        }
    }
}

/// How a service starts, as the Service Control Manager stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartType {
    /// Started by the boot loader.
    Boot,
    /// Started by the kernel during load.
    System,
    /// Started automatically at boot.
    Automatic,
    /// Started automatically, after the boot delay.
    AutomaticDelayed,
    /// Started on demand.
    Manual,
    /// Cannot be started.
    Disabled,
}

impl StartType {
    /// The lower-case name used in operation arguments and plans.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boot => "boot",
            Self::System => "system",
            Self::Automatic => "automatic",
            Self::AutomaticDelayed => "automatic_delayed",
            Self::Manual => "manual",
            Self::Disabled => "disabled",
        }
    }

    /// Parses the argument spelling of a start type.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "boot" => Self::Boot,
            "system" => Self::System,
            "automatic" => Self::Automatic,
            "automatic_delayed" => Self::AutomaticDelayed,
            "manual" => Self::Manual,
            "disabled" => Self::Disabled,
            _ => return None,
        })
    }
}

/// One step of a service's recovery policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Take no action.
    None,
    /// Restart the service.
    Restart,
    /// Reboot the machine.
    Reboot,
}

impl RecoveryAction {
    /// The lower-case name used in operation arguments and plans.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Restart => "restart",
            Self::Reboot => "reboot",
        }
    }

    /// Parses the argument spelling of a recovery action.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "none" => Self::None,
            "restart" => Self::Restart,
            "reboot" => Self::Reboot,
            _ => return None,
        })
    }
}

/// A service's recovery policy: the ordered actions, the delay before a restart, and the
/// period after which the failure counter resets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryConfig {
    /// The ordered recovery actions (Windows stores three).
    pub actions: Vec<RecoveryAction>,
    /// Seconds to wait before the restart action runs.
    pub restart_delay_seconds: u32,
    /// Seconds of running without failure before the failure counter resets.
    pub reset_period_seconds: u32,
}

/// One service, as the Service Control Manager describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceInfo {
    /// The service's key name (for example `wuauserv`).
    pub name: String,
    /// The human display name.
    pub display_name: String,
    /// The current state.
    pub state: ServiceState,
    /// How the service starts.
    pub start_type: StartType,
}

/// How serious a Windows Update is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A low-severity update.
    Low,
    /// A moderate-severity update.
    Moderate,
    /// An important update.
    Important,
    /// A critical update.
    Critical,
    /// The Windows Update Agent did not classify it.
    Unspecified,
}

impl Severity {
    /// The lower-case name used in operation arguments, plans and reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Moderate => "moderate",
            Self::Important => "important",
            Self::Critical => "critical",
            Self::Unspecified => "unspecified",
        }
    }

    /// Parses the argument spelling of a severity.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "low" => Self::Low,
            "moderate" => Self::Moderate,
            "important" => Self::Important,
            "critical" => Self::Critical,
            "unspecified" => Self::Unspecified,
            _ => return None,
        })
    }
}

/// One update the Windows Update Agent offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    /// The update's stable id (the WUA `Identity.UpdateID`).
    pub id: String,
    /// The Microsoft Knowledge Base number, when the update has one (for example
    /// `KB5034441`).
    pub kb: Option<String>,
    /// The update's title.
    pub title: String,
    /// The update's severity.
    pub severity: Severity,
    /// Whether installing it requires a reboot.
    pub reboot_required: bool,
    /// Whether the update has been downloaded and is ready to install.
    pub downloaded: bool,
    /// Whether the update is already installed.
    pub installed: bool,
}

/// The direction of a firewall rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Traffic into the machine.
    Inbound,
    /// Traffic out of the machine.
    Outbound,
}

impl Direction {
    /// The lower-case name used in operation arguments and plans.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        }
    }

    /// Parses the argument spelling of a direction.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "inbound" => Self::Inbound,
            "outbound" => Self::Outbound,
            _ => return None,
        })
    }
}

/// What a firewall rule does with matching traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleAction {
    /// Permit it.
    Allow,
    /// Refuse it.
    Block,
}

impl RuleAction {
    /// The lower-case name used in operation arguments and plans.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Block => "block",
        }
    }

    /// Parses the argument spelling of a rule action.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "allow" => Self::Allow,
            "block" => Self::Block,
            _ => return None,
        })
    }
}

/// The transport protocol of a firewall rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// TCP.
    Tcp,
    /// UDP.
    Udp,
    /// Any protocol.
    Any,
}

impl Protocol {
    /// The lower-case name used in operation arguments and plans.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Any => "any",
        }
    }

    /// Parses the argument spelling of a protocol.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "tcp" => Self::Tcp,
            "udp" => Self::Udp,
            "any" => Self::Any,
            _ => return None,
        })
    }
}

/// One of the three Windows Firewall profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Profile {
    /// The domain profile.
    Domain,
    /// The private profile.
    Private,
    /// The public profile.
    Public,
}

impl Profile {
    /// The lower-case name used in operation arguments, plans and reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Private => "private",
            Self::Public => "public",
        }
    }

    /// Parses the argument spelling of a profile.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "domain" => Self::Domain,
            "private" => Self::Private,
            "public" => Self::Public,
            _ => return None,
        })
    }

    /// Every profile, in the order the console lists them.
    #[must_use]
    pub fn all() -> [Profile; 3] {
        [Self::Domain, Self::Private, Self::Public]
    }
}

/// One Windows Firewall rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirewallRule {
    /// The rule's name (its key).
    pub name: String,
    /// Which way the traffic goes.
    pub direction: Direction,
    /// What the rule does.
    pub action: RuleAction,
    /// The protocol.
    pub protocol: Protocol,
    /// The local ports the rule matches; empty means "any port".
    pub local_ports: Vec<u16>,
    /// The profiles the rule applies to; empty means "all profiles".
    pub profiles: Vec<Profile>,
    /// Whether the rule is enabled.
    pub enabled: bool,
}

/// One local account, as NetAPI describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalUser {
    /// The account name.
    pub name: String,
    /// Whether the account may sign in.
    pub enabled: bool,
    /// The full name, when set.
    pub full_name: String,
    /// The comment, when set.
    pub comment: String,
    /// The local groups the account belongs to.
    pub groups: Vec<String>,
}

/// One local group, as NetAPI describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalGroup {
    /// The group name.
    pub name: String,
    /// The comment, when set.
    pub comment: String,
    /// The accounts in the group.
    pub members: Vec<String>,
}

/// The typed seam to Windows: services, Windows Update, the firewall and local
/// users/groups.
///
/// Implemented by [`crate::fake::FakeWin`] (tests, Linux) and [`crate::real`] (the
/// `windows` crate, `#[cfg(windows)]`). Every method is safe and returns a typed
/// [`WindowsError`] rather than panicking.
pub trait WinApi: Send + Sync {
    /// Every service the Service Control Manager knows, sorted by name.
    ///
    /// # Errors
    /// [`WindowsError::Api`] when the enumeration fails.
    fn list_services(&self) -> Result<Vec<ServiceInfo>, WindowsError>;

    /// One service by name.
    ///
    /// # Errors
    /// [`WindowsError::UnknownService`] when no such service exists;
    /// [`WindowsError::Api`] on a control failure.
    fn service_status(&self, name: &str) -> Result<ServiceInfo, WindowsError>;

    /// Starts a service and waits up to `timeout_seconds` for it to run.
    ///
    /// # Errors
    /// [`WindowsError::UnknownService`], [`WindowsError::ServiceTimeout`],
    /// [`WindowsError::Api`].
    fn start_service(&self, name: &str, timeout_seconds: u64) -> Result<(), WindowsError>;

    /// Stops a service and waits up to `timeout_seconds` for it to stop.
    ///
    /// # Errors
    /// [`WindowsError::UnknownService`], [`WindowsError::ServiceTimeout`],
    /// [`WindowsError::Api`].
    fn stop_service(&self, name: &str, timeout_seconds: u64) -> Result<(), WindowsError>;

    /// Sets a service's start type.
    ///
    /// # Errors
    /// [`WindowsError::UnknownService`], [`WindowsError::Api`].
    fn set_service_start_type(&self, name: &str, start_type: StartType)
    -> Result<(), WindowsError>;

    /// A service's current recovery policy.
    ///
    /// # Errors
    /// [`WindowsError::UnknownService`], [`WindowsError::Api`].
    fn service_recovery(&self, name: &str) -> Result<RecoveryConfig, WindowsError>;

    /// Sets a service's recovery policy.
    ///
    /// # Errors
    /// [`WindowsError::UnknownService`], [`WindowsError::Api`].
    fn set_service_recovery(
        &self,
        name: &str,
        recovery: &RecoveryConfig,
    ) -> Result<(), WindowsError>;

    /// Every update the Windows Update Agent offers, including installed ones.
    ///
    /// # Errors
    /// [`WindowsError::Api`] when the search fails; [`WindowsError::Unsupported`] when no
    /// Windows Update Agent is reachable.
    fn search_updates(&self, include_hidden: bool) -> Result<Vec<UpdateInfo>, WindowsError>;

    /// Downloads one update so it is ready to install.
    ///
    /// # Errors
    /// [`WindowsError::UnknownUpdate`], [`WindowsError::Api`].
    fn download_update(&self, id: &str) -> Result<(), WindowsError>;

    /// Installs one downloaded update.
    ///
    /// # Errors
    /// [`WindowsError::UnknownUpdate`], [`WindowsError::Api`].
    fn install_update(&self, id: &str) -> Result<(), WindowsError>;

    /// Whether the machine is waiting for a reboot to finish an install.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    fn reboot_required(&self) -> Result<bool, WindowsError>;

    /// Schedules a reboot at an absolute Unix instant; never reboots immediately.
    ///
    /// # Errors
    /// [`WindowsError::RebootTimeInvalid`] when `at_unix_seconds` is not a future second;
    /// [`WindowsError::Api`].
    fn schedule_reboot(&self, at_unix_seconds: i64) -> Result<(), WindowsError>;

    /// The instant a reboot is scheduled for, when one is pending.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    fn scheduled_reboot(&self) -> Result<Option<i64>, WindowsError>;

    /// Every firewall rule, sorted by name.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    fn list_firewall_rules(&self) -> Result<Vec<FirewallRule>, WindowsError>;

    /// Adds a rule, or updates the rule of the same name.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    fn add_firewall_rule(&self, rule: &FirewallRule) -> Result<(), WindowsError>;

    /// Removes a rule by name.
    ///
    /// # Errors
    /// [`WindowsError::UnknownRule`], [`WindowsError::Api`].
    fn remove_firewall_rule(&self, name: &str) -> Result<(), WindowsError>;

    /// Enables or disables one firewall profile.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    fn set_profile_enabled(&self, profile: Profile, enabled: bool) -> Result<(), WindowsError>;

    /// Whether one firewall profile is enabled.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    fn profile_enabled(&self, profile: Profile) -> Result<bool, WindowsError>;

    /// Every local user, sorted by name.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    fn list_local_users(&self) -> Result<Vec<LocalUser>, WindowsError>;

    /// One local user by name.
    ///
    /// # Errors
    /// [`WindowsError::UnknownUser`], [`WindowsError::Api`].
    fn local_user(&self, name: &str) -> Result<LocalUser, WindowsError>;

    /// Creates a local user with `password`.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    fn create_local_user(
        &self,
        name: &str,
        password: &Secret,
        full_name: &str,
        comment: &str,
    ) -> Result<(), WindowsError>;

    /// Enables or disables a local user.
    ///
    /// # Errors
    /// [`WindowsError::UnknownUser`], [`WindowsError::Api`].
    fn set_user_enabled(&self, name: &str, enabled: bool) -> Result<(), WindowsError>;

    /// Sets a local user's password.
    ///
    /// # Errors
    /// [`WindowsError::UnknownUser`], [`WindowsError::Api`].
    fn set_user_password(&self, name: &str, password: &Secret) -> Result<(), WindowsError>;

    /// Replaces a local user's group membership with exactly `groups`.
    ///
    /// # Errors
    /// [`WindowsError::UnknownUser`], [`WindowsError::UnknownGroup`],
    /// [`WindowsError::Api`].
    fn set_user_groups(&self, name: &str, groups: &[String]) -> Result<(), WindowsError>;

    /// Every local group, sorted by name.
    ///
    /// # Errors
    /// [`WindowsError::Api`].
    fn list_local_groups(&self) -> Result<Vec<LocalGroup>, WindowsError>;

    /// Adds a user to a local group (idempotent).
    ///
    /// # Errors
    /// [`WindowsError::UnknownGroup`], [`WindowsError::UnknownUser`],
    /// [`WindowsError::Api`].
    fn add_group_member(&self, group: &str, user: &str) -> Result<(), WindowsError>;

    /// Removes a user from a local group (idempotent).
    ///
    /// # Errors
    /// [`WindowsError::UnknownGroup`], [`WindowsError::UnknownUser`],
    /// [`WindowsError::Api`].
    fn remove_group_member(&self, group: &str, user: &str) -> Result<(), WindowsError>;
}
