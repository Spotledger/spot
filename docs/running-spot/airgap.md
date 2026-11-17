# Air-gapped operation: `spot airgap`

A server with no route to the outside is installed and upgraded from signed files on removable media. Nothing is trusted because of where it came from; only a signature that chains to a root pinned on the server is.

## Commands

| Command | Where | What |
|---|---|---|
| `spot airgap export --out DIR --what runtime,agent,packs,mirror --source DIR --version V --key FILE --certificate FILE` | connected machine | builds one signed bundle per kind |
| `spot airgap verify BUNDLE [--trust FILE --revocations FILE]` | air-gapped host | checks signature chain, validity window, revocation list, every file digest, and that nothing unlisted is present |
| `spot airgap import BUNDLE --repo DIR [...]` | air-gapped host | verifies, refuses a downgrade or a changed digest, stages, commits by one rename |
| `spot airgap list --repo DIR` | air-gapped host | what the repository holds |

`BUNDLE` is one bundle directory (it holds `offline.toml`) or the export directory; every bundle in it is handled.

## Source layout and output

`--source` holds `runtime/`, `agent/`, `packs/<name>/` (one bundle per pack) and `mirror/` (an OS package mirror prepared with the OS tools). `--what` accepts only those four words. The export directory must not exist; it is written as `<out>.partial` and renamed. Output: `<out>/runtime`, `<out>/agent`, `<out>/packs/<name>`, `<out>/mirror`, each with `offline.toml`, `offline.sig`, `certificate.json` and `files/`.

## Keys and trust

- Signing key: a file with the base64 of the 32-byte seed, mode 0600 (a looser mode is refused). It is read once and never printed or copied. `--certificate` is the publisher certificate for that key.
- Trust on the air-gapped host: `--trust` (one base64 root public key per line) and `--revocations` (the signed list, JSON), or `[trust] roots` and `[trust] revocations` in `common.toml`. Without them every verify and import refuses. There is no flag that skips the signature.
- The newest revocation generation seen is remembered in `<repo>/revocation.generation`; an older list is refused.

## What is refused (stable `airgap.*` codes in the message)

unsigned (`airgap.unsigned`), unknown or expired root or certificate, revoked publisher or key, tampered file (`airgap.file_size`, `airgap.file_digest`), unlisted file (`airgap.extra_file`), unsafe manifest path with `..`, an absolute path or a backslash (`airgap.manifest`), any symbolic link in a bundle, release directory or repository (never followed), downgrade (`airgap.downgrade`), changed digest at the same version, stale revocation list. A release directory holding a private key is refused at export. Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3 failed.

## Atomic import

Files are staged under `<repo>/staging/...partial` and the directory is renamed into `<repo>/bundles/<kind>/<name>/<version>` only after every file verified; the index is written last. After a power cut the repository still holds the previous version, and the next import resumes the staging directory.

## The agent's offline update

An agent bundle's files are `update.json` (the offer the gateway would answer: `manifest`, `proof`, optional `revocations`) and the payload archive named in the manifest. After import, point the agent at `<repo>/bundles/agent/spot-agent/<version>/` (`spot_agent::update::DirSource`): it applies the same signature, window, revocation and archive checks as the network path, and never installs an older version.
