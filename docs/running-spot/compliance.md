Part of [Running Spot](../running-spot.md).

# Compliance evidence

The `compliance` module (order 2000) collects the evidence for the controls the server can attest
by itself, appends it to the audit chain and serves the auditor a read-only view and an evidence
pack. The control library, the sealing and the pack format are `spot-compliance`'s
(`docs/security/controls/`); this module wires them to the hosted services. The code is
`crates/framework/spot-cli/src/services/compliance.rs` and its `compliance/` directory.

## Settings

`[services.compliance]` in `common.toml` (unknown keys are refused):

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` registers nothing. |
| `collect_every_secs` | `3600` | Seconds between collection ticks (at least 60). |
| `period_days` | `90` | The period each item reports on, ending at the collection time (1 to 366). |
| `min_mfa_permille` | `950` | Lowest MFA enrolment, per 1000 users, the authentication control accepts. |
| `max_idle_timeout_secs` | `3600` | Longest idle session timeout the authentication control accepts. |
| `max_session_secs` | `43200` | Longest absolute session lifetime the authentication control accepts. |

## Where the facts come from

Evidence is never typed in. Each fact is a port, `dyn FactSource<T>`, that the hosting module of the
subsystem provides before this module starts (a smaller `ORDER`):

| Fact | Control it evidences | Source id |
| --- | --- | --- |
| `ChainFact` (verified entries, findings, head) | `ctl.audit_integrity` | `audit-chain` |
| `ReviewFact` (campaigns closed, overdue) | `ctl.access_review` | `access-review` |
| `SodFact` (rules checked, open conflicts) | `ctl.sod_conflicts` | `sod-analysis` |
| `KeyFact` (rotation dates against the allowed age) | `ctl.key_rotation` | `key-rotation` |
| `BackupFact` (latest restore check) | `ctl.backup_verification` | `backup-verification` |
| `RetentionFact` (enabled, latest run) | `ctl.retention` | `configuration` |
| `AuthFact` (MFA enrolment, SSO, session timeouts) | `ctl.authentication_strength` | `configuration` |
| `VulnFact` (open critical and high findings) | `ctl.vulnerability_management` | `dependency-scan` |

The evidence goes to `dyn EvidenceChain` (an `EvidenceLog` over the site's audit chain implements it).

A control whose subsystem is not hosted has no collector. It stays **manual**, is listed with its
uncollected sources in the controls view and is never filled with a placeholder. A control whose
source id is shared (the `audit-chain` source also serves the consent and effect-hold controls)
is collected only for the control named in the table; the others stay manual. A fact that fails to
read produces a **failed** evidence item with the reason, appended like any other: a failed
collection is evidence too. An item that cannot be appended to the chain is not kept.

## Schedule

Every tick the controls that are due are collected: a control with no evidence yet, or whose
frequency window has passed since its last collection (continuous and per-change controls: every
tick).

## The auditor

`CompliancePort` answers only when the caller's grants are exactly the read-only auditor role
(`spot_compliance::is_read_only_auditor`): read on the control library, evidence items,
assessments and packs, no other right, not owner-scoped. Anything else, including an empty set,
is `compliance_host.forbidden`, decided on the server. The view holds controls, evidence summaries
(scanned by the redactor when sealed) and the pack; it never holds a customer document. The pack is
generated on demand from the latest evidence and verifies with `Pack::verify`.

## Level and failure

Optional. Without the evidence chain port the start-up check fails ("no evidence chain is hosted")
and readiness shows the site degraded; every other service is unaffected. A failed tick is retried
on the next tick and never stops the site.
