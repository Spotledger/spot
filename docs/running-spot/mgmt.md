Part of [Running Spot](../running-spot.md).

# `spot mgmt`: the management API client

`spot mgmt` is an equal client of the management API (ADR-44), the same API the Admin Center console
calls. It talks to the site's **management listener**, which is a separate port from the public API.

```text
spot mgmt list|describe OP|validate OP|run OP|status ID|cancel ID  --url https://host:9443 \
          --cert client.pem --key client.key --ca ca.pem [--json]
```

- `validate OP --param k=v` checks the parameters on the server and prints the **equivalent command
  line**. It runs nothing and needs no signature.
- `run OP --param k=v --audience SITE --sign-with admin.key [--wait]` builds the signed envelope on your
  machine and posts it. `--audience` is the site id. A `read` operation needs no key; every other
  operation needs `--sign-with`, and `high` and `critical` operations also need the approval records the
  catalogue names.

## Secrets

A secret parameter is **never** a `--param` value: it would stay in the shell history and the process
list. The server's catalogue says which parameters are secret; give those with
`--param-file name=PATH` (a file with the value) or `--param-env name=VARIABLE`.

## Making the trust material

```text
spot mgmt ca init --dir ./trust --cn alice --server-name host.example.test
spot mgmt key gen --out ./trust/admin.key
```

`ca init` writes an operator CA, a client certificate and, with `--server-name`, a server certificate
(ECDSA P-256, one year by default, all files mode 0600, never overwritten). Put `ca.pem` in the host's
`operator_ca`, `CN=alice` in its operator allow-list, and the `public_key` that `key gen` prints in its
`administrator_keys`. Keep `ca.key` offline after issuing. `key sign` builds a signed envelope offline
for an air-gapped site.

Key files must be mode 0600; a looser file is refused. Hardware keys (`--sign-with-pkcs11`) are not
available in this build.

## TLS and exit codes

The server is verified against `--ca` and TLS 1.3 is mandatory: there is no `--insecure` flag and no
plain `http://`. Exit codes: 0 done, 1 usage, 2 refused (`mgmt.forbidden`, `mgmt.approval_required`, no
signature key; nothing changed), 3 failed.
