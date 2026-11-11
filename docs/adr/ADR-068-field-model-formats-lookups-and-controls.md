# ADR-068: The field model: fields as data, formats, configurable lookups, and controls kept out of fields

- **Status:** Approved by the architecture owner, 3 Oct 2026, including the ADR-057 amendment (app API 0.2.0). **Amendment A1 (6 Oct 2026, requested by the architecture owner with ADR-070): proposed, replaces `permlevel`; see the end of this document.** The open questions below were answered by approving the assumptions stated in the text; question 1 (several roles) is provisional and will be revisited before WP-UI-10 starts.
- **Date:** 3 Oct 2026
- **Layer:** Platform (WS2, metadata)
- **Builds on:** ADR-05, ADR-08, ADR-16, ADR-17, ADR-32, ADR-49, ADR-053, ADR-055, ADR-057
- **Work packages:** WP-MET-01 (follow-up task), WP-CON-02 (contract amendment), WP-MET-02, WP-UI-10, WP-UI-11, WP-UI-14
- **Code today:** `crates/framework/spot-meta/`, `contracts/app-api/src/meta.rs`

## Context

A review of `spot-meta` (WP-MET-01.T1, merged) found four problems:

1. **The field vocabulary is thin.** There are 14 field types and no formats. Everything an ERP needs beyond text, number and date (money in a currency, e-mail, rating, duration, files, secrets, rich text, a multi-pick list) has no home.
2. **The app view loses information.** `DocTypeDef` keeps only `name`, `kind`, `required` and `allow_on_posted` per field. The values of a `select`, `read_only`, the default, the label, decimal precision and the difference between `text` and `long_text` are all dropped. A `dynamic_link` has no `FieldKind` and is skipped (`compile.rs`). The document engine and the UI therefore cannot see or enforce what the definition says.
3. **A link always means one lookup.** Frappe's link field always calls one search function (`frappe.desk.search_link`). That worked for Frappe. Here, a field must be able to ask a different operation for its candidates, sometimes only on one form, sometimes only when a condition holds, and this has to be configuration, never a code change (ADR-05).
4. **Layout and data are mixed in Frappe's model** (Section Break, Column Break, Button and the like are fields). We do not copy that, but the question of where those components live has to be settled before the UI work starts.

A fifth point is a way of thinking rather than a defect: **a field is itself data.** It has properties, those properties have rules, and users and tools change them at runtime. `FieldSpec` already behaves that way for custom fields and overrides, but nothing says so, and the properties are not described as data anywhere.

ADR-49 already decided that a **Process** is the only workflow concept, that it is first-class in the framework core, and that it is itself data. This ADR does not redesign it. It fixes where a DocType and a Process meet.

## Decision

### 1. A field is a record of properties, and the property set is data

1. A field's definition is a **record** (a document of the built-in DocType `field`, with `doctype` and `custom_field` records around it). `spot-meta` stays the **compiler and validator** of those records; editing goes through the same document engine, API, permissions, audit and AI tools as any other data. How the records are persisted is WP-MET-01.T2; this ADR only fixes the principle so the format does not close it off.
2. Each property has a **descriptor**: its name, the field types and formats it applies to, its **layer** (below), and whether a site may override it. Validation, the admin UI, the API schema and the override rules are all derived from the descriptors. There is no separate list of "overridable properties" in code.
3. Properties fall in four layers:

   | Layer | Examples | Rule |
   | --- | --- | --- |
   | Identity and storage | `name`, `type`, `format`, `scale`, `precision`, `max_chars`, `unique`, `index` | Fixed once a field exists. A site cannot change them (ADR-05, as now). |
   | Rules | `required`, `read_only`, `allow_on_posted`, `default`, `options`, `depends_on`, `mandatory_depends_on`, `read_only_depends_on`, `fetch_from` | Overridable under the existing rules (select values only grow, `max_chars` only rises). The `*_depends_on` and `fetch_from` properties are expressions (§3.9). |
   | Behaviour | `lookup` (§3) | Overridable by a site and by a view. |
   | Presentation hints | `label`, `description`, `hidden`, `in_list_view`, `in_standard_filter`, `no_copy` | Overridable by a site and by a view. They never change what is stored. (`permlevel` was listed here; it is replaced by the **Security** layer below, amendment A1.) |
   | Security (amendment A1) | `protection_group`, `classification` tags, `masking` | A site may **add** protection to a field and may never **remove or weaken** protection that an app declared. A view cannot change it. They never change what is stored. |

### 2. Types and formats

The storage types stay few. **A format is a validated, displayed interpretation of a base type**; it never changes the storage type. Distinct types exist only where storage or ownership of the value differs.

| Base type | Formats | Notes |
| --- | --- | --- |
| `text` | `plain` (default), `email`, `phone`, `url`, `color`, `barcode` | Format-checked on save. `barcode` also names the symbology as a property. |
| `long_text` | `plain` (default), `markdown`, `html`, `code` | `code` takes a `syntax` property. `html` is sanitised on save (the rule is a WP-MET-01 task detail). |
| `int` | `plain` (default), `duration`, `rating` | `duration` is whole seconds. `rating` has `max` (default 5). |
| `decimal` | `plain` (default), `currency`, `percent` | `currency` needs `currency_field` (a link to the currency, in the same record or its parent), so rounding and display follow ADR-059. Scale and precision are always explicit; no floats anywhere. |
| `json` | `plain` (default), `geolocation` | `geolocation` is GeoJSON, checked on save. |
| `attach` (new type) | `file` (default), `image`, `signature` | A **reference** to a file record (WP-PLT-10), never the bytes. Storage shape is decided there; this ADR fixes only that the value is a reference with an access check. |
| `secret` (new type) | none | A password or key. The value is held by the secrets service (`spot-secrets`) and is **never** part of the document, its events, its audit diff or an API response. The document holds only whether one is set. |
| `table_multiselect` (new type) | none | A child table whose rows hold one link field; a multi-pick of a master. It stores like a `table`. |

Unchanged: `bool`, `date`, `time`, `datetime`, `select`, `link`, `dynamic_link`, `bytes`, `table`.

- **A format must name only what its base type allows.** A format on the wrong base type is a validation error.
- **`select` values** live in the app view (§6), so the engine can refuse a value that is not listed.
- **No format is Frappe's list copied** (ADR-32): the table above was derived from what the storage and validation layers need, and each entry stays only if a scenario needs it.

### 3. Lookups: how a field gets its candidate values

1. A `lookup` is a **property** (behaviour layer) on `link`, `dynamic_link`, `select`, `table_multiselect` and `text` (suggestions only). It names a **seam** (ADR-08) that returns candidates. The default is the platform seam `platform:link_search`, which searches the target DocType, so a field with no `lookup` behaves like Frappe's.
2. A lookup spec is data:

   ```toml
   [fields.lookup]
   seam   = "billing:customer_search"       # an app-declared seam; default platform:link_search
   params = { only_active = "true" }        # fixed, typed parameters
   [[fields.lookup.when]]                   # optional, evaluated in order, first match wins
   if   = "doc.order_type == 'export'"
   seam = "billing:export_customer_search"
   ```

3. **Seam contract** (versioned, JSON at the boundary like every seam, so native, WASM and script providers are interchangeable):
   - **In:** the DocType and field, the search text, a page cursor, the **current document snapshot** (and parent document for a child row), and the `params`.
   - **Out:** rows of `{ id, label, description?, extras }` and `has_more`. `extras` is a map the form may copy into other fields when a row is picked (the feeding side of `fetch_from`, WP-MET-02).
4. **Resolution order** (first that gives a value wins), shown from most to least specific:
   1. the **view** of the form (§7), which includes the step context of a Process (§8);
   2. the **site override** (a property override on the field, ADR-05);
   3. the field's **definition**, with its `when` rules evaluated in order;
   4. the platform default.
5. **The server resolves it.** The client sends one request (DocType, field, text, cursor, document snapshot) to a single lookup operation of the native API (ADR-055, a new operation, specified in its own task). The server picks the seam. A client never names a seam, so desktop and browser behave identically (the same rule as the form-event endpoint, WP-UI-01) and a client cannot reach an operation it is not allowed to call.
6. **A lookup suggests; it does not validate.** What is stored is still checked as today: a `link` must name a document of the target DocType that exists, and the caller needs permission to link to it (`document.invalid_link`). A seam that returns an id the engine will not accept simply produces a form that cannot save, which is a configuration error to surface, not a security hole.
7. **Declared, not discovered.** Every lookup seam is registered in an app manifest (ADR-17). `Registry::build` takes the set of known seam names and reports a field that names an unknown one, with the DocType and field, like every other validation error.
8. **Failure is explicit.** If the chosen seam fails or is missing at run time, the lookup returns a typed error with an explanation (ADR-26). It never falls back silently to the default, because a silent fallback would show the user the wrong list.
9. **Conditions need the shared expression language** (WP-UI-11). Until it exists, a lookup may be unconditional and may be overridden by a site or a view; `when` rules are parsed and kept but not evaluated, and a definition that uses one is accepted only once WP-UI-11 lands. (This keeps the data format stable from the start.)

The same layered resolution (view, site, definition, default) is how `default`, `fetch_from` and the `*_depends_on` expressions will resolve in WP-MET-02. There is one override mechanism, not one per property.

### 4. Layout is never a field

- A **field** holds a value. Section, column, tab, button, heading, HTML block, spacer and static image are **not** field types and have no storage and no entry in `DocTypeDef`. (`insert_after` on a custom field places it in the default view only; a real layout is the view, §7.)
- They belong to the **view definition** (WP-UI-10): groups, tabs, columns, actions and unbound controls. A custom field is placed in a view like any other field.
- A DocType with no hand-written view gets a **generated default view** (WP-UI-10), so layout stays optional.

### 5. Controls: a catalog, kept apart from field types

The controls you drop on a form (the WinForms toolbox) are a **versioned catalog in the UI contract**, not part of metadata. Each field type and format has a **default control**, and a view may bind another compatible one.

| Control | Binds to |
| --- | --- |
| Text box, multi-line box | `text`, `long_text` (plain) |
| E-mail, phone, URL, colour, barcode box | `text` with the matching format |
| Rich text, markdown, code editor | `long_text` with the matching format |
| Number box | `int`, `decimal` (plain) |
| Currency, percent, duration, rating box | `decimal` / `int` with the matching format |
| Check box | `bool` |
| Date, time, date-time picker | `date`, `time`, `datetime` |
| Combo box | `select` |
| Lookup box | `link`, `dynamic_link` (§3) |
| Multi-pick box | `table_multiselect` |
| File, image, signature pad | `attach` by format |
| Map picker | `json` with `geolocation` |
| Secret box | `secret` |
| Grid | `table` |
| Label, heading, button, panel or group box, tab control, columns, separator, static image, HTML block, chart | **no field** (unbound) |

Each control declares which base types and formats it accepts. A view that binds an incompatible control is a validation error in the view, not at runtime. Third-party controls (tier B, WP-UI-14) join the catalog through the component contract, with the same declaration. A field's lookup, formats and rules are read by every control through the same renderer-neutral contract (WP-UI-07), so a new renderer needs only controls, never new field logic.

### 6. The app view carries the whole field (amends ADR-057)

`DocTypeDef` becomes the complete, versioned description of a field for the engine and the UI. Proposed shape (the exact Rust is a contract task):

- `FieldKind` gains `Select { values }`, `DynamicLink { type_field }`, `Attach { format }`, `Secret`, `TableMultiselect { doctype, link_field }`, and carries `format` on `Text`, `LongText`, `Int`, `Decimal` and `Json`. `Decimal` carries `precision` as well as `scale`. `LongText` is separate from `Text`.
- `FieldDef` gains `label`, `read_only`, `default` (a parsed `Value`), `lookup` (the resolved base spec) and the presentation hints. The `*_depends_on` and `fetch_from` expressions are added by WP-MET-02.
- The enum is marked `#[non_exhaustive]` so later types and formats are additive.
- `FieldKind::accepts` and the stub host's validation grow with it; the `contract_tests` cases for them are part of the change.
- This is a breaking change to a frozen contract: `spot-app-api` goes to 0.2.0, and `spot-http`'s fake, the stub host and the contract suite move with it. It is lead-written (contracts are never written by implementers) and approved by the architecture owner.

### 7. The view is a layer of metadata, not a copy of it

Where fields appear, in what order, and how they are grouped is the **view's** business (ADR-27), never the field's. The `fields` list order in a definition is only the order the **generated default view** uses.

1. **A view** (WP-UI-10) holds groups, tabs, columns, the order of fields and unbound controls within them, grid columns, density, and actions. It binds fields to controls (§5).
2. **Views layer, like properties.** From least to most specific: the app's **standard view** (or the generated default), a **site customisation** of it, a **role view**, and the **step context** of a Process (§8). Each layer stores only its **differences** (move a field, put these fields in a tab, hide that one, add a heading), so an app update that adds a field still shows it, in the standard position, to a site that customised the layout.
   - **Views are per role, never per user.** A person who needs a special layout is given a special role; there is no user-level view. This keeps layout auditable (it is granted like any other access) and keeps support simple.
   - **Workspaces are per module** (accounting, foreign trade and so on), not per role. A workspace shapes the **menu and the layout of its home only**. It is not a security boundary: what a user may open or do inside it is decided by permissions, which stay the **union** of everything the user's roles allow. A workspace lists the processes and DocTypes of its module that the user has access to, and hides the rest.
   - **One role view applies, chosen by priority** (the architecture owner, 3 Oct 2026, provisional). Roles carry a priority order, and a user gets the view of their highest-priority role that has one for that DocType. Role views are **not merged**. The workspace does not decide it, because it belongs to a module, not a role. A user with no role view gets the site customisation or the standard view. The architecture owner will revisit this; see the note under the open questions.
   - **Personal UI state is not a view.** Column widths, collapsed sections, the last open tab and similar belong to the user's client preferences, are never shared, and cannot hide a field the role view shows.
3. **A site or a role view can reorder, regroup, move fields into tabs, hide fields, and add unbound controls.** They cannot **delete** a standard field or change its storage properties: hiding is a view difference and the field stays in the DocType, its data and its API. Anything standard stays standard; only the arrangement is theirs.
4. **Hiding is not security.** A hidden field is still stored, still in API responses and still checked by permissions (field and protection-group grants, ADR-070 section 12, amendment A1); access is a permission matter.
5. **A hidden field must not strand a save.** A view that hides a field that is `required` and has no `default` (and no value another rule sets) is a validation error in the view, reported when the view is saved. The alternative, a form that cannot be saved with no visible reason, is refused.
6. **Lookups and presentation hints can be overridden in a view** too (§1, §3.4); storage properties cannot.
7. A view is merged over the effective metadata at resolution time, so it needs no rebuild (L10).

This is why layout customisation is compatible with the read-only standard fields of §1.1: standard fields are never edited to move them. The view is what moves.

### 8. Where a DocType meets a Process (ADR-49)

- **There is no workflow, state machine or approval property on a DocType or a field.** That is deliberate and consistent with ADR-49: a Process is the only workflow concept. (ADR-42's draft, post and cancel stays a DocType flag; it is a lifecycle, not a workflow.)
- A **Process is data and a DocType**, and refers to DocTypes and their fields by name. The reference runs one way: a process names the DocTypes it works on, not the other way round. Metadata does not hold a `process` property.
- A **document step in a Case** may supply a **step context**: a view-layer override (§7) that applies only while the document is open from that step. For example it can make a field read-only or required, hide it, or switch its lookup to a narrower seam for the approver. This is the one point of contact. It uses the same override layers as everything else, so no new metadata mechanism is needed for processes.
- A step changes a document only through the document's own lifecycle (ADR-49), so a lookup or a rule can never be a way around a validation.
- Governance (a governed DocType can only be created inside a Case) is configuration in BPM-08, not metadata in `spot-meta`.

## Consequences

- **Easy:** a field that gets its candidates from a different operation, per form or per condition, is configuration. New formats are data plus one validator and one default control. A Process can tailor a form without touching the DocType. The engine and the UI see every property of a field.
- **Hard:** the app contract changes (0.2.0) while WP-DOC-04, `spot-http` and the stub host depend on it, so the change must land once and early. `lookup` adds a server round trip per keystroke batch, so the lookup operation needs a performance budget and caching rules. Descriptor-driven validation is more machinery than the current hard-coded checks.
- **Divergences from ERPNext (ADR-04):** layout is not a field; formats instead of many types; lookup is a seam, not a fixed search function; no per-DocType workflow.
- **Work packages after approval** (the lead decomposes them into tasks):
  1. WP-CON-02 follow-up: app API 0.2.0 (§6). Lead lane, contract. First, because others depend on it.
  2. WP-MET-01 follow-up: base types, formats, property descriptors, `lookup` spec and its layers, known-seam validation, compile to both views. Core lane.
  3. WP-MET-02: expressions (`depends_on`, `fetch_from`, `when`) once WP-UI-11 exists.
  4. WP-UI-10: view definitions with control binding, the compatibility matrix and view-level overrides.
  5. A native API lookup operation (ADR-055) with the standard seam `platform:link_search`.
  6. WP-UI-14: tier B controls declare accepted types and formats.
- **Risks:** the format and control lists will grow, and the descriptor table must stay the only place that says what each one allows; `html` and `code` formats open a sanitising question; `secret` and `attach` depend on WP-PLT-10 and `spot-secrets` interfaces that are not final.

## Open questions (answered by approval; question 1 provisional)

1. **Several roles, one DocType (§7.2): provisional.** Decided for now as highest-priority role wins, no merging. The architecture owner will comment later; revisit before WP-UI-10 starts. Alternatives: merge, or a user-selected active role.
2. **Approve the contract amendment (§6)?** It is a breaking change to ADR-057 at 0.2.0. The alternative, adding the variants later one by one, is more breaking changes, not fewer.
3. **A site or view may switch a field to a lookup seam of another app.** Allowed here, because the stored value is validated separately (§3.6). Is that acceptable, or should a seam declare which DocTypes it may serve?
4. **`field` as a built-in DocType (§1.1):** edited as ordinary documents, or read-only records generated from definitions with only custom fields and overrides editable? This ADR assumes the second until WP-MET-01.T2.
5. **Per-keystroke lookup cost:** is a server round trip per search acceptable on the desktop client, or should small, slowly changing masters be cacheable on the client? A seam would declare it. This ADR leaves it out of v1 unless you want it in.
6. **Step context (§8):** confirm that a Process step may only narrow a form (more read-only, more required, hidden, narrower lookup) and not widen it past the user's permissions. That is the assumption here.

## Amendment A1 (6 Oct 2026): protection groups replace `permlevel`

- **Status:** Proposed by the lead on the architecture owner's direction (ADR-070, field-level access approved 6 Oct 2026). Awaits the architecture owner's approval as a contract-affecting amendment.
- **Raised by:** the field-level access design in ADR-070 section 12, from a survey of Dynamics 365, SAP, Workday, Oracle Fusion and NetSuite. The ERPNext-style numeric `permlevel` cannot express closed-by-default groups, per-field grants, masking, `create` versus `update`, a report right or explicit deny.

### What changes

1. **`permlevel` is removed as a security mechanism.** It leaves the presentation hints (§1) and the `FieldHints` struct. Nothing may grant or deny access by `permlevel`.
2. **New field properties (Security layer, §1):**
   - `protection_group: Option<GroupName>`: the field belongs to a named protection group declared on its DocType. A field with a group is **closed by default**. (`inherited_groups` is computed, never authored; see item 5.)
   - `classification: Vec<Tag>`: tags such as `pii`, `financial`, `secret`, `health`. They grant nothing; they set default protection, drive review and export rules, and the lint below.
   - `masking: Option<MaskFormat>`: how a masked value is shown (`Placeholder`, `LastChars(n)`). Required on any field whose group lets a role hold `read_masked`.
3. **New DocType property:** `protection_groups: Vec<ProtectionGroupDef>` with `{ name, label, trace: bool, sync_class_hint }`. `trace` writes a field access trace on every `read` and `reveal` (ADR-070 section 12, rule 9). `sync_class_hint` is only a default (`everywhere`, `encrypted`, `sealed`, `home_only`); the tenant decides (ADR-070 rule 12.13).
4. **Layer rule (§1):** the Security properties are overridable by a site **only upwards**. A site may put a field into a group or add a tag. It may not remove an app-declared group, remove a tag, or weaken a mask. Standard protection is part of the standard.
5. **Derived values inherit protection (ADR-070 section 12, rule 5).** An expression-bearing property or computed value (`fetch_from`, `depends_on` outputs that copy a value, computed or rollup fields, report columns) is **secured by the groups of its inputs**. Two mechanisms, deliberately both:
   - **Computed inheritance (the safety net).** `spot-meta` computes the transitive closure of a field's inputs and records `inherited_groups` on the effective field. Access to the derived field needs a grant in its own group **and in every inherited group**; the least access wins. When a site later puts an input into a group (or tightens it), every derived field picks it up with no further change.
   - **The author's statement (the lint).** A definition whose field is derived from a protected input must say so: set `protection_group` to one of the input groups, or set `declassified` with a reason (reviewed). Otherwise the definition fails with `meta.derived_unprotected`. This keeps the definition honest and the diff reviewable; the computed inheritance still applies. `declassified` removes the inherited groups of that field only, and is itself recorded for the access review.
6. **Tag lint:** a field tagged `secret` or `pii` with no group and no declassification fails the lint (`meta.tag_unprotected`).
7. **`permlevel` migration.** The contract keeps the `permlevel: u8` field as **deprecated and ignored** at 0.3.0 so existing definitions still load. A loader maps an imported `permlevel n >= 1` to the group `level_n` (created on the DocType if missing) and logs a deprecation message; `permlevel` is removed at the next breaking version. Nothing in the engine reads it.

### Consequences

- **Contract:** app API 0.3.0, additive (new field properties and `ProtectionGroupDef`; `permlevel` deprecated, not removed). Task **WP-CON-02.T3** (lead lane), then a **WP-MET-01** task for compile, inheritance and lints (core lane). WP-PLT-03.T1 does not wait for either: it defines the same input types locally (`FieldFacts`) and the adapter from `EffectiveMeta` is a later task.
- **ADR-070:** section 12 is now backed by the metadata model; protection-group sync classes (rule 12.13) have somewhere to be declared.
- **Open questions §7.2, §4, §6 are unaffected.** Question 4 (`field` as a built-in DocType) gains these properties: custom protection added by a site is an editable override, app-declared protection is read-only.

## Amendment A2 (15 Oct 2026): field-presentation inputs for the control catalogue

- **Status:** Decided by the architecture owner, 15 Oct 2026, question by question; carried by `contracts/native-api` 0.5.0 (WP-CON-03.T6; ADR-055 amendment).
- **Raised by:** the blocked audit (WP-UI-08.T1): six inputs a field control needs were in neither the contract nor the client's field model.

### Decisions

1. **Masked values.** The server forms the masked text and **never sends the real value of a masked field** until `field_reveal` succeeds. The mask format (`Placeholder`, `LastChars(n)`, A1) therefore stays on the server and is in no client-visible metadata; a client shows the text it is given, never derives or stores the real value, and never writes a masked text back. The right to reveal belongs to **permissions**: `auth_permissions` lists `fields.revealable`, the masked fields this user may reveal. A client offers a reveal action only for those; `field_reveal` remains the authority.
2. **Currency symbol.** The **server sends it.** `Document._currency` maps each `currency`-format field path to `{code, symbol}`, resolved from the field's `currency_field` on that document and formatted for the caller's locale (ADR-073). Resolving it in the client from a cached currency table is a later optimisation and would not change the wire.
3. **Time zone.** No field-level zone setting. ADR-077 already settles it after a review of SAP, Oracle Fusion, Dynamics 365 and NetSuite: an **instant** (`datetime`) is shown in the user's zone (`LocaleContext.timezone`, display only); a **business date** (`date`) is never converted; a **wall-clock value** carries its own IANA zone inside the value. The control therefore needs no `format_params.timezone`. What a `LocalDateTime` or `Time` control must show (value plus zone name) comes with the field kinds of ADR-077 (WP-TIM-01) as a separate additive bump. **This decision depends on ADR-077 being approved**; if its approval changes, this item is reopened.
4. **Attachment limits.** One global default with narrower overrides (for example one department may upload larger files). The **server resolves the effective limit for the caller** and sends it as `attach_limits` per `attach` field in `auth_permissions`: `{max_bytes, accept}`. The client never merges scopes. `file_upload` enforces it, and `files.max_upload_bytes` stays the ceiling no override can pass. The administration of the override (which scope may set what) is the settings layer's (WP-ADM-07), not this contract's.
5. **Create from a lookup.** Always **derived from permission**: a client offers "create" for a `link` field when `rights.create` is true for the `link_to` DocType in the permission summary. No field-level capability exists.
6. **Control hint and multi-select.** `FieldHints.control` is a **closed set** (`switch`, `checkbox`, `dropdown`, `radio`, `segmented`, `autocomplete`, `chips`, `textarea`) that grows additively; it is a hint a client may ignore. A `select` field may be `multiple`: its value is an array of its `select` values. A picker may show **several columns**: a lookup page names `columns` and each row carries `cells`, while `label` stays the text kept once picked.

### Consequences

- **Contract:** native API 0.5.0, additive, no new operation and no new error code (WP-CON-03.T6).
- **Server work that is not yet tasked** and must be before a client can show any of this: filling `fields.revealable`, `attach_limits` and `Document._currency` (the adapters of WP-PLT-05 and the permission summary), validating and storing `multiple` selects and `control` hints in `spot-meta`, and letting a lookup seam return columns. The lead will brief these once the client side is scheduled; until then a 0.5.0 client degrades to plain controls.
- **Client:** WP-UI-07.T3 carries the new inputs in the metadata runtime; WP-UI-08.T4 builds the masked presentation with reveal, the currency symbol, attachment limits, create action, control hints and multi-valued select. It need not read ADR-077.
- **Unchanged:** the Security layer of A1; a site may still only add protection.

## Amendment A3 (19 Oct 2026): rule expressions on the wire and the lookup's provisional fills

- **Status:** Decided by the architecture owner, 15 Oct 2026, question by question; carried by `contracts/native-api` 0.6.0 (WP-CON-03.T7; ADR-055 amendment). Server-side compilation of these properties is WP-MET-02; this amendment is the wire.
- **Raised by:** the audit (WP-UI-07.T2), whose Handoff lists four gaps between the metadata runtime and the contract.

### Decisions

1. **Where the expressions live.** `FieldMeta` gains the optional strings `depends_on`, `mandatory_depends_on`, `read_only_depends_on` and `default_expr`, written in the shared expression language (`docs/expression-language.md`), and `DocTypeMeta` gains `expr_version`, the language version the strings are written for. A client that does not know the version ignores the expressions and falls back to the static properties. Sending no expressions and answering every rule through `form_event` was rejected: a round trip per keystroke defeats ADR-28.
2. **Lookup filters.** The **server** applies the field's filter using the `doc` and `parent` snapshots the lookup request already carries (§3, conditional resolution is the server's). The client sends and evaluates no filter, so the expression language gets no map type and `LookupRequest` stays closed.
3. **Views.** No view schema in this version. The client uses definition order and `hidden` (as WP-UI-07.T2 already permits); a view schema is its own later bump with the form designer.
4. **Fetch-from.** `fetch_from` stays server-side: the server fills it in `form_event`. It is not on the wire.
5. **Provisional fills from the lookup row.** A lookup result row gains an optional `provides` map of the values the form's `fetch_from` fields would copy from that row, keyed by the form's receiving field and sent only when the asker may see them (never masked or reveal-gated values). The client may fill them at once as **provisional**; the answer of `form_event` replaces them and the server recomputes on save, so a stale hint can flicker but never persist. Client-side caching of lookup pages (stale-while-revalidate, keyed by the exact request, dropped on a metadata version change) needs no contract change and belongs to the UI tasks. The server still applies filters and permissions on every request.

### Consequences

- **Contract:** native API 0.6.0, additive, no new operation and no new error code.
- **Server work not yet tasked:** compiling and validating the four expression properties and `expr_version` in `spot-meta`, and filling `provides` in the lookup seam answer (WP-MET-02). Until then a server emits none of them and a 0.6.0 client behaves as a 0.5.0 one.
- **Client:** WP-UI-07.T2 may now read the properties; the four decisions it was waiting for are taken, so its scope no longer needs narrowing for the views and `fetch_from` points beyond what decisions 3 and 4 say.
- **Unchanged:** the expression language, the Security layer of A1, and the rule that a lookup suggests and never validates.

## Amendment A4 (26 Oct 2026): fields set only at creation, and the consumer reference (ADR-083)

- **Status:** Proposed by the lead for the architecture owner's signature; the architecture owner approved writing it on 26 Oct 2026 (ADR-083 open question 10). Carried by app API 0.7.0 and native API 0.9.0.
- **Raised by:** ADR-083 barrier 4 (`User.user_type` must never change after creation) and section 2.2 item 4 (a consumer-caused record needs an owner scope that is not a link to a `User`).

### Decisions

1. **`create_only` is a field property, enforced by the document engine for every channel.** The stored value is set when the document is created and never changes: an update, a bulk update, an import, a sync apply, an SCIM write and a method writing through the document service all go through the engine, so one check covers them. A write that carries the value the field already has is not a change (a client may send the whole document back); a different value or a clear is refused with `document.read_only_field` on that field and nothing is written. No new error code was added, because the existing one already says "this field cannot be changed" and a client already handles it. Anonymisation leaves the field as it is (the anonymiser does not write it). It is meaningless on a child table and contradictory with `allow_on_posted` (both refused at definition time). Field-level permission does not replace it: a right can be granted by mistake, a property of the field cannot be granted away.
2. **Proposed: it is metadata, so a site cannot lift it.** Like protection declared by an app (A1), a site may add `create_only` to a field and may never remove it from one an app declared. (Whether a site can add it at all is the field-editing question 4 of section 7 and is not decided here.)
3. **`ConsumerRef` is a field kind, not a link.** The consumer lives in the consumer plane's own store (ADR-083 2.2), not in a DocType, so there is no target DocType, no referential check, no lookup and no join; the value is the consumer's id. It is the owner scope of a record a consumer caused: the consumer surface reaches the records whose `ConsumerRef` is the consumer's. A DocType has at most one, and it is `create_only`, because changing it would move a record to another consumer. On the wire it is the `consumer_ref` field type, encoded like a link's value; a staff client shows it as read-only text.
4. **Not decided here (see the PR):** the surface definition that lists what a consumer may read and invoke waits for the view-tree ADR; the order of checks (owner scope before field rights) and the storage mapping of `ConsumerRef` (a 16-byte id column with an index) are for the tasks below.

### Consequences

- **Contract:** app API 0.7.0 and native API 0.9.0, additive, no new operation and no new error code.
- **Server work not yet tasked:** the TOML loader and `spot-meta` read `create_only` and compile `consumer_ref`; the document engine enforces `create_only` (the stub is the reference); `User.user_type` gets `create_only = true` in `apps/platform/doctypes/user.toml` (ADR-083 6.1 task 5, core lane, after the architecture owner's signature); the native API serves `create_only` and `consumer_ref` in the metadata; the conformance suite of ADR-083 Appendix A (B4) proves it per channel. Until then a 0.9.0 server emits neither and a client behaves as with 0.8.0.
- **Unchanged:** the Security layer of A1, the rule expressions of A3, and `read_only` (the engine's per-state decision), which `create_only` does not replace.
