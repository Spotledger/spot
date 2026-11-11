#!/usr/bin/env bash
# Browser visual regression (WP-UI-16.T2).
#
#   run.sh             capture the published browser head and compare with baselines/browser/
#   run.sh --update    capture and write those captures as the baselines (review every PNG first)
#   run.sh --self-test the Python tests, then a capture against whatever publish exists (skipped
#                      with a message when there is none — the comparator tests still run)
#
# A missing baseline, a mismatch or an unusable publish exits non-zero. Playwright is the version
# WP-UI-09.T3 pins; nothing new is installed here beyond that version and its Chromium.
#
# Environment:
#   SPOT_BROWSER_PUBLISH      web root of the published build (default artifacts/browser-publish/wwwroot)
#   SPOT_BROWSER_VISUAL_OUT   results root (default tools/browser-visual/results)
#   SPOT_PLAYWRIGHT_HOME      directory whose node_modules holds playwright
#   PLAYWRIGHT_BROWSERS_PATH  browser cache outside the repository (Playwright's default cache otherwise)
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
mode=check
for argument in "$@"; do
  case "$argument" in
    --update) mode=update ;;
    --self-test) mode=self-test ;;
    --check) mode=check ;;
    -h|--help) sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "run.sh: unknown argument: $argument" >&2; exit 2 ;;
  esac
done

die() { echo "run.sh: $*" >&2; exit 1; }
python=${PYTHON:-python3}
command -v "$python" >/dev/null || die "python3 is required"
command -v node >/dev/null || die "node is required (>= 20)"

publish=${SPOT_BROWSER_PUBLISH:-$here/../../artifacts/browser-publish/wwwroot}
results=${SPOT_BROWSER_VISUAL_OUT:-$here/results}
captures=$results/captures
baselines=$here/baselines/browser
budgets=$here/../browser-budgets

export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=${PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD:-1}

playwright_dir() {
  if [ -n "${SPOT_PLAYWRIGHT_HOME:-}" ] && [ -d "$SPOT_PLAYWRIGHT_HOME/node_modules/playwright" ]; then
    echo "$SPOT_PLAYWRIGHT_HOME"; return 0
  fi
  if [ -d "$budgets/node_modules/playwright" ]; then echo "$budgets"; return 0; fi
  if [ -d "$here/node_modules/playwright" ]; then echo "$here"; return 0; fi
  return 1
}

ensure_playwright() {
  if playwright_dir >/dev/null; then return 0; fi
  echo "run.sh: installing the pinned Playwright packages into tools/browser-budgets (npm ci)"
  ( cd "$budgets" && npm ci --no-audit --no-fund ) || die "npm ci failed in tools/browser-budgets"
  playwright_dir >/dev/null || die "playwright is still missing after npm ci"
}

ensure_chromium() {
  local where
  where=$(playwright_dir)
  if ( cd "$where" && node -e 'const { chromium } = require("playwright"); const fs = require("node:fs"); process.exit(fs.existsSync(chromium.executablePath()) ? 0 : 1)' 2>/dev/null ); then
    return 0
  fi
  echo "run.sh: installing Chromium into ${PLAYWRIGHT_BROWSERS_PATH:-the Playwright cache} (outside the repository)"
  ( cd "$where" && npx --no-install playwright install chromium ) || die "playwright install chromium failed"
}

capture() {
  rm -rf "$captures"
  mkdir -p "$captures"
  node "$here/src/capture.mjs" --root "$publish" --out "$captures"
}

compare() {
  "$python" "$here/src/compare.py" --captures "$captures" --baselines "$baselines" --out "$results" "$@"
}

self_test() {
  echo "run.sh --self-test: Python tests (no browser)"
  ( cd "$here/../.." && "$python" -m unittest discover -s tools/browser-visual/tests -v ) || die "the unit tests failed"
  if [ ! -f "$publish/index.html" ]; then
    echo "run.sh --self-test: no published browser build at $publish — the capture step is skipped"
    echo "run.sh --self-test: publish it with 'dotnet publish clients/avalonia/src/Spotledger.Client.Browser -c Release -p:SpotBrowserAot=false -o artifacts/browser-publish'"
    return 0
  fi
  ensure_playwright
  ensure_chromium
  echo "run.sh --self-test: capturing against $publish (compared with the committed baselines)"
  capture
  compare
}

if [ "$mode" = self-test ]; then
  self_test
  exit 0
fi

[ -f "$publish/index.html" ] || die "no published browser build at $publish; build it with 'dotnet publish clients/avalonia/src/Spotledger.Client.Browser -c Release -p:SpotBrowserAot=false -o artifacts/browser-publish'"
ensure_playwright
ensure_chromium
mkdir -p "$results"
capture

if [ "$mode" = update ]; then
  echo "run.sh: writing baselines into $baselines — review every PNG before committing"
  compare --update
  echo "run.sh: baselines updated; rerun without --update to prove the comparison"
  exit 0
fi

compare || die "the browser visual comparison failed; review $results/review"
echo "run.sh: browser visual regression passed"
