//! The Windows Firewall binding for [`RealWin`](super::RealWin): the `HNetCfg.FwPolicy2`
//! COM object's `INetFwPolicy2` interface.

use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::NetworkManagement::WindowsFirewall::{
    INetFwPolicy2, INetFwRule, NET_FW_ACTION_ALLOW, NET_FW_ACTION_BLOCK, NET_FW_IP_PROTOCOL_ANY,
    NET_FW_IP_PROTOCOL_TCP, NET_FW_IP_PROTOCOL_UDP, NET_FW_PROFILE_TYPE2, NET_FW_PROFILE2_DOMAIN,
    NET_FW_PROFILE2_PRIVATE, NET_FW_PROFILE2_PUBLIC, NET_FW_RULE_DIR_IN, NET_FW_RULE_DIR_OUT,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Ole::IEnumVARIANT;
use windows::Win32::System::Variant::{VARIANT, VariantClear};
use windows::core::{BSTR, GUID, Interface, w};

use super::variant::{self, Value};
use super::{RealWin, api, prog_id, with_com};
use crate::api::{Direction, FirewallRule, Profile, Protocol, RuleAction};
use crate::error::WindowsError;

impl RealWin {
    pub(super) fn list_firewall_rules(&self) -> Result<Vec<FirewallRule>, WindowsError> {
        with_com(|| {
            let policy = firewall_policy()?;
            // SAFETY: the policy COM object owns the rules collection for the call, and the
            // enumerator returned by `_NewEnum` is a live `IEnumVARIANT` over it.
            let rules = unsafe { api(policy.Rules())? };
            let enumerator: IEnumVARIANT =
                unsafe { api(rules._NewEnum())? }
                    .cast()
                    .map_err(|error| WindowsError::Api {
                        detail: format!("HRESULT {:#010x}", error.code().0 as u32),
                    })?;
            let mut out = Vec::new();
            loop {
                let mut slot = [VARIANT::default()];
                let mut fetched = 0u32;
                // SAFETY: `slot` holds one initialised (empty) variant and `fetched` is a
                // valid out-pointer; a filled slot owns one object reference, cleared below.
                let status = unsafe { enumerator.Next(&mut slot, &raw mut fetched) };
                if status.is_err() {
                    return Err(WindowsError::Api {
                        detail: format!("HRESULT {:#010x}", status.0 as u32),
                    });
                }
                if fetched == 0 {
                    break;
                }
                let value = variant::read(&slot[0]);
                // SAFETY: the variant was filled by `Next` and is cleared exactly once.
                unsafe {
                    let _ = VariantClear(&mut slot[0]);
                }
                if let Value::Object(object) = value {
                    let rule: INetFwRule = object.cast().map_err(|error| WindowsError::Api {
                        detail: format!("HRESULT {:#010x}", error.code().0 as u32),
                    })?;
                    out.push(read_rule(&rule)?);
                }
            }
            Ok(out)
        })
    }

    pub(super) fn add_firewall_rule(&self, rule: &FirewallRule) -> Result<(), WindowsError> {
        with_com(|| {
            let policy = firewall_policy()?;
            // SAFETY: the rule is created, filled and added on this thread.
            unsafe {
                let created: INetFwRule =
                    api(CoCreateInstance(&firewall_rule_clsid()?, None, CLSCTX_ALL))?;
                write_rule(&created, rule)?;
                let rules = api(policy.Rules())?;
                api(rules.Add(&created))?;
            }
            Ok(())
        })
    }

    pub(super) fn remove_firewall_rule(&self, name: &str) -> Result<(), WindowsError> {
        with_com(|| {
            let policy = firewall_policy()?;
            // SAFETY: the policy COM object owns the rules collection for the call.
            let rules = unsafe { api(policy.Rules())? };
            match unsafe { api(rules.Remove(&BSTR::from(name))) } {
                Ok(()) => Ok(()),
                Err(_) => Err(WindowsError::UnknownRule {
                    name: name.to_owned(),
                }),
            }
        })
    }

    pub(super) fn set_profile_enabled(
        &self,
        profile: Profile,
        enabled: bool,
    ) -> Result<(), WindowsError> {
        with_com(|| {
            let policy = firewall_policy()?;
            // SAFETY: the policy COM object is used on this thread only.
            unsafe {
                api(policy.put_FirewallEnabled(profile_code(profile), VARIANT_BOOL::from(enabled)))
            }
        })
    }

    pub(super) fn profile_enabled(&self, profile: Profile) -> Result<bool, WindowsError> {
        with_com(|| {
            let policy = firewall_policy()?;
            // SAFETY: the policy COM object is used on this thread only.
            let value = unsafe { api(policy.get_FirewallEnabled(profile_code(profile)))? };
            Ok(value.as_bool())
        })
    }
}

/// The `HNetCfg.FwPolicy2` policy object.
fn firewall_policy() -> Result<INetFwPolicy2, WindowsError> {
    let clsid = prog_id(w!("HNetCfg.FwPolicy2"))?;
    // SAFETY: the CLSID came from the registry; CoCreateInstance returns a live interface.
    unsafe { api(CoCreateInstance(&clsid, None, CLSCTX_ALL)) }
}

/// The CLSID of the firewall rule coclass.
fn firewall_rule_clsid() -> Result<GUID, WindowsError> {
    prog_id(w!("HNetCfg.FwRule"))
}

/// Reads one `INetFwRule` into a [`FirewallRule`].
fn read_rule(rule: &INetFwRule) -> Result<FirewallRule, WindowsError> {
    // SAFETY: every getter is a COM call on a live rule object.
    unsafe {
        let name = api(rule.Name())?.to_string();
        let direction = match api(rule.Direction())? {
            NET_FW_RULE_DIR_OUT => Direction::Outbound,
            _ => Direction::Inbound,
        };
        let action = match api(rule.Action())? {
            NET_FW_ACTION_BLOCK => RuleAction::Block,
            _ => RuleAction::Allow,
        };
        let protocol = match api(rule.Protocol())? {
            code if code == NET_FW_IP_PROTOCOL_TCP.0 => Protocol::Tcp,
            code if code == NET_FW_IP_PROTOCOL_UDP.0 => Protocol::Udp,
            _ => Protocol::Any,
        };
        let ports = match api(rule.LocalPorts())?.to_string() {
            text if text.is_empty() || text == "*" => Vec::new(),
            text => text
                .split(',')
                .filter_map(|p| p.trim().parse().ok())
                .collect(),
        };
        let profiles = read_profiles(api(rule.Profiles())?);
        let enabled = api(rule.Enabled())?.as_bool();
        Ok(FirewallRule {
            name,
            direction,
            action,
            protocol,
            local_ports: ports,
            profiles,
            enabled,
        })
    }
}

/// Fills an `INetFwRule` from a [`FirewallRule`].
fn write_rule(rule: &INetFwRule, source: &FirewallRule) -> Result<(), WindowsError> {
    // SAFETY: every setter is a COM call on a live rule object.
    unsafe {
        api(rule.SetName(&BSTR::from(source.name.as_str())))?;
        api(rule.SetDirection(match source.direction {
            Direction::Outbound => NET_FW_RULE_DIR_OUT,
            Direction::Inbound => NET_FW_RULE_DIR_IN,
        }))?;
        api(rule.SetAction(match source.action {
            RuleAction::Allow => NET_FW_ACTION_ALLOW,
            RuleAction::Block => NET_FW_ACTION_BLOCK,
        }))?;
        api(rule.SetProtocol(match source.protocol {
            Protocol::Tcp => NET_FW_IP_PROTOCOL_TCP.0,
            Protocol::Udp => NET_FW_IP_PROTOCOL_UDP.0,
            Protocol::Any => NET_FW_IP_PROTOCOL_ANY.0,
        }))?;
        let ports = source
            .local_ports
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        // Windows refuses a port list on a rule whose protocol is not TCP or UDP, so an
        // empty list (every port) is simply not set.
        if !ports.is_empty() {
            api(rule.SetLocalPorts(&BSTR::from(ports.as_str())))?;
        }
        api(rule.SetProfiles(profile_mask(&source.profiles)))?;
        api(rule.SetEnabled(VARIANT_BOOL::from(source.enabled)))?;
        Ok(())
    }
}

/// The `NET_FW_PROFILE_TYPE2` code for a [`Profile`].
fn profile_code(profile: Profile) -> NET_FW_PROFILE_TYPE2 {
    match profile {
        Profile::Domain => NET_FW_PROFILE2_DOMAIN,
        Profile::Private => NET_FW_PROFILE2_PRIVATE,
        Profile::Public => NET_FW_PROFILE2_PUBLIC,
    }
}

/// The profile bitmask for a set of profiles.
fn profile_mask(profiles: &[Profile]) -> i32 {
    if profiles.is_empty() {
        return NET_FW_PROFILE2_DOMAIN.0 | NET_FW_PROFILE2_PRIVATE.0 | NET_FW_PROFILE2_PUBLIC.0;
    }
    profiles
        .iter()
        .map(|profile| profile_code(*profile).0)
        .fold(0, |acc, bit| acc | bit)
}

/// The profiles named by a profile bitmask.
fn read_profiles(mask: i32) -> Vec<Profile> {
    let mut out = Vec::new();
    for profile in Profile::all() {
        if mask & profile_code(profile).0 != 0 {
            out.push(profile);
        }
    }
    out
}
