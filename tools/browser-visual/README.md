# Browser visual baselines (WP-UI-16.T2)

Screenshot regression for the published Avalonia browser head, at the three viewports the brief
fixes, driven only through the page contract in `clients/avalonia/docs/browser-publish.md`
(`window.spotBrowser.ready/openForm/openList/metrics`). Comparison uses the same tolerance as the
.NET screenshot helper — a pixel is changed when any of R, G, B or A differs by more than **2**,
and at most **0.1%** of pixels may change — ported to Python in `src/compare.py`. Nothing here
shells out to .NET and nothing adds a dependency: PNG decode/encode is `src/png.py`, Playwright is
the version WP-UI-09.T3 pins (`tools/browser-budgets/package.json`, 1.64.1).

## Run it

```sh
dotnet publish clients/avalonia/src/Spotledger.Client.Browser -c Release -p:SpotBrowserAot=false \
  -o artifacts/browser-publish            # the interpreted build: fast, and what the baselines use
bash tools/browser-visual/run.sh          # capture + compare against baselines/browser/
bash tools/browser-visual/run.sh --update # capture and write the baselines (review every PNG first)
bash tools/browser-visual/run.sh --self-test
python -m unittest discover -s tools/browser-visual/tests -v
```

`run.sh` finds Playwright in `tools/browser-budgets/node_modules`, in `$SPOT_PLAYWRIGHT_HOME`, or
in `tools/browser-visual/node_modules`, in that order; it installs it where WP-UI-09.T3 pins it
when none is present. Chromium lives in Playwright's own cache, never in the repository.

Environment: `SPOT_BROWSER_PUBLISH` (web root, default `artifacts/browser-publish/wwwroot`),
`SPOT_BROWSER_VISUAL_OUT` (results root, default `tools/browser-visual/results`).

## What is captured

`scenarios.json` is the single list: viewports **1280x800**, **768x1024**, **390x844** and the
states *login* (signed-out start screen), *home with tabs*, *a list*, *a form* and the two gallery
pages. Each capture is `<scenario>-<width>x<height>.png` plus `manifest.json`, which records the
app-ready metrics, the captures and — for a state the published head cannot reach — an
`unavailable` entry with its reason. An unavailable state is reported on every run and is never
captured from a fixture: the gallery pages need a browser route the head does not have yet, and
list/form tabs need sign-in, so today they are reported, not faked.

Captures are deterministic: fixed viewport, device scale 1, `reducedMotion: reduce` (animations
finish before the frame), light colour scheme, en-GB locale, UTC, animations disabled in the
screenshot and the caret hidden.

## Baselines

`baselines/browser/` holds the reviewed PNGs. A missing baseline **fails** and prints the exact
command that creates it (`bash tools/browser-visual/run.sh --update`); there is no fallback to
another platform or to the current capture. On a mismatch the comparator writes
`results/review/<case>-expected.png`, `-actual.png`, `-diff.png` and an HTML page (red = changed,
black = unchanged, magenta = absent on one canvas) and the CI job uploads them.

Never approve a baseline because updating makes a failing test pass: look at all three images, say
why the pixels changed, and list the affected cases.

## CI

`.github/workflows/ui.yml` runs this as the `browser-visual` job, which is opt-in through the
repository variable `SPOT_UI_BROWSER_VISUAL=1` (or a `workflow_dispatch` run with
`browser_visual: true`) until the baselines are committed from a real publish. The job publishes
the interpreted build, captures, compares and uploads `results/` — including the review images.
