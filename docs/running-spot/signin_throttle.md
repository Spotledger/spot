Part of [Running Spot](../running-spot.md).

# Sign-in throttle (`[services.signin_throttle]`)

Every credential door of a site is admitted through one shared throttle before the name or the password is looked at. The counters are rows of the site database (the coordinator tables), so they are shared by every node and survive a restart.

```toml
[services.secrets]
kek_file = "keys/acme.json"

[services.signin_throttle]
per_account     = { capacity = 10,   per_seconds = 900 }   # one account name, per kind of attempt
per_source      = { capacity = 30,   per_seconds = 900 }   # one address (or one connection)
per_destination = { capacity = 10,   per_seconds = 3600 }  # one e-mail or phone a code goes to
per_tenant      = { capacity = 6000, per_seconds = 60 }    # ceiling that protects the database
salt_version          = 1          # bump to rotate
# previous_salt_version = 1   # the old version, for one full longest window after a bump
```

(`previous_salt_version` defaults to none.) Unknown keys are refused. The per-tenant rate must be at least as fast as the per-source rate.

Level: **required**. If the salt cannot be derived or the coordinator tables cannot be prepared, the site refuses to start; if the salt can no longer be derived while running, the site is reported stopped and nobody is admitted.

## What is counted

Each attempt is charged one unit to the source, then (when the attempt names one) the destination, the account and the tenant, in that order; the first bucket that is full refuses it. The account bucket is a **rate that refills**, not a lock: it slows a name that is being guessed, and it counts a name that belongs to nobody exactly like a real one, so the answer is no oracle for which accounts exist. It does not replace the durable lockout on the user record, which an operator can read.

Buckets are named by a keyed digest of the tenant, the dimension and the value: no account name, address or e-mail is stored. The key is derived from the secrets service's key (`derive_key`), per tenant and per salt version, so every node derives the same salts and none is stored.

## Rotating the salt

Set `salt_version = N+1` and `previous_salt_version = N`, reload every node, and leave both for one full longest window (an hour with the defaults). While both are set, an attempt is checked against the old buckets as well, so a rotation never resets a bucket that was already full. Then remove `previous_salt_version`.

## When the coordinator cannot be reached

Nobody is admitted. The sign-in answers `503` with `login.admission_unavailable` and `Retry-After`; the other doors (API key, SCIM, recovery code, SSO, password-change re-authentication) answer `503 server.unavailable` with `Retry-After`. Each refusal is counted under the door only (`admitted`, `throttled`, `unavailable`); no metric carries an account or an address.

## Adding a door

A door is anywhere a credential is checked. Add it to `DOORS` in `src/services/signin_throttle.rs`; a test scans `spot-cli/src` and `spot-login/src` for `Authenticator::` and `verify_password(` and fails when a file that makes those calls is not listed.
