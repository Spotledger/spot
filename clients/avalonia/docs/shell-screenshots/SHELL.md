# Shell chrome — what it is and how the PNGs prove it

The signed-in application chrome of the Avalonia client (WP-UI-02.T2). Every PNG in this
directory is rendered by `ScreenshotParityTests` (tests/Spotledger.Client.Shell.Tests)
against the **real** `ShellView` + `ShellViewModel` at 1920×1080, headless Skia, states
driven through the same command model and native input pipeline the desktop host runs.
The architecture owner reviews these PNGs and this file before the code.

## The chrome

- **Command model** (`Commands/`): `CommandDefinition` (Id, Label, Description, Category,
  MenuPath, Mnemonic, RequiredContext) in a `CommandRegistry`, dispatched by a
  `CommandDispatcher`. Menu items, launcher rows, keyboard shortcuts, the New button and
  tab/context menus are all triggers of the *same* `ShellCommand` objects; availability is
  `Available | Blocked | NeedsContext | Restricted` with a reason, computed from local
  shell state (the server seam, ADR-055, comes later). A running command ignores a second
  trigger. Shortcuts live in the `desktop`/`browser` JSON keymaps (`Keymaps/`); tests fail
  on a mnemonic collision at one menu level or two commands sharing a chord.
- **Menu bar** (`MenuBarBuilder` + Avalonia's native `Menu`/`MenuItem`, token-styled):
  File, Edit, View, Transactions (Purchasing, Stock, Sales submenus per the mockup),
  Processes, Reports, Tools, Window, Help. Items the product does not have yet are absent;
  real-but-unavailable commands are greyed with their reason as tooltip. Click opens,
  second click/Escape closes, hover switches, arrows/Home/End/Enter navigate, Left/Right
  enter and leave submenus, Alt+letter opens, F10 focuses, mnemonics underline while Alt
  is held. No popup overlay or modal — the dropdown lives in the window's own layer.
- **One window frame**: logo, menu bar, `Search (Ctrl+K)` box, primary **New** button,
  bell with unread count, user chip (name, site, initials from the session endpoint),
  tab strip, content, status bar (message, keymap profile, density). Hairline separators
  and the `Space.*` token scale only; the two sizes outside the token scale are the
  launcher width (560) and its drop offset (see `ShellView.axaml` resources).
- **Tabs**: close ×, pin icon (Shell path geometry), `+`, keyboard-reachable ⋯ overflow;
  the active tab joins the content area. Right-click/⋯ menu: Close, Close Others, Close
  All, Pin/Unpin, Duplicate, Move to New Window (shown greyed with its reason).
  Ctrl+Tab / Ctrl+W (desktop profile). Dirty close asks Save/Discard/Cancel; batch close
  gathers every decision first and one Cancel keeps all tabs. Tab identity is the
  document UUID or `workspace:<name>`, so opening an open record focuses its tab.
- **Sidebar**: eight workspaces (Home, My Tasks, Selling, Buying, Stock, Accounts,
  Reports, Settings) with hand-parsed 24×24 path icons (`Icons/ShellIcons.cs` — no icon
  package, per the brief). Expanded → icons-only → hidden with Ctrl+B; a hidden sidebar
  keeps a visible restore toggle in the tab strip. An unbuilt workspace shows one line
  saying what it will hold. Every row dispatches through the command model — no inert
  primary action.
- **Command launcher (Ctrl+K)**: floating panel under the header (shell-layer panel, not
  a menu popup or modal): search field, grouped results (Create, Open, Reports, Recent),
  icon + title + caption + right-aligned category chip, highlighted selected row with an
  `Enter` badge, Enter runs, Escape closes, focus returns to the origin. The default
  highlight is the first *available* row and it is scrolled into the clipped viewport;
  blocked rows show their reason as the caption. `shell.new` (the New button) opens it.
- **Polish**: hover/pressed/focus/disabled states from tokens; light and dark
  (`RequestedThemeVariant`; the code-built chrome rebuilds its token brushes on variant
  change); Compact/Comfortable density (`Density` attached property); RTL mirrors menu
  bar, tabs, sidebar and status bar via the window's `FlowDirection` (session switch).

## PNG → mockup panel

Mockup panels: `docs/ui-research/images/multi-layer-ui.png` and `home-screen-concept.png`.

| PNG | Mockup panel / brief item | What it shows |
| --- | --- | --- |
| `01-home-tabs.png` | header + tab strip in every panel; "Sidebar Collapse and Tab Management" | one frame (logo, menu bar, search, New, bell+3, user chip), four tabs: active joined, dirty dot (PO-1042), pinned (Selling), plain; `+` and ⋯ at the right |
| `02-menu-transactions.png` | "Alt Menu Navigation" | Transactions dropdown open directly under the item with Purchasing submenu to the side; blocked document items greyed with reasons |
| `03-launcher.png` | "Command Palette" | Ctrl+K palette under the header, grouped rows, chip + `Enter` badge on the highlighted available row |
| `04-sidebar-expanded.png` | sidebar panels | icons + labels, selected workspace, toggle at top |
| `05-sidebar-collapsed.png` | "Sidebar Collapse and Tab Management" | icons-only rail, labels hidden |
| `06-tab-menu.png` | tab management | Selling tab's menu: Close / Close Others / Close All / Unpin / Duplicate / Move to New Window (greyed with reason) |
| `07-rtl.png` | RTL (Urdu) requirement | whole chrome mirrored right-to-left from the session `FlowDirection` switch |
| `08-dark.png` | light + dark polish | full chrome in `RequestedThemeVariant=Dark` from the token theme dictionaries |
| `09-sidebar-hidden.png` | "Sidebar Collapse and Tab Management" (hidden state) | sidebar removed, content full-width, restore toggle visible in the tab strip |
| `10-compact-density.png` | density polish | Compact density across header, tabs, sidebar, status bar |

## Honest remaining gaps (one line each)

- Launcher rows show category chips but only the selected row carries the `Enter` badge;
  per-command chord badges (e.g. Ctrl+K on "Command launcher") are not rendered per row.
- The bell button has no dropdown yet — the notification feed arrives with a later task;
  the count is shell state, not server data.
- The user chip opens nothing yet; account/site switching is a later task.
- Drag-to-reorder tabs is not implemented (Close/Pin/Duplicate/Close-Others are).
- Window and Help menu content is thin by design: items the product does not have are
  absent per the brief, so some mockup entries (theme/density) live in the View and Tools
  menus instead.
- RTL mirrors layout only; the mockup's Urdu string samples are not rendered (no locale
  runtime yet — later task).

## Regenerating

```
dotnet test clients/avalonia/tests/Spotledger.Client.Shell.Tests -c Release \
  --filter FullyQualifiedName~ScreenshotParityTests
```

Writes every PNG above into this directory from the real views (1920×1080, headless Skia,
states driven via the viewmodel/native input pipeline — deterministic, no timing).
Commit the changed PNGs with the code they prove.
