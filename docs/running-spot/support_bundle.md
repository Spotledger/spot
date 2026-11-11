# `spot support`: diagnostic bundles, crash/telemetry consent, offline help

WP-PRD-13.T3 wires the already-shipped `spot-unattended-support` and `spot-redact` crates
into the CLI. Everything here runs with no AI and no network unless the operator explicitly
configures an upload endpoint and turns consent on.

## `spot support bundle`

```
spot support bundle --out FILE [--include logs,config,health,versions] [--since 24h]
```

Writes a ZIP to `FILE` with up to four categories, each defaulting to included:

- `versions`: the `spot` binary's version, operating system and CPU architecture.
- `config`: an **allow-listed structural summary** of `common.toml` and every site's
  `site.toml` — never the raw file text. Only keys on a known-safe list (for example `site`,
  `database`, `apps`, `enabled`, `port`, `security`, a handful of timing/queue settings) show
  their value; every other key, whatever it is named and whatever it contains, is shown as
  `[redacted:<type>]`. This is an allow list, not a list of "looks like a secret" names to
  deny: a free-form `[services.<module>]` table, or an operator typo, is redacted by default.
- `health`: a local, no-network summary — whether the configuration files parse, and how
  many sites are on disk. This never opens a database connection and never asks a running
  server anything, so a bundle can always be taken, including with the host down.
- `logs`: every `*.log` file under the host root (for example a site's
  `identity-audit.log` when `audit.identity_file_mirror` is on), excluding any `files`
  directory (uploaded document content is never a log) and never following a symlink
  anywhere in the walk. `--since 24h` (or `30m`, `7d`, `45s`) skips a log file whose last
  modification is older than that; `config`, `health` and `versions` are unaffected (they
  are always current).

**What is guaranteed:**

- Every text file in the bundle passes through `spot-redact` before it is written: secrets,
  tokens, connection strings, personal data patterns, and (for the values `spot support
  bundle` itself read out of `common.toml`, such as the PostgreSQL admin URL and an SMTP
  password, plus any secret-shaped environment variable) the known-secret registry, so a
  literal appearance elsewhere in the bundle is caught too, not only the field that declared
  it. A log file that cannot be redacted is excluded from the bundle entirely — never emitted
  un-redacted.
- The bundle never contains a database dump or document data: the command never connects to
  a database and never reads a site's stored records.
- `--out` must **not already exist**: the file is created exclusively (`O_CREAT|O_EXCL`),
  never truncated or overwritten, whatever it currently is (a regular file or a symlink). Its
  parent directory must already exist (this command never creates directories for you) and
  must not be the data directory (`<root>/sites`, where every site's database-adjacent files
  and secrets live) or fall inside it — checked against the **canonical** form of both paths,
  not only a lexical comparison, so a symlink cannot relocate the write after the check runs.
  On Unix the file is also opened with `O_NOFOLLOW`, refusing a symlink in the final
  component outright (a stricter, independent guard against the same class of attack).
- The file is written mode 0600 (owner read/write only) on Unix, set at creation — never
  `chmod`ed afterwards.
- The bundle is capped in both file count and total content size; a bundle that hit either
  cap says so in a `bundle_truncated.txt` entry rather than silently growing without bound.
- Every file written and its byte count are printed to standard output, so the file can be
  reviewed before it is sent anywhere.

## `spot support consent on|off|status`

Reads and changes the single consent file (`support-consent.toml` under the host root, mode
0600) that gates the optional crash-reporting and telemetry service
(`crates/framework/spot-cli/src/services/unattended_support.rs`). `on` and `off` set both the
crash-reporting and telemetry switches together (there is exactly one decision to make from
this CLI); `status` prints the current value of each. The default, before this command is
ever run, is `ask` for both, which sends nothing anywhere.

Changing the switch here takes effect the next time a long-running `spot serve` process reads
the consent file at start-up; a host that wants crash reporting live over the lifetime of one
process builds a `services::unattended_support::UnattendedSupport` and calls its `arm` method
once (see that module's doc comment, and the Open questions of the task report this guide
belongs to for why `spot serve` does not call it itself yet).

## `spot support help-code CODE`

Prints the offline, plain-language explanation for an error code from the bundled
`spot-unattended-support` help catalogue (`crates/framework/spot-unattended-support/help/*.toml`):
what happened, why it usually happens, and what to do, one step at a time, plus a `learn_more`
document reference when the catalogue has one. Works on an air-gapped machine: the catalogue
is compiled into the binary (`include_str!`), nothing is looked up over the network, and no AI
model is involved. `--locale` (default `en`) selects a locale, falling back through the
language part to English when the exact locale has no entry.

A code with no offline help prints a refusal (`support_bundle.help_missing`) rather than a
blank answer, so the absence is visible rather than silent.
