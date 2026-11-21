Part of [Running Spot](../running-spot.md).

# Undo and governance

When [governance](process_governance.md) is on for a record type, a direct `cancel` or `delete`
of such a record is refused unless the call carries a **grant**: a command id the guard verifies
for the acting person and the current attempt of a step of an open request. The undo of
[process effects](process_effects.md) reverses records through the same lifecycle, so without a
grant the guard refused the engine's own reversal and the undo ended in `needs_attention`.

## What the module does

`GrantedReversal` (`services/process_effects/granted.rs`) sits between the undo's once-per-receipt
journal and the live port. For a record whose type governance governs it:

1. opens a grant from the governance module's `GrantHost` for the step `undo` of the request,
   for that DocType, to the acting person (never the system);
2. passes the grant to the lifecycle call as the command id, in place of the request key.

Every other record type, and any site where the governance module is not hosted, behave as
before: no grant, the request key as command id. The set of types is read from
`[services.process_governance].governed` (entries with mode `governed` or `process_only`, any
scope: the effects module has no company, so a grant is requested for a type governed anywhere on
the site; a grant for a type the guard then does not govern is harmless).

## Exactly once across a crash

The grant carries an expiry, so a resumed call cannot rely on the engine answering a stored
result for an equal command id. Before the lifecycle call the module writes a `started:<key>`
mark into the effects ledger. A resumed call that finds the mark and no receipt reads the
record's current version:

- still the version the entry left it at: the lifecycle did not commit, the reversal runs again
  with a **new** grant (an expired grant is never reused);
- anything else: the call may have committed, so the undo stops for a person with
  `effects.governed_reversal_unconfirmed`. It never reverses twice and never guesses a receipt.

## What an operator sees

A reversal the guard or the grant refuses stops the undo for a decision exactly like any other
refusal, with the guard's own code as the reason: `bpm_governance.outside_process_denied` (the
guard did not accept the grant), `bpm_request_authority.attempt_not_current` (the request is
closed or the step moved on) or `effects.governed_reversal_unconfirmed` (see above).

A request the governance module has **closed** (it closes a request when its Run ends) cannot
open a grant, so an undo started after the Run ended is refused with
`bpm_request_authority.attempt_not_current` until governance keeps the request open for its undo.
No setting changes this here; it is reported as an open item for the governance module.

Nothing in this module logs a grant.
