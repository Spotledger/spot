# Demo decision log

All mock data, command availability and save operations are demo simulations. Owner acceptance is separate from the code review.

| Status | Decision | Source / reason |
| --- | --- | --- |
| recorded | Mock backend behind IBackend; typed errors with field paths, versions and command ids; decimal KPI values | Corrections §1, ADR-055 |
| recorded | One registry and dispatcher for menus, Alt sequences, launcher, shortcuts, context menus and Home links | Corrections §4 |
| recorded | UUID document identities; workspaces have stable keys | Corrections §5 |
| recorded | Governance configurable; demo starts on; off renders simple task-tile Home | Corrections §3, §8 |
| recorded | Desktop and browser JSON keymaps; browser avoids reserved Ctrl+W/T/N and bare Alt | Corrections §4 |
| recorded | English / Urdu Home and shell, token styling and design-system controls | Corrections §5, §10 |
| recorded | Plain INotifyPropertyChanged and RelayCommand; no MVVM framework | Corrections §10 |
| recorded | No real persistence, server login, process rules, posting, POS, reports or Admin Center in this part | Task scope and corrections §9 |
| proposed | Consume the parent's pinned Avalonia.Desktop, Avalonia.Themes.Fluent, Avalonia.Headless.XUnit, xunit.v3, Microsoft.NET.Test.Sdk and xunit.runner.visualstudio; add no package versions or dependencies | Existing desktop/headless patterns. Fluent supplies standard menu, list, scroll and window primitives; Spot controls supply action/entry controls |
| proposed | Demo-only command ids are explicit local ids, not invented production endpoint names | Home and demo controls have no native API command contract yet; replace with generated-client adapter when defined |
| proposed | C# view renders presentation models and uses the existing Controls and Theme; no runtime XAML or second component library | Scope is a demo shell, with presentation state only |
| proposed | Width constraints derive from Space.12; gaps, borders, radii, colours, fonts and density derive from named theme tokens | Theme has no shell layout-width tokens; no literal pixel sizes in demo views |
| proposed | Sidebar cycles expanded → icons → hidden with Ctrl+B; the rail toggles separately; both persist per synthetic user only in memory | Required session persistence, no production storage |
| proposed | Pin protects bulk-close, while explicit Close can close pinned tabs | Task requires pin but leaves policy open |
| proposed | Bulk dirty-close gathers every decision before changing tab state; Cancel preserves all tabs | Avoid partial tab removal on cancellation. Successful server saves cannot be rolled back if a later save fails |
| proposed | Role switches retain resolved tabs, focus Home and load the selected user's layout | User explicitly resolves Save/Discard/Cancel; no silent discard |
| proposed | Maintenance and enforcement banners are simulated; maintenance blocks the canned Post example; enforcement affects the banner only | No real policy or ledger in this part |
| proposed | One company with two establishments; context initially stays at Head office; establishment switching is not advertised | Establishment selection and company switching are beyond part 1 |
| proposed | Synthetic UUIDs, fixed injectable clock, decimal KPI fixtures; editable placeholder notes use versioned merge patches and Reset reseeds them | No business calculation or process engine in C# |
| recorded | Use the shared Controls ScrollViewer theme directly; the temporary demo-local override has been removed after rebasing onto the library fix | A library fix repaired the read-only Extent/Viewport bindings; a rebase and fresh acceptance runs were requested. The demo retains its minimal ScrollViewer headless regression |
| open question | Owner must run the demo and accept the shell before this task is considered done | Corrections §11 and task Done when; local tests do not establish visual acceptance |
| open question | Confirm native API command ids/endpoints for Home projections and demo-only navigation during future real-client wiring | Part 1 IBackend intentionally carries demo projections; conventions match the native API, but this is not a generated transport |

Simulated boundaries: company/role identity, tasks and due times, KPIs with mock source/as-of, process counts, activity and recent records, availability and resolution commands, maintenance, enforcement, connection/sync indicators, note saving and reset. Every other business destination opens a labelled placeholder naming the later part. No transactional posting is simulated.
