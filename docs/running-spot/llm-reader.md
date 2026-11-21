Part of [Running Spot](../running-spot.md).

The service that uses it: [the LLM service](llm.md).

# The document reader for model prompts

A prompt can include a document only through a reader that reads **as the invoking user**. Until
this module, the LLM service used a reader that denied everything, so no prompt could ever carry a
document. `llm_reader` provides the real one. Code:
`crates/framework/spot-cli/src/services/llm_reader.rs` and `services/llm_reader/`.

The module is **optional** and on by default. It needs no other module's port: it reads the site's
document engine, permission core and principal directory from the live runtime on every call, and
it starts (order 1255) before the LLM service (1260), which picks the reader up.

```toml
[services.llm_reader]
enabled = true          # default true
max_value_chars = 4000  # a longer field value is cut here with an ellipsis; 1 to 100000
```

## What one read does

1. **The caller must be `user:<id>`**, the typed reference of a signed-in user or an agent acting as
   one. A `consumer:` principal, the platform, a component name or any other spelling is refused.
2. **The load is the user's load.** The document engine loads the record as that user: role grants,
   user permissions and shares. A missing document, one the user may not read, a hidden DocType and a
   user the directory does not know all answer the same way (`denied`): there is no way to learn
   whether a document exists. A right revoked a moment ago applies to the very next read; nothing is
   cached between calls.
3. **Field-level permissions apply.** The document goes through the site's field guard on the
   `Search` channel (the one retrieval uses). A field the guard hides is absent. A field it would
   only mask is dropped too, never passed in its masked form.
4. **Classes are honest.** Every remaining field is labelled for the prompt builder from the served
   metadata: `protected` (in a protection group), `sealed` (classification `secret`, or a value in
   the sealed or encrypted envelope), `personal` (classification `pii`), otherwise `plain`. The
   prompt builder then drops protected and sealed fields (and personal ones unless its policy
   includes them) and redacts the rest; the reader does not do that a second time.
5. **An outage is not a denial.** If the document engine or the principal directory cannot answer,
   the caller is told the read is *unavailable*, the module counts it, and readiness says
   `degraded: llm_reader (llm_reader.reads_failed: N read(s) failed ...)` until the next check that
   finds none. A denial never hides an outage from the operator.

Only the record's own fields are read: child-table rows are not part of the view. Binary, geographic
and range values have no text form and are left out; dates, numbers, links and JSON are rendered as
text.

## When it is off or not ready

With no site runtime (a host with no live hub) the module says
`degraded: llm_reader (llm_reader.site_unavailable ...)` and provides nothing, so the LLM service
keeps its deny-all reader. While the site serves no metadata yet, readiness says
`llm_reader.not_ready` and every read is denied. Neither case ever releases a field.

## Codes

`llm_reader.settings_invalid`, `llm_reader.site_unavailable`, `llm_reader.not_ready`,
`llm_reader.reads_failed`.
