#![allow(clippy::unwrap_used)]
//! The address path (`Guard::authorize_addr`, for SMTP and other non-HTTP callers) decides like
//! the URL path: the tenant allow-list, the platform deny list, the denial record, the pin.

use std::net::SocketAddr;
use std::sync::Arc;

use spot_egress::{
    DenialSink, EgressError, EgressPolicy, Guard, InMemorySink, Purpose, TestResolver,
};

struct Rig {
    guard: Guard,
    sink: Arc<InMemorySink>,
}

fn rig(policy_toml: &str, hosts: &[(&str, &[&str])]) -> Rig {
    let resolver = Arc::new(TestResolver::new());
    for (host, ips) in hosts {
        let ips: Vec<std::net::IpAddr> = ips.iter().map(|ip| ip.parse().unwrap()).collect();
        resolver.add_response(host, vec![ips; 16]);
    }
    let sink = Arc::new(InMemorySink::new());
    let guard = Guard::new(
        EgressPolicy::from_toml(policy_toml).unwrap(),
        resolver,
        sink.clone(),
        Arc::new(spot_core::SystemClock::new()),
    );
    Rig { guard, sink }
}

const RELAY: &str = r#"
[tenants.a]
[[tenants.a.allow]]
host = "relay.corp.test"
ports = [587]
purpose = "mail"
allow_private = ["10.20.0.5"]

[tenants.b]
[[tenants.b.allow]]
host = "other.test"
"#;

fn hosts() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        ("relay.corp.test", &["10.20.0.5"][..]),
        ("meta.corp.test", &["169.254.169.254"][..]),
        ("mixed.test", &["93.184.216.34", "10.20.0.5"][..]),
        ("pub.test", &["93.184.216.34"][..]),
        ("other.test", &["93.184.216.35"][..]),
    ]
}

#[tokio::test]
async fn an_allowed_internal_host_is_reachable_for_its_tenant_only() {
    let r = rig(RELAY, &hosts());
    let ok = r
        .guard
        .authorize_addr("relay.corp.test", 587, Purpose::Mail, "a", None)
        .await
        .unwrap();
    assert_eq!(
        ok.socket_addrs(),
        ["10.20.0.5:587".parse::<SocketAddr>().unwrap()]
    );
    assert_eq!(ok.host(), "relay.corp.test");

    let b = r
        .guard
        .authorize_addr("relay.corp.test", 587, Purpose::Mail, "b", None)
        .await;
    assert!(matches!(b, Err(EgressError::PolicyDenied)), "{b:?}");
    let nobody = r
        .guard
        .authorize_addr("relay.corp.test", 587, Purpose::Mail, "nobody", None)
        .await;
    assert!(
        matches!(nobody, Err(EgressError::AddressDenied { .. })),
        "a tenant with no entry gets the default policy: {nobody:?}"
    );
    assert!(r.sink.records().iter().all(|d| d.tenant != "a"));
}

#[tokio::test]
async fn an_entry_covers_its_purpose_host_and_port_only() {
    let r = rig(RELAY, &hosts());
    let webhook = r
        .guard
        .authorize_addr("relay.corp.test", 587, Purpose::Webhook, "a", None)
        .await;
    assert!(
        matches!(webhook, Err(EgressError::PolicyDenied)),
        "{webhook:?}"
    );
    let other_port = r
        .guard
        .authorize_addr("relay.corp.test", 465, Purpose::Mail, "a", None)
        .await;
    assert!(
        matches!(other_port, Err(EgressError::PolicyDenied)),
        "{other_port:?}"
    );
    // Another name that resolves to the same private address is not opened by the entry.
    let r2 = rig(RELAY, &[("sneaky.test", &["10.20.0.5"][..])]);
    let sneaky = r2
        .guard
        .authorize_addr("sneaky.test", 587, Purpose::Mail, "a", None)
        .await;
    assert!(sneaky.is_err());
}

#[tokio::test]
async fn metadata_is_refused_even_when_the_tenant_lists_it() {
    let policy = r#"
[tenants.a]
[[tenants.a.allow]]
host = "meta.corp.test"
allow_private = ["169.254.169.254", "169.254.0.0/16", "::ffff:169.254.169.254"]
"#;
    let r = rig(policy, &hosts());
    let denied = r
        .guard
        .authorize_addr("meta.corp.test", 80, Purpose::Mail, "a", None)
        .await;
    assert!(
        matches!(denied, Err(EgressError::AddressDenied { .. })),
        "{denied:?}"
    );
    // The same rule holds on the URL path.
    let url = r
        .guard
        .authorize(&spot_egress::Request {
            url: "https://meta.corp.test/".parse().unwrap(),
            purpose: Purpose::Webhook,
            tenant: "a".into(),
            actor: None,
        })
        .await;
    assert!(
        matches!(url, Err(EgressError::AddressDenied { .. })),
        "{url:?}"
    );
}

#[tokio::test]
async fn a_host_with_one_denied_answer_is_refused_as_a_whole() {
    let policy = r#"
[tenants.a]
[[tenants.a.allow]]
host = "mixed.test"
"#;
    let r = rig(policy, &hosts());
    let denied = r
        .guard
        .authorize_addr("mixed.test", 587, Purpose::Mail, "a", None)
        .await;
    assert!(
        matches!(denied, Err(EgressError::AddressDenied { .. })),
        "{denied:?}"
    );
}

#[tokio::test]
async fn a_denial_names_the_tenant_actor_and_purpose() {
    let r = rig(RELAY, &hosts());
    let _ = r
        .guard
        .authorize_addr(
            "relay.corp.test",
            587,
            Purpose::Mail,
            "b",
            Some("mail_queue:billing".into()),
        )
        .await;
    let _ = r
        .guard
        .authorize_addr("relay.corp.test", 587, Purpose::Mail, "nobody", None)
        .await;
    let records = r.sink.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].tenant, "b");
    assert_eq!(records[0].purpose, Purpose::Mail);
    assert_eq!(records[0].actor.as_deref(), Some("mail_queue:billing"));
    assert_eq!(records[0].rule, "policy");
    assert_eq!(records[1].tenant, "nobody");
    assert_eq!(records[1].rule, "address");
    assert_eq!(records[1].port, 587);
}

#[tokio::test]
async fn socket_addrs_carry_the_requested_port() {
    let policy = "[tenants.a]\n[[tenants.a.allow]]\nhost = \"pub.test\"\n";
    let r = rig(policy, &hosts());
    let ok = r
        .guard
        .authorize_addr("pub.test", 2525, Purpose::Mail, "a", None)
        .await
        .unwrap();
    assert_eq!(
        ok.socket_addrs(),
        ["93.184.216.34:2525".parse::<SocketAddr>().unwrap()]
    );
}

#[tokio::test]
async fn a_dangerous_port_opens_only_through_an_entry_that_lists_it() {
    let policy = r#"
[tenants.a]
[[tenants.a.allow]]
host = "relay.corp.test"
purpose = "mail"
ports = [25]
allow_private = ["10.20.0.5"]

[[tenants.a.allow]]
host = "pub.test"
purpose = "mail"
"#;
    let r = rig(policy, &hosts());
    assert!(
        r.guard
            .authorize_addr("relay.corp.test", 25, Purpose::Mail, "a", None)
            .await
            .is_ok()
    );
    let unlisted = r
        .guard
        .authorize_addr("pub.test", 25, Purpose::Mail, "a", None)
        .await;
    assert!(
        matches!(unlisted, Err(EgressError::PortDenied { port: 25 })),
        "{unlisted:?}"
    );
}

#[tokio::test]
async fn the_deny_list_wins_on_the_address_path_too() {
    let policy = r#"
[tenants.a]
[[tenants.a.allow]]
host = "pub.test"
[[tenants.a.deny]]
host = "pub.test"
"#;
    let r = rig(policy, &hosts());
    let denied = r
        .guard
        .authorize_addr("pub.test", 587, Purpose::Mail, "a", None)
        .await;
    assert!(
        matches!(denied, Err(EgressError::PolicyDenied)),
        "{denied:?}"
    );
}

#[tokio::test]
async fn a_tenant_with_no_entry_may_reach_public_hosts_and_nothing_internal() {
    let r = rig("", &hosts());
    assert!(
        r.guard
            .authorize_addr("pub.test", 587, Purpose::Mail, "ghost", None)
            .await
            .is_ok()
    );
    assert!(
        r.guard
            .authorize_addr("relay.corp.test", 587, Purpose::Mail, "ghost", None)
            .await
            .is_err()
    );
    assert!(
        r.guard
            .authorize_addr("pub.test", 25, Purpose::Mail, "ghost", None)
            .await
            .is_err()
    );
}

fn class_of(a: &str) -> spot_egress::AddrClass {
    spot_egress::classify(a.parse().unwrap())
}

#[test]
fn every_transition_form_is_judged_by_the_ipv4_it_embeds() {
    use spot_egress::AddrClass as C;
    // (embedded IPv4, its class, mapped, compatible, 6to4, NAT64)
    let table = [
        (
            "127.0.0.1",
            C::Loopback,
            "::ffff:7f00:1",
            "::7f00:1",
            "2002:7f00:1::",
            "64:ff9b::7f00:1",
        ),
        (
            "10.0.0.1",
            C::Private,
            "::ffff:a00:1",
            "::a00:1",
            "2002:a00:1::",
            "64:ff9b::a00:1",
        ),
        (
            "172.16.0.1",
            C::Private,
            "::ffff:ac10:1",
            "::ac10:1",
            "2002:ac10:1::",
            "64:ff9b::ac10:1",
        ),
        (
            "192.168.1.1",
            C::Private,
            "::ffff:c0a8:101",
            "::c0a8:101",
            "2002:c0a8:101::",
            "64:ff9b::c0a8:101",
        ),
        (
            "169.254.169.254",
            C::CloudMetadata,
            "::ffff:a9fe:a9fe",
            "::a9fe:a9fe",
            "2002:a9fe:a9fe::",
            "64:ff9b::a9fe:a9fe",
        ),
        (
            "169.254.170.2",
            C::CloudMetadata,
            "::ffff:a9fe:aa02",
            "::a9fe:aa02",
            "2002:a9fe:aa02::",
            "64:ff9b::a9fe:aa02",
        ),
        (
            "100.100.100.200",
            C::CloudMetadata,
            "::ffff:6464:64c8",
            "::6464:64c8",
            "2002:6464:64c8::",
            "64:ff9b::6464:64c8",
        ),
        (
            "168.63.129.16",
            C::CloudMetadata,
            "::ffff:a83f:8110",
            "::a83f:8110",
            "2002:a83f:8110::",
            "64:ff9b::a83f:8110",
        ),
        (
            "192.0.0.192",
            C::CloudMetadata,
            "::ffff:c000:c0",
            "::c000:c0",
            "2002:c000:c0::",
            "64:ff9b::c000:c0",
        ),
    ];
    for (v4, class, mapped, compat, sixtofour, nat64) in table {
        assert_eq!(class_of(v4), class, "{v4}");
        for form in [mapped, compat, sixtofour, nat64] {
            assert_eq!(class_of(form), class, "{form} embeds {v4}");
        }
    }
    // A public IPv4 behind a transition form is still never plain public.
    for form in [
        "::ffff:808:808",
        "::808:808",
        "2002:808:808::",
        "64:ff9b::808:808",
    ] {
        assert!(class_of(form).is_denied_by_default(), "{form}");
    }
}

fn policy_allow(entries: &str) -> EgressPolicy {
    EgressPolicy::from_toml(&format!(
        "[tenants.t]\n[[tenants.t.allow]]\nhost = \"h.test\"\nallow_private = {entries}\n"
    ))
    .unwrap()
}

fn allowed(p: &EgressPolicy, ip: &str) -> bool {
    let addr: std::net::IpAddr = ip.parse().unwrap();
    p.is_address_allowed_for(
        "t",
        Purpose::Mail,
        "tcp",
        "h.test",
        addr,
        587,
        spot_egress::classify(addr),
    )
}

#[test]
fn allow_private_matches_only_the_literal_address_it_names() {
    // An IPv6 entry never opens the IPv4 address it embeds, in any form.
    let v6_entries = [
        r#"["::ffff:10.20.0.5"]"#,
        r#"["::10.20.0.5"]"#,
        r#"["2002:a14:5::"]"#,
        r#"["64:ff9b::a14:5"]"#,
    ];
    for entry in v6_entries {
        let p = policy_allow(entry);
        assert!(!allowed(&p, "10.20.0.5"), "{entry} must not open 10.20.0.5");
    }
    // An IPv4 entry never opens a transition form that embeds it.
    let p = policy_allow(r#"["10.20.0.5"]"#);
    assert!(allowed(&p, "10.20.0.5"));
    for form in ["::ffff:a14:5", "::a14:5", "2002:a14:5::", "64:ff9b::a14:5"] {
        assert!(!allowed(&p, form), "10.20.0.5 entry must not open {form}");
    }
    // The literal still opens exactly itself, and not a sibling form.
    let p = policy_allow(r#"["::ffff:10.20.0.5"]"#);
    assert!(allowed(&p, "::ffff:10.20.0.5"));
    assert!(!allowed(&p, "::ffff:10.20.0.6"));
    assert!(!allowed(&p, "64:ff9b::a14:5"));
}

#[test]
fn metadata_cannot_be_opened_in_any_encoding() {
    let entries = r#"["169.254.169.254", "169.254.0.0/16", "::ffff:169.254.169.254", "::a9fe:a9fe", "2002:a9fe:a9fe::", "64:ff9b::a9fe:a9fe", "::/0", "0.0.0.0/0", "fd00:ec2::254"]"#;
    let p = policy_allow(entries);
    for a in [
        "169.254.169.254",
        "::ffff:a9fe:a9fe",
        "::a9fe:a9fe",
        "2002:a9fe:a9fe::",
        "64:ff9b::a9fe:a9fe",
        "168.63.129.16",
        "::ffff:a83f:8110",
        "fd00:ec2::254",
    ] {
        assert!(!allowed(&p, a), "{a}");
    }
}

#[tokio::test]
async fn a_name_that_fails_to_resolve_leaves_one_resolution_record() {
    let r = rig(RELAY, &[]);
    let by_addr = r
        .guard
        .authorize_addr("relay.corp.test", 587, Purpose::Mail, "a", None)
        .await;
    assert!(
        matches!(by_addr, Err(EgressError::ResolutionFailed)),
        "{by_addr:?}"
    );
    let records = r.sink.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].rule, "resolution");
    assert_eq!(records[0].tenant, "a");
    assert!(records[0].resolved.is_empty());

    r.sink.clear();
    let by_url = r
        .guard
        .authorize(&spot_egress::Request {
            url: "https://relay.corp.test:587/".parse().unwrap(),
            purpose: Purpose::Mail,
            tenant: "a".into(),
            actor: None,
        })
        .await;
    assert!(
        matches!(by_url, Err(EgressError::ResolutionFailed)),
        "{by_url:?}"
    );
    let records = r.sink.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].rule, "resolution");
}

#[tokio::test]
async fn an_empty_answer_leaves_one_resolution_record() {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response("relay.corp.test", vec![vec![]; 4]);
    let sink = Arc::new(InMemorySink::new());
    let guard = Guard::new(
        EgressPolicy::from_toml(RELAY).unwrap(),
        resolver,
        sink.clone(),
        Arc::new(spot_core::SystemClock::new()),
    );
    let by_addr = guard
        .authorize_addr("relay.corp.test", 587, Purpose::Mail, "a", None)
        .await;
    assert!(
        matches!(by_addr, Err(EgressError::ResolutionFailed)),
        "{by_addr:?}"
    );
    let records = sink.records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].rule, "resolution");
    assert!(
        records[0].resolved.is_empty(),
        "no answer text in the record"
    );

    sink.clear();
    let by_url = guard
        .authorize(&spot_egress::Request {
            url: "https://relay.corp.test:587/".parse().unwrap(),
            purpose: Purpose::Mail,
            tenant: "a".into(),
            actor: None,
        })
        .await;
    assert!(
        matches!(by_url, Err(EgressError::ResolutionFailed)),
        "{by_url:?}"
    );
    assert_eq!(sink.records().len(), 1);
    assert_eq!(sink.records()[0].rule, "resolution");
}
