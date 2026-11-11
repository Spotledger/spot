#!/usr/bin/env bash
# Gate: native API contract checks (WP-CON-03, ADR-055). Runs check.py and its self-test,
# which breaks the spec once per rule and fails unless every break is detected.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
contract="$repo_root/contracts/native-api"
venv="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/spot-native-api-venv"
if [[ ! -x "$venv/bin/python" ]]; then
  python3 -m venv "$venv"
fi
"$venv/bin/python" -m pip install --quiet --disable-pip-version-check -r "$contract/requirements.txt"
"$venv/bin/python" "$contract/check.py"
"$venv/bin/python" "$contract/check.py" --self-test
"$venv/bin/python" "$repo_root/tools/lints/native-api/flow_scalars.py" "$contract/openapi.yaml"
