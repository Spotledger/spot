# Contract clients

Generated API clients for the native API contract
(`contracts/native-api/openapi.yaml`, OpenAPI 3.1, ADR-055). Generation is
part of the build and never committed; this directory commits the generator
wrappers and the gate that runs them.

| Path | What it is |
| --- | --- |
| `convert.py` | OpenAPI 3.1 → 3.0.3 translation for the Rust generator, with deliberate-failure modes |
| `check.sh` | The gate: regenerate both clients into temp copies, build them, check exactness |
| `requirements.txt` | Python dep of `convert.py` (PyYAML, pinned) |
| `rust/` | Rust client crate; `build.rs` generates the client into `OUT_DIR` at build time |
| `csharp/` | .NET class library; a dotnet tool manifest pins Kiota, generation runs at check time |

## Why two toolchains, and why a converter

Both clients are generated from the same contract so they cannot drift apart
from the contract itself (WP-CON-03, WP-CON-05).

- **Rust: `progenitor` 0.15** (MIT OR Apache-2.0), the Dropshop-style code
  generator; runtime `progenitor-client` 0.15, `reqwest` 0.13 (rustls, no
  OpenSSL), `regress` 0.12 for string `pattern` validation. Progenitor reads
  **OpenAPI 3.0 only**, so `convert.py` translates our 3.1 contract into a
  3.0.3 JSON document at build time. The conversion is mechanical, printed,
  and gated (see "What the converter changes" below).
- **C#: `microsoft.openapi.kiota` 1.35.0** (MIT) reads the 3.1 contract
  directly. Version pinned by `csharp/.config/dotnet-tools.json`
  (`dotnet tool restore`, manifest version 1) plus the runtime
  `Microsoft.Kiota.Bundle` 2.1.2 pinned in `NativeApi.Client.csproj`.
  Target `net10.0` (SDK 10.0.40i), warnings as errors.
- **`convert.py`**: plain Python with PyYAML 6.0.3 only.

Generated artifacts and their locations:

| Artifact | Where it ends up | Committed? |
| --- | --- | --- |
| `openapi-3.0.json` | `<tmp copy of rust>/spec/` | no (`.gitignore`) |
| `rust` client code | `$OUT_DIR/codegen.rs`, compiled into the crate | no |
| `csharp` client code | `<temp copy>/csharp/Generated/` | no (`.gitignore`) |

The committed `rust/Cargo.lock` pins every transitive dependency of the
build-dep (progenitor, openapiv3) and runtime set; `cargo build` in
`check.sh` runs `--locked`.
## What the converter changes

`convert.py to-30` prints every translation (`TRANSLATED ...`) and every drop
(`DROPPED ...`) to stderr, and `check.sh` fails if a drop is not visible in
the converted document. The constructs the contract uses:

| 3.1 construct | 3.0 translation | Where it comes from |
| --- | --- | --- |
| `openapi: 3.1.x` | `openapi: 3.0.3` | version string |
| `jsonSchemaDialec`, `webhooks` | dropped | doc-level 3.1 keys |
| `const: "x"` | `enum: ["x"]` | `Document_state` and friends |
| `type: [string, "null"]` (arrays) | `type: string` + `nullable: true` (single non-null type kept) | `FieldValue`, optional properties |
| siblings of `$ref` (`description`, `pattern`…) | inlined into the target schema, description moved to doc comment | path parameters, `FieldMeta.default`, `PullBatch.next_position` |
| `pattern` on string schemas | kept; typify compiles them with `regress` | id/uuid/date/decimal types |
| response `default` (Error body) | copied to ranges `4XX` and `5XX` — every operation has exactly one success body and one error body type | every operation |
| a second request-body media type | dropped; the first one progenitor can type is kept | the file upload's two encodings |

Three kinds of drop, all logged (`DROPPED ...`) and, where `check.sh` asserts
them, visible in the converted document:

1. An explicit error response whose body is not the error envelope:
   `/health/ready`'s `503` (a `Ready` body) and `/api/v1/files/{id}`'s `416`
   (no body at all). Progenitor allows exactly one error body type per
   operation. The drop is loud and `check.sh` asserts the `/health/ready` one.
   The C# client, which reads the 3.1 contract, keeps the full behaviour; the
   Rust client reports those statuses through its folded `5XX`/`4XX` error arm
   instead (the body is not the error envelope, so callers still see an error
   rather than a parsed result).
2. A request body with more than one media type. Progenitor types exactly one
   request body and has no multipart support, so `/api/v1/files`'s
   `multipart/form-data` encoding is dropped and the generated Rust client
   carries `application/octet-stream` only. The C# client keeps both
   encodings. (`convert.py` leaves a body whose *only* media type it cannot
   type alone, so progenitor reports it instead of the operation silently
   losing its parameter.)
3. Media Type Object `examples` under non-schema keys — checked, not
   translated; nothing in this contract uses them.

Unchanged on purpose: `101` (realtime) and `304` responses stay exact
statuses (neither is 4xx/5xx, so the fold ignores them).

## Exactness gates in `check.sh`

- build.rs asserts the parsed contract's `Decimal` schema is
  `Type(String)` before generating (fail text: "must keep the
  Decimal schema as a string (ADR-12)").
- The generated Rust client must contain the string-typed `Identifier` and
  `Date` structs, the ranged error arms `400u16 ..= 499u16` /
  `500u16 ..= 599u16`, the `Client` type, and **no** `f32`/`f64`.
- The generated C# models must deserialize the typical string-with-pattern
  property (`FilterMember6.Field`) with `GetStringValue()` and **no**
  model anywhere may call `GetDoubleValue`/`GetSingleValue`/
  `GetDecimalValue` (kiota's path-indexer `double position` in request
  builders is not a model and is not affected).

Both gates are self-tested: `check.sh --self-test` breaks the contract at
exactly two points and expects each gate to catch its break:

1. `convert.py break-decimal` sets `Decimal` to `type: number` in a mutated
   copy of the 3.1 contract, which the script then converts to 3.0 like
   every real run; the Rust build must fail with the build.rs assert.
2. `convert.py break-identifier` sets `Identifier` to `type: number`, the
   closest referenced analogue (Decimal itself is referenced nowhere in the
   contract); kiota must generate `double? Field` where `string? Field`
   was, which is what the C# numeric gate greps for.

## Run

```bash
tools/contract-clients/check.sh            # regenerate + build + exactness
tools/contract-clients/check.sh --self-test  # also prove the gates catch breaks
python3 tools/contract-clients/test_convert.py  # convert.py's $ref unit tests only
```

`check.sh` runs `test_convert.py` first, so a regression in the `$ref`
resolution or the sibling inlining fails before the generators run.

Prereqs on PATH: `python3`, `cargo` (1.9x), `dotnet` (SDK 10). PyYAML is
installed into a venv next to the lints (`$RUNNER_TEMP/spot-contract-clients-venv`),
as in `tools/lints/native-api/run.sh`. Proven versions: cargo/rust
1.98.1, progenitor 0.15.0 (client 0.15.0, reqwest 0.13.x with the `rustls`
feature, regress 0.12.0), kiota 1.35.0, `Microsoft.Kiota.Bundle` 2.1.2,
PyYAML 6.0.3, .NET SDK 10.0.401.

## Known limitations

- A contract change that adds 3.1-only output constructs fails `check.sh`
  until `convert.py` and its drops are extended — by design, fail-loud.
- Rust crate warnings are suppressed crate-wide (`#![allow(warnings)]`)
  because generated code is not maintainable surface; the C# side keeps
  warnings as errors (kiota output is warning-clean).
- The rust client returns `reqwest::Upgraded` for the realtime `101`; the
  WebSocket framing is implemented in-app, not in the stub.
- The generated rust client uploads files with `application/octet-stream`
  only: progenitor types one request body and cannot express
  `multipart/form-data`. The C# client keeps both encodings.
