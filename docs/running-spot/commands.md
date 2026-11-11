Part of [Running Spot](../running-spot.md).

# Adding a `spot` command

`spot` has two kinds of subcommand:

- the **built-in** commands: the variants of `enum Command` in `crates/framework/spot-cli/src/main.rs`
  (`new-site`, `serve`, `install-app`, `list-apps`, `migrate`, `add-user`, `set-password`,
  `reset-admin`, `reload`, `check`). They stay where they are; and
- the **registered** commands: one file each in `crates/framework/spot-cli/src/commands/`.
  Adding one edits no shared file, `main.rs` included.

`spot --help` lists both. An unknown name prints the clap message and `available commands:` with
every name, and exits 1.

## A command file

`src/commands/<name>.rs` (lowercase letters, digits and underscores) declares three items at
column 0. `build.rs` finds them and generates the module list and the `COMMANDS` table; a file
that declares only some of them stops the build, and a file with none is a helper module.

```rust
//! `spot backup`: ...
use super::{CommandCtx, Exit};

pub const NAME: &str = "backup";

pub fn command() -> clap::Command {
    clap::Command::new(NAME)
        .about("Back up one site")
        .arg(clap::Arg::new("site").long("site").required(true))
        .arg(clap::Arg::new("apply").long("apply").action(clap::ArgAction::SetTrue))
}

pub async fn run(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let site = matches.get_one::<String>("site").map(String::as_str).unwrap_or_default();
    let backend = ctx.backend(site).await?;
    // ... print the plan; change data only when --apply is given ...
    writeln!(ctx.out(), "plan: ...")?;
    Ok(Exit::Ok)
}
```

`NAME` must equal the `clap::Command`'s own name. The build fails, naming both files, when two
files declare the same name, and naming the file when the name equals a built-in command. (The
global `--root` option is read by `CommandCtx`; a command does not declare it.)

## What `CommandCtx` offers

| Call | What |
| --- | --- |
| `ctx.root()` | the host root directory (`--root`, `SPOT_ROOT`) |
| `ctx.common()`, `ctx.sites()`, `ctx.site(name)` | the loaded `common.toml` and sites, read and validated on first use |
| `ctx.backend(site).await` | the pooled PostgreSQL backend on that site's database, per site, on demand |
| `ctx.clock()` | the clock; a command never reads the wall clock itself |
| `ctx.out()`, `ctx.err()` | standard output and error writers |

There is no raw SQL helper and no separate administrative connection handle: a command reaches data
through the site's backend and the same document engine, actor and permission path as the server.
A command that changes data does not write around them. Be aware of two limits of what the context
can enforce: the site backend connects with the host's configured PostgreSQL role (as `spot migrate`
does), so `backend.pool()` is a raw connection a reviewer must not accept in a command; and
`ctx.common()` returns the whole `common.toml` model including `postgres.admin_url`, which a command
must never print or log. Review enforces both.

## Exit codes

| Code | `Exit` | Meaning |
| --- | --- | --- |
| 0 | `Exit::Ok` | done |
| 1 | `Exit::Usage` | wrong usage: unknown command, missing or invalid arguments |
| 2 | `Exit::Refused` | refused by policy or permission; nothing was changed |
| 3 | `Exit::Failed` | failed while running |

Return `Ok(Exit::Refused)` after writing the reason to `ctx.err()`. An `Err` from `run` is printed
on standard error and exits 3, so keep error text free of secrets, keys, payloads and personal
data. (Clap's own errors for a registered command's arguments keep clap's exit code.)

## Conventions for a command that changes data

- It takes `--site` and acts on one site.
- It prints the plan first and applies it only with `--apply` (the convention `install-app` and
  `migrate` follow); without `--apply` nothing changes.
- It never takes a secret as a positional argument or an option value: it would appear in the
  process list. Take a file path, the name of an environment variable, or an interactive prompt
  (see `set-password`: `--password-stdin` or the terminal).

## Testing

A command is tested without a process: `tests/commands_registry.rs` includes `src/commands/mod.rs`
by path, builds a `CommandCtx` over in-memory writers and a `ManualClock`, registers a test
command (`Registration { name, command, run }`) and asserts its output and exit code. A command
that needs a database follows `tests/commands.rs` (skipped when no PostgreSQL is configured).
