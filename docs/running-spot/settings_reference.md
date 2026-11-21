Part of [Running Spot](../running-spot.md).

# Settings reference

A **setting** is one named, typed value that an operator can change without editing code: its key
(for example `runtime.jobs.worker_concurrency`), its type and bounds, its default, the scopes it may
be set at, whether changing it restarts something, and how risky it is. Every setting is described
once, in a file under `crates/framework/spot-settings/settings/`, and this page is generated from
those files. To read or change a setting through the management API, see
[the settings management service](settings_api.md); this page does not repeat it.

## Scopes and which value wins

A setting can be set at one or more **scopes**, from broad to narrow: fleet, group, server, tenant,
company, user. The value in force for a given place is found by walking the chain of scopes that
apply there, broad to narrow, and taking the **narrowest scope that has a value set**. If no scope
in the chain has one, the default applies. A value stored at a scope the setting does not allow is
ignored. This is `resolve` in `crates/framework/spot-settings/src/resolution.rs`; the tests
`the_narrowest_set_value_wins_and_the_chain_explains_it`, `an_unset_setting_resolves_to_its_default`,
`a_value_at_a_disallowed_scope_is_ignored` and the property test `resolve_equals_the_naive_model`
(in `src/tests/resolution.rs`) prove it. The answer also lists every scope in the chain with what it
held, so the console can show why a value is what it is.

## Restart

"Restart" says what must be restarted for a change to take effect: nothing, a named service, or the
host (`Restart` in `src/model.rs`). A change to a setting that needs a restart is applied whole, and
the result names what must restart ([the settings service](settings_api.md)). The service names come
from the setting files; no registry of running services exists yet to check them against, so compare
a name with the services table of [Running Spot](../running-spot.md).

## Risk

Each setting carries a risk, from read to critical, and the console shows it with the setting. Over
the management API a setting whose risk is high or critical cannot be changed with `settings.set`;
it needs the separate `settings.set_high_risk` operation, which is itself a critical operation
(`crates/framework/spot-cli/src/services/settings_api.rs`).

## Secrets

A secret setting never shows its value. It has no default, resolution returns "redacted" for it
(`EffectiveValue::Redacted`), and the change history holds digests only. Through the management API
a secret setting holds the **name** of an entry in the secret store, not the secret itself, so what
you can see is a name.

## The settings

Settings are grouped by their `group` (settings without one are under Other); inside a group they
follow the setting's `order`, then the key. A default shown as `none` means the setting has no
default (always so for a secret).

Adding a setting: add a file under crates/framework/spot-settings/settings/ and run python3 tools/docs-check/gen_settings_reference.py --write.

CI runs `python3 tools/docs-check/gen_settings_reference.py --check`, which fails with a diff when
this page is stale, and also lints the catalogue: the file name matches the key, keys are unique, a
secret has no default, a default fits its type and bounds, every scope and restart value is known,
`since` is `major.minor.patch` and a `help_url` starts with `https://`.

<!-- BEGIN GENERATED: settings -->

### Email

#### `runtime.smtp.host`

| Field | Value |
| --- | --- |
| Title | SMTP Host |
| Summary | Hostname of the SMTP server for outgoing email. |
| Description | The hostname or IP address of the SMTP server used for sending emails. |
| Value | text up to 255 characters, the whole value matching `^[a-zA-Z0-9.-]+$` |
| Default | `localhost` |
| Scopes | fleet, group, server, tenant |
| Restart | restarts service `email` |
| Risk | medium |
| Secret | no |
| Read only | no |
| Since | 0.1.0 |
| Tags | `email`, `smtp` |

#### `runtime.smtp.password`

| Field | Value |
| --- | --- |
| Title | SMTP Password |
| Summary | Password for SMTP authentication (secret). |
| Description | The password used for SMTP authentication. This value is never shown, logged, or included in hashes. |
| Value | text up to 256 characters |
| Default | none |
| Scopes | fleet, group, server, tenant |
| Restart | restarts service `email` |
| Risk | high |
| Secret | yes |
| Read only | no |
| Since | 0.1.0 |
| Tags | `email`, `smtp`, `secret` |

### Other

#### `company.fiscal_year_start`

| Field | Value |
| --- | --- |
| Title | Fiscal Year Start |
| Summary | The month when the fiscal year starts. |
| Description | The calendar month (1-12) when the company's fiscal year begins. Used for financial reporting. |
| Value | integer 1 to 12 |
| Default | 1 |
| Scopes | company |
| Restart | none |
| Risk | low |
| Secret | no |
| Read only | no |
| Since | 0.1.0 |
| Tags | `finance`, `reporting` |

#### `os.kernel.version`

| Field | Value |
| --- | --- |
| Title | Kernel Version |
| Summary | The running kernel version (read-only). |
| Description | The version of the operating system kernel. This is observed from the system and cannot be changed. |
| Value | text up to 128 characters |
| Default | `unknown` |
| Scopes | server |
| Restart | none |
| Risk | read |
| Secret | no |
| Read only | yes |
| Since | 0.1.0 |
| Tags | `os`, `readonly` |

#### `runtime.http.listen_address`

| Field | Value |
| --- | --- |
| Title | HTTP Listen Address |
| Summary | The address and port the HTTP server binds to. |
| Description | The address and port the HTTP server binds to. Use `0.0.0.0` for all interfaces or `127.0.0.1` for localhost only. |
| Value | text up to 64 characters, the whole value matching `^([0-9]{1,3}\.){3}[0-9]{1,3}:[0-9]{1,5}$` |
| Default | `0.0.0.0:8080` |
| Scopes | fleet, group, server |
| Restart | restarts service `http` |
| Risk | medium |
| Secret | no |
| Read only | no |
| Since | 0.1.0 |
| Tags | `network`, `http` |

#### `runtime.jobs.worker_concurrency`

| Field | Value |
| --- | --- |
| Title | Job Worker Concurrency |
| Summary | Maximum number of concurrent job workers. |
| Description | The maximum number of job workers that can run concurrently. Higher values increase throughput but consume more resources. |
| Value | integer 1 to 256 |
| Default | 4 |
| Scopes | fleet, group, server |
| Restart | restarts service `job-runner` |
| Risk | medium |
| Secret | no |
| Read only | no |
| Since | 0.1.0 |
| Tags | `jobs`, `performance` |

#### `runtime.logging.level`

| Field | Value |
| --- | --- |
| Title | Log Level |
| Summary | Minimum log level for application logs. |
| Description | Controls the verbosity of application logs. Higher levels produce more output. |
| Value | one of `trace`, `debug`, `info`, `warn`, `error` |
| Default | `info` |
| Scopes | fleet, group, server, tenant |
| Restart | none |
| Risk | low |
| Secret | no |
| Read only | no |
| Since | 0.1.0 |
| Tags | `logging`, `observability` |

#### `runtime.session.idle_timeout`

| Field | Value |
| --- | --- |
| Title | Session Idle Timeout |
| Summary | Maximum idle time before a session expires. |
| Description | Maximum idle time in seconds before a user session is automatically terminated. Set to 0 to disable. |
| Value | duration in whole seconds |
| Default | 1800 seconds |
| Scopes | fleet, group, server, tenant, company |
| Restart | none |
| Risk | low |
| Secret | no |
| Read only | no |
| Since | 0.1.0 |
| Tags | `session`, `security` |

#### `tenant.backup.retention_days`

| Field | Value |
| --- | --- |
| Title | Backup Retention Days |
| Summary | Number of days to retain automated backups. |
| Description | How many days to keep automated backups before they are purged. Set to 0 to keep forever (not recommended). |
| Value | integer 0 to 3650 |
| Default | 30 |
| Scopes | tenant, company |
| Restart | none |
| Risk | medium |
| Secret | no |
| Read only | no |
| Since | 0.1.0 |
| Tags | `backup`, `retention` |

#### `tenant.default_currency`

| Field | Value |
| --- | --- |
| Title | Default Currency |
| Summary | The default currency for the tenant. |
| Description | The default currency code used for new documents and reports in this tenant. |
| Value | one of `USD`, `EUR`, `GBP`, `JPY`, `INR`, `CNY` |
| Default | `USD` |
| Scopes | tenant, company |
| Restart | none |
| Risk | low |
| Secret | no |
| Read only | no |
| Since | 0.1.0 |
| Tags | `currency`, `finance` |

<!-- END GENERATED: settings -->
