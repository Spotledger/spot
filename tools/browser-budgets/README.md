# tools/browser-budgets

The browser budgets of the Avalonia browser target and the runner that measures
them. Numbers and the machine they are for live in `budgets.toml`; `run.sh`
serves the published build from a local static server, drives it through the
`window.spotBrowser` hooks, and writes one result JSON per budget in the format
`tools/ci/ui/perf_gate.py` reads.

| File | What it is |
| --- | --- |
| `budgets.toml` | The seven budgets: `max`, `unit`, `metric`, `quick`, the network `profile`, a `# reference:` comment naming the hardware, and `proposed = true` where no source defines the number |
| `perf-budgets.toml` | Generated projection in the gate's format (`budget_cli.py gate --write`); a unit test fails when it drifts from `budgets.toml` |
| `budgets.py` | Catalogue parsing and validation, percentiles, aggregation, result schema |
| `budget_cli.py` | `validate`, `gate`, `plan`, `aggregate`, `check` — all the arithmetic and every verdict |
| `src/measure.mjs` | The Playwright driver: one raw JSON per run holding samples, never a verdict |
| `src/probe.js` | Injected page instrumentation (animation-frame intervals, key-to-frame latency, WebAssembly memory) |
| `src/serve.mjs` | Static server: Brotli/gzip siblings, `application/wasm`, the cache headers `docs/browser-publish.md` requires |
| `fixture/` | A page that implements exactly the `window.spotBrowser` contract, for `--self-test` and for `--quick` on a machine with no published build |
| `tests/` | `unittest` tests: no network, no browser, no .NET |

The page contract the driver relies on is
`clients/avalonia/docs/browser-publish.md` (hooks `ready`, `openForm`,
`openList`, `metrics()`). The driver adds nothing to it: the extra page
instrumentation is injected by Playwright (`src/probe.js`) and the metrics come
from the documented marks.

## Run it locally

```sh
dotnet publish clients/avalonia/src/Spotledger.Client.Browser -c Release -o artifacts/browser-publish   # once
bash tools/browser-budgets/run.sh --quick
```

`run.sh` installs the pinned npm packages (`npm ci`) and Chromium if they are
missing, into Playwright's own cache (`~/.cache/ms-playwright`, or
`PLAYWRIGHT_BROWSERS_PATH`), never into the repository. Nothing is committed
except `package.json` and `package-lock.json`.

| Mode | What it does |
| --- | --- |
| `--quick` | Headless Chromium, one run per measure, only the `quick = true` budgets (every budget today). The CI mode; well under five minutes |
| `--full` | Ten runs per measure; reports p50 and p95 beside the number the budget names |
| `--self-test` | The Python unit tests, then the driver against `fixture/`, then a good and a deliberately breaching aggregation. No .NET build, no network |

A breach, a missing result (a measure that produced no sample) or invalid
evidence exits non-zero. Absence is never a zero.

When no published build is found, `--quick` falls back to `fixture/` and says so
loudly; every result then carries `"fixture": true`. In CI (`CI` set) that
fallback is refused unless `SPOT_BUDGETS_ALLOW_FIXTURE=1`, so a fixture page can
never stand in for the build.

| Environment | Default |
| --- | --- |
| `SPOT_BROWSER_PUBLISH` | `artifacts/browser-publish/wwwroot` |
| `SPOT_PERF_OUT` | `tools/browser-budgets/results/<mode>/perf` (git-ignored) |
| `SPOT_BUDGET_RAW` | the parent of `SPOT_PERF_OUT` plus `/raw` (git-ignored) |
| `SPOT_PERF_RUNNER` | `ci-linux` (recorded in every result; the gate needs the same label) |
| `GITHUB_SHA` | `local` |

## Signed-in measures (stub backend)

`browser.form.open`, `browser.list.scroll.frame` and `browser.child-edit.key` need a signed-in shell.
`run.sh --quick|--full` passes `--signed-in` by default when `CI` is set (and `--signed-out` skips it):
the driver starts the loopback stub in `stub-backend/` (see its README for the test credential and its
safety rules), types the stub's URL into the shell's server picker, signs in on the login screen with the
keyboard, and only then measures. Startup, download and memory stay signed-out (memory runs before the
sign-in). A failed sign-in records the same error on the three signed-in ids and saves
`sign-in-failure-run-N.png` next to the raw samples. Against the fixture page (`--fixture`) the sign-in
screens do not exist, so the stub is started and exercised over HTTP only.

## Read a result

One file per budget, named after its id, and nothing else: the gate refuses a
directory that holds a file it cannot match to a budget, so the raw per-run
samples live in a sibling `raw/` directory.

```json
{
  "id": "browser.startup.cold",
  "measured": 3600,
  "max": 4000,
  "unit": "ms",
  "metric": "p95",
  "runner": "ci-linux",
  "commit": "local",
  "status": "PASS",
  "count": 1,
  "stats": { "count": 1, "p50": 3600, "p95": 3600, "max": 3600, "mean": 3600 },
  "samples": [3600]
}
```

`id`, `measured`, `max`, `unit`, `metric`, `runner` and `commit` are the gate's
fields. `status` is this runner's own verdict against the **unscaled** maximum;
`BREACH` (not `FAIL`) keeps it apart from the gate's vocabulary, because the
gate recomputes everything from the budget files and applies the reviewed
multiplier — it is the authority. `stats` and `samples` are the evidence behind
the single number.

```sh
python tools/ci/ui/perf_gate.py --budget tools/browser-budgets/perf-budgets.toml \
  --results tools/browser-budgets/results/quick/perf
```

Percentiles are nearest rank, so a percentile of byte counts stays an integer:
no floating-point arithmetic where a count is meant. Timings are floats because
`performance.now()` is.

## An informational budget

`[[budget]]` may carry `informational = true` plus a mandatory
`informational_reason` string (who decides and since when). An informational
budget's breach is still printed and still written into its result file
(`"status": "BREACH"`), but it never makes `budget_cli.py aggregate`/`check`
(so `run.sh`, so the CI step) exit non-zero. A **missing** result for an
informational budget still fails: absence is never a pass, informational or
not. The result JSON always carries `"informational": true`/`false` so a
reader of the artefact (or `perf_gate.py`'s evaluation, were it ever pointed
at these files directly) can tell which is which.

This is **not** a way to silence a real regression: it exists for exactly one
situation — a number the lead has not yet decided on (see `browser.startup.warm`
below). `tools/ci/ui/perf_gate.py`'s own projection, `perf-budgets.toml`, never
carries the flag (it rejects unknown `[[budget]]` keys); the informational
mechanism lives entirely in this package's own verdict, which is what the CI
step actually gates on.

Today only `browser.startup.warm` carries it: SPK-07's draft is <= 1.5 s and
the CI runner measures p95 1.67 s. Raising the `max` is the lead's call, not
a tuning knob this flag is a shortcut around — see "Change a budget" below.

## Change a budget

**A budget change needs the lead.** It is a commitment made to the platform, not
a tuning knob, and raising a maximum to make a run pass is exactly what the gate
exists to prevent. Open an issue (or ask on the task issue) with the number, its
metric, the measurement that justifies it and the hardware it is for; the lead
decides.

Once a change is approved, in the same pull request:

1. Edit the `[[budget]]` block in `budgets.toml`: `max`, `unit`, `metric` and
   the `# reference:` comment (the parser rejects a block without one).
2. Regenerate the projection: `python3 tools/browser-budgets/budget_cli.py gate --write`.
3. Run `bash tools/browser-budgets/run.sh --self-test` and
   `python -m unittest discover -s tools/browser-budgets/tests -v`.

Adding a budget also means adding its measure to `src/measure.mjs` (a `STARTUP`
or `OPERATIONS` id) and a passing sample to `tests/_support.all_values`.

## What these numbers are, and what they are not

- The reference hardware and bandwidth are stated in `[reference]`: a 4-core
  laptop with a GPU, 50 Mbit/s at 20 ms RTT for the startup measures, no
  throttling for the operation measures. SPK-07's 4-vCPU container without a GPU
  is not reference hardware.
- `browser.download.compressed` is proposed: no source defines a size number
  (SPK-07 reports 9.80 MB of Brotli files for the AOT build and marks the row
  "report").
- Only the reference multiplier is declared. A self-hosted `ci-linux` run
  therefore uses the documented 3x default and a multiplied breach is a
  WARNING until the lead publishes reviewed factors — that is the policy in
  `clients/avalonia/docs/testing.md`, not something this directory decides.
- `browser.child-edit.key` and `browser.form.open` drive the shell's tabs; until
  WP-UI-09.T2 supplies real form and list views they measure a placeholder
  surface, so they are a runner for the measure, not evidence about grid cell
  editing yet.
