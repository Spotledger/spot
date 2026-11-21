# The conformance corpus

The executable form of `docs/expression-language.md`. Every implementation (Rust `spot-expr`,
the .NET one of WP-UI-11.T2) must give the same result on every case. A case changes only after
the specification changes first.

## Files

One JSON file per topic, each a list of cases: `literals`, `arithmetic` (with decimal rounding),
`comparison` (with null), `logic`, `precedence`, `text`, `dates`, `lists`, `fields`,
`functions`, `errors` (syntax and names) and `limits` (node count and nesting).

## A case

```json
{
  "id": "arithmetic-014",
  "expr": "1.25 * 1.25",
  "context": { "doc.qty": {"int": "3"} },
  "now": "2026-10-05T10:20:30Z",
  "expect": { "ok": {"decimal": "1.56"} },
  "note": "optional, for people"
}
```

- `id`: unique across the corpus, stable.
- `expr`: the source text, exactly as given to the parser (it may contain line feeds and
  non-ASCII characters).
- `context`: a flat map from dotted path (`doc.qty`, `parent.status`, `doc.customer.name`) to a
  typed value. A path not in the map reads as `null`. Always present.
- `now`: the instant `now()` returns, `YYYY-MM-DDTHH:MM:SS[.f]Z`. Always present.
- `expect`: either `{"ok": <typed value>}` or `{"error": "<code>"}`. A parse error may add
  `"offset"`, the 0-based character offset; when present it must match. Evaluation errors have
  no offset. A case whose expression fails to parse and one that fails to evaluate are both
  `error` cases: the implementation reports the code of whichever stage fails.

## Typed values

No implementation may rely on JSON number semantics, so every value is an object with one key:

| JSON | Meaning |
| --- | --- |
| `{"null": true}` | null |
| `{"bool": true}` | boolean |
| `{"int": "42"}` | 64-bit integer, as a string |
| `{"decimal": "12.50"}` | decimal, as a string; **the scale counts**: `12.50` and `12.5` are different results |
| `{"text": "abc"}` | text |
| `{"date": "2026-10-05"}` | date |
| `{"datetime": "2026-10-05T10:20:30Z"}` | instant in UTC; canonical form (fraction only when non-zero, then six digits) |
| `{"list": [ ... ]}` | list of typed values |

Results are compared by type, value and, for decimals, scale. Decimal expectations in
`arithmetic.json` and `functions.json` were also checked against an independent oracle
(Python's `decimal` module with half-even rounding).
