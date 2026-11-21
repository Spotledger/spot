# tools/component-shell

Builds a browser shell around one tenant's **approved, signed** tier B component set, and writes a
content-addressed manifest of that set plus a record of the trust material the build used.

Tier B on the web is not dynamic loading. The tenant chooses a component set, the store's registry
signs each package, and the shell is generated with a **static** reference to every component it
ships. This tool is the build step: it verifies each package's signature before anything is copied,
refuses a package it cannot vouch for, and records what it bundled.

```bash
python tools/component-shell/build-shell.py --components tenant-set.toml \
    --root-key root-key.json --anchor-set anchor-set.json \
    --revocations revocations.json --as-of 2026-10-26T00:00:00Z \
    --min-anchors-generation 3 --min-revocation-generation 12
```

## Where trust comes from

The set file is *not* a trust source. Until a recorded security finding it carried `[trust.keys]`, and a build host with
write access to that file could name its own key and bundle a package the store itself would refuse.
Trust now comes from material the store signed, each document verified against the one before it:

| Argument | What it is |
| --- | --- |
| `--root-key FILE` | one root's public key, trusted out of band — the only file an operator vouches for |
| `--anchor-set FILE` | the registry's **root-signed** set: trusted roots with their windows, and the publisher certificates those roots issued |
| `--revocations FILE` | the store's **root-signed** revocation list (`spot-revocation-list`, the document the desktop host parses) |
| `--min-anchors-generation N` | the lowest anchor-set `generation` this build accepts; required, a non-negative integer, no default and no environment fallback |
| `--min-revocation-generation N` | the lowest revocation-list `generation` this build accepts; same rules |
| `--as-of INSTANT` | the instant every window and every `effective_from` is judged at (UTC seconds, or ISO-8601 such as `2026-10-26T00:00:00Z`); required, because a build reads no clock |

A package is bundled only if

1. its signing key is named by a certificate in the verified anchor set,
2. that certificate names the package's publisher and its window covers `--as-of`,
3. neither the publisher nor the key is revoked as of `--as-of` (a `compromised` key is refused
   whenever it signed), and the package's digest is not on the list,
4. its signature verifies over the manifest's canonical bytes with the certificate's key,

and the set file lists it. Anything else is refused by code and nothing is written.

The revocation list is judged by **effective dates at `--as-of`**: this is the crate's model
(`spot-bundle-signing`, where a signature made before a revocation stays valid), so a rebuild of an
old set is judged by the date the set was reviewed. The desktop host at install time is stricter and
refuses anything the list *it* holds names, so pass the `--as-of` a build actually ran at.

### Pinning the generations (replay)

A signature proves who signed a document, not that it is the latest. Someone who controls the build
host's files can supply a genuine, older, correctly signed revocation list (from before a key or
package was revoked) with the matching older anchor set. Both documents carry a monotonic
`generation`, and the build refuses one below its pin: `component.anchors.stale` and
`component.revocation.stale`, each naming the document's number and the pin. The signature is verified
first, so an edited number fails as a bad signature, not as a stale one.

The pins come **only from the command line**, which the release process records. They are never read
from the set file, the documents or the environment, and omitting one is a usage error (exit 2), not
"accept anything". A release process takes each pin from the newest list it has seen and **raises it on
every release, never lowers it** (for example in the release checklist; a later task can read them
from a signed release record). `--reviewed-set` mode trusts a reviewed digest, not a signed list: it
neither needs nor accepts the pins (passing one there is a usage error), so they cannot be used to
weaken it. The trust record names the generations used and the pins (`anchor_set_generation`,
`revocation_generation`, `min_anchors_generation`, `min_revocation_generation`).

### The anchor set

The registry never published a signed list of *which* roots are current, so this tool defines one.
It is signed by the root named in `--root-key` with the same encoding as the rest of the signing
scheme — a domain prefix and canonical JSON, so a store-side issuer written against
`crates/framework/spot-bundle-signing/` produces bytes this tool accepts:

```json
{
  "format": "spot-component-anchor-set",
  "version": 1,
  "generation": 3,
  "issued_at": 1759000000,
  "roots": [
    {"key_id": "<32 hex>", "public_key": "<base64 32 bytes>",
     "valid_from": 1750000000, "valid_to": 1790000000}
  ],
  "certificates": [
    {"format": "spot-publisher-certificate", "version": 1,
     "publisher_id": "acme", "display_name": "Acme Components",
     "public_key": "<base64 32 bytes>",
     "valid_from": 1750000000, "valid_to": 1790000000,
     "issued_by": "<root key id>", "serial": 1, "signature": "<base64 64 bytes>"}
  ],
  "signature": "<base64 64 bytes, by the root named in --root-key>"
}
```

The signed bytes are `SPOT-COMPONENT-ANCHOR-SET-V1\n` followed by the canonical JSON of
`{certificates, generation, issued_at, roots}` — every certificate's identity fields included, so a
build host cannot swap in a key the store did not certify without breaking the set's signature. A
root's `valid_to` of `null` means no end.

## The set file

```toml
[shell]
name = "acme-shell"                    # the generated assembly's name and the manifest's shell name

[[component]]
package = "components/acme.rating-1.0.0"   # relative to this file
```

A `[trust]` table here is refused in the default mode: silently ignoring it would leave whoever wrote
it believing it is a trust source.

Both tables are closed. `[shell]` carries `name` and nothing else, and `[trust]` carries `keys`,
`revoked_keys` and `revoked_publishers` and nothing else. So a `min_revocation_generation` written
into a set file is a refusal naming the key — not a pin, because a number in a file the build host
can edit pins nothing.

## The reviewed-set fallback

For a set reviewed before the store published signed material:

```bash
python tools/component-shell/build-shell.py --components reviewed-set.toml --reviewed-set
```

`--reviewed-set [FILE]` keeps the old behaviour — the keys and revoked ids in the set file's
`[trust]` table — but only when the set file's SHA-256 is recorded in FILE, which defaults to
`tools/component-shell/reviewed-sets.txt` (one digest per line, `#` comments). That file ships empty
on purpose. The mode prints a loud `WARNING: reviewed-set mode` notice, because what it trusts is a
file an editor could have written, and the printed summary says `REVIEWED SET (unverified)`.

## What it writes

```
publish/
  Shell.csproj                  one <Reference> per component, plus the component host project
  Program.cs                    the shell's start-up check
  Components.g.cs               the component list compiled into the shell
  component-set-<hash>.json     the manifest, named after the SHA-256 of its own bytes
  component-trust-<hash>.json   the build's trust record: what was bundled, and the material used
  components/<id>/<version>/    each package, exactly as it was signed
```

`Program.cs` reads `component-set-<hash>.json` through the host's `ComponentSetManifestReader`,
refuses a manifest whose bytes do not hash to its name, compares the compiled-in component list with
the manifest (id, version, signed-manifest digest), re-hashes each bundled assembly against the digest
the publisher signed, and exits non-zero on any difference. That is the runtime check: **the loaded
component list equals the manifest.**

`component-trust-<hash>.json` is this tool's own record, content-addressed like the manifest: it names
the mode, `--as-of`, each bundled component's signed-manifest digest, and either the anchor-set and
revocation-list digests plus the root key id (signed mode) or the reviewed set's digest and the
reviewed list (fallback), with one `trust_sha256` over the material as a whole. It is what an audit
reads to answer "what did this build trust, and when".

The generated project is emitted, not compiled, by this tool: building it needs the Avalonia browser
workload and a browser-side host, which WP-UI-14.T2 does not own. The C# side of the check
(`ComponentSetManifest`, `ComponentSetManifestReader`) is covered by the host's own tests, and the
generated files use nothing but that public surface.

## Refusals

A refusal names a stable code and stops the build before anything is written. The `component.*` codes
below are the desktop host's (`ComponentRefusalCodes`); `component.trust.*` are this tool's own,
because the host is handed its trust roots and never parses these documents.

| Code | What it means |
| --- | --- |
| `component.package.manifest_missing` | no `component.json` |
| `component.manifest.invalid` | the manifest is not the documented closed object |
| `component.package.signature_missing` | no `component.sig`: an unsigned package is refused |
| `component.signature.malformed` | the signature is not standard base64 of 64 bytes |
| `component.package.hash_mismatch` | a file's bytes changed after the publisher signed |
| `component.package.file_missing` | a listed file is not in the package |
| `component.package.path_invalid` | a listed path is absolute, escapes the package, or is not a path |
| `component.signature.unknown_key` | the signing key is not certified by the anchor set |
| `component.publisher.mismatch` | the manifest's publisher is not the certificate's |
| `component.publisher.revoked` | the store revoked the publisher as of `--as-of` |
| `component.key.revoked` | the store revoked the key as of `--as-of` |
| `component.key.compromised` | the key is marked compromised: every signature it made is refused |
| `component.package.revoked` | the package's digest is on the revocation list |
| `component.certificate.expired` | the certificate's window ended before `--as-of` |
| `component.certificate.not_yet_valid` | the certificate's window starts after `--as-of` |
| `component.certificate.unknown_root` | the certificate's issuing root is not in the anchor set |
| `component.certificate.signature_invalid` | the certificate does not verify under the root that issued it |
| `component.revocation.signature_invalid` | the revocation list does not verify under the root it names |
| `component.signature.invalid` | the signature does not verify over the canonical bytes |
| `component.set.duplicate` | the set lists one component id twice |
| `component.trust.anchor_set_malformed` | the anchor set is not the documented closed document |
| `component.trust.anchor_set_signature_invalid` | the anchor set does not verify under `--root-key` |
| `component.trust.root_key_malformed` | the root key file is not `{format, version, key_id, public_key}` |
| `component.trust.revocation_malformed` | the revocation list is not the documented closed document |
| `component.trust.unknown_root` | a listed root / the revocation list's signer is not in the anchor set |
| `component.trust.root_outside_window` | the revocation list's signing root is not trusted at `--as-of` |
| `component.anchors.stale` | the anchor set's generation is below `--min-anchors-generation` |
| `component.revocation.stale` | the revocation list's generation is below `--min-revocation-generation` |
| `component.trust.set_carries_keys` | the set file names trust material in the default mode |
| `component.trust.reviewed_set_not_recorded` | the set file's digest is not in the reviewed list |
| `component.trust.reviewed_list_malformed` | a reviewed-sets line is not a SHA-256 |
| `component.trust.instant_invalid` | `--as-of` is not UTC seconds or an ISO-8601 instant with an offset |
| `component.trust.unreadable` | a trust document could not be read |

## Why the primitives are here

The repository allows no new Python dependencies, so canonical JSON (`component_shell/canonical.py`)
and Ed25519 (`component_shell/ed25519.py`) are in this folder. Both are checked against bytes another
implementation produced:

* the canonical encoder against the `signed_bytes_hex` of the Rust-generated vectors in
  `clients/avalonia/tests/Spotledger.Component.Host.Tests/Vectors/component-signing-vectors.json`
  (manifest, certificate and revocation list);
* Ed25519 against the RFC 8032 test vectors, and against the key ids in the same vector file.

`component_shell/trust.py` reads the two documents the store already signs (`spot-publisher-certificate`
inside the anchor set, `spot-revocation-list`) with those same bytes, so a certificate or list the
Rust crate produced verifies here unchanged. The anchor set is the one document this tool defines (see
above); its tests pin the encoding, and its signature is the only thing that makes its contents
believable.

The tool's tests also read the committed set manifest
(`Vectors/component-set-eadd8795b07d255e.json`) that the C# host's tests read, so the two languages
agree on one file rather than on two opinions of one file.

## Tests

```bash
python3 -m unittest discover -s tools/component-shell
python3 -m unittest discover -s tools/component-shell/tests
```

Both find the same suite (`tests/` is a package, so the tool directory is a valid discovery root).
Every root, publisher and revocation fixture is generated in the test; no key is committed.

`tools/ci/run_python_suites.py` discovers this suite automatically, so CI runs it without a workflow
change.
