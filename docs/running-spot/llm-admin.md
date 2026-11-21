Part of [Running Spot](../running-spot.md).

The service itself: [the LLM service](llm.md).

# `spot llm`: configure a tenant's model provider

Until now a tenant's provider could only be seeded from `[[services.llm.tenants]]` in
`common.toml`, which cannot carry a key. `spot llm` is the operator's command for the rest: it
shows, sets and removes a tenant's configuration, enters its key without ever showing it, reads
usage and send records and proves the connection works. Code:
`crates/framework/spot-cli/src/commands/llm.rs`.

Every subcommand takes `--site SITE` (optional when the host has exactly one site) and works on the
site's database and secrets vault directly, the way `spot secrets` does: no running server is
needed. The site needs `[services.secrets]` (to seal keys) and, for `test`, the egress policy.

Exit codes: **0** done, **1** usage, **2** refused (nothing changed, or nothing sent), **3** failed.

## Show

```
spot llm show [--tenant T] [--json]
```

The configuration without the key: kind, endpoint, model, allowed hosts, tasks, price, budget,
embedding model and `key: present` or `key: absent`. Every tenant when none is named. `--json`
prints one object per tenant with the same fields and `"key": "present"|"absent"`.

## Set

```
spot llm set --tenant T --kind anthropic|openrouter|openai|ollama \
    --endpoint URL --model M --allow-host H [--allow-host H2 ...] \
    [--task chat --task summarise] [--price-in N --price-out N] \
    [--budget-day N | --budget-month N] \
    [--embedding-model M --embedding-dimensions N]
```

Replaces the tenant's configuration in one write (the stored key is kept). The command calls the
service's own validator (`TenantConfig::validate`, the same code that checks `common.toml`), so the
rules are the service's, never a copy: no credentials, query or fragment in the endpoint, an
allow-list that covers the endpoint's host, a budget only with a price, valid task names, an
embedding model only on a provider that has an embeddings endpoint (not Anthropic) and dimensions
only with a model. A refusal prints the validator's fixed sentence (it never quotes the value you
typed) and exits 2.

Money is whole micro-units; prices are micro-units per million tokens (see
[the LLM service](llm.md#usage-and-budgets)). Defaults of the fields the flags do not name (context
window, output limits, tasks `chat`) are the service's.

## The key: write-only

```
spot llm key set --tenant T            # a no-echo prompt on the terminal
echo "$KEY" | spot llm key set --tenant T --stdin
spot llm key remove --tenant T
```

The key is read from a **no-echo prompt on a terminal**, or **one line of standard input** with
`--stdin`. It is never taken from an argument or an environment variable (both end up in process
listings and shell history). The text is wrapped in the secret type at once, sealed by the site's
secrets vault (bound to the site and tenant) and stored in its own column; it is printed nowhere,
not in the output, an error or an audit record, and no command returns it. `show` only says
`present` or `absent`. The tenant must have a configuration first.

`key remove` deletes the stored key and keeps the configuration, usage and send records; the
tenant's next call is refused with `llm_host.key_missing` (Ollama needs no key).

## Usage and send records

```
spot llm usage --tenant T [--since 7d] [--json]
spot llm sends --tenant T [--limit N]
```

`usage` lists the settled usage per UTC day, task, provider and model: calls, input, output and
cache tokens and the exact cost in micro-units. Embedding calls appear under the task `embed`.
`--since 7d` keeps the last seven days (days only). `sends` lists the send records, newest first
(default 20, at most 1000): outcome, task, endpoint (scheme, host and port only), provider, model,
the first 12 hex characters of the request digest, its size, the DocTypes, the number of fields
of each class, the user, the purpose and the tokens. **Never a text.** Fields a caller controls
are escaped, so a hostile value cannot write control sequences to your terminal.

## Test

```
spot llm test --tenant T
```

Makes one minimal, content-free call (the single word `ping`, at most one token back) on the
tenant's first task through the same path as every call: the allow-list, the egress guard
(`purpose = llm`, after DNS), the key and the budget. It prints **`ok`** or the stable
`llm_host.*` code and nothing else (`llm_host.endpoint_denied`, `llm_host.key_missing`,
`llm_host.budget_exceeded`, `llm_host.provider_failed`, ...). The call is counted like any other:
it writes a send record (user `operator`, purpose `connection test`) and usage, and it is refused
when the budget is spent. A refusal by policy (allow-list, egress, key, budget) exits 2; a
provider or database failure exits 3.

## Audit

`set`, `key set` and `key remove` write a record on the site's identity audit chain **before** they
change anything: the kind (`llm_config_set`, `llm_key_set`, `llm_key_removed`), the tenant and the
names of the fields that change, never a value. If the record cannot be stored (the audit chain is
not ready), the command changes nothing and exits 2.

## When a running server sees a change

The command writes the database. A running server keeps each tenant's provider stack for at most
one minute before it reads the configuration and key again, so a change applies to the next call
after that, with no restart and no reload request.

## Codes

The command prints the codes of [the LLM service](llm.md#codes) (`llm_host.config_invalid`,
`llm_host.not_configured`, `llm_host.key_missing`, `llm_host.endpoint_not_allowed`,
`llm_host.endpoint_denied`, `llm_host.budget_exceeded`, `llm_host.provider_failed`,
`llm_host.store_unavailable`, `llm_host.secrets_unavailable`, `llm_host.settings_invalid`).
