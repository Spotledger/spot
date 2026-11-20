# tools/lints/egress

Outbound-connection gate (WP-SEC-09.T6, risk R-030). Every outbound call from the platform passes
one policy, `spot-egress` (private, loopback, link-local and metadata ranges denied; DNS pinned;
redirects re-validated). This lint fails when product code opens a connection by any other path.
CI runs every `tools/lints/*/run.sh`, so it runs in the `gates` job.

```sh
bash tools/lints/egress/run.sh                                  # exit 0 clean, 1 violations, 2 unreadable input
python3 -m unittest discover -s tools/lints/egress/tests -v
```

## The rule

Every `*.rs` file under `crates/`, `apps/` and `harness/` is scanned (not `target/`, `tests/` or
`benches/` directories, not `#[cfg(test)]` modules; comments and string literals are blanked first).
A finding is any use of: `TcpStream::connect`, `tokio::net::TcpStream`, `UdpSocket::connect`,
`ToSocketAddrs` in a file that connects, `hyper::client`, `hyper_util::client`, `reqwest`, `ureq`,
`isahc`, `surf`, `curl`, `lettre`, `tokio_rustls::TlsConnector`, `native_tls`, `tokio_native_tls`,
`hyper_rustls`, `rustls::ClientConnection`. An import is reported too (a later use of the imported
name has no forbidden path). `lettre::message`, `lettre::address`, `lettre::Address` and
`lettre::Message` only build a message and are not findings; every other `lettre` path is.

Every `Cargo.toml` is checked too: a crate listing `reqwest`, `ureq`, `isahc`, `surf`, `curl`,
`lettre`, `native-tls`, `tokio-native-tls` or `hyper-rustls` under `[dependencies]` (also
`[target.*.dependencies]`, and a renamed one through `package =`) is reported. `[dev-dependencies]`
is not.

It is a heuristic over text, not a parser, and fails closed.

## Adding an exception

Prefer not to. Build the call on `spot_egress::EgressConnector` / `HttpSender`, or call
`Guard::authorize` / `authorize_addr` first. If a file really must be listed, add one
`[[allow]]` entry to `allow.toml` (at most one per file; the key is the file's path from the
repository root; a `Cargo.toml` is a file too):

| field | |
|---|---|
| `path` | file, repository-relative, `/` separators |
| `reason` | non-empty: why this is safe |
| `owner_wp` | the work package that owns the file |
| `kind` | one of the kinds below |

Kinds: `egress-implementation` (spot-egress itself); `loopback-only` (can only target a local port
the process chose, for example spot-pg-supervisor to its own PostgreSQL); `authorised-by-guard`
(the call is preceded by `Guard::authorize`/`authorize_addr` or built on `EgressConnector`; add
`function = "<name>"`); `peer-pinned` (a peer whose identity is pinned by certificate and whose
address the operator configured; add `pinned_at = "<where the pin is enforced>"`); `harness`
(test harness crates under `harness/`); `gap` (a real hole, with `tracked_by = "<follow-up>"`).

A hit that fits no kind is a `gap`, never silently allowed.

## The list can only shrink

- an entry that matches no hit is an error (stale): delete it when the code is fixed;
- an entry without a reason, with an unknown kind or without its kind's extra field is an error;
- the number of `gap` entries is printed on every run, and the lint fails when it exceeds the
  `max_gaps = N` line in `allow.toml`. Lower `max_gaps` whenever a gap is closed; never raise it.
