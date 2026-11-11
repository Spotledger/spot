# spot-egress: Outbound Network Policy

Prevents server-side request forgery (SSRF) by enforcing an outbound network policy on scripts, WASM apps, webhooks, connectors, mail, templates, the PDF renderer and LLM providers.

## Why it exists

Without egress control, a tenant's script or webhook can reach:
- Cloud metadata endpoints (`169.254.169.254`, etc.)
- The database server
- Internal services on private networks
- Any other service the platform can reach

This crate is the sole guard: every outbound caller (scripts, webhooks, connectors, mail, LLM providers) must authorize the request through `Guard::authorize` or `Guard::authorize_addr`.

## How it works

1. **Address classification**: Identifies loopback, private, cloud metadata, and other non-public addresses. The classifier is exhaustive and table-tested; anything not provably `Public` is denied by default.

2. **Policy evaluation**: Checks the request against the per-tenant policy (allow-list and deny-list), with exact host and port matching.

3. **DNS pinning**: Resolves the host once, classifies every returned address, and returns only those addresses. The caller connects *only* to the pinned addresses—the defence against DNS rebinding.

4. **Denial records**: Every denied request is logged (with no secrets) so the tenant and the operator can see why a request was rejected.

## Usage

### For HTTP callers (webhooks, scripts, connectors)

```rust,ignore
use spot_egress::{Guard, Request, Purpose, EgressPolicy, SystemResolver, InMemorySink};
use spot_core::SystemClock;
use std::sync::Arc;

let policy = EgressPolicy::default();
let resolver = Arc::new(SystemResolver);
let sink = Arc::new(InMemorySink::new());
let clock = Arc::new(SystemClock::new());
let guard = Guard::with_defaults(policy, clock);

let request = Request {
    url: "https://api.example.com/webhook".parse()?,
    purpose: Purpose::Webhook,
    tenant: "tenant1".to_string(),
    actor: Some("user@tenant1".to_string()),
};

match guard.authorize(&request).await {
    Ok(authorized) => {
        // Connect only to authorized.addrs; use authorized.original_host for SNI
        println!("Allowed to connect to: {:?}", authorized.addrs);
    }
    Err(e) => {
        // Connection denied
        eprintln!("Egress denied: {}", e);
    }
}
```

### For non-HTTP callers (SMTP, database, etc.)

```rust,ignore
match guard.authorize_addr("mail.example.com", 587, Purpose::Mail, "tenant1", None).await {
    Ok(authorized) => {
        // Dial authorized.socket_addrs() and nothing else; use authorized.host()
        // only as the TLS server name. Never resolve "mail.example.com" again.
        let stream = TcpStream::connect(authorized.socket_addrs()).await?;
    }
    Err(e) => {
        // Connection denied
    }
}
```

`authorize_addr` decides exactly as `authorize` does for a URL with the same host, port and
purpose (one shared code path):

- The tenant's `deny` list wins. An `allow` entry must cover the host, the port (if it lists
  any) and the purpose (if it names one). An entry without a `scheme` covers address-based
  requests; an entry that names `http` or `https` does not.
- A non-public address is reachable only through the `allow_private` list of an entry that
  covers this destination. An entry for one host or purpose never opens a private address for
  another host or purpose, and a tenant's entry never helps another tenant.
- **Cloud metadata addresses are never reachable**, whatever a tenant lists (this includes the
  IPv4-mapped IPv6 spelling). It is the platform deny list that no policy can override.
- A dangerous port (25, 5432, ...) opens only through a covering entry that lists that port in
  `ports`; an entry without `ports` never opens one. Mail needs no separate port list: an entry
  with `purpose = "mail"` and its own `ports` is the mail list.
- One denied answer refuses the whole host, so a name that resolves to a public and an
  internal address is refused.
- Every denial is recorded with the tenant, actor, purpose, host, port and rule.
- A tenant with no policy entry at all gets the default policy as written (`deny_non_public`):
  public addresses on ordinary ports pass, nothing internal does. (The URL path denies such a
  tenant outright.)

**Connect only to `Authorized::socket_addrs()`.** The host was resolved once and every answer
checked. A caller that resolves the hostname again after authorising (a plain
`connect(host, port)`, a client library's own lookup) can be handed an internal address by a
hostile DNS server (DNS rebinding), which defeats the guard.

### Requests with a method and a body

Webhooks, connectors and payment or tax calls POST a payload through the same path, so they
never need a second transport: authorization is identical for every method (the same allow-list,
the same address classification, the same DNS pin, the same timeouts and the same denial
records), and the sender writes `Host` from the URL and `Content-Length` from the body. A
caller-supplied `Host`, `Content-Length`, `Transfer-Encoding` or other hop-by-hop header is
dropped, never forwarded.

```rust,ignore
use spot_egress::{HttpRequest, Method, MAX_REQUEST_BODY_BYTES};

let request = HttpRequest::new(
    Method::Post,
    "https://api.example.com/webhook".parse()?,
    my_headers,                       // never Host/Content-Length/Transfer-Encoding
    Some(b"{\"event\":\"posted\"}".as_slice().into()),
);

let authorized = guard.authorize(&spot_egress::Request {
    url: request.url.clone(),
    purpose: Purpose::Webhook,
    tenant: "tenant1".to_string(),
    actor: None,
}).await?;

let response = connector
    .follow_request_redirects(&sender, request, 5)
    .await?;
```

A request body is bounded by `MAX_REQUEST_BODY_BYTES` (10 MiB): a larger one fails with
`egress.request_too_large` before any connection is made. Redirects never replay a body by
default. A `301`/`302`/`303` becomes a body-less `GET`, and only after the new target passes the
whole policy again; a `307`/`308` is returned to the caller as it is unless the request sets
`follow_body_redirects`, and even then the body goes to the new target only after it is
re-authorized.

## Configuration

Policies are TOML files with per-tenant allow-lists and deny-lists:

```toml
default = "deny_non_public"

[tenants.tenant1.allow.0]
scheme = "https"
host = "api.example.com"
ports = [443]
purpose = "webhook"

[tenants.tenant1.allow.1]
host = "db.internal"
ports = [5432]
allow_private = ["10.20.0.5:5432"]  # Explicit address; hostname not allowed
```

### Policy rules

- **Default**: `deny_non_public` (the default). All requests to non-public addresses are denied unless explicitly allowed.
- **Allow-list**: Exact host and port matching. A wildcard like `*.example.com` matches subdomains but not the base domain.
  - `scheme`: Required for HTTP; defaults to `https` only. Add `http` to allow it.
  - `host`: Exact or `*.suffix` wildcard.
  - `ports`: A list of allowed ports. If omitted, any port is allowed.
  - `purpose`: Optional; if specified, the rule applies only to this purpose.
  - `allow_private`: A list of explicit addresses or CIDRs that are allowed. Private addresses are *always* denied unless named here.
- **Deny-list**: Overrides any allow entry; `deny` always wins.
- **Dangerous ports**: By default, ports 22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250 are denied unless explicitly allowed.

## Address classification

Every IP address is classified as:
- `Public`: Routable on the internet.
- `Loopback`: 127.0.0.0/8, ::1.
- `Unspecified`: 0.0.0.0/8, ::.
- `Private`: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16.
- `CarrierGradeNat`: 100.64.0.0/10.
- `LinkLocal`: 169.254.0.0/16, fe80::/10.
- `CloudMetadata`: 169.254.169.254 (AWS), 169.254.170.2 (GCP), 100.100.100.200 (Alibaba), 168.63.129.16 (Azure), 192.0.0.192 (Google), fd00:ec2::254 (AWS IPv6).
- `UniqueLocal`: fc00::/7.
- `Multicast`: 224.0.0.0/4, ff00::/8.
- `Broadcast`: 255.255.255.255/32.
- `Documentation`: 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24, 2001:db8::/32.
- `Benchmarking`: 198.18.0.0/15, 2002::/16, 2001::/32.
- `Ipv4Mapped`: IPv6 addresses that embed a private IPv4.
- `Ipv4Compatible`: IPv6 addresses (deprecated) that embed a private IPv4.

## Isolation boundary

**Outbound policy is the sole decision-maker for connectivity.**

The `Guard` decides *whether* a connection may be made and returns the pinned addresses. Callers are responsible for:
- Connecting only to the pinned addresses, `Authorized::socket_addrs()` (never the hostname).
- Preserving the original hostname for SNI and the `Host` header.
- Respecting timeouts set in the policy (connect, TLS, total).
- Following redirects, if supported (with re-authorization on every hop).
- Bounding response bodies to `max_response_bytes`.

**Retry, circuit breaking and bulkheads** are handled by the resilience kit (ADR-072), *not* this crate. The two compose:

```text
resilience kit (retry, breaker, bulkhead) → egress policy (allow/deny, DNS pinning) → connection
```

The egress policy runs inside the bulkhead; if it denies, the call fails immediately without retry.

## Denial records

Every denial is recorded with:
- `tenant`, `purpose`, `actor`: Who tried to connect.
- `scheme`, `host`, `port`: Where they tried to go.
- `resolved`: The resolved IP addresses (empty if policy denied before resolution).
- `class`: The classification of the resolved address.
- `rule`: Which rule matched (`policy`, `address`, `port`, `resolution`).
- `at`: When the denial occurred.

Denial records never contain:
- Query strings, userinfo, or fragments from the URL.
- Request headers or bodies (they may contain secrets).

A rate-limited sink (first N per key per minute, then a counter) prevents log flooding.

## Testing

The crate provides:
- `TestResolver` for scripting DNS responses, including DNS rebinding (public first, private second).
- `InMemorySink` for collecting denials in tests.
- Property tests that verify `classify` never panics and covers all address classes.

Example:

```rust,ignore
use spot_egress::TestResolver;

let resolver = TestResolver::new();
resolver.add_response(
    "rebind.test",
    vec![
        vec!["8.8.8.8".parse().unwrap()],  // First lookup
        vec!["127.0.0.1".parse().unwrap()], // Second lookup
    ],
);
```

## Errors

All errors are typed in the `EgressError` enum:
- `AddressDenied { class }`: A resolved address was denied.
- `PolicyDenied`: The policy doesn't allow the destination.
- `PortDenied { port }`: The port is dangerous or denied.
- `ResolutionMixed`: A single lookup returned mixed address classes (an attack).
- `ResolutionFailed`: DNS resolution failed.
- `ResponseTooLarge`: The response body exceeded the caller's bound.
- `RequestTooLarge { limit }`: The request body exceeded `MAX_REQUEST_BODY_BYTES`.
- `InvalidUrl`: The request URL is malformed.
- `InvalidConfig`: Policy configuration is invalid.
