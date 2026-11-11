#!/bin/sh
set -eu

if [ -z "${SPOT_HYGIENE_INDEX:-}" ]; then
    echo "no reference index configured"
    exit 0
fi

root=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
python3 "$root/tools/lints/hygiene/hygiene.py" \
    --index "$SPOT_HYGIENE_INDEX" \
    --target "$root" \
    --include 'crates/**' \
    --include 'clients/**' \
    --include 'scenarios/**' \
    --include 'harness/**' \
    --include 'tools/**' \
    --exclude 'tools/lints/hygiene/tests/corpus/**' \
    --exclude 'crates/framework/spot-journal-compat/fixtures/**' \
    --allow "$root/tools/lints/hygiene/allow.toml"
