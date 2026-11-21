# Screens manifest

One `*.toml` file per module (or layer) lists the screens that **approved designs
require**. `tools/ui-coverage/check.py` compares the manifest with the screens the
Avalonia code implements and the UI tests that cover them. The manifest is the
contract with the designs: add a screen here when its design is approved, never
before. Format: `../screens.schema.md`. `example.toml` is ignored by the checker.

## Marking code and tests

```csharp
using Spotledger.Client.Analytics;

[ScreenId("sales.invoice.form")]            // on the view model (or view) that is the screen
public sealed class InvoiceFormViewModel { }

[CoversScreen("sales.invoice.form", "empty", "error")]   // on a headless UI test class or method
public void Shows_server_error_on_field() { }
```

A `[CoversScreen]` with no states covers the `default` state. A screen counts as
"covered for required states" when the states named across its tests include every
entry of `required_states`.

## Running

```bash
python tools/ui-coverage/check.py                                 # report
python tools/ui-coverage/check.py --min-implemented 80 --min-covered 60
python tools/ui-coverage/check.py --regressions-only              # CI: new gaps fail, old ones do not
python tools/ui-coverage/check.py --write-baseline                # record today's gaps as baseline.json
```

`baseline.json` (next to the manifests) holds the known gaps. The reflection exporter
(`ScreenCoverageExporter` in `Spotledger.Client.Analytics.Tests`) writes
`screens-found.json` when `SPOT_SCREENS_FOUND` names a path; pass it with `--found`.
