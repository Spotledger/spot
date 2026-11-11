# UI extension contract changelog

Every change is an ADR amendment plus a semantic version bump.

## 0.1.0 (draft, ADR-069 amendment A1; awaits the architecture owner's approval)

First version (WP-UI-13.T1). Tier A: an app extends the client by declaring data, never by
shipping client code.
- The closed slot list: `shell.sidebar`, `shell.menubar`, `shell.statusbar`, `shell.user-menu`,
  `home.tiles`, `doc.toolbar`, `doc.sidepanel`, `doc.tab`, `list.toolbar`, `list.sidepanel`, each
  with the one kind it accepts, a limit per app and DocType, a label length and the ordering rule.
- Actions (one effect: `call`, `open` or `navigate_url`), panels (seven block types, each data
  block over a declared server query), composite components (catalog controls and layout
  primitives, acyclic, depth 8, 500 nodes), tiles and tabs, and the `ui` document that carries them.
- A closed icon-token set; the restricted markdown subset; `uiext.*` error codes.
- A shared corpus (`examples/valid`, `examples/invalid`, the sample app) and `check.py`.
