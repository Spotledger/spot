# ADR-069: UI composition: how apps register navigation, how definitions are customised, and the compiled UI bundle

- **Status:** Proposed by the lead, 3 Oct 2026, after a design discussion with the architecture owner. The architecture owner approved in that discussion: layers (site and role, no per-user views), the split between presentation and behaviour customisation, the hidden-mandatory rule, and anchored operations with an upgrade report. Everything else below is a recommendation that awaits the architecture owner (see Open questions).
- **Date:** 3 Oct 2026
- **Layer:** Platform (WS9 UI, with WS2 metadata and WS3 manifest)
- **Builds on:** ADR-05, ADR-08, ADR-09, ADR-16, ADR-17, ADR-22, ADR-27, ADR-28, ADR-055, ADR-066, ADR-068 (approved by the architecture owner)
- **Work packages:** WP-UI-10, WP-UI-13, WP-UI-02, WP-UI-09, WP-EXT-04 (follow-up), WP-ALM-01, WP-MIG-03, plus new packages listed under Consequences
- **Code today:** `crates/framework/spot-manifest/` (no UI section), `crates/framework/spot-meta/`, `clients/avalonia/`

## Context

Frappe builds its desk from one per-request blob (bootinfo): modules, workspaces, sidebar, permissions, settings and counts, computed on every load, with app-shipped and user-edited records in the same rows. We reject that (ADR-09, ADR-05), but the blueprint does not yet say what replaces it:

1. **No registration path.** `spot-manifest` has HTTP `routes` and `assets` but nothing that puts an app in a sidebar, a menu bar, a launcher or the command palette. WP-UI-13 (tier A) names "views, workspaces, actions" without saying how they are declared, merged or ordered across apps.
2. **No compiled, cacheable UI.** WP-UI-09 caches the Avalonia binary only. The client has no defined way to learn what to show, to keep it locally, and to refetch only what changed.
3. **Customisation is under-specified at the edges.** ADR-068 §7 fixes layered views for forms, but an upgrade that removes a field, a destination or a view still has nowhere to report conflicts, and navigation has no override story at all.

The UI must not be rebuilt on the fly. ADR-068 §7.7 says a view is merged at resolution time with no rebuild; this ADR makes that precise: the merge is a deterministic **compile** to content-addressed chunks that clients cache, not a per-request computation.

## Decision

### 1. Definitions are data; the frame is code

| In code (versioned UI contract) | In data (documents, persisted and permissioned like any other) |
| --- | --- |
| The frame: menu bar, sidebar, status bar and home controls, and the **closed list of slots** they offer | What fills the slots: workspaces, destinations, commands, actions, home blocks |
| The control catalog (ADR-068 §5), tokens, tier B components | Views and their layers (ADR-068 §7) |
| A small **core navigation** compiled into the shell | Everything else, including Settings and Customize |

1. **The source of truth is documents**, so permissions, audit, history, REST, sync, backup and customisation bundles (WP-ALM-01) apply with no new machinery. System DocTypes (names provisional): `workspace`, `destination`, `command`, `view`, `slot_item`, `ui_override`. A single `ui_prefs` document per user holds personal state (column widths, pins, favourites, last tab); it is never part of any view or bundle (ADR-068 §7.2).
2. **The shell stays generic.** It knows no app. Slots are a closed, versioned set (for example `shell.sidebar`, `shell.menubar`, `shell.statusbar`, `shell.user-menu`, `home.tiles`, `doc.toolbar`, `doc.sidepanel`); an app fills slots with declarative content and cannot add chrome or inject script (ADR-10, ADR-28).
3. **Core navigation is a rescue path.** It is built into the shell, always present, and reaches the editors of these DocTypes, so a broken definition can never lock out an administrator. It is the only navigation not delivered as data.

### 2. Destinations and projections

1. A **destination** is anything you can go to: a DocType list or form, a report, a workspace, a command, a route. Every destination has a stable id, `app.name`.
2. The menu bar, sidebar, launcher, command palette, home tiles and breadcrumbs are **projections of one destination registry**, not separate lists. An app declares a destination once and says where it is placed; this is the command-registry rule of the UI corrections (section 4) extended to navigation.
3. **A workspace (ADR-068 §7.2) is the top-level area:** per module, shaping the menu and the layout of its home, never a security boundary. This ADR adds no second concept for it. An app may declare several modules, and an app may add destinations to another app's workspace only if it declares a dependency on that app (ADR-17).
4. **Desktop and browser render the same tree.** The desktop menu bar and the browser sidebar are two renderings; keymaps are data per target (UI corrections, section 4). The mnemonic-collision test is kept and runs on the resolved tree.

### 3. Registration through the manifest

1. The manifest gains a `[ui]` section that **lists seed files** (workspaces, destinations, commands, views, slot items). It adds no behaviour: the installer loads the files as documents (a fixtures category, ADR-17) with `origin = app:<name>`.
2. Resolution is deterministic: dependency order, then an explicit `order` hint, then app name. Ids are namespaced by app.
3. **Install and upgrade are rejected** on: a destination that points at nothing, a duplicate id, a duplicate mnemonic at one menu level, an extension of an app that is not a declared dependency, a cycle, or a permission reference that does not exist. Errors name the app, file and id, like every other manifest error.

### 4. Ownership and customisation

1. **App-owned rows are protected.** Only the app installer and upgrader write them, under declared intent (ADR-22); they are not editable through the UI or REST. This is what keeps upgrade and uninstall clean.
2. **A customisation is a list of typed operations, not a copy.** Approved by the architecture owner. The operations are **anchored**: hide, move (after or before a named anchor, into a named group or tab), relabel, reorder, regroup, and for grids choose columns. A saved copy would freeze the view and hide every later app change; operations survive upgrades (ADR-068 §7.2).
3. **Layers.** App standard, then site, then role. The role view is chosen by role priority as in ADR-068 §7.2 (provisional, to be revisited before WP-UI-10). **There is no per-user layer.** Navigation uses the same operation engine against destinations, so nothing extra is built for it.
4. **Two kinds of customisation, one screen.** An administrator uses one "Customize Form". Presentation (hide, move, relabel, columns) is written as view operations. Behaviour and schema (a custom field, required, read-only, default, depends-on) are written as property overrides under ADR-068 §1. A hidden field is still stored, still in the API and still permission-checked (ADR-068 §7.4). A hidden `required` field with no default is rejected when the override is saved (ADR-068 §7.5), and a hide or move of a field that a view rule depends on is flagged.
5. **Upgrade report.** When an upgrade removes or renames something an operation anchors to, the operation falls back to a safe position (the app's own position, never silent loss), and the case appears in the migration plan (WP-MIG-03) with the app, view, operation and reason. Reset to default means deleting the operations.
6. **Validate at save, publish per definition.** A save validates referential integrity, cycles, mnemonics, anchors and permission references; a bad row cannot be saved. A view or a navigation override has draft and published states, versions and rollback, and an administrator can preview as a role before publishing. A multi-document change set (several destinations at once) is a later refinement and is not required for form customisation.

### 5. The compiled UI bundle

1. **The compiler.** A framework crate (no Avalonia dependency) takes manifests, definitions, overrides and DocType metadata, including the generated default views (ADR-068 §4), and produces a **UI bundle**. It is deterministic: the same input always gives byte-identical output.
2. **Chunks, content-addressed.** The bundle is an index plus chunks, each with a content hash:
   - one **navigation chunk** per permission-set hash (the union of what a user's roles allow, ADR-068 §7.2), so many users share one;
   - one **view chunk** per DocType per effective layer set, shared by everyone who resolves to the same view;
   - workspace-home and command chunks.
3. **Triggers.** A recompile is caused by an outbox event (ADR-11): app install, upgrade or removal, a published view or override, or a DocType metadata version change (the same invalidation bus as WP-MET-03). It recompiles only the affected chunks. If a chunk's hash does not change, nothing changes for clients.
4. **Wire format.** v0.1 is canonical JSON, compressed, hashable and debuggable. The contract allows a binary encoding later without changing meaning.
5. **Per site.** The installed apps differ per site, so a bundle belongs to a site. Chunks are de-duplicated in the object store by hash across sites. Compilation is deterministic, so sync can ship the bundle by hash and edges need not recompile (ADR-062 to ADR-066, version skew).
6. **Versioning.** The bundle declares its UI contract version and the minimum shell it needs, like `requires_host`. A shell that is too old keeps its cached bundle and shows an upgrade notice rather than failing to render.

### 6. Session, cache and revalidation

1. **The boot payload is small, under 5 KB:** user, capability set id, locale, company and establishment, feature flags, and the current bundle id. Nothing in it is a definition.
2. **Revalidate by hash.** The client sends the bundle id it holds. A match transfers nothing. Otherwise it fetches the chunks whose hashes differ. Chunks are immutable, so an HTTP or local cache hit is always correct.
3. **Live changes arrive by realtime** (`ui.bundle.changed`), not by polling. They apply at a safe point (idle, with a visible notice), never in the middle of an edit.
4. **Browser and desktop.** The browser keeps chunks in IndexedDB beside the cached shell (WP-UI-09); the desktop keeps them on disk. Both can start offline from the cached bundle, which is what offline POS needs.
5. **Live data is separate.** Badges, counts and notifications are pushed or fetched on demand and are never part of boot or the bundle.

### 7. Permissions and leakage

- The server filters navigation per permission set, so a client never receives entries it may not see. View chunks are fetched lazily on first open and the server checks permission at that moment.
- A bundle is a presentation artifact. It is never an authorisation: every operation is checked again by the server (ADR-10, ADR-068 §7.4).

## Consequences

- **Easy:** an app registers navigation by declaring files, with no shell change. An administrator customises a form without breaking upgrades. A change to one view downloads one small chunk. Offline start comes from the cache.
- **Hard:** several system DocTypes and an operations engine to design; the editors depend on the generated default view and the core navigation working first; the compiler's determinism must be tested as a property; the realtime and safe-point apply path needs UI tests on both targets.
- **Divergences from ERPNext (ADR-04):** no per-request boot blob; app rows and customisations are separate; navigation is a projection of a destination registry; no desk script injection.
- **Order of work** (the lead decomposes into tasks): (1) the view-override operation set and validation, compiling view chunks (WP-UI-10 follow-up); (2) the bundle compiler, the serve and revalidate endpoint, and the contract (WP-CON follow-up); (3) the manifest `[ui]` section and destination registry (WP-EXT-04 follow-up); (4) the client cache and apply (WP-UI-02, WP-UI-09); (5) navigation overrides and the upgrade report in WP-MIG-03.
- **Risks:** the slot list and control catalog will grow; the compiler becomes a critical path and needs a conformance corpus; an ill-chosen anchor model could still surprise administrators, so the upgrade report is mandatory, not optional.

## Open questions

Approved by the architecture owner on 3 Oct 2026: layers (site and role, no per-user view), the presentation and behaviour split, the hidden-mandatory rule, and anchored operations with an upgrade report.

Recommendations for the architecture owner to confirm:

1. **Documents as the source of truth,** with the manifest `[ui]` section only as seed (section 1, 3).
2. **App-owned rows protected,** customised only through operations (section 4.1).
3. **Draft and publish per definition,** with multi-document change sets later (section 4.6).
4. **Core navigation compiled into the shell** as the rescue path (section 1.3).
5. **Slots defined in code and versioned,** with apps only filling them (section 1.2).
6. **Wire format:** canonical JSON v0.1 (section 5.4).
7. **Navigation filtered server-side** per permission set (section 7).
8. **Apply at a safe point,** not only on restart (section 6.3).
9. **One bundle per site,** with chunks de-duplicated by hash (section 5.5).
10. **Naming:** keep ADR-068's "workspace" for the top-level area, and add no second term.

## Amendment A1 (WP-UI-13.T1): the tier A extension contract

Written by the lead as a draft for the architecture owner. `contracts/ui-ext/` (version 0.1.0) is
the contract directory: JSON Schemas, a shared corpus of valid and invalid examples, `errors.md`
(`uiext.*` codes) and `check.py`, the reference validator.

1. **The slot list is normative** and closed: `shell.sidebar`, `shell.menubar`, `shell.statusbar`,
   `shell.user-menu`, `home.tiles`, `doc.toolbar`, `doc.sidepanel`, `doc.tab`, `list.toolbar`,
   `list.sidepanel`. Each accepts one kind of contribution (action, panel, tile or tab) with a limit per
   app and DocType and a label length (`contracts/ui-ext/schema/slot.json`). Order: dependency order,
   `order`, app name, id.
2. **Tier A has no client code.** An app declares actions, panels, tiles, tabs and composite
   components as data. Behaviour is a server method its manifest declares, or an expression of the
   shared language. No script, no remote content, no markup, no unbounded recursion (depth 8, 500
   nodes), no widening of permissions.
3. **Reuse, not a second vocabulary.** Composites use the ADR-068 control catalog (read-only
   presentations) and layout primitives; expressions are the shared language, with the props as `doc`.
4. **Validation at install** is the `uiext.*` rule set; the installer refuses a bundle with any finding.
5. **Closed by this amendment:** open question 5 (slots defined in code and versioned) is made concrete.
   **Left open** (README "Open questions" of the contract): expression roots, the icon set in the design
   system, the manifest's declaration of queries, destinations and routes, and translation keys.

