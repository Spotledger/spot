Part of [Running Spot](../running-spot.md).

# Personal-field protection and erasure (`[services.journal_privacy]`)

A field flagged personal is encrypted in the journal under a **per-subject key**. Erasing a subject destroys the key: replays, snapshots and rebuilds then yield the documented placeholder for those fields, and everything else is untouched (ADR-061 sections 3 and 6). The platform ships the mechanism and no legal defaults: what may be erased is policy data on the field.

## Configuration

Off without the table (the journal then ships lines as they are, and says so in the start-up log). The metadata catalogue carries no `personal` flag yet, so the declarations are a list:

```toml
[services.secrets]
kek_file = "/etc/spot/keys/{site}.json"       # the subject keys are wrapped by this provider

[services.journal_privacy]
forbid_erase_classes = ["tax_10y"]             # retention classes that forbid erasing a field
erasers = ["<user id of the data-protection officer>"]   # interim for the permission journal.erase_subject
cache_ttl_millis = 60000                       # how long a node may serve a key cached before a destroy elsewhere
max_cached_keys = 1024

[[services.journal_privacy.fields]]
entity = "contact"
path = "email"                                 # `*` walks an array: phones.*.number
subject = "person"                             # required: the field that identifies the data subject
purpose = "notifications"
retention_class = "contact_5y"
# erasable = false                             # legal retention: sealed under the tenant journal key, never erased
# secret = true                                # also a secret
```

Run `spot migrate` first: the wrapped-key table `spot_journal_subject_key` is created by it. A personal field with no `subject` refuses the site's start (`journal_privacy.subject_missing`): it could not be sealed. With fields declared but no `[services.secrets]` or no table, the journal shipper **refuses every line** (`journal_privacy.protection_missing`) rather than write a personal value in clear.

## Erasing a subject

`JournalPrivacyHandle::erase_subject(subject, requester)`:

1. the requester must be listed in `erasers` (default deny); a refusal is recorded;
2. fields under a class in `forbid_erase_classes` (and fields declared `erasable = false`) stay readable: the result lists them (`retained`), so a partial erase is explicit; if **every** field is retained the erase is refused naming the entity and field;
3. the audit record (`sys_journal_admin`, `subject_erased`) is queued, then the key is destroyed (idempotent; a tombstone, so the subject never gets a second key), then the record is written. The target of the record is a salted hash of the subject, never the id.

A key another node cached before the destroy is served for at most `cache_ttl_millis` (default 60 s) on that node. Keys of different sites are separate: erasing subject S in site A has no effect on a subject with the same id in site B.

Out of scope here: legal retention defaults, erasure in the live database (the data-policy engine's), the data-subject request UI.
