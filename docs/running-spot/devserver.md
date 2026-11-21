Part of [Running Spot](../running-spot.md).

# Hot reload for development: `[services.devserver]`

On a developer's machine, the host can watch the app directories the site was built from and
apply a change **without a restart**, then tell connected clients. It is the host side of the
`spot-devserver` library: the library decides what a change is, in which order the steps run and
how long to wait for an editor to finish saving; this module gives it the real site.

It is **off by default** and for loopback only. A site that does not set `enabled = true` registers
nothing: no watcher thread, no listener.

## Starting it

Add the table to `common.toml`, then start the host as usual:

```toml
[services.devserver]
enabled = true
# Optional. The values below are the defaults.
status_addr = "127.0.0.1:7311"
debounce_ms = 200
poll_ms = 250
history = 100
# To rebuild a component on a Rust or `.wasm` change:
# build = "cargo"
# build_args = ["build", "--release", "--target", "wasm32-wasip1"]
# artifact = "target/wasm32-wasip1/release/app.wasm"
```

The module refuses to start (and the site with it) when **any** `server.bind` address, or the
status address, is not loopback. `spot serve --dev` already requires loopback binds; this module
asserts it again because it can change what the site serves.

## What is live and what needs a restart

| You edit | What happens | Restart? |
| --- | --- | --- |
| a DocType TOML (`doctypes/` or `meta/`) | the whole metadata is rebuilt and validated; on success it is swapped into the engine, the permission snapshot and what the API serves. A file that does not build leaves the previous metadata serving and the reload fails with the problems | no |
| a view (`views/`) | parsed and validated against what the engine serves | no |
| a TypeScript script (`ts/`) | transpiled, type-checked against the live declarations and handed to the site's script engine; a call in flight finishes on the old version, the next call uses the new one | no |
| a WASM component (build output) | validated and swapped into the app runtime; a component that fails validation keeps the old one | no |
| a fixture or an asset | noted in the status page; **never reseeded**, a real site's data is not reset | no |
| any other file | shown as ignored | no |
| a **Rust native controller** | needs a rebuild of the host binary | **yes** |
| permission TOML | read when the site is built, not on a reload | yes |

A reload that comes out as `ok = false` does not change what the site serves.

## The status page

`GET /dev/status` on the status address (loopback, read-only) answers JSON: the number of reloads
and failures and the most recent records, each with the step, the paths, `ok`, one sentence, every
error (file, line and message where the loader gives them), the metadata version and the duration.
It carries no file content.

```sh
curl -s http://127.0.0.1:7311/dev/status
```

## `dev.reloaded`

Every record is also published as `dev.reloaded` on the site room of the realtime hub: the step, the
kind, `ok`, the paths, the version, artefact names, the duration and a stable error code. Never a
file's contents (ADR-25: a realtime event is a notification, the client refetches). A client that is
not signed in cannot join the room, so it receives nothing.

## Safety

- Only the directories of apps installed from a local path are watched; the embedded platform app
  has none.
- A symlink that leaves the app directory is **not followed**: the change is ignored and the record
  says so.
- A file that is not a DocType, view, script, component, fixture or asset is ignored and shown as
  ignored.
- The module does not reset or reseed a real site. The library's scratch-site control (create, seed,
  reset, snapshot) stays a tool of `spot dev check`; resetting a database is destructive.

## What the module needs from the host

Script and component reloads go through two small ports that the modules hosting those runtimes
provide (`ScriptRuntime` and `ComponentRuntime`, in `services/devserver/seams.rs`). When no module
provides one, a script or component change **fails visibly** ("this site hosts no TypeScript
engine") instead of being swallowed.
