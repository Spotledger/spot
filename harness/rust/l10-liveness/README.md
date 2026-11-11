# spot-l10-liveness (WP-TST-11)

The **L10 metadata liveness suite**: 16 scripted scenarios proving that every metadata change a
site makes takes effect **without a rebuild or restart** and is visible to *every layer* within
the same request (ADR-05; blueprint test layer L10, "UI customizations take effect without
restart"). Where `spot-meta`'s own tests pin one operation in isolation, this suite pins the
**combinations and the liveness cross-layer promise**: immediate visibility, version arithmetic,
both compiled views agreeing, refusals leaving nothing behind, app updates preserving site
customisations, deterministic replay, and reader-thread snapshot consistency.

The suite is **host-neutral**. It compiles against `spot-meta`'s public API only, so it can
never touch engine internals; no floats, no `unwrap`/`expect` outside tests, no dependencies
beyond `spot-meta`. It runs today on `spot_meta::Registry` through the `MetaHost` adapter in
`tests/meta.rs` (with `StandardHost`/`StandardFactory` shipped in `src/host.rs` as the
reference implementation). The layers that will satisfy the cross-layer halves later —
`spot-plan` (WP-MET-02), the schema migration engine (WP-STO-11) and the view layer (WP-UI-10)
— each plug in through a **probe** and run the *same* scenarios.

## How it is built

- **`LivenessHost`** (`src/lib.rs`) — the contract, three operations:
    - `apply(&Change) -> Result<ChangeEffect, HostError>`: one atomic change. On `Err` nothing
      may have changed — the metadata, its version and the customisations stay exactly as
      before (ADR-05: a refused edit never leaves a half-applied site).
    - `meta() -> Arc<EffectiveMeta>`: the currently effective, merged metadata. Immutable — a
      reader holding a previously returned `Arc` keeps a consistent snapshot while further
      changes land.
    - `probe(Probe) -> ProbeOutcome`, default `NotSupported`: asks an optional layer (plan,
      schema, view) whether it *already* sees the metadata the last `apply` produced. A host
      without the layer answers `NotSupported`; scenarios skip that part and record it in the
      outcome's notes, so the report always says which layers participated.
- **`HostFactory`** — a fresh host per scenario (`run_all(&factory)`, the same shape as the L3
  suite's `Fixture`), so scenarios never see each other's changes. `run_one(&factory, name)`
  runs one scenario by name — the seam per-scenario tests (including `#[ignore]`d ones) use.
- **`Change`** (`src/change.rs`) — the five runtime changes, in suite input types (never the
  host's constructor shapes, so a scenario never depends on internals):
  `AddCustomField`, `RemoveCustomField`, `SetProperty`, `AddDocType` (a runtime-created
  DocType, ADR-15), `ReplaceDefinitions` (an app update). `ChangeEffect` carries the new
  metadata version; `Refusal` is the typed refusal, carrying the colliding field name where a
  host has one to give.
- **`run_all(factory) -> Report`** (`src/scenarios.rs`, `src/report.rs`) — every scenario on a
  fresh host, each outcome carrying pass/fail plus `notes` (probe participation, recorded wall
  time). Notes never turn a failure into a pass.
- **`SEQUENTIAL_BUDGET`** — the per-change bound the 200-change scenario gates at: **500 ms**,
  generous by design. L10 proves liveness, not speed (that is L9's job); the scenario records
  the total wall time in its notes and fails only a gross regression (a change that costs a
  rebuild-shaped fraction of a second).

## The scenarios

Every scenario cites the decision record it enforces, in its comment and here. "Host layers"
reads: **meta** = the merged metadata every host has; **plan**/**schema**/**view** = the
optional probes — a host without the layer records `NotSupported` and the report says so.

| Scenario | Pins | ADR | Host layers |
| --- | --- | --- | --- |
| `custom_field_is_immediately_visible_and_version_moves_by_one` | A custom field is in the effective definitions immediately; the version rises by exactly 1 per accepted change (add, override, remove). | ADR-05 | meta |
| `custom_field_reaches_catalog_and_doctype_views_in_one_version` | One change lands in both compiled views (storage catalog, app API `DocTypeDef`) stamped with the same version. | ADR-05, ADR-068 §6 | meta, schema probe |
| `property_override_takes_effect_and_touches_nothing_else` | An override moves exactly the property it names; no other property, field, or DocType's view content moves (the version stamp moves by design). | ADR-068 §1 | meta |
| `removing_a_custom_field_updates_both_views_and_shares_unrelated_views` | Removal reaches definitions and the app view, and an unrelated DocType's view keeps the **same `Arc`**. | ADR-05 | meta, schema probe |
| `runtime_doctype_appears_with_engine_fields_and_a_valid_catalog` | A runtime-created DocType appears immediately; a posted one carries `doc_state`/`amended_from`; a custom field can be added to it. | ADR-15, ADR-05 | meta, schema probe |
| `doctype_name_collision_is_refused_and_the_version_does_not_move` | Creating a DocType whose name collides with an existing one is refused; the version does not move, the old `Arc` stays held. | ADR-05 | meta |
| `a_refused_change_leaves_the_previous_metadata_fully_intact` | Four refused changes (unknown DocType, bad prefix, lowered `max_chars`, non-custom removal) leave version, every view, and earlier customisations intact — same version **and same `Arc`**. | ADR-05 | meta |
| `app_update_adding_a_standard_field_keeps_site_customisations` | An app update (`ReplaceDefinitions`) adds a standard field without clobbering the site's custom field or override. | ADR-05, ADR-068 §1.1 | meta |
| `app_update_removal_succeeds_and_a_colliding_update_is_refused_by_name` | Removing an unreferenced standard field succeeds; an update that would make a site's custom field name collide is refused **with the colliding field reported**, and the site's field stays. | ADR-05 | meta |
| `commuting_changes_agree_in_either_order_and_non_commuting_ones_differ_deterministically` | Commuting pairs (custom fields on different DocTypes; an override and an unrelated field) give identical final metadata in either order; same-property overrides differ by order, each order deterministic (later wins). | ADR-05 | meta |
| `replaying_a_change_sequence_from_empty_metadata_reproduces_the_same_version_and_views` | The same sequence from empty metadata reproduces the same final version and the same views byte for byte. | ADR-05 | meta |
| `readers_holding_an_old_snapshot_see_a_consistent_metadata_while_changes_land` | Reader threads (`std::thread`, barrier + joins, no sleeps) holding old `Arc<EffectiveMeta>` snapshots see internally consistent metadata (every view carries the snapshot's version) while a writer applies 10 changes. | ADR-05 | meta |
| `two_hundred_sequential_changes_stay_within_the_recorded_per_change_budget` | 200 sequential changes complete, each within `SEQUENTIAL_BUDGET` (500 ms); the total wall time is recorded, gated only at the gross bound. | ADR-05 | meta |
| `allow_on_posted_override_is_reflected_in_the_app_api_view` | Changing a posted DocType's `allow_on_posted` flag lands in the app API view, both directions. | ADR-068 §1.3, §6 | meta |
| `engine_reserved_field_names_cannot_be_custom_field_names` | `doc_state` and `amended_from` are refused as custom field names; nothing moves. The scenario asserts refusal and an unmoved version — not which Refusal kind, which no ADR fixes. | ADR-068 §1.1, ADR-05 | meta |
| `every_supported_probe_sees_the_new_version_within_the_same_apply` | Every probe the host supports sees the new version within the same `apply` call ("within one request"); unsupported layers are recorded. | ADR-05 | plan, schema, view probes |

## Running it

```
cargo test -p spot-l10-liveness
```

Three tests, in `tests/meta.rs`:

- `spot_meta_registry_passes_the_liveness_suite` — runs all 16 scenarios on the `MetaHost`
  adapter and requires **15 passes plus exactly one recorded failure**: the sharing scenario,
  whose failure must be the same-`Arc` assertion itself (checked by name and by message, so a
  different regression cannot hide behind the exclusion).
- `the_satisfiable_scenarios_pass_individually` and
  `the_standard_factory_passes_the_satisfiable_scenarios` — the 15 pass one by one, through
  both the test-side `MetaHost` adapter and the library's `StandardFactory`.
- `the_unshared_views_scenario_fails_only_on_sharing` — `#[ignore]`d (see Open questions): the
  full scenario run plus a direct `Registry` demonstration that a change rebuilds even an
  unrelated DocType's view `Arc`, i.e. the failure is *only* the sharing gap. Run it with
  `cargo test -p spot-l10-liveness -- --ignored`.

## What is and is not asserted

Every scenario asserts only behaviour an ADR fixes. Where behaviour is *not* fixed — which
`Refusal` kind reports a reserved name, whether a host's message wording carries a property —
the scenario accepts every allowed shape and records what this host said. Contract-level
refusals are asserted structurally (the `Refusal` kind, the colliding field name from
`HostError::refusal`), never by message text.

## How to add a host adapter (WP-MET-02, WP-STO-11, WP-UI-10)

1. **Depend on `spot-l10-liveness`** (path: `harness/rust/l10-liveness`) from a `tests/`
   integration test in your crate.
2. **Implement `LivenessHost`** over your stack:
    - `apply`: map each `Change` onto your layer operations, atomically. On refusal return
      `HostError { refusal }` — the closest kind, carrying the colliding field name for a
      `FieldCollision` — and leave *everything* unchanged, including your own layer's state
      (the refusal scenarios then hold on your host too).
    - `meta()`: return the merged metadata as an `Arc<EffectiveMeta>`. If your layer is not
      `spot-meta`, build your own `EffectiveMeta` with `spot_meta::Registry::build` from the
      definitions your stack serves — the suite reads only the public
      `version`/`definition`/`doctype`/`doctypes` surface.
    - `probe`: answer `NotSupported` for every layer you do not own. For the layer you do
      own, return `Live { version }` **after** the change is durable in your layer; the
      scenarios compare this against the version `apply` returned, i.e. "within one request".
      Never return `Live` with an older version — a stale probe fails the scenario on
      purpose; that is the cross-layer liveness promise.
3. **Start from the suite's DocTypes**: every scenario runs against the standard two DocTypes
   (`party`, `invoice`) the suite ships; a host that cannot serve arbitrary definitions maps
   them in its adapter's constructor (see `StandardHost::new` for the reference conversion).
4. **Run the suite** and pin the counts:
   ```rust
   struct MyFactory;
   impl HostFactory for MyFactory {
       fn fresh(&self) -> Box<dyn LivenessHost> { Box::new(MyHost::new()) }
   }

   #[test]
   fn my_layer_passes_the_liveness_suite() {
       let report = spot_l10_liveness::run_all(&MyFactory);
       assert_eq!(report.outcomes.len(), 16);
       report.assert_all_passed();
   }
   ```
5. **Do not weaken a scenario.** If one cannot pass on your host because a piece is genuinely
   missing, that is a finding for your work package: `#[ignore]` it with the reason in the
   test, list it here and in your PR's Open questions, and keep a test that runs the scenario
   and shows what exactly fails (the pattern of
   `the_unshared_views_scenario_fails_only_on_sharing`).

## Open questions

1. **`Arc` sharing of unrelated DocType views (the one `#[ignore]`).** The brief's scenario
   asks that an unrelated DocType's compiled view be the *same `Arc`* after a change to
   another DocType. `spot_meta::Registry` rebuilds the whole `EffectiveMeta` — every view
   `Arc` — on every change (its `change` helper), so the scenario cannot pass on the registry
   adapter today. It is excluded from the passing run, `#[ignore]`d in
   `the_unshared_views_scenario_fails_only_on_sharing` with the reason, and its other
   assertions (definitions, both views' contents, removal semantics) still pass there.
   Sharing unchanged views is a `spot-meta` follow-up (an efficiency property, on top of
   ADR-05's snapshot consistency, which already holds); it is not something this suite can
   provide.
2. **Schema probe on `spot-meta` today.** `EffectiveMeta::catalog(collation)` compiles the
   storage catalog on demand, but no persisted "what the schema engine would apply next" state
   exists (that is WP-STO-11's deliverable). The scenarios therefore probe through the host
   and record `NotSupported` on the registry adapter; the *content* half (a custom field
   reaching the compiled catalog) is pinned by `spot-meta`'s own tests.
3. **Plan and view probes.** Neither layer exists yet (WP-MET-02, WP-UI-10). S16 records all
   three layers as `NotSupported` on the registry adapter; when a layer lands, its adapter
   makes the same scenario assert the version it sees with no suite change.
4. **Per-change budget.** 500 ms per change is a gross liveness bound stated here per the
   brief ("generous"); real performance budgets belong to L9 (the scale suite).