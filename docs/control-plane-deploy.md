# Deploying the control plane

Packaging for `spot-control-plane` (WP-CTL-01.T6) lives in `packaging/linux/control-plane/`, with the
same hardening rules as the host packages (`packaging/linux/`). Build: prebuilt binary in, no Rust compiled:

```sh
python3 packaging/linux/control-plane/build_control_plane.py rpm --binary target/release/spot-control-plane --version 1.2.3 --out dist
python3 packaging/linux/control-plane/build_control_plane.py deb --binary target/release/spot-control-plane --version 1.2.3 --out dist
bash packaging/linux/verify/verify-control-plane.sh
```

## What the packages do and do not do

- Install the daemon, the unit, the sysusers/tmpfiles data and two templates
  (`/etc/spot-control-plane/control-plane.toml.example`, `environment`). **No key, certificate or
  password is shipped**, in a package or in the image.
- Never start, enable or configure the service. The unit has
  `ConditionPathExists=/etc/spot-control-plane/control-plane.toml`: it does not start until you write one.
- Upgrade: a running service is restarted once; configuration and state are kept. `remove` and `purge`
  never delete `/var/lib/spot-control-plane` or your keys.
- The unit uses the same sandbox directives as the host unit (no capabilities, `NoNewPrivileges`,
  `ProtectSystem=strict`, private tmp and devices, restricted address families and system calls), plus
  `MemoryDenyWriteExecute=true` (the control plane runs no WASM or script engine). The verify script requires
  its `systemd-analyze security` score to be no worse than the host unit's (1.5 on systemd 259, 2026-11-12).

## Ports

| Listener | Default | Who connects | Routes |
| --- | --- | --- | --- |
| operator | `127.0.0.1:8443` | operators (client certificate from the operator CA) | `/cp/v1/...` |
| host | `127.0.0.1:8444` | tenant hosts and agents (client certificate from the host CA) | `/cp/v1/host/...`, `/agent/v1/...` |

Both default to loopback. Change the addresses in the configuration and put a firewall in front: the host
listener is the only one tenant machines need to reach, the operator listener belongs on an admin network.
The two listeners must trust different CAs and use different addresses; the service refuses to start otherwise.
Both require TLS 1.3 and a client certificate at the handshake.

## Certificates: three different keys

1. **Operator CA** (`operator-ca/ca.pem`): signs the operators' client certificates. The service holds only the
   certificate; keep the CA key offline or in your own PKI.
2. **Host CA** (`host-ca/ca.pem` and `ca.key`): the control plane issues short-lived (default 30 days, renewal over
   the existing certificate) client certificates to tenant hosts and agents with it. The key is a file (mode 0600,
   owner `spot-control-plane`) read through the key-vault abstraction; it is never logged. Issuance and revocation
   are audited. Revoked fingerprints are written to the file named by `host_ca.revocation_list`, which must be under
   `/var/lib/spot-control-plane/` (the rest of the file system is read-only to the service).
3. **Licence key** (`licence/`, directory 0750 root:spot-control-plane): signs tenant licences. It is a separate key
   from both CAs. The T6 configuration does not read it; the licensing module will name its path.

The listener server certificates (`tls/operator-server.{pem,key}`, `tls/host-server.{pem,key}`) are ordinary TLS
server certificates for the names clients use. Key files are mode 0600; certificate files may be 0640. Never reuse a
CA key as a server key. Put the database password in `/etc/spot-control-plane/environment`
(`SPOT_CP_DB_PASSWORD=...`, mode 0600, owner `spot-control-plane`); the configuration names the variable, not the value.

## First start

```sh
sudo cp /etc/spot-control-plane/control-plane.toml.example /etc/spot-control-plane/control-plane.toml
sudoedit /etc/spot-control-plane/control-plane.toml        # administrator keys, operators, paths
sudo -u spot-control-plane spot-control-plane check   --config /etc/spot-control-plane/control-plane.toml
sudo -u spot-control-plane spot-control-plane migrate --config /etc/spot-control-plane/control-plane.toml
sudo systemctl enable --now spot-control-plane
```

`check` reports certificates, the CA key, the database and the clock, each failure separately.

## Health and readiness

| Probe | Request | Notes |
| --- | --- | --- |
| liveness | `GET /cp/v1/live` on the operator listener | needs any client certificate from the operator CA; no allow-list entry |
| readiness | `GET /cp/v1/readiness` | an allow-listed operator; one entry per module, `503` when a module is not ready |

The container `HEALTHCHECK` uses liveness with a **dedicated probe client certificate** (issued by the operator CA,
mounted read-only at `/etc/spot-control-plane/probe/{client.pem,client.key,server-ca.pem}`), curl in exec form, no shell.

## Container

`container/Containerfile` assembles the image from an empty root (glibc, libgcc and curl-minimal only): no shell, no
package manager, runs as uid 10001, meant for `--read-only --cap-drop=all --security-opt=no-new-privileges`. Configuration
and keys are mounted from the host; state is the `/var/lib/spot-control-plane` volume.
`compose/compose.yaml` runs it next to PostgreSQL **for development only**: throwaway certificates from
`compose/gen-dev-certs.sh`, loopback ports, the database password from your environment.

## Backing up the control database

The control database holds the tenant registry, placement, licences, usage counters, health, support grants and links
(never tenant business data). Back it up like any production database, with these specifics:

- Take a logical backup (`pg_dump -Fc spot_control`) at least daily, plus WAL archiving if your recovery point
  objective is shorter. Encrypt backups: they contain registry and licence data.
- Back up the **keys and certificates separately** from the database dump (offline, access-controlled): the host CA
  key, the licence key and the operator CA material. A restored database without the host CA key cannot renew host
  certificates; every host would have to be re-enrolled.
- Restore test: restore into an empty database, run `spot-control-plane migrate`, then `check`. Do it on a schedule.
- `/var/lib/spot-control-plane` holds the revocation list: include it in the file backup, or a restore forgets revocations.

## Known limits

- The image build, the RPM and DEB builds and the compose file were written without a container runtime, `rpmbuild`
  or `dpkg-deb` and have not been run by the task that wrote them; the bundle or the packaging runner runs them.
- Rootless podman maps uid 10001 to a subordinate id: `podman unshare chown -R 10001:10001 ./dev` before `compose up`.
- The probe route is `/cp/v1/live` (as implemented), not `/cp/v1/health` as the task text says.
