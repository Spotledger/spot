# Service ports of the jobs module

The `jobs` module (order 600) provides three narrow ports to modules that register after it.
A module takes one with `ctx.require_port::<dyn JobSubmit>()` (or `ctx.port()` when it can do
without). A module that registers before `jobs` gets the registry's error, which names the
module and the port. The code is `crates/framework/spot-cli/src/services/ports.rs`.

Every port belongs to one site. It is built from that site's own job store, coordinator,
change feed and engine, and no method takes a site id. A port of site A never reads or writes
site B.

## JobSubmit

Can: create one job with a name, a queue, arguments, an idempotency key and an optional
not-before time. It answers `true` for a new job and `false` when a job with the same name and
key already exists.

Cannot: list, read, cancel or retry any job. It refuses a malformed name, a queue the site's
workers do not claim, an empty or over-long key (200 bytes) and arguments over 64 KiB.

## OutboxLease

Can: take, renew, check and release a lease named `outbox:<site>:<consumer>` (a consumer name is
lower-case letters, digits, `_` and `-`, up to 64 bytes), and read published outbox events after
a position, at most 500 at a time. A holder that finds `still_held` false must stop acting.

Cannot: touch the jobs intake lease (`serve:<site>:jobs`) or the scheduler lease, accept a lease
made for another site, or write to the outbox. The port keeps no cursor: the consumer stores
its own `FeedPosition` and passes it to `read`.

## SystemReader

Can: load one document by DocType and record id, as the platform itself (a system actor named
`service_ports`), for the current site. It answers `None` when the site has no such document.

The system principal is not filtered by permissions or protection groups, so a document comes
back whole except for `secret` fields: the engine keeps those out of a `Document` altogether
(their sealed bytes live in engine-managed columns), so `webhook.secret` and
`user.password_hash` never reach a module. A JSON field that holds a sealed value (webhook
`headers`) returns it sealed, as stored. A module that takes this port must not copy a
document's fields into a log, a job argument or a delivery record.

Cannot: write, list, query, or read another site. It is not reachable from a request path or a
DocType script; only a module that takes the port from the registry holds it.

## After the stop signal

When the site's stop signal arrives every method answers `PortError::Stopped` at once (it does
not wait). The one exception is `OutboxLease::release`, so a holder can still hand its lease
back. Errors never quote a payload, key, document name or secret.

## Job handlers

A handler is registered with `ctx.job_handlers()`, as mail does; the jobs module takes the
registry when every module has registered. It is not a port.

## Reloads and the memory store

Ports are provided on the assembling build only. A reload keeps the running services and has no
store to build from, so a module that needs a port builds its service only when
`ctx.assembling()` is true. With `services.jobs.store = "memory"` the ports exist as well (tests
use them) and the memory-store notice is unchanged.
