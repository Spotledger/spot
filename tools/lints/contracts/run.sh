#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
if [[ -n "${GITHUB_BASE_REF:-}" ]]; then
  git -C "$repo_root" fetch --no-tags origin "$GITHUB_BASE_REF"
fi
exec python3 "$repo_root/tools/lints/contracts/contract_lint.py" "$repo_root"
