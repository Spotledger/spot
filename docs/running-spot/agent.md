Part of [Running Spot](../running-spot.md).

# The host agent

`spot-agent` is the program an operator installs on every server Spotledger manages. It is the one
privileged piece of the management plane, so this page says exactly what it does, what it keeps on
disk and what it refuses. Every statement was checked against the source on this branch (the crate
`crates/framework/spot-agent`) and against the channel contract
`contracts/agent-channel/openapi.yaml`; where the source and an earlier plan differ, the source
wins and the difference is stated. `python3 tools/docs-check/check_agent_guide.py` keeps the
subcommands, configuration keys, file modes, error codes and paths on this page equal to the source.

Two things the code on this branch does **not** do yet, so that you do not look for them: the running
agent does not check for or install updates by itself (the update library exists and is tested, but
nothing in `run` calls it), and the executable does not accept Windows `service` commands (the
library that plans them exists). Both are described below where they matter.

## What the agent is and is not

- **One process per managed server.** It reads the state directory, talks to the gateway and runs
  operations. It starts no other long-lived process; privileged work goes through a small helper
  (see Signed commands, below, and `crates/framework/spot-agent/src/privilege/mod.rs`).
- **It dials out and never listens.** Every connection is opened by the agent. There is no listening
  socket in the crate: `check_agent_guide.py` fails when a listener API appears in its source, and
  `tests/channel.rs` runs the loop against a TLS gateway the agent connects to.
- **It runs only operations of the typed catalogue, signed by an administrator key it trusts, within
  their validity window, once.** A command with no valid signature, an expired one, one addressed to
  another agent or one that repeats an accepted nonce is refused and never executed
  (`tests/channel.rs`: `refused_commands_are_reported_with_core_codes_and_never_run`,
  `a_signed_command_executes_once_and_a_replay_rereports`).
- **There is no free-form command line.** Privileged operations are fixed templates with typed
  parameters; argv is built from the template only, with no shell
  (`tests/handlers.rs`: `valid_values_build_argv_from_the_template_only`,
  `every_template_rejects_shell_metacharacters_newlines_and_option_like_values`).
- **Which operation families it may run is your choice: `allowed_modules`, and the default is none.**
  A namespace that is not listed is not registered at all, so a command for it is answered
  `agent.unsupported` (`tests/handlers.rs`: `a_namespace_that_is_not_enabled_is_not_registered_at_all`).

## Install and enrol

**Linux.** The state directory is `/var/lib/spot-agent`, created mode `0700` by `enrol`. Run the agent
as a dedicated unprivileged user. The few operations that need root go through the helper
`spot-agent-priv`, which the code expects at `/usr/libexec/spot-agent/spot-agent-priv` and invokes as
`sudo -n`. Generate the sudoers drop-in for your agent user with
`spot-agent-priv --print-sudoers --user USER`: it names the helper and each template exactly, with no
wildcard (`tests/handlers.rs`: `the_sudoers_output_allows_exactly_the_template_names_and_no_wildcard`).
No systemd unit file and no package ship in the repository yet (the packaging tasks are separate), so
for now you write the unit yourself: run `spot-agent run` as the agent user, with restart on failure;
the agent's panic hook logs the location and exits non-zero so the service manager restarts it.

**Windows.** The state directory is `%ProgramData%\Spotledger\Agent`. When it creates the directory the
agent applies an ACL that gives full control to SYSTEM and to the built-in Administrators group and to
nobody else (`windows_icacls_args` in `crates/framework/spot-agent/src/state.rs`; `tests/agent.rs`:
`windows_acl_strings`). The service host is described in the Windows service section.

**Enrol.** An administrator issues a one-time enrolment token in the console. On the server:

```sh
spot-agent enrol --gateway https://gateway.example.invalid --token-file /root/enrol.token --ca /etc/spot-agent/gateway-ca.pem
```

- `--token-file FILE` reads the token from a file (`--token TOKEN` also works but leaves the token in
  your shell history and process list; prefer the file).
- **`--ca FILE` is required in practice.** The usage text calls it optional, but the agent has no
  system trust store fallback: `enrol` stops with `agent.transport` and the message "--ca is
  required" when it is missing, and `run` stops with `agent.config_invalid` ("`ca` is required")
  when `ca` is not set. The gateway certificate is pinned to the certificates in that file.
- The token must be 16 to 256 printable characters and is accepted once. How long a token stays
  valid is decided where it is issued (the contract says only "a used or expired token is
  `401 agent.token_invalid`"); the agent has no expiry of its own.
- On success it prints one line: `enrolled as <agent id> (key <fingerprint>, <n> trusted administrator key(s))`.

What enrolment stores, in this order: the trusted administrator keys (`trust.toml`), the
configuration (`agent.toml`), then the enrolment record (`agent.key.identity`), which is the marker
that the agent is enrolled. The identity key itself is generated by the agent (`agent.key`, Ed25519)
and its private half never leaves the server. The token is never stored: the log and the local audit
log hold only the first 8 hex characters of its SHA-256 digest. Enrolling again on an enrolled
server is refused with `agent.already_enrolled`.

`spot-agent rotate-key --token-file FILE` replaces the identity key: a new key is generated beside
the old one, enrolled with a fresh one-time token, and installed only after the gateway accepted it.
`spot-agent --state-dir DIR ...` uses another state directory (tests and unusual layouts).
`spot-agent audit-verify [--file PATH] [--agent-id ID]` checks the hash chain of the local audit log (see below). `spot-agent probe` is the short run of a new version that the running agent starts during a self-update: it sends one heartbeat that carries its own version and exits 0, or exits non-zero so the update is rolled back; it opens no audit log and changes no state. `spot-agent version` prints the agent's version, and `spot-agent status` prints whether it is enrolled, its id, the gateway, the allowed modules, the last poll, the last command, the backlog and the clock skew (and a failed start-up network recovery, see below).

The `--key-store tpm` option exists and always fails with `agent.key_store_unavailable`: TPM storage
is not implemented yet. Only the file key store works.

## Files in the state directory

| File | What it is | Mode |
| --- | --- | --- |
| `agent.toml` | the configuration (below) | `0600` |
| `agent.key` | the Ed25519 identity key | `0600` |
| `agent.key.identity` | the enrolment record (agent id and enrolment time); its presence means "enrolled" | `0600` |
| `trust.toml` | the administrator public keys this agent trusts for commands | `0600` |
| `audit.log` (and `audit.log.<seq>`) | the local hash-chained command audit log | written with the log's own appends |
| `agent.log` | structured JSON log lines, redacted | append |
| `channel-status.json` | what `status` reads: last poll, last command, backlog, clock skew | `0600` |
| `commands/` | one record per command and the replay window; written and fsynced before a command runs (the directory is `0700`, its files `0600`) | `0700` |
| `network/` | the network revert ticket and its backup (Linux) | - |
| `network-recover.failed` | exists only while the last start-up network recovery failed | `0600` |

Every file the agent writes is written atomically (temporary file, fsync, rename, fsync of the
directory) with mode `0600`. At start the agent refuses to run if the state directory allows group or
other access (it must be `0700`) or if `agent.key`, `trust.toml` or `agent.toml` is accessible by
group or others: the error is `agent.state_permissions` and the exit code is 4
(`tests/agent.rs`: `modes_are_enforced`).

## Configuration

`agent.toml` is written by `enrol` and can be edited. It is strict: an unknown key, a wrong type or a
bad value refuses to start with `agent.config_invalid` (the struct is `deny_unknown_fields`;
`tests/agent.rs`: `config_with_an_unknown_key_is_refused`).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `gateway` | string | none, required | The gateway base URL. Must start with `https://`; at most 253 characters after it; no spaces, `@`, `?` or `#`. |
| `ca` | string | none | Path of the PEM file with the gateway's trust anchor certificate. Required to run, see Enrol. |
| `poll_seconds` | integer | `5` | Pause between polls, 1 to 3600 seconds. |
| `log_level` | string | `"info"` | One of `error`, `warn`, `info`, `debug`. Logs stay redacted at every level. |
| `update_check_hours` | integer | `6` | Hours between scheduled self-update checks, 0 to 168; `0` turns the scheduled check off. Each pause gets a random extra of up to 20 per cent. The signature checks of an update are never optional. |
| `install_root` | string | the platform's default install root | The install root the updater manages (`versions/`, `current`, `update-trust.json`); at most 260 characters. |
| `allowed_modules` | list of strings | `[]` | The operation namespaces the agent may execute. A namespace is dot-separated segments of lower-case letters, digits, `_` and `-`, at most 120 bytes; `server.service` allows `server.service.restart`. |

A complete example:

```toml
gateway = "https://gateway.example.invalid"
ca = "/etc/spot-agent/gateway-ca.pem"
poll_seconds = 5
log_level = "info"
allowed_modules = ["server.service"]
```

## Network

The agent opens outbound TLS 1.3 connections to the gateway and to nothing else; the certificate is
checked against the pinned CA file only. To allow it through a firewall: **outbound** TCP from the
server to the host and port in `gateway` (443 unless the URL says otherwise). Nothing needs to be
opened inbound.

- It calls `POST /agent/v1/poll` (a long poll the gateway holds for up to 30 seconds; the agent waits up
  to 45), then reports results (`/agent/v1/results`) and sends a heartbeat (`/agent/v1/heartbeat`) after each
  poll, then waits `poll_seconds`. Other calls time out after 30 seconds; a connection attempt after 15.
- Every request is signed by the agent key. The agent learns the gateway's clock offset and reports it in
  the heartbeat as `clock_skew_seconds`, with `agent_backlog`.
- **When the gateway is unreachable** the agent retries with jittered exponential backoff from 1 second up
  to 5 minutes (half to full of each step, steps 1, 2, 4 ... 300 seconds), and resets to 1 second after a
  success. Meanwhile it executes nothing that is not signed: commands only ever arrive over the channel.
  Commands it had already accepted are kept in `commands/` and finish or are reported as described below.
- **When the gateway answers `403 agent.revoked`** the agent stops polling and deletes `agent.key`,
  `agent.key.identity` and `trust.toml`; it must be enrolled again with a new token.

## Signed commands in plain words

An administrator signs a command with their key; the gateway only passes it on. The agent trusts only
the keys in its own `trust.toml` (returned once at enrolment, at most 16 keys; the running agent never
replaces them from a gateway message; only `rotate-key`, which an operator runs, rewrites the file). Before it runs anything the agent checks, in this order: the
signature(s) against the keys it trusts, that the command is addressed to this agent, the validity
window (at most 15 minutes long, with 60 seconds of clock skew tolerated), that the nonce was never
accepted before (the replay window is on disk, so it survives a restart), the operation's risk level and
the arguments against the operation's schema. Only then is the command written to `commands/` and run
(`crates/framework/spot-agent/src/channel/intake.rs` documents the fixed order).

How many administrators must sign depends on the operation's risk (`crates/framework/spot-agent-core/src/policy.rs`):

| Risk | Needs |
| --- | --- |
| read, low, medium | one valid administrator signature |
| high | two distinct administrators, and the requester may not be one of the two |
| critical | the same, plus a clearance (a configured delay or a break-glass grant); this agent has none configured, so a critical operation is refused with `agent_core.critical_not_cleared` |

The default policy also requires every signing key to be hardware-backed. A key's protection is
recorded in `trust.toml` as `software` or `hardware`; enrolment writes `software` for every key,
because it does not attest keys. See Troubleshooting: with the default policy a command signed by a
key recorded as `software` is refused with `agent_core.software_key_refused`.

A command id runs at most once. If the agent stops after accepting a command but before it started,
it re-verifies and runs it once at restart; if it stopped after the handler started, the result is
reported as `failed` with `agent.indeterminate` and the command is never run again
(`tests/channel.rs`: `crash_between_persist_and_execute`). Output reported to the gateway is capped at
64 KiB and redacted first.

## The local audit log

Every decision is recorded in `audit.log` in the state directory: one JSON line per command (accepted,
refused with its code, executed, handler failed), plus the events `agent.enrol`, `agent.rotate_key` and
`agent.network_recover`. A key store that is only software (a file key store) is refused at enrolment with `agent.software_keys_not_allowed` unless the operator passes `--allow-software-keys`, and the choice is recorded in this log. Each line is a hash-chained entry (`SHA-256(seq, prev, record)`, starting from a
genesis hash derived from the agent id), fsynced as it is appended. If the log cannot record a decision,
the command is **not run** (`agent_core.log_unavailable`).

- A line holds the command id, a digest of the command, the operation id, the signers' key ids, the
  outcome and the code. It never holds parameter values, a token or a key.
- The log rotates at 4 MiB into files named `audit.log.` followed by the sequence number, and keeps the
  newest 8 rotated files.
- `spot-agent audit-verify` checks the chain (it calls `verify_file` in
  `crates/framework/spot-agent/src/audit.rs`) and prints `audit log ok: N entries`; it reads the agent id
  from the enrolment unless `--agent-id` is given.
  `tests/agent.rs`: `audit_chain_verifies_continues_after_restart_and_detects_tampering`.
- `agent.log` is separate: JSON lines with `level`, `event` and `message`, every message passed through
  the redaction rules (`tests/agent.rs`: `no_secret_reaches_the_logs`).

## Self-update

The update code is in the crate (`crates/framework/spot-agent/src/update/`). The running agent checks
for an update every `update_check_hours` hours (default 6, `0` turns the check off). What the code
guarantees: the update is accepted only when its manifest is signed by the update trust anchor (a file the package
installs next to the binary; never fetched from the gateway), inside that key's validity window and not
on the revocation list; an older version is never installed (a fleet rollback is a newer version number
carrying the old code); the archive is checked for digest, size and unsafe paths; the new version is
started in a probe mode first and must report healthy before the old one is replaced; if it does not,
the old version is restored and the result is `agent.update_rolled_back`; if neither works, the result
is `agent.update_attention` and an operator must look. There is no setting that turns the signature
check off. An offline site would use a local directory source; no page describes that yet.

## Network changes and the revert timer (Linux)

A network change made through the agent is applied **with a confirmation window**: the previous
configuration is saved first, a revert ticket is written to `network/` under the state directory, and an
independent `systemd-run` timer is scheduled that runs `spot-agent network-revert <ticket id>`. If the
change is not confirmed in time, the previous configuration is restored. This also holds when the agent
restarts inside the window (the timer is re-armed), after the deadline (the revert happens at once, and
only once) and when the clock steps or the machine reboots
(`crates/framework/spot-agent-linux-sys/tests/network_revert.rs`).

`spot-agent network-revert <id>` is run by the timer as root. It needs no enrolment, no gateway and no
network. It prints one line and exits **0** for every outcome (`network revert: reverted`,
`network revert: already done`, `network revert: not this ticket`) and **1** for an error (a missing,
wrong or overflowing id; or a platform without the Linux network module, where it prints
"network-revert is not supported on this platform"). `tests/network_revert.rs` covers each case.

At start, before the first poll and before any plan is accepted, the agent deals with a change a
previous run left pending and records the outcome in the audit log. A failure is logged, remembered in
`network-recover.failed` and shown by `spot-agent status` as `network recovery: FAILED (...)` until the
next successful recovery; it never stops the agent from starting.

## Windows service

The same executable is meant to run as the service `SpotledgerAgent` under the virtual account
`NT SERVICE\SpotledgerAgent`, with a second service `SpotledgerAgentPriv` as `LocalSystem` that serves the
privileged templates over the named pipe `\\.\pipe\SpotledgerAgentPriv` (its ACL admits only the agent
account). Both start automatically, delayed, and the service manager restarts them on failure after 5, 30
and 60 seconds (the helper: 5 and 30). Installation runs `sc.exe` with arguments only, no shell and no
prompt, so it is silent; it is idempotent (a second install does nothing, a changed binary path updates
the service) and it refuses a binary outside `Program Files` or `Windows` or in a user-writable folder
(exit code 10; the other exit codes are 0 done, 1 failed, 2 usage, 5 access denied: run elevated). Stop
and shutdown wait up to 30 seconds for the running command.

**On this branch the executable does not accept the service commands**: its usage text lists no
`service` command, and the grammar (install, uninstall, run, run with `--helper`) lives only in
`crates/framework/spot-agent/src/winsvc.rs`, tested on Linux with `crates/framework/spot-agent/tests/winsvc.rs`.
There is no installer page yet.

## Removing the agent

1. Stop the service or process (`SIGTERM` or Ctrl-C ends the loop; a command that is running gets up to 30
   seconds, after that it is left as `started` and reported indeterminate at the next start).
2. Disable it in your service manager and remove the unit (Linux) or the services (Windows).
3. Delete the state directory (`/var/lib/spot-agent`, or `%ProgramData%\Spotledger\Agent`) and the sudoers
   drop-in for the helper.

What remains: anything the operations you ran changed on the server (hardening, services, network), and,
on the control plane, the agent's enrolment. Revoking an enrolment is a control-plane operation; no
page documents it yet. A revoked agent deletes its own keys the next time it polls.

## Errors

The agent's own codes (`agent.*`) are defined in `crates/framework/spot-agent/src/error.rs` and in the
modules named in the table; they are **not** in the error catalogue yet. The `agent_core.*` codes are in
`crates/framework/spot-errors/catalogue/agent_core.toml`, with the remedies quoted here. A code appears in
one or more of: stderr (with the exit code), the audit log, the result reported to the gateway.

| Code | Where | Meaning and remedy |
| --- | --- | --- |
| `agent.usage` | stderr, exit 2 | A wrong command line. The usage text is printed. |
| `agent.not_enrolled` | stderr, exit 3 | No enrolment record or configuration. Run `spot-agent enrol`. |
| `agent.already_enrolled` | stderr, exit 3 | The server is already enrolled; enrol never overwrites an identity. |
| `agent.state_permissions` | stderr, exit 4 | The state directory is not `0700` or a key or config file is open to group or others. Fix the mode. |
| `agent.config_invalid` | stderr, exit 1 | `agent.toml` has an unknown key or a bad value; the message names the key. |
| `agent.state_io` | stderr, exit 1 | A state file could not be read or written. |
| `agent.token_invalid` | stderr, exit 1 | The gateway refused the enrolment token (one answer for every cause: used, expired, unknown). Ask for a new token. |
| `agent.gateway_protocol` | stderr, exit 1 | The gateway's answer was malformed. |
| `agent.transport` | stderr, exit 1 | The gateway could not be reached, or no `--ca` was given or readable. |
| `agent.transport_unavailable` | stderr, exit 1 | No gateway transport is built in. |
| `agent.key_store_unavailable` | stderr, exit 1 | `--key-store tpm` is not implemented. Use `file`. |
| `agent.audit_failed` | stderr, exit 1 | The local audit log could not record, or its chain is broken. |
| `agent.revoked` | log, gateway | The gateway revoked this agent; keys deleted. Enrol again. |
| `agent.replayed` | log | The gateway saw a request nonce twice; the agent retries. |
| `agent.invalid` | log | The gateway's answer did not fit the contract. |
| `agent.unsupported` | result, audit log | No handler for the operation: its namespace is not in `allowed_modules`, or the operation is unknown. |
| `agent.indeterminate` | result | The agent stopped while the handler was running; the result is unknown and the command is not re-run. |
| `agent.handler_failed` | result | The handler ran and failed; its (redacted, capped) detail is the output. |
| `agent.handler_panicked` | result | The handler panicked. |
| `agent.privilege_refused` | result | The operation is not one of the fixed privileged templates. |
| `agent.privilege_unavailable` | result | The privilege helper is not installed or not allowed by sudoers. |
| `agent.privilege_spawn_failed` | result | The helper could not start the command. |
| `agent.privilege_unknown_operation` | helper | The helper does not know the template name. |
| `agent.privilege_param_count` | helper | The helper received the wrong number of parameters. |
| `agent.privilege_bad_param` | helper | A parameter failed the template's validation. |
| `agent.revert_executable_unknown` | log | The agent's own path cannot be resolved, so a network change that needs a revert timer does not start. |
| `agent.update_unavailable` | result | No update is published for this agent. |
| `agent.update_refused` | result | The offered update failed a check before anything changed (signature, window, revocation, downgrade, archive). |
| `agent.update_failed` | result | The update could not run (network, layout, trust files). |
| `agent.update_rolled_back` | result | The new version did not come up healthy; the previous one is serving. |
| `agent.update_attention` | result | Neither the update nor its rollback completed; an operator must look. |
| `agent_core.envelope_malformed` | result, audit | The command envelope is malformed. Resend it from the console or CLI. |
| `agent_core.wrong_audience` | result, audit | The command is addressed to a different agent. Re-issue it for this agent. |
| `agent_core.not_yet_valid` | result, audit | The issue time is in the future beyond the clock skew. Check the issuer's clock. |
| `agent_core.expired` | result, audit | The validity window has passed. Re-issue the command. |
| `agent_core.lifetime_exceeded` | result, audit | The window is longer than the policy allows (15 minutes). Re-issue with a shorter one. |
| `agent_core.unknown_signer` | result, audit | A signature names a key that is not in `trust.toml`. Enrol the administrator's hardware key before signing. |
| `agent_core.revoked_signer` | result, audit | A signature names a key that has been revoked. |
| `agent_core.invalid_signature` | result, audit | A signature does not verify. Re-sign the command. |
| `agent_core.insufficient_signatures` | result, audit | Too few distinct valid administrator signatures for the risk level. |
| `agent_core.replay` | result, audit | The nonce was already accepted. Re-issue the command. |
| `agent_core.nonce_store` | result, audit | The replay window is full or its file failed. |
| `agent_core.unknown_operation` | result, audit | The operation is not in the agent's catalogue. |
| `agent_core.invalid_arguments` | result, audit | The parameters do not satisfy the operation's schema. |
| `agent_core.software_key_refused` | result, audit | A signature came from a key that is not hardware-backed. Sign with the administrator's hardware key. |
| `agent_core.requester_cannot_approve` | result, audit | The requester signed a command the policy forbids them approving. Have two other administrators approve. |
| `agent_core.critical_not_cleared` | result, audit | A critical operation lacks the configured delay or a break-glass clearance. |
| `agent_core.log_unavailable` | result | The audit log could not record the decision, so the command was not run. Restore its storage. |
| `agent_core.unknown_handler` | result, audit | No handler is registered for the operation in the executor. |
| `agent_core.plan_unverified` | result, audit | The plan is not the deterministic plan of a validly signed command; treat the broker as compromised. |
| `agent_core.plan_hash_mismatch` | result, audit | The plan's hash does not match its fields; treat it as tampered with. |
| `agent_core.untrusted_broker` | result, audit | The plan names a broker key the executor does not trust. |
| `agent_core.broker_signature_invalid` | result, audit | The broker's signature over the plan does not verify. |
| `agent_core.invalid_key` | result, audit | A public key offered for trust is not a valid Ed25519 key. |
| `agent_core.malformed` | result, audit | The host supplied a malformed value while building the agent's trusted inputs. |
| `agent_core.attestation` | result, audit | A hardware-key attestation did not verify against the vendor roots. |

## Troubleshooting

| Symptom | Likely cause | What to do |
| --- | --- | --- |
| `enrol` says `agent.token_invalid` | The token was already used, expired or is wrong; the gateway gives one answer for all three | Ask an administrator for a new one-time token; never reuse one. |
| `enrol` says `agent.transport` and mentions `--ca`, or `run` says `agent.config_invalid` and mentions `ca` | No CA file, or the file is unreadable or holds no certificate | Pass `--ca` (enrol) or set `ca` in `agent.toml` (run) to the PEM file with the gateway's certificate. |
| The TLS connection fails after enrolment | The CA file does not match the gateway's certificate | Replace the CA file; the agent trusts only what is in it. |
| `agent.state_permissions` and exit 4 | The state directory is not `0700`, or `agent.key`, `trust.toml` or `agent.toml` is readable by group or others | `chmod 700` the directory and `chmod 600` the files, then start again. |
| `agent.config_invalid`, "unknown field" | A key in `agent.toml` is not one of the five in the table | Remove or correct the key; the agent will not start until the file parses. |
| A command is refused as `agent_core.expired` or `agent_core.not_yet_valid` | The clocks of the issuer and this server differ by more than the tolerance | Fix time synchronisation; `spot-agent status` shows `clock skew`. Re-issue the command. |
| A command is refused as `agent_core.replay` | The nonce was already accepted (the gateway re-sent it, or it was re-issued unchanged) | Issue a fresh command; the replay window is on disk and survives a restart. |
| A command is answered `agent.unsupported` | The operation's namespace is not in `allowed_modules` | Add the namespace to `allowed_modules` in `agent.toml` and restart the agent. |
| A command is refused as `agent_core.software_key_refused` | The default policy requires hardware-backed signing keys and `trust.toml` records the key as `software` (enrolment records every key that way) | Sign with a hardware key that is recorded as `hardware`; see the open questions of this task before editing `trust.toml` by hand. |
| A change to the network was undone | It was not confirmed within its window, so the revert timer restored the old configuration | Check `audit.log` for `agent.network_recover` and the timer's output; make the change again and confirm it. |
| `spot-agent status` shows `network recovery: FAILED` | The start-up recovery of a pending network change failed | Read the line and `agent.log` (event `network.recover`); fix the cause; the marker clears on the next successful recovery. |
| An update is refused (`agent.update_refused`) | A bad signature, a key outside its window, a revoked key, a downgrade or a bad archive | Nothing changed on disk. Publish a correctly signed, newer update. (The running agent does not fetch updates on this branch.) |
| A Windows service install exits 10 | The binary is in a user-writable folder or not under `Program Files` or `Windows` | Install the binary under `Program Files` and run the install elevated. (The executable does not route `service` commands yet.) |
| No heartbeat in the console | The agent cannot reach the gateway; `status` says `last poll: never` or an old time | Read `agent.log` for `poll_failed` lines (they say how long until the next try); check outbound access to the gateway host and port. |
| Commands stop being run and `agent_core.log_unavailable` appears | The disk holding `audit.log` is full or read-only | Free the space; the agent refuses to run anything it cannot record. |
| The agent stopped polling and its key files are gone | The gateway revoked it (`agent.revoked`) | Enrol again with a new token. |
