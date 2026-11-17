Part of [Running Spot](../running-spot.md).

# Process governance and process-first entry

Governance (`spot-bpm-governance`) forbids creating or changing a **governed** record type
outside a process. Process-first entry (`spot-bpm-entry`, ADR-080) replaces "New" by "Start" and
keeps what a person types in a **draft record** until the step that owns it submits it. This
module (`crates/framework/spot-cli/src/services/process_governance.rs`, start order `1140`, after
the task inbox) hosts both for each site.

Governance is **off by default**: a record type nobody lists is free, and a site with no
`[services.process_governance]` table changes nothing.

## Configuration

```toml
[services.process_governance]
admin_override = false          # an administrator is not exempt unless this is true
admin_roles = ["System Manager"]
key_ids = ["grant-1"]           # the current key id first; older ids still verify
grant_ttl_secs = 600            # 30 to 900
draft_retention_days = 30       # 1 to 3650
tick_secs = 30                  # 5 to 3600
reason_min_chars = 8
companies = ["acme"]            # company paths a record may name besides the site itself

[[services.process_governance.governed]]
doctype = "sales_invoice"
mode = "governed"               # or "process_only"
# scope = "acme"                # absent: the whole site; a scope covers the units below it

[[services.process_governance.entry]]
doctype = "item"
process = "new_item"            # absent: the platform generates a minimal entry process
```

Unknown keys, an unknown mode, a bad key id or an out-of-range number refuse the site's start,
naming the key and never its value.

## The rule for a governed record type

A write (create, update, post, cancel, delete) to a governed DocType is allowed only when:

1. it carries a **request grant** the authority verifies for **this person** and the **current
   attempt** of an open step that may write that type; the grant is **consumed in the same
   transaction** as the write (a second use of the same grant is a replay and is refused); or
2. the person holds the `bpm.outside_process` permission and gives a reason of at least
   `reason_min_chars` characters (the audited exception); or
3. `admin_override = true` and the person holds one of `admin_roles` (audited as
   `admin_override`).

Everything else is refused with `bpm_governance.outside_process_denied`
(`bpm_governance.reason_required` when the permission is held but the reason is short). A refusal
is stable and carries no document value.

A `process_only` record type has **no** permission and **no** override: a create needs a grant for
a step of the type's entry process, otherwise `bpm_entry.start_process` names the process to
start; a change needs a grant.

## The command id proves nothing

A command id is an idempotency key the caller chooses. It is a request context only when the
production authority verifies it as a grant it signed (`rg1.…`, HMAC-SHA256) for this person, for
the current attempt of an open step that has not expired. A header or a field claiming a process
origin, the old plain `request:node:attempt` form, a grant of another person, an older attempt
and a grant of a closed request all fail the same way.

## Keys and rotation

The grant key is **derived from the site's key-encryption key** (`[services.secrets]`) by key id:
no key sits in a settings file. To rotate, put the new id first in `key_ids` and keep the old one
for 15 minutes (the longest a grant lives), then remove it. Without `[services.secrets]` no grant
is issued or accepted and readiness says so (`process_governance_keys` degraded); a write to a
governed type is then refused unless the person holds the outside-process permission or is an
allowed administrator.

## Draft records

A form step stores what was typed as a draft record in `spot_bpm_gov_draft`: it is not in the
DocType's table, in lists, in search or in reports.

- Only the **requester** can read, change or submit it while it is a draft.
- Once **submitted**, the requester and the **participants of the Run** (the people the Run handed
  a step to) can read it.
- Anybody else gets exactly the answer an unknown id gets.
- A draft nobody touched for `draft_retention_days` is removed by the holder of the lease.

## The ledger

| Table | Holds |
| --- | --- |
| `spot_bpm_gov_request` | one row per step handed a grant: the attempt, the types it may write, closed or not |
| `spot_bpm_gov_grant_use` | one row per consumed grant, written with the governed write |
| `spot_bpm_gov_audit` | one row per refusal, override and policy failure: who, what, which rule |
| `spot_bpm_gov_draft` | the draft records |

The tables are created if absent when the site starts. An override also rides the outbox as a
`sys_bpm_governance` event in the write's transaction; a refusal's row is written on its own
connection because the write's transaction is rolling back.

## What an operator sees

- `process_governance` in readiness: degraded when the tables are not usable;
  `process_governance_keys`: degraded when no grant key can be derived (no `[services.secrets]`).
- Logs: a stable rule token and ids, never a reason, a grant, a key or a document value.

## Known limits

- The guard is a transaction participant. The services registry has no hook for a module to add a
  participant, so until the host's participants table installs `GovernanceHost::participant()`
  the module is built, tested and ready but **not enforcing**.
- The Document node and the form step must ask `GrantHost` for their grants; until they do, a
  Run's writes to a governed type are refused.
