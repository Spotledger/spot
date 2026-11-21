# spot-spatial-text

The one text form of `geo` points and `range` values in CSV and NDJSON cells, shared by
`spot-import` (cell to value) and `spot-bulk-data` (value to cell).

| Value | Text | Example |
| --- | --- | --- |
| Geo point | `POINT(<lon> <lat>)`, longitude first, at most 7 decimals | `POINT(67.0011 24.8607)` |
| Integer range | `[<start>,<end>)` | `[3,10)` |
| Decimal range | same, decimals as `Decimal::to_string` | `[1.50,2.25)` |
| Date range | `YYYY-MM-DD` bounds | `[2026-01-01,2026-02-01)` |
| Instant range | RFC 3339 UTC, six fractional digits, `Z` | `[2026-01-01T08:00:00.000000Z,2026-01-01T09:00:00.000000Z)` |

A point prints the digits the decimal holds after `normalize` (no padding to 7). Parsing takes
the keyword in any case and spaces around the parentheses (`point (1 2)`), a dot as decimal
separator, and tolerates spaces around the whole cell. Ranges are always half-open; spaces
inside a bound are not allowed. A bound never contains a comma or a closing bracket, so no
quoting is needed inside the cell (CSV quoting of the whole cell is the writer's job).

## Refused cells

| Cell | Reason |
| --- | --- |
| `POINT(181 0)`, `POINT(0 91)` | `OutOfRange` |
| `POINT(1.12345678 2)` | `TooManyDecimals` |
| `POINT(1)`, `POINT(1 2 3)`, `POINT(1e3 2)`, `POINT(1,5 2)` | `Syntax` |
| `[3,3)`, `[4,3)` | `Empty` |
| `[1,3]`, `(1,3)` | `NotHalfOpen` |
| `[1,)` | `Unbounded` |
| `[a,b)` for an integer range | `Syntax` |
| `[2026-01-01,2026-02-01)` for an instant range, `[1,2026-01-01)` for an integer range | `KindMismatch` |

An error message repeats at most the first 40 characters of the cell. There is no `f32` or
`f64` in the crate; a test reads the sources and fails if one appears.
