# Accessibility gate

An ERP sold to businesses has to be usable with a keyboard and a screen reader. This is the
headless gate (WP-UI-16.T2): `AccessibilityAudit` walks any shown window and reports defects, and a
committed ratchet baseline turns them into a list of work that can only shrink. **The audit does
not fix anything**; findings belong to the control's owner.

- Code: `clients/avalonia/tests/Spotledger.Client.TestSupport/A11y/`
- Baseline: `clients/avalonia/tests/Spotledger.Client.TestSupport.Tests/a11y-baseline.json`
- Coverage: `GalleryAccessibilityTests` audits the control catalogue, the grid gallery page and the
  shell chrome in Light, Dark and both high-contrast variants, LTR and RTL.
- Self-tests: `AccessibilityAuditTests` (a failing and a passing fixture per rule),
  `AccessibilityRatchetTests` (the ratchet itself), `ReducedMotionTests`.

## The rules

| id | Fails when | Example finding |
| --- | --- | --- |
| `missing-name` | an interactive control (button, text box, combo box, check box, list/tab/menu item, slider, numeric box) has no accessible name: no `AutomationProperties.Name`, no `LabeledBy` association, and nothing readable in its own content | `Edit has no accessible name and no label association (AutomationProperties.LabeledBy).` on `SpotDecimalInput` inside a `SpotField` whose visible label is not associated |
| `icon-only-button` | a button shows only a graphic and has no name | `A button whose content is only a graphic has no accessible name` |
| `duplicate-sibling` | two siblings expose the same name **and** role, so a screen reader cannot tell them apart | two `Save` buttons in one toolbar: `2 siblings share the Button name "Save"` |
| `grid-patterns` | a custom-drawn grid (a control named `…Grid` that is not the layout `Grid`) has no automation peer, reports no grid role, exposes no row items, or exposes no selection pattern | `Custom-drawn grid has no grid automation peer (role None)` on a `TestGrid` fixture |
| `tab-order` | in tab order, a control is reached before a control that is drawn earlier in reading order (rows top-to-bottom, then left-to-right, mirrored when `FlowDirection` is RTL) | `Tab order reaches Button "With icon" (tab #6) before Edit (tab #7), but Edit is drawn earlier (reading #2 vs #12)` |
| `focus-ring` | a focusable control changes nothing in the 2 px band hugging its edge when focused with keyboard navigation | `Button "New" paints nothing in the 2 px ring around its edge when focused` |
| `contrast` | a rendered text/background pair is below WCAG AA: 4.5:1, or 3:1 for large text (24 px, or 18.66 px semibold and heavier) | `1.55:1 is below 4.5:1 (ordinary text, #1D4ED8 on #1E3A8A): "Home"` |
| `motion` | with reduced motion enabled, a transition lasts longer than the bound (100 ms) | `Transition lasts 400 ms; the reduced-motion bound is 100 ms.` |

How two of them read the pixels, because that is the part people ask about:

- **Contrast** takes the background from the most frequent colour actually rendered inside the
  text's rectangle (so an opacified or layered surface is judged as drawn) and the text colour from
  the declared foreground when at least one pixel confirms it — otherwise the colour furthest from
  the background that covers enough pixels to be a glyph. A glyph too thin to render its own colour
  never produces a false failure.
- **Focus ring** captures the frame with nothing focused, focuses the control with
  `NavigationMethod.Tab` (so `:focus-visible` styles apply exactly as they do for a Tab press),
  captures again, and counts visibly different pixels in the band from 2 px inside to 2 px outside
  the control's edge. A control that cannot take focus is skipped; the ring is never inferred from
  a style.

## Run the audit on a control

```csharp
[AvaloniaFact]
public void MyControlMeetsTheAccessibilityRules()
{
    var window = A11yHost.Show(new MyControl(), 800, 600, ThemeVariant.Dark, FlowDirection.RightToLeft, "my-control-dark-rtl");
    try
    {
        var report = AccessibilityAudit.Run("my-control-dark-rtl", window);
        Assert.True(report.Count == 0, report.Describe());       // or ratchet it, see below
    }
    finally { A11yHost.Close(window); }
}
```

- The window must be **shown**: the pixel rules need a rendered frame (`Run` throws otherwise, so a
  forgotten `Show()` cannot look like a pass).
- `scope` names the audit and is part of every finding id; include the variant and the direction.
- Narrow the rules with `new A11yOptions { Rules = new HashSet<string>([A11yRule.Contrast]) }` when a
  test owns one rule; `Pixels = false` skips all pixel work; `FocusRingSample` (default 16) bounds
  the number of focus captures in a large window.
- `A11yHost` shows the same theme stack the Desktop host installs (Fluent, tokens, control themes).

## The ratchet baseline

`a11y-baseline.json` is the list of findings that exist today. `GalleryAccessibilityTests` fails when

- a finding appears that is not in the baseline (**new defect**: fix it or record it deliberately), or
- a baseline entry no longer matches any finding (**something was fixed**: delete the entry — the
  file may only shrink, so a fix cannot be forgotten), or
- the file is missing, malformed, has a wrong version or duplicate ids (an absent baseline is never
  "no findings").

Rewrite it only on purpose, after reading every change:

```sh
UPDATE_A11Y_BASELINE=1 dotnet test clients/avalonia/tests/Spotledger.Client.TestSupport.Tests \
  --filter FullyQualifiedName~GalleryAccessibility
dotnet test clients/avalonia/tests/Spotledger.Client.TestSupport.Tests   # prove the comparison
```

Ids are `scope|rule|control|path`, one entry per line; `path` is relative to the audited content, so
a change in the window host does not rewrite the file, while a change in a control template
legitimately shows up as removed + added entries. Review that diff as carefully as a screenshot
baseline. Today's file holds 353 findings: 328 `missing-name`, 24 `tab-order`, 1 `contrast` — the
gallery fields' visible labels are not associated with their inputs, which is the largest single
accessibility debt in the client.

## Reduced motion and high contrast

- `ReducedMotion` (TestSupport only) is the seam: `using (ReducedMotion.Enable())` makes
  `ReducedMotion.Bound` 100 ms and `Clamp` cut longer durations. `ReducedMotionTests` asserts every
  catalogue duration (`Motion.Fast` 120 ms, `Motion.Normal` 200 ms, `Motion.Slow` 320 ms) is clamped,
  and that no control in the galleries or the shell carries a transition longer than the bound.
  The theme catalogue has **no** `motion.reduced` token yet; adding one is the theme owner's task.
- High contrast: the gallery audit runs the same rules under `ThemeVariants.HighContrast` and
  `HighContrastDark`. When a palette is absent the audit skips those scopes and
  `EveryPlannedThemeVariantIsAvailable` skips with a message naming the missing variant — the
  scopes are never audited as Light and reported as passing.

## Still manual

Automated checks cannot tell whether an announcement is intelligible. Before a release:

**Windows, Narrator** (`Win+Ctrl+Enter`):

1. Tab through the shell: every stop announces a name, a role and its state ("Amount, edit,
   required"), and the order matches the screen.
2. Open the command launcher with the keyboard only: the focus moves into it, Escape returns focus
   to where it was.
3. Read the grid with the table commands: rows, columns and the focused cell are announced, and
   selection changes are reported.
4. Switch Windows to a high-contrast theme and repeat 1–3: focus is always visible.

**Fedora, Orca** (`orca --replace`, or Settings → Accessibility → Screen Reader):

1. Same walk in browse and focus modes; check that the status bar and error messages are announced
   (they are live regions).
2. Toggle the Urdu/RTL layout: reading order follows the visual order, numbers still read as
   numbers.
3. `Alt+F2` → `accerciser` when a control is silent: it shows whether the name, role and states the
   audit checks are what the AT-SPI bridge actually publishes.

VoiceOver and macOS are out of scope for this gate.

## Limitations (deliberate, and where the fixes live)

- Avalonia 12 has no `IGridProvider`/`ITableProvider`, so `grid-patterns` holds what a screen reader
  can actually use: a peer, a grid role, row items and a selection pattern. If a future Avalonia
  adds the providers, tighten the rule rather than adding a second one.
- Text contrast is judged for `TextBlock`; borders, icons and other non-text UI (3:1) are held by
  the token contract tests in `TokenContractTests`, not sampled here.
- The focus-ring rule samples at most `FocusRingSample` controls per window (sorted by path, so the
  sample is deterministic). Raise it in the options when auditing a small, important window.
- Data-grid cell editors are audited through their grid's peer; a cell editor that only exists while
  editing is not in the tree at rest.
