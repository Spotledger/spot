Part of [Running Spot](../running-spot.md).

# The LLM service: models under the tenant's policy

Server code, and later agents, call a model through one gateway per site. The gateway hosts the
bring-your-own-key providers (`spot-llm-anthropic`, `spot-llm-openai`: Anthropic, OpenRouter, any
OpenAI-compatible endpoint, Ollama) behind the task router (`spot-llm-router`) and puts four
controls around every call: an allow-list, a budget, a record of what was sent, and redaction.
Code: `crates/framework/spot-cli/src/services/llm.rs` and `services/llm/`.

The service is **optional**: if it cannot start, the site serves and readiness says
`degraded: llm (...)`, and no model call can be made (the gateway port is not provided). It needs
the **secrets host** (`[services.secrets]`, to seal keys) and the **egress service**; without
either the service says so and stays off. Model calls are never made without the egress guard.

## Settings

```toml
[services.llm]
enabled = true                 # default true
max_retries = 2                # retries of a retryable provider failure on the same route; 0 to 10
retention_days = 90            # how long a send record is kept; 1 to 3650
maintenance_seconds = 30       # usage flush and retention sweep interval; 1 to 3600
request_timeout_seconds = 120  # the total time one model request may take; 1 to 3600

# Optional: store a tenant's configuration at start. A key is never configured here.
[[services.llm.tenants]]
tenant = "acme"                # a-z, 0-9, _ and -; the name the egress policy uses for the tenant
kind = "anthropic"             # anthropic | openrouter | openai | ollama
endpoint = "https://api.anthropic.com"
model = "claude-sonnet-4-5"
allowed_hosts = ["api.anthropic.com"]   # exact names or *.suffix; required
tasks = ["chat", "summarise"]  # default ["chat"]
context_window = 200000
max_output = 8192
default_max_output_tokens = 1024
price = { input_micro_per_million = 3000000, output_micro_per_million = 15000000 }
budget = { limit_micro = 50000000, period = "month", warn_at_percent = 80 }   # period: day | month
```

Unknown keys, out-of-range numbers and an invalid tenant entry stop the site from starting; the
message names the key and never a value. Money is whole micro-units (millionths of a currency unit)
and prices are micro-units per million tokens, so cost is an exact integer (rounded half up).

## Keys

A key is **write-only**. It is sealed by the secrets vault, bound to the site and the tenant (a
copied value does not open elsewhere), and stored in its own column. No operation returns it; it
is opened only on the call path, into a zeroising buffer inside the provider. It appears in no
log line, error, usage row or send record. Ollama needs none.

## What stands between a prompt and the network

In this order, each refusing before anything is sent:

1. the tenant has a configuration and the task is one of its tasks;
2. the endpoint host is on the tenant's own `allowed_hosts`;
3. the provider's key is stored (Ollama excepted);
4. **the egress policy** (`purpose = "llm"`, see [egress.md](egress.md)): the host is resolved and
   every answer checked. A name that resolves to a private, loopback or metadata address is refused
   with no connection. A local model host works only when the egress policy lists it with
   `allow_private`; the transport authorises every redirect hop again and dials only the pinned
   addresses. Both the tenant's list and the egress policy must allow;
5. **the budget**: the worst case of the call (input estimate plus the output cap) is reserved
   before the provider is called; a call that would pass the period's cap is refused with
   `llm_host.budget_exceeded`.

## Usage and budgets

Every settled call is added to a row per tenant, task, day, provider and model (`spot_llm_usage`:
tokens, cache tokens, calls, cost). A budget is per tenant and per `day` or `month` (UTC). The
spend of the period is read back from the database when the period starts or the process does, so a
restart does not reset it, and a new period starts from zero. Usage is written after each call; if
the database is down the rows wait in memory and are retried by the maintenance loop and at
shutdown.

## The record of what was sent

One row per call in `spot_llm_send`, written **before** the provider is called and closed with the
outcome: the tenant, task and request id; the endpoint as `scheme://host[:port]` (no path, no
query); provider and model; a SHA-256 digest of the request and its size in bytes; the DocTypes
whose fields were included and how many fields of each class were included or stripped
(`plain`, `personal`, `stripped_protected`, `stripped_sealed`, `stripped_personal`); the user and
the purpose; the outcome (`sent`, `refused_budget`, `refused_allow_list`, `refused_egress`,
`refused_key`, `refused_config`, `failed_llm_*`) and the tokens. Never the content, never the
key. Refusals are recorded too, so what was blocked is as visible as what went out. Rows older
than `retention_days` are deleted by the maintenance loop.

## Prompts

A prompt is built with `LlmGateway::prompt`. Free text is redacted (keys, tokens, card numbers,
connection strings). A document enters only through the permission-checked read **as the invoking
user** (`DocumentReader`): protected-group fields and sealed (secret) fields are always dropped,
personal-data fields unless the policy includes them, and what remains is redacted. Until the
engine-backed reader is provided as a port the host uses a reader that denies every read, so no
document can enter a prompt.

## Codes

`llm_host.settings_invalid`, `llm_host.config_invalid`, `llm_host.not_configured`,
`llm_host.task_unknown`, `llm_host.key_missing`, `llm_host.key_unusable`,
`llm_host.endpoint_not_allowed`, `llm_host.endpoint_denied` (carries the egress code),
`llm_host.budget_exceeded`, `llm_host.provider_failed` (carries the provider's `llm.*` code),
`llm_host.store_unavailable`, `llm_host.secrets_unavailable`, `llm_host.egress_unavailable`.

## Tables

`spot_llm_tenant` (configuration and sealed key), `spot_llm_usage`, `spot_llm_send`, all keyed by
the site name and created idempotently at start.
