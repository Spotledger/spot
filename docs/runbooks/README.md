# Runbooks

Procedures an operator runs under stress. Each is one page, written against the product's own
operations, and kept true by tests.

| Runbook | Use it when |
| --- | --- |
| [Recover a site from its journal](journal-recovery.md) | the site's database is lost or corrupted |
| [The lifecycle of a branch node](branch-node-lifecycle.md) | enrolling, suspending, restoring, rotating, revoking or retiring a branch |
| [Reading `/api/health/ready`](readiness.md) | a load balancer or a person reads the readiness endpoint |

## Rules for a runbook

1. One page per procedure.
2. A command is a fenced block that starts with `spot ops` and is **exactly** what the product
   renders for that operation: the tests in `spot-mgmt-journal` and `spot-mgmt-sync` parse every
   block, validate it against the catalogue and render it back; the page must equal the result.
3. Every operation id the page names must exist in the catalogue; every relative link must resolve.
4. A critical operation's command carries its confirmation flag.
5. No secret in a command line: secrets are prompted. No real host name, address, key, token,
   code or personal name: identifiers are `example-` or `EXAMPLE-` placeholders.
6. Each page says when to stop and call, as "the support contact in your agreement".
7. Never tell an operator to disable a check, copy a key by hand or run SQL: every action is an
   operation.
