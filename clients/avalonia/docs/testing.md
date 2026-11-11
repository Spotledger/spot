# Avalonia UI testing

## Headless tests

Use the existing `Avalonia.Headless.XUnit` and xUnit v3 packages, not a second test
framework or a new renderer dependency. A rendering test uses `[AvaloniaFact]` or
`[AvaloniaTheory]`; a plain `[Fact]` does not initialize the headless application.
Register an `AvaloniaTestApplication` factory at assembly scope and serialize
collections with `[assembly: CollectionBehavior(DisableTestParallelization = true)]`.
Avalonia's application and dispatcher are process-global.

For actual pixels, follow `Spotledger.Client.Grid.Tests/TestApp.cs`: configure
`.UseSkia().UseHarfBuzz().UseHeadless(...)` with `UseHeadlessDrawing = false`.
The Shell tests obtain Skia through their existing Desktop project reference.
A null `CaptureRenderedFrame()` is a failure, never an empty baseline. Load the
same token resources and control themes as the control's real host before rendering.

Show the control in a window of an explicit logical width and height. Assert its
state and layout before capturing. Pump `Dispatcher.UIThread.RunJobs()` and
`AvaloniaHeadlessPlatform.ForceRenderTimerTick()` to process work and draw frames;
use a bounded condition-driven pump with a diagnostic on exhaustion, not an
unbounded loop. Close each window and dispose every captured bitmap. Restore any
global theme or culture in `finally`/`Dispose`.

## Time and inputs

Do not wait for a guessed real duration. Supply an injectable clock or
`TimeProvider` where the component already accepts one. A test clock must advance
scheduled work deterministically; injecting a timestamp alone does not control a
real timer. If a component has no clock seam, record the prerequisite for its owner
rather than adding a real sleep to a test. Use dispatcher pumping for dispatcher
work, a completion task for asynchronous fake-source work, and native headless
keyboard/pointer APIs for input. Fake sources are local, deterministic and bounded;
performance tests must never make network requests.

## Screenshot matrix and names

Name a case by its state, logical dimensions, density, theme and direction, for
example `grid-selected-960x480-compact-dark-rtl.png`. One `ScreenshotMatrix` case
has four dimensions and an optional fifth: **density** (Comfortable, Compact),
**theme** (`dark` for the light/dark pair, or `Variant` for any of the four token
palettes), **direction** (LTR, RTL), and **per-OS** (`PerOs`, only for a
documented rendering exception). Exercise Comfortable and Compact density, Light
and Dark themes, LTR and RTL, and the component's supported width breakpoints. Set
`Density.SetMode` for Spot controls, `RequestedThemeVariant` for the host and
`FlowDirection` explicitly. RTL must include actual mirrored layout assertions, not
just an RTL suffix. Fix culture, scale and fonts for a text-bearing baseline;
changing the fonts or rasterizer requires baseline review.

The stem is `name-WxH-density-theme-direction`, with the theme spelled exactly
`light`, `dark`, `highcontrast` or `highcontrastdark`
(`ScreenshotMatrix.PaletteName`), plus `-windows`/`-linux`/`-macos` when `PerOs` is
set. Never rename a case to make a baseline match.

### Theme variants in the matrix

`Variant: ThemeVariantKind.HighContrastDark` makes the palette a matrix dimension
instead of a second screenshot helper:

```csharp
var window = ScreenshotAssert.MatchesWindow("urdu-field-text", control,
    new ScreenshotMatrix(baselines, 480, 140, density, dark: variant.IsDark(), rtl: true, Variant: variant));
// assert direction/density/variant on `window`, then:
ScreenshotAssert.Close(window);
```

- `Variant: null` is the default and behaves and names exactly as before: `dark`
  decides. With `Variant` set the file is named after the variant, and it **must
  agree with `dark`** — a dark palette needs `dark: true`; a contradiction throws
  `ArgumentException` instead of quietly picking one. Leave `dark` alone only for
  the light palettes.
- The variant is applied to the window **and** to
  `Application.RequestedThemeVariant` for the duration of the capture, and the
  previous application variant is restored afterwards — on success and on the
  failing-comparison path. A window-only variant is not enough: token brushes
  resolve against `Application.ActualThemeVariant`, so it paints a dark screen with
  a light grid.
- A variant case also paints the window with the palette's `Color.Surface.Canvas`
  token, so a host that installs no built-in theme shows the palette rather than
  the host's own window brush.
- `ScreenshotAssert.MatchesWindow` returns the open host so a case can assert the
  window it asked for; `Matches` closes the host for you.

A new variant is a theme change first: add the field to `ThemeVariants`
(`src/Spotledger.Client.Theme/ThemeVariants.cs`), the palette to
`design/tokens.json`, the `ThemeVariantKind` member, and its stem spelling in
`ScreenshotMatrix.PaletteName`. A custom theme key must be a `ThemeVariant` field
referenced from XAML as `x:Static`; Avalonia's type converter refuses a custom key
written as text, and its dictionary must inherit Light or Dark so built-in control
themes keep resolving (WP-UI-15.T3).

Linux is the first baseline platform. Baselines are **per platform**, in
`TestSupport.Tests/Baselines/<platform>/` (`linux`, `windows`, `macos`):
`ScreenshotAssert.PlatformBaselineDirectory(root)` picks the directory from the OS that is
rendering, and there is no fallback — a case with no file for the running platform fails and
prints the exact command that creates it. A text-free primitive may have byte-identical images on
two platforms; it still has one file per platform, because a shared file is how the wrong
platform's pixels get approved by accident. `tools/ci/ui/screenshot_baselines.py --platform linux
--check` proves the inventory is complete before the build (the `ui` workflow runs it for every
matrix row); a case that genuinely exists on one platform only is listed in
`Baselines/exceptions.json` with a written reason. **Approved exceptions: none yet.** Do not copy
a Linux capture into a purported Windows baseline.

The tolerance is the same on every platform: the shared comparer normalizes images to RGBA and
marks a pixel changed when any channel differs by more than **2** (0–255). At most **0.001** (0.1%)
of pixels may change by default. `ScreenshotTolerance(delta, fraction)` makes stricter or reviewed
case-specific tolerances explicit; both are validated. Dimensions and missing baselines fail
regardless of pixel tolerance. The diff is red for changed pixels, black for unchanged pixels, and
magenta for pixels absent on one dimension canvas. Ordinary tests never update baselines.
`UPDATE_BASELINES=1` is the explicit update switch (`SPOT_UPDATE_SHOTS=1` from part 1 still works);
CI pins both to `0`. On mismatch, real actual/expected/diff PNGs and a per-case HTML review page are
written below `SPOT_SHOTS_OUT` (or `matrix.OutputDirectory`). A missing baseline retains only the
real actual: no expected image or diff is fabricated.

### The Windows row

The Windows matrix row (paused by `SPOT_CI_OS`, see the CI runner notes) renders the same
matrix cases and compares them with `Baselines/windows/`. Two ways to create those files, both on
a Windows runner — **never** on Linux and never by copying:

1. A `workflow_dispatch` run of the `ui` workflow with `generate_baselines: true` and
   `os: ["windows-latest"]`. The row sets `UPDATE_BASELINES=1`, renders every case, re-runs the
   comparison against the fresh files to prove they are clean, and uploads
   `shots/generated-baselines/` plus the `git status` of `Baselines/`. Nothing is committed by CI:
   download the artefact, review every PNG, commit it under `Baselines/windows/`.
2. On the Windows VM runner: `UPDATE_BASELINES=1 dotnet test
   clients/avalonia/tests/Spotledger.Client.TestSupport.Tests --filter
   FullyQualifiedName~CommittedPrimitiveMatrix`, then rerun without the flag.

Until those files exist the Windows row fails at the (cheap) inventory step with the exact command;
it does not compare Windows pixels with Linux ones and it does not pass.


Baseline changes require the reviewer's inspection of all three images, a reason
for the visual change, the affected matrix cases, and confirmation that dimensions,
fonts and renderer were unchanged (or an explicit explanation of their change).
Never approve a baseline simply because an update makes a failing test pass.

### Existing captures are not comparison gates

`Shell.Tests/ScreenshotParityTests.cs` currently writes ten PNGs directly into
`clients/avalonia/docs/shell-screenshots/` during an ordinary test run. These are
captures, not assertions against stored pixels. `Grid.Tests/GridScreenshotTests.cs`
writes captures below its build output. Neither is a visual regression gate.
Check `git status` after running the Shell suite; do not commit incidental baseline
rewrites. Migrating these tests requires their owners; this task does not own them.

## Performance measurements

Keep source construction, initial resource loading and unrelated I/O outside the
measured operation unless the budget explicitly measures cold start. Warm up the
same operation, discard warm-up samples, then collect a fixed positive sample
count. Measure wall time using `Stopwatch` and synchronous allocations using
`GC.GetAllocatedBytesForCurrentThread`; do not attribute allocations across an
`await` to a single thread. Report the sample count and p50/p95/max. A first-paint
measurement must force a real paint and verify that the intended rows appeared;
invalidating a visual alone does not establish that it was drawn.

Budgets must state their metric, unit and runner class. Ordinary CI machines are
not reference hardware merely because they run Linux. Any multiplier must be
reviewed data, not an implicit `CI` flag heuristic. The proposed result record is
`id, measured, max, runner, commit`; record breaches before failing the test and
make missing measurements fail the aggregate gate. Never substitute zero for a
missing result or treat an unrecognized declaration as no budgets.

## Shared helpers and CI policy

The decisions behind this suite are recorded in its pull-request review.
The `ui` workflow uses `SPOT_CI_OS` (JSON OS list, default `["ubuntu-latest"]`)
and the same `SPOT_RUNNER` / `SPOT_RUNNER_WINDOWS` selection as `ci.yml`.
Windows capability and configuration are present but Windows remains paused; no
Windows result is required by a Linux invocation. There is no reference runner yet.
Only `contents: read` is granted; the table is written to `GITHUB_STEP_SUMMARY`,
not posted using a write token or a new PR-comment publisher.

### Budget data

The dependency-free .NET reader accepts the following deliberately small TOML
subset: `[[budget]]`, `[runner.<bare-label>]`, one key/value per line,
double-quoted JSON-compatible basic strings, positive decimal/exponent numbers,
blank lines and `#` comments. No arrays, dates, multiline strings, dotted keys,
inline tables or implicit defaults for required fields. Unknown/duplicate keys,
empty catalogues and malformed values fail. The Python gate uses `tomllib` and
validates the same schema; author files in the documented shared subset.

```toml
[[budget]]
id = "grid.first-paint"
metric = "p95"             # p50, p95, max, mean (nearest-rank percentiles)
max = 50
unit = "ms"                # ms or bytes
runner = "reference"       # reference, ci-linux, ci-windows

[runner.ci-linux]
multiplier = 3.0
[runner.ci-windows]
multiplier = 3.0
[runner.reference]
multiplier = 1.0
```

Each file carries its own runner multiplier table. `SPOT_PERF_RUNNER` identifies
the executing label; `SPOT_RUNNER_KIND` is `hosted`, `self-hosted`, or `reference`.
Workflow identity comes from GitHub's runner context, not a measurement record.
Hosted and verified reference runs are always **1x** and breaches fail. A shared
self-hosted runner uses its label's configured multiplier or **3x** when absent
(including unlabelled/local runs); a multiplied breach is **WARNING**, per the
lead's temporary no-reference policy. Setting a multiplier to 1 makes its breach
fail. Reference kind requires the explicit reference label; never assign that
label to ordinary `spot` hardware. Every summary loudly states the executing
kind/label and each applied factor. Missing, stale, malformed or unknown results
always fail, including on multiplied runners. Equality to a maximum is allowed.

`runner = "ci-windows"` declarations are active only in a Windows invocation;
`reference` declarations are measured on each enabled OS with that OS's policy.
The example table does not enable Windows or establish reference hardware.

```csharp
var budgets = PerfBudget.Load("perf-budgets.toml");
var samples = PerfMeter.Measure(PaintAndPump, samples: 20, warmup: 3);
new Budget(budgets).Assert("grid.first-paint", samples);
// Or Assert(id, measured) for an already measured scalar.
```

`Measure` accepts synchronous actions only, discards warm-up, and returns count,
p50/p95/max/mean for wall milliseconds and per-thread allocated bytes. An optional
`prepare(index)` runs outside timing/allocation measurement (warm-up indices are
negative); use it to set up a scroll, not to move measured rendering out of the
operation. No real sleeps, network or async-void operations. `Assert` chooses the
budget's unit/statistic and writes a unique atomic JSON file to `SPOT_PERF_OUT`
**before** throwing on an enforced breach. Records include `id, measured, max,
runner, commit, metric, unit, multiplier, allowed, status`, plus full statistics
when passed a measurement. `max` is always the unscaled declaration; `allowed`
is the scaled value. Commit is `GITHUB_SHA` or `local`. Failures writing evidence
are failures, never silently ignored. Contract tests use isolated temporary output
so synthetic breach fixtures cannot pollute real CI evidence.

The gate discovers every source `perf-budgets.toml` under the test root (excluding
`bin`/`obj`). It rejects duplicate declarations and missing measurements. It
recomputes maxima/multipliers from budget files and uses the worst observation if
an ID was executed more than once; neither a forged record status nor a later pass
can hide a breach. It validates commit, runner, metric, unit and unscaled maximum.
CI uploads JSON, screenshot diffs/HTML and TRX even when tests or the gate fail.

```sh
python tools/ci/ui/perf_gate.py --budget-root clients/avalonia/tests --results "$SPOT_PERF_OUT"
# For a narrow local catalogue, repeat --budget FILE instead of --budget-root.
```

### Legacy Grid bridge (implemented, no owner edits)

The flat Grid catalogue is read compatibly without changing its thresholds:
`max_frame_ms` → `grid.frame.max` (max/ms), `max_alloc_bytes_per_frame` →
`grid.frame.alloc.mean` (mean/bytes), `first_paint_50_rows_ms` → `grid.first-paint`
(p95/ms). `rows`, `columns` and `scroll_steps` remain positive workload parameters.
Mixing legacy and new declarations is invalid; no unknown flat key is ignored.

Owned `MeasuredUiTests` supplies real results for all three legacy budgets until
Grid's owner migrates its assertions. The scroll bridge renders a 100000×12 fake
source in 300 measured render/dispatcher steps with ten warm-up steps; the
protected production keyboard handler pages between samples outside the meter.
Fresh automation peers and real pixel capture confirm rows near the end of the
source. The first-paint bridge warms three times and measures ten fresh hosts,
including capture and assertions of at least 50 real visible rows in a 2200-pixel
host. This is a conservative integration workload, not a claim of identical timing
to the owner's current best-of-five 600-pixel host. Original tests still run and
retain their own assertions: this task does not bypass their possible wall-clock
flake. Shared-runner aggregate warnings do not suppress ordinary test failures.

### Screenshot use and committed coverage

```csharp
ScreenshotAssert.Matches("selected", control,
    new ScreenshotMatrix(baselines, 960, 480, DensityMode.Compact, dark: true, rtl: true));
```

The helper creates/closes its own fixed-size undecorated host at headless scale 1,
sets host theme, control density and inherited direction, and pumps bounded render
work. Pass an unattached control. Application/theme resources are supplied by the
test host, not fabricated by the helper. Captures and normalization buffers are
disposed; the caller retains the control. Names are plain case identifiers with
matrix dimensions/density/theme/direction appended. `PerOs = true` adds an OS suffix
only for a documented exception; there is no fallback to another OS baseline.
A variant case captures until two consecutive frames are byte-identical before it
compares: its baselines were recorded from a settled frame, so the comparison must
see one (a frame that never settles fails loudly instead of being compared). The
plain `dark` case keeps the single-frame capture its baselines were recorded with.

Eight committed text-free primitive baselines in `TestSupport.Tests/Baselines/linux` exercise both
densities/themes/directions on Linux. They test the shared helper, not full Shell/Grid visual
parity. They have no OS exceptions, and `Baselines/windows` is created the same way on a Windows
runner. Generate explicitly, inspect the images, and then rerun without the update flag:

```sh
UPDATE_BASELINES=1 dotnet test clients/avalonia/tests/Spotledger.Client.TestSupport.Tests --filter FullyQualifiedName~CommittedPrimitiveMatrix
dotnet test clients/avalonia/tests/Spotledger.Client.TestSupport.Tests
python tools/ci/ui/screenshot_baselines.py --platform linux --check
```

`PerOs = true` is no longer needed (every baseline is per platform); it is kept only so an existing
call keeps compiling, and it still never falls back to another OS.

## Accessibility gate

`AccessibilityAudit` walks a shown window and reports missing accessible names, icon-only buttons,
ambiguous siblings, a custom-drawn grid without a grid peer, tab order that contradicts reading
order (mirrored in RTL), an invisible focus ring and WCAG AA contrast — from the rendered pixels for
Light, Dark and both high-contrast variants. It is a ratchet, not a cliff: findings are committed in
`TestSupport.Tests/a11y-baseline.json` and only a new one fails. Rules, how to audit a new control,
how to update the baseline and the manual Narrator/Orca checks are in
[`accessibility.md`](accessibility.md).

## Browser visual baselines

`tools/browser-visual/` captures the published browser head in headless Chromium at 1280x800,
768x1024 and 390x844, drives it through `window.spotBrowser`, and compares the screenshots with
`baselines/browser/` using the same tolerance as the .NET comparer (ported to Python, no .NET call).
States the head cannot reach are reported as unavailable rather than captured from a fixture. See
`tools/browser-visual/README.md`; it runs as the opt-in `browser-visual` job in the `ui` workflow.

### Proposed owner integration (not performed)

1. Grid project: add a project reference to TestSupport; migrate the three flat
   thresholds to stable IDs `grid.frame.max`, `grid.frame.alloc.mean` and
   `grid.first-paint`, preserving the current 8 ms, 4096 bytes and 50 ms reference
   values and the 100000-row/12-column/300-step workload. Obtain reviewed runner
   multipliers rather than deriving them from the existence of `CI`.
2. `PerfBudgetTests.FirstPaintOfFiftyRowsUnderBudget`: replace the local parser,
   best-of-five assertion and `CI ? 3 : 1` heuristic with the shared loader,
   explicit warm-up and measured samples; assert real rendering before recording.
3. `PerfBudgetTests.ScrollOf100kRowsIn300StepsStaysWithinBudgets`: replace local
   stopwatch/allocation bookkeeping and final assertions with shared measurements
   and three declared result IDs; force actual rendering instead of measuring only
   `InvalidateVisual`. Retain the row-count/fixture validity assertions.
4. Shell project: add a TestSupport project reference when migrating captures.
   There are no existing Shell stopwatch/allocation budget assertions to replace.
   `TestHarness.WaitUntilAsync` is a completion deadline, not a performance budget.
5. Replace `ScreenshotParityTests.Capture`'s unconditional docs writes and
   `GridScreenshotTests.Capture`'s output-only captures with real comparisons after
   the helper exists; keep their current scenario setup and state assertions.
   Review and explicitly generate baseline matrices rather than accepting current
   capture side effects as baseline approval.

## Tests that depend on load

Two kinds of UI test used to pass on an idle machine and fail on a loaded one.

**The accessibility gallery test.** `A11yHost.Show` used to pump one frame and the contrast rule read
that frame, so a colour that is mid-way through a clock-driven transition (#AE6364 is a blend) was
audited as if it were the control's colour. `A11yHost.Show` now calls `A11yHost.Settle`: it
captures frames, with the dispatcher pumping and at least 20 ms of real time between two compared
frames, until two consecutive frames are pixel-identical. It is bounded (2 s, 60 frames); a window that
never settles fails with the frame count, the first differing rectangle and the control under it
instead of being audited. A control that animates for ever (a spinner) must be stopped by the test.

Reproduction (2026-10-30, Linux 7.2, the shared dev workstation at load average about 25):
`dotnet test ... TestSupport.Tests` three times as it was, and three times under `taskset -c 0` with a
busy loop on the same core. All six runs passed the gallery test, so the three contrast findings
("Ready", "Selling", "Accounts") could **not** be reproduced here; the fix follows the diagnosis from
the failing CI log and is covered by the settle tests (`ASettledWindowIsAuditedAfterItsOpacityTransitionEnded`,
`AControlThatNeverSettlesFailsWithAClearMessage`).

**Budget assertions.** The local `Assert.True(... * Scale)` of the Reports and Lists `BudgetTests` used
the constant `CI is null ? 1 : 3`. `LoadScale.Applied` is that constant times `LoadScale.Current`:
`SPOT_UI_BUDGET_SCALE` if set (a positive number), otherwise on Linux `clamp(loadavg1 / cores, 1, 4)`
from `/proc/loadavg`, otherwise 1. At or below one load per core nothing changes, so an idle machine
keeps exactly the declared budget; every failure message prints `LoadScale.Describe()` (the scale and
the load average it came from).
