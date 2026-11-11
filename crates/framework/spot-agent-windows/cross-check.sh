#!/usr/bin/env bash
# Type-checks and lints the `#[cfg(windows)]` code of spot-agent-windows and
# spot-agent-windows-sys from a Linux host (WP-ADM-05.T5), for x86_64-pc-windows-gnu.
#
# This proves the Win32/COM bindings compile and pass clippy. It does not run them: the
# behaviour of the real modules is checked only on a Windows runner.
#
# Needs: `rustup target add x86_64-pc-windows-gnu`. `ring` (through spot-postgres, reached
# by spot-agent-windows-sys) has a build script that wants a C compiler for the target.
# When `x86_64-w64-mingw32-gcc` (package gcc-mingw-w64-x86-64) is installed it is used;
# otherwise this script puts check-only stand-ins on PATH. They create empty objects, which
# is enough for `cargo check` and `cargo clippy` (nothing is linked) and useless for a build.
set -euo pipefail

target=x86_64-pc-windows-gnu
if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
  stubs="$(mktemp -d)"
  trap 'rm -rf "$stubs"' EXIT
  cat >"$stubs/x86_64-w64-mingw32-gcc" <<'STUB'
#!/bin/bash
out=""; prev=""
for a in "$@"; do if [ "$prev" = "-o" ]; then out="$a"; fi; prev="$a"; done
if [ -n "$out" ]; then : >"$out"; fi
exit 0
STUB
  cat >"$stubs/x86_64-w64-mingw32-ar" <<'STUB'
#!/bin/bash
shift
: >"$1"
exit 0
STUB
  cp "$stubs/x86_64-w64-mingw32-ar" "$stubs/x86_64-w64-mingw32-ranlib"
  chmod +x "$stubs"/*
  export PATH="$stubs:$PATH"
fi

cargo clippy --target "$target" -p spot-agent-windows -p spot-agent-windows-sys \
  --all-targets -- -D warnings
