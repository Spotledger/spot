Part of [Running Spot](../running-spot.md).

# Overrides and seams on a site

An app can override a step of another app (`replace`, `wrap`, `disable`, `reorder`) or replace a seam. The administrator can see every override in force without reading manifests.

```
spot overrides list --site <SITE> [--app <APP>] [--json]
```

It builds the registry of the site's installed apps with the host's own builder, starts no server and reads no database, and prints:

```
OVERRIDES
app  kind     target_step  doctype  hook_point   order  reason
...
SEAMS
name  version  provider_app  replaced_by  reason
```

`--json` prints `{ "overrides": [...], "seams": [...], "conflicts": [] }`, the same data the administrator-only method `GET /api/v1/method/platform/seams/overrides` serves (the answer for anyone else is that of an unknown function). Rows are sorted and identical on every run; `--app` keeps the rows that involve that app. Exit 0; 3 when the registry cannot be built (every conflict is printed, one per line, code first) or the site cannot be read; 1 usage.

## Reading it

| Kind | Meaning |
| --- | --- |
| `replace` | the overriding step runs in the target's place; the target never runs |
| `wrap` | the overriding step runs in the target's place and may run the target once |
| `disable` | the target never runs |
| `reorder` | the overriding step runs right before or after the target |

`order` is the position at which the override takes effect within its DocType and hook point; `reason` is what the declaring app wrote. A listing never shows step code or arguments.

## Conflicts and removal

Two apps that replace the same step, an override of a step nobody declares, or a seam major-version mismatch is a conflict with a stable code (for example `override.duplicate_replace`); the site refuses to start and the command prints each one with the apps involved. To remove an override, uninstall the app that declares it.

## Status

The method's handler (`services/seams_admin.rs`) is written and tested as data, but is not yet mounted as a function of the platform app (it needs `services/function.rs` and `apps/platform/app.toml`); `spot install-app` printing seam conflicts before any schema change needs `src/apps.rs`. Until then use the command.

## Install refuses seam conflicts

`spot install-app` plans the site's app list with the new app added. Before it builds metadata, opens the database or writes anything, it builds the controllers' strict registry for that planned list. If the registry finds a seam conflict, the install is refused and the message has one line per conflict:

```
the app set cannot be installed: platform, billing, billing_plus
seam.duplicate_provider: <message naming the seam and both apps> (apps: billing, billing_plus)
```

The code is the catalogued one (`seam.duplicate_provider`, `seam.replace_unknown`, `seam.duplicate_replacement`, `seam.version_mismatch`, `seam.required_missing`); the line names apps and the seam, never step code. Re-running an install of an app the site already has plans the same list and is not a conflict. A planned set that fails for another reason (for example a native controller this binary does not contain) is not refused here; that surfaces when the site starts, as before.
