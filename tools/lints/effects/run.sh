#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
exec python3 "$repo_root/tools/lints/effects/check.py" "$repo_root/crates" "$repo_root/apps"
