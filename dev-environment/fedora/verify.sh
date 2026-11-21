#!/usr/bin/env bash
# Verify a Fedora 44 Spotledger dev environment (WP-PRD-10.T1): run it inside
# the image built by dev-environment/build.sh, with the repository mounted at
# /work (or on a Fedora host that has the same toolchains).
#
#   podman run --rm -v "$PWD:/work:Z" -w /work -e SPOT_REPO=/work \
#     spotledger-dev:fedora44 bash dev-environment/fedora/verify.sh
#   dev-environment/build.sh --verify        # the above, in one step
#
# It checks the pinned versions and then builds and tests a fast subset: cargo
# fmt over the workspace, one small crate, and the gate-matrix unit tests. It is
# a smoke test of the image, not the full CI run (that is the nightly workflow).
set -euo pipefail

repo="${SPOT_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
cd "$repo"

fail() { echo "verify.sh: FAIL $*" >&2; exit 1; }
ok() { echo "verify.sh: OK $*"; }

expect_version() { # <what> <actual> <expected>
  local what="$1" actual="$2" expected="$3"
  [[ "$actual" == "$expected" ]] || fail "$what is '$actual', expected '$expected'"
  ok "$what $actual"
}

rust_toolchain="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$repo/rust-toolchain.toml")"
dotnet_sdk="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sdk"]["version"])' "$repo/clients/avalonia/global.json")"

# --- the pinned versions the repository asks for ---
command -v rustc >/dev/null || fail "rustc is not on PATH"
expect_version "rustc" "$(rustc --version | awk '{print $2}')" "$rust_toolchain"
expect_version "cargo" "$(cargo --version | awk '{print $2}')" "$rust_toolchain"
rustup component list --installed | grep -qx rustfmt || fail "rustfmt is not installed"
rustup component list --installed | grep -qx clippy || fail "clippy is not installed"
ok "rustfmt and clippy installed"

command -v dotnet >/dev/null || fail "dotnet is not on PATH"
dotnet --list-sdks | grep -q "^$dotnet_sdk " || fail ".NET SDK $dotnet_sdk is not installed"
ok ".NET SDK $dotnet_sdk installed"

command -v python3 >/dev/null || fail "python3 is not on PATH"
python3 - <<'PY' || exit 1
import sys
if sys.version_info[:2] != (3, 12):
    raise SystemExit(f"verify.sh: FAIL python3 is {sys.version.split()[0]}, expected 3.12")
print(f"verify.sh: OK python3 {sys.version.split()[0]}")
PY

command -v psql >/dev/null || fail "psql is not on PATH"
case "$(psql --version)" in
  *" $POSTGRES_VERSION."*|*" $POSTGRES_VERSION "*) ok "psql $(psql --version | awk '{print $3}')" ;;
  *) fail "psql is $(psql --version | awk '{print $3}'), expected major $POSTGRES_VERSION" ;;
esac
[[ -x "${SPOT_PG_BIN:-/usr/pgsql-18/bin}/initdb" ]] || fail "initdb is not at ${SPOT_PG_BIN:-/usr/pgsql-18/bin}"

# --- build and test a fast subset ---
echo "verify.sh: cargo fmt over the workspace"
cargo fmt --all -- --check

echo "verify.sh: cargo check a small crate"
cargo check -p spot-errors

echo "verify.sh: the gate-matrix unit tests"
python3 -m unittest discover -s tools/ci/matrix/tests

if [[ "${SPOT_VERIFY_FULL:-0}" == "1" ]]; then
  echo "verify.sh: SPOT_VERIFY_FULL=1, checking a bigger crate"
  cargo check -p spot-core
fi

ok "Fedora 44 dev environment is ready"
