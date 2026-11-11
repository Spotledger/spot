Part of [Running Spot](../running-spot.md).

# The secrets service: the key file, rotation and what never to do

**A lost key file loses every sealed value.** Sealed values (passwords, token digests, client
secrets: every `secret` field) are encrypted under keys that the key file protects. Without the
file nothing can be opened, and there is no recovery: not from the database, not from a backup of
the database, not from us. Back the key file up, apart from the data, before the first value is
written, and again after every rotation.

## What it is

`spot serve` builds one key provider and one vault per site from `[services.secrets]` of
`common.toml` and hands the vault to the document engine, so a `secret` field is sealed in the
same transaction as its record. The provider is a **local key file** today; the `KeyProvider`
trait is the seam for a KMS later.

```toml
[services.secrets]
provider = "local"                       # the only provider
kek_file = "/var/lib/spot-keys/{site}.json"   # absolute, outside the site folders; {site} = the site's name
```

* `kek_file` is mandatory once the table exists, must be an absolute path, must not contain `..`,
  and must not be inside `<root>/sites` (a copy or backup of a site would carry the key with the
  data). With more than one site it must contain `{site}`: every site has its own key file, so a
  leak of one does not open another.
* The file must be a non-empty regular file, mode `0600` (no bit for group or others) and owned by
  the user that runs `spot` (checked on Unix; on Windows keep it in a folder only that account can
  read). An unset, missing, empty, world-readable, foreign-owned or malformed file stops the host
  from starting; the message names the file and never a key.
* The key lives in the process only inside the provider (zeroed on drop). It is not in
  `site.toml`, not in an environment variable, not in a log line, a metric, an audit record, an
  error, a readiness detail or any route.
* **Level: required.** The `secrets` service is checked at start-up: it wraps and unwraps a
  canary under the current key version, checks the blind-index key derives, and reads the newest
  stored values of every secret field to confirm the file holds the key version each was sealed
  under (an older copy of the file fails here, naming the version). Any failure stops the site
  (`spot serve` exits; on a reload the site answers 503). There is no plaintext fallback and no
  default key.
* A host whose `common.toml` has no `[services.secrets]` table starts without a vault (as before
  this service existed); a secret field then cannot be written. Add the table before storing any
  credential in a secret field.
* The credentials that still live in `site.toml` (password verifiers, API key digests) stay
  readable; the start-up log says how many remain. Moving them is WP-PLT-02.T8.

## Setting it up

```bash
spot secrets init --site acme            # prints the plan
spot secrets init --site acme --apply    # creates /var/lib/spot-keys/acme.json, mode 0600
cp /var/lib/spot-keys/acme.json /secure/offline/backup/   # NOW
spot serve
```

`init` never overwrites a key file. The server never creates one.

## Looking at it

```bash
spot secrets status --site acme
```

prints the key file, the current version, the versions that still hold key material and those
retired, and for every secret field how many stored values are under each version. Counts only.

## Rotating the key (no downtime)

```bash
spot secrets rotate-kek --site acme            # plan only
spot secrets rotate-kek --site acme --apply
```

1. Takes the key file's lock (`<kek_file>.lock`), so two rotations cannot race.
2. Writes the audit record `secrets_kek_rotation_started` to the identity audit chain; if the
   chain is not available nothing is changed.
3. Adds key version N+1 to the key file (written atomically, mode 0600) and touches the site's
   reload marker: the running server rebuilds the site and starts sealing under N+1. Until it
   has, it keeps sealing under N, which is still held, so no write fails.
4. Moves every sealed value (and every blind index) onto N+1 in batches (`--batch`, one
   transaction each, default 100), printing progress, until a whole pass moves nothing. Lookups
   keep working throughout: while values are on the old version the engine asks for the current
   and the previous blind index.
5. Verifies with a read-only scan that no stored value is under an older version, and writes
   `secrets_kek_rotation_finished` with the count moved.

Version N is **kept**: a value still under it stays readable, and the rotation can be repeated.
If the command stops early, or a write by a server that had not reloaded yet left a value under
N, `status` shows the count and

```bash
spot secrets rotate-kek --site acme --resume --apply
```

finishes the job (it adds no version). A plain `rotate-kek --apply` refuses while values are
under an older version, so a half-done rotation is never stacked under a second one.

Rotation is audited in two places: the identity audit chain gets `secrets_kek_rotation_started`,
`_finished`, `_failed` or `_refused` (a count and a reason class only), and the engine journals
each rewrap batch (and each value that could not be moved) in the same transaction as the batch.

Back the key file up again when it ends: the new copy holds the new version.

## Retiring an old version

```bash
spot secrets retire-kek --site acme            # plan
spot secrets retire-kek --site acme --apply --confirm-all-servers-reloaded
```

`--apply` alone is refused with `secrets_host.retire_unconfirmed`; nothing is read or changed. The
flag is your statement that you have checked that every server of the site has reloaded onto the
current key version; the command cannot see the other servers.

destroys the key material of every version that is not current, **only when no stored value is
under it**, and then touches the reload marker. It is irreversible: a copy of the key file taken
earlier is the only way back. Run it only when every server of the site has reloaded onto the
current version (a server that has not yet would still seal under the old one, and values sealed
there become unreadable once it is gone).

A database backup or snapshot taken **before** a rotation holds values sealed under the versions
that were current then. Once those versions are retired, such a backup can be read only with a copy
of the key file that still holds them. Keep the key file copies taken before a retirement for as
long as any backup that predates it, and expire them together.

## What an operator must never do

* Never keep the only copy of the key file on the machine that holds the database.
* Never put the key file in `site.toml`, in a site folder, in a container image, in the repository,
  in a backup together with the database it protects, or in an environment variable.
* Never `chmod` it wider than `0600`, and never share one file between sites.
* Never restore an older copy of the key file over a newer one: values sealed after that copy was
  taken are unreadable by it (the start-up check refuses a site whose stored values name a
  version the file lacks).
* Never edit the file by hand, and never delete `<kek_file>.lock` unless no rotation is running.
* Never run `retire-kek` before the rotation is verified and the servers have reloaded.
* Never retire a version while a backup you may still restore predates the rotation and no copy of
  the older key file exists.
* Never paste a key, a sealed value or a blind index into a ticket or a chat: none of the
  commands above print one, and neither should you.

## Error codes

| Code | Meaning | Fix |
| --- | --- | --- |
| `secrets_host.settings_invalid` | `[services.secrets]` has an unknown key or a wrong type | fix the table |
| `secrets_host.kek_unset` | no `kek_file`, or a provider other than `local` | set `kek_file` |
| `secrets_host.kek_path_refused` | relative path, `..`, inside the site folders, or shared by several sites | use an absolute path with `{site}` outside `<root>/sites` |
| `secrets_host.kek_file_unusable` | missing, empty, unreadable or not a key file | restore it from the backup; `spot secrets init` for a new site only |
| `secrets_host.kek_file_unsafe` | not mode 0600 or not owned by the running user | `chmod 600`, `chown` |
| `secrets_host.canary_failed` | the provider could not wrap and unwrap its canary | the file is damaged: restore it |
| `secrets_host.key_version_missing` | stored values are sealed under a version the file lacks | restore the newest copy of the key file |
| `secrets_host.rotation_running` | the lock exists, or a rotation is unfinished | wait, or `--resume`; remove the lock only if no rotation runs |
| `secrets_host.rewrap_failed` | values could not be moved | `status`, the secret audit journal, then `--resume` |
| `secrets_host.retire_refused` | values are still under that version | finish the rotation |
| `secrets_host.retire_unconfirmed` | `--apply` without `--confirm-all-servers-reloaded` | check the servers, add the flag |
| `secrets_host.audit_unavailable` | the identity audit chain cannot take the record | `spot migrate --apply`; check the database |
