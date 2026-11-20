Part of [Running Spot](../running-spot.md).

# The hardening baseline

A new server should reach a known safe state, and anything that drifts from it should be reported.
This page says what that state is, how to read a result, which fixes the agent can make and which it
only reports, and what to do before a fix that could cut you off from the host. The table of
controls at the end is generated from the baseline files, so it cannot be out of date.

## What the baseline is

The baselines are our own. They were written from first principles and public security guidance, in
our own words; they are not a copy of any licensed benchmark. There is one file per operating system
family in `crates/framework/spot-hardening/baselines/`: `linux.toml` and `windows.toml`. Each control
has an id, a title, a severity, a check over named facts, and, when one exists, a fix.

A fact is the agent's normalised view of the host, for example `ssh.permit_root_login`. If the host
does not report a fact that a control reads, that control is **not applicable** on that host; it is
neither a pass nor a failure.

The baseline is applied by the host agent ([the agent guide](agent.md)). It exposes three operations
to the console: `hardening.check` (evaluate and report), `hardening.plan` (show the exact changes a
selection would make) and `hardening.apply` (make them). The agent's handler evaluates the Linux
baseline today (`HardeningHandler::baseline` in `crates/framework/spot-agent/src/handlers/hardening_ops.rs`);
the Windows controls are defined and tested in `spot-hardening` and are listed below.

## Reading a result

Every control gets exactly one of four outcomes (`Outcome` in `crates/framework/spot-hardening/src/report.rs`):

| Outcome | Meaning |
| --- | --- |
| pass | The host satisfies the control. |
| fail | The host reports a value the baseline does not allow. The report says what was `found` and what was `expected`, in words. |
| not applicable | A fact the control needs is not reported on this host. |
| error | The control could not be evaluated, for example the fact source failed or the check is malformed. |

Where a control has several checks, `all` fails if any nested check fails, then errors, then is not
applicable, and passes only when every check passes; `any` passes as soon as one nested check passes,
and otherwise reports an error, then not applicable, then a failure
(`evaluate_all` and `evaluate_any` in `crates/framework/spot-hardening/src/check.rs`).

The compliance report lists one result per control in the order of the baseline file, with its
severity, its fix, and whether it is reversible and carries lock-out risk, plus a score: how many
passed, failed, were not applicable or errored, how many were applicable (passed, failed and errored;
not applicable controls are left out), and the percentage that passed in basis points
(`Score::of`). The agent also writes the findings and a count of failures by severity to its state
directory (`hardening-report.json` and `hardening-drift.json`).

## What is fixed automatically and what is only reported

A control can be fixed by the agent only when it has a fix (an operation in the management
catalogue) **and** the fix is reversible (`Control::auto_remediable` in `model.rs`). In the table
below, "Fix: none: reported only" means the control has no fix at all, and "Reversible: no" means
the fix, if listed, is never applied on its own.

The rules, as `remediation_plan` and `suggested_plan` in `crates/framework/spot-hardening/src/remediate.rs`
apply them:

- Only a control that **failed** is considered. A pass, a not applicable result or an error is never fixed.
- A control with no fix, or with a fix that is not reversible, is never applied automatically. It stays in the report.
- Only controls at or above the policy's minimum severity are considered.
- The policy must be in auto-remediate mode. In suggest-only mode, which is the default, nothing is
  applied and every candidate is listed as a suggestion for a person to approve.
- A control with lock-out risk is applied only when the policy allows lock-out risk **and** a
  connectivity guard is present (`Policy::may_apply_lockout`). Otherwise it is held back as a suggestion.
- Fixes shared by several controls (the two SSH controls share one) appear once in the plan.

The `hardening.apply` operation of the agent is stricter in one way and different in another. It is
a dry run unless you set `dry_run` to false, it applies only the rules you name in `rules`, and each
named rule must currently fail and have a fix (`select` in `hardening_ops.rs`); after each fix it runs the
operation's own verification and reports a failure if that does not pass. Today it does not look at the
`reversible` flag when you name a rule yourself: you are the approval.

## Before you apply a fix with lock-out risk

A fix with lock-out risk (turning off the SSH service, setting the firewall to deny by default) can
cut you off from the host. `hardening.apply` refuses it, before changing anything, unless both of
these are true (`HardeningHandler::guard_satisfied` and `Refused::LockoutGuard`):

1. You state `admin_key_session=true`: you are connected with a key-based administrator session
   that does not depend on what you are about to change.
2. You pass `revert_ticket` naming a **pending** network revert ticket (WP-ADM-04.T5), so the
   network changes made by this session undo themselves if you do not confirm them. The ticket must
   be the one the agent holds as pending.

A dry run needs neither: run the dry run first, read the plan, then apply.

If you do lock yourself out, there is no break-glass or rescue procedure documented for hosts yet.
The only recovery the project documents is the network revert ticket above: a network change made
with a revert timer is reverted to its previous configuration if it is not confirmed in time
(`crates/framework/spot-agent-linux-sys/src/network.rs`).

## Adding or changing a control

1. Edit the baseline file in `crates/framework/spot-hardening/baselines/` (the file comments say why
   each control exists), keeping to the check kinds in `model.rs` (`CheckSpec`).
2. Run `python3 tools/docs-check/gen_hardening_reference.py --write` to regenerate the table below.
3. Commit the baseline and this page together. CI runs
   `python3 tools/docs-check/gen_hardening_reference.py --check`, which fails with a diff when the
   table is stale or has been edited by hand.

## The controls

The table is generated; do not edit it. Severities are low, medium, high or critical. "Reversible"
and "Lock-out risk" are the flags of the control; see above for what they change.

<!-- BEGIN GENERATED: controls -->

### Linux controls

| Id | Control | Severity | What is checked | Fix | Reversible | Lock-out risk |
| --- | --- | --- | --- | --- | --- | --- |
| `ssh.root_login` | SSH root login is disabled | high | either fact `ssh.enabled` must be false or fact `ssh.permit_root_login` must be one of `no` | `os.service.disable` | yes | yes |
| `ssh.password_auth` | SSH password authentication is disabled | high | either fact `ssh.enabled` must be false or fact `ssh.password_authentication` must be one of `no` | `os.service.disable` | yes | yes |
| `mac.enforcing` | Mandatory access control is enforcing | high | fact `mac.mode` must be one of `enforcing` | none: reported only | no | no |
| `firewall.default_deny` | Inbound firewall defaults to deny | high | fact `firewall.default_inbound_deny` must be true | none: reported only | no | yes |
| `firewall.declared_ports` | Only declared ports are open inbound | medium | `firewall.open_inbound_ports` must be a subset of `firewall.declared_ports` | none: reported only | no | yes |
| `updates.security_auto` | Security updates are applied automatically | high | fact `updates.security_updates_automatic` must be true | none: reported only | no | no |
| `services.unnecessary` | Only role-allow-listed services are running | medium | `services.running` must be a subset of `services.allowed` | none: reported only | no | no |
| `time.sync` | System time is synchronised | medium | fact `time.synchronised` must be true | `os.service.enable` | yes | no |
| `kernel.sysctls` | Kernel hardening sysctls are set | high | fact `kernel.hardening_sysctls` must be true | none: reported only | no | no |
| `files.spotledger_permissions` | Spotledger directories have safe permissions | high | fact `files.spotledger_dirs_secure` must be true | none: reported only | no | no |

### Windows controls

| Id | Control | Severity | What is checked | Fix | Reversible | Lock-out risk |
| --- | --- | --- | --- | --- | --- | --- |
| `windows.firewall_profiles` | Windows Firewall profiles are enabled | high | all of: fact `windows.firewall.domain_enabled` must be true; fact `windows.firewall.private_enabled` must be true; fact `windows.firewall.public_enabled` must be true | none: reported only | no | yes |
| `windows.smbv1` | SMB version 1 is disabled | high | fact `windows.smb1_enabled` must be false | `os.service.set_start_type` | yes | no |
| `windows.rdp_nla` | Remote Desktop requires Network Level Authentication | high | fact `windows.rdp.nla_required` must be true | none: reported only | no | no |
| `windows.defender` | Microsoft Defender real-time protection is on | high | fact `windows.defender.realtime_enabled` must be true | `os.service.start` | yes | no |
| `windows.uac` | User Account Control is enabled | high | fact `windows.uac.enabled` must be true | none: reported only | no | no |

<!-- END GENERATED: controls -->
