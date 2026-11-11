# stub-backend

A loopback stub of the native API (`contracts/native-api/openapi.yaml`, 0.9.0) for the browser budgets
(WP-UI-09.T5). The `browser` workflow has no server, and three budgets need a signed-in shell
(`browser.form.open`, `browser.list.scroll.frame`, `browser.child-edit.key`). `measure.mjs --signed-in`
starts this stub, points the real shell at it and signs in through the shell's own screens.

## Test credential (a test value of this stub, not a secret)

| | |
| --- | --- |
| user | `budgets@stub.test` |
| password | `stub-only-not-a-secret` |

It exists only in this folder and in the driver, is accepted by no real server, and is never put in the
build. The stub refuses to start unless `CI=true` or `SPOT_BUDGETS_STUB=1`; `run.sh` sets the latter only
for `--signed-in`. It binds `127.0.0.1` only, sets no cookie (the session is a bearer token in a header)
and allows CORS for the page origin it was started with, never `*`.

## What it serves

`GET /api/health/live`, `GET /api/health/ready`, `POST /api/v1/auth/login`, `POST /api/v1/auth/logout`,
`GET /api/v1/auth/session`, `GET /api/v1/meta/{doctype}` (with `ETag` and `304`),
`GET /api/v1/resource/sales_invoice` (keyset paging), `POST .../query` (limit, cursor and `with_total`
only; filter and order are ignored, the order is fixed) and `GET .../{id}`. One DocType
(`sales_invoice`, label "Sales Invoice", child `sales_invoice_item`), 2,000 list rows, and one document
with a child table of 20 rows. Every amount and quantity is a decimal string. `GET /__stub/state` is a
test-only counter endpoint for the driver and the tests, not part of the contract.

## Data

`data/*.json` are written by `generate_data.py` (deterministic, one compact line per file) and checked
against it by `tests/test_stub_contract.py`. Edit the script, run it, commit both.

```bash
CI=true node tools/browser-budgets/stub-backend/server.mjs --allow-origin http://127.0.0.1:8080
```
