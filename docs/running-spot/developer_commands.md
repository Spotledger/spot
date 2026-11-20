Part of [Running Spot](../running-spot.md).

# Developer commands

An app developer needs `spot` and nothing else. These commands create, check, pack and inspect an app from the one binary; they forward to the scaffolder (`spot-scaffold`) and the packer (`spot-pack`) with the same arguments, exit codes and output.

They need **no `common.toml`, no site and no database**, they run no other process and they never touch the network. They work in any directory.

| Command | Does |
| --- | --- |
| `spot app new NAME` | Creates a project: the packable app under `app/`, a README, a `.gitignore` and a generated load test. |
| `spot doctype add APP LABEL` | Adds a DocType, a starter grant and a test to an app. |
| `spot controller add APP DOCTYPE` | Adds a native controller skeleton and its manifest rows. |
| `spot script add APP NAME` | Adds a TypeScript route handler and its manifest rows. |
| `spot app check DIR` | Checks an app directory and reports every problem at once. |
| `spot app pack DIR --out FILE` | Packs a checked app into one deterministic bundle (and signs it with `--sign`). |
| `spot app verify FILE` | Verifies a bundle against its signature, certificate, trust roots and revocations. |
| `spot app inspect FILE` | Shows a bundle: manifest identity, digest, entries, capabilities, publisher. |

`spot app new --help` lists every flag (`--title`, `--version`, `--author`, `--licence`, `--with-controller`, `--with-script`, `--example-fields`, `--out`, `--dry-run`, `--force`, `--json`). The scaffolding flags are the scaffolder's own, so a flag added there appears here.

## The five-minute path

```
$ spot app new demo
wrote 9 files (6120 bytes)
  .gitignore
  README.md
  app/app.wasm
  app/doctypes/note.toml
  app/fixtures/README.md
  app/app.toml
  app/permissions/grants.toml
  tests/load.rs
now run `spot app check app`

$ spot doctype add demo "Sales Invoice"
wrote 3 files (1480 bytes)
  ...

$ spot app check app
ok: the app checks clean (0 problems)

$ spot install-app demo --site mysite --apply     # on a development site, see "Installing" below

$ spot app pack app --out demo.bundle
packed demo.bundle (3f9a...)
```

(The file counts and digest above are examples; run the commands to see yours.)

Add `--dry-run` to any scaffolding command to see the tree and the byte count without writing anything; `--json` prints the same as one JSON object.

### Installing

`spot install-app` reads an app **directory** with `app.toml` at its root and a `doctypes/` directory, listed in `common.toml` `apps`, or a **bundle** for a site that requires signed apps. `spot app new` writes exactly that layout (`app/app.toml`, `app/doctypes/`, `app/permissions/`, plus the component and fixtures), so the generated `app/` directory installs without an edit: list it in `common.toml` `apps` (or copy it to where your apps live) and run `spot install-app APP --site SITE --apply`. Installing is a separate step from these commands and is documented with `spot install-app`.

The packer (`spot-pack`) and the bundle know the manifest as `manifest.toml`. `spot app check` and `spot app pack` therefore read a directory that has `app.toml` through a private, temporary copy in which the file has that name; the app directory itself is never changed and the bundle carries `manifest.toml` as before. A directory that has both files is refused (`pack.layout`): keep `app.toml`. A symlink anywhere in the directory is refused (`pack.path_escape`), as before. Messages from the packer name the file `manifest.toml`; it is your `app.toml`.

`spot doctype add`, `spot controller add` and `spot script add` edit the same files: they leave the manifest named `app.toml`. They replace existing files (the grants file, `app.toml`, `controllers.toml`), so they need `--force`, as every replacement does. If `app.toml` has a host-level `controller = "native:..."` key under `[app]`, `controller add` and `script add` refuse (they cannot rewrite the file without dropping that key): add the row by hand.

`spot app new` refuses when `<out>/app` already exists and holds files, unless `--force` is given.

## Where generated files go

Everything goes under `--out` (default: the current directory): the app in `<out>/app`, the README, `.gitignore` and `tests/load.rs` beside it, a native controller crate in `<out>/Cargo.toml` and `<out>/src/` and scripts in `<out>/app/scripts/`.

## Exit codes

The `spot` binary's numbers apply (0 done, 1 usage, 2 refused, 3 failed). The scaffolder and the packer have numbers of their own; this is how they map.

| Source | Meaning | `spot` exit |
| --- | --- | --- |
| scaffold 0, pack 0 | done | 0 |
| scaffold 2 (`scaffold.usage`); pack 2 for a wrong argument (`check` on something that is not a directory, `verify --mode` with an unknown mode) | usage | 1 |
| scaffold 3 for any code except `scaffold.io` (an invalid or reserved name, a path that escapes, a symlink, a file that exists without `--force`, a duplicate, not an app, unknown DocType); pack 1 (check problems, a refused signature); an unsigned bundle without `--allow-unsigned-development`; a key file that is too open | refused, nothing changed | 2 |
| `scaffold.io`; pack 2 for a file that cannot be read or written | failed | 3 |

`clap`'s own parse errors (a missing argument, an unknown flag) exit 2, as for every command of `spot`.

`--json` output is the library's own text. For `app new` the keys are the scaffolder's (`files`, `bytes`, `next`); `next` names the `spot` command to run instead of `spot-pack`.

## Safety

- **Output paths** (`--out` of the scaffolding commands, `--out` of `app pack`) are relative to the current directory, contain no `..` and no symlink on the path (the last component included). An absolute path or a symlink is refused (exit 2) and nothing is written.
- **Nothing is replaced without `--force`.** A scaffold validates every file before it writes the first; a refusal leaves no file half-written. `app pack` checks the bundle and the signature file the same way.
- **A signing key** (`app pack --sign KEYFILE --certificate CERT`) is a file of 32 seed bytes in base64. It must be a regular file that only its owner can read: on Unix mode 0600 or tighter, otherwise the command refuses before it packs. The key is never printed, in output or in an error. On Windows the mode check is not made: keep the key in a directory only your account can read (an owner-only ACL, for example under your profile) and never in a shared folder.
- **Time** is never read by the command itself. `--signed-at` and `--now` take UTC seconds; when omitted they are the clock's present time.
- **Unsigned bundles.** `app verify` refuses a bundle without `--signature`. `--allow-unsigned-development` opens it anyway and says it is UNSIGNED: it proves nothing about who made it. Use it only on a development machine.

## Signing and verifying

```
spot app pack app --out demo.bundle --sign publisher.key --certificate publisher.cert
spot app verify demo.bundle --signature app.sig --certificate publisher.cert \
     --trust roots.txt --revocations revocations.json
```

`pack --sign` writes `app.sig` next to the bundle (the name the installer looks for). `roots.txt` has one root public key per line (base64, `#` comments allowed).
