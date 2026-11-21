# spot-sealed-fields

## Node write binding

`NodeFieldStore::new(node, tenant, doctype, hub_key)` binds the store to one tenant and one
DocType. `NodeFieldStore::write(&SealTarget, plaintext, &SealedAd)` refuses, on the node and
before anything is sealed, stored or synced, a write whose associated data is not bound to the
store's tenant and DocType and to the field and document named by the `SealTarget`, or whose
`SealTarget` names a hub key version (the hub public key) other than the one the store seals
to. The tenant and DocType are never parameters of `write`. `EncryptedStore::write` refuses an
associated data whose key epoch is not the epoch of the call. The hub keeps its own check:
`unseal` authenticates the same binding.

## Channel contract

No plaintext of a sealed or hidden field leaves through a secondary channel. Redaction is by
construction, not by a filter applied afterwards:

- `Sealed` debug-prints `[sealed]` and never its bytes; `SecretBytes` prints `[redacted]`.
- `SealedFieldsError` and `RevealError` messages name groups, fields and nodes only.
- Audit records store field names and metadata only, never a value.
- Log lines of this crate carry node, group, field and document ids only.
- A sync batch carries a sealed field only as its ciphertext (`OutgoingValue::Sealed`); a
  `home_only` field is dropped and a `sealed` field is dropped for a branch.
- An unregistered channel emits nothing (`ChannelUnregistered`).

Tests: `tests/channel_leaks.rs` drives a marker value through the crate's own channels (error
`Display`/`Debug`, audit record, log line, sync batch, stored forms) and looks for the marker,
its base64, its hex and its length.

### Channels this crate does not own

Logs of other crates, error messages, audit records written by other crates, outbox and
journal events (`journal_stream`, `realtime_event`), search (`search_index`, `embedding`),
exports (`export`, `report`, `print`), API and SCIM output, notifications, bootstrap snapshots,
cache keys and values (`cache`) and backups must follow the same rule:

1. Ask `ChannelGuard` (through `ChannelRegistry::emission`) before emitting a protected field.
   `Drop` omits the field, `Mask` writes the masked form only, `Emit` writes the stored form;
   only a hub channel (`Destination::Hub`) may carry a value, and only through the audited
   reveal and the caller's rights.
2. Never print a value, its base64, its hex or its length in a log line, an error, a cache key
   or an event; a cache key is built from ids, never from a value.
3. Register with `GuardedChannel` and `ChannelRegistry::register`; until it does, `emission`
   returns `ChannelUnregistered`.

`tests/channel_doubles.rs` holds a documented test double that renders through `ChannelGuard`
exactly as above, one test per name in `ADR_CHANNELS`. It shows the rule; it does not test the
other crates' code. A leak in another crate is reported against that crate's own tests.
