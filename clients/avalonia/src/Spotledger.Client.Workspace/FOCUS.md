# FOCUS.md: the keyboard rules of the list-and-form workspace

This is the contract WP-UI-17.T2 makes with a person who does not use a mouse. The code is the
source of truth (`Keyboard/FocusRules.cs`, `Keyboard/PaneRing.cs`, `Keyboard/WorkspaceKeymaps.cs`,
`Split/ListFormSplit.cs`); this file says the same thing in words and records the decisions and the
gaps. Tests: `tests/Spotledger.Client.Workspace.Tests/{Split,Keyboard,Workflows}`.

## The arrangement

* The list is on the left in a left-to-right layout and on the right in a right-to-left one (the panes
  mirror with the window's `FlowDirection`), the form of the selected row is on the other side, and a
  splitter between them drags with the pointer and resizes from the keyboard.
* Three modes: **list only**, **list and form**, **form only**. The mode and the splitter's share are
  remembered per DocType **per user** (an in-memory store for tests and a JSON store for a host).
* A selection change never closes a dirty form silently. The change waits, the guard says what is at
  stake, and one answer decides: **Save** (send, then show the row that was waiting), **Discard**
  (throw the changes away, show the row), **Stay here** (`Esc`, the selection goes back to the document
  on screen and the changes stay).
* One form view model per document id, always. Walking back to a row reuses its form; a document moved
  to its own tab (`Ctrl+Enter`) is handed to the tab and is not built a second time here — selecting it
  again says so instead of showing two forms for one document.

## Chords

Registered at runtime through the Shell's `KeymapProfile` / `ShellViewModel.ExtendKeymaps` seam. The
shipped `Keymaps/*.json` files are **not** edited; `WorkspaceKeymaps.Extend` appends to whichever
profile is loaded and refuses to shadow an existing chord.

| Chord | Command | Why this chord |
| --- | --- | --- |
| `Ctrl+Alt+1` / `2` / `3` | list only / list and form / form only | a view switch that cannot fire while typing; `Alt` alone opens menus |
| `Alt+Down` / `Alt+Up` | next / previous document | the Windows convention for next/previous record; no text box consumes it |
| `F6` / `Shift+F6` | next / previous pane | Windows and GNOME pane cycling |
| `Ctrl+Alt+L` / `Ctrl+Alt+F` | focus the list / focus the form | direct jumps for a person who knows where they are going |
| `Ctrl+Enter` | open the form in its own tab | the shell's document-open gesture |
| `Ctrl+Alt+N` | new document, in the form pane | `Ctrl+N` is a browser-reserved chord |
| `Ctrl+Alt+S` | save and next | `Ctrl+S` belongs to the form's own save |
| `Ctrl+PageDown` / `Ctrl+PageUp` | next / previous tab | the new commands are the chrome's own `tab.next` / `tab.previous` |
| `Enter` / `F2` (in the list) | focus the form | the form pane replaces "open a tab" while it is visible |
| `Esc` (in the form) | back to the list row | the GNOME/Windows "leave the pane" key |
| `Ctrl+Z` / `Ctrl+Y` (outside a text editor) | the form's undo / redo | inside an editor the field keeps both keys |

No chord above is in `WorkspaceKeymaps.TextEditingChords`, and none is a browser-reserved chord
(`Ctrl+W`, `Ctrl+T`, `Ctrl+N`); both are asserted by `Keyboard/KeymapTests`.

## Focus rules

The table is `FocusRules.Resolve(rule, context)`. Two invariants hold for every rule: **focus never
lands on a disabled or hidden control** (the ring skips such a pane, and `FocusTrace` records a
violation if a control ever takes the focus in that state), and **a change nobody asked for never
takes the focus away from where a person put it**.

| Moment | Focus goes to | Announced |
| --- | --- | --- |
| A row is opened | the form's first field | "Document <id>" / "New document" |
| A save succeeds | stays where it was (the form) | "Saved" (when the arrangement performed the save) |
| A document is deleted | the list | "Deleted" |
| A document is cancelled | the list | "Cancelled" |
| An operation fails | the first invalid field, or the form | the message |
| A dialog closes | the control that opened it | — |
| A pane is switched (`F6`) | the pane's entry control (the form's first field) | — |
| A tab is switched | the new tab's list | — |
| A refresh finishes | stays where it was | the count |
| A validation failure on save | the first invalid field, **scrolled into view** | the field's own message |

Announcements go through one assertive live region (`Keyboard/Announcer.cs`, automation id
`split.live`), so a screen reader hears them and the workflow harness can assert them.

## The keyboard-only harness

`Workflows/KeyScript.cs` is a script of steps: `Press`, `Type`, `ExpectFocus`, `ExpectText`,
`ExpectAnnouncement`, `Expect`, `Await`. There is deliberately **no pointer step**: a workflow that
needs a click cannot be written. `Workflows/WorkflowRunner.cs` sends each key through the headless
window's real input pipeline, counts keystrokes (a typed character is one), measures wall-clock time
against the script's budgets, and counts pointer events — a workflow that produces one fails.

Waits are on state, never on time: `Await` pumps the dispatcher and reads the condition
(`SplitRig.PumpUntil`). No test in this project sleeps, and none asserts on a timer.

Shipped workflows:

1. `edit-two-fields-save`: focus the list, open a row, edit the title, tick the done box, save; the
   patch that reaches the server carries both.
2. `walk-and-close-15`: open a worklist of 15 tasks and close every one with `Tab`, `Tab`, `Space`,
   `Ctrl+Alt+S` (save and next), ending on the last row.

Both declare a keystroke budget and a time budget. The keystroke budget is the interesting one: 15
tasks cost 62 keystrokes in total, and a regression that adds a wrong turn (a dialog, a confirmation,
an extra field stop) breaks the budget instead of quietly costing a person time.

## Candidate workflows for the next round

Twenty more the owner may want to prioritise, roughly by how much a keyboard-only user gains:

1. Create a document with a 20-row child table: add rows with `Ctrl+I`, type a row, `Ctrl+V` paste a
   block, `Ctrl+Delete` a row, save.
2. Submit a document, then cancel it with a reason (`Ctrl+Enter`, dialog by key, `Esc` to keep it).
3. Recover a draft after a crash: open a DocType with a stored draft, restore it, save.
4. Open a document by id from the launcher (`Ctrl+K`, type the id, `Enter`).
5. Filter with the builder entirely by key (`Alt+F` → field, operator, value).
6. Sort a column and keep the selection (`Alt+Up` after the sort).
7. Delete several rows and confirm with `Enter` on the guard.
8. Rename a document (`F2` in the form when the list is not in the way).
9. Change a link field's target with the lookup by key.
10. Run a report and walk its grid (`WP-UI-04.T1`).
11. Copy a document into a new one and edit the copy.
12. Move a document between states through the workflow actions.
13. Attach a file with the keyboard only (a file dialog is a pointer surface: needs a decision).
14. Reorder columns and keep the focus on the same field.
15. Duplicate a document into its own tab and compare the two by `Ctrl+Tab`.
16. Close every tab with unsaved work and answer the guard once for all.
17. Pin a tab and confirm it survives "close others".
18. Undo a header edit — **blocked** by the gap below.
19. Print or export the current list by key.
20. Switch the language to Urdu (RTL) and walk the same worklist mirrored.

## Gaps this task found and could not close (they belong to other projects)

* **A header edit made in the form is invisible to the undo stack.** A field control commits to its
  `FieldState` on `LostFocus` (`FieldControlFactory`), and `DocumentDraft.FieldChanged` is never
  raised, so the T1 `UndoStack` records no step and `Ctrl+Z` has nothing to undo. `Ctrl+Z` is routed
  from the keyboard correctly (`tests/.../Split/UndoRoutingTests`), and undo works for changes that do
  notify (a `draft.Set`, a child-table edit). The fix is in Forms/Controls: commit through the draft
  (or raise the notification after committing).
* **`IsDirty` is not notified for a field commit either**, so the arrangement cannot announce a save a
  person performed with the form's own `Ctrl+S`; it announces the saves it performs itself
  (save-and-next). Same root cause.
* **A text field with a clear button is two tab stops** (the editor, then the button), which the
  workflows have to know. That is a Controls decision (`ShowClearButton`); the arrangement does not
  work around it.
