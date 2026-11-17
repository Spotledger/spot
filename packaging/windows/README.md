# Windows installer (`Spotledger.msi`)

WiX 5 project for the per-machine x64 installer: the server and its `SpotledgerServer` service (via
`spot-winsvc install`), the optional `Agent` feature, the `Postgres` feature (bundled PostgreSQL
files), an inbound firewall rule for the chosen port, and a Start-menu shortcut to the admin entry.
The operator guide is the project process notes (WP-PRD-03.T6).

Build (Windows runner only): `python tools/windows-packaging/build_msi.py --version X.Y.Z --payload DIR`.
Static checks run anywhere: `python -m unittest discover -s tools/windows-packaging/tests -v`.

## Silent install

`msiexec /i Spotledger.msi /qn /l*v log.txt` succeeds with defaults. Properties:

| Property | Default | Meaning |
| --- | --- | --- |
| `INSTALLDIR` | `%ProgramFiles%\Spotledger` | program files |
| `DATADIR` | `%ProgramData%\Spotledger` | data and config (kept on uninstall) |
| `HTTP_PORT` | `8080` | the server's port (and the firewall rule's) |
| `ADMIN_EMAIL` | empty | the first administrator's address |
| `ENROL_GATEWAY` | empty | agent enrolment gateway |
| `ENROL_TOKEN` | empty | agent enrolment token: a **hidden** property (`***` in the log), consumed once |
| `LAN_DISCOVERY` | `0` | `1` advertises the server on the LAN |
| `FIREWALL` | `1` | `0` adds no firewall rule (Private and Domain profiles otherwise) |
| `REMOVE_DATA` | `0` | `1` with an uninstall **deletes the data folder**: backups first |

An upgrade MSI (same `UpgradeCode`, higher version) stops the service, upgrades, runs the
compatibility check of `spot-winsvc upgrade` and restarts. A downgrade is refused. A failing custom
action rolls the whole install back (no half service).

## Signing

`build_msi.py` signs the MSI and the binaries with the certificate named by `SPOT_SIGN_CERT`
(password in `SPOT_SIGN_PASSWORD`). No certificate is in this repository. Without one the build is an
unsigned dev build: the build kind `dev-unsigned` is written to the MSI
(`HKLM\Software\Spotledger\BuildKind` and the version string) and the updater refuses it.
