# spot-devserver

The local development server with hot reload (WP-DX-02, ADR-054, ADR-068 §7).

The acceptance sentence of the work package is the design: **code and metadata changes are
visible without restart**, on Fedora and Windows. The runtime already owns the pieces this
crate wires together:

| Change | Step | Reuses |
| --- | --- | --- |
| `meta/*.toml` (and `doctypes/*.toml`) | rebuild the registry, validate, swap | `spot-meta-live` (`LiveMeta::apply`) |
| `views/*.toml` | parse, resolve and validate over the effective metadata | `spot-views` |
| `ts/*.ts` | transpile, type-check against the generated `.d.ts`, swap | `spot-ts-transpile`, `spot-dts` |
| `*.wasm`, Rust sources | run the project build, validate/hash the rebuilt component | `spot-wasm` |
| `fixtures/`, assets | reseed the scratch site, notify the client only | `SiteControl` seam |

## Shape

- [`watch`]: a `Watcher` over an [`EventSource`](watch::EventSource). The platform
  implementation uses `notify` (inotify on Fedora, `ReadDirectoryChangesW` on Windows); a
  `VecEventSource` is the fake the unit tests drive with an injected clock. Events are
  normalised across platforms: path case is folded, an editor's rename-as-write and its atomic
  temp files are handled, and `target/`, `node_modules/` and `.git/` are ignored.
- [`reload`]: the `Reloader` and one `Step` per change class. Every step builds off to the side
  and only swaps in a good result: **a failed metadata or view reload leaves the old version
  serving**. Each step reports its outcome and duration.
- [`site`]: the scratch development site — create, seed from fixtures, reset and
  snapshot/restore — behind the `SiteControl` seam, so the host can delegate database creation
  to `spot-cli` and server lifecycle to `spot-pg-supervisor` without a dependency cycle
  (`spot-cli` depends on this crate, not the other way round).
- [`publish`]: `dev.reloaded` on the `spot-realtime` bus so a running client refreshes the
  affected screen.
- [`status`]: the small `GET /dev/status` endpoint listing the last reloads and errors.
- [`cli`]: the `spot dev` command registration and the `spot-devserver` binary.

## Constraints honoured

- Paths are UTF-8 and cross-platform (`camino`); no `OsStr` juggling. Path keys fold case so the
  same file saved as `Views/x.toml` and `views/x.toml` debounces once.
- Time is always an argument: the watcher is polled with a monotonic `Duration`, the reloader
  takes a [`spot_core::Clock`]. Nothing in this crate reads the wall clock or sleeps.
- No `unsafe`, no `unwrap`/`expect` outside tests, no floats (durations are integer
  milliseconds).
- A live registry is never replaced by one that failed validation (`spot-meta-live` builds
  before it swaps; this crate only calls it).
