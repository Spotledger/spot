Part of [Running Spot](../running-spot.md).

# Sign-in stores (`[services.login_stores]`)

Sessions, sign-in lockouts, password verifiers, second factors, recovery codes and API key digests are records of the site database. Every credential is sealed through the secrets service (`[services.secrets]` must be configured). A restart or a reload keeps every session and every lockout, and two nodes on one database share both.

```toml
[services.login_stores]
import_site_file = true   # default; unknown keys are refused
```

Level: **required**. If the stores or the vault do not answer, the site reports `login_stores` stopped and nobody can sign in; if a staged credential cannot be imported, the site refuses to start and names the user id (never the secret).

## What `spot set-password` and `spot create-user` do

They run offline and cannot reach the vault, so they stage the credential in `site.toml`. At the next start or reload (the commands ask a running host to reload) the `login_stores` service writes each chosen-password verifier and API key digest into the sealed fields, one write per user, then removes it from the file, which is replaced atomically with mode 0600. The import is idempotent: if the host stops half-way, the next start writes the same values again. A staged verifier replaces a stored one: the operator meant it to.

After an import `site.toml` holds no chosen-password verifier and no API key digest. It still holds **one-time first sign-in secrets** (`first_sign_in = true`) until the user chooses a password or the secret expires (72 hours by default); a session one of them starts is restricted to changing the password, signing out and reading the session.

## Locked out

A user is locked after repeated failures (five by default; the lock is on the user record, readable by an operator, and survives a restart). Wait for `locked_until`, or reset the Administrator with `spot reset-admin --site <SITE>`, which replaces the Administrator's credentials with a new one-time secret and clears the staged ones. There is no `spot unlock` command yet: clearing another user's lock needs an administrator signed in with the user-management right (the `locked_until` and `failed_attempts` fields are read-only to the interface).

## Two nodes

Both nodes read the same `session` and `user` records. A session ended on one node stops working on the other as soon as its cache entry expires (`cache_ttl`, zero by default in the server). The password-change throttle is still per process.
