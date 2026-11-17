//! The privilege model (WP-ADM-03.T2).
//!
//! The agent runs as a dedicated **unprivileged** user. The few operations that need root
//! go through a small helper, `spot-agent-priv`, invoked as `sudo -n <helper> run <name>`
//! with the parameters on **stdin** (one JSON line), so the sudoers drop-in names each
//! template exactly and needs no wildcard. The helper accepts a fixed set of template
//! names with typed parameters ([`templates`]), validates them again itself (it never
//! trusts the agent's validation), builds argv from the template (no shell, no string
//! concatenation, no free-form argv), runs the child in its own process group with a
//! timeout and an output cap ([`exec`]) and refuses everything else.
//!
//! On Windows the same templates are served by a LocalSystem service over a local named
//! pipe whose ACL admits only the agent account ([`protocol`]; the service host itself is
//! WP-ADM-05.T6).
//!
//! # Privileged templates (documented per operation in [`templates::TEMPLATES`])
//!
//! `service.start|stop|restart|reload|enable|disable|mask|unmask`, `service.status`,
//! `user.lock|unlock`, `cert.install`. Anything else that needs privilege is refused with
//! `agent.privilege_refused`; adding a template is a reviewed code change.
//!
//! The sudoers drop-in and the Windows ACL fragments ship as files under `packaging/`
//! (WP-PRD-09.T7, WP-PRD-03.T4); this crate provides their generator
//! (`spot-agent-priv --print-sudoers`).

pub mod exec;
pub mod helper;
pub mod protocol;
pub mod runner;
pub mod sudoers;
pub mod templates;

pub use runner::PrivilegedRunner;
pub use templates::{TEMPLATES, Template};

/// The installed helper path the sudoers drop-in names.
pub const HELPER_PATH: &str = "/usr/libexec/spot-agent/spot-agent-priv";
