# TypeScript surface generator

`generate.py` turns `contracts/native-api/openapi.yaml` into a typed TypeScript
surface: types, an operation table and a transport-injected client. It is the
third contract generator beside progenitor (Rust) and Kiota (C#), task C4 of
ADR-092 section 10.2, with **no third-party generator**: Python 3.11+, the
standard library and PyYAML (pinned in `../requirements.txt`). It needs no
network and no Node. **The output is never committed**; the script and its tests
are.

```
python3 tools/contract-clients/typescript/generate.py --out DIR     # write the files
python3 tools/contract-clients/typescript/generate.py --check       # generate, self-test, write nothing else
python3 -m unittest discover -s tools/contract-clients/typescript/tests -v
```

`--contract PATH` picks another contract (its `VERSION` file must sit beside
it). `--check` exits non-zero on any finding, any `UNREPRESENTABLE` schema and
any exactness failure. If `tsc` is on `PATH` (or `SPOT_TSC` names a command,
for example `node /path/to/typescript/bin/tsc`) the output is compiled with
`--strict --noEmit --target ES2022 --lib ES2022 --skipLibCheck false`; without
it the check prints `tsc not available: type check skipped` and passes, unless
`SPOT_REQUIRE_TSC=1` makes the absence a failure.

## What it writes

| File | What it is |
| --- | --- |
| `types.ts` | One exported `interface` or `type` per component schema, names as in the contract. |
| `operations.ts` | `operations`, a frozen table (method, path, path/query/header parameters, request, success and error entries, `idempotent`, `etag`); `OperationId`; and the `Operations` interface that gives every operation its argument type and its discriminated result type. |
| `client.ts` | `Transport`, `HttpRequest`, `HttpResponse`, `FormParts` and `NativeApi`, one method per operation. |
| `contract.ts` | `CONTRACT_VERSION` (the contract's `VERSION` file) and `CONTRACT_SHA256` (of `openapi.yaml`). |
| `index.ts` | Re-exports of the four files above. |
| `report.txt` | Counts, every approximate translation, every drop, every refusal. Also printed to stderr. |

Every file starts with a header naming the contract version and "Generated, do
not edit". Output is deterministic: running twice gives byte-identical files
(contract order or sorted keys everywhere, no timestamps).

## The client

```ts
const api = new NativeApi(transport, "https://host");
const r = await api.document_get({ path: { doctype: "invoice", id } });
if (r.unexpected) { /* a status the contract does not declare: r.problem is unknown or the default body */ }
else if (r.status === 200) { r.body; r.headers["ETag"]; }
else { r.problem; }   // a declared error status
```

- One method per operation, one typed argument object `{ path, query, headers, body }` holding only the
  members that operation has. A required path parameter is a required member, so an unfilled one is a
  compile-time error. Path parameters are percent-encoded.
- The result is a union on `status`. Success statuses (below 400) carry `body`, declared error statuses
  carry `problem`, and every result carries the response `headers`. A status the contract does not
  declare (including a `default` response) is the member `{ status: number; unexpected: true; problem }`.
  An HTTP status never throws; a malformed JSON body on a declared success does.
- The client calls the `Transport` it is given and **never `fetch`** (ADR-088): the shell injects the
  Rust core's transport, a test injects a fake.

## Type mapping

| Contract | TypeScript |
| --- | --- |
| object with properties | `interface` (named) or `{ ... }` (inline); members not in `required` are optional |
| `additionalProperties: false` | no index signature (closed) ; an object with no properties is `{ [key: string]: never }` |
| `additionalProperties: <schema>` | `{ [key: string]: <schema> }` (intersected with the named members when there are any) |
| `enum` | union of literals |
| `const` | literal type |
| `type: [x, "null"]`, `nullable: true` | `x \| null` |
| `type: [a, b]` | `a \| b` |
| `oneOf`, `anyOf` | union (`anyOf` is listed as approximate) |
| `allOf` | intersection |
| `string` with `format: date-time`, `date`, `uuid`, `decimal`... | `string` (the core owns parsing; the surface is honest about the wire) |
| `string` with `format: binary` | `Uint8Array` |
| `integer`, `number` | `number` (see the 53-bit rule) |
| `array` | `T[]` |
| `$ref` | the named type |
| `not`, `propertyNames` | no TypeScript form: a comment plus the nearest honest type, listed in the report |
| no schema | `unknown` |

Request and response bodies: JSON is typed from its schema; `application/octet-stream` and other
binary types are `Uint8Array`; `text/*`, NDJSON, XML and event streams are `string`;
`multipart/form-data` takes `FormParts`; `application/x-www-form-urlencoded` takes the schema's object.
When a response or request lists several media types, JSON is preferred, then
`application/octet-stream`, then the first listed; the others are printed as `DROPPED` in the report.

## The 53-bit rule

Money, quantities and ids never become a lossy JavaScript number silently (ADR-12). A schema with
`format: int64` or `uint64`, or an integer bound beyond 2^53, is refused: the report prints
`UNREPRESENTABLE <schema path>` and `--check` fails. The generator does not guess `string`; the contract
author decides (decimals are already strings, `Decimal` in the contract).

## Safety of the generated text

All contract-provided text goes through `tsescape.py`: keys are quoted unless a plain identifier
(`__proto__` is always quoted), string literals are JSON-escaped, doc comments neutralise `*/`, and
operation ids must be plain identifiers (they become method names). The script never `eval`s and shells
out only to the optional `tsc`.

## What is deliberately not generated

No runtime validation (the core validates), no `fetch`, no React hooks, no caching or retries: the
core owns behaviour (ADR-088). The surface only says what is on the wire.

## How the shells use it (ADR-092, U1)

The React web shell and the React Native shell run the generator at build time into their own build
directory, import `NativeApi` and the types, and give `NativeApi` a `Transport` that forwards to
`spot-client-core`. A contract change shows up as a TypeScript compile error in the shell, in the same
way it shows up as a compile error in the Rust and C# clients.

## Files

`generate.py` (CLI), `tsschema.py` (JSON Schema to type text), `tsmodel.py` (operations),
`tsemit.py` and `tsclient.py` (emitters), `tsescape.py` (escaping), `tsreport.py`, `selftest.py` (the
exactness self-test), `tests/` (fixtures, goldens, unit tests; regenerate goldens with
`SPOT_UPDATE_GOLDEN=1` and read the diff).
