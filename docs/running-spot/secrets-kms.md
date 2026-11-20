Part of [Running Spot](../running-spot.md).

See also [the secrets service](secrets.md).

# Customer-managed keys: the secrets service with your own KMS

**Revoking the KMS key (or the site's token) stops the site from unsealing anything once its
cached data keys expire: at most `cache_ttl_seconds` later, 5 minutes by default.** Until then
values that were unsealed recently keep opening from memory; after it every read of a sealed value
is refused with a stable code and no plaintext fallback exists. `spot secrets-kms flush-cache`
makes the refusal immediate on a running server (within a few seconds). New values cannot be
sealed from the moment the KMS refuses. Nothing is deleted: when the key is enabled again the site
reads its values again.

## What it is

With `provider = "kms"` the key that protects every `secret` field lives in your KMS (HashiCorp
Vault Transit or a compatible service) and never reaches the host. The host keeps a short-lived
cache of unwrapped data keys in memory, and one small file, the **seed file**, from which blind
indexes (the `lookup` of a `secret` field) are derived, because a KMS never reveals a key to
derive from. The provider is `spot-secrets-kms`; this page is how `spot serve` hosts it.

## Configuration

```toml
[services.secrets]
provider = "kms"

[services.secrets.kms]
address = "https://kms.example.com:8200"        # https only
mount = "transit"                                # default
key_name = "spot-acme"                           # the key in the mount
token_file = "/etc/spot/kms.token"               # absolute, mode 0600
seed_file = "/var/lib/spot-keys/{site}.seed.json" # absolute, mode 0600, outside the site folders
cache_ttl_seconds = 300                          # default: the revocation latency, 1..86400
refresh_interval_seconds = 60                    # default: how often key versions are re-read
```

Unknown keys are refused. `{site}` in `seed_file` is the site's name: with more than one site it
is required, so each site has its own seed. Neither file may lie inside `<root>/sites` (a copy of
a site would carry it).

First start of a site, once, with the KMS key and token in place:

```
spot secrets-kms init --site acme            # prints the plan
spot secrets-kms init --site acme --apply    # creates the seed file under the KMS key
```

The server never makes a seed: a server that quietly made a new one would change every blind
index. `init` refuses when the file exists.

## The token file

The token is read from `token_file`, never from an environment variable or an argument. The file
must be a regular file (a symbolic link is refused), not empty, and grant nothing to group or
others (`chmod 600`). A missing, empty or open file refuses to start, with a message that names
the file and never the content. The token needs `encrypt` and `decrypt` on the key and `read` on
the key's path; nothing else. A token file replaced with a new token is picked up at the next
probe (within `refresh_interval_seconds`); if the new file cannot be read the old token stays in
use.

## The egress allow entry

The KMS is reached only through the platform's egress guard, as the site's tenant and the
`connector` purpose. Add the KMS to the egress policy of the host:

```toml
[[tenants.acme.allow]]
scheme = "https"
host = "kms.example.com"
ports = [8200]
purpose = "connector"     # an entry that names a purpose must name `connector`
# allow_private = ["10.0.0.5"]   # when the KMS sits on a private address
```

When the entry is missing the site refuses to start and the message prints the entry to add
(`secrets_kms_host.egress_not_allowed`). Nothing else is reachable by this provider.

## At start and while running

At start the service reads the key, checks it is enabled, wraps and unwraps a canary under the
current version and derives the blind-index key. Any failure stops the site (the service is
required), naming the key and the cause code.

After start the site does **not** stop when the KMS is away. Readiness shows `secrets_kms` as
degraded with the seconds left before refusals begin, while cached data keys still serve; after
that, reads and writes of sealed values are refused and counted. The refusals are the metric
`spot_secrets_kms_refused_total{reason}`, reason one of `unreachable`, `rejected`,
`egress_refused`, `response_invalid`, `key_disabled`, `version_retired`, `tampered`,
`unknown_key`, `other`.

```
spot secrets-kms status --site acme
```

prints what the running server last reported: key name, latest and minimum-decryption versions,
cache size and age, the seed file's presence, health, the last KMS error code and refusals by
reason. It reports a status file the server keeps next to the seed file
(`<seed_file>.status.json`: counts, versions and codes only).

## Rotation

Order matters:

1. Rotate the key in the KMS (a new transit version). The server follows within
   `refresh_interval_seconds`.
2. `spot secrets-kms rewrap-seed --site acme --apply` moves the seed file's wrapping to the new
   version.
3. `spot secrets rotate-kek --site acme --apply` runs the existing sweep: every stored data key is
   re-wrapped under the latest version (decrypt and encrypt through the KMS; the values
   themselves are not touched) while the site serves. `status` shows the stored values still on an
   old version; it must reach zero.
4. Only then raise `min_decryption_version` in the KMS. A value still on a version below it is
   refused with `secrets_kms.version_retired`.

Never do step 4 first.

## The seed file: back it up

**A lost seed file loses every blind index** (lookups by a `secret` field find nothing, and unique
constraints on it stop holding). The sealed values themselves are not lost. The file is useless
without the KMS key, so it may be backed up with the site's other state. Copy it after `init` and
after every `rewrap-seed`, apart from the data. Restore the newest copy; an older copy still works
until the version it was wrapped under is retired in the KMS.

## What an operator must never do

* Put the token in the configuration, an environment variable or a command line.
* Delete or disable the KMS key, or raise `min_decryption_version`, before the sweep has moved
  every value (every sealed value under the old version becomes unreadable).
* Make a new seed to replace a lost one, or delete the seed file to "reset" it.
* Share one seed file between sites, or put a seed or token file inside a site folder.
* Lower the cache lifetime to nothing to get instant revocation: use `flush-cache` instead. A
  short lifetime sends every unseal to the KMS.
* Read the status or logs for a key: there is none in them, and no tool prints one.

## Codes

| Code | Meaning |
|------|---------|
| `secrets_kms_host.settings_invalid` | the table is missing, has an unknown key or a bad value |
| `secrets_kms_host.path_refused` | a file path is relative, has `..`, lies in the site folders or is shared |
| `secrets_kms_host.file_missing` / `file_unusable` / `file_unsafe` | the token or seed file is missing, not a usable regular file, or open to group or others |
| `secrets_kms_host.egress_not_allowed` | the egress policy has no entry for the KMS; the message prints it |
| `secrets_kms_host.start_failed` / `canary_failed` | the key cannot be read or is disabled; the start-up check failed |
| `secrets_kms_host.seed_exists` | `init` will not overwrite a seed file |
| `secrets_kms_host.rewrap_failed` | the seed file was not moved |

The provider's own refusals (`secrets_kms.unreachable`, `secrets_kms.rejected`,
`secrets_kms.key_disabled`, `secrets_kms.version_retired`, ...) are in the `spot-secrets-kms`
README.
