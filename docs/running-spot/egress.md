Part of [Running Spot](../running-spot.md).

# Egress: what a site may reach

Every outbound connection the server makes for a tenant (a webhook, a script, a connector, mail,
an LLM call, a template or PDF fetch, an app) is authorised by one `spot-egress` guard per site
before it is opened. The host builds that guard once, from `[services.egress]` in `common.toml`,
and records what it refuses. Code: `crates/framework/spot-cli/src/services/egress.rs`; operator
commands: `spot egress check`, `spot egress denials`, `spot egress reload`.

The service is **optional** (a failure degrades the site, it does not stop it), with the
exceptions listed under "What stops the site".

## Settings

```toml
[services.egress]
policy_file = "/etc/spot/egress.toml"      # absent: the built-in default policy
denial_log = "/var/lib/spot/{site}/egress-denials.log"   # default: <root>/sites/<site>/egress-denials.log
denial_log_max_bytes = 8388608             # 64 KiB to 1 GiB; four files are kept
denials_per_minute = 60                    # per tenant, purpose, host and port; 1 to 100000
```

Unknown keys are refused. A relative path is relative to the host root; `..` is refused. `{site}`
is the site's name. With several sites an explicit `denial_log` must contain `{site}`: two sites
never share a file. The policy file may be shared by every site of the host (it has one section
per tenant, and a tenant is a site's name).

## The policy file

TOML, read by `EgressPolicy::from_toml` and then checked. The file must not be writable by
everyone (`chmod o-w`); it is at most 256 KiB; every problem found is listed in one refusal.

```toml
default = "deny_non_public"          # or "allow" (discouraged: any unlisted tenant reaches any public host)
dangerous_ports = [22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250]

[tenants.acme]                       # the site's name
connect_timeout_secs = 10            # 1 to 3600
tls_timeout_secs = 10
total_timeout_secs = 30
max_response_bytes = 104857600       # 1 to 4 GiB
max_redirects = 5                    # 0 to 20

# One SMTP relay. Port 25 is on the dangerous list, so it is opened here by naming it; 587 would
# not need to be named but is listed so nothing else on this host is reachable.
[[tenants.acme.allow]]
host = "smtp.relay.example"
ports = [587]
purpose = "mail"

# One webhook host, over https only.
[[tenants.acme.allow]]
scheme = "https"
host = "hooks.partner.example"
ports = [443]
purpose = "webhook"

# One LLM endpoint (a wildcard covers sub-domains, never the bare suffix).
[[tenants.acme.allow]]
scheme = "https"
host = "api.llm.example"
ports = [443]
purpose = "llm"

# A service on the company network. A non-public address is reachable only through allow_private
# of the very entry that covers the host, port and purpose. Cloud metadata is never reachable.
[[tenants.acme.allow]]
scheme = "https"
host = "erp.corp.example"
ports = [8443]
purpose = "connector"
allow_private = ["10.20.0.0/16"]

# A tenant's deny list wins over its allow list.
[[tenants.acme.deny]]
host = "*.blocked.example"
```

Rules worth knowing:

- An entry without `scheme` covers address-based callers (SMTP) and `https`, never plain `http`.
  Name `scheme = "http"` to allow it.
- An entry without `ports` covers every ordinary port, never a dangerous one. A dangerous port is
  opened only by an entry that lists it.
- `purpose` is one of `webhook`, `script`, `connector`, `mail`, `llm`, `template`, `pdf`, `app`.
  An entry without it covers every purpose.
- A URL-based request (`Guard::authorize`) from a tenant with **no section at all** is refused.
  An address-based request (`authorize_addr`, used for SMTP) from such a tenant gets the default
  policy: public addresses on ordinary ports.
- `allow_private` entries are an address, a CIDR, or either with a port (`10.20.0.5:5432`,
  `[fd00::5]:443`).

Without `policy_file` the default is `deny_non_public` with no tenant sections: nothing but public
addresses on ordinary ports for an address-based caller, nothing at all for a URL-based one.

## Checking a destination

```
spot egress check https://hooks.partner.example/in --purpose webhook [--tenant acme] [--site acme]
```

It resolves the host, classifies every answer, applies the site's policy file and prints the
verdict. It never opens a connection and records nothing. Only the scheme, host and port of the
URL are used and printed (a query string or credentials you type are dropped). Output:

```
allowed: https://hooks.partner.example:443 (tenant acme, purpose webhook)
a caller would be pinned to: 203.0.113.9
```

or

```
denied: https://10.0.0.5:443 (tenant acme, purpose webhook)
rule: address
address class: private
code: egress.denied_address
```

`rule` is `policy` (not in the allow-list), `address` (a resolved address is not public and no
`allow_private` covers it), `port`, `resolution` (the name does not resolve) or `url`.

Exit codes follow the command registry: **0** allowed, **1** usage (not an http or https URL, an
unknown purpose, no or an unknown site), **2** denied, **3** failed (for example the policy file is
invalid). The brief of this task listed 2 for usage and 3 for denied; the registry's convention
(1 usage, 2 refused, 3 failed, see [commands](commands.md)) wins.

## Reading denials

```
spot egress denials [--since 1h] [--purpose webhook] [--tenant acme] [--limit 50] [--json]
```

Newest first, read from the tail of the denial log (at most 4 MiB of it and of the first rotated
file; a line that does not parse is skipped). `--since` takes `90s`, `30m`, `1h` or `7d`; `--limit`
is 1 to 1000. Text output escapes every field a tenant controls, so a hostile host name cannot
send control sequences to your terminal. `--json` prints one object per line:

```json
{"at":1789387200,"tenant":"acme","purpose":"webhook","actor":"svc-flow","scheme":"https","host":"internal.test","port":8443,"class":"private","rule":"address"}
```

`at` is unix seconds. `class` is the class of the refused address (`loopback`, `private`,
`link_local`, `cloud_metadata`, `carrier_grade_nat`, ...), or `public` for a refusal that is not about
an address. Counts since the server started are on the metric `spot_egress_denied_total{purpose}`
(the purpose is a closed set; no host or tenant is ever a label) and on the `EgressStatus` port.

When the budget (`denials_per_minute`) is spent, further denials of the same tenant, purpose, host
and port are counted but not written; the difference between the count and the lines written is
`EgressStatus::suppressed`.

### What is never logged

The line has no query string, no userinfo (`user:password@`), no fragment, no path, no header, no
body, and **no resolved address**: an internal address is what a tenant must not learn, and the file
may be sent to support. The denial log is mode 0600, never a symlink, and is not served over HTTP.
`spot egress check` prints the address class of a refused address, not the address.

## Changing the policy

Edit the file, then either send SIGHUP to the server or run

```
spot egress reload [--site acme]
```

which validates the file first and, only when it is good, asks the running server to re-read it
(the server looks for the request once a second). A file that does not parse changes nothing: the
server keeps the previous policy, the site reports itself **degraded** (`egress_host.reload_failed`
on the readiness page) and the problems are in the server log; run `spot egress reload` to see every
problem. The next good reload clears it. A reload that finds the same file does nothing.

A reload swaps the guard behind one shared handle, `SharedGuard`, whose identity never changes: a
holder cloned before the reload sees the new policy on its next decision (a removed allow is denied, a
new allow works), and a decision made during the swap sees the old policy or the new one, never a mix.
A reload that fails validation changes nothing and reports the catalogued error. Every applied swap
writes one audit line (target `spot::audit::egress_policy`: generation, the first 12 hex digits of the
policy digest and the tenant counts, no host or entry).

The plain `Arc<Guard>` the site also provides is the policy **at start** and does not follow
reloads; new code takes the `SharedGuard`.

## What stops the site

At start, a refused `[services.egress]` table, a path with `..`, a policy file that is missing,
unreadable, writable by everyone or invalid (all problems listed) stop the site from starting: a
site must not run with a policy nobody meant. After start, a denial log that cannot be written
degrades the site (`egress_host.denial_log_unwritable`) and counts the failures, once a minute in the
log by kind; the request is refused exactly as before. A write failure never turns a denial into an
allow.

## For module authors: taking the shared guard

The module registers at `ORDER` 950 and provides three ports:

```rust
let guard = ctx.require_port::<spot_cli::services::egress::SharedGuard>()?;   // follows reloads
// (`spot_egress::Guard` is still provided: the start-time snapshot, kept for callers not yet moved)
let status = ctx.port::<spot_cli::services::egress::EgressStatus>();   // counts, tenants, digest
```

A module that registers before 950 gets the registry's error naming the port. Pass the site's name as
the tenant and a purpose from the closed set; dial only `Authorized::socket_addrs()`.

### Migration note for callers

Today two callers still build their own guard and an in-memory sink, so their denials vanish and
are not in `spot egress denials`:

- `services/mail.rs` (`policy_for`: the site's SMTP host and port, purpose `mail`);
- `services/process_integration.rs` (`integration_egress_policy`: `default = "allow"`, no tenant
  entries, used by the integration nodes' raw webhook post).

Each should take `ctx.port::<SharedGuard>()`, and its own allow entries move
into the tenant's section of the policy file (the SMTP relay as above). The integration policy's
`default = "allow"` is the widest setting there is; moving it into the file lets an operator narrow
it. Other callers (connectors, webhooks, the app runtime, LLM providers) start on the shared guard.
