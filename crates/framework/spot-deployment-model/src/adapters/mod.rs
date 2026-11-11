//! Adapters from the platform's authoritative crates to deployment facts.
//!
//! Each adapter is a pure read of one source, shaped as a [`FactSource`](crate::FactSource):
//!
//! - [`InstallerFacts`] — installed apps and the host's runtime version (`spot-installer`);
//! - [`SettingsFacts`] — the node profile's cache and coordination shape and the site policies
//!   (`spot-settings`, resolved values only);
//! - [`RuntimeFacts`] — topology, node roles, PostgreSQL version and replicas
//!   (`spot-agent-runtime`, ADM-06);
//! - [`FleetFacts`] — hub reachability and the last successful sync (`spot-fleet-health`);
//! - [`SeamsFacts`] — the step overrides the site applies (`spot-seams`);
//! - [`AgentFacts`] — the agent's own maximum tier (`spot-agent-tiers`).
//!
//! An adapter names the version of its source ([`FactSource::epoch`](crate::FactSource::epoch))
//! and never carries a credential: a profile, a role set and a version are deployment facts; a
//! connection string or a key is not.

mod agent;
mod fleet;
mod installer;
mod runtime;
mod seams;
mod settings;

pub use agent::AgentFacts;
pub use fleet::FleetFacts;
pub use installer::InstallerFacts;
pub use runtime::RuntimeFacts;
pub use seams::SeamsFacts;
pub use settings::SettingsFacts;
