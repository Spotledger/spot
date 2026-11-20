# spot-secrets-kms

Customer-managed keys for `spot-secrets` (WP-SEC-03.T13). `KmsKeyProvider` implements
`spot_secrets::KeyProvider` with the key-encryption key held in the customer's KMS, so that
**disabling the key in the KMS stops the site from unsealing anything**, bounded by the cache
lifetime (default 5 minutes).

The KMS is reached through the `KmsClient` trait. The one implementation here, `TransitClient`,
speaks the HashiCorp Vault Transit API (`encrypt`, `decrypt`, key read) over a
`spot_egress::Sender`; the caller builds the `Sender` over the platform's egress `Guard` and
passes a `GuardGate` for the tenant, so every call is subject to the egress policy. This crate
never creates an HTTP client. AWS KMS, Azure Key Vault, GCP KMS and PKCS#11 are further
`KmsClient` implementations for later (they need their own request signing).

## Threat model

**What the customer controls.** The key-encryption key never leaves their KMS. Disabling the key
(or revoking the site's token, or raising `min_decryption_version`) ends the site's ability to
unwrap data keys. The documented revocation latency is `cache_ttl` (default 300 s): an unwrapped
data key is cached in memory for at most that long, and when the KMS cannot be reached or
refuses, a cached key younger than the lifetime is still served and an older one is refused.
`KmsKeyProvider::flush_cache()` drops the cache at once (an operator action).

**What stays local.** Blind-index and derived keys cannot come from a KEK that a remote KMS never
reveals. They come from a 32-byte *derivation seed*, generated once through the injected
`Entropy`, stored wrapped under the KMS key in a small JSON envelope (`seed_file`, mode `0600`,
written atomically through a `create_new` temp file, fsynced, directory synced), unwrapped at
start and held only in zeroising memory. The seed is wrapped with an AAD that names the key id,
so an envelope from another key's site does not unwrap. `index_key` and `derive_key` are
HKDF-SHA256 over the seed with a per-key-id salt and the same `info` conventions and bounds as
`spot-secrets`.

The seed belongs to the key *id* (`kms:<key_name>`), **not** to a transit version: a transit key
rotation changes the wrapping key, never the seed, so blind indexes and derived keys are the same
before and after a rotation and across restarts (tests prove both). Losing the seed file loses
every blind index and derived key: back it up with the site's other state (it is useless without
the KMS key).

**What a compromised host can still do.** Use the KMS for as long as it holds a valid token:
unwrap any data key it has a ciphertext for, and read the seed. The token file is the
credential; it must be a regular file (a symlink is refused) with no group or other permission
bits, is never read from an environment variable or an argument, is held in zeroising memory and
is in no `Debug`, error or log. Ownership of the file is not checked (std cannot read the
process uid without `unsafe`); the host's start-up check owns that. There is no plaintext
fallback anywhere: a failed unwrap is a refusal.

## Failure behaviour

Every refusal is a `SecretError::Provider` whose text names the key name and a `secrets_kms.*`
code (catalogue: `spot-errors/catalogue/secrets_kms.toml`), or `SecretError::Tampered` when the
KMS reports an authentication failure (wrong aad, altered ciphertext). Texts are fixed sentences:
never the token, a ciphertext, a data key, the seed, an address or a response body (a non-2xx
body is dropped unread). Responses are bounded at 64 KiB.

| Code | Meaning |
|------|---------|
| `secrets_kms.config_invalid` | address (https only), mount, key name, namespace, timeout or cache bounds |
| `secrets_kms.token_unreadable` | token file missing, not a regular file, too open, empty, odd |
| `secrets_kms.egress_refused` | the egress policy refused the KMS address |
| `secrets_kms.unreachable` | no answer in time, or connection failure |
| `secrets_kms.rejected` | the KMS answered a non-success status (token revoked, key disabled) |
| `secrets_kms.response_invalid` | an answer in a shape this client does not accept |
| `secrets_kms.key_disabled` | the key reads as disabled |
| `secrets_kms.version_retired` | the wrap's key version is below `min_decryption_version` |
| `secrets_kms.input_invalid` | a value outside the accepted bounds |
| `secrets_kms.seed_invalid` | the seed file is corrupt, foreign or open to group or other |
| `secrets_kms.seed_io` | the seed file cannot be read or written |

## Rotation

1. Rotate the key in the KMS (a new transit version). `current()` follows at the next
   `refresh_interval` (default 60 s) or at the first wrap.
2. Run the host's re-wrap sweep (WP-SEC-03.T9 `rotate-kek`): it calls
   `KmsKeyProvider::rewrap(&WrappedKey, aad)` per stored data key (decrypt, then encrypt under
   the latest version) and `rewrap_seed()` once for the seed envelope.
3. Only then raise `min_decryption_version` in the KMS. A wrap below it is refused with
   `secrets_kms.version_retired`.

## Example configuration

```rust,ignore
let transit = TransitConfig {
    address: "https://kms.customer.example:8200".into(),
    mount: "transit".into(),
    key_name: "spot-site-acme".into(),
    token_file: "/etc/spot/kms.token".into(),   // 0600, a regular file
    namespace: None,                             // or Some("admin/team-a".into())
    request_timeout: Duration::from_secs(5),
};
let client = TransitClient::open(transit, sender, Arc::new(GuardGate::new(guard, "acme"))).await?;
let provider = KmsKeyProvider::open(
    Arc::new(client),
    ProviderConfig::new("/var/lib/spot/kms-seed.json"),  // cache_ttl 300 s, refresh 60 s
    clock,
    Arc::new(OsEntropy),
).await?;
```

The tenant's egress policy needs an allow entry for the KMS host (`scheme = "https"`, the KMS
port); a private address is refused unless the entry says `allow_private`.

## Tests

`tests/` runs the `spot-secrets` provider contract (round trip, wrong aad, tamper, rotation,
derived keys) against `KmsKeyProvider` over `FakeTransit` (`tests/support/`), an in-process
`Sender` that answers Transit's paths and JSON with a toy authenticated cipher. It opens no
socket: the fake replaces the HTTP layer, so the egress guard is exercised separately with a real
`Guard` over a scripted resolver (`tests/transit.rs`).
