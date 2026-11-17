# The management API (`[services.mgmt_api]`)

The management API is the typed administrative interface the Admin Center console and `spot mgmt`
call (ADR-44). It is **off by default** and runs on its **own listener** (its own port and TLS), never on
the public API port.

```toml
[services.mgmt_api]
enabled            = true
listen             = "127.0.0.1:9443"      # a non-loopback address needs allow_remote = true
server_cert        = "/etc/spot/mgmt/server.pem"
server_key         = "/etc/spot/mgmt/server.key"   # mode 0600 or 0640, owned by the service user
operator_ca        = "/etc/spot/mgmt/ca.pem"
operators          = ["CN=alice"]          # certificate subjects on the allow-list
administrator_keys = "/etc/spot/mgmt/administrators.txt"   # one Ed25519 public key (64 hex) per line
# audience         = "my-site"             # defaults to the site id
```

## How a call is authorised

1. **Mutual TLS 1.3.** The client certificate must chain to `operator_ca`, and its subject must be on
   the `operators` list. There is no token, password or cookie.
2. **A signed command** for anything above `read`: the administrator's Ed25519 signature over the
   command, naming this site as audience, valid for at most ten minutes, with a nonce that has never
   been used. The nonces are kept in PostgreSQL, so a **restart does not reopen the replay window**.
3. **Approvals** for `high` and `critical` operations where the catalogue names them.

Every request that reaches authentication is written to the site's audit chain, refusals included. If a
record cannot be stored the command is refused: nothing runs unaudited.

## Creating the trust material

With the helper of `spot mgmt` (see `mgmt.md`):

```text
spot mgmt ca init --dir ./trust --cn alice --server-name host.example.test
spot mgmt key gen --out ./trust/admin.key        # prints the public key for administrators.txt
```

or with `openssl`: an operator CA, a server certificate for the listener, and a client certificate per
operator. Guard administrator keys like signing keys: a hardware token is the best place for them.
The port should be reachable only from the administrators' network: firewall it.

## Start-up, reload and shutdown

When enabled the service is **required**: if the certificate, key, CA or administrator keys do not
load, the key file is too open, the nonce table is unreachable or the clock is wrong, the site does not
start. `spot reload` and SIGHUP re-read the certificate, key, operator CA, operator list and
administrator keys; new connections use them, running commands are not interrupted, and a file that does
not load leaves the old configuration in place. Shutdown waits for running commands up to
`drain_seconds` (default 30).

## Metrics

`MgmtMetrics` counts commands by operation and outcome, signature refusals, replay refusals and
refused connection attempts.
