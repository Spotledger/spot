# spot-secrets

Envelope encryption for secrets held in site configuration and settings documents (database
password, SMTP/IMAP credentials, S3/Azure keys, webhook signing keys, TOTP seeds). Every other
crate stores only the sealed form (`spsec:v1:<base64url>`); this crate is the one place that
encrypts and decrypts.

## How it works

- `SecretBox::seal` draws a random 256-bit data key per value and encrypts the value with
  XChaCha20-Poly1305. The data key is wrapped by a key-encryption key (KEK) from a `KeyProvider`.
- `Sealed` is self-describing: magic `SPS1`, KEK id, KEK version, wrapped data key, nonce,
  ciphertext.
- The associated data (`aad`) binds a value to where it is stored, for example
  `site=acme;doctype=Email Account;field=password;name=Sales`. A sealed value copied to another
  field fails to open (`secrets.tampered`). It is checked at both the value and the KEK layer.
- `SecretString` / `SecretBytes` are zeroed on drop, print `[redacted]`, and deliberately do
  not implement `Serialize`.

Errors carry stable codes: `secrets.unknown_key`, `secrets.retired_key`, `secrets.tampered`,
`secrets.malformed`, `secrets.provider`. Messages never contain plaintext or key material.

## Providers

| Provider | Use |
| --- | --- |
| `LocalKeyring` | Key file with versioned entries; supports `rotate` and `retire`. |
| `EnvKey` | One KEK from an environment variable (base64, 32 bytes), for containers. No rotation. |
| `MemoryKeyring` | Tests. |

`KeyProvider` is the extension point for a cloud KMS or HSM provider (AWS KMS, Azure Key Vault,
GCP KMS, PKCS#11). It never exposes the KEK: it only wraps and unwraps data keys. None is
implemented here.

## Threat model

Protects against: a stolen database dump or backup **without** the key file (or env key).
Secrets are never in the database or backups in plain form, and logs cannot print them by
accident.

Does not protect against: a compromised running process (it holds the KEKs and plaintext in
memory), someone with both the data and the key file, or an attacker who can call `open` with
the right `aad`. Keep the key file out of backups of the database, and back it up separately.

## Key file placement

Keep it outside the web root and outside database backups, readable only by the service user.

- **Linux:** `/etc/spot/keys/<site>.json` (or `/var/lib/spot/keys/`), directory `0700`.
- **macOS:** `/usr/local/etc/spot/keys/` or the service user's `Library/Application Support/spot/`.
- **Windows:** `%ProgramData%\spot\keys\` in a directory restricted to the service account.

On Unix the file is created `0600` (via a `0600` temp file renamed into place, so it is never
briefly world-readable). **Windows gap:** no ACL is set by this crate; the file inherits the
directory's ACL, so restrict the directory to the service account yourself. Containers should
prefer `EnvKey` fed from the platform's secret store.

## Rotation procedure (no downtime)

1. `keyring.rotate().await` adds version n+1 and makes it current. New values use it; old
   values keep opening.
2. For every sealed value, if `secret_box.needs_rewrap(&sealed)`, call
   `secret_box.rewrap(&sealed, aad).await` and store the result. `rewrap` re-wraps the data key
   under the current KEK **without decrypting the value**.
3. When nothing uses the old version any more (including backups you still want to restore),
   `keyring.retire(old, false).await` deletes its key material. Passing `true` for
   `values_still_use` refuses. The current version cannot be retired. A value still under a
   retired version gives `secrets.retired_key`.

Keep a copy of the key file taken **before** retiring while old backups matter: restoring a
backup whose values are under a retired version needs that key.
