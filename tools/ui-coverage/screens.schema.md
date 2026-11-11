# Screens manifest schema

Files: `screens/*.toml` (default directory `tools/ui-coverage/screens/`; pass `--screens`
to read another, for example `docs/ui/screens/`). Files whose name starts with `example`
are ignored. Each file holds an array of tables, `[[screen]]`. Unknown keys are errors.

| Key | Type | Required | Meaning |
| --- | --- | --- | --- |
| `id` | string | yes | Stable id, `^[a-z0-9][a-z0-9._-]{0,63}$`, unique across all files. The value used in `[ScreenId]`, `[CoversScreen]` and analytics. |
| `title` | string | yes | Human name. |
| `module` | string | yes | Same token pattern as `id`; the unit of the per-module report. |
| `kind` | string | yes | `form`, `list`, `report`, `dashboard`, `dialog` or `workspace`. |
| `doctype` | string | no | DocType the screen shows, when it is bound to one. |
| `route` | string | no | Route or deep-link pattern. |
| `design_ref` | string | yes | Pointer to the approved design (path with optional anchor). Printed in the missing list. |
| `required_states` | list of string | no, default `[]` | States tests must cover (`default`, `empty`, `error`, `readonly`, `loading`, ...). Names are `^[a-z][a-z0-9_-]{0,31}$`. |
| `priority` | string | yes | `p0`, `p1` or `p2`. |

## Baseline (`baseline.json`)

```json
{"unimplemented": ["id"], "untested": ["id"], "missing_states": {"id": ["empty"]}}
```

With `--regressions-only` the check fails only on gaps not in the baseline. Gaps in the
baseline that are later closed do not fail; refresh with `--write-baseline` so they cannot
come back unnoticed.

## Reflection export (`screens-found.json`)

```json
{"implemented": {"screen.id": "Namespace.Type"}, "covered": {"screen.id": {"states": ["default"]}}}
```
