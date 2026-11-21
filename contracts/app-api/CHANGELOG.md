# spot-app-api changelog

Every change is an ADR plus a semantic version bump (blueprint §4, contracts-first rule).

## 0.7.0 (draft, ADR-068 amendment A4, ADR-057 amendment; ADR-083, the architecture owner approved this contract amendment on 26 Oct 2026)

Two field-model additions for the consumer identity design (ADR-083), both additive:
- `FieldDef::create_only: bool` (default false): the field can be set only when the document is
  created ("settable only at creation", ADR-083 barrier 4). No operation on an existing document
  changes the stored value, by any channel: update, bulk update, import, sync apply, SCIM, a
  method writing through the document service. A write that carries the value the field already
  has is not a change and is accepted (a client may send the whole document back); a different
  value, or a clear, is refused with the existing `document.read_only_field` on that field and
  writes nothing. No new error code. Two `meta.*` definition errors: `meta.create_only_table`
  (a child table is not a stored value) and `meta.create_only_allow_on_posted` (a field never
  changed after creation cannot be one a posted document may change). `FieldDef::new` leaves it
  false. The reference stub enforces it in `update`.
- `FieldKind::ConsumerRef`: the owner scope of a consumer-caused record (ADR-083 section 2.2 item
  4). It holds the consumer's id (`Value::Id`, like a link's value) but is **not** a link: no
  target DocType, no referential check, no lookup, no join, because the consumer lives in the
  consumer plane's own store. `meta.consumer_ref_duplicate` (a DocType has one) and
  `meta.consumer_ref_not_create_only` (changing it would move the record to another consumer).
- `Actor` (re-exported from the events contract 0.6.0) gains `Consumer { id }`; `PrincipalKind`,
  `PrincipalRef` and `SYSTEM_PRINCIPAL` are re-exported from `spot_app_api::context` and the
  crate root, so apps and the engine use one definition. An `Invocation` whose actor is a
  consumer is an ordinary invocation; the reference stub runs it and its events name the consumer.
- Contract cases join the suite (48 in total): create-only is set at creation and never after
  (and shows in the host's view); the two definition rules; `ConsumerRef` accepts only an id, is
  create-only and unique; a consumer actor runs an operation and is audited as `consumer:<id>`; a
  0.6.0 definition built with the constructor passes unchanged.
- `CONTRACT_VERSION` brought to 0.7.0.

Compatibility: **additive in behaviour and on the wire.** A 0.6.0 DocType, written with
`FieldDef::new` and no knowledge of either addition, compiles, validates and behaves exactly as
before (`create_only` false, no `ConsumerRef`). `FieldDef` is already `#[non_exhaustive]`, so the
new field costs no struct-literal migration outside this crate, and `FieldKind` is
`#[non_exhaustive]`, so a match on it already has a wildcard; the workspace code that maps a kind
to a wire type or a storage column answers an unknown kind with an error (it fails closed), which
is why a `ConsumerRef` cannot be used until the metadata compiler and the storage mapping learn it
(follow-up tasks, listed in ADR-068 A4). The `Actor::Consumer` variant is the one source break,
described in the events contract's 0.5.0 entry.

## 0.6.0 (draft, ADR-16 amendment, WP-DOC-11.T2; the architecture owner approved this contract amendment)

`MethodDecl` carries the right its method needs, so the engine can authorise a whitelisted call by
the operation it declares instead of guessing `read` or `update` from `mutates`:
- New `MethodRight`: `Read`, `Write`, `Create`, `Submit`, `Cancel` and `Permission(Name)` for an
  app-registered permission. `#[non_exhaustive]`. On the wire it is the native API's `right`
  value (`"read"`, `"write"`, `"create"`, `"submit"`, `"cancel"`) or
  `{"permission": "<name>"}`; a permission name is validated like a storage name on decode, so an
  unknown or malformed right is refused rather than defaulted.
- `MethodDecl` gains `right: MethodRight`. `MethodDecl::new(name, mutates)` derives it — `Read`
  when the method does not change documents, `Write` when it does — and
  `MethodDecl::with_right(right)` states a finer one without touching `mutates`.
- The consistency rule, documented on `MethodRight` and answered by `MethodRight::contradicts` /
  `MethodDecl::contradicts`: `Read` requires `mutates == false`; `Write`, `Create`, `Submit` and
  `Cancel` require `mutates == true`; a `Permission` implies `mutates == true` unless the
  declaration states otherwise (a named permission may guard a read-only method).
- `contract_tests::check_method_rights(&dyn Controller)` — the contract's test kit — fails a
  controller whose declared right contradicts `mutates`, naming the DocType and the method, so a
  host can refuse it at registration.
- Contract cases join the suite (43 in total): the right derives from `mutates` and round-trips
  through the wire form; a contradicting declaration is refused by the helper while a permission
  method is accepted either way; and a 0.5.0-shaped controller (declarations with no right)
  registers, passes the helper and runs on its defaults.
- `CONTRACT_VERSION` brought to 0.6.0.

Compatibility: **additive in behaviour and on the wire.** Nothing is removed or renamed, and a
controller written against 0.5.0 compiles and passes unchanged, because a declaration that does not
set the right gets the default above. It is, however, a *source* break for `MethodDecl { name, mutates }` struct literals, so the nine in
the workspace (the macro crate, `spot-ts-controllers`, `spot-http`'s fake host, `spot-document`'s
authz test, `spot-controllers`' tests and two harness crates) move to `MethodDecl::new` in this same
change, and `MethodDecl` is now `#[non_exhaustive]`: a literal outside this crate is no longer
possible, so the next field on it costs nobody a migration. The TypeScript adapter carries the
`right` its module declares (`read`, `write`, `create`, `submit`, `cancel`, otherwise an app
permission). The macro still emits `MethodDecl::new` (the derived right); emitting the declared
right and enforcing it in the engine is WP-DOC-11.T3.

## 0.5.0 (draft, ADR-080 section 6)

Effect declarations and operation receipts (WP-EXT-11.T1). Additive only; nothing from 0.4.0
removed or renamed, and a controller written against 0.4.0 compiles and passes unchanged:
- New module `effects`. `EffectDecl { kind, class, writes, reversal }` with
  `EffectKind::{Internal, External}`, `Reversal::{ReversibleBy(Operation), CompensableBy(ActionRef),
  Irreversible { reason }}` and the summary `Reversibility`. Names are validated:
  `EffectClassName` and `StoreName` like a storage name, `ActionRef` as `<app>:<action>`.
- Rules, in `EffectDecl::validate` and `validate_at(&HookPoint)`: an internal effect must be
  `ReversibleBy` (refused otherwise, `effects.internal_not_reversible`), writes at least one store,
  runs in phase 3, and on `post` is reversed by `cancel` (`reversing_operation`; `correct` is
  cancel plus a new draft). An external effect runs in the committed phase and may be any
  reversal; an irreversible one needs a reason. `validate_declarations` checks a controller's
  declarations against its steps.
- Receipts: `OperationReceipt { operation, doctype, name, version, effects, committed_at }`,
  `EffectReceipt { step, class, kind, store_refs, count, digest, state }`,
  `ReceiptState::{Applied, Held, Released, Delivered, Failed { code }}` (`#[non_exhaustive]`),
  `OpaqueRef` (1 to 128 bytes; consumers must not interpret it) and `Hex32`. Serde with stable
  snake_case names; the timestamp is microseconds since the epoch. `StepName` gains serde (as
  its string form).
- `HookContext::report_effect(&mut self, EffectReceipt)` with a default no-op. Only declared
  effects may be reported (`EffectReceipt::matches_declaration`).
- **Deviation from the brief, decision needed:** `StepDecl` is NOT given an `effects` field,
  because ~25 struct-literal call sites in other crates would stop compiling and the task may not
  edit them. Declarations travel beside the steps instead: `StepEffects { step, effects }` and
  `Controller::step_effects()` (default empty). When implementers can touch those sites, an
  `effects` field plus `with_effects` can be added as a follow-up; `step_effects` can then default
  to reading it.
- `CONTRACT_VERSION` constant brought to 0.5.0 (it still read 0.3.0).
- Contract cases join the suite (40 in total): declaration validation, placement and pairing,
  receipt round trip, default `report_effect` is a no-op, a 0.4.0-shaped controller passes.
- Not done here (implementers): the engine collecting reports into the receipt and returning it
  from `DocumentService` (`Outcome` is built literally by hosts, so a field there is a later
  breaking-risk decision); TypeScript mirror in `spot-ts-controllers`; there is no WIT form of
  `StepDecl`, so nothing to mirror there.

## 0.4.0 (draft, ADR-057 amended by ADR-068 amendment A2)

The app half of native API 0.5.0 (WP-CON-02.T4): metadata can now carry a multi-valued
select and a control hint. Additive only; nothing removed or renamed:
- `FieldDef` gains `multiple: bool` (default false): a `select` that holds several of its
  values; its stored value is an array of the chosen texts. `DocTypeDef::validate` refuses it
  on any other kind (`meta.multiple_not_select`, with a remedy in `meta.toml`). The contract
  has no per-field `unique` property (uniqueness is declared elsewhere), so the brief's
  "multiple with unique" refusal belongs to the layer that declares uniqueness, not here.
- `ControlHint` (`#[non_exhaustive]`): `Switch`, `Checkbox`, `Dropdown`, `Radio`, `Segmented`,
  `Autocomplete`, `Chips`, `Textarea`, with `as_str` and `parse` equal to the native API's
  `FieldHints.control` values. `FieldHints` gains `control: Option<ControlHint>`; it is a hint
  and `validate` never refuses one.
- Contract cases join the suite (35 in total): a `multiple` select round-trips; `multiple` on
  a non-select is refused; each control hint's string form equals the native API value and a
  field built with the 0.3.0 constructor has `multiple == false` and no hint.

## 0.3.0 (draft, ADR-057 amended by ADR-068 amendment A1)

The security layer of the field model (WP-CON-02.T3), carrying ADR-068 amendment A1 so
metadata can declare protection (ADR-070 section 12). Additive only; `permlevel` is
deprecated, not removed:
- `GroupName` and `Tag` newtypes validated like a storage name.
- `MaskFormat::{Placeholder, LastChars(u8)}` — how a masked value is shown; `LastChars`
  counts 1..=8 characters.
- `ProtectionGroupDef { name, label, trace, sync_class_hint }` with `SyncClassHint` one of
  `Everywhere`, `Encrypted`, `Sealed`, `HomeOnly` (a default only; the tenant decides,
  ADR-070 rule 12.13).
- `FieldSecurity { protection_group, classification, masking, declassified, inherited_groups
 }`; `FieldDef` gains `security` (default empty) and `DocTypeDef` gains `protection_groups`
  (default empty). `inherited_groups` is computed by `spot-meta`, never authored — a
  definition that sets it by hand is refused (`meta.inherited_authored`).
- `DocTypeDef::validate` reports, all at once: a field naming an undeclared group
  (`meta.unknown_group`); a masked group member without a `MaskFormat` (`meta.mask_missing`)
  or a malformed one (`meta.mask_invalid`); a `declassified` with an empty reason
  (`meta.declassify_reason`). The derived-field and tag lints need compiled expressions and
  live in `spot-meta` (WP-MET-01.T6).
- `FieldHints::permlevel` is deprecated in the docs and ignored by every consumer; a loader maps an
  imported `permlevel n >= 1` to the group `level_n` (removed at the next breaking version).
- Three contract cases join the suite (32 in total): a protected field round-trips; an
  undeclared group is refused; `declassified` without a reason is refused.

## 0.2.0 (draft, ADR-057 amended by ADR-068)

The field model, formats, configurable lookups and controls (WP-CON-02.T2):
- `FieldKind` gains `Select { values }`, `DynamicLink { type_field }`, `Attach { format }`,
  `Secret`, `TableMultiselect { doctype, link_field }`, and carries a `format` on `Int`,
  `Decimal`, `Text`, `LongText` and `Json`; `Decimal` carries `precision`; `LongText` is
  separate from `Text`. The enum is `#[non_exhaustive]`.
- The format enums `TextFormat`, `LongTextFormat`, `IntFormat`, `DecimalFormat`, `JsonFormat`
  and `AttachFormat` (each `#[non_exhaustive]`).
- `FieldDef` gains `label`, `read_only`, `default` (a parsed `Value`), `lookup` and `hints`
  (`FieldHints`: description, hidden, in_list_view, in_standard_filter, no_copy, permlevel).
- `lookup.rs`: `LookupSpec` and `LookupRule` (the data), the seam wire types
  `LookupRequest`/`LookupRow`/`LookupPage` in the ADR-055 value encoding, `lookup_seam` and
  the `platform:link_search` default. A lookup suggests; the engine still validates; it runs
  under the caller's permissions; failure is a typed error with no silent fallback. `when`
  rules are kept, not evaluated (WP-UI-11).
- `accepts`: `Select` accepts `Text` (the listed-values check is the engine's); `LongText`
  accepts `Text`; `DynamicLink` and `Attach` accept `Id`; `Secret` accepts only `Null` (a
  secret never travels in a document); both table kinds accept nothing.
- The stub host validates a select value against `values` on write
  (`document.value_not_listed`) and refuses a write to a `Secret` field
  (`document.secret_not_writable`).
- `spot-meta` compiles the whole field: labels, `read_only`, precision, parsed defaults,
  select values and dynamic links (no longer skipped from the app view).
- Five contract cases join the suite (29 in total): the ones above.

Breaking change to the frozen v0.1 contract (ADR-068 §6): every consumer of `FieldKind`
must move with it — `spot-http`'s test fake, the stub host and the contract suite do.

## 0.1.0 (draft, awaiting architecture-owner approval)

First version (WP-CON-02.T1, ADR-057):
- the dynamic document handle, with change tracking and ADR-055 merge patches;
- typed views;
- lifecycle order and phase rules as data, with *correct* as one transaction;
- controllers with named steps and whitelisted methods;
- step overrides (before, after, wrap, replace, disable);
- JSON-boundary seams;
- the repository, hook and method contexts;
- job, event, realtime and cache facades;
- typed errors (one pointer wide) and messages, and the storage-error mapping table;
- `DocumentService` for hosts;
- the in-memory stub host;
- the 24-case contract suite.
