#!/usr/bin/env bash
# Browser budget runner (WP-UI-09.T3).
#
#   run.sh --quick      one headless-Chromium run per measure, the quick = true
#                       budgets only (the CI mode); under five minutes
#   run.sh --full       ten runs per measure, reports p50 and p95
#   run.sh --self-test  the Python unit tests plus the driver against the
#                       built-in fixture page and the stub backend; needs no
#                       .NET build and no network beyond the npm/browser cache
#
#   --signed-in / --signed-out (with --quick or --full): start the loopback stub
#   backend and sign the shell in before the signed-in measures (the default when
#   CI is set), or skip that and leave those three results missing (the default
#   elsewhere). See stub-backend/README.md.
#
# A breach, a missing result or invalid evidence exits non-zero.
#
# Environment:
#   SPOT_BROWSER_PUBLISH  web root of the published build
#                         (default: artifacts/browser-publish/wwwroot)
#   SPOT_PERF_OUT         directory for the result JSON the gate reads
#                         (default: tools/browser-budgets/results/<mode>/perf)
#   SPOT_BUDGET_RAW       directory for the raw per-run samples
#                         (default: the parent of SPOT_PERF_OUT plus /raw)
#   SPOT_PERF_RUNNER      runner label recorded in every result (ci-linux)
#   PLAYWRIGHT_BROWSERS_PATH  browser cache outside the repository
#                         (default: Playwright's ~/.cache/ms-playwright)
#   SPOT_BUDGETS_ALLOW_FIXTURE=1  let --quick measure the fixture page when the
#                         published build is missing (never in CI)
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
mode=quick
signed=""

usage() {
  sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

for argument in "$@"; do
  case "$argument" in
    --quick) mode=quick ;;
    --full) mode=full ;;
    --self-test) mode=self-test ;;
    --signed-in) signed=in ;;
    --signed-out) signed=out ;;
    -h|--help) usage; exit 0 ;;
    *) echo "run.sh: unknown argument: $argument" >&2; usage >&2; exit 2 ;;
  esac
done

die() { echo "run.sh: $*" >&2; exit 1; }

python=${PYTHON:-python3}
command -v "$python" >/dev/null || die "python3 is required"
command -v node >/dev/null || die "node is required (>= 20)"
command -v npm >/dev/null || die "npm is required"

# Browsers live in Playwright's own cache (~/.cache/ms-playwright) or in
# whatever PLAYWRIGHT_BROWSERS_PATH names: never inside the repository.
# `npm ci` must never download a browser; run.sh installs one deliberately below.
export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
export SPOT_PERF_RUNNER=${SPOT_PERF_RUNNER:-ci-linux}
commit=${GITHUB_SHA:-local}
budgets=$here/budgets.toml
cli=$here/budget_cli.py

# The signed-in measures need the stub backend; CI gets them by default.
if [ -z "$signed" ]; then
  if [ -n "${CI:-}" ]; then signed=in; else signed=out; fi
fi

runs=1
if [ "$mode" = full ]; then runs=10; fi

ensure_node_modules() {
  if [ -d "$here/node_modules/playwright" ] && [ -d "$here/node_modules/playwright-core" ]; then
    return 0
  fi
  echo "run.sh: installing the pinned Playwright packages (npm ci)"
  ( cd "$here" && npm ci --no-audit --no-fund ) || die "npm ci failed"
}

ensure_browser() {
  if ( cd "$here" && node -e 'const { chromium } = require("playwright"); const fs = require("node:fs"); process.exit(fs.existsSync(chromium.executablePath()) ? 0 : 1)' 2>/dev/null ); then
    return 0
  fi
  echo "run.sh: installing Chromium into ${PLAYWRIGHT_BROWSERS_PATH:-the Playwright cache} (outside the repository)"
  ( cd "$here" && npx --no-install playwright install chromium ) \
    || die "playwright install chromium failed"
}

plan_and_measure() {
  root=$1
  extra=$2
  results=${SPOT_PERF_OUT:-$here/results/$mode/perf}
  raw=${SPOT_BUDGET_RAW:-$(dirname "$results")/raw}
  mkdir -p "$raw" "$results"
  rm -f "$raw"/run-*.json "$raw"/plan.json
  "$python" "$cli" plan --budgets "$budgets" --mode "$( [ "$mode" = full ] && echo full || echo quick )" \
    --runs "$runs" --out "$raw/plan.json" || die "cannot plan the measurement"
  if [ "$signed" = in ]; then
    # Explicit opt-in the stub requires (it also starts under CI=true).
    export SPOT_BUDGETS_STUB=1
    extra="$extra --signed-in"
  fi
  # shellcheck disable=SC2086  # $extra is flags or empty, never a path with spaces
  node "$here/src/measure.mjs" --plan "$raw/plan.json" --out "$raw" --runs "$runs" \
    --root "$root" $extra || die "the browser driver failed"
  "$python" "$cli" aggregate --budgets "$budgets" --raw "$raw" --out "$results" \
    --runs "$runs" --runner "$SPOT_PERF_RUNNER" --commit "$commit" \
    || die "budget verdict failed (breach, missing result or invalid evidence)"
  echo "run.sh: results in $results (gate: python tools/ci/ui/perf_gate.py --budget $here/perf-budgets.toml --results $results)"
}

self_test() {
  echo "run.sh --self-test: Python unit tests"
  ( cd "$here/../.." && "$python" -m unittest discover -s tools/browser-budgets/tests -v ) \
    || die "the unit tests failed"
  echo "run.sh --self-test: the launch-arguments builder"
  node "$here/tests/measure-launch-args.test.mjs" || die "the launch-arguments builder test failed"
  ensure_browser
  work=${TMPDIR:-/tmp}/spot-budgets-self-test-$$
  mkdir -p "$work/raw" "$work/perf" "$work/breach"
  trap 'rm -rf "$work"' EXIT
  echo "run.sh --self-test: the driver against the fixture page"
  "$python" "$cli" plan --budgets "$here/tests/fixtures/fixture-budgets.toml" --mode quick \
    --runs 1 --out "$work/raw/plan.json" || die "cannot plan the fixture run"
  node "$here/src/measure.mjs" --plan "$work/raw/plan.json" --out "$work/raw" --runs 1 \
    --fixture --root "$here/fixture" || die "the driver failed against the fixture"
  echo "run.sh --self-test: the driver against the fixture page and the stub backend (--signed-in)"
  SPOT_BUDGETS_STUB=1 node "$here/src/measure.mjs" --plan "$work/raw/plan.json" --out "$work/raw-stub" --runs 1 \
    --fixture --signed-in --root "$here/fixture" || die "the driver failed against the fixture with the stub backend"
  "$python" -c 'import json, sys; r = json.load(open(sys.argv[1])); sys.exit(0 if not r["errors"] and not r.get("error") and len(r["measures"]) == 7 else 1)' \
    "$work/raw-stub/run-1.json" || die "the signed-in fixture run did not produce all seven results"
  echo "run.sh --self-test: aggregation accepts a good result set"
  "$python" "$cli" aggregate --budgets "$here/tests/fixtures/fixture-budgets.toml" \
    --raw "$work/raw" --out "$work/perf" --runs 1 --runner fixture --commit local \
    || die "aggregation rejected its own valid fixture results"
  "$python" "$cli" check --budgets "$here/tests/fixtures/fixture-budgets.toml" \
    --results "$work/perf" --runner fixture --commit local \
    || die "the result files failed the schema check"
  echo "run.sh --self-test: aggregation rejects a breach"
  # Same samples, every maximum knocked down to 0.001: any real measurement trips it.
  sed 's/^max = .*/max = 0.001/' "$here/tests/fixtures/fixture-budgets.toml" >"$work/breach-budgets.toml"
  if "$python" "$cli" aggregate --budgets "$work/breach-budgets.toml" \
      --raw "$work/raw" --out "$work/breach" --runs 1 --runner fixture --commit local >"$work/breach.log" 2>&1; then
    die "a breach was accepted: the verdict is broken"
  fi
  grep -q "breach" "$work/breach.log" || die "the breach exit did not report a breach"
  echo "run.sh --self-test: PASSED"
}

ensure_node_modules

if [ "$mode" = self-test ]; then
  self_test
  exit 0
fi

ensure_browser
publish=${SPOT_BROWSER_PUBLISH:-$here/../../artifacts/browser-publish/wwwroot}
if [ -f "$publish/index.html" ]; then
  echo "run.sh: serving the published build at $publish"
  plan_and_measure "$publish" ""
  exit 0
fi

echo "run.sh: no published browser build at $publish" >&2
echo "run.sh: build it with 'dotnet publish clients/avalonia/src/Spotledger.Client.Browser -c Release -o artifacts/browser-publish'" >&2
if [ -n "${CI:-}" ] && [ "${SPOT_BUDGETS_ALLOW_FIXTURE:-0}" != 1 ]; then
  die "refusing to measure the fixture page in CI; publish the browser build first"
fi
echo "run.sh: falling back to the fixture page: THESE NUMBERS ARE NOT EVIDENCE (results say \"fixture\": true)" >&2
plan_and_measure "$here/fixture" "--fixture"
