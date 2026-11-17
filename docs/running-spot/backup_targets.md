# Backup targets: USB drive and cloud

`spot backup` writes to a named target. A folder target is described in [backup.md](backup.md);
this page covers the other two kinds. A target is chosen by name (`--target usb`), never by a path
on the command line.

> Status: the target adapters (`services::backup_targets`) are in place and tested. `spot backup`
> and the `[backup.targets]` table accept the `removable` kind and the cloud options below once
> the follow-up task that edits `config.rs` and `commands/backup.rs` has landed; until then those
> two kinds are refused as unsupported.

## USB drive (kind `removable`)

```toml
[backup.targets.usb]
kind = "removable"
volume = "SPOT-BACKUP"      # label, or a UUID: "1234-ABCD", "uuid:1a2b3c4d"
subdir = "spot-backups"     # optional; the default is spot-backups
```

The volume is named by **label or UUID**, never by drive letter (`E:`) or mount path, because
those change from one plug-in to the next. A bare 36-character UUID, `XXXX-XXXX` or 16 hexadecimal
digits is read as a UUID, anything else as a label; write `uuid:` or `label:` to be explicit (a
Windows volume serial number needs `uuid:`). Prefer the UUID when two sticks share a label: two
matches are refused (`backup.target_ambiguous`) rather than guessing.

Detection reads `/dev/disk/by-uuid`, `/dev/disk/by-label` and `/proc/self/mountinfo` on Linux and
`Win32_LogicalDisk` (drive type 2, `DRIVE_REMOVABLE`) on Windows. A volume is used only when it is

- present **and mounted** (the empty mount-point directory of a removed stick is never written to),
- reported as removable by the platform, and
- not on the same physical disk as `/`, `/boot`, `/home`, `/var` or `/usr` (Windows: the system
  drive).

A USB hard disk that reports itself as fixed is refused (`backup.target_unsafe`); use a folder
target for it.

### When the stick is missing

The run stops before taking anything, prints `backup.target_absent` and exits "refused". The
scheduled service raises the alert and tries again at the next slot; nothing is written, and no
half-copied file is left on the stick (a package appears only after it is complete). Plug the stick
in and mount it (most desktops do this on plug-in); the next run picks it up.

After an interactive run the command prints how to remove the stick safely: on Linux
`udisksctl unmount -b <device>`, on Windows "Safely Remove Hardware".

### Restore from the stick

`spot restore` reads the package through the same target. A package whose manifest names another
tenant is refused (`backup.target_foreign_tenant`), and the package's own digest is checked before
anything is restored.

## Cloud (kind `cloud`)

```toml
[backup.targets.offsite]
kind = "cloud"
endpoint = "https://s3.example.com"
key_id = "AKIA..."                  # an identifier, not the secret
credential_ref = "secret:backup-s3" # a reference into the secret store
prefix = "acme"                     # optional
max_kbps = 512                      # optional upload limit, KiB per second
quota_bytes = 107374182400          # optional, for stores that cannot report remaining space
```

- **Client-side encryption.** Each package is cut into 4 MiB chunks, each sealed with an
  authenticated cipher under the **tenant's own key** (the recovery key printed once when cloud
  backup was set up) bound to the package name and the chunk number. The key never leaves this
  host and is not held by the control plane; the store sees only ciphertext and object names.
- **Resumable.** A chunk already uploaded and still valid is not sent again after an interrupted
  run. The sealed manifest is written last, so a half-uploaded package is not listed and is never
  used by a restore.
- **Verified.** Every chunk is read back and its digest compared after upload; a restore
  re-checks every chunk digest and authentication tag and the digest of the whole package.
- **Credentials.** `credential_ref` names a secret (`secret:<name>`); the secret value is never in
  `common.toml`, the output, an error or the audit record. There is no `insecure` option: TLS is
  always verified.
- **Bandwidth.** `max_kbps` spreads each chunk over time (4 MiB at 512 KiB/s takes 8 seconds).
- **Endpoint.** Any S3-compatible endpoint. Without WP-CTL-08.T2 (the managed service) you supply
  the endpoint yourself.
- A wrong key cannot open a backup: the run fails with `backup.decrypt_failed`.

## Errors

| Code | Meaning |
| --- | --- |
| `backup.target_absent` | The stick is not plugged in or not mounted; retried at the next slot. |
| `backup.target_unsafe` | Not removable, or on the system disk. |
| `backup.target_ambiguous` | Two volumes match the label; use the UUID. |
| `backup.target_invalid` | The target's configuration is wrong (the message names the key). |
| `backup.target_foreign_tenant` | The package belongs to another tenant. |
| `backup.unsupported` | Cloud target on a host without a connector or tenant key. |
