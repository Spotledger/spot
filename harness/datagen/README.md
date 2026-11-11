# Spotledger synthetic data generator

Deterministic, stdlib-only generator for neutral business datasets: master
data (companies, warehouses, items, customers, suppliers, accounts), opening
balances and a full ledger (journal entries, stock movements, invoices,
returns, payments) for one or more companies over a period. It exists so
scale tests (blueprint §3 L9) and later scenario/invariant suites can start
from the same bytes on every machine.

Python 3.12, standard library only. No third-party packages.

## Usage

```console
$ python harness/datagen/datagen.py --profile tiny --seed 42 --out /tmp/demo
$ python harness/datagen/datagen.py --profile tiny --seed 42 --out /tmp/demo --format csv
$ python harness/datagen/datagen.py --verify /tmp/demo
$ python harness/datagen/datagen.py --list-profiles
```

`--profile NAME` accepts a shipped profile name (`tiny`, `small-trader`,
`mid-distributor`, `large`) or the path of any profile TOML file.
`--format` picks the storage format (`jsonl`, the default, or `csv`).
`--out DIR` must be absent or empty; the generator refuses to overwrite an
existing dataset. `--verify DIR` re-hashes every file against
`manifest.json`, then replays the whole ledger in stock order: referential
integrity, invoice line sums, payment allocation coverage, stock never going
negative, company boundaries, balanced journals and date windows. It reports
every violation on stderr and exits non-zero if any was found.

## Profiles

A profile is a TOML file (`profiles/*.toml`) with a header comment naming its
intended use. Every value is an integer; there are no floats anywhere.

| table | fields |
| --- | --- |
| `[period]` | `start`, `end` (ISO dates, `start <= end`) |
| `[counts]` | `companies`, `warehouses_per_company`, `items`, `customers`, `suppliers`, `sales_invoices`, `purchase_invoices`, `payments`, `journal_entries`, `stock_transfers`, `stock_adjustments` (integer counts ≥ 0; master counts ≥ 1) |
| `[distributions]` | `item_popularity_skew`, `customer_activity_skew` (exponents ≤ 8), `paid_share_per_mille` (≤ 1000; `payments` must equal sales×share + purchase×share, each payment fully pays one invoice), `sales_return_per_mille`, `purchase_return_per_mille`, `opening_stock_coverage_per_mille`, `opening_stock_min_milli` ≤ `opening_stock_max_milli`, `opening_receivable_per_mille`, `opening_payable_per_mille` (all per-mille, ≤ 1000), `monthly_seasonality` (12 non-negative integers, at least one positive) |
| `[distributions.lines_per_invoice]` | `min` ≤ `peak` ≤ `max` (≥ 1) |
| `[distributions.payment_delay]` | `min_days` ≤ `max_days` (≤ 365) |

## Determinism rules

- One RNG stream per concern: the seed and the stream name are hashed with
  SHA-256 (`sha256(f"{seed}:{stream}")`) to build each stream seed. Master
  data streams do not depend on transaction counts, so changing one count
  never changes master data, and adding journal entries leaves invoices
  byte-identical.
- Sampling uses only integer methods: `randrange`, `choices` with
  `cum_weights`, integer Zipf weights built in `sampling.py`. No `random()`,
  no floats, no `pow`/`log`.
- Every JSONL row is written with `json.dumps(..., sort_keys=True,
  separators=(",", ":"))` and newline endings; sets are never iterated; CSV
  rows follow RFC 4180. Money and quantities are fixed-scale decimal strings
  (`"123.45"` paisa, `"7.500"` milli-units); line amounts are
  `quantity × rate` rounded half-even and an invoice total is the exact sum
  of its line amounts.
- Names are built from a fixed syllable table; there are no real people or
  company names anywhere in the output.

## Output

23 tables in two shapes. In `jsonl` mode there is one `.jsonl` file per
parent table; rows with children carry the child rows inline under
`lines`/`allocations`. In `csv` mode children expand into their own tables
keyed by the parent id and a 1-based `line` number, so both formats hold
exactly the same rows for the same seed: masters (`companies`, `warehouses`,
`items`, `customers`, `suppliers`, `accounts`), ledger parents
(`opening_stock`, `opening_invoice`, `opening_balance`, `journal_entry`,
`stock_transfer`, `stock_adjustment`, `sales_invoice`, `purchase_invoice`,
`payment`, `sales_return`, `purchase_return`) and children
(`journal_entry_line`, `sales_invoice_line`, `purchase_invoice_line`,
`payment_allocation`, `sales_return_line`, `purchase_return_line`).

`manifest.json` records the generator name and version, the profile name,
the profile file's SHA-256, the seed, the storage `format`, and per file the
row count and SHA-256. Writing streams; memory stays flat with the invoice
count, only master data is held in memory.

Replenishment: when consumption would take an item's stock negative, the
generator first writes an automatic purchase invoice (`is_replenishment =
true`) covering the shortfall plus a top-up, so stock never dips below zero
throughout the replay. Trailing events keep a grace window after the period
end (returns up to the return-delay bound, payments up to the payment-delay
bound); purchase invoices themselves are always dated inside the period.

## SQL staging

`sql/schema.sql` creates the 23 tables as typed columns (BIGINT paisa/milli
units, DECIMAL(12,2)/(18,3) rendered values, booleans, dates) and
`sql/load.sql` `\copy`s every file in foreign-key order. To stage a
dataset:

```console
$ createdb spotledger_datagen
$ psql -d spotledger_datagen -f harness/datagen/sql/schema.sql
$ psql -d spotledger_datagen -f harness/datagen/sql/load.sql   # edit paths per dataset
```

Schema-coherence tests (`tests/test_schema.py`) keep the DDL, the load order
and the writer/verifier schemas in agreement without a live server.

## Measured performance (this machine, seed 42)

| profile | rows | wall time | throughput |
| --- | --- | --- | --- |
| small-trader | ~9k | 0.2 s | ~40k rows/s |
| mid-distributor | 546,626 | 9.3 s | ~58,900 rows/s |
| mid-distributor (csv) | 1,494,266 | 11.1 s | ~134,500 csv rows/s |

Both `mid-distributor` runs passed `--verify` afterwards and cover the
~250,000 invoice mark (exactly 250,000 sales + purchase invoices) required
by the L9 work package.

## Tests

```console
$ python -m unittest discover -s harness/datagen/tests
```

101 tests cover money rounding, name generation, integer sampling, profile
validation (including per-mille ceilings and the payments invariant),
byte-for-byte determinism in both formats, stream independence, manifest
provenance and determinism, JSONL/CSV parity, the verifier's failure classes,
small-trader distribution parameters (replenishment, paid share, trailing
events), streaming memory behaviour, SQL schema coherence and the CLI.