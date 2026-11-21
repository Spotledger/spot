Part of [Running Spot](../running-spot.md).

# Incoming mail

A site can receive mail: the host polls a mailbox, creates one document of a DocType you name for
each new message, and says in readiness and metrics whether it is working. It is **off by
default**: a site without `[services.mail_intake] enabled = true` registers nothing. There is no
platform DocType for it: accounts are configuration, and the target DocType belongs to an app.

The engine is the `spot-mail-intake` library (one poll step per account, a durable UID cursor,
deduplication, loop and flood protection); this service hosts it. Sending mail is a separate
service, [Mail](mail.md).

## Setting up an account

In the site's `site.toml`:

```toml
[services.mail_intake]
enabled = true
store = "postgres"            # or "memory": a restart forgets the position (a warning is logged)
lease_seconds = 300           # 30 to 3600; how long one node's lease on an account lasts

[[services.mail_intake.accounts]]
name = "support"              # a storage name: lower-case letters, digits, underscore
host = "imap.example.com"
port = 993
security = "tls"              # or "starttls"; plain IMAP does not exist
username = "support@example.com"
address = "support@example.com"   # the account's own address (loop protection)
password_ref = "support_imap"     # the name of a sealed secret, see Credentials
mailbox = "INBOX"
poll_interval_seconds = 300       # 30 to 86400
max_messages_per_poll = 50        # 1 to 200
max_message_bytes = 10485760      # at most 25 MiB; larger mail arrives as a header-only stub
max_messages_per_day = 10000      # 1 to 100000
target_doctype = "support_ticket"
run_as = "mail-robot"             # a user of the site; required
store_attachments = false         # true stores attachments, see Attachments
max_attachments = 10
# blocked_extensions = ["exe", "js"]   # default: executable and script types

[services.mail_intake.accounts.field_map]
from = "requester"            # required
subject = "title"             # required
text = "description"
html = "description_html"
to = "recipients"
received_at = "received_at"
thread_key = "thread"
message_id = "message_id"
```

A bad account (a broken rule, an unknown user) stops **that account**, is reported, and the others
run. A malformed table (an unknown key, a wrong type) stops the start.

## The target DocType and the user

Each message becomes one document created through the document engine **as the `run_as` user**, so
that user's create right, field rights, validation and hooks all apply: the platform never uses a
system principal for it. Create a user for the purpose (`spot add-user`), give it a role that may
create `target_doctype` and write the mapped fields, and make sure the DocType's required fields
are all either mapped or have defaults. `from` and `subject` must be mapped; the other parts are
optional and are written only when mapped.

- A missing, disabled or unauthorised `run_as` user **pauses** the account (`run_as_unavailable`)
  and drops nothing: the message is tried again when the user is fixed.
- A document the DocType refuses (a failed validation) is a **poison message**: counted,
  audited by reason code, skipped; the next message still arrives.

## What is dropped, and why

Dropped messages never reach the DocType and are counted per reason; the mailbox position still
moves past them.

- `auto_reply`: `Auto-Submitted` other than `no`, or `Precedence: bulk` or `auto_reply`.
- `own_mail`: the `From` is the account's own `address`, **or** the message carries a valid
  `X-Spot-Outgoing` marker. The marker is a keyed MAC (HMAC-SHA256) over the message's own
  Message-ID, put there by the site's outgoing mail, under a key derived from the site's mail
  secret. An outsider cannot make one: a forged or copied-to-another-mail marker is handled as
  ordinary mail, and a message without a Message-ID is never dropped by marker. One audit entry
  per poll (`mail_intake.own_mail`) holds the count and the first UID, never an address, subject
  or Message-ID. If the mail secret is rotated, older markers stop verifying; such mail then
  falls back to the `From` and auto-submitted checks. A site with no mail secret recognises own
  mail by `From` only.
- `daily_cap`: more than `max_messages_per_day` in one UTC day; one audit entry per day.
- Duplicates (the same Message-ID already imported) are skipped and counted, not dropped.

The mail itself is never marked, moved or deleted on the server.

## Credentials

The IMAP password is never in clear in configuration, logs, errors or readiness. Seal it under
the site's `[services.secrets]` key and refer to it by name:

```toml
[services.mail_intake.secrets.support_imap]
sealed = "spsec:v1:..."      # the sealed text
aad = "..."                  # the associated data it was sealed under
```

This is the same sealed-secret shape the process triggers use (see the secrets guide).

If the secrets service is not configured, no account can be polled (`auth_failed`).

## Attachments

`store_attachments = true` stores each allowed attachment as a **file of the `run_as` user, attached
to the document the message created**. The upload goes through the same file service the
`/api/v1/files` route uses, so every check of an HTTP upload applies: the site's type policy, the
size limit, the magic-number check, the quota, the malware scan (a quarantined or infected file is
refused) and the user's right to update the document. The site must serve files; an account that
asks for attachments on a site without the files service is reported `invalid` and not polled (the
other accounts run).

- Inline images of the HTML body are part of the body, not files.
- `blocked_extensions` (every dot-separated tail) and `max_attachments` skip an attachment before
  the upload; skips are counted in `spot_mail_intake_attachments_skipped_total`.
- A file the service refuses (too large, type refused or mismatched, malware, quota, no right) is
  counted as skipped and never undoes the document; a refusal names its catalogued code
  (`file.too_large`, `file.virus_detected`, `fileservice.type_mismatch`, `permission.denied`, ...)
  in the log, never a file name, a sender or the bytes.
- The raw message is not stored.

## Network

Each account's host and port are authorised through the site's egress guard exactly as SMTP is:
the policy lists those two and nothing else; a private address is allowed only when the host is
written as that address.

## Limits and mailbox resets

- One poll takes at most `max_messages_per_poll` messages; if more wait, the next poll follows
  after a second.
- After a transient failure the delay grows from 30 seconds to at most 15 minutes. After an
  authentication or configuration refusal polling stops until an operator fixes it and restarts
  the service or the site.
- If the server rebuilds the mailbox (its `UIDVALIDITY` changes), the new generation is recorded,
  nothing is imported from that read, an audit entry is written, and the next poll reads the
  mailbox from the start: messages already imported are skipped by Message-ID. A message with no
  Message-ID cannot be recognised after a reset.
- The seen set keeps 90 days of Message-ID digests (never the ids themselves) and is purged every
  six hours.

## Readiness and metrics

Service `mail_intake` is optional in `/api/health/ready`: `running`, or stopped with a reason:
`auth_failed`, `config_rejected`, `unreachable`, `run_as_unavailable`. Metrics (labels `site` and
`account` only): `spot_mail_intake_polls_total`, `_accepted_total`, `_duplicates_total`,
`_dropped_auto_reply_total`, `_dropped_own_mail_total`, `_dropped_daily_cap_total`,
`_dropped_other_total`, `_rejected_total`, `_attachments_skipped_total`, `_lag_seconds` (seconds
since the last successful poll) and `_account_state`.

## Pausing an account

Set `enabled = false` on the account (or remove it) and reload or restart. The stored position
stays, so enabling it later continues where it stopped.
