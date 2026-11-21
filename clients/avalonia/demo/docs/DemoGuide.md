# Spotledger interactive demo · part 1

From the repository root, using the pinned .NET SDK:

```sh
dotnet run --project clients/avalonia/demo/src/Spotledger.Demo
```

The desktop window starts maximized. All data is synthetic, the backend is in memory, and no server is contacted. Company and establishment show the legal context. The status bar explicitly says Demo and labels connection/sync as simulated.

## Supported scenarios

1. Open Home. The procurement officer sees tasks, shortcuts, KPI source/as-of, process counts and recent activity. Every link opens a labelled placeholder or a seeded document.
2. Press Alt, T, P, R. Transactions → Procurement → Receive Goods shows the backend's restriction and a resolution command. Resolve switches to the warehouse persona; invoke Receive Goods again to open its part-2 placeholder. Menus also work by mouse, arrows, Enter and Escape.
3. Press Ctrl+K. Search `buy materials` to find Purchase Request by alias. Search labels, descriptions or document numbers. Each row shows its group, keyboard chord, availability and reason. Arrows select, Enter invokes; Escape restores the original focus.
4. Use Edit → Edit placeholder note. Type a note and observe the dirty `*`. Ctrl+S saves against the document version. Opening that record again focuses the existing UUID tab. Ctrl+Tab / Ctrl+Shift+Tab cycle tabs.
5. Close a dirty tab, switch role or Reset. Save, Discard and Cancel are explicit choices. Cancel preserves the draft. The OS window close and File → Exit use the same prompt.
6. Right-click a tab (or Shift+F10 when it is focused) for Close, Close Others, Close to the Right and Pin. Bulk close preserves pinned tabs. Explicit Close can close a pin. Window → All tabs / Ctrl+Shift+L exposes overflow with keyboard focus.
7. Ctrl+B cycles the sidebar through full labels, icons and hidden. View → Toggle right rail changes the rail separately. Switch persona and back to see per-user session layout retention.
8. Demo → Role switches among seven personas. Tasks and shortcuts change. Manager has an explicit empty task state linked to Reports.
9. Demo → Toggle governance changes full Home to simple task tiles. Demo also switches maintenance, Guided/Standard/Strict enforcement, Compact/Comfortable density and English/Urdu. Urdu sets shell RTL and uses the Urdu font token and localized Home/shell labels. Synthetic fixture names and diagnostic messages can remain English.
10. Demo → Keymap → Browser uses no Ctrl+W/T/N or bare Alt. Menus remain reachable by their buttons. Help → Keyboard map shows both JSON profiles. See [KeyboardMap.md](KeyboardMap.md).
11. Demo → Reset restores seed notes, versions, procurement persona, default layout, keymap, language, density and governance. It closes all document/workspace tabs and reopens Home after resolving dirty notes.

## Boundaries

Procure-to-Pay arrives in part 2; Sales Invoice, Party and lists in part 3; POS in part 4; reports in part 5. Placeholder tabs preserve an illustrative filter and scroll state. Only seeded document notes can be edited/saved. Post always explains its backend-provided boundary. KPIs are fixture amounts and do not represent real accounting. Administration is an explicit outside-demo message. Enforcement levels affect presentation only. There is no real login, offline mode, sync, ledger, company switching or persistence across app restarts in this part.

## Local acceptance

```sh
dotnet build clients/avalonia/demo/Spotledger.Demo.sln -c Release -warnaserror
dotnet test clients/avalonia/demo/Spotledger.Demo.sln -c Release --no-build
```

KeyboardMap.md is generated from JSON by the test's generator and compared byte for byte (normalizing platform line endings). Owner review still needs an interactive desktop run, especially for OS keyboard conventions, Urdu font availability, contrast, text scaling and the shell's visual layout. Code review and green tests do not replace that acceptance.
