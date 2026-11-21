#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
exec python3 "$repo_root/tools/lints/features/features_lint.py" "$repo_root"
