#!/usr/bin/env bash
# Gate for WP-CON-03.T2 (ADR-055): regenerate the Rust and C# contract clients
# from ``contracts/native-api/openapi.yaml`` and check the results for ADR-12
# exactness (money and other pattern-typed strings stay strings; no floats).
#
# Both generators run inside a temporary copy of this directory:
#
#   0. convert.py's own unit tests (test_convert.py) run first: they pin the
#      $ref resolution and the sibling inlining the conversion depends on.
#   1. convert.py to-30 builds the OpenAPI 3.0.3 JSON input for progenitor;
#      the exactness checks below assert the fold (``default`` error response
#      became ``4XX`` + ``5XX``) and assert /health/ready keeps exactly its
#      two Ready responses (its body is loggable detail, not the error type).
#   2. The Rust crate builds against that input (build.rs also asserts the
#      Decimal schema is still a string) and the client is checked at the
#      text level.
#   3. Kiota consumes the OpenAPI 3.1 contract directly; the project is built
#      with warnings as errors and the generated models are checked.
#
# ``check.sh --self-test`` repeats the Rust build with a deliberately broken
# contract (Decimal as ``number``) and the Kiota run with a broken
# Identifier, and fails unless each gate catches its break.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../.." && pwd)"
contract="$repo_root/contracts/native-api/openapi.yaml"
csproj_file="$here/csharp/NativeApi.Client.csproj"
rust_crate_toml="$here/rust/Cargo.toml"
self_test=0
if [[ "${1:-}" == "--self-test" ]]; then
  self_test=1
fi

fail() { echo "check.sh: FAIL $*" >&2; exit 1; }
step() { echo; echo "=== $* ==="; }
assert_grep() { # <file> <fixed string>
  [[ -f $1 ]] || fail "missing file $1 (needed for exactness check '$2')"
  grep -F -q "$2" "$1" || fail "exactness check failed: '$2' not found in $1"
}
expect_fail() { # <what> <expected text> <log> <command...>
  local what="$1" expected="$2" log="$3"; shift 3
  if "$@" >"$log" 2>&1; then
    fail "$what unexpectedly succeeded; see $log"
  fi
  [[ -f $log ]] || fail "$what produced no log at $log"
  if ! grep -q "$expected" "$log"; then
    fail "$what failed, but not for the expected reason ('$expected' missing); see $log"
  fi
  echo "  caught the deliberate break (expected: '$expected')"
}

step "tools"
command -v python3 >/dev/null || fail "python3 is required"
command -v cargo >/dev/null || fail "cargo is required"
command -v dotnet >/dev/null || fail "dotnet is required"
python3 --version

venv="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/spot-contract-clients-venv"
if [[ ! -x "$venv/bin/python" ]]; then
  python3 -m venv "$venv"
fi
"$venv/bin/python" -m pip install --quiet --disable-pip-version-check -r "$here/requirements.txt"
"$venv/bin/python" - <<'EOF'
import yaml
print("PyYAML", yaml.__version__)
EOF

work="$(mktemp -d "${TMPDIR:-/tmp}/spot-contract-clients.XXXXXX")"
trap 'rm -rf "$work"' EXIT
step 'convert: unit tests ($ref resolution and inlining)'
if ! "$venv/bin/python" "$here/test_convert.py" -v >"$work/convert-tests.log" 2>&1; then
  tail -20 "$work/convert-tests.log" >&2
  fail "convert.py unit tests failed; see $work/convert-tests.log"
fi
tail -3 "$work/convert-tests.log"

step "convert: OpenAPI 3.1 -> 3.0.3 (progenitor input)"
spec="$work/spec/openapi-3.0.json"
mkdir -p "$work/spec"
if ! "$venv/bin/python" "$here/convert.py" to-30 "$contract" --output "$spec" \
    2>"$work/503-dropped.log" >"$work/convert.log"; then
  cat "$work/convert.log" "$work/503-dropped.log" >&2
  fail "convert.py to-30 failed"
fi
cat "$work/convert.log"

# The fold must be visible in the converted document: no operation keeps a
# ``default`` response, /health/ready keeps exactly its Ready statuses, and
# Decimal is still a string.
"$venv/bin/python" - "$spec" <<'PYEOF'
import json, sys
doc = json.load(open(sys.argv[1]))
for path, item in doc["paths"].items():
    for method, op in item.items():
        if method in ("get", "put", "post", "delete", "options", "head", "patch", "trace"):
            assert "default" not in op["responses"], f"{path} {method} kept a default response"
ready = doc["paths"]["/api/health/ready"]["get"]["responses"]
assert set(ready) == {"200"}, f"health/ready responses are {sorted(ready)}"
assert doc["components"]["schemas"]["Decimal"]["type"] == "string"
print(f"  {len(doc['paths'])} paths translated; default responses folded into 4XX+5XX; Decimal is a string")
PYEOF
grep -qF "DROPPED paths:/api/health/ready:get: response 503" "$work/503-dropped.log" \
  || fail "the health/ready 503 response must be dropped for progenitor (and logged)"

step "rust: progenitor generates the client (crate name spot-native-api-client)"
cp -a "$here/rust/." "$work/rust/"
mkdir -p "$work/rust/spec"
cp "$spec" "$work/rust/spec/openapi-3.0.json"
grep -F 'progenitor' "$rust_crate_toml"
if ! (cd "$work/rust" && CARGO_TARGET_DIR="$work/rust-target" cargo build --locked) >"$work/rust-build.log" 2>&1; then
  tail -40 "$work/rust-build.log" >&2
  fail "cargo build failed; see $work/rust-build.log"
fi
cargo --version
rustc --version
codegen="$(find "$work/rust-target" -name codegen.rs -print -quit)"
[[ -n "$codegen" ]] || fail "build.rs did not produce codegen.rs"
cp "$codegen" "$work/normal-codegen.rs"
# Exactness: pattern-typed strings stay string types; no floats anywhere; the
# client type and the folded ranged error arms are present.
assert_grep "$work/normal-codegen.rs" 'pub struct Client'
assert_grep "$work/normal-codegen.rs" 'pub struct Identifier (:: std :: string :: String)'
assert_grep "$work/normal-codegen.rs" 'pub struct Date (:: std :: string :: String)'
assert_grep "$work/normal-codegen.rs" '400u16 ..= 499u16'
assert_grep "$work/normal-codegen.rs" '500u16 ..= 599u16'
if grep -qE 'f32|f64' "$work/normal-codegen.rs"; then
  fail "the generated Rust client contains f32/f64 (ADRs 12 and 45)"
fi
echo "  generated $(grep -o 'pub async fn [a-z_]*' "$work/normal-codegen.rs" | wc -l | tr -d ' ') Rust operations; exactness checks passed"

step "csharp: kiota generates from the OpenAPI 3.1 contract"
cp -a "$here/csharp/." "$work/csharp/"
kiota_generate() { # <spec> <log>
  (cd "$work/csharp" && dotnet tool run kiota -- generate --language CSharp \
      --openapi "$1" --output Generated --class-name NativeApiClient \
      --namespace-name Spotledger.NativeApi --clean-output) >"$2" 2>&1
}
(cd "$work/csharp" && dotnet tool restore) >"$work/kiota-restore.log" 2>&1
if ! kiota_generate "$contract" "$work/kiota-generate.log"; then
  tail -30 "$work/kiota-generate.log" >&2
  fail "kiota generate failed; see $work/kiota-generate.log"
fi
dotnet --version
(cd "$work/csharp" && dotnet tool run kiota -- --version) 2>/dev/null | sed 's/^/kiota /'
grep -E 'Microsoft.Kiota.Bundle' "$csproj_file"
if ! (cd "$work/csharp" && dotnet build -warnaserror) >"$work/dotnet-build.log" 2>&1; then
  tail -30 "$work/dotnet-build.log" >&2
  fail "dotnet build failed; see $work/dotnet-build.log"
fi
grep -E 'Build succeeded|[0-9]+ Warning' "$work/dotnet-build.log" | tail -2
# Exactness on the generated models: the typical string-with-pattern property
# still deserializes as a string and no generated model deserializes into a
# numeric CLR type (kiota writes path-indexer doubles in request builders; the
# gate keys on the model deserializers instead).
csharp_generated="$work/csharp/Generated"
f6="$csharp_generated/Models/FilterMember6.cs"
assert_grep "$f6" 'public string? Field { get; set; }'
assert_grep "$f6" 'GetStringValue()'
if grep -rqE 'GetDoubleValue|GetSingleValue|GetDecimalValue' "$csharp_generated"; then
  grep -rlE 'GetDoubleValue|GetSingleValue|GetDecimalValue' "$csharp_generated" | head -5 >&2
  fail "a generated C# model reads a numeric value; string types leaked (ADR-12)"
fi
echo "  generated $(find "$csharp_generated" -name '*.cs' | wc -l | tr -d ' ') C# files; exactness checks passed"

if [[ "$self_test" == 1 ]]; then
  step "self-test rust: break-decimal must be caught by build.rs"
  "$venv/bin/python" "$here/convert.py" break-decimal "$contract" \
    --output "$work/contract-mutant.yaml" 2>/dev/null
  "$venv/bin/python" "$here/convert.py" to-30 "$work/contract-mutant.yaml" \
    --output "$work/spec-mutant.json" 2>/dev/null
  cp -a "$here/rust/." "$work/rust-mutant/"
  mkdir -p "$work/rust-mutant/spec"
  cp "$work/spec-mutant.json" "$work/rust-mutant/spec/openapi-3.0.json"
  expect_fail "mutant rust build" 'must keep the Decimal schema as a string' \
    "$work/rust-mutant.log" \
    bash -c "cd '$work/rust-mutant' && CARGO_TARGET_DIR='$work/rust-mutant-target' cargo build --locked"

  step "self-test csharp: break-identifier must flip the generated type"
  "$venv/bin/python" "$here/convert.py" break-identifier "$contract" --output "$work/mutant-identifier.yaml" 2>/dev/null
  if ! kiota_generate "$work/mutant-identifier.yaml" "$work/kiota-mutant.log"; then
    tail -20 "$work/kiota-mutant.log" >&2
    fail "mutant kiota generation failed outright; see $work/kiota-mutant.log"
  fi
  mutant_f6="$work/csharp/Generated/Models/FilterMember6.cs"
  assert_grep "$mutant_f6" 'public double? Field { get; set; }'
  if grep -q 'public string? Field' "$mutant_f6"; then
    fail "the mutant still generates a string Field; the C# numeric gate would never fire"
  fi
  echo "  kiota turned the broken Identifier into double (GetStringValue -> GetDoubleValue): gate would catch it"
fi

step "result"
if [[ "$self_test" == 1 ]]; then
  echo "check.sh: OK (self-test passed)"
else
  echo "check.sh: OK (run with --self-test to also verify the gates catch deliberate breaks)"
fi