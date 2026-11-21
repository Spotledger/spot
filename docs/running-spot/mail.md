Part of [Running Spot](../running-spot.md).

# Mail

Mail is a job, `platform:send_mail`, added to the outbox by the code that sends it; the site
has one account, called `default`, from `services.mail`. `sink = "memory"` records messages in
the process. `sink = "smtp"` sends through the configured server over a connection pinned to the
addresses the egress guard authorised for the site: the site's own host and port are the only
destination its policy lists, a private address is allowed only when the host is written as that
address, and a host that resolves to a private address is refused. A transient failure (a 4xx
reply, a timeout) is retried by the job queue; a permanent one bounces.

The log of accepted messages is the site's database (`spot_mail_sent_log`) whenever the job
store is PostgreSQL, which is the default: a restart between the server's acceptance and the
job's completion does not send the message again, on this node or another. The host creates the
table at start if it is missing (`spot migrate` also knows it, so `spot migrate`, `spot
install-app --apply` and runtime customisation succeed on a site that has it). With the memory
job store, or `sent_log = "memory"`, the log lives in the process and the host logs a warning
once: a restart can send an accepted message again.
