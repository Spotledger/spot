//! `[services.process_conversations]`: the numbers, the contracts this site holds, the routes to
//! its peers and the sealed identity key reference. Nothing here is a secret: the configuration
//! names a secret by reference, it never holds one in clear.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use spot_bpm_conversations::WaitFor;
use spot_tenant_link::LinkId;

use crate::services::process_triggers::SealedSecret;

/// The longest a contract path or a name may be.
const MAX_TEXT: usize = 200;

/// `[services.process_conversations]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all.
    pub enabled: bool,
    /// Seconds between two passes of the driver (arrivals, timeouts, delivery waits), 1 to 300.
    pub tick_secs: u64,
    /// The most conversations one link may hold open (1 to 10000).
    pub max_open_conversations: usize,
    /// The most refused inputs one link may hold in the quarantine (1 to 100000).
    pub max_held_inputs: usize,
    /// The most messages one link may deliver per clock minute (1 to 1000000). Over it, the
    /// message is not taken (the sender keeps it and tries again), an audit row is written and
    /// the health check warns.
    pub max_messages_per_minute: u32,
    /// The largest payload, in bytes, this site takes from a link (1024 to 262144).
    pub max_payload_bytes: usize,
    /// How far a send must be delivered before its step continues when the node does not say:
    /// `queued`, `acknowledged` or `applied`.
    pub wait_for: String,
    /// The reference, in `secrets`, of the site's link identity key (a 32-byte seed).
    pub identity: Option<String>,
    /// The process contracts this site holds, by name.
    pub contracts: BTreeMap<String, ContractSetting>,
    /// Which link reaches which peer role of which contract.
    pub peers: Vec<PeerSetting>,
    /// Sealed secrets by reference, sealed under the site's `[services.secrets]` key.
    pub secrets: BTreeMap<String, SealedSecret>,
}

/// One process contract this site plays a role in.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractSetting {
    /// The contract file, relative to the configuration root: plain components only.
    pub path: String,
    /// The role this site plays in the contract.
    pub role: String,
}

/// The link that reaches one peer role of one contract.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerSetting {
    /// The contract id.
    pub contract: String,
    /// The role the peer plays.
    pub role: String,
    /// The link, as its id text.
    pub link: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_secs: 5,
            max_open_conversations: spot_bpm_conversations::MAX_OPEN_CONVERSATIONS,
            max_held_inputs: spot_bpm_conversations::MAX_HELD_INPUTS,
            max_messages_per_minute: 600,
            max_payload_bytes: 64 * 1024,
            wait_for: "queued".to_owned(),
            identity: None,
            contracts: BTreeMap::new(),
            peers: Vec::new(),
            secrets: BTreeMap::new(),
        }
    }
}

impl Settings {
    /// Checks every number and name.
    ///
    /// # Errors
    /// The key that is out of range, as `services.process_conversations.<key>: out of range`.
    pub fn validate(&self) -> Result<(), String> {
        let bad = |key: &str| {
            Err(format!(
                "services.process_conversations.{key}: out of range"
            ))
        };
        if !(1..=300).contains(&self.tick_secs) {
            return bad("tick_secs");
        }
        if !(1..=10_000).contains(&self.max_open_conversations) {
            return bad("max_open_conversations");
        }
        if !(1..=100_000).contains(&self.max_held_inputs) {
            return bad("max_held_inputs");
        }
        if !(1..=1_000_000).contains(&self.max_messages_per_minute) {
            return bad("max_messages_per_minute");
        }
        if !(1_024..=262_144).contains(&self.max_payload_bytes) {
            return bad("max_payload_bytes");
        }
        if self.wait_for().is_none() {
            return bad("wait_for");
        }
        if let Some(name) = &self.identity
            && !(token(name) && self.secrets.contains_key(name))
        {
            return bad("identity");
        }
        for (name, contract) in &self.contracts {
            if !token(name) || !token(&contract.role) || relative_path(&contract.path).is_none() {
                return bad("contracts");
            }
        }
        for peer in &self.peers {
            if !token(&peer.contract) || !token(&peer.role) || peer.link.parse::<LinkId>().is_err()
            {
                return bad("peers");
            }
        }
        if self.secrets.keys().any(|name| !token(name)) {
            return bad("secrets");
        }
        Ok(())
    }

    /// The default delivery requirement of a send step.
    #[must_use]
    pub fn wait_for(&self) -> Option<WaitFor> {
        match self.wait_for.as_str() {
            "queued" => Some(WaitFor::Queued),
            "acknowledged" => Some(WaitFor::Acknowledged),
            "applied" => Some(WaitFor::Applied),
            _ => None,
        }
    }
}

/// A name: 1 to 200 of `[A-Za-z0-9._-]`, never anything that could be a path, a header or an
/// expression.
pub(super) fn token(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_TEXT
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `path` as a relative path of plain components, or `None` for anything else: absolute, `..`,
/// a backslash, a drive, an empty component, a character outside `[A-Za-z0-9._-]`.
pub(super) fn relative_path(path: &str) -> Option<PathBuf> {
    // No empty segment either: `a//b` and a trailing `/` are not plain relative paths.
    if path.is_empty()
        || path.len() > MAX_TEXT
        || path.contains('\\')
        || path.split('/').any(str::is_empty)
    {
        return None;
    }
    let parsed = Path::new(path);
    let mut clean = PathBuf::new();
    for component in parsed.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str()?;
                if part == "." || part == ".." || !token(part) {
                    return None;
                }
                clean.push(part);
            }
            _ => return None,
        }
    }
    (!clean.as_os_str().is_empty()).then_some(clean)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_unknown_keys_are_refused() {
        assert!(Settings::default().validate().is_ok());
        assert!(toml::from_str::<Settings>("tick = 5").is_err());
        assert_eq!(Settings::default().wait_for(), Some(WaitFor::Queued));
    }

    #[test]
    fn numbers_outside_their_range_are_refused() {
        for text in [
            "tick_secs = 0",
            "tick_secs = 301",
            "max_open_conversations = 0",
            "max_held_inputs = 0",
            "max_messages_per_minute = 0",
            "max_payload_bytes = 10",
            "wait_for = \"sometime\"",
        ] {
            let settings: Settings = toml::from_str(text).expect("decodes");
            assert!(settings.validate().is_err(), "{text}");
        }
    }

    #[test]
    fn contract_paths_are_plain_relative_paths() {
        assert!(relative_path("contracts/request.toml").is_some());
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a\\b",
            "a//b/",
            "a b",
            "./",
        ] {
            assert!(relative_path(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn an_identity_must_name_a_secret_and_peers_a_link_id() {
        let settings: Settings = toml::from_str("identity = \"missing\"").expect("decodes");
        assert!(settings.validate().is_err());
        let settings: Settings = toml::from_str(
            "[contracts.c]\npath = \"c.toml\"\nrole = \"a\"\n\
             [[peers]]\ncontract = \"c\"\nrole = \"b\"\nlink = \"not-a-link\"\n",
        )
        .expect("decodes");
        assert!(settings.validate().is_err());
    }
}
